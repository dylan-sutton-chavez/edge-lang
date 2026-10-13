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
    // Round half to even, no i64 cast so large/huge values are exact.
    if !x.is_finite() { return x; }
    let fl = libm::floor(x);
    let diff = x - fl;
    if diff < 0.5 { fl }
    else if diff > 0.5 { fl + 1.0 }
    // Exactly .5, pick the even neighbour. `fl` is integral here, so test evenness without a cast.
    else if libm::floor(fl / 2.0) * 2.0 == fl { fl }
    else { fl + 1.0 }
}

/* Whether `base ** exp` has only a complex result, a finite negative base under a fractional power. */
#[inline]
pub fn complex_power(base: f64, exp: f64) -> bool {
    base < 0.0 && base > f64::NEG_INFINITY && exp.is_finite() && libm::floor(exp) != exp
}

/* `base ** exp` as libm `pow` rounds it, None when finite operands overflow to an infinity. */
#[inline]
pub fn fpowf(base: f64, exp: f64) -> Option<f64> {
    // These exponents take one correctly rounded operation, which is what `pow` returns for them.
    let r = if exp == 2.0 { base * base } else if exp == 1.0 { base } else if exp == 0.0 { 1.0 } else if exp == -1.0 { 1.0 / base } else { libm::pow(base, exp) };
    (!r.is_infinite() || !base.is_finite() || !exp.is_finite()).then_some(r)
}

/* Floor quotient and remainder of ints, None for a zero divisor or `i128::MIN / -1`. */
pub fn int_divmod(a: i128, b: i128) -> Option<(i128, i128)> {
    let q = a.checked_div(b)?;
    let r = a - q * b;
    Some(if r != 0 && (r < 0) != (b < 0) { (q - 1, r + b) } else { (q, r) })
}

/* Floor quotient and remainder of floats, exact where `a - floor(a/b)*b` loses precision. */
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
