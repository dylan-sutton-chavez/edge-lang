use core::cell::RefCell;
use alloc::{rc::Rc, string::String, vec::Vec};

use super::super::VM;
use super::super::types::*;

impl<'a> VM<'a> {

    /* Heap-alloc `s` and push the resulting Val. Used by builtins that produce string results. */
    pub(crate) fn alloc_and_push_str(&mut self, s: String) -> Result<(), VmErr> {
        let v = self.heap.alloc(HeapObj::Str(s))?;
        self.push(v); Ok(())
    }

    /* Allocate a List from items and push. Centralises the Rc::new(RefCell::new(items)) construction inlined. */
    pub(crate) fn alloc_list(&mut self, items: Vec<Val>) -> Result<Val, VmErr> {
        self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))
    }

    /* Allocate a List, push it, return Ok. */
    pub(crate) fn alloc_and_push_list(&mut self, items: Vec<Val>) -> Result<(), VmErr> {
        let v = self.alloc_list(items)?;
        self.push(v); Ok(())
    }

    // Allocate a Set from `items` (deduped by content) and push. Mirrors `alloc_and_push_list`.
    pub(crate) fn alloc_and_push_set(&mut self, items: Vec<Val>) -> Result<(), VmErr> {
        let v = self.alloc_set_result(items, false)?;
        self.push(v); Ok(())
    }

    /* Allocate a Dict from a DictMap and push. */
    pub(crate) fn alloc_and_push_dict(&mut self, dm: DictMap) -> Result<(), VmErr> {
        let v = self.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(dm))))?;
        self.push(v); Ok(())
    }

    /* Allocate a Tuple and push. */
    pub(crate) fn alloc_and_push_tuple(&mut self, items: Vec<Val>) -> Result<(), VmErr> {
        let v = self.tuple_from_items(items)?;
        self.push(v); Ok(())
    }

    // Build a tuple Val from items. Shared by the VM and the plugin ABI.
    pub(crate) fn tuple_from_items(&mut self, items: Vec<Val>) -> Result<Val, VmErr> {
        self.heap.alloc(HeapObj::Tuple(items))
    }

    // Build a set Val from items, rejecting unhashable elements first.
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    pub(crate) fn set_from_items(&mut self, items: Vec<Val>) -> Result<Val, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        self.alloc_set_result(items, false)
    }

    // Build a frozenset Val from items, rejecting unhashable elements first.
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    pub(crate) fn frozenset_from_items(&mut self, items: Vec<Val>) -> Result<Val, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        self.alloc_set_result(items, true)
    }

    pub fn build_set(&mut self, op: u16) -> Result<(), VmErr> {
        let items = self.pop_n(op as usize)?;
        let s = self.valset_of(&items)?;
        let val = self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))?;
        self.push(val); Ok(())
    }

    #[inline]
    pub fn build_slice(&mut self, op: u16) -> Result<(), VmErr> {
        let step = if op == 3 { self.pop()? } else { Val::none() };
        let stop = self.pop()?;
        let start = self.pop()?;
        let val = self.heap.alloc(HeapObj::Slice(start, stop, step))?;
        self.push(val); Ok(())
    }

    // Operand packs kw<<8 | pos, keep counts distinct.
    pub fn call_dict(&mut self, op: u16) -> Result<(), VmErr> {
        let pos = (op & 0xFF) as usize;
        let kw = (op >> 8) as usize;
        if pos > 1 {
            return Err(cold_type("dict expected at most 1 argument"));
        }
        // Keyword pairs sit above the positional source.
        let ok = core::mem::take(&mut self.job_ok);
        let kw_flat = self.pop_n(kw * 2)?;
        let src = if pos == 1 { Some(self.pop()?) } else { None };
        // A generator called where a frame may open runs as a frame of the loop, its pairs gathered as it yields them.
        if ok && kw == 0 && let Some(s) = src && s.is_heap() && self.gather_gen(s, crate::vm::jobs::Takes::All(crate::parser::OpCode::CallDict, op), &[])? { return Ok(()); }
        // A dict source copies with the hashes it stored.
        if let Some(rc) = src.and_then(|s| self.heap.dict_cell(s)) && kw_flat.is_empty() {
            let dm = rc.borrow().clone();
            return self.alloc_and_push_dict(dm);
        }
        let mut pairs = match src { Some(s) => self.pairs_of(s)?, None => Vec::new() };
        pairs.extend(kw_flat.chunks(2).map(|p| (p[0], p[1])));
        let dm = self.dictmap_of(pairs)?;
        self.alloc_and_push_dict(dm)
    }

    /* `frozendict()` as `dict()` builds it, a frozendict alone given back as itself. */
    pub fn call_frozendict(&mut self, op: u16) -> Result<(), VmErr> {
        if op == 1 && let Some(&top) = self.stack.last() && matches!(self.heap.try_get(top), Some(HeapObj::FrozenDict(_))) { return Ok(()); }
        self.call_dict(op)?;
        let made = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
        if let Some(slot) = self.heap.try_get_mut(made) && let HeapObj::Dict(rc) = slot { *slot = HeapObj::FrozenDict(rc.clone()); }
        Ok(())
    }

    /* Entries of a mapping, or of an iterable of two-item iterables, as `dict()` takes them. */
    pub(crate) fn pairs_of(&mut self, src: Val) -> Result<Vec<(Val, Val)>, VmErr> {
        if let Some(rc) = self.heap.dict_cell(src) { self.heap.spend(rc.borrow().len()); return Ok(rc.borrow().iter().collect()); }
        let mut pairs = Vec::new();
        for item in self.extract_iter(src)? {
            let item = self.extract_iter(item).map_err(|e| match e {
                VmErr::TypeMsg(_) => VmErr::TypeMsg("object is not iterable".into()),
                e => e,
            })?;
            let [k, v] = item[..] else {
                return Err(VmErr::Raised(crate::s!("ValueError: dictionary update sequence element #", int pairs.len() as i64, " has length ", int item.len() as i64, "; 2 is required")));
            };
            self.require_key(k)?;
            self.keyed(k)?;
            pairs.push((k, v));
        }
        Ok(pairs)
    }

    pub fn call_set(&mut self, op: u16) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        let src = if op == 0 { None } else { Some(self.pop()?) };
        if ok && let Some(s) = src && s.is_heap() && self.gather_gen(s, crate::vm::jobs::Takes::All(crate::parser::OpCode::CallSet, 1), &[])? { return Ok(()); }
        let s = self.set_source(src)?;
        let val = self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))?;
        self.push(val);
        Ok(())
    }

    /* The items `set(src)` or `frozenset(src)` holds, a set source copies the hashes it stored. */
    fn set_source(&mut self, src: Option<Val>) -> Result<ValSet, VmErr> {
        let Some(src) = src else { return Ok(ValSet::new()) };
        match self.heap.try_get(src) {
            Some(HeapObj::Set(rc)) => return Ok(rc.borrow().clone()),
            Some(HeapObj::FrozenSet(rc)) => return Ok((**rc).clone()),
            _ => {}
        }
        let items = self.extract_iter(src)?;
        self.valset_of(&items)
    }

    /* `frozenset()` | `frozenset(iter)`, construct an immutable, hashable set from an iterable. Without args returns the empty frozenset. */
    pub fn call_frozenset(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        if args.len() > 1 { return Err(cold_type("frozenset() takes 0 or 1 argument")); }
        let s = self.set_source(args.first().copied())?;
        let v = self.heap.alloc(HeapObj::FrozenSet(Rc::new(s)))?;
        self.push(v); Ok(())
    }

    /* `bytes()`, empty, `n` zero bytes, an iterable of ints in range(256), a bytes-like value or `(str, encoding, errors)`. */
    #[inline(never)]
    pub fn call_bytes(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let buf = self.octets_of(&args, "bytes")?;
        let v = self.heap.alloc(HeapObj::Bytes(buf))?;
        self.push(v); Ok(())
    }

    /* `bytearray(...)`, the arguments `bytes()` takes, a mutable copy. */
    pub fn call_bytearray(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let buf = self.octets_of(&args, "bytearray")?;
        let v = self.heap.alloc(HeapObj::ByteArray(buf))?;
        self.push(v); Ok(())
    }

    /* The octets the arguments of `bytes()` or `bytearray()` name. */
    fn octets_of(&mut self, args: &[Val], what: &str) -> Result<Vec<u8>, VmErr> {
        Ok(match *args {
            [] => Vec::new(),
            [a] if a.is_int() || a.is_bool() || crate::frontend::int::is_big(&self.heap, a) => {
                let n = self.as_i128(a).unwrap_or(i128::MAX);
                if n < 0 { return Err(cold_value("negative count")); }
                // Length is user-controlled, cap it against the memory left so a huge count errors instead of aborting.
                let n = usize::try_from(n).map_err(|_| cold_overflow())?;
                self.heap.reserve(n)?;
                alloc::vec![0u8; n]
            }
            [a] if self.heap.bytes_of(a).is_some() => self.heap.bytes_of(a).unwrap_or_default().to_vec(),
            [a] if matches!(self.heap.try_get(a), Some(HeapObj::Str(_))) => return Err(VmErr::TypeMsg(crate::s!("string argument without an encoding"))),
            [a] => {
                let items = self.extract_iter(a).map_err(|_| VmErr::TypeMsg(crate::s!("cannot convert '", str self.type_name(a), "' object to ", str what)))?;
                // `bytes` names its range in the plural, `bytearray` in the singular.
                items.into_iter().map(|v| self.byte_value(v).map_err(|e| if what == "bytes" && matches!(e, VmErr::Value(_)) { cold_value("bytes must be in range(0, 256)") } else { e })).collect::<Result<Vec<u8>, VmErr>>()?
            }
            [text, ref rest @ ..] if rest.len() <= 2 => {
                for (&a, name) in rest.iter().zip(["encoding", "errors"]) {
                    if !matches!(self.heap.try_get(a), Some(HeapObj::Str(_))) {
                        return Err(VmErr::TypeMsg(crate::s!(str what, "() argument '", str name, "' must be str, not ", str self.type_name(a))));
                    }
                }
                let Some(HeapObj::Str(t)) = self.heap.try_get(text) else { return Err(cold_type("encoding without a string argument")) };
                let (encoding, errors) = crate::vm::methods::string::codec_args(self, rest)?;
                crate::vm::codecs::encode(t, encoding, errors)?
            }
            _ => return Err(VmErr::TypeMsg(crate::s!(str what, "() takes at most 3 arguments"))),
        })
    }
}
