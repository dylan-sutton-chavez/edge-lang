use alloc::{string::String, vec::Vec};

use super::super::VM;
use super::super::types::*;
pub(crate) use crate::value::host::{normalize_index, slice_bounds};


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


impl<'a> VM<'a> {

    pub(crate) fn get_item(&mut self, ip: usize, chunk: &crate::parser::SSAChunk, cache: &crate::vm::cache::OpcodeCache, rerun: crate::vm::redo::Rerun) -> Result<(), VmErr> {
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
        // A user object indexed or indexing, or a class by its `__class_getitem__`, runs its dunders as frames of the loop, the op run again after each.
        let user = self.heap.users && self.instance_class(obj).is_some();
        let armed = (user || self.heap.user(obj) || self.heap.user(idx) || self.user_keys(&[idx])) && self.redo_late(ip, rerun, self.stack.len() + 2, &[obj, idx]);
        // Before the program makes a class only a classed builtin could answer, which the full lookup finds.
        let dunder = if user || !self.heap.users { self.try_call_dunder(obj, "__getitem__", &[idx], chunk) } else { Ok(None) };
        let r = self.get_item_of(obj, idx, dunder, ip, chunk, cache);
        self.redo_done(armed);
        r
    }

    /* `obj[idx]` past the plain list and tuple indexes, `dunder` what an instance's `__getitem__` gave. */
    #[inline(always)]
    fn get_item_of(&mut self, obj: Val, idx: Val, dunder: Result<Option<Val>, VmErr>, ip: usize, chunk: &crate::parser::SSAChunk, cache: &crate::vm::cache::OpcodeCache) -> Result<(), VmErr> {
        // instance `__getitem__` runs before built-in indexing, and slices pass through as a single Slice arg.
        if let Some(r) = dunder? {
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


    /* Instance indexes coerce via `__index__`, including slice bounds. Dict keys never coerce, they look up by hash and eq. */
    fn coerce_index(&mut self, cont: Val, idx: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        if let Some(i) = self.plain_index(cont, idx) { return Ok(i); }
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


}
