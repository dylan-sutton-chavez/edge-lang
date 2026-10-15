use super::prelude::*;
use crate::vm::opcodes::subscript::slice_bounds;

// `slice.indices(len)`, the bounds a sequence of that length reads.
pub fn indices(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let &HeapObj::Slice(start, stop, step) = vm.heap.get(recv) else { return Err(cold_type("indices() requires a slice")); };
    if !pos[0].is_int() { return Err(cold_type("slice indices must be integers")); }
    if pos[0].as_int() < 0 { return Err(cold_value("length should not be negative")); }
    let clip = |v: Val| crate::frontend::int::clip_index(&vm.heap, v);
    let (s, e, st) = slice_bounds(clip(start), clip(stop), clip(step), pos[0].as_int())?;
    let t = vm.heap.alloc(HeapObj::Tuple(vec![Val::int(s), Val::int(e), Val::int(st)]))?;
    vm.push(t); Ok(())
}
