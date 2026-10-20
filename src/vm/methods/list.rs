use super::prelude::*;

/* `list.index`, `count` or `remove` named `name`, `eq` matches an item, the VM passes one that runs `__eq__`. */
pub(crate) fn search<H: Host>(vm: &mut H, recv: Val, name: &str, pos: &[Val], mut eq: impl FnMut(&mut H, Val, Val) -> Result<bool, VmErr>) -> Result<(), VmErr> {
    let (len, ty) = match vm.heap().try_get(recv) {
        Some(HeapObj::List(rc)) => (rc.borrow().len(), "list"),
        Some(HeapObj::Tuple(t)) => (t.len(), "tuple"),
        _ => return Err(cold_type("method requires a list receiver")),
    };
    vm.heap().spend(len);
    let len = len as i64;
    // Optional start/end clamp like Python, negatives count from the end. Bools count as ints.
    let as_i = |v: Val| -> i64 { if v.is_bool() { v.as_bool() as i64 } else { v.as_int() } };
    let norm = |v: Val| (if as_i(v) < 0 { len + as_i(v) } else { as_i(v) }).clamp(0, len) as usize;
    let (start, stop) = if name == "index" {
        // A bound past i64 clips to the inline extreme, past either end of any list.
        let bound = |k: usize| pos.get(k).map(|&v| crate::frontend::int::clip_index(vm.heap(), v)).filter(|v| v.is_int());
        let start = bound(1).map_or(0, norm);
        (start, bound(2).map_or(len as usize, norm).max(start))
    } else { (0, len as usize) };
    let mut hits = 0;
    // Each item is read live, a `__eq__` that shrinks the list ends the walk early.
    for i in start..stop {
        let Some(v) = (match vm.heap().try_get(recv) {
            Some(HeapObj::List(rc)) => rc.borrow().get(i).copied(),
            Some(HeapObj::Tuple(t)) => t.get(i).copied(),
            _ => None,
        }) else { break };
        if !eq(vm, v, pos[0])? { continue; }
        match name {
            "count" => hits += 1,
            "index" => { vm.push(Val::int(i as i64)); return Ok(()); }
            _ => {
                list_mut(vm, recv, "remove: receiver is not a list", |list| { if i < list.len() { list.remove(i); } Ok(()) })?;
                vm.push(Val::none());
                return Ok(());
            }
        }
    }
    match name {
        "count" => { vm.push(Val::int(hits)); Ok(()) }
        "index" if ty == "tuple" => Err(cold_value("tuple.index(x): x not in tuple")),
        "index" => Err(cold_value("list.index(x): x not in list")),
        _ => Err(cold_value("list.remove(x): x not in list")),
    }
}

pub fn index<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    search(vm, recv, "index", pos, |vm, a, b| Ok(eq_member(a, b, vm.heap())))
}

pub fn count<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    search(vm, recv, "count", pos, |vm, a, b| Ok(eq_member(a, b, vm.heap())))
}

pub fn copy<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let items = list_clone(vm, recv)?;
    vm.alloc_and_push_list(items)
}

pub fn append(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    list_mut(vm, recv, "append: receiver is not a list", |list| {
        list.push(pos[0]); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn clear(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    list_mut(vm, recv, "clear: receiver is not a list", |list| {
        list.clear(); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn reverse(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let moved = list_mut(vm, recv, "reverse: receiver is not a list", |list| {
        list.reverse(); Ok(list.len())
    })?;
    vm.heap.spend(moved);
    vm.push(Val::none()); Ok(())
}

pub fn extend(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let items = vm.extract_iter(pos[0])?;
    list_mut(vm, recv, "extend: receiver is not a list", |list| {
        list.extend_from_slice(&items); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn insert(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    if !pos[0].is_int() { return Err(if crate::frontend::int::is_big(&vm.heap, pos[0]) { VmErr::Overflow } else { cold_type("list indices must be integers") }); }
    let moved = list_mut(vm, recv, "insert: receiver is not a list", |list| {
        let i = pos[0].as_int();
        let ui = if i < 0 {
            (list.len() as i64).saturating_add(i).max(0) as usize
        } else {
            (i as usize).min(list.len())
        };
        list.insert(ui, pos[1]);
        Ok(list.len() - ui)
    })?;
    // The items after the slot shift, counted as native work.
    vm.heap.spend(moved);
    vm.push(Val::none()); Ok(())
}

pub fn remove(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    search(vm, recv, "remove", pos, |vm, a, b| Ok(eq_member(a, b, &vm.heap)))
}

pub fn pop(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    // An index past i64 overflows before the list is looked at.
    if pos.first().is_some_and(|&i| crate::frontend::int::is_big(&vm.heap, i)) { return Err(VmErr::Overflow); }
    let (popped, moved) = list_mut(vm, recv, "pop: receiver is not a list", |list| {
        if list.is_empty() { return Err(cold_index("pop from empty list")); }
        if pos.is_empty() { return Ok((list.pop().unwrap(), 0)); }
        if !pos[0].is_int() { return Err(cold_type("list indices must be integers")); }
        let i = pos[0].as_int();
        let ui = if i < 0 {
            let adj = (list.len() as i64).saturating_add(i);
            if adj < 0 { return Err(cold_index("pop index out of range")); }
            adj as usize
        } else { i as usize };
        if ui >= list.len() { return Err(cold_index("pop index out of range")); }
        Ok((list.remove(ui), list.len() - ui))
    })?;
    vm.heap.spend(moved);
    vm.push(popped); Ok(())
}

/* `list.__init__(self, iterable)`, the list takes the items of `iterable` in place of its own. */
pub fn init(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let items = match pos.first() { Some(&src) => vm.extract_iter(src)?, None => Vec::new() };
    list_mut(vm, recv, "__init__: receiver is not a list", |list| { *list = items; Ok(()) })?;
    vm.push(Val::none());
    Ok(())
}
