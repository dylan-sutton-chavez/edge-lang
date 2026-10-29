use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use edge::lang::{Args, Cx, Ret, Sig};
use edge::value::{HeapPool, Val, VmErr, fabs, fround, ftrunc};

use super::{big, bool_int, negative};
use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

/* a number rounded to an int or to decimals where `R` breaks ties */
pub const fn round<S: Site, R: Round>() -> Kernel { Kernel::new(Sig::range(1, 2), round_run::<S, R>).fast(round_fast::<R>) }

/* how a rounding breaks a tie */
pub trait Round: 'static {
    // a tie goes away from zero rather than to the even neighbor
    const AWAY: bool;
}

/* ties go to the even neighbor */
pub struct HalfEven;

/* ties go away from zero */
pub struct HalfAway;

impl Round for HalfEven { const AWAY: bool = false; }

impl Round for HalfAway { const AWAY: bool = true; }

/* `x` rounded to a whole float by the rule of `R` */
#[inline]
fn whole<R: Round>(x: f64) -> f64 {
    if !R::AWAY { return fround(x); }
    let t = ftrunc(x);
    if fabs(x - t) >= 0.5 { t + if x < 0.0 { -1.0 } else { 1.0 } } else { t }
}

fn round_run<S: Site, R: Round>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let o = a.pos[0];
    let Some(n) = a.get(1).filter(|n| !n.is_none()) else { return round_whole::<S, R>(cx, o) };
    let int = o.is_int() || bool_int(cx.heap(), o).is_some() || cx.heap().is_wide(o);
    if !int && !o.is_float() { return Err(fail::<S>(cx, Fail::RoundArg)); }
    let Some(nd) = ndigits(cx, n) else { return Err(fail::<S>(cx, Fail::NotIndex(n))) };
    if o.is_float() {
        return match round_float::<R>(o.as_float(), nd) {
            Some(f) => Ok(Val::float(f).into()),
            None => Err(fail::<S>(cx, Fail::RoundRange)),
        };
    }
    let o = bool_int(cx.heap(), o).map_or(o, Val::int);
    if nd >= 0 { return Ok(o.into()); }
    // an inline int dropping at most 18 digits rounds in a word
    if o.is_int() && nd >= -18 {
        let (x, f) = (o.as_int(), 10i64.pow(nd.unsigned_abs() as u32));
        let (q, r) = (x.div_euclid(f), x.rem_euclid(f));
        let tie = if R::AWAY { x >= 0 } else { q % 2 != 0 };
        return Ok(cx.new_int(((q + (2 * r > f || (2 * r == f && tie)) as i64) * f) as i128)?.into());
    }
    Ok(big::round_tens::<S::Lang>(cx, o, nd.unsigned_abs(), R::AWAY).unwrap_or(Ok(o))?.into())
}

/* the count of decimals as a word where a wider int saturates by its sign */
fn ndigits(cx: &Cx<'_, '_>, n: Val) -> Option<i64> {
    if let Some(i) = cx.int(n) { return Some(i.clamp(i64::MIN as i128, i64::MAX as i128) as i64); }
    cx.heap().is_wide(n).then(|| if negative(cx, n) { i64::MIN } else { i64::MAX })
}

/* the nearest int by the rule of `R` */
fn round_whole<S: Site, R: Round>(cx: &mut Cx<'_, '_>, o: Val) -> Ret {
    if let Some(v) = round_fast::<R>(cx.heap_mut(), o) { return Ok(v.into()); }
    if o.is_float() { return Ok(float_to_int::<S>(cx, whole::<R>(o.as_float()))?.into()); }
    if cx.heap().is_wide(o) { return Ok(o.into()); }
    Err(fail::<S>(cx, Fail::RoundArg))
}

/* the nearest int of an int or of a float whose int fits a word */
fn round_fast<R: Round>(heap: &mut HeapPool, v: Val) -> Option<Val> {
    if v.is_int() { return Some(v); }
    if let Some(b) = bool_int(heap, v) { return Some(Val::int(b)); }
    if !v.is_float() { return None; }
    let r = whole::<R>(v.as_float());
    // an integral float below 2^63 converts exactly
    if r.is_nan() || r.abs() >= 9.223_372_036_854_776e18 { return None; }
    heap.int(r as i64 as i128).ok()
}

/* the int an integral float holds where nan and infinity fail */
fn float_to_int<S: Site>(cx: &mut Cx<'_, '_>, f: f64) -> Result<Val, VmErr> {
    if f.is_nan() { return Err(fail::<S>(cx, Fail::FloatNan)); }
    if f.is_infinite() { return Err(fail::<S>(cx, Fail::FloatInf)); }
    big::from_f64::<S::Lang>(cx, f)
}

/* `x` rounded to `nd` decimals on its exact value and none past the float range */
fn round_float<R: Round>(x: f64, nd: i64) -> Option<f64> {
    // past 323 decimals every float rounds to itself
    if !x.is_finite() || nd > 323 { return Some(x); }
    if nd >= 0 {
        // the exact decimal text rounds once where scaling by ten would round twice
        let text = alloc::format!("{:.*}", nd as usize, x);
        let near = text.parse().unwrap_or(x);
        return Some(if R::AWAY { away_tie(x, nd as usize).unwrap_or(near) } else { near });
    }
    let r = round_float_tens::<R>(x, nd.unsigned_abs().min(309) as usize);
    (!r.is_infinite()).then_some(r)
}

/* an exact tie at `nd` decimals rounded away from zero and none for no tie */
fn away_tie(x: f64, nd: usize) -> Option<f64> {
    // a tie has exact decimals ending in that 5 as its fractional bits tell
    let bits = x.to_bits();
    let (biased, frac) = ((bits >> 52) as i64 & 0x7ff, bits & ((1 << 52) - 1));
    let (m, e) = if biased == 0 { (frac, -1074) } else { (frac | 1 << 52, biased - 1075) };
    if m == 0 || -(e + i64::from(m.trailing_zeros())) != nd as i64 + 1 { return None; }
    let text = alloc::format!("{:.*}", nd + 1, fabs(x));
    let mut digits: Vec<u8> = text.into_bytes();
    digits.pop();
    // the magnitude grows by one unit in the last place carrying through nines
    let mut i = digits.len();
    loop {
        if i == 0 { digits.insert(0, b'1'); break; }
        i -= 1;
        match digits[i] {
            b'.' => continue,
            b'9' => digits[i] = b'0',
            d => { digits[i] = d + 1; break; }
        }
    }
    let r: f64 = String::from_utf8(digits).ok()?.parse().ok()?;
    Some(r.copysign(x))
}

/* `x` rounded to a multiple of 10^k from its printed digits by `R` */
fn round_float_tens<R: Round>(x: f64, k: usize) -> f64 {
    let t = ftrunc(x);
    let mut d = alloc::format!("{:0>w$.0}", t.abs(), w = k + 1).into_bytes();
    let cut = d.len() - k;
    let up = match d[cut..].cmp(alloc::format!("5{:0<w$}", "", w = k - 1).as_bytes()) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => R::AWAY || x != t || d[cut - 1] % 2 == 1,
    };
    d.truncate(cut);
    if up {
        let carried = d.iter().rposition(|&c| c != b'9').map_or(0, |i| { d[i] += 1; i + 1 });
        d[carried..].fill(b'0');
        if carried == 0 { d.insert(0, b'1'); }
    }
    let r: f64 = core::str::from_utf8(&d).ok().and_then(|h| alloc::format!("{h}e{k}").parse().ok()).unwrap_or(0.0);
    r.copysign(x)
}
