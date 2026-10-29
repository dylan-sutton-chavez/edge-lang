use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use edge::lang::{Args, CLASS, Cx, Ret, Sig, TYPE};
use edge::s;
use edge::value::{BinOp, HeapObj, HeapPool, Val, VmErr, cold_overflow, fabs, ftrunc};

use super::{big, bool_int};
use crate::fail::{Fail, fail, receiver};
use crate::lang::Endian;
use crate::uni::Digits;
use crate::{Kernel, Lang, Site};

/* an int from a number or text in a base where `D` names the digits */
pub const fn new<S: Site, D: Digits>() -> Kernel { Kernel::new(Sig::range(0, 2), new_run::<S, D>).fast(new_fast).flags(TYPE) }

/* an int in the power of two base `R` names after its sign and prefix */
pub const fn radix<S: Site, R: Radix>() -> Kernel { Kernel::new(Sig::exact(1), radix_run::<S, R>).fast(radix_fast::<R>) }

/* how many bits an int's magnitude takes */
pub const fn bit_length<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), bit_length_run) }

/* how many ones an int's magnitude holds */
pub const fn bit_count<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), bit_count_run) }

/* an int's bytes in a length and an order signed or not */
pub const fn to_bytes<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 3), to_bytes_run::<S>) }

/* the int bytes spell in an order signed or not read off the type */
pub const fn from_bytes<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 3), from_bytes_run::<S>).flags(CLASS) }

/* a power of two base and the prefix its text takes */
pub trait Radix: 'static {
    const SHIFT: u32;
    const PREFIX: &'static str;
}

/* base two */
pub struct Bin;

/* base eight */
pub struct Oct;

/* base sixteen */
pub struct Hex;

impl Radix for Bin { const SHIFT: u32 = 1; const PREFIX: &'static str = "0b"; }

impl Radix for Oct { const SHIFT: u32 = 3; const PREFIX: &'static str = "0o"; }

impl Radix for Hex { const SHIFT: u32 = 4; const PREFIX: &'static str = "0x"; }

/* why text spells no int */
pub enum Parse {
    // the text is no literal in the base
    Literal,
    Fail(VmErr),
}

impl From<VmErr> for Parse { fn from(e: VmErr) -> Self { Parse::Fail(e) } }

fn new_run<S: Site, D: Digits>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(x) = a.get(0) else {
        if a.get(1).is_some() { return Err(fail::<S>(cx, Fail::IntMissing)); }
        return Ok(Val::int(0).into());
    };
    if let Some(base) = a.get(1) { return based::<S, D>(cx, x, base); }
    if let Some(v) = new_fast(cx.heap_mut(), x) { return Ok(v.into()); }
    if x.is_float() { return Ok(float_int::<S>(cx, x.as_float())?.into()); }
    if cx.heap().is_wide(x) { return Ok(x.into()); }
    if let Some(n) = cx.bytes_of(x).and_then(plain_decimal) { return Ok(cx.new_int(i128::from(n))?.into()); }
    match int_text(cx, x) {
        Some(Some(text)) => parsed::<S, D>(cx, x, &text, 10),
        Some(None) => Err(fail::<S>(cx, Fail::IntLiteral { base: 10, v: x })),
        None => Err(fail::<S>(cx, Fail::IntArg(x))),
    }
}

/* an int of an int a bool a fitting float or plain decimal text */
fn new_fast(heap: &mut HeapPool, x: Val) -> Option<Val> {
    if x.is_int() { return Some(x); }
    if x.is_bool() { return Some(Val::int(x.as_bool() as i64)); }
    if x.is_float() {
        let f = x.as_float();
        return (f.is_finite() && fabs(f) < Val::INT_MAX as f64).then(|| Val::int(f as i64));
    }
    let Some(HeapObj::Str(s)) = heap.try_get(x) else { return None };
    let n = plain_decimal(s.as_bytes())?;
    heap.int(n as i128).ok()
}

/* a sign and at most eighteen ascii digits which no word overflows */
fn plain_decimal(b: &[u8]) -> Option<i64> {
    let (neg, digits) = match b.first()? { b'-' => (true, &b[1..]), b'+' => (false, &b[1..]), _ => (false, b) };
    if digits.is_empty() || digits.len() > 18 { return None; }
    let mut n = 0i64;
    for &d in digits {
        if !d.is_ascii_digit() { return None; }
        n = n * 10 + i64::from(d - b'0');
    }
    Some(if neg { -n } else { n })
}

/* an int with a base checked first where only text reads in one */
fn based<S: Site, D: Digits>(cx: &mut Cx<'_, '_>, x: Val, base: Val) -> Ret {
    // a bool counts where bools are ints and a wide int lies past every base
    let base = if base.is_int() { base.as_int() } else if let Some(b) = bool_int(cx.heap(), base) { b } else if cx.heap().is_wide(base) { i64::MAX } else {
        return Err(fail::<S>(cx, Fail::IntBaseType));
    };
    if base != 0 && !(2..=36).contains(&base) { return Err(fail::<S>(cx, Fail::IntBase)); }
    if base == 10 && let Some(n) = cx.text(x).map(str::as_bytes).or_else(|| cx.bytes_of(x)).and_then(plain_decimal) { return Ok(cx.new_int(i128::from(n))?.into()); }
    match int_text(cx, x) {
        Some(Some(text)) => parsed::<S, D>(cx, x, &text, base),
        Some(None) => Err(fail::<S>(cx, Fail::IntLiteral { base, v: x })),
        None => Err(fail::<S>(cx, Fail::IntExplicitBase)),
    }
}

/* the text of a text or utf-8 bytes with an inner none for bad bytes */
fn int_text(cx: &Cx<'_, '_>, x: Val) -> Option<Option<String>> {
    if let Some(s) = cx.text(x) { return Some(Some(String::from(s))); }
    Some(cx.bytes_of(x).map(|b| core::str::from_utf8(b).map(String::from))?.ok())
}

/* the int `text` spells in `base` where an invalid literal quotes `x` as written */
fn parsed<S: Site, D: Digits>(cx: &mut Cx<'_, '_>, x: Val, text: &str, base: i64) -> Ret {
    match parse::<S::Lang, D>(cx, text, base) {
        Ok(v) => Ok(v.into()),
        Err(Parse::Literal) => Err(fail::<S>(cx, Fail::IntLiteral { base, v: x })),
        Err(Parse::Fail(e)) => Err(e),
    }
}

/* the int `s` spells in `base` with a sign and prefix and `L`'s separators */
pub fn parse<L: Lang, D: Digits>(cx: &mut Cx<'_, '_>, s: &str, base: i64) -> Result<Val, Parse> {
    // a base past 36 reads no digit and callers word that first
    if base != 0 && !(2..=36).contains(&base) { return Err(Parse::Literal); }
    let t = crate::lang::number_text::<L, D>(s);
    let t = t.as_ref();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let prefix = if rest.len() >= 2 && rest.as_bytes()[0] == b'0' {
        match rest.as_bytes()[1] | 0x20 { b'x' => Some(16u32), b'o' => Some(8), b'b' => Some(2), _ => None }
    } else { None };
    let (mut radix, mut body, mut had_prefix) = (base as u32, rest, false);
    if base == 0 {
        match prefix {
            Some(p) => (radix, body, had_prefix) = (p, &rest[2..], true),
            None => {
                radix = 10;
                // a runtime may refuse a leading zero unless every digit is zero
                let zeros = || rest.bytes().all(|b| b == b'0' || Some(b) == L::NUMERALS.separator);
                if !L::NUMERALS.leading_zero && rest.len() > 1 && rest.starts_with('0') && !zeros() { return Err(Parse::Literal); }
            }
        }
    } else if L::NUMERALS.prefixed_base && prefix == Some(base as u32) {
        // an explicit base may carry its own prefix
        (body, had_prefix) = (&rest[2..], true);
    }
    // one separator may follow the base prefix
    if let Some(c) = L::NUMERALS.separator && had_prefix && L::NUMERALS.after_prefix && body.as_bytes().first() == Some(&c) { body = &body[1..]; }
    // a sign after the first one or after the prefix spells no int
    if body.starts_with(['+', '-']) { return Err(Parse::Literal); }
    let cleaned: Cow<'_, str> = match L::NUMERALS.separator {
        Some(c) if body.as_bytes().contains(&c) => {
            let b = body.as_bytes();
            if b.first() == Some(&c) || b.last() == Some(&c) || (!L::NUMERALS.doubled && b.windows(2).any(|w| w == [c, c])) { return Err(Parse::Literal); }
            body.chars().filter(|&d| d != char::from(c)).collect::<String>().into()
        }
        _ => body.into(),
    };
    if cleaned.is_empty() { return Err(Parse::Literal); }
    if let Ok(n) = i64::from_str_radix(&cleaned, radix) { return Ok(cx.new_int(if neg { -(n as i128) } else { n as i128 })?); }
    let digits: Option<Vec<u8>> = cleaned.chars().map(|c| c.to_digit(radix).map(|d| d as u8)).collect();
    let Some(digits) = digits else { return Err(Parse::Literal) };
    Ok(big::from_digits::<L>(cx, neg, &digits, radix)?)
}

// every float below two to the 127 fits an i128
const I128_EDGE: f64 = (1u128 << 127) as f64;

/* the int a float truncates to where nan and infinity fail */
fn float_int<S: Site>(cx: &mut Cx<'_, '_>, f: f64) -> Result<Val, VmErr> {
    if f.is_nan() { return Err(fail::<S>(cx, Fail::FloatNan)); }
    if f.is_infinite() { return Err(fail::<S>(cx, Fail::FloatInf)); }
    let t = ftrunc(f);
    if fabs(t) < I128_EDGE { return cx.new_int(t as i128); }
    // past two to the 127 the float is its mantissa shifted left by its exponent
    let bits = t.to_bits();
    let mantissa = i128::from((bits & ((1 << 52) - 1)) | (1 << 52));
    let m = cx.new_int(if t < 0.0 { -mantissa } else { mantissa })?;
    let shift = Val::int(((bits >> 52) & 0x7ff) as i64 - 1075);
    cx.heap_mut().foreign_binop(BinOp::Shl, m, shift)?.ok_or_else(cold_overflow)
}

fn radix_run<S: Site, R: Radix>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let x = a.pos[0];
    if let Some(v) = radix_fast::<R>(cx.heap_mut(), x) { return Ok(v.into()); }
    let Some(d) = big::radix_digits::<S::Lang>(cx, x, 1 << R::SHIFT) else { return Err(fail::<S>(cx, Fail::NotIndex(x))) };
    let (neg, digits) = d?;
    Ok(cx.new_str(s!(str if neg { "-" } else { "" }, str R::PREFIX, str &digits))?.into())
}

/* `radix` of an inline int or bool written with no wide digits */
fn radix_fast<R: Radix>(heap: &mut HeapPool, x: Val) -> Option<Val> {
    let n = if x.is_int() { x.as_int() } else { bool_int(heap, x)? };
    let (mut digits, mut at, mut m) = ([0u8; 64], 64, n.unsigned_abs());
    loop {
        at -= 1;
        digits[at] = b"0123456789abcdef"[(m & ((1 << R::SHIFT) - 1)) as usize];
        m >>= R::SHIFT;
        if m == 0 { break; }
    }
    let mut out = String::with_capacity(3 + 64 - at);
    if n < 0 { out.push('-'); }
    out.push_str(R::PREFIX);
    out.extend(digits[at..].iter().map(|&d| char::from(d)));
    heap.alloc(HeapObj::Str(out)).ok()
}

fn bit_length_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let bits = match cx.int(a.recv) { Some(i) => u64::from(128 - i.unsigned_abs().leading_zeros()), None => big::bit_length(cx, a.recv).ok_or_else(receiver)? };
    Ok(Val::int(bits as i64).into())
}

fn bit_count_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ones = match cx.int(a.recv) { Some(i) => u64::from(i.unsigned_abs().count_ones()), None => big::bit_count(cx, a.recv).ok_or_else(receiver)? };
    Ok(Val::int(ones as i64).into())
}

/* the bytes of an int given the int its length and its order */
pub const fn int_bytes<S: Site>() -> Kernel { Kernel::new(Sig::exact(3), |cx, a| to_bytes_of::<S>(cx, a.pos[0], a.get(1), a.get(2), None)) }

/* the int bytes spell as a function of immutable bytes and an order */
pub const fn bytes_int<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), bytes_int_run::<S>) }

fn bytes_int_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    if !matches!(cx.heap().try_get(a.pos[0]), Some(HeapObj::Bytes(_))) { return Err(fail::<S>(cx, Fail::BytesFirst)); }
    from_bytes_run::<S>(cx, a)
}

fn to_bytes_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { to_bytes_of::<S>(cx, a.recv, a.get(0), a.get(1), a.get(2)) }

/* the bytes of `v` unsigned unless asked and big endian unless told */
fn to_bytes_of<S: Site>(cx: &mut Cx<'_, '_>, v: Val, length: Option<Val>, order: Option<Val>, signed: Option<Val>) -> Ret {
    let length = match length {
        None => 1,
        Some(n) if n.is_int() && n.as_int() < 0 => return Err(fail::<S>(cx, Fail::LengthNegative)),
        Some(n) if n.is_int() => n.as_int() as usize,
        Some(_) => return Err(fail::<S>(cx, Fail::LengthType)),
    };
    // a length past the memory left fails at once and a long one pays
    cx.reserve(length)?;
    if length > 64 { cx.spend(length); }
    let big = order_is_big::<S>(cx, order)?;
    let signed = signed.is_some_and(|s| cx.truthy(s));
    let out = big::to_bytes::<S::Lang>(cx, v, length, big, signed).ok_or_else(receiver)??;
    Ok(cx.new_bytes(out, false)?.into())
}

/* the int the first argument's bytes spell big endian unsigned unless told */
fn from_bytes_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (b, order, signed) = (a.pos[0], a.get(1), a.get(2));
    let Some(bytes) = cx.bytes_of(b).map(<[u8]>::to_vec) else { return Err(fail::<S>(cx, Fail::BufferArg(b))) };
    cx.spend(bytes.len());
    let big = order_is_big::<S>(cx, order)?;
    let signed = signed.is_some_and(|s| cx.truthy(s));
    let mut le = bytes;
    if big { le.reverse(); }
    Ok(big::from_bytes_le::<S::Lang>(cx, &le, signed)?.into())
}

/* whether the byte order the runtime's word names is big where none reads big */
fn order_is_big<S: Site>(cx: &mut Cx<'_, '_>, order: Option<Val>) -> Result<bool, VmErr> {
    let Some(v) = order.filter(|v| !v.is_none()) else { return Ok(true) };
    match cx.text(v).and_then(<S::Lang as Lang>::endian) {
        Some(e) => Ok(e == Endian::Big),
        None => Err(fail::<S>(cx, Fail::ByteOrder)),
    }
}
