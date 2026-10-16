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

/* An integer argument such as a width, refused the way an index is when it is none. */
pub(super) fn index_arg(vm: &VM, v: Val) -> Result<i64, VmErr> {
    if v.is_int() || v.is_bool() { return Ok(vm.as_i128(v).unwrap_or(0) as i64); }
    Err(VmErr::TypeMsg(crate::s!("'", str vm.type_name(v), "' object cannot be interpreted as an integer")))
}

/* A width the caller grows its result to, charged before the result is made so a huge one fails cleanly. */
pub(super) fn width_arg(vm: &VM, v: Val) -> Result<usize, VmErr> {
    let width = index_arg(vm, v)?.max(0) as usize;
    vm.heap.reserve(width)?;
    vm.heap.spend(width);
    Ok(width)
}

/* The tab size `expandtabs` takes, 8 when not given, held in a C int. */
pub(super) fn tab_size(vm: &VM, pos: &[Val]) -> Result<usize, VmErr> {
    let Some(&v) = pos.first() else { return Ok(8) };
    let n = index_arg(vm, v)?;
    if i32::try_from(n).is_err() { return Err(VmErr::Raised(String::from("OverflowError: Python int too large to convert to C int"))); }
    Ok(n.max(0) as usize)
}

/* `expandtabs` over the units of a text or of bytes, each tab padded to the next stop and a line break resetting the column, refused past `limit`. */
pub(super) fn expand_tabs<T: Copy + PartialEq>(units: impl IntoIterator<Item = T>, [tab, nl, cr, space]: [T; 4], ts: usize, limit: usize) -> Result<Vec<T>, VmErr> {
    let (mut out, mut col) = (Vec::new(), 0usize);
    for u in units {
        if u == tab {
            let n = if ts == 0 { 0 } else { ts - col % ts };
            if col.saturating_add(n) > limit { return Err(crate::vm::types::cold_heap()); }
            out.extend(core::iter::repeat_n(space, n));
            col += n;
        } else {
            if col >= limit { return Err(crate::vm::types::cold_heap()); }
            out.push(u);
            col = if u == nl || u == cr { 0 } else { col + 1 };
        }
    }
    Ok(out)
}

/* The receiver bytes borrowed in place, for methods that only read them. */
#[inline]
pub(super) fn recv_bytes_ref<'v>(vm: &'v VM, recv: Val) -> Result<&'v [u8], VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => { vm.heap.spend(b.len()); Ok(b) }
        _ => Err(cold_type("method requires a bytes receiver")),
    }
}

#[inline]
pub(super) fn recv_bytes(vm: &VM, recv: Val) -> Result<Vec<u8>, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => { vm.heap.spend(b.len()); Ok(b.clone()) }
        _ => Err(cold_type("method requires a bytes receiver")),
    }
}

#[inline]
pub(super) fn val_to_str(vm: &VM, v: Val, what: &str) -> Result<String, VmErr> {
    match vm.heap.try_get(v) {
        Some(HeapObj::Str(s)) => { vm.heap.spend(s.len()); Ok(s.clone()) }
        _ => Err(VmErr::TypeMsg(crate::s!(str what, ", not ", str vm.type_name(v)))),
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
        Some(HeapObj::Dict(rc) | HeapObj::FrozenDict(rc)) => { vm.heap.spend(rc.borrow().len()); Ok(rc.borrow().iter().collect()) }
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
        HeapObj::FrozenSet(s) => { vm.heap.spend(s.len()); Ok(s.iter().copied().collect()) }
        _ => Err(cold_type("method requires a set receiver")),
    }
}

// Reads the receiver set in place, no copy.
#[inline]
pub(super) fn set_ref<R>(vm: &VM, recv: Val, f: impl FnOnce(&ValSet, &HeapPool) -> R) -> Result<R, VmErr> {
    match vm.heap.try_get(recv) {
        Some(HeapObj::Set(rc)) => Ok(f(&rc.borrow(), &vm.heap)),
        Some(HeapObj::FrozenSet(s)) => Ok(f(s, &vm.heap)),
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
pub(super) fn iter_to_vec(vm: &mut VM, v: Val) -> Result<Vec<Val>, VmErr> { set_operand(vm, v, false) }

// The items of a set operand, an unhashable one named as a set element, or only by its type for an operation that only looks them up.
pub(super) fn set_operand(vm: &mut VM, v: Val, lookup: bool) -> Result<Vec<Val>, VmErr> {
    let items = vm.extract_iter(v)?;
    for &x in &items {
        if lookup { vm.require_hashable(x)?; } else { vm.require_element(x)?; }
        vm.keyed(x)?;
    }
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
