use core::cell::RefCell;
use alloc::{rc::Rc, string::{String, ToString}, vec::Vec};

use super::super::VM;
use super::super::types::*;
use crate::vm::eq::range_len;

fn normalize_index(i: i64, len: usize) -> usize {
    (if i < 0 { len as i64 + i } else { i }) as usize
}

/* `xs[a:b] = items`, or `del xs[a:b]` without items, on a vector of any item, an extended slice needing an exact count. */
fn splice<T>(b: &mut Vec<T>, (s, e, st): (i64, i64, i64), items: Option<Vec<T>>) -> Result<(), VmErr> {
    if st == 1 {
        b.splice(s as usize..e.max(s) as usize, items.unwrap_or_default());
        return Ok(());
    }
    // Selected positions sit `st` apart from `s`, strictly before `e`.
    let picked = |k: i64| (if st > 0 { k < e && k >= s } else { k > e && k <= s }) && (k - s) % st == 0;
    let Some(items) = items else {
        let mut k = -1;
        b.retain(|_| { k += 1; !picked(k) });
        return Ok(());
    };
    let span = if st > 0 { (e - s + st - 1) / st } else { (s - e - st - 1) / -st };
    if items.len() as i64 != span.max(0) {
        return Err(VmErr::Raised(crate::s!("ValueError: attempt to assign sequence of size ", int items.len() as i64, " to extended slice of size ", int span.max(0))));
    }
    for (k, v) in items.into_iter().enumerate() { b[(s + k as i64 * st) as usize] = v; }
    Ok(())
}

/* Slice bounds clamped to a sequence of `len` items, the triple `slice.indices(len)` returns. */
pub(crate) fn slice_bounds(start: Val, stop: Val, step: Val, len: i64) -> Result<(i64, i64, i64), VmErr> {
    let st = if step.is_none() { 1 } else if step.is_int() { step.as_int() } else {
        return Err(cold_type("slice step must be an integer"));
    };
    if st == 0 { return Err(cold_value("slice step cannot be zero")); }
    let clamp = |v: Val, def: i64| -> i64 {
        if v.is_none() { def }
        else if v.is_int() { let i = v.as_int(); if i < 0 { (len+i).max(0) } else { i.min(len) } }
        else { def }
    };
    // Negative step bounds at [-1, len-1], an underflowing index floors at -1, not 0.
    let clamp_neg = |v: Val, def: i64| -> i64 {
        if v.is_none() { def }
        else if v.is_int() { let i = v.as_int(); (if i < 0 { len + i } else { i }).clamp(-1, len - 1) }
        else { def }
    };
    Ok(if st > 0 { (clamp(start, 0), clamp(stop, len), st) } else { (clamp_neg(start, len - 1), clamp_neg(stop, -1), st) })
}

impl<'a> VM<'a> {

    pub fn get_item(&mut self, ip: usize, chunk: &crate::parser::SSAChunk, cache: &crate::vm::cache::OpcodeCache) -> Result<(), VmErr> {
        // An in-range int index on a list or tuple needs no dunder or coercion.
        let n = self.stack.len();
        if n >= 2 && self.stack[n - 1].is_int() && self.stack[n - 2].is_heap() {
            let i = self.stack[n - 1].as_int();
            let hit = match self.heap.plain(self.stack[n - 2]) {
                Some(HeapObj::List(v)) => { let b = v.borrow(); b.get(normalize_index(i, b.len())).copied() }
                Some(HeapObj::Tuple(v)) => v.get(normalize_index(i, v.len())).copied(),
                _ => None,
            };
            if let Some(v) = hit { self.stack.truncate(n - 2); self.push(v); return Ok(()); }
        }
        let idx = self.pop()?;
        let obj = self.pop()?;

        // instance `__getitem__` runs before built-in indexing, and slices pass through as a single Slice arg.
        if let Some(r) = self.try_call_dunder(obj, "__getitem__", &[idx], chunk)? {
            // Record monomorphic hit so the next iteration skips the class lookup.
            self.record_dunder_hit(ip, cache, obj, "__getitem__", 2);
            self.push(r);
            return Ok(());
        }

        let idx = self.coerce_index(obj, idx, chunk)?;
        if self.heap.dict_cell(obj).is_some() { self.keyed(idx)?; }
        if let Some(p) = self.heap.dict_cell(obj) {
            let found = p.borrow().get(&idx, &self.heap).copied();
            // A class over dict answers a missing key through its `__missing__`.
            if found.is_none() && self.heap.classed(obj).is_some() && let Some(v) = self.try_call_dunder(obj, "__missing__", &[idx], chunk)? {
                self.push(v);
                return Ok(());
            }
            let Some(v) = found else { self.require_key(idx)?; return Err(self.key_error(idx)) };
            self.push(v);
            return Ok(());
        }
        match self.get_item_builtin(obj, idx) {
            Err(e) if obj.is_heap() && matches!(self.heap.get(obj), HeapObj::Class(..)) => self.class_getitem(obj, idx, e, chunk),
            r => r,
        }
    }

    /* `A[int]` on a class calls its `__class_getitem__`, a class with type parameters builds a generic alias. */
    fn class_getitem(&mut self, cls: Val, idx: Val, err: VmErr, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if let Some((f, _)) = self.lookup_class_member(cls, "__class_getitem__") {
            // An implicit classmethod, decorated or not.
            let f = match self.heap.try_get(f) { Some(&HeapObj::ClassMethod(inner)) => inner, _ => f };
            self.push(f);
            self.push(cls);
            self.push(idx);
            return self.exec_call(2, chunk);
        }
        if self.lookup_class_member(cls, "__type_params__").is_none() { return Err(err); }
        let alias = self.generic_alias(cls, idx)?;
        self.push(alias);
        Ok(())
    }

    /* KeyError carrying the missing key itself, so `e.args[0]` keeps its type and its text is the repr. */
    pub(crate) fn key_error(&mut self, key: Val) -> VmErr {
        let msg = self.repr(key);
        match self.heap.alloc(HeapObj::ExcInstance(String::from("KeyError"), alloc::vec![key], Val::undef())) {
            Ok(exc) => { self.pending.exc_val = Some(exc); VmErr::Raised(crate::s!("KeyError: ", str &msg)) }
            Err(e) => e,
        }
    }

    /* KeyError carrying the text `msg`, as the methods that find nothing to remove raise. */
    pub(crate) fn text_key_error(&mut self, msg: &str) -> VmErr {
        match self.heap.alloc(HeapObj::Str(msg.into())) { Ok(k) => self.key_error(k), Err(e) => e }
    }

    /* Instance indexes coerce via `__index__`, including slice bounds. Dict keys never coerce, they look up by hash and eq. */
    fn coerce_index(&mut self, cont: Val, idx: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        // `xs[True]` indexes like `xs[1]`, a dict keeps the bool key.
        let keyed = self.heap.dict_cell(cont).is_some();
        if idx.is_bool() && !keyed { return Ok(Val::int(idx.as_bool() as i64)); }
        if !idx.is_heap() || keyed { return Ok(idx); }
        // An int of a class deriving from int indexes as the int.
        if let &HeapObj::Boxed(n) = self.heap.get(idx) && (n.is_int() || n.is_foreign()) { return Ok(n); }
        match *self.heap.get(idx) {
            HeapObj::Instance(..) => match self.try_call_dunder(idx, "__index__", &[], chunk)? {
                Some(r) if r.is_int() || r.is_bool() => {
                    // Normalize bool to int, the builtin paths below accept only `is_int`.
                    let i = self.as_i128(r).unwrap_or(r.as_bool() as i128);
                    self.int_to_val(Some(i))
                }
                Some(r) => Err(self.returned_wrong(idx, "__index__", "an int", r)),
                None => Ok(idx),
            },
            HeapObj::Slice(start, stop, step) => {
                let is_inst = |v: &Val| v.is_heap() && matches!(self.heap.get(*v), HeapObj::Instance(..));
                if !is_inst(&start) && !is_inst(&stop) && !is_inst(&step) { return Ok(idx); }
                let start = self.coerce_index(cont, start, chunk)?;
                let stop = self.coerce_index(cont, stop, chunk)?;
                let step = self.coerce_index(cont, step, chunk)?;
                self.heap.alloc(HeapObj::Slice(start, stop, step))
            }
            _ => Ok(idx),
        }
    }

    /* No-dunder indexing path. Used by callers without a bytecode frame (FFI re-entry) and as the post-dunder fallback inside `get_item`. */
    pub fn get_item_builtin(&mut self, obj: Val, idx: Val) -> Result<(), VmErr> {
        if idx.is_heap()
            && let &HeapObj::Slice(start, stop, step) = self.heap.get(idx) {
                let v = self.slice_val(obj, start, stop, step)?;
                self.push(v);
                return Ok(());
        }

        let ascii = idx.is_int() && self.heap.str_is_ascii(obj);
        if obj.is_heap() && idx.is_int()
            && let HeapObj::Str(s) = self.heap.get(obj) {
                let i = idx.as_int();
                let one: String = if ascii {
                    let b = s.as_bytes();
                    let c = *b.get(normalize_index(i, b.len())).ok_or_else(|| cold_index("string index out of range"))?;
                    (c as char).to_string()
                } else {
                    let ui = normalize_index(i, s.chars().count());
                    s.chars().nth(ui).ok_or_else(|| cold_index("string index out of range"))?.to_string()
                };
                let val = self.heap.alloc(HeapObj::Str(one))?;
                self.push(val);
                return Ok(());
        }

        // `bytes[i]` returns the byte as int (`0..=255`), unlike `str[i]` (length-1 str).
        if obj.is_heap() && idx.is_int()
            && let HeapObj::Bytes(b) | HeapObj::ByteArray(b) = self.heap.get(obj) {
                let i = idx.as_int();
                let ui = normalize_index(i, b.len());
                let what = if matches!(self.heap.get(obj), HeapObj::ByteArray(_)) { "bytearray index out of range" } else { "index out of range" };
                let byte = *b.get(ui).ok_or_else(|| cold_index(what))?;
                self.push(Val::int(byte as i64));
                return Ok(());
        }

        let v = self.getitem_val(obj, idx)?;
        self.push(v);
        Ok(())
    }

    fn slice_val(&mut self, obj: Val, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> {
        if !obj.is_heap() { return Err(cold_type("slice requires a sequence")); }
        if let Some(m) = self.view(obj) { return self.view_slice(m?, start, stop, step); }

        // Item count without materialising the source.
        let ascii = self.heap.str_is_ascii(obj);
        let len: i64 = match self.heap.get(obj) {
            HeapObj::List(v) => v.borrow().len() as i64,
            HeapObj::Tuple(v) => v.len() as i64,
            HeapObj::Str(s) => if ascii { s.len() as i64 } else { s.chars().count() as i64 },
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b.len() as i64,
            // A range slices into another range, its bounds resolved at any width.
            HeapObj::Range(..) | HeapObj::BigRange(..) => return crate::frontend::int::range_slice(&mut self.heap, obj, start, stop, step).unwrap_or(Err(VmErr::Overflow)),
            _ => return Err(cold_type("object is not sliceable")),
        };
        // A boxed index lies past either end of any sequence, so it clamps like the inline extreme.
        let clip = |v: Val| crate::frontend::int::clip_index(&self.heap, v);
        let (s, e, st) = slice_bounds(clip(start), clip(stop), clip(step), len)?;

        // Step 1 copies one contiguous range.
        let contiguous = st == 1;
        let (lo, hi) = (s.max(0) as usize, e.max(s).max(0) as usize);
        let mut indices = Vec::new();
        if !contiguous {
            let mut cur = s;
            if st > 0 { while cur < e { indices.push(cur as usize); cur += st; } }
            else { while cur > e { indices.push(cur as usize); cur += st; } }
        }
        let pick = |v: &[Val]| -> Vec<Val> {
            if contiguous { return v[lo.min(v.len())..hi.min(v.len())].to_vec(); }
            indices.iter().filter_map(|&i| v.get(i).copied()).collect()
        };
        let pick_bytes = |b: &[u8]| -> Vec<u8> {
            if contiguous { return b[lo.min(b.len())..hi.min(b.len())].to_vec(); }
            indices.iter().filter_map(|&i| b.get(i).copied()).collect()
        };

        // Pick while borrowing the source, allocate after.
        enum Out { List(Vec<Val>), Tuple(Vec<Val>), Str(String), Bytes(Vec<u8>), ByteArray(Vec<u8>) }
        let out = match self.heap.get(obj) {
            HeapObj::List(v) => Out::List(pick(&v.borrow())),
            HeapObj::Tuple(v) => Out::Tuple(pick(v)),
            HeapObj::Str(text) if ascii => Out::Str(String::from_utf8(pick_bytes(text.as_bytes())).unwrap_or_default()),
            HeapObj::Str(text) => {
                let chars: Vec<char> = text.chars().collect();
                Out::Str(if contiguous { chars[lo.min(chars.len())..hi.min(chars.len())].iter().collect() }
                    else { indices.iter().filter_map(|&i| chars.get(i)).collect() })
            }
            HeapObj::Bytes(buf) => Out::Bytes(pick_bytes(buf)),
            HeapObj::ByteArray(buf) => Out::ByteArray(pick_bytes(buf)),
            _ => return Err(cold_type("object is not sliceable")),
        };
        match out {
            Out::List(v) => self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(v)))),
            Out::Tuple(v) => self.heap.alloc(HeapObj::Tuple(v)),
            Out::Str(s) => self.heap.alloc(HeapObj::Str(s)),
            Out::Bytes(b) => self.heap.alloc(HeapObj::Bytes(b)),
            Out::ByteArray(b) => self.heap.alloc(HeapObj::ByteArray(b)),
        }
    }

    pub fn getitem_val(&mut self, obj: Val, idx: Val) -> Result<Val, VmErr> {
        if !obj.is_heap() { return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(obj), "' object is not subscriptable"))); }
        let bad = |vm: &Self, kind: &str| if crate::frontend::int::is_big(&vm.heap, idx) { cold_index("cannot fit 'int' into an index-sized integer") }
            // A string words it without slices and quotes the type.
            else if kind == "string" { VmErr::TypeMsg(crate::s!("string indices must be integers, not '", str vm.type_name(idx), "'")) }
            else { VmErr::TypeMsg(crate::s!(str kind, " indices must be integers or slices, not ", str vm.type_name(idx))) };
        if self.heap.dict_cell(obj).is_some() { self.keyed(idx)?; }
        match self.heap.get(obj) {
            HeapObj::List(v) => {
                if !idx.is_int() { return Err(bad(self, "list")); }
                let b = v.borrow(); let i = idx.as_int();
                let ui = normalize_index(i, b.len());
                b.get(ui).copied().ok_or_else(|| cold_index("list index out of range"))
            }
            HeapObj::Tuple(v) => {
                if !idx.is_int() { return Err(bad(self, "tuple")); }
                let i = idx.as_int();
                let ui = normalize_index(i, v.len());
                v.get(ui).copied().ok_or_else(|| cold_index("tuple index out of range"))
            }
            HeapObj::Dict(p) | HeapObj::FrozenDict(p) => {
                let hit = p.borrow().get(&idx, &self.heap).copied();
                match hit {
                    Some(v) => Ok(v),
                    None => { self.require_key(idx)?; Err(self.key_error(idx)) }
                }
            }
            HeapObj::Str(_) => Err(bad(self, "string")),
            HeapObj::Bytes(_) => Err(bad(self, "byte")),
            HeapObj::ByteArray(_) => Err(bad(self, "bytearray")),
            HeapObj::MemoryView(_) => { let m = self.view(obj).unwrap_or(Err(cold_heap()))?; self.view_item(m, idx) }
            &HeapObj::BigRange(..) if idx.is_int() || crate::frontend::int::is_big(&self.heap, idx) => crate::frontend::int::range_item(&mut self.heap, obj, idx).unwrap_or(Err(VmErr::Overflow)),
            HeapObj::BigRange(..) => Err(bad(self, "range")),
            // `range(n)[i]` is computed, never materialised.
            &HeapObj::Range(s, e, st) => {
                if !idx.is_int() { return Err(bad(self, "range")); }
                let (i, len) = (idx.as_int(), range_len(s, e, st) as i64);
                let i = if i < 0 { i + len } else { i };
                if !(0..len).contains(&i) { return Err(cold_index("range object index out of range")); }
                self.heap.int((s + i * st) as i128)
            }
            // `list[int]` and `Pair[int]` build a generic alias, the args kept as one tuple.
            HeapObj::Type(n) if matches!(n.as_str(), "list" | "tuple" | "dict" | "set" | "frozenset" | "type") => self.generic_alias(obj, idx),
            HeapObj::TypeAlias(..) => self.generic_alias(obj, idx),
            _ => Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(obj), "' object is not subscriptable"))),
        }
    }

    fn generic_alias(&mut self, origin: Val, idx: Val) -> Result<Val, VmErr> {
        let args = if idx.is_heap() && matches!(self.heap.get(idx), HeapObj::Tuple(_)) { idx } else { self.heap.alloc(HeapObj::Tuple(alloc::vec![idx]))? };
        self.heap.alloc(HeapObj::GenericAlias(origin, args))
    }

    /* Reject mutable types (list/dict/set) used as dict/set keys, plus instances whose class defines `__eq__`. */
    pub(in crate::vm) fn require_hashable(&self, v: Val) -> Result<(), VmErr> { self.require_hashable_at(v, 0) }

    /* A set probe of a set looks up as the frozenset it equals, so only other values must hash. */
    pub(in crate::vm) fn require_set_probe(&self, v: Val) -> Result<(), VmErr> {
        if v.is_heap() && matches!(self.heap.get(v), HeapObj::Set(_)) { return Ok(()); }
        self.require_element(v)
    }

    /* `v` as a dict key, an unhashable one named for that use as well as for its type. */
    pub(in crate::vm) fn require_key(&self, v: Val) -> Result<(), VmErr> { self.require_hashable(v).map_err(|e| self.unhashable_as(v, &e, "a dict key")) }

    /* `v` as a set element, an unhashable one named for that use as well as for its type. */
    pub(in crate::vm) fn require_element(&self, v: Val) -> Result<(), VmErr> { self.require_hashable(v).map_err(|e| self.unhashable_as(v, &e, "a set element")) }

    #[cold]
    fn unhashable_as(&self, v: Val, e: &VmErr, what: &str) -> VmErr {
        VmErr::TypeMsg(crate::s!("cannot use '", str self.type_name(v), "' as ", str what, " (", str &e.message(), ")"))
    }

    fn require_hashable_at(&self, v: Val, depth: usize) -> Result<(), VmErr> {
        if v.is_heap() && depth <= EQ_DEPTH_MAX {
            match self.heap.get(v) {
                HeapObj::List(_) => return Err(cold_type("unhashable type: 'list'")),
                HeapObj::ByteArray(_) => return Err(cold_type("unhashable type: 'bytearray'")),
                HeapObj::MemoryView(m) if m.released => return Err(cold_value("operation forbidden on released memoryview object")),
                HeapObj::MemoryView(m) if matches!(self.heap.try_get(m.obj), Some(HeapObj::ByteArray(_))) => return Err(cold_value("cannot hash writable memoryview object")),
                HeapObj::Dict(_) => return Err(cold_type("unhashable type: 'dict'")),
                HeapObj::Set(_) => return Err(cold_type("unhashable type: 'set'")),
                HeapObj::DictView(..) => return Err(VmErr::TypeMsg(crate::s!("unhashable type: '", str self.type_name(v), "'"))),
                // A tuple hashes through its items as deep as the hash looks, so each must be hashable too.
                HeapObj::Tuple(items) => for &item in items { self.require_hashable_at(item, depth + 1)?; },
                // A frozendict hashes through its values too, its keys hashable already.
                HeapObj::FrozenDict(d) => for (_, item) in d.borrow().iter() { self.require_hashable_at(item, depth + 1)?; },
                // A `__eq__` without `__hash__` would let two equal keys hash apart.
                HeapObj::Instance(cls, _) if self.hash_hook(*cls) == Some(None) => return Err(VmErr::TypeMsg(crate::s!("unhashable type: '", str self.type_name(v), "'"))),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn store_item(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let n = self.stack.len();
        if n >= 3 && self.stack[n - 2].is_int() && self.stack[n - 3].is_heap()
            && let Some(HeapObj::List(v)) = self.heap.plain(self.stack[n - 3]) {
            let mut b = v.borrow_mut();
            let ui = normalize_index(self.stack[n - 2].as_int(), b.len());
            if ui < b.len() {
                b[ui] = self.stack[n - 1];
                drop(b);
                self.stack.truncate(n - 3);
                return Ok(());
            }
        }
        let value = self.pop()?;
        let idx_val = self.pop()?;
        let cont = self.pop()?;
        if !cont.is_heap() { return Err(cold_type("object does not support item assignment")); }
        // instance `__setitem__(idx, value)` short-circuits the built-in dispatch.
        if self.try_call_dunder(cont, "__setitem__", &[idx_val, value], chunk)?.is_some() {
            return Ok(());
        }
        let idx_val = self.coerce_index(cont, idx_val, chunk)?;
        self.store_item_builtin(cont, idx_val, value)
    }

    /* No-dunder item-assignment path. Used by callers without a bytecode frame (FFI re-entry) and as the post-dunder fallback inside `store_item`. */
    pub fn store_item_builtin(&mut self, cont: Val, idx_val: Val, value: Val) -> Result<(), VmErr> {
        if !cont.is_heap() { return Err(cold_type("object does not support item assignment")); }
        if let Some(m) = self.view(cont) { return self.view_store(m?, idx_val, value); }
        // Slice assignment `xs[a:b] = iterable` materialises the RHS and splices it in place.
        if idx_val.is_heap()
            && let &HeapObj::Slice(start, stop, step) = self.heap.get(idx_val)
        {
            let new_items = self.extract_iter(value)?;
            return self.store_slice(cont, start, stop, step, Some(new_items));
        }
        // Reject mutable keys before borrowing the container mutably below.
        if matches!(self.heap.get(cont), HeapObj::Dict(_)) {
            self.require_key(idx_val)?;
            self.keyed(idx_val)?;
        }
        match self.heap.get(cont) {
            HeapObj::List(v) => {
                if !idx_val.is_int() { return Err(cold_type("list indices must be integers")); }
                let mut b = v.borrow_mut();
                let i = idx_val.as_int();
                let ui = normalize_index(i, b.len());
                if ui >= b.len() { return Err(cold_index("list assignment index out of range")); }
                b[ui] = value;
            }
            HeapObj::Dict(p) => p.borrow_mut().insert(idx_val, value, &self.heap),
            HeapObj::ByteArray(_) => {
                if !idx_val.is_int() { return Err(VmErr::TypeMsg(crate::s!("bytearray indices must be integers or slices, not ", str self.type_name(idx_val)))); }
                let byte = self.byte_value(value)?;
                let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(cont) else { return Ok(()) };
                let ui = normalize_index(idx_val.as_int(), b.len());
                if ui >= b.len() { return Err(cold_index("bytearray index out of range")); }
                b[ui] = byte;
            }
            _ => return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(cont), "' object does not support item assignment"))),
        }
        Ok(())
    }

    pub fn del_item(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let idx_val = self.pop()?;
        let cont = self.pop()?;
        if !cont.is_heap() { return Err(cold_type("object does not support item deletion")); }
        // instance `__delitem__(idx)` short-circuits the built-in dispatch.
        if self.try_call_dunder(cont, "__delitem__", &[idx_val], chunk)?.is_some() {
            return Ok(());
        }
        let idx_val = self.coerce_index(cont, idx_val, chunk)?;
        if idx_val.is_heap()
            && let &HeapObj::Slice(start, stop, step) = self.heap.get(idx_val)
        {
            return self.store_slice(cont, start, stop, step, None);
        }
        if matches!(self.heap.get(cont), HeapObj::Dict(_)) { self.keyed(idx_val)?; }
        match self.heap.get(cont) {
            HeapObj::List(v) => {
                if !idx_val.is_int() { return Err(cold_type("list indices must be integers")); }
                let mut b = v.borrow_mut();
                let ui = normalize_index(idx_val.as_int(), b.len());
                if ui >= b.len() { return Err(cold_index("list assignment index out of range")); }
                b.remove(ui);
            }
            HeapObj::Dict(p) => {
                if p.borrow_mut().remove(&idx_val, &self.heap).is_none() {
                    self.require_key(idx_val)?;
                    return Err(self.key_error(idx_val));
                }
            }
            HeapObj::ByteArray(_) => {
                if !idx_val.is_int() { return Err(VmErr::TypeMsg(crate::s!("bytearray indices must be integers or slices, not ", str self.type_name(idx_val)))); }
                let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(cont) else { return Ok(()) };
                let ui = normalize_index(idx_val.as_int(), b.len());
                if ui >= b.len() { return Err(cold_index("bytearray index out of range")); }
                b.remove(ui);
            }
            HeapObj::Tuple(_) => return Err(cold_type("tuple does not support item deletion")),
            _ => return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(cont), "' object does not support item deletion"))),
        }
        Ok(())
    }

    /* `xs[a:b] = items` or, without items, `del xs[a:b]`, an extended slice needs an exact count. */
    fn store_slice(&mut self, cont: Val, start: Val, stop: Val, step: Val, items: Option<Vec<Val>>) -> Result<(), VmErr> {
        let clip = |v: Val| crate::frontend::int::clip_index(&self.heap, v);
        let (start, stop, step) = (clip(start), clip(stop), clip(step));
        if matches!(self.heap.get(cont), HeapObj::ByteArray(_)) {
            let bytes = match items { Some(items) => Some(items.into_iter().map(|v| self.byte_value(v)).collect::<Result<Vec<u8>, VmErr>>()?), None => None };
            let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(cont) else { return Ok(()) };
            let bounds = slice_bounds(start, stop, step, b.len() as i64)?;
            return splice(b, bounds, bytes);
        }
        let HeapObj::List(rc) = self.heap.get(cont) else {
            return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(cont), "' object does not support slice assignment")));
        };
        let mut b = rc.borrow_mut();
        let bounds = slice_bounds(start, stop, step, b.len() as i64)?;
        splice(&mut b, bounds, items)
    }

    /* `v` as one byte, the int `bytearray` items must be. */
    pub(crate) fn byte_value(&self, v: Val) -> Result<u8, VmErr> {
        let v = crate::value::unboxed(v, &self.heap);
        let Some(n) = self.as_i128(v).filter(|_| !v.is_float()) else {
            return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(v), "' object cannot be interpreted as an integer")));
        };
        u8::try_from(n).map_err(|_| cold_value("byte must be in range(0, 256)"))
    }

    // `slice(stop)` | `slice(start, stop)` | `slice(start, stop, step)`, builtin, usable as a sequence index.
    pub fn call_slice(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let (start, stop, step) = match args.as_slice() {
            [stop] => (Val::none(), *stop, Val::none()),
            [start, stop] => (*start, *stop, Val::none()),
            [start, stop, step] => (*start, *stop, *step),
            _ => return Err(cold_type("slice() takes 1 to 3 arguments")),
        };
        let v = self.heap.alloc(HeapObj::Slice(start, stop, step))?;
        self.push(v); Ok(())
    }
}
