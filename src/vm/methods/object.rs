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

// `BaseException.add_note(note)`, a line the traceback prints after the message.
pub fn exc_add_note(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.add_note(recv, pos[0])?;
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
    let Some(v) = found else { vm.require_key(pos[0])?; return Err(vm.key_error(pos[0])) };
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

/* Where `x` sits in range `recv`, an integral value by arithmetic, and whether `x` was an int at all. */
fn range_pos(vm: &VM, recv: Val, x: Val) -> (Option<i128>, bool) {
    let HeapObj::Range(s, e, st) = *vm.heap.get(recv) else { return (None, true) };
    let int = vm.as_i128(x);
    let k = match int {
        Some(k) => k,
        // Any other value matches by `==`, which only a whole float can win.
        None if x.is_float() && libm::floor(x.as_float()) == x.as_float() && x.as_float().abs() < 1e38 => x.as_float() as i128,
        None => return (None, false),
    };
    let (s, e, st) = (s as i128, e as i128, st as i128);
    let inside = if st > 0 { s <= k && k < e } else { e < k && k <= s };
    ((inside && (k - s) % st == 0).then(|| (k - s) / st), int.is_some())
}

pub fn range_index(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    match range_pos(vm, recv, pos[0]) {
        (Some(i), _) => { let v = vm.int_to_val(Some(i))?; vm.push(v); Ok(()) }
        (None, true) => Err(cold_value("range.index(x): x not in range")),
        (None, false) => Err(cold_value("sequence.index(x): x not in sequence")),
    }
}

pub fn range_count(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let n = range_pos(vm, recv, pos[0]).0.is_some() as i64;
    vm.push(Val::int(n));
    Ok(())
}
