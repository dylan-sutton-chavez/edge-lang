use super::prelude::*;

/* A set probes as the frozenset it equals and never matches the set being changed, anything else must hash. */
fn probes(vm: &VM, recv: Val, v: Val) -> Result<bool, VmErr> {
    if v.0 == recv.0 { return Ok(false); }
    vm.require_set_probe(v)?;
    Ok(true)
}

pub fn add(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.require_element(pos[0])?;
    set_mut(vm, recv, "add: receiver is not a set", |set, heap| {
        set.insert(pos[0], heap); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn remove(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    if !probes(vm, recv, pos[0])? { return Err(VmErr::Raised("KeyError".into())); }
    set_mut(vm, recv, "remove: receiver is not a set", |set, heap| {
        // KeyError, not ValueError.
        if !set.remove(pos[0], heap) { return Err(VmErr::Raised("KeyError".into())); }
        Ok(())
    })?;
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
    let popped = set_mut(vm, recv, "pop: receiver is not a set", |set, heap| {
        // No `pop()`, grab the first element via `iter()` and remove. Empty set raises.
        let pick = set.iter().next().copied().ok_or_else(|| cold_key("pop from an empty set"))?;
        set.remove(pick, heap);
        Ok(pick)
    })?;
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

pub fn update(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let args = collect_args(vm, pos)?;
    set_mut(vm, recv, "update: receiver is not a set", |set, heap| {
        for items in args { for v in items { set.insert(v, heap); } }
        Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

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
    let args = collect_args(vm, pos)?;
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
    let args = collect_args(vm, pos)?;
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
    let rhs = ValSet::from_vals(&iter_to_vec(vm, pos[0])?, &vm.heap);
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
