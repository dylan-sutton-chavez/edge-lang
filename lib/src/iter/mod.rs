use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use edge::lang::{Acc, Args, Cx, Flow, Fold, Intrinsic, Item, KindId, Out, Ret, STEPS, Sent, Sig, Step, TYPE};
use edge::value::{HeapObj, HeapPool, IterFrame, Val, VmErr};

use crate::fail::{Fail, fail, receiver};
use crate::lang::Derived;
use crate::{Kernel, Lang, Site};

pub mod fold;
pub mod items;
pub use items::{gather, plain_items, plain_iter, seq};

/* the ints from a start to a stop by a step */
pub const fn range<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 3), range_run::<S>).fast(range_to).flags(TYPE).intrinsic(Intrinsic::Range) }

/* an iterator over a value or over calls until a sentinel the runtime allows */
pub const fn iter<S: Site>() -> Kernel { Kernel::new(if <S::Lang as Lang>::SENTINEL { Sig::range(1, 2) } else { Sig::exact(1) }, iter_run::<S>).fast(plain_iter::<S::Lang>) }

/* each item beside its count from a start */
pub const fn numbered<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), numbered_run::<S>).fast(numbered_of::<S::Lang>) }

/* the items of several iterables side by side where a strict walk checks lengths */
pub const fn zipped<S: Site>() -> Kernel { Kernel::new(Sig::var(0), zipped_run::<S>).fast(zipped_of::<S::Lang>).reads(1) }

/* a function called on the items of several iterables side by side */
pub const fn mapped<S: Site>() -> Kernel { Kernel::new(Sig::var(2), mapped_run::<S>) }

/* the items a function finds true */
pub const fn filtered<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), filtered_run::<S>) }

/* the items of a sequence from its end */
pub const fn backward<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), backward_run::<S>).fast(backward_of::<S::Lang>) }

/* the next item of an iterator or a default once it ends */
pub const fn next<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), next_run::<S>).flags(STEPS).intrinsic(Intrinsic::Next) }

/* where an int sits in a range */
pub const fn range_index<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), range_index_run::<S>) }

/* how many times a value appears in a range */
pub const fn range_count<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), range_count_run) }

/* resumes a generator with a value */
pub const fn send<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), send_run).flags(STEPS) }

/* resumes a generator by raising in it with a value where the runtime takes one */
pub const fn throw<S: Site>() -> Kernel { Kernel::new(if <S::Lang as Lang>::THROW_VALUE { Sig::range(1, 2) } else { Sig::exact(1) }, throw_run).flags(STEPS) }

/* ends a generator at the point it waits */
pub const fn close<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), close_run).flags(STEPS) }

// a range past the word with this many items or fewer reads them up front
const FEW_BACK: i128 = 64;

/* the item count of a word range and none for a zero step */
pub(crate) fn span(s: i64, e: i64, st: i64) -> Option<i128> {
    let (lo, hi, step) = match st {
        0 => return None,
        1.. => (s as i128, e as i128, st as i128),
        _ => (e as i128, s as i128, -(st as i128)),
    };
    Some(if hi > lo { (hi - lo + step - 1) / step } else { 0 })
}

fn range_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (s, e, st) = match *a.pos {
        [e] => (Val::int(0), e, Val::int(1)),
        [s, e] => (s, e, Val::int(1)),
        [s, e, st, ..] => (s, e, st),
        [] => return Err(fail::<S>(cx, Fail::RangeArgs)),
    };
    if let (Some(s), Some(e), Some(st)) = (in_word(cx, s), in_word(cx, e), in_word(cx, st)) {
        if st == 0 { return Err(fail::<S>(cx, Fail::ZeroStep)); }
        return Ok(cx.heap_mut().alloc(HeapObj::Range(s, e, st))?.into());
    }
    // the first bound that is no int names its type
    if let Some(&bad) = [s, e, st].iter().find(|&&v| !int_like(cx, v)) { return Err(fail::<S>(cx, Fail::NotIndex(bad))); }
    wide_range::<S>(cx, s, e, st)
}

/* the range up to an inline int made with no call */
fn range_to(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let e = if v.is_int() { v.as_int() } else { crate::num::bool_int(heap, v)? };
    heap.alloc(HeapObj::Range(0, e, 1)).ok()
}

/* a range with a bound past a word kept as its three ints */
fn wide_range<S: Site>(cx: &mut Cx<'_, '_>, s: Val, e: Val, st: Val) -> Ret {
    if cx.int(st) == Some(0) { return Err(fail::<S>(cx, Fail::ZeroStep)); }
    let norm = |v: Val| if v.is_bool() { Val::int(v.as_bool() as i64) } else { v };
    Ok(cx.heap_mut().alloc(HeapObj::BigRange(norm(s), norm(e), norm(st)))?.into())
}

/* whether `v` is an int of any width or a bool where bools are ints */
fn int_like(cx: &Cx<'_, '_>, v: Val) -> bool { v.is_int() || crate::num::bool_int(cx.heap(), v).is_some() || cx.heap().is_wide(v) }

/* an int that fits a machine word where no other foreign value counts */
fn in_word(cx: &Cx<'_, '_>, v: Val) -> Option<i64> {
    if v.is_foreign() && !cx.heap().is_wide(v) { return None; }
    cx.int(v).and_then(|i| i64::try_from(i).ok())
}

fn iter_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(sentinel) = a.get(1) else { return Ok(cx.iter_of(a.pos[0])?.into()) };
    let f = a.pos[0];
    if !cx.callable(f) { return Err(fail::<S>(cx, Fail::IterCallable)); }
    Ok(cx.new_iter(IterFrame::Call { f, sentinel }, <S::Lang as Derived>::ITERS.calls)?.into())
}

fn next_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (it, default) = (a.pos[0], a.get(1));
    // a generator holding no item with no default steps as the engine's next steps it
    if default.is_none() && cx.resumable(it) { return Ok(Out::Next(it)); }
    match cx.next_plain(it) {
        Some(Ok(Some(v))) => return Ok(v.into()),
        Some(Ok(None)) => return match default { Some(d) => Ok(d.into()), None => Err(cx.stop_iteration(Val::none())) },
        Some(Err(e)) => return Err(e),
        None => {}
    }
    if cx.kind(it) != KindId::Iter && !cx.is_generator(it) { return Err(fail::<S>(cx, Fail::NotIterator(it))); }
    let mut acc = Acc::new();
    acc.v[0] = default.unwrap_or(Val::undef());
    Ok(Step::Drain { it, fold: Fold::first(first, acc) }.into())
}

/* the first item stops the walk and its end gives the default kept in `v[0]` */
fn first(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => Ok(Flow::Stop(v)),
        Item::End(_) if !acc.v[0].is_undef() => Ok(Flow::Stop(acc.v[0])),
        Item::End(ret) => Err(cx.stop_iteration(ret)),
        Item::Called(_) => Ok(Flow::More),
    }
}

fn numbered_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let n = match a.get(1) {
        None => Val::int(0),
        // a bool start counts from its int where bools are ints
        Some(b) if b.is_bool() && cx.policy().bool_is_int => Val::int(b.as_bool() as i64),
        Some(n) if n.is_int() || cx.heap().is_wide(n) => n,
        Some(_) => return Err(fail::<S>(cx, Fail::StartType)),
    };
    let it = cx.iter_of(a.pos[0])?;
    Ok(cx.new_iter(IterFrame::Enumerate { it, n }, <S::Lang as Derived>::ITERS.numbered)?.into())
}

/* `numbered(xs)` of what `plain_iter` takes made with no call */
fn numbered_of<L: Lang>(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let it = plain_iter::<L>(heap, v)?;
    heap.alloc(HeapObj::Iter(Rc::new(RefCell::new(IterFrame::Enumerate { it, n: Val::int(0) })), L::ITERS.numbered)).ok()
}

fn zipped_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let strict = a.kw(0).is_some_and(|v| cx.truthy(v));
    let mut its = Vec::with_capacity(a.len());
    for &v in a.pos { its.push(cx.iter_of(v)?); }
    Ok(cx.new_iter(IterFrame::Zip { its: its.into(), strict }, <S::Lang as Derived>::ITERS.zipped)?.into())
}

/* `zipped(xs)` of what `plain_iter` takes made with no call */
fn zipped_of<L: Lang>(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let it = plain_iter::<L>(heap, v)?;
    heap.alloc(HeapObj::Iter(Rc::new(RefCell::new(IterFrame::Zip { its: Rc::from([it]), strict: false })), L::ITERS.zipped)).ok()
}

fn mapped_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let own = makes_iterator(cx, a.pos[1]);
    let mut its = Vec::with_capacity(a.len() - 1);
    for &v in &a.pos[1..] { its.push(cx.iter_of(v)?); }
    Ok(cx.new_iter(IterFrame::Map { f: a.pos[0], its: its.into(), own }, <S::Lang as Derived>::ITERS.mapped)?.into())
}

fn filtered_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let own = makes_iterator(cx, a.pos[1]);
    let it = cx.iter_of(a.pos[1])?;
    // a nil test keeps the items true in themselves as no test does
    let f = if a.pos[0].is_none() { Val::undef() } else { a.pos[0] };
    Ok(cx.new_iter(IterFrame::Filter { f, it, own }, <S::Lang as Derived>::ITERS.filtered)?.into())
}

/* whether the iterator made for `v` is a fresh one only its caller holds */
fn makes_iterator(cx: &Cx<'_, '_>, v: Val) -> bool {
    matches!(cx.heap().try_get(v), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Range(..) | HeapObj::Str(_) | HeapObj::Dict(_) | HeapObj::Set(_)))
}

fn backward_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let o = a.pos[0];
    let names = <S::Lang as Derived>::ITERS;
    let (frame, name) = match cx.heap().try_get(o) {
        Some(HeapObj::List(rc)) => (IterFrame::ListRev { rc: rc.clone(), idx: rc.borrow().len() }, names.list_back),
        Some(&HeapObj::Range(s, e, st)) => (match back_range(s, e, st) { Some(frame) => frame, None => far_back::<S>(cx, s, e, st)? }, names.range),
        Some(HeapObj::BigRange(..)) => (wide_back(cx, o)?, names.range),
        Some(HeapObj::Str(_) | HeapObj::Tuple(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_)) => (back_items::<S>(cx, o, false)?, names.backward),
        Some(HeapObj::Dict(_)) => (back_items::<S>(cx, o, true)?, names.keys_back),
        _ => return Err(fail::<S>(cx, Fail::NotReversible(o))),
    };
    Ok(cx.new_iter(frame, name)?.into())
}

/* `backward` of a list read live or a sequence read up front with no call */
fn backward_of<L: Lang>(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let names = L::ITERS;
    let (frame, name) = match heap.try_get(v)? {
        HeapObj::List(rc) => (IterFrame::ListRev { rc: rc.clone(), idx: rc.borrow().len() }, names.list_back),
        &HeapObj::Range(s, e, st) => (back_range(s, e, st)?, names.range),
        HeapObj::Str(_) | HeapObj::Tuple(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_) => (back(plain_items(heap, v)?, v, false), names.backward),
        HeapObj::Dict(_) => (back(plain_items(heap, v)?, v, true), names.keys_back),
        _ => return None,
    };
    heap.alloc(HeapObj::Iter(Rc::new(RefCell::new(frame)), name)).ok()
}

/* a word range walked from its last item or none once a bound leaves it */
fn back_range(s: i64, e: i64, st: i64) -> Option<IterFrame> {
    let n = span(s, e, st)?;
    let cur = i64::try_from(s as i128 + (n - 1) * st as i128).ok()?;
    let end = i64::try_from(s as i128 - st as i128).ok()?;
    Some(IterFrame::Range { cur, end, step: st.checked_neg()? })
}

/* a range walked back past the word read up front when short else wide */
fn far_back<S: Site>(cx: &mut Cx<'_, '_>, s: i64, e: i64, st: i64) -> Result<IterFrame, VmErr> {
    let Some(n) = span(s, e, st) else { return Err(fail::<S>(cx, Fail::ZeroStep)) };
    let (s, st) = (s as i128, st as i128);
    if n <= FEW_BACK {
        let mut items = Vec::with_capacity(n as usize);
        for k in (0..n).rev() { items.push(cx.new_int(s + k * st)?); }
        cx.spend(items.len());
        return Ok(seq(items));
    }
    Ok(IterFrame::BigRange { cur: cx.new_int(s + (n - 1) * st)?, end: cx.new_int(s - st)?, step: cx.new_int(-st)? })
}

/* a wide range walked back from its last item through the wide int kind */
fn wide_back(cx: &mut Cx<'_, '_>, o: Val) -> Result<IterFrame, VmErr> {
    let (cur, end, step) = cx.range_reversed(o).unwrap_or_else(|| Err(VmErr::Overflow))?;
    Ok(IterFrame::BigRange { cur, end, step })
}

/* the items of a sequence read up front from its end */
fn back_items<S: Site>(cx: &mut Cx<'_, '_>, o: Val, map: bool) -> Result<IterFrame, VmErr> {
    let Some(items) = cx.items(o)? else { return Err(fail::<S>(cx, Fail::NotReversible(o))) };
    Ok(back(items, o, map))
}

/* `items` walked from the end where a map's walk fails once it resizes */
fn back(mut items: Vec<Val>, of: Val, map: bool) -> IterFrame {
    items.reverse();
    let len = items.len();
    if map { IterFrame::Watched { items: items.into(), idx: 0, of, len } } else { seq(items) }
}

fn range_index_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (r, x) = (a.recv, a.pos[0]);
    let found = match cx.heap().try_get(r) {
        Some(&HeapObj::Range(s, e, st)) => match word_pos(cx, s, e, st, x) { Some(k) => Some(cx.new_int(k)?), None => None },
        Some(HeapObj::BigRange(..)) => cx.range_index(r, x).transpose()?.flatten(),
        _ => return Err(receiver()),
    };
    match found {
        Some(i) => Ok(i.into()),
        None if int_like(cx, x) => Err(fail::<S>(cx, Fail::NotFound(r))),
        // any other value is searched for by equality which only a whole float wins
        None => Err(fail::<S>(cx, Fail::NotInSequence)),
    }
}

/* where `x` sits in a word range with a whole float as its int */
fn word_pos(cx: &Cx<'_, '_>, s: i64, e: i64, st: i64, x: Val) -> Option<i128> {
    let k = match cx.int(x) {
        Some(k) => k,
        None if x.is_float() && x.as_float().abs() < 1e38 && x.as_float() as i128 as f64 == x.as_float() => x.as_float() as i128,
        None => return None,
    };
    let (s, e, st) = (s as i128, e as i128, st as i128);
    let inside = if st > 0 { s <= k && k < e } else { e < k && k <= s };
    (inside && (k - s).checked_rem(st) == Some(0)).then(|| (k - s) / st)
}

fn range_count_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Val::int(cx.contains(a.recv, a.pos[0])? as i64).into()) }

fn send_run(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Resume { g: a.recv, sent: Sent::Value(a.pos[0]) }.into()) }

fn throw_run(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Resume { g: a.recv, sent: Sent::Throw(a.pos[0], a.get(1).unwrap_or(Val::undef())) }.into()) }

fn close_run(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Resume { g: a.recv, sent: Sent::Close }.into()) }
