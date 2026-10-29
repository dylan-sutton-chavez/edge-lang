use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use edge::lang::{Acc, Args, Cx, Flow, Fold, Intrinsic, Item, PUSH, Ret, STEPS, Sig, Step, TYPE};
use edge::value::foreign::{Kind, Snapshot};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::fail::{Fail, fail, receiver};
use crate::iter::items::{drained, gather, generator, plain_items, taken};
use crate::{Kernel, Site};

pub mod sort;
mod timsort;
pub mod top;

/* the kind of the slice a key of bounds makes under the type name `type_name` */
pub const fn slice<L: crate::Lang>(type_name: &'static str) -> Kind {
    Kind {
        name: "bounds",
        type_name,
        family: "bounds",
        // two slices of equal bounds are equal and hash each bound as a table does
        hash: |heap, p| p.downcast_ref::<[Val; 3]>().map_or(0, |b| b.iter().fold(0x5_11CE, |h: u64, &v| (h ^ edge::value::hash_val_with_heap(v, heap)).wrapping_mul(0x100_0000_01B3))),
        eq: |heap, p, other| p.downcast_ref::<[Val; 3]>().zip(heap.slice_of(other)).is_some_and(|(a, b)| a.iter().zip(b).all(|(&x, y)| edge::value::eq_vals_with_heap(x, y, heap))),
        get: Some(bound::<L>),
        snapshot: Snapshot::Store,
        ..Kind::PLAIN
    }
}

/* the start stop or step of a slice under the runtime's field names */
fn bound<L: crate::Lang>(heap: &mut HeapPool, v: Val, name: &str) -> Result<Option<Val>, VmErr> {
    Ok(heap.slice_of(v).zip(L::NAMES.fields.bound(name)).map(|(b, i)| b[i]))
}

/* how many items a container holds */
pub const fn len<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), len_run).fast(len_of).intrinsic(Intrinsic::Len) }

/* a new list of every item of an iterable */
pub const fn list<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), list_run).fast(list_fast).flags(TYPE | STEPS) }

/* a new tuple of every item of an iterable where a tuple is itself */
pub const fn tuple<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), tuple_run).fast(tuple_fast).flags(TYPE | STEPS) }

/* where an item first sits between a start and a stop */
pub const fn index<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 3), index_run::<S>) }

/* how many items equal a value */
pub const fn count<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), count_run::<S>) }

/* removes the first item equal to a value */
pub const fn remove<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), remove_run::<S>) }

/* a new list of the same items */
pub const fn copy<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), copy_run) }

/* adds an item at the end which a call on a local lowers in place */
pub const fn push<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), push_run).flags(PUSH) }

/* removes every item */
pub const fn clear<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), clear_run) }

/* turns the items around in place */
pub const fn reverse<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), reverse_run) }

/* adds every item of an iterable at the end */
pub const fn extend<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), extend_run).flags(STEPS) }

/* puts an item before an index */
pub const fn insert<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), insert_run::<S>) }

/* removes and gives the item at an index or the last */
pub const fn pop<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), pop_run::<S>) }

fn len_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let n = cx.len(a.pos[0])?;
    Ok(cx.new_int(n)?.into())
}

/* the length the loop takes with no call and none where the full call answers */
fn len_of(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let n = match heap.try_get(v)? {
        HeapObj::List(l) => l.borrow().len(),
        HeapObj::Tuple(t) => t.len(),
        HeapObj::Dict(d) => d.borrow().len(),
        HeapObj::Set(s) => s.borrow().len(),
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b.len(),
        HeapObj::Str(_) => heap.text_len(v)?,
        &HeapObj::Range(s, e, st) => return word(crate::iter::span(s, e, st)?),
        _ => return None,
    };
    i64::try_from(n).ok().and_then(Val::int_checked)
}

/* a count as an inline int and none past what one holds */
fn word(n: i128) -> Option<Val> { i64::try_from(n).ok().and_then(Val::int_checked) }

fn list_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(src) = a.get(0) else { return Ok(cx.new_list(Vec::new())?.into()) };
    if generator(cx, src) { return if cx.spent(src) { Ok(cx.new_list(Vec::new())?.into()) } else { Ok(drained(src, list_end, Acc::new())) }; }
    if let Some(v) = list_fast(cx.heap_mut(), src) { return Ok(v.into()); }
    match cx.items(src)? {
        Some(items) => Ok(cx.new_list(items)?.into()),
        None => Ok(drained(src, list_end, Acc::new())),
    }
}

/* the end of a list drain making the list of every item it gathered */
fn list_end(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return taken(acc, cx, item) };
    Ok(Flow::Stop(cx.new_list(core::mem::take(&mut acc.items))?))
}

/* a list of a list tuple or range copied with no call */
#[inline]
fn list_fast(heap: &mut HeapPool, x: Val) -> Option<Val> {
    if !copies(heap, x) { return None; }
    let items = plain_items(heap, x)?;
    heap.alloc(HeapObj::List(Rc::new(RefCell::new(items)))).ok()
}

fn tuple_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(src) = a.get(0) else { return Ok(cx.new_tuple(Vec::new())?.into()) };
    if generator(cx, src) { return if cx.spent(src) { Ok(cx.new_tuple(Vec::new())?.into()) } else { Ok(drained(src, tuple_end, Acc::new())) }; }
    if let Some(v) = tuple_fast(cx.heap_mut(), src) { return Ok(v.into()); }
    match cx.items(src)? {
        Some(items) => Ok(cx.new_tuple(items)?.into()),
        None => Ok(drained(src, tuple_end, Acc::new())),
    }
}

/* the end of a tuple drain making the tuple of every item it gathered */
fn tuple_end(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return taken(acc, cx, item) };
    Ok(Flow::Stop(cx.new_tuple(core::mem::take(&mut acc.items))?))
}

/* a tuple of a tuple as itself and of a list or range copied */
#[inline]
fn tuple_fast(heap: &mut HeapPool, x: Val) -> Option<Val> {
    if let Some(HeapObj::Tuple(_)) = heap.try_get(x) { return Some(x); }
    if !copies(heap, x) { return None; }
    let items = plain_items(heap, x)?;
    heap.alloc(HeapObj::Tuple(items)).ok()
}

/* whether the items of `x` copy with no call */
#[inline(always)]
fn copies(heap: &HeapPool, x: Val) -> bool { matches!(heap.try_get(x), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Range(..))) }


fn index_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { search::<S>(cx, a.recv, Search::Index, a.pos) }

fn count_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { search::<S>(cx, a.recv, Search::Count, a.pos) }

fn remove_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { search::<S>(cx, a.recv, Search::Remove, a.pos) }

#[derive(Clone, Copy, PartialEq)]
enum Search { Index, Count, Remove }

/* index count or remove of `pos[0]` over the items which no comparison can change */
fn search<S: Site>(cx: &mut Cx<'_, '_>, recv: Val, what: Search, pos: &[Val]) -> Ret {
    let len = match cx.heap().try_get(recv) {
        Some(HeapObj::List(rc)) => rc.borrow().len(),
        Some(HeapObj::Tuple(t)) => t.len(),
        _ => return Err(receiver()),
    };
    cx.spend(len);
    // an index search clamps its bounds as every window does
    let (start, stop) = if what == Search::Index {
        crate::num::seq_bounds(cx, pos.get(1).copied(), pos.get(2).copied(), len).map_err(|e| e.raise::<S>(cx))?.unwrap_or((len, len))
    } else { (0, len) };
    let (x, count) = (pos[0], what == Search::Count);
    // two inline ints compare by value with no lookup
    let scan = |items: &[Val]| -> Result<(Option<usize>, i64), VmErr> {
        let mut hits = 0;
        for (i, &v) in items.iter().enumerate() {
            let same = if v.is_int() && x.is_int() { v.as_int() == x.as_int() } else { cx.eq(v, x)? };
            if !same { continue; }
            if !count { return Ok((Some(start + i), 0)); }
            hits += 1;
        }
        Ok((None, hits))
    };
    let (at, hits) = match cx.heap().try_get(recv) {
        Some(HeapObj::List(rc)) => { let items = rc.borrow(); scan(items.get(start..stop).unwrap_or(&[]))? }
        Some(HeapObj::Tuple(t)) => scan(t.get(start..stop).unwrap_or(&[]))?,
        _ => (None, 0),
    };
    match (what, at) {
        (Search::Count, _) => Ok(Val::int(hits).into()),
        (Search::Index, Some(i)) => Ok(Val::int(i as i64).into()),
        (Search::Remove, Some(i)) => {
            if let Some(HeapObj::List(rc)) = cx.heap().try_get(recv) && i < rc.borrow().len() { rc.borrow_mut().remove(i); }
            Ok(Val::none().into())
        }
        (_, None) => Err(fail::<S>(cx, Fail::NotFound(recv))),
    }
}

/* the list `recv` holds through `f` where any other receiver fails */
fn with_list<R>(cx: &mut Cx<'_, '_>, recv: Val, f: impl FnOnce(&mut Vec<Val>) -> R) -> Result<R, VmErr> {
    match cx.heap().try_get(recv) {
        Some(HeapObj::List(rc)) => Ok(f(&mut rc.borrow_mut())),
        _ => Err(receiver()),
    }
}

fn copy_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let items = with_list(cx, a.recv, |l| l.clone())?;
    cx.spend(items.len());
    Ok(cx.new_list(items)?.into())
}

fn push_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    with_list(cx, a.recv, |l| l.push(a.pos[0]))?;
    Ok(Val::none().into())
}

fn clear_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    with_list(cx, a.recv, Vec::clear)?;
    Ok(Val::none().into())
}

fn reverse_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let moved = with_list(cx, a.recv, |l| { l.reverse(); l.len() })?;
    cx.spend(moved);
    Ok(Val::none().into())
}

/* takes a container at once and user steps after a drain gathers them */
fn extend_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(items) = cx.items(a.pos[0])? else {
        let mut acc = Acc::new();
        acc.v[0] = a.recv;
        return Ok(Step::Drain { it: a.pos[0], fold: Fold::collect(extend_fold, acc) }.into());
    };
    with_list(cx, a.recv, |l| l.extend_from_slice(&items))?;
    Ok(Val::none().into())
}

/* the drain of `extend` whose end adds every item at once */
fn extend_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => gather(acc, cx, v),
        Item::End(_) => {
            let items = core::mem::take(&mut acc.items);
            with_list(cx, acc.v[0], |l| l.extend_from_slice(&items))?;
            Ok(Flow::Stop(Val::none()))
        }
        Item::Called(_) => Ok(Flow::More),
    }
}

fn insert_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (at, v) = (a.pos[0], a.pos[1]);
    // a bool is its int and any int a word holds clamps to the list
    let i = match crate::num::word(cx, at) {
        Some(r) => r?,
        None => return Err(fail::<S>(cx, Fail::IndexType(a.recv))),
    };
    let moved = with_list(cx, a.recv, |l| {
        // an index past what a pointer holds lies past the end on any target
        let ui = if i < 0 { (l.len() as i64).saturating_add(i).max(0) as usize } else { usize::try_from(i).map_or(l.len(), |u| u.min(l.len())) };
        l.insert(ui, v);
        l.len() - ui
    })?;
    // the items past the slot shift as native work
    cx.spend(moved);
    Ok(Val::none().into())
}

fn pop_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    // the index is read before the list and only one past a word overflows
    let at = match a.get(0).map(|v| crate::num::word(cx, v)) {
        None => None,
        Some(Some(r)) => Some(r?),
        Some(None) => return Err(fail::<S>(cx, Fail::IndexType(a.recv))),
    };
    let popped = with_list(cx, a.recv, |l| {
        let Some(i) = at else { return l.pop().map(|v| (v, 0)).ok_or_else(|| Fail::EmptyPop(Val::undef())) };
        if l.is_empty() { return Err(Fail::EmptyPop(Val::undef())); }
        let ui = if i < 0 {
            let adj = (l.len() as i64).saturating_add(i);
            if adj < 0 { return Err(Fail::PopRange); }
            adj as usize
        } else { usize::try_from(i).map_err(|_| Fail::PopRange)? };
        if ui >= l.len() { return Err(Fail::PopRange); }
        Ok((l.remove(ui), l.len() - ui))
    })?;
    let (popped, moved) = match popped {
        Ok(p) => p,
        // the receiver names the kind once the borrow of its items is over
        Err(Fail::EmptyPop(_)) => return Err(fail::<S>(cx, Fail::EmptyPop(a.recv))),
        Err(Fail::IndexType(_)) => return Err(fail::<S>(cx, Fail::IndexType(a.recv))),
        Err(f) => return Err(fail::<S>(cx, f)),
    };
    cx.spend(moved);
    Ok(popped.into())
}
