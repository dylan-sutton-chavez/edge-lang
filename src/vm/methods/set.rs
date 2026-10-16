use super::prelude::*;

/* A set probes as the frozenset it equals and never matches the set being changed, anything else must hash. */
fn probes(vm: &mut VM, recv: Val, v: Val) -> Result<bool, VmErr> {
    if v.0 == recv.0 { return Ok(false); }
    vm.require_set_probe(v)?;
    vm.keyed(v)?;
    Ok(true)
}

pub fn add(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.require_element(pos[0])?;
    vm.keyed(pos[0])?;
    set_mut(vm, recv, "add: receiver is not a set", |set, heap| {
        set.insert(pos[0], heap); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn remove(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    // A missing element raises KeyError with the element, not ValueError.
    if !probes(vm, recv, pos[0])? || !set_mut(vm, recv, "remove: receiver is not a set", |set, heap| Ok(set.remove(pos[0], heap)))? { return Err(vm.key_error(pos[0])); }
    vm.push(Val::none()); Ok(())
}

pub fn discard(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    if probes(vm, recv, pos[0])? {
        set_mut(vm, recv, "discard: receiver is not a set", |set, heap| {
            set.remove(pos[0], heap); Ok(())
        })?;
    }
    vm.push(Val::none()); Ok(())
}

pub fn pop(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    // The first element in order goes, an empty set raises.
    let popped = set_mut(vm, recv, "pop: receiver is not a set", |set, heap| {
        let pick = set.iter().next().copied();
        if let Some(p) = pick { set.remove(p, heap); }
        Ok(pick)
    })?;
    let Some(popped) = popped else { return Err(vm.text_key_error("pop from an empty set")) };
    vm.push(popped); Ok(())
}

pub fn clear(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    set_mut(vm, recv, "clear: receiver is not a set", |set, _heap| {
        set.clear(); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

// Materialize every argument iterable up front (each may run iteration code).
fn collect_args(vm: &mut VM, pos: &[Val]) -> Result<Vec<Vec<Val>>, VmErr> {
    pos.iter().map(|&a| iter_to_vec(vm, a)).collect()
}

// The arguments of an intersection, whose items are only looked up in the set.
fn lookup_args(vm: &mut VM, pos: &[Val]) -> Result<Vec<Vec<Val>>, VmErr> {
    pos.iter().map(|&a| set_operand(vm, a, true)).collect()
}

pub fn update(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = collect_args(vm, pos)?;
    set_mut(vm, recv, "update: receiver is not a set", |set, heap| {
        for items in args { for v in items { set.insert(v, heap); } }
        Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn frozen_copy(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { vm.push(recv); Ok(()) }

pub fn copy(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let items = set_clone(vm, recv)?;
    vm.alloc_and_push_set(items)
}

pub fn union(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = collect_args(vm, pos)?;
    // alloc_and_push_set dedups by content, so concatenating every source is enough.
    let mut out = set_clone(vm, recv)?;
    for items in args { out.extend(items); }
    vm.alloc_and_push_set(out)
}

pub fn intersection(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = lookup_args(vm, pos)?;
    let lhs = set_clone(vm, recv)?;
    let arg_sets: Vec<ValSet> = args.iter().map(|it| ValSet::from_vals(it, &vm.heap)).collect();
    let out: Vec<Val> = lhs.into_iter().filter(|&v| arg_sets.iter().all(|s| s.contains(v, &vm.heap))).collect();
    vm.alloc_and_push_set(out)
}

pub fn difference(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = collect_args(vm, pos)?;
    let lhs = set_clone(vm, recv)?;
    let arg_sets: Vec<ValSet> = args.iter().map(|it| ValSet::from_vals(it, &vm.heap)).collect();
    let out: Vec<Val> = lhs.into_iter().filter(|&v| !arg_sets.iter().any(|s| s.contains(v, &vm.heap))).collect();
    vm.alloc_and_push_set(out)
}

pub fn symmetric_difference(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let lhs = set_clone(vm, recv)?;
    let rhs = iter_to_vec(vm, pos[0])?;
    let lset = ValSet::from_vals(&lhs, &vm.heap);
    let rset = ValSet::from_vals(&rhs, &vm.heap);
    let mut out: Vec<Val> = lhs.iter().filter(|&&v| !rset.contains(v, &vm.heap)).copied().collect();
    out.extend(rhs.iter().filter(|&&v| !lset.contains(v, &vm.heap)).copied());
    vm.alloc_and_push_set(out)
}

pub fn intersection_update(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = lookup_args(vm, pos)?;
    set_mut(vm, recv, "intersection_update: receiver is not a set", |set, heap| {
        let arg_sets: Vec<ValSet> = args.iter().map(|it| ValSet::from_vals(it, heap)).collect();
        let keep: Vec<Val> = set.iter().filter(|&&v| arg_sets.iter().all(|s| s.contains(v, heap))).copied().collect();
        set.clear();
        for v in keep { set.insert(v, heap); }
        Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn difference_update(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = collect_args(vm, pos)?;
    set_mut(vm, recv, "difference_update: receiver is not a set", |set, heap| {
        for items in args { for v in items { set.remove(v, heap); } }
        Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn symmetric_difference_update(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let other = iter_to_vec(vm, pos[0])?;
    set_mut(vm, recv, "symmetric_difference_update: receiver is not a set", |set, heap| {
        for v in other { if !set.remove(v, heap) { set.insert(v, heap); } }
        Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn issubset(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let rhs = ValSet::from_vals(&set_operand(vm, pos[0], true)?, &vm.heap);
    let hit = set_ref(vm, recv, |lhs, heap| lhs.iter().all(|&v| rhs.contains(v, heap)))?;
    vm.push(Val::bool(hit));
    Ok(())
}

pub fn issuperset(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let rhs = iter_to_vec(vm, pos[0])?;
    let hit = set_ref(vm, recv, |lhs, heap| rhs.iter().all(|&v| lhs.contains(v, heap)))?;
    vm.push(Val::bool(hit));
    Ok(())
}

pub fn isdisjoint(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let rhs = iter_to_vec(vm, pos[0])?;
    let hit = set_ref(vm, recv, |lhs, heap| !rhs.iter().any(|&v| lhs.contains(v, heap)))?;
    vm.push(Val::bool(hit));
    Ok(())
}
