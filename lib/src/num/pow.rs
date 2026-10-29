use edge::lang::{Args, Cx, IntMod, Ret, Sig};
use edge::value::{Val, VmErr};

use super::{big, bool_int, numeric};
use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

/* a power and with a third argument the power modulo it */
pub const fn pow<S: Site>() -> Kernel { Kernel::new(Sig::range(2, 3), pow_run::<S>) }

fn pow_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (b, e) = (a.pos[0], a.pos[1]);
    // a nil modulus asks the plain power
    let Some(m) = a.get(2).filter(|m| !m.is_none()) else {
        if !numeric(cx.heap(), b) || !numeric(cx.heap(), e) { return Err(fail::<S>(cx, Fail::PowArgs)); }
        return Ok(cx.pow(b, e)?.into());
    };
    let int_like = |v: Val| v.is_int() || bool_int(cx.heap(), v).is_some() || cx.heap().is_wide(v);
    if cx.int(m) == Some(0) && int_like(b) && int_like(e) { return Err(fail::<S>(cx, Fail::PowZero)); }
    // a modulus below 2^63 keeps every product in i128 so small operands skip wide ints
    if let (Some(x), Some(y), Some(md)) = (cx.int(b), cx.int(e), cx.int(m)) && md.unsigned_abs() < 1 << 63 {
        let truncated = matches!(cx.policy().int_mod, IntMod::Truncated);
        return match pow_small(cx, x, y, md, truncated)? { Some(v) => Ok(v.into()), None => Err(fail::<S>(cx, Fail::PowInverse)) };
    }
    match big::pow_mod::<S::Lang>(cx, b, e, m) {
        Some(Ok(Some(r))) => Ok(r.into()),
        Some(Ok(None)) => Err(fail::<S>(cx, Fail::PowInverse)),
        Some(Err(e)) => Err(e),
        None => Err(fail::<S>(cx, Fail::PowThird)),
    }
}

/* the modular power in i128 below 2^63 signed by the remainder rule */
fn pow_small(cx: &mut Cx<'_, '_>, x: i128, y: i128, m: i128, truncated: bool) -> Result<Option<Val>, VmErr> {
    let am = m.abs();
    let mut b = x.rem_euclid(am);
    if y < 0 {
        let (mut r, mut r1, mut t, mut t1) = (am, b, 0i128, 1i128);
        while r1 != 0 { let q = r / r1; (r, r1, t, t1) = (r1, r - q * r1, t1, t - q * t1); }
        if r != 1 { return Ok(None); }
        b = t.rem_euclid(am);
    }
    // the exponent's magnitude so the least i128 needs no negation
    let (mut acc, mut e) = (1i128, y.unsigned_abs());
    while e > 0 { if e & 1 == 1 { acc = acc * b % am; } b = b * b % am; e >>= 1; }
    let r = acc % am;
    // a floored remainder takes the modulus's sign and a truncated one the power's
    let negative = if truncated { x < 0 && y & 1 == 1 } else { m < 0 };
    cx.new_int(if negative && r != 0 { r - am } else { r }).map(Some)
}
