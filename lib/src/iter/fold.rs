use edge::lang::{Acc, Args, Cx, ErrKind, Falsy, Flow, Fold, Item, KindId, Language, Ret, STEPS, Sig, Step};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

/* whether every item is true */
pub const fn every<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), every_run).fast(every_of).flags(STEPS) }

/* whether any item is true */
pub const fn some<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), some_run).fast(some_of).flags(STEPS) }

/* the least of an iterable or the positionals by a key with a default */
pub const fn least<S: Site>() -> Kernel { Kernel::new(Sig::var(1), least_run::<S>).fast(least_fast).flags(STEPS).reads(2) }

/* the greatest of an iterable or the positionals by a key with a default */
pub const fn most<S: Site>() -> Kernel { Kernel::new(Sig::var(1), most_run::<S>).fast(most_fast).flags(STEPS).reads(2) }

/* the total of an iterable from a start where `R` may refuse text */
pub const fn sum<S: Site, R: Refuse>() -> Kernel { Kernel::new(Sig::range(1, 2), sum_run::<S, R>).fast(sum_fast).flags(STEPS) }

/* whether a total refuses a text or bytes start which joining builds instead */
pub trait Refuse: 'static { const TEXT: bool; }

/* a total that adds whatever its start is */
pub struct Adds;

impl Refuse for Adds { const TEXT: bool = false; }

/* a total that refuses a text or bytes start */
pub struct Joins;

impl Refuse for Joins { const TEXT: bool = true; }

fn every_run(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::truth(false) }.into()) }

fn some_run(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::truth(true) }.into()) }

fn every_of(heap: &mut HeapPool, v: Val) -> Option<Val> { truth_of(heap, v, false) }

fn some_of(heap: &mut HeapPool, v: Val) -> Option<Val> { truth_of(heap, v, true) }

/* `some` or `every` of a list tuple or word range settled with its reads spent */
fn truth_of(heap: &mut HeapPool, v: Val, any: bool) -> Option<Val> {
    let (read, found) = match heap.try_get(v)? {
        HeapObj::List(rc) => scan(&rc.borrow(), heap, any),
        HeapObj::Tuple(t) => scan(t, heap, any),
        // bytes read in place with no value made for each
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => match b.iter().position(|&x| (x != 0 || zero_true(heap)) == any) { Some(i) => (i as i128 + 1, true), None => (b.len() as i128, false) },
        &HeapObj::Range(s, e, st) => range_truth(s, e, st, any, || zero_true(heap))?,
        _ => return None,
    };
    match usize::try_from(read) {
        Ok(n) => heap.spend(n),
        // reads past usize end at a budget below it which the widest spend still meets
        Err(_) if heap.afford(usize::MAX).is_err() => heap.spend(usize::MAX),
        Err(_) => return None,
    }
    Some(Val::bool(found == any))
}

/* whether a zero reads as true asked only once a zero item comes */
#[inline(always)]
fn zero_true(heap: &HeapPool) -> bool { !heap.falsy().has(Falsy::ZERO) }

/* the items a truth walk reads and whether one of truth `any` stopped it */
fn scan(items: &[Val], heap: &HeapPool, any: bool) -> (i128, bool) {
    let truth = |v: Val| if v.is_int() { v.as_int() != 0 || zero_true(heap) } else { heap.truthy(v) };
    match items.iter().position(|&v| truth(v) == any) {
        Some(i) => (i as i128 + 1, true),
        None => (items.len() as i128, false),
    }
}

/* `scan` of a word range where zero alone may read as false */
fn range_truth(s: i64, e: i64, st: i64, any: bool, zero_true: impl Fn() -> bool) -> Option<(i128, bool)> {
    let n = super::span(s, e, st)?;
    let stop = match any {
        true if n > 0 && (s != 0 || zero_true()) => Some(0),
        true => (n > 1).then_some(1),
        false => zero_at(s, st).filter(|&k| k < n && !zero_true()),
    };
    Some((stop.map_or(n, |k| k + 1), stop.is_some()))
}

/* the index zero lands at in the run from `s` by `st` if any */
fn zero_at(s: i64, st: i64) -> Option<i128> {
    let (s, st) = (s as i128, st as i128);
    ((-s).checked_rem(st) == Some(0) && -s / st >= 0).then(|| -s / st)
}

fn least_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { extreme::<S>(cx, a, true) }

fn most_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { extreme::<S>(cx, a, false) }

/* the least or greatest of an iterable or the positionals with `acc.n` flagging the least */
fn extreme<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>, least: bool) -> Ret {
    let key = a.kw(0).filter(|k| !k.is_none());
    let default = a.kw(1);
    if a.pos.len() > 1 {
        if default.is_some() { return Err(fail::<S>(cx, Fail::KeywordDefault)); }
        if key.is_none() {
            let mut best = a.pos[0];
            for &v in &a.pos[1..] { if beats::<S>(cx, v, best, least)? { best = v; } }
            return Ok(best.into());
        }
    }
    let Some(k) = key else { return keyless::<S>(cx, a.pos[0], least, default) };
    let mut acc = Acc::new();
    acc.items.extend(default);
    acc.n = least as i64;
    let it = if let &[it] = a.pos { it } else { cx.new_tuple(a.pos.to_vec())? };
    Ok(Step::Drain { it, fold: Fold::best(least, k, keyed::<S>, acc) }.into())
}

/* the least or greatest item of `it` where a container's items compare with no walk */
fn keyless<S: Site>(cx: &mut Cx<'_, '_>, it: Val, least: bool, default: Option<Val>) -> Ret {
    // a number fails as no iterable before any walk starts
    let container = matches!(cx.kind(it), KindId::List | KindId::Tuple | KindId::Str | KindId::Dict | KindId::Set | KindId::Bytes | KindId::ByteArray | KindId::None | KindId::Bool | KindId::Int | KindId::Float);
    if container && let Some(items) = cx.items(it)? {
        let Some((&first, rest)) = items.split_first() else {
            return match default { Some(d) => Ok(d.into()), None => Err(fail::<S>(cx, Fail::ExtremeEmpty)) };
        };
        let mut best = first;
        for &v in rest { if beats::<S>(cx, v, best, least)? { best = v; } }
        return Ok(best.into());
    }
    let mut acc = Acc::new();
    acc.items.extend(default);
    acc.n = least as i64;
    Ok(Step::Drain { it, fold: Fold::extreme(least, extreme_fold::<S>, acc) }.into())
}

/* whether `v` beats `best` where the greatest words a failure by its own operator */
fn beats<S: Site>(cx: &mut Cx<'_, '_>, v: Val, best: Val, least: bool) -> Result<bool, VmErr> {
    if v.is_int() && best.is_int() { return Ok(if least { v.as_int() < best.as_int() } else { v.as_int() > best.as_int() }); }
    if least { return cx.lt(v, best); }
    cx.lt(best, v).map_err(|e| unordered::<S>(cx, e, v, best))
}

/* the failure of a greatest whose items do not order which names its own operator */
#[cold]
#[inline(never)]
fn unordered<S: Site>(cx: &mut Cx<'_, '_>, e: VmErr, v: Val, best: Val) -> VmErr {
    if e.is_kind(ErrKind::Type) { fail::<S>(cx, Fail::NotOrdered(v, best)) } else { e }
}

/* the items of a least or greatest the engine could not compare as plain numbers */
fn extreme_fold<S: Site>(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => {
            if acc.v[0].is_undef() || beats::<S>(cx, v, acc.v[0], acc.n != 0)? { acc.v[0] = v; }
            Ok(Flow::More)
        }
        Item::End(_) => best_or_default::<S>(acc, cx),
        Item::Called(_) => Ok(Flow::More),
    }
}

/* a keyed least or greatest holding the item in `v[2]` while key `v[3]` runs */
fn keyed<S: Site>(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => { acc.v[2] = v; Ok(Flow::Call(acc.v[3], v)) }
        Item::Called(k) => {
            if acc.v[1].is_undef() || beats::<S>(cx, k, acc.v[1], acc.n != 0)? { (acc.v[0], acc.v[1]) = (acc.v[2], k); }
            acc.v[2] = Val::undef();
            Ok(Flow::More)
        }
        Item::End(_) => best_or_default::<S>(acc, cx),
    }
}

/* the best item or else the default once the items ran out */
fn best_or_default<S: Site>(acc: &Acc, cx: &mut Cx<'_, '_>) -> Result<Flow, VmErr> {
    if !acc.v[0].is_undef() { return Ok(Flow::Stop(acc.v[0])); }
    match acc.items.first() {
        Some(&d) => Ok(Flow::Stop(d)),
        None => Err(fail::<S>(cx, Fail::ExtremeEmpty)),
    }
}

fn least_fast(heap: &mut HeapPool, v: Val) -> Option<Val> { extreme_fast(heap, v, true) }

fn most_fast(heap: &mut HeapPool, v: Val) -> Option<Val> { extreme_fast(heap, v, false) }

/* the least or greatest plain number of a list tuple or range with no call */
fn extreme_fast(heap: &mut HeapPool, v: Val, least: bool) -> Option<Val> {
    let (best, n) = match heap.try_get(v)? {
        HeapObj::List(rc) => { let items = rc.borrow(); (plain_best(&items, least)?, items.len()) }
        HeapObj::Tuple(items) => (plain_best(items, least)?, items.len()),
        // bytes read in place with no value made for each
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => (Val::int(i64::from(if least { *b.iter().min()? } else { *b.iter().max()? })), b.len()),
        &HeapObj::Range(s, e, st) => range_best(s, e, st, least)?,
        _ => return None,
    };
    // each item counts as one step as the engine's walk charges it
    heap.spend(n);
    Some(best)
}

/* the best of items all inline ints or all floats none of them nan */
fn plain_best(items: &[Val], least: bool) -> Option<Val> {
    let (&first, rest) = items.split_first()?;
    let mut best = first;
    if first.is_int() {
        for &v in rest {
            if !v.is_int() { return None; }
            if (v.as_int() < best.as_int()) == least && v.as_int() != best.as_int() { best = v; }
        }
    } else if first.is_float() && !first.as_float().is_nan() {
        for &v in rest {
            if !v.is_float() || v.as_float().is_nan() { return None; }
            if if least { v.as_float() < best.as_float() } else { v.as_float() > best.as_float() } { best = v; }
        }
    } else {
        return None;
    }
    Some(best)
}

/* how many ints a range holds or none past what a count holds */
fn range_count(s: i64, e: i64, st: i64) -> Option<usize> {
    let (s, e, st) = (s as i128, e as i128, st as i128);
    let n = if st > 0 { if s >= e { 0 } else { (e - s - 1) / st + 1 } } else if s <= e { 0 } else { (s - e - 1) / -st + 1 };
    usize::try_from(n).ok()
}

/* the first or last int of a nonempty range whichever is the least or greatest */
fn range_best(s: i64, e: i64, st: i64, least: bool) -> Option<(Val, usize)> {
    let n = range_count(s, e, st).filter(|&n| n > 0)?;
    let last = s as i128 + (n as i128 - 1) * st as i128;
    let best = if (st > 0) == least { s as i128 } else { last };
    Some((Val::int_checked(i64::try_from(best).ok()?)?, n))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode { Words, Floats, Any }

/* the stage a total reached kept in its fold's second value */
fn mode(acc: &Acc) -> Mode {
    match acc.v[1] {
        v if v.is_undef() => Mode::Words,
        v if v.is_int() && v.as_int() == 1 => Mode::Floats,
        _ => Mode::Any,
    }
}

fn set_mode(acc: &mut Acc, m: Mode) { acc.v[1] = match m { Mode::Words => Val::undef(), Mode::Floats => Val::int(1), Mode::Any => Val::int(2) }; }

/* `v` as a machine word when it is an int that fits one */
fn word(cx: &Cx<'_, '_>, v: Val) -> Option<i64> { cx.int(v).and_then(|n| i64::try_from(n).ok()) }

fn sum_run<S: Site, R: Refuse>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let it = a.pos[0];
    let mut acc = Acc::new();
    let Some(start) = a.get(1) else { return Ok(Step::Drain { it, fold: Fold::words(sum_fold, acc).taking(sum_take::<S::Lang>) }.into()) };
    // text and bytes join which the failure points to
    if R::TEXT && matches!(cx.heap().try_get(start), Some(HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_))) { return Err(fail::<S>(cx, Fail::SumText(start))); }
    // a bool start adds through `+` so an empty total gives the bool back
    if let Some(w) = word(cx, start).filter(|_| !start.is_bool()) {
        acc.n = w;
        return Ok(Step::Drain { it, fold: Fold::words(sum_fold, acc).taking(sum_take::<S::Lang>) }.into());
    }
    if start.is_float() { acc.f = (start.as_float(), 0.0); set_mode(&mut acc, Mode::Floats); } else { acc.v[0] = start; set_mode(&mut acc, Mode::Any); }
    Ok(Step::Drain { it, fold: Fold::with(sum_fold, acc).taking(sum_take::<S::Lang>) }.into())
}

/* a word or float item a total adds in place as `add` does or false */
fn sum_take<L: Language>(acc: &mut Acc, v: Val) -> bool {
    let word = if v.is_int() { Some(v.as_int()) } else if v.is_bool() && L::POLICY.bool_is_int { Some(v.as_bool() as i64) } else { None };
    match (mode(acc), word) {
        (Mode::Words, Some(b)) => match acc.n.checked_add(b) { Some(n) => acc.n = n, None => return false },
        // the first float takes the word total as `+` would
        (Mode::Words, None) if v.is_float() => { acc.f = (acc.n as f64 + v.as_float(), 0.0); set_mode(acc, Mode::Floats); }
        (Mode::Floats, Some(b)) => acc.f.0 += b as f64,
        (Mode::Floats, None) if v.is_float() => acc.f = neumaier(acc.f, v.as_float()),
        _ => return false,
    }
    true
}

/* the items of a total the engine left unadded and its total at the end */
fn sum_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => { add(acc, cx, v)?; Ok(Flow::More) }
        Item::End(_) => Ok(Flow::Stop(match mode(acc) {
            Mode::Words => cx.new_int(acc.n as i128)?,
            Mode::Floats => Val::float(settle(acc.f)),
            Mode::Any => acc.v[0],
        })),
        Item::Called(_) => Ok(Flow::More),
    }
}

/* adds `v` with words exact then floats compensated then anything through `+` never going back */
fn add(acc: &mut Acc, cx: &mut Cx<'_, '_>, v: Val) -> Result<(), VmErr> {
    match mode(acc) {
        Mode::Words => match word(cx, v).and_then(|b| acc.n.checked_add(b)) {
            Some(n) => acc.n = n,
            None => {
                let total = cx.new_int(acc.n as i128)?;
                let total = cx.add(total, v)?;
                if total.is_float() { acc.f = (total.as_float(), 0.0); set_mode(acc, Mode::Floats); } else { acc.v[0] = total; set_mode(acc, Mode::Any); }
            }
        },
        Mode::Floats if v.is_float() => acc.f = neumaier(acc.f, v.as_float()),
        Mode::Floats => match word(cx, v) {
            Some(b) => acc.f.0 += b as f64,
            None => { acc.v[0] = cx.add(Val::float(settle(acc.f)), v)?; set_mode(acc, Mode::Any); }
        },
        Mode::Any => acc.v[0] = cx.add(acc.v[0], v)?,
    }
    Ok(())
}

/* one neumaier step of a compensated float total */
fn neumaier((sum, comp): (f64, f64), x: f64) -> (f64, f64) {
    let t = sum + x;
    let c = if sum.abs() >= x.abs() { (sum - t) + x } else { (x - t) + sum };
    (t, comp + c)
}

/* a compensated total with its compensation added only when nonzero and finite */
fn settle((sum, comp): (f64, f64)) -> f64 { if comp != 0.0 && comp.is_finite() { sum + comp } else { sum } }

/* what a total of plain numbers comes to before it is boxed */
enum Total { Int(i128), Float(f64) }

/* the total of a list tuple or range of plain numbers with no call */
fn sum_fast(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let (total, n) = match heap.try_get(v)? {
        HeapObj::List(rc) => { let items = rc.borrow(); (plain_sum(&items, heap)?, items.len()) }
        HeapObj::Tuple(items) => (plain_sum(items, heap)?, items.len()),
        // bytes add in place with no value made for each
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => (Total::Int(b.iter().map(|&x| i128::from(x)).sum()), b.len()),
        &HeapObj::Range(s, e, st) => range_sum(s, e, st)?,
        _ => return None,
    };
    let r = match total { Total::Int(t) => heap.int(t).ok()?, Total::Float(f) => Val::float(f) };
    // each item counts as one step as the engine's walk charges it
    heap.spend(n);
    Some(r)
}

/* words added exactly then compensated floats with any other item left to the full path */
fn plain_sum(items: &[Val], heap: &HeapPool) -> Option<Total> {
    let mut w = 0i64;
    let mut i = 0;
    while i < items.len() {
        let v = items[i];
        if v.is_float() { break; }
        w = w.checked_add(if v.is_int() { v.as_int() } else { crate::num::bool_int(heap, v)? })?;
        i += 1;
    }
    let Some(&first) = items.get(i) else { return Some(Total::Int(w as i128)) };
    // the first float takes the word total as `+` would
    let mut f = (w as f64 + first.as_float(), 0.0);
    for &v in &items[i + 1..] {
        if v.is_float() { f = neumaier(f, v.as_float()); }
        else if v.is_int() { f.0 += v.as_int() as f64; }
        else { f.0 += crate::num::bool_int(heap, v)? as f64; }
    }
    Some(Total::Float(settle(f)))
}

/* the total of a range's ints in closed form and how many it holds */
fn range_sum(s: i64, e: i64, st: i64) -> Option<(Total, usize)> {
    let n = range_count(s, e, st)?;
    let (s, st, k) = (s as i128, st as i128, n as i128);
    let t = k.checked_mul(s)?.checked_add(st.checked_mul(k.checked_mul(k - 1)? / 2)?)?;
    Some((Total::Int(t), n))
}
