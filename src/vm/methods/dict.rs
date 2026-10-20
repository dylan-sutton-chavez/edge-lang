use super::prelude::*;

// A view that reads `recv` live, so later changes to the dict show through it.
fn push_view(vm: &mut VM, recv: Val, kind: View) -> Result<(), VmErr> {
    if vm.heap.dict_cell(recv).is_none() { return Err(cold_type("method requires a dict receiver")); }
    let v = vm.heap.alloc(HeapObj::DictView(recv, kind))?;
    vm.push(v);
    Ok(())
}

pub fn keys(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { push_view(vm, recv, View::Keys) }

pub fn values(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { push_view(vm, recv, View::Values) }

pub fn items(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { push_view(vm, recv, View::Items) }

// `view.isdisjoint(other)`, true when no item of `other` is in the view.
pub fn view_isdisjoint<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let other = iter_to_vec(vm, pos[0])?;
    let Some(&HeapObj::DictView(d, kind)) = vm.heap().try_get(recv) else { return Err(cold_type("isdisjoint requires a dict view")) };
    let mine = vm.view_items(d, kind)?;
    let set = vm.valset_of(&mine)?;
    let disjoint = !other.iter().any(|&v| set.contains(v, vm.heap()));
    vm.push(Val::bool(disjoint));
    Ok(())
}

// `dict.copy()`, shallow copy, mutations don't affect the original.
pub fn frozen_copy<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { vm.push(recv); Ok(()) }

pub fn copy<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let entries = dict_entries(vm, recv)?;
    let mut dm = DictMap::with_capacity(entries.len());
    for (k, v) in entries { dm.insert(k, v, vm.heap()); }
    vm.alloc_and_push_dict(dm)
}

// `dict.clear()`, remove all entries in place.
pub fn clear(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    dict_mut(vm, recv, "clear: receiver is not a dict", |dict, _heap| {
        dict.clear(); Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

// `dict.popitem()`, pop the last (k, v), KeyError on empty dict.
pub fn popitem(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let pair = dict_mut(vm, recv, "popitem: receiver is not a dict", |dict, heap| {
        Ok(dict.last().inspect(|(k, _)| { dict.remove(k, heap); }))
    })?;
    let Some((k, v)) = pair else { return Err(vm.text_key_error("popitem(): dictionary is empty")) };
    vm.alloc_and_push_tuple(vec![k, v])
}

// `dict.fromkeys(iterable, value=None)` classmethod, new dict mapping each key to `value`.
pub fn fromkeys<H: Host>(vm: &mut H, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let keys = vm.extract_iter(pos[0])?;
    for &k in &keys { vm.require_key(k)?; vm.keyed(k)?; }
    let value = pos.get(1).copied().unwrap_or(Val::none());
    let mut dm = DictMap::with_capacity(keys.len());
    for k in keys { dm.insert(k, value, vm.heap()); }
    vm.alloc_and_push_dict(dm)
}

pub fn get<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.keyed(pos[0])?;
    let default = if pos.len() == 2 { pos[1] } else { Val::none() };
    let Some(rc) = vm.heap().dict_cell(recv) else { return Err(cold_type("get: receiver is not a dict")) };
    let found = rc.borrow().get(&pos[0], vm.heap()).copied();
    if found.is_none() { vm.require_key(pos[0])?; }
    vm.push(found.unwrap_or(default)); Ok(())
}

pub fn update(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    // Merge each source in order, dispatcher packs kwargs as trailing dict.
    let mut pairs: Vec<(Val, Val)> = Vec::new();
    for &src in pos { pairs.extend(vm.pairs_of(src)?); }
    dict_mut(vm, recv, "update: receiver is not a dict", |dict, heap| {
        for (k, v) in pairs { dict.insert(k, v, heap); }
        Ok(())
    })?;
    vm.push(Val::none()); Ok(())
}

pub fn pop(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    vm.keyed(pos[0])?;
    let default = if pos.len() == 2 { Some(pos[1]) } else { None };
    let removed = dict_mut(vm, recv, "pop: receiver is not a dict", |dict, heap| Ok(dict.remove(&pos[0], heap)))?;
    if removed.is_none() { vm.require_key(pos[0])?; }
    let result = match removed {
        Some(val) => val,
        None => match default {
            Some(d) => d,
            // raises KeyError whose str is the missing key's repr.
            None => return Err(vm.key_error(pos[0])),
        },
    };
    vm.push(result); Ok(())
}

pub fn setdefault(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let default = if pos.len() > 1 { pos[1] } else { Val::none() };
    vm.require_key(pos[0])?;
    vm.keyed(pos[0])?;
    let result = dict_mut(vm, recv, "setdefault: receiver is not a dict", |dict, heap| {
        if let Some(v) = dict.get(&pos[0], heap).copied() { Ok(v) }
        else { dict.insert(pos[0], default, heap); Ok(default) }
    })?;
    vm.push(result); Ok(())
}
