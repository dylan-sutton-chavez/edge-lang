/* `python.complex`, the FOREIGN kind that holds a complex number as two floats, a member of the numbers family so
   `1 + 0j == 1` holds and both hash alike. */
use alloc::{rc::Rc, string::String};
use core::any::Any;

use crate::value::foreign::{BinOp, Kind, KindToken, Snapshot, Transfer, UnOp};
use crate::value::{HeapPool, Val, VmErr, as_i128, num_f64};

/* A complex number, its real and imaginary parts. */
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Complex { pub re: f64, pub im: f64 }

pub static KIND: Kind = Kind {
    name: "python.complex",
    version: 1,
    type_name: "complex",
    family: "python.number",
    immutable: true,
    repr: |p| Ok(repr(*of(p))),
    truthy: |p| { let z = of(p); z.re != 0.0 || z.im != 0.0 },
    hash: |p| hash(*of(p)) as u64,
    eq: |heap, p, other| parts(heap, other).is_some_and(|w| w == *of(p)),
    cmp: None,
    binop: Some(binop),
    unop: Some(unop),
    to_f64: None,
    to_i128: None,
    from_i128: None,
    trace: None,
    snapshot: Snapshot::Serialize {
        save: |p, out| { let z = of(p); out.extend_from_slice(&z.re.to_le_bytes()); out.extend_from_slice(&z.im.to_le_bytes()); },
        load: |b| {
            let half = |i: usize| b.get(i..i + 8).and_then(|s| s.try_into().ok()).map(f64::from_le_bytes);
            Some(Rc::new(Complex { re: half(0)?, im: half(8)? }) as Rc<dyn Any>)
        },
    },
    transfer: Transfer::Copy,
};

fn of(p: &dyn Any) -> &Complex { p.downcast_ref::<Complex>().expect("python.complex payload") }

fn raised(m: &str) -> VmErr { VmErr::Raised(m.into()) }

fn token(heap: &mut HeapPool) -> KindToken {
    match heap.foreign.token_of(&KIND) { Some(t) => t, None => heap.register_kind(&KIND) }
}

/* A complex value from its two parts. */
pub fn val(heap: &mut HeapPool, re: f64, im: f64) -> Result<Val, VmErr> {
    let t = token(heap);
    heap.foreign_box(t, Rc::new(Complex { re, im }))
}

/* Whether `v` is a complex value. */
pub fn is_complex(heap: &HeapPool, v: Val) -> bool { heap.foreign_kind(v).is_some_and(|k| core::ptr::eq(k, &KIND)) }

/* The parts of any number, an int, bool or float with a zero imaginary part, None for anything else. */
pub fn parts(heap: &HeapPool, v: Val) -> Option<Complex> {
    if is_complex(heap, v) { return heap.foreign.token_of(&KIND).and_then(|t| heap.foreign_payload::<Complex>(t, v)).copied(); }
    if v.is_float() || v.is_int() || v.is_bool() || as_i128(v, heap).is_some() || crate::frontend::int::is_big(heap, v) {
        return Some(Complex { re: num_f64(v, heap)?.ok()?, im: 0.0 });
    }
    None
}

/* A part as complex repr writes it, a whole number without `.0`. */
fn part(f: f64) -> String {
    let s = crate::util::fstr::format_f64(f);
    match s.strip_suffix(".0") { Some(t) => t.into(), None => s }
}

/* `(1+2j)`, or `2j` alone when the real part is a positive zero. */
pub fn repr(z: Complex) -> String {
    let im = part(z.im);
    let sign = if im.starts_with('-') { "" } else { "+" };
    if z.re == 0.0 && z.re.is_sign_positive() { return crate::s!(str &im, "j"); }
    crate::s!("(", str &part(z.re), str sign, str &im, "j)")
}

/* The hash of the real part plus a multiple of the hash of the imaginary part, as numbers hash. */
fn hash(z: Complex) -> i64 {
    let (r, i) = (crate::value::eq::hash_float(z.re) as u64, crate::value::eq::hash_float(z.im) as u64);
    let h = r.wrapping_add(i.wrapping_mul(1_000_003)) as i64;
    if h == -1 { -2 } else { h }
}

fn mul(a: Complex, b: Complex) -> Complex { Complex { re: a.re * b.re - a.im * b.im, im: a.re * b.im + a.im * b.re } }

/* `a / b`, scaled by the larger part of `b` so the quotient keeps its precision. */
fn div(a: Complex, b: Complex) -> Result<Complex, VmErr> {
    let (abr, abi) = (libm::fabs(b.re), libm::fabs(b.im));
    if abr >= abi {
        if abr == 0.0 { return Err(raised("ZeroDivisionError: division by zero")); }
        let ratio = b.im / b.re;
        let denom = b.re + b.im * ratio;
        Ok(Complex { re: (a.re + a.im * ratio) / denom, im: (a.im - a.re * ratio) / denom })
    } else if abi >= abr {
        let ratio = b.re / b.im;
        let denom = b.re * ratio + b.im;
        Ok(Complex { re: (a.re * ratio + a.im) / denom, im: (a.im * ratio - a.re) / denom })
    } else {
        Ok(Complex { re: f64::NAN, im: f64::NAN })
    }
}

/* `a ** n` for a whole exponent, by squaring so small powers stay exact. */
fn pow_int(a: Complex, n: i64) -> Result<Complex, VmErr> {
    let (mut r, mut x, mut e) = (Complex { re: 1.0, im: 0.0 }, a, n.unsigned_abs());
    while e > 0 {
        if e & 1 == 1 { r = mul(r, x); }
        x = mul(x, x);
        e >>= 1;
    }
    if n < 0 { div(Complex { re: 1.0, im: 0.0 }, r) } else { Ok(r) }
}

/* `a ** b`, whole exponents up to 100 by squaring and the rest through the polar form. */
fn pow(a: Complex, b: Complex) -> Result<Complex, VmErr> {
    if b.re == 0.0 && b.im == 0.0 { return Ok(Complex { re: 1.0, im: 0.0 }); }
    if a.re == 0.0 && a.im == 0.0 {
        if b.im != 0.0 || b.re < 0.0 { return Err(raised("ZeroDivisionError: zero to a negative or complex power")); }
        return Ok(Complex { re: 0.0, im: 0.0 });
    }
    let r = if b.im == 0.0 && b.re == libm::floor(b.re) && libm::fabs(b.re) <= 100.0 { pow_int(a, b.re as i64)? } else {
        let vabs = libm::hypot(a.re, a.im);
        let mut len = libm::pow(vabs, b.re);
        let at = libm::atan2(a.im, a.re);
        let mut phase = at * b.re;
        if b.im != 0.0 {
            len /= libm::exp(at * b.im);
            phase += b.im * libm::log(vabs);
        }
        Complex { re: len * libm::cos(phase), im: len * libm::sin(phase) }
    };
    let finite = |z: Complex| z.re.is_finite() && z.im.is_finite();
    if !finite(r) && finite(a) && finite(b) { return Err(raised("OverflowError: complex exponentiation")); }
    Ok(r)
}

/* `a ** b` of two floats as complex numbers, what a negative base under a fractional power gives. */
pub fn power(heap: &mut HeapPool, a: f64, b: f64) -> Result<Val, VmErr> {
    let r = pow(Complex { re: a, im: 0.0 }, Complex { re: b, im: 0.0 })?;
    val(heap, r.re, r.im)
}

fn binop(heap: &mut HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
    let (Some(x), Some(y)) = (parts(heap, a), parts(heap, b)) else { return Ok(None) };
    // A real operand takes part as a real number alone, so its missing imaginary zero never flips a sign.
    let (xr, yr) = (!is_complex(heap, a), !is_complex(heap, b));
    let r = match op {
        BinOp::Add if xr => Complex { re: x.re + y.re, im: y.im },
        BinOp::Add if yr => Complex { re: x.re + y.re, im: x.im },
        BinOp::Add => Complex { re: x.re + y.re, im: x.im + y.im },
        BinOp::Sub if xr => Complex { re: x.re - y.re, im: -y.im },
        BinOp::Sub if yr => Complex { re: x.re - y.re, im: x.im },
        BinOp::Sub => Complex { re: x.re - y.re, im: x.im - y.im },
        BinOp::Mul if xr => Complex { re: x.re * y.re, im: x.re * y.im },
        BinOp::Mul if yr => Complex { re: x.re * y.re, im: x.im * y.re },
        BinOp::Mul => mul(x, y),
        BinOp::TrueDiv if yr => {
            if y.re == 0.0 { return Err(raised("ZeroDivisionError: division by zero")); }
            Complex { re: x.re / y.re, im: x.im / y.re }
        }
        BinOp::TrueDiv => div(x, y)?,
        BinOp::Pow => pow(x, y)?,
        _ => return Ok(None),
    };
    val(heap, r.re, r.im).map(Some)
}

fn unop(heap: &mut HeapPool, op: UnOp, v: Val) -> Result<Option<Val>, VmErr> {
    let Some(z) = parts(heap, v) else { return Ok(None) };
    match op {
        UnOp::Neg => val(heap, -z.re, -z.im).map(Some),
        UnOp::Pos => Ok(Some(v)),
        UnOp::Abs => {
            let m = libm::hypot(z.re, z.im);
            if m.is_infinite() && z.re.is_finite() && z.im.is_finite() { return Err(raised("OverflowError: absolute value too large")); }
            Ok(Some(Val::float(m)))
        }
        UnOp::Invert => Ok(None),
    }
}

/* `complex("1+2j")`, spaces around it and parentheses allowed, None when the text is no complex number. */
pub fn parse(text: &str) -> Option<Complex> {
    let t = text.trim();
    let t = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')).map_or(t, str::trim);
    let float = |s: &str| -> Option<f64> {
        if s.is_empty() || s.contains("__") || s.starts_with('_') || s.ends_with('_') { return None; }
        let s = s.replace('_', "");
        match s.to_ascii_lowercase().trim_start_matches(['+', '-']) {
            "inf" | "infinity" | "nan" => {}
            body if body.starts_with(|c: char| c.is_ascii_digit() || c == '.') => {}
            _ => return None,
        }
        s.parse().ok()
    };
    let Some(body) = t.strip_suffix(['j', 'J']) else { return Some(Complex { re: float(t)?, im: 0.0 }) };
    // The sign that starts the imaginary part, past any exponent sign.
    let bytes = body.as_bytes();
    let split = (1..bytes.len()).rev().find(|&i| matches!(bytes[i], b'+' | b'-') && !matches!(bytes[i - 1], b'e' | b'E'));
    let imag = |s: &str| if s == "+" || s.is_empty() { Some(1.0) } else if s == "-" { Some(-1.0) } else { float(s) };
    match split {
        Some(i) => Some(Complex { re: float(&body[..i])?, im: imag(&body[i..])? }),
        None => Some(Complex { re: 0.0, im: imag(body)? }),
    }
}
