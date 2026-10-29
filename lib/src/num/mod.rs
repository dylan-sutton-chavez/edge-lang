use core::cmp::Ordering;

use edge::lang::{Args, Cx, KindId, Ret, Sig};
use edge::value::{HeapPool, UnOp, Val, VmErr};

use crate::fail::{Error, Fail, fail};
use crate::{Kernel, Site};

pub mod big;
pub mod div;
pub mod float;
pub mod hash;
pub mod int;
pub mod pow;
pub mod round;

/* the distance of a number from zero */
pub const fn abs<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), abs_run::<S>).fast(abs_fast) }

/* the int a bool counts as where the policy counts bools as ints */
#[inline(always)]
pub fn bool_int(heap: &HeapPool, v: Val) -> Option<i64> { if v.is_bool() && heap.policy().bool_is_int { Some(v.as_bool() as i64) } else { None } }

/* an int as a word or none for other kinds and an overflow past one */
#[inline]
pub fn word(cx: &Cx<'_, '_>, v: Val) -> Option<Result<i64, VmErr>> {
    if v.is_int() { return Some(Ok(v.as_int())); }
    if let Some(b) = bool_int(cx.heap(), v) { return Some(Ok(b)); }
    (cx.kind(v) == KindId::Int).then(|| cx.int(v).and_then(|n| i64::try_from(n).ok()).ok_or_else(|| VmErr::Overflow))
}

/* an int argument that must fit a word with the failure for another type */
pub fn ssize(cx: &Cx<'_, '_>, v: Val) -> Result<i64, Error> {
    match word(cx, v) {
        Some(r) => Ok(r?),
        None => Err(Error::Fail(Fail::NotIndex(v))),
    }
}

/* the window two bounds give over `len` items and none past the end */
pub fn bounds(cx: &Cx<'_, '_>, start: Option<Val>, stop: Option<Val>, len: usize) -> Result<Option<(usize, usize)>, Error> { window(cx, start, stop, len, true) }

/* `bounds` where a nil bound is refused as a sequence's search refuses it */
pub fn seq_bounds(cx: &Cx<'_, '_>, start: Option<Val>, stop: Option<Val>, len: usize) -> Result<Option<(usize, usize)>, Error> { window(cx, start, stop, len, false) }

/* the window of two bounds where `nil` says whether a nil bound stands for none */
fn window(cx: &Cx<'_, '_>, start: Option<Val>, stop: Option<Val>, len: usize, nil: bool) -> Result<Option<(usize, usize)>, Error> {
    if !nil && start.into_iter().chain(stop).any(|v| v.is_none()) { return Err(Error::Fail(Fail::SliceIndex)); }
    let (start, stop, n) = (bound(cx, start)?, bound(cx, stop)?, len as i64);
    let at = |i: i64| (if i < 0 { n.saturating_add(i) } else { i }).clamp(0, n) as usize;
    if start.is_some_and(|s| s > n) { return Ok(None); }
    let (s, e) = (start.map_or(0, at), stop.map_or(len, at));
    Ok((s <= e).then_some((s, e)))
}

/* a bound as a word where nil is none and wide ints clip by sign */
fn bound(cx: &Cx<'_, '_>, v: Option<Val>) -> Result<Option<i64>, Error> {
    let Some(v) = v.filter(|v| !v.is_none()) else { return Ok(None) };
    match word(cx, v) {
        Some(Ok(n)) => Ok(Some(n)),
        Some(Err(_)) => Ok(Some(if negative(cx, v) { i64::MIN } else { i64::MAX })),
        None => Err(Error::Fail(Fail::SliceIndex)),
    }
}

/* whether wide int `v` is below zero */
pub(crate) fn negative(cx: &Cx<'_, '_>, v: Val) -> bool { cx.heap().foreign_cmp(v, Val::int(0)) == Some(Ordering::Less) }

/* whether `v` may take part in arithmetic with foreign kinds left to answer for themselves */
pub(crate) fn numeric(heap: &HeapPool, v: Val) -> bool { v.is_int() || v.is_float() || v.is_foreign() || bool_int(heap, v).is_some() }

/* `x` times two to the `n` rounded once as scaling a float rounds */
pub(crate) fn scalbn(x: f64, mut n: i32) -> f64 {
    let (up, down) = (f64::from_bits(0x7FE0_0000_0000_0000), f64::from_bits(0x0010_0000_0000_0000) * f64::from_bits(0x4340_0000_0000_0000));
    let mut y = x;
    if n > 1023 {
        y *= up;
        n -= 1023;
        if n > 1023 { y *= up; n = (n - 1023).min(1023); }
    } else if n < -1022 {
        // two steps keep the last scaling the only rounding in the subnormal range
        y *= down;
        n += 1022 - 53;
        if n < -1022 { y *= down; n = (n + 1022 - 53).max(-1022); }
    }
    y * f64::from_bits(((0x3ff + n) as u64) << 52)
}

fn abs_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let o = a.pos[0];
    if let Some(v) = abs_fast(cx.heap_mut(), o) { return Ok(v.into()); }
    // the least inline int has an abs past the inline range
    if o.is_int() { return Ok(cx.new_int((o.as_int() as i128).abs())?.into()); }
    if let Some(r) = cx.heap_mut().foreign_unop(UnOp::Abs, o)? { return Ok(r.into()); }
    Err(fail::<S>(cx, Fail::BadOperand(o)))
}

/* the distance of a plain number with no call and none past the inline range */
fn abs_fast(heap: &mut HeapPool, v: Val) -> Option<Val> {
    if v.is_float() { return Some(Val::float(v.as_float().abs())); }
    if v.is_int() { return Val::int_checked(v.as_int().abs()); }
    bool_int(heap, v).map(Val::int)
}
