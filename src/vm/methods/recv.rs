use alloc::{string::String, vec::Vec};

use crate::vm::{VM, Val, VmErr, HeapObj, DictMap};
use crate::vm::types::{ValSet, HeapPool, cold_type};

/* Receiver-type unwrap and arity-check primitives shared by the method dispatcher in mod.rs. */
#[inline]
pub(super) fn recv_str(vm: &VM, recv: Val) -> Result<String, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Str(s)) => { vm.heap.spend(s.len()); Ok(s.clone()) }
        _ => Err(cold_type("method requires a string receiver")),
    }
}

/* The receiver string borrowed in place, for methods that only read it. */
#[inline]
pub(super) fn recv_str_ref<'v>(vm: &'v VM, recv: Val) -> Result<&'v str, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Str(s)) => { vm.heap.spend(s.len()); Ok(s) }
        _ => Err(cold_type("method requires a string receiver")),
    }
}

/* The receiver bytes borrowed in place, for methods that only read them. */
#[inline]
pub(super) fn recv_bytes_ref<'v>(vm: &'v VM, recv: Val) -> Result<&'v [u8], VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Bytes(b)) => { vm.heap.spend(b.len()); Ok(b) }
        _ => Err(cold_type("method requires a bytes receiver")),
    }
}

#[inline]
pub(super) fn recv_bytes(vm: &VM, recv: Val) -> Result<Vec<u8>, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Bytes(b)) => { vm.heap.spend(b.len()); Ok(b.clone()) }
        _ => Err(cold_type("method requires a bytes receiver")),
    }
}

#[inline]
pub(super) fn val_to_str(vm: &VM, v: Val) -> Result<String, VmErr> {
    match vm.heap.try_get(v) {
        Some(HeapObj::Str(s)) => { vm.heap.spend(s.len()); Ok(s.clone()) }
        _ => Err(cold_type("argument must be a string")),
    }
}

#[inline]
pub(super) fn list_clone(vm: &VM, recv: Val) -> Result<Vec<Val>, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::List(rc)) => { vm.heap.spend(rc.borrow().len()); Ok(rc.borrow().clone()) }
        _ => Err(cold_type("method requires a list receiver")),
    }
}

#[inline]
pub(super) fn dict_entries(vm: &VM, recv: Val) -> Result<Vec<(Val, Val)>, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Dict(rc)) => { vm.heap.spend(rc.borrow().len()); Ok(rc.borrow().iter().collect()) }
        _ => Err(cold_type("method requires a dict receiver")),
    }
}

/* Borrow the list of `recv` for `f`, the closure cannot touch `vm`. */
#[inline]
pub(super) fn list_mut<F, R>(vm: &mut VM, recv: Val, err: &'static str, f: F) -> Result<R, VmErr>
where F: FnOnce(&mut Vec<Val>) -> Result<R, VmErr>
{
    match vm.heap.try_get(recv) {
        Some(HeapObj::List(rc)) => f(&mut rc.borrow_mut()),
        _ => Err(cold_type(err)),
    }
}

// Same shape as `list_mut` for dict receivers, clones the Rc so `&heap` stays free for content hashing.
#[inline]
pub(super) fn dict_mut<F, R>(vm: &mut VM, recv: Val, err: &'static str, f: F) -> Result<R, VmErr>
where F: FnOnce(&mut DictMap, &HeapPool) -> Result<R, VmErr>
{
    let rc = match vm.heap.try_get(recv) {
        Some(HeapObj::Dict(rc)) => rc.clone(),
        _ => return Err(cold_type(err)),
    };
    f(&mut rc.borrow_mut(), &vm.heap)
}

// Snapshot a set as Vec so the heap stays free for subsequent allocations.
#[inline]
pub(super) fn set_clone(vm: &VM, recv: Val) -> Result<Vec<Val>, VmErr> {
    match vm.heap.get(recv) {
        HeapObj::Set(rc) => { vm.heap.spend(rc.borrow().len()); Ok(rc.borrow().iter().copied().collect()) }
        _ => Err(cold_type("method requires a set receiver")),
    }
}

// Reads the receiver set in place, no copy.
#[inline]
pub(super) fn set_ref<R>(vm: &VM, recv: Val, f: impl FnOnce(&ValSet, &HeapPool) -> R) -> Result<R, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Set(rc)) => Ok(f(&rc.borrow(), &vm.heap)),
        _ => Err(cold_type("method requires a set receiver")),
    }
}

// Same shape as `list_mut` for set receivers, clones the Rc so `&heap` stays free for content hashing.
#[inline]
pub(super) fn set_mut<F, R>(vm: &mut VM, recv: Val, err: &'static str, f: F) -> Result<R, VmErr>
where F: FnOnce(&mut ValSet, &HeapPool) -> Result<R, VmErr>
{
    let rc = match vm.heap.try_get(recv) {
        Some(HeapObj::Set(rc)) => rc.clone(),
        _ => return Err(cold_type(err)),
    };
    f(&mut rc.borrow_mut(), &vm.heap)
}

// `Vec<Val>` from any iterable (str/range/dict/bytes/frozenset/list/tuple/set) for set ops, so every item must hash as a set element.
#[inline]
pub(super) fn iter_to_vec(vm: &mut VM, v: Val) -> Result<Vec<Val>, VmErr> {
    let items = vm.extract_iter(v)?;
    for &x in &items { vm.require_element(x)?; }
    Ok(items)
}

#[inline]
pub(super) fn capitalize_first(s: &str) -> String {
    let mut cs = s.chars();
    let mut out = String::with_capacity(s.len());
    if let Some(c) = cs.next() { crate::util::uni::push_title(&mut out, c); }
    out.push_str(&cs.as_str().to_lowercase());
    out
}

#[inline]
pub(super) fn title_case(s: &str) -> String {
    // each maximal run of cased chars is a word, first char titlecased, rest lowercased, non-cased chars (spaces, digits, punctuation) are boundaries.
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for c in s.chars() {
        if crate::util::uni::is_cased(c) {
            if prev_cased { out.extend(c.to_lowercase()); } else { crate::util::uni::push_title(&mut out, c); }
            prev_cased = true;
        } else {
            out.push(c);
            prev_cased = false;
        }
    }
    out
}
