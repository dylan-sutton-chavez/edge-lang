#[inline]
pub fn fpowi(mut base: f64, exp: i32) -> f64 {
    if exp == 0 { return 1.0; }
    let neg = exp < 0;
    let mut e = (exp as i64).unsigned_abs() as u32;
    let mut r = 1.0;
    while e > 0 { if e & 1 != 0 { r *= base; } base *= base; e >>= 1; }
    if neg { 1.0 / r } else { r }
}

#[inline]
pub fn fround(x: f64) -> f64 {
    // rounds half to even with no int cast so huge values stay exact
    if !x.is_finite() { return x; }
    let fl = libm::floor(x);
    let diff = x - fl;
    if diff < 0.5 { fl }
    else if diff > 0.5 { fl + 1.0 }
    // exactly half picks the even neighbour tested on the integral `fl` with no cast
    else if libm::floor(fl / 2.0) * 2.0 == fl { fl }
    else { fl + 1.0 }
}

/* whether a finite negative base under a fractional power has only a complex result */
#[inline]
pub fn complex_power(base: f64, exp: f64) -> bool {
    base < 0.0 && base > f64::NEG_INFINITY && exp.is_finite() && libm::floor(exp) != exp
}

/* `base ** exp` as libm `pow` rounds it or none when finite operands overflow */
#[inline]
pub fn fpowf(base: f64, exp: f64) -> Option<f64> {
    // these exponents take one correctly rounded operation which is what `pow` gives them
    let r = if exp == 2.0 { base * base } else if exp == 1.0 { base } else if exp == 0.0 { 1.0 } else if exp == -1.0 { 1.0 / base } else { libm::pow(base, exp) };
    (!r.is_infinite() || !base.is_finite() || !exp.is_finite()).then_some(r)
}

/* floored quotient and remainder of ints or none for a zero divisor or an overflow */
pub fn int_divmod(a: i128, b: i128) -> Option<(i128, i128)> {
    let q = a.checked_div(b)?;
    let r = a - q * b;
    Some(if r != 0 && (r < 0) != (b < 0) { (q - 1, r + b) } else { (q, r) })
}

/* floored quotient and remainder of floats exact where `a - floor(a/b)*b` loses precision */
pub fn float_divmod(a: f64, b: f64) -> (f64, f64) {
    let m = libm::fmod(a, b);
    let mut d = (a - m) / b;
    let m = if m == 0.0 { libm::copysign(0.0, b) } else if (b < 0.0) != (m < 0.0) { d -= 1.0; m + b } else { m };
    let q = if d == 0.0 { libm::copysign(0.0, a / b) } else { let f = libm::floor(d); if d - f > 0.5 { f + 1.0 } else { f } };
    (q, m)
}

#[inline]
pub fn fabs(x: f64) -> f64 {
    f64::from_bits(f64::to_bits(x) & 0x7FFF_FFFF_FFFF_FFFF)
}

#[inline]
pub fn ftrunc(x: f64) -> f64 { libm::trunc(x) }

/* `a / b` of two ints rounded once to the nearest float ties to even */
pub fn div_i128(a: i128, b: i128) -> f64 {
    let (n, d) = (a.unsigned_abs(), b.unsigned_abs());
    if n == 0 { return if (a < 0) != (b < 0) { -0.0 } else { 0.0 }; }
    // one numerator shift leaves 66 quotient bits and the loop takes what does not fit
    let bits = |x: u128| 128 - x.leading_zeros() as i64;
    let k = (66 - bits(n) + bits(d)).clamp(0, n.leading_zeros() as i64);
    let n = n << k;
    let (mut q, mut r, mut exp) = (n / d, n % d, -k);
    while bits(q) < 66 { r <<= 1; q = q << 1 | (r >= d) as u128; if r >= d { r -= d; } exp -= 1; }
    let shift = (bits(q) as u32).saturating_sub(64);
    let sticky = r != 0 || q & ((1u128 << shift) - 1) != 0;
    let f = round_to_f64((q >> shift) as u64, exp + shift as i64, sticky).unwrap_or(f64::INFINITY);
    if (a < 0) != (b < 0) { -f } else { f }
}

/* `top` times `2^exp` with `sticky` below rounded half even to 53 bits or none */
pub fn round_to_f64(top: u64, exp: i64, sticky: bool) -> Option<f64> {
    if top == 0 { return Some(0.0); }
    let len = 64 - top.leading_zeros() as i64;
    // the leading bit's weight decides the precision of 53 bits above the subnormals
    let lead = len - 1 + exp;
    if lead > 1023 { return None; }
    if lead < -1076 { return Some(0.0); }
    let keep = if lead < -1022 { 53 - (-1022 - lead) } else { 53 };
    if keep <= 0 {
        // only a value above half the least subnormal rounds up to it
        let above_half = keep == 0 && (top != 1 << (len - 1) || sticky);
        return Some(if above_half { libm::ldexp(1.0, -1074) } else { 0.0 });
    }
    let drop = len - keep;
    let (m, x) = if drop > 0 {
        let (half, rest, m) = (1u64 << (drop - 1), top & ((1u64 << drop) - 1), top >> drop);
        let up = rest > half || (rest == half && (sticky || m & 1 == 1));
        (m + up as u64, exp + drop)
    } else {
        (top << -drop, exp + drop)
    };
    let f = libm::ldexp(m as f64, x as i32);
    (!f.is_infinite()).then_some(f)
}
