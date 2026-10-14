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
        let kw_flat = self.pop_n(kw * 2)?;
        let src = if pos == 1 { Some(self.pop()?) } else { None };
        // A dict source copies with the hashes it stored.
        if let Some(HeapObj::Dict(rc)) = src.and_then(|s| self.heap.try_get(s)) && kw_flat.is_empty() {
            let dm = rc.borrow().clone();
            return self.alloc_and_push_dict(dm);
        }
        let mut pairs = match src { Some(s) => self.pairs_of(s)?, None => Vec::new() };
        pairs.extend(kw_flat.chunks(2).map(|p| (p[0], p[1])));
        let dm = self.dictmap_of(pairs)?;
        self.alloc_and_push_dict(dm)
    }

    /* Entries of a mapping, or of an iterable of two-item iterables, as `dict()` takes them. */
    pub(crate) fn pairs_of(&mut self, src: Val) -> Result<Vec<(Val, Val)>, VmErr> {
        if let Some(HeapObj::Dict(rc)) = self.heap.try_get(src) { self.heap.spend(rc.borrow().len()); return Ok(rc.borrow().iter().collect()); }
        let mut pairs = Vec::new();
        for item in self.extract_iter(src)? {
            let item = self.extract_iter(item).map_err(|e| match e {
                VmErr::TypeMsg(_) => cold_type("cannot convert dictionary update sequence element to a sequence"),
                e => e,
            })?;
            let [k, v] = item[..] else {
                return Err(cold_value("dictionary update sequence element must have length 2"));
            };
            self.require_key(k)?;
            pairs.push((k, v));
        }
        Ok(pairs)
    }

    pub fn call_set(&mut self, op: u16) -> Result<(), VmErr> {
        let src = if op == 0 { None } else { Some(self.pop()?) };
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

    /* `bytes()`, empty, `n` zero bytes, iter of ints (0..=255), or `(str, encoding)`. Encodings limited to utf-8/utf8/ascii, unknown ones error so mismatches aren't silent. */
    pub fn call_bytes(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let buf: Vec<u8> = match args.len() {
            0 => Vec::new(),
            1 => {
                let a = args[0];
                if a.is_int() {
                    let n = a.as_int();
                    if n < 0 { return Err(cold_value("negative count")); }
                    // Length is user-controlled, cap it against the memory left so a huge count errors instead of aborting.
                    self.heap.reserve(n as usize)?;
                    alloc::vec![0u8; n as usize]
                } else if a.is_heap() {
                    if let HeapObj::Bytes(b) = self.heap.get(a) {
                        b.clone()
                    } else {
                        let items = self.extract_iter(a)?;
                        let mut out = Vec::with_capacity(items.len());
                        for v in items {
                            if !v.is_int() {
                                return Err(cold_type("bytes() iterable must contain ints"));
                            }
                            let n = v.as_int();
                            if !(0..=255).contains(&n) {
                                return Err(cold_value("bytes must be in range(0, 256)"));
                            }
                            out.push(n as u8);
                        }
                        out
                    }
                } else {
                    return Err(cold_type("bytes() requires an int, an iterable of ints, or (str, encoding)"));
                }
            }
            2 => {
                // `bytes(s, "utf-8")`, string encoding form.
                let (s, enc) = (args[0], args[1]);
                let Some(HeapObj::Str(text)) = self.heap.try_get(s).cloned() else {
                    return Err(cold_type("bytes() first argument must be a string when encoding is given"));
                };
                let Some(HeapObj::Str(encoding)) = self.heap.try_get(enc) else {
                    return Err(cold_type("bytes() encoding must be a string"));
                };
                match encoding.as_str() {
                    "utf-8" | "utf8" => text.into_bytes(),
                    "ascii" => {
                        if !text.is_ascii() {
                            return Err(VmErr::Raised("UnicodeEncodeError: 'ascii' codec can't encode non-ASCII characters".into()));
                        }
                        text.into_bytes()
                    }
                    _ => return Err(cold_value("unsupported encoding (expected 'utf-8' or 'ascii')")),
                }
            }
            _ => return Err(cold_type("bytes() takes at most 2 arguments")),
        };
        let v = self.heap.alloc(HeapObj::Bytes(buf))?;
        self.push(v); Ok(())
    }
}
