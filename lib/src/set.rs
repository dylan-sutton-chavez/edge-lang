use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use edge::lang::{Acc, Args, Cx, Flow, Fold, Item, KindId, Ret, STEPS, Sig, Step, TYPE};
use edge::value::{HeapObj, HeapPool, SET_TABLE, Val, ValSet, VmErr, cold_runtime};

use crate::fail::{Fail, fail, receiver};
use crate::iter::items::{SHORT, drained, generator, range_items, taken};
use crate::{Kernel, Site};

// the bytes a set holds for an item with its hash before any index
const ENTRY: usize = 16;

/* a new set of every item of an iterable each hashable */
pub const fn new<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), new_run).fast(new_fast).flags(TYPE | STEPS) }

/* puts an item in */
pub const fn add<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), add_run) }

/* takes an item out or fails when it is missing */
pub const fn remove<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), remove_run::<S>) }

/* takes an item out when it is there */
pub const fn discard<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), discard_run) }

/* takes out and gives some item */
pub const fn pop<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), pop_run::<S>) }

/* takes every item out */
pub const fn clear<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), clear_run) }

/* a new set of the same items */
pub const fn copy<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), copy_run) }

/* the items in one operand or the other but not both */
pub const fn symmetric<S: Site>() -> Kernel { one::<S>(Op::SymDiff) }

/* keeps the items in one or the other but not both */
pub const fn symmetric_update<S: Site>() -> Kernel { one::<S>(Op::SymDiffUpdate) }

/* whether every item is in the operand */
pub const fn subset<S: Site>() -> Kernel { one::<S>(Op::Subset) }

/* whether every item of the operand is in the set */
pub const fn superset<S: Site>() -> Kernel { one::<S>(Op::Superset) }

/* whether no item is in the operand */
pub const fn disjoint<S: Site>() -> Kernel { one::<S>(Op::Disjoint) }

/* puts in the items of every operand */
pub const fn update<S: Site>() -> Kernel { many::<S>(Op::Update) }

/* a new set of the items of the set and every operand */
pub const fn union<S: Site>() -> Kernel { many::<S>(Op::Union) }

/* a new set of the items in the set and in every operand */
pub const fn intersection<S: Site>() -> Kernel { many::<S>(Op::Intersection) }

/* a new set of the items in the set and in no operand */
pub const fn difference<S: Site>() -> Kernel { many::<S>(Op::Difference) }

/* keeps only the items in every operand */
pub const fn intersection_update<S: Site>() -> Kernel { many::<S>(Op::InterUpdate) }

/* takes out the items of every operand */
pub const fn difference_update<S: Site>() -> Kernel { many::<S>(Op::DiffUpdate) }

/* a kernel reading one operand */
const fn one<S: Site>(op: Op) -> Kernel { Kernel::new(Sig::exact(1), run::<S>(op)).flags(STEPS) }

/* a kernel reading any number of operands which drain one after another */
const fn many<S: Site>(op: Op) -> Kernel { Kernel::new(Sig::var(0), run::<S>(op)).flags(STEPS) }

/* the body of method `op` */
const fn run<S: Site>(op: Op) -> edge::lang::NativeFn {
    match op {
        Op::Update => by::<S, 0>, Op::Union => by::<S, 1>, Op::Intersection => by::<S, 2>, Op::Difference => by::<S, 3>,
        Op::SymDiff => by::<S, 4>, Op::InterUpdate => by::<S, 5>, Op::DiffUpdate => by::<S, 6>, Op::SymDiffUpdate => by::<S, 7>,
        Op::Subset => by::<S, 8>, Op::Superset => by::<S, 9>, Op::Disjoint => by::<S, 10>,
    }
}

fn by<S: Site, const OP: usize>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { operands::<S>(cx, a, Op::ALL[OP]) }

/* a set of `items` each one hashable */
pub fn set_of(cx: &Cx<'_, '_>, items: &[Val]) -> Result<ValSet, VmErr> {
    for &v in items.iter().filter(|v| v.is_heap()) { cx.require_element(v)?; }
    ValSet::build(items, cx.heap())
}

fn new_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(src) = a.get(0) else { return Ok(cx.new_set(ValSet::new())?.into()) };
    if generator(cx, src) { return if cx.spent(src) { Ok(cx.new_set(ValSet::new())?.into()) } else { Ok(drained(src, new_end, Acc::new())) }; }
    if let Some(v) = copy_of(cx.heap_mut(), src) { return Ok(v?.into()); }
    match cx.items(src)? {
        Some(items) => { let s = set_of(cx, &items)?; Ok(cx.new_set(s)?.into()) }
        None => Ok(drained(src, new_end, Acc::new())),
    }
}

/* the end of a set drain making the set of every item it gathered */
fn new_end(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return taken(acc, cx, item) };
    let s = set_of(cx, &acc.items)?;
    acc.items.clear();
    Ok(Flow::Stop(cx.new_set(s)?))
}

/* the copy `copy_of` makes and none where its refusal ends the full call */
fn new_fast(heap: &mut HeapPool, x: Val) -> Option<Val> { copy_of(heap, x)?.ok() }

/* a set of a set copied with its hashes or of keys no check refuses */
fn copy_of(heap: &mut HeapPool, x: Val) -> Option<Result<Val, VmErr>> {
    let plain = |v: &Val| !v.is_heap() || matches!(heap.try_get(*v), Some(HeapObj::Str(_)));
    let s = match heap.try_get(x)? {
        HeapObj::Set(rc) => {
            let s = rc.borrow();
            // a long copy is refused before it is built and pays for its entries
            if s.len().saturating_mul(ENTRY) > SHORT && let Err(e) = heap.reserve(s.len().saturating_mul(SET_TABLE)) { return Some(Err(e)); }
            if s.len() > 64 { heap.spend(s.len()); }
            s.clone()
        }
        HeapObj::List(rc) => {
            let items = rc.borrow();
            if !items.iter().all(plain) { return None; }
            heap.spend(items.len());
            ValSet::build(&items, heap).ok()?
        }
        HeapObj::Tuple(t) => {
            if !t.iter().all(plain) { return None; }
            heap.spend(t.len());
            ValSet::build(t, heap).ok()?
        }
        // bytes read in place adding each value the first time it shows
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => {
            heap.spend(b.len());
            let mut seen = [false; 256];
            let firsts: Vec<Val> = b.iter().filter(|&&x| !core::mem::replace(&mut seen[usize::from(x)], true)).map(|&x| Val::int(i64::from(x))).collect();
            ValSet::from_vals(&firsts, heap)
        }
        &HeapObj::Range(start, stop, step) => {
            let items = range_items(heap, start, stop, step)?;
            heap.spend(items.len());
            ValSet::build(&items, heap).ok()?
        }
        _ => return None,
    };
    Some(heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s)))))
}

fn add_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    cx.require_element(a.pos[0])?;
    with_set(cx, a.recv, |s, heap| s.insert(a.pos[0], heap))?;
    Ok(Val::none().into())
}

fn remove_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let v = a.pos[0];
    if !probes(cx, a.recv, v)? || !with_set(cx, a.recv, |s, heap| s.remove(v, heap))? { return Err(fail::<S>(cx, Fail::MissingKey(v))); }
    Ok(Val::none().into())
}

fn discard_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let v = a.pos[0];
    if probes(cx, a.recv, v)? { with_set(cx, a.recv, |s, heap| s.remove(v, heap))?; }
    Ok(Val::none().into())
}

/* whether `v` may match where the set itself never does and other sets skip hashing */
fn probes(cx: &Cx<'_, '_>, recv: Val, v: Val) -> Result<bool, VmErr> {
    if v.raw() == recv.raw() { return Ok(false); }
    if cx.kind(v) != KindId::Set { cx.require_element(v)?; }
    Ok(true)
}

fn pop_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let first = with_set(cx, a.recv, |s, heap| s.pop_first(heap))?;
    match first { Some(v) => Ok(v.into()), None => Err(fail::<S>(cx, Fail::EmptySet(a.recv))) }
}

fn clear_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    with_set(cx, a.recv, |s, _| s.clear())?;
    Ok(Val::none().into())
}

fn copy_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    // a copy is refused past the memory left before it is built
    let n = look(cx, a.recv, ValSet::len)?;
    if n > 64 { cx.heap().reserve(n.saturating_mul(SET_TABLE))?; }
    let s = look(cx, a.recv, ValSet::clone)?;
    cx.spend(s.len());
    Ok(cx.new_set(s)?.into())
}

/* what a method that reads its operands whole does with them */
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op { Update, Union, Intersection, Difference, SymDiff, InterUpdate, DiffUpdate, SymDiffUpdate, Subset, Superset, Disjoint }

impl Op {
    const ALL: [Op; 11] = [
        Op::Update, Op::Union, Op::Intersection, Op::Difference, Op::SymDiff, Op::InterUpdate,
        Op::DiffUpdate, Op::SymDiffUpdate, Op::Subset, Op::Superset, Op::Disjoint,
    ];

    /* the method a drain keeps in its accumulator's count */
    fn of(n: i64) -> Result<Op, VmErr> { Op::ALL.get(n as usize).copied().ok_or_else(|| cold_runtime("a set drain lost its method")) }

    /* whether an unhashable item is named by its type alone not as a set element */
    fn lookup(self) -> bool { matches!(self, Op::Intersection | Op::InterUpdate | Op::Subset) }
}

/* reads every operand and runs `op` draining each one whose steps may run user code */
fn operands<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>, op: Op) -> Ret {
    if matches!(op, Op::Subset | Op::Superset | Op::Disjoint) && let Some(hit) = against_set(cx, op, a.recv, a.pos[0])? { return Ok(Val::bool(hit).into()); }
    match a.pos {
        &[v] => match cx.items(v)? {
            Some(items) => {
                checked(cx, op, &items)?;
                // a set's items are distinct already so each toggles once with no second set
                if op == Op::SymDiffUpdate && cx.kind(v) == KindId::Set { return Ok(edit(cx, a.recv, |s, heap| toggled(s, heap, &items))?.into()); }
                Ok(finish(cx, op, a.recv, &[items])?.into())
            }
            None => Ok(Step::Drain { it: v, fold: Fold::collect(chain_end, kept(a.recv, op)) }.into()),
        },
        pos if pos.iter().any(|&v| framed(cx, v)) => chain(cx, kept(a.recv, op), pos),
        pos => {
            let mut parts = Vec::with_capacity(pos.len());
            for &v in pos {
                let Some(items) = cx.items(v)? else { return Err(fail::<S>(cx, Fail::NotIterable(v))) };
                checked(cx, op, &items)?;
                parts.push(items);
            }
            Ok(finish(cx, op, a.recv, &parts)?.into())
        }
    }
}

/* a subset superset or disjoint test whose operand is a set read in place */
fn against_set(cx: &mut Cx<'_, '_>, op: Op, recv: Val, other: Val) -> Result<Option<bool>, VmErr> {
    let n = match cx.heap().try_get(other) {
        Some(HeapObj::Set(s)) => s.borrow().len(),
        _ => return Ok(None),
    };
    // charged as copying the operand would be
    cx.charge(n)?;
    let heap = cx.heap();
    let (Some(HeapObj::Set(lhs)), Some(HeapObj::Set(rhs))) = (heap.try_get(recv), heap.try_get(other)) else { return Err(receiver()) };
    let (lhs, rhs) = (lhs.borrow(), rhs.borrow());
    Ok(Some(match op {
        Op::Subset => every_in(heap, lhs.iter().copied(), &rhs, true)?,
        Op::Superset => every_in(heap, rhs.iter().copied(), &lhs, true)?,
        _ if rhs.len() <= lhs.len() => every_in(heap, rhs.iter().copied(), &lhs, false)?,
        _ => every_in(heap, lhs.iter().copied(), &rhs, false)?,
    }))
}

/* whether every item is in `set` when `inside` or none is when not */
fn every_in(heap: &HeapPool, items: impl IntoIterator<Item = Val>, set: &ValSet, inside: bool) -> Result<bool, VmErr> {
    for v in items {
        if set.contains(v, heap) != inside { return Ok(false); }
        heap.within_budget()?;
    }
    Ok(true)
}

/* the items of `from` every set holds when `all` or no set holds */
fn kept_in(heap: &HeapPool, from: impl IntoIterator<Item = Val>, sets: &[ValSet], all: bool) -> Result<Vec<Val>, VmErr> {
    let mut out = Vec::new();
    for v in from {
        let keep = if all { sets.iter().all(|s| s.contains(v, heap)) } else { !sets.iter().any(|s| s.contains(v, heap)) };
        if keep { out.push(v); }
        heap.within_budget()?;
    }
    Ok(out)
}

/* each item toggled in or out of `s` once */
fn toggled<'a>(s: &mut ValSet, heap: &HeapPool, items: impl IntoIterator<Item = &'a Val>) -> Result<(), VmErr> {
    for &v in items {
        if !s.remove(v, heap) { s.insert(v, heap); }
        heap.within_budget()?;
    }
    Ok(())
}

/* whether stepping `v` may need frames so the operands drain in a chain */
fn framed(cx: &Cx<'_, '_>, v: Val) -> bool { matches!(cx.kind(v), KindId::Coro | KindId::Iter) }

/* a drain's accumulator holding the receiver and the method */
fn kept(recv: Val, op: Op) -> Acc {
    let mut acc = Acc::new();
    (acc.v[0], acc.n) = (recv, op as i64);
    acc
}

/* reads the operands in order at once until one steps whose items the engine drains */
fn chain(cx: &mut Cx<'_, '_>, mut acc: Acc, pos: &[Val]) -> Ret {
    for (i, &v) in pos.iter().enumerate() {
        if let Some(items) = cx.items(v)? {
            part(&mut acc, cx, &items)?;
            continue;
        }
        // the operands after it wait in a tuple until its drain ends
        if i + 1 < pos.len() { acc.v[1] = cx.new_tuple(pos[i + 1..].to_vec())?; }
        return Ok(Step::Drain { it: v, fold: Fold::collect(chain_end, acc) }.into());
    }
    Ok(settle(cx, &mut acc)?.into())
}

/* the end of one operand's drain which reads on to the operands waiting after it */
fn chain_end(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return Ok(Flow::More) };
    acc.items.push(Val::undef());
    loop {
        // the next waiting operand counts from the second byte of the method's number
        let next = match cx.heap().try_get(acc.v[1]) {
            Some(HeapObj::Tuple(rest)) => rest.get((acc.n >> 8) as usize).copied(),
            _ => None,
        };
        let Some(v) = next else { return Ok(Flow::Stop(settle(cx, acc)?)) };
        acc.n += 1 << 8;
        match cx.items(v)? {
            Some(items) => part(acc, cx, &items)?,
            None => return Ok(Flow::Drain(v)),
        }
    }
}

/* takes one operand's items and the undef closing them refusing more than the memory left */
fn part(acc: &mut Acc, cx: &Cx<'_, '_>, items: &[Val]) -> Result<(), VmErr> {
    if (acc.items.len() + items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= cx.room() { return Err(VmErr::Heap); }
    acc.items.extend_from_slice(items);
    acc.items.push(Val::undef());
    Ok(())
}

/* checks every operand a chain gathered and runs the method on them */
fn settle(cx: &mut Cx<'_, '_>, acc: &mut Acc) -> Result<Val, VmErr> {
    let (op, items) = (Op::of(acc.n & 0xFF)?, core::mem::take(&mut acc.items));
    let mut parts: Vec<&[Val]> = items.split(|v| v.is_undef()).collect();
    // every operand ends at an undef so the split leaves an empty last part
    parts.pop();
    for p in &parts { checked(cx, op, p)?; }
    finish(cx, op, acc.v[0], &parts)
}

/* refuses an operand holding an item no set may hold */
fn checked(cx: &Cx<'_, '_>, op: Op, items: &[Val]) -> Result<(), VmErr> {
    for &v in items.iter().filter(|v| v.is_heap()) {
        if op.lookup() { cx.require_hashable(v)?; } else { cx.require_element(v)?; }
    }
    Ok(())
}

/* runs `op` on set `recv` with each operand's items already read and checked */
fn finish<P: AsRef<[Val]>>(cx: &mut Cx<'_, '_>, op: Op, recv: Val, parts: &[P]) -> Result<Val, VmErr> {
    let every = || parts.iter().flat_map(AsRef::as_ref).copied();
    let one = parts.first().map_or(&[] as &[Val], AsRef::as_ref);
    let out: Vec<Val> = match op {
        Op::Update => return edit(cx, recv, |s, heap| { for v in every() { s.insert(v, heap); heap.within_budget()?; } Ok(()) }),
        Op::DiffUpdate => return edit(cx, recv, |s, heap| { for v in every() { s.remove(v, heap); heap.within_budget()?; } Ok(()) }),
        Op::SymDiffUpdate => {
            // each distinct item toggles once however often the operand repeats it
            let other = ValSet::build(one, cx.heap())?;
            return edit(cx, recv, |s, heap| toggled(s, heap, other.iter()));
        }
        Op::InterUpdate => {
            let sets = tables(cx.heap(), parts)?;
            return edit(cx, recv, |s, heap| {
                let keep = kept_in(heap, s.iter().copied(), &sets, true)?;
                s.clear();
                for v in keep { s.insert(v, heap); }
                Ok(())
            });
        }
        Op::Subset | Op::Superset | Op::Disjoint => {
            let heap = cx.heap();
            let rhs = if op == Op::Subset { ValSet::build(one, heap)? } else { ValSet::new() };
            let hit = look(cx, recv, |lhs| match op {
                Op::Subset => every_in(heap, lhs.iter().copied(), &rhs, true),
                Op::Superset => every_in(heap, every(), lhs, true),
                _ => every_in(heap, every(), lhs, false),
            })??;
            return Ok(Val::bool(hit));
        }
        Op::Union => {
            let mut out = members(cx, recv)?;
            out.extend(every());
            out
        }
        Op::Intersection | Op::Difference => {
            let lhs = members(cx, recv)?;
            let heap = cx.heap();
            let sets = tables(heap, parts)?;
            kept_in(heap, lhs, &sets, op == Op::Intersection)?
        }
        Op::SymDiff => {
            let lhs = members(cx, recv)?;
            let heap = cx.heap();
            let (lset, rset) = (ValSet::build(&lhs, heap)?, ValSet::build(one, heap)?);
            let mut out = kept_in(heap, lhs, core::slice::from_ref(&rset), false)?;
            out.extend(kept_in(heap, one.iter().copied(), core::slice::from_ref(&lset), false)?);
            out
        }
    };
    let s = ValSet::build(&out, cx.heap())?;
    cx.new_set(s)
}

/* a set of each operand's items built within the memory left */
fn tables<P: AsRef<[Val]>>(heap: &HeapPool, parts: &[P]) -> Result<Vec<ValSet>, VmErr> { parts.iter().map(|p| ValSet::build(p.as_ref(), heap)).collect() }

/* the set `recv` holds changed through `f` where any other receiver fails */
fn with_set<R>(cx: &Cx<'_, '_>, recv: Val, f: impl FnOnce(&mut ValSet, &HeapPool) -> R) -> Result<R, VmErr> {
    let heap = cx.heap();
    match heap.try_get(recv) {
        Some(HeapObj::Set(s)) => Ok(f(&mut s.borrow_mut(), heap)),
        _ => Err(receiver()),
    }
}

/* `with_set` for a method whose value is nil */
fn edit(cx: &Cx<'_, '_>, recv: Val, f: impl FnOnce(&mut ValSet, &HeapPool) -> Result<(), VmErr>) -> Result<Val, VmErr> {
    with_set(cx, recv, f)??;
    Ok(Val::none())
}

/* the set `recv` holds read through `f` */
fn look<R>(cx: &Cx<'_, '_>, recv: Val, f: impl FnOnce(&ValSet) -> R) -> Result<R, VmErr> {
    match cx.heap().try_get(recv) {
        Some(HeapObj::Set(s)) => Ok(f(&s.borrow())),
        _ => Err(receiver()),
    }
}

/* the items of set `recv` copied out and charged for their count */
fn members(cx: &Cx<'_, '_>, recv: Val) -> Result<Vec<Val>, VmErr> {
    let items: Vec<Val> = look(cx, recv, |s| s.iter().copied().collect())?;
    cx.spend(items.len());
    Ok(items)
}
