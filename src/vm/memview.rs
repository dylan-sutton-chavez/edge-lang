/* `memoryview`, a view of the bytes of a bytes or bytearray value that reads and, over a bytearray, writes them
   in place. A view of a view shows the same object, its offsets composed. */

use super::VM;
use super::types::*;
use super::opcodes::attr_lookup::AttrLookup;
use crate::s;
use alloc::vec::Vec;

fn released() -> VmErr { cold_value("operation forbidden on released memoryview object") }

/* Where item `i` of a view sits in the bytes beneath it, a negative `i` counting from the end. */
fn at(m: &MemView, i: i128) -> Result<usize, VmErr> {
    let i = if i < 0 { i + m.len as i128 } else { i };
    if !(0..m.len as i128).contains(&i) { return Err(cold_index("index out of bounds on dimension 1")); }
    Ok((m.start as isize + i as isize * m.step) as usize)
}

impl<'a> VM<'a> {
    /* `memoryview(obj)`, the whole of a bytes-like value. */
    pub fn call_memoryview(&mut self) -> Result<(), VmErr> {
        let obj = self.pop()?;
        let view = match self.heap.try_get(obj) {
            Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => MemView { obj, start: 0, len: b.len(), step: 1, released: false },
            Some(HeapObj::MemoryView(m)) if m.released => return Err(released()),
            Some(&HeapObj::MemoryView(m)) => m,
            _ => return Err(VmErr::TypeMsg(s!("memoryview: a bytes-like object is required, not '", str self.type_name(obj), "'"))),
        };
        let v = self.heap.alloc(HeapObj::MemoryView(view))?;
        self.push(v);
        Ok(())
    }

    /* The view `v` holds, refused once released. */
    pub(crate) fn view(&self, v: Val) -> Option<Result<MemView, VmErr>> {
        match self.heap.try_get(v)? {
            HeapObj::MemoryView(m) if m.released => Some(Err(released())),
            &HeapObj::MemoryView(m) => Some(Ok(m)),
            _ => None,
        }
    }

    /* Whether the bytes under a view can be written, a bytearray beneath it. */
    fn writable(&self, m: &MemView) -> bool { matches!(self.heap.try_get(m.obj), Some(HeapObj::ByteArray(_))) }

    /* `m[i]` of a view, the byte as an int. */
    pub(crate) fn view_item(&self, m: MemView, i: Val) -> Result<Val, VmErr> {
        let Some(i) = self.as_i128(i).filter(|_| !i.is_float()) else {
            return Err(VmErr::TypeMsg(s!("memoryview: invalid slice key")));
        };
        let at = at(&m, i)?;
        let byte = self.heap.bytes_of(m.obj).and_then(|b| b.get(at)).copied().ok_or_else(|| cold_index("index out of bounds on dimension 1"))?;
        Ok(Val::int(byte as i64))
    }

    /* `m[a:b:c]`, a view of part of the view. */
    pub(crate) fn view_slice(&mut self, m: MemView, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> {
        let clip = |v: Val| crate::frontend::int::clip_index(&self.heap, v);
        let (s, e, st) = super::opcodes::subscript::slice_bounds(clip(start), clip(stop), clip(step), m.len as i64)?;
        let len = if st > 0 { ((e - s).max(0) + st - 1) / st } else { ((s - e).max(0) - st - 1) / -st } as usize;
        let start = (m.start as isize + s as isize * m.step).max(0) as usize;
        self.heap.alloc(HeapObj::MemoryView(MemView { obj: m.obj, start, len, step: m.step * st as isize, released: false }))
    }

    /* `m[i] = byte` or `m[a:b] = bytes`, written into the bytearray beneath. */
    pub(crate) fn view_store(&mut self, m: MemView, idx: Val, value: Val) -> Result<(), VmErr> {
        if !self.writable(&m) { return Err(cold_type("cannot modify read-only memory")); }
        let (positions, bytes): (Vec<usize>, Vec<u8>) = match self.heap.try_get(idx) {
            Some(&HeapObj::Slice(start, stop, step)) => {
                let part = self.view_slice(m, start, stop, step)?;
                let Some(&HeapObj::MemoryView(p)) = self.heap.try_get(part) else { return Ok(()) };
                let size = self.heap.bytes_of(m.obj).map_or(0, <[u8]>::len);
                let bytes = self.heap.octets(value).map(|b| b.to_vec()).ok_or_else(|| VmErr::TypeMsg(s!("a bytes-like object is required, not '", str self.type_name(value), "'")))?;
                (p.positions(size).collect(), bytes)
            }
            _ => {
                let Some(i) = self.as_i128(idx) else { return Err(cold_type("memoryview: invalid slice key")) };
                (alloc::vec![at(&m, i)?], alloc::vec![self.byte_value(value)?])
            }
        };
        if positions.len() != bytes.len() { return Err(cold_value("memoryview assignment: lvalue and rvalue have different structures")); }
        if let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(m.obj) {
            for (p, x) in positions.into_iter().zip(bytes) { if let Some(slot) = b.get_mut(p) { *slot = x; } }
        }
        Ok(())
    }

    /* The attributes of a view, None for names it does not have. */
    pub(crate) fn view_attr(&self, v: Val, name: &str) -> Option<Result<AttrLookup, VmErr>> {
        let Some(HeapObj::MemoryView(m)) = self.heap.try_get(v) else { return None };
        let m = *m;
        if m.released && !matches!(name, "release" | "obj") { return crate::vm::methods::lookup_method("memoryview", name).map(|id| Ok(AttrLookup::BuiltinMethod(id))).or(Some(Err(released()))); }
        let int = |n: i64| Some(Ok(AttrLookup::ClassMember(Val::int(n))));
        match name {
            "readonly" => Some(Ok(AttrLookup::ClassMember(Val::bool(!self.writable(&m))))),
            "nbytes" => int(m.len as i64),
            "itemsize" | "ndim" => int(1),
            "format" => Some(Ok(AttrLookup::Name("B".into()))),
            "obj" => Some(Ok(AttrLookup::ClassMember(m.obj))),
            "contiguous" | "c_contiguous" | "f_contiguous" => Some(Ok(AttrLookup::ClassMember(Val::bool(m.step == 1 || m.len <= 1)))),
            "shape" => Some(Ok(AttrLookup::ExcArgs(alloc::vec![Val::int(m.len as i64)]))),
            "strides" => Some(Ok(AttrLookup::ExcArgs(alloc::vec![Val::int(m.step as i64)]))),
            _ => None,
        }
    }

    /* `tobytes`, `tolist`, `hex` and `release` of a view. */
    pub(crate) fn view_method(&mut self, recv: Val, name: &str, pos: &[Val]) -> Result<(), VmErr> {
        if name == "release" {
            if let Some(HeapObj::MemoryView(m)) = self.heap.try_get_mut(recv) { m.released = true; }
            self.push(Val::none());
            return Ok(());
        }
        if let Some(Err(e)) = self.view(recv) { return Err(e); }
        let bytes = self.heap.octets(recv).map(|b| b.to_vec()).unwrap_or_default();
        let v = match name {
            "tobytes" => self.heap.alloc(HeapObj::Bytes(bytes))?,
            "tolist" => { let items = bytes.iter().map(|&b| Val::int(b as i64)).collect(); return self.alloc_and_push_list(items); }
            _ => {
                let b = self.heap.alloc(HeapObj::Bytes(bytes))?;
                return self.with_roots([b], |vm| crate::vm::methods::bytes::hex(vm, b, pos));
            }
        };
        self.push(v);
        Ok(())
    }
}
