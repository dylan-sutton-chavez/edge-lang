use super::prelude::*;

// `it.__next__()` of a builtin iterator, StopIteration once it is spent.
pub fn iter_next(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let item = vm.iter_step(recv)?.ok_or_else(|| VmErr::Raised(String::from("StopIteration")))?;
    vm.push(item); Ok(())
}

// `it.__iter__()` is the iterator itself.
pub fn iter_self(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { vm.push(recv); Ok(()) }

// `BaseException.__init__(self, *args)` keeps the arguments as `args`.
pub fn exc_init(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.set_exc_args(recv, pos.to_vec())?;
    vm.push(Val::none()); Ok(())
}

// `BaseException.__str__(self)`, the message `args` gives.
pub fn exc_str(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let text = vm.display(recv);
    vm.alloc_and_push_str(text)
}

/* `__getitem__` a class over dict or list reaches through `super()`, the builtin lookup past its overrides. */
pub fn container_getitem(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let found = match vm.heap.get(recv) {
        HeapObj::Dict(rc) => rc.borrow().get(&pos[0], &vm.heap).copied(),
        _ => return vm.get_item_builtin(recv, pos[0]),
    };
    let Some(v) = found else { vm.require_hashable(pos[0])?; return Err(vm.key_error(pos[0])) };
    vm.push(v);
    Ok(())
}

pub fn container_setitem(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.store_item_builtin(recv, pos[0], pos[1])?;
    vm.push(Val::none());
    Ok(())
}

pub fn container_len(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let n = vm.builtin_len(recv)?;
    let v = vm.int_to_val(Some(n))?;
    vm.push(v);
    Ok(())
}
