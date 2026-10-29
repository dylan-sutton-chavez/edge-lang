use alloc::string::String;

use edge::lang::{Args, CLASS, Cx, Ret, Sig, TYPE};
use edge::s;
use edge::value::{HeapObj, HeapPool, Val, ftrunc};

use crate::fail::{Fail, fail, receiver};
use crate::fmt::render::Style;
use crate::uni::Digits;
use crate::{Kernel, Lang, Site};

/* a float from a number or text where `D` names the digits text may use */
pub const fn new<S: Site, D: Digits>() -> Kernel { Kernel::new(Sig::range(0, 1), new_run::<S, D>).fast(new_fast).flags(TYPE) }

/* whether a float has no fraction */
pub const fn is_whole<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), is_whole_run) }

/* a float in hexadecimal with thirteen fraction digits */
pub const fn hex<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), hex_run::<S>) }

/* the float hexadecimal text spells which reads off the type itself */
pub const fn from_hex<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), from_hex_run::<S>).flags(CLASS) }

fn new_run<S: Site, D: Digits>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(x) = a.get(0) else { return Ok(Val::float(0.0).into()) };
    if let Some(v) = new_fast(cx.heap_mut(), x) { return Ok(v.into()); }
    if let Some(f) = cx.float(x) { return Ok(Val::float(f?).into()); }
    let read = match cx.str_of(x) {
        Some(s) => Some(parse::<S::Lang, D>(s)),
        None => cx.bytes_of(x).map(|b| { cx.spend(b.len()); parse::<S::Lang, D>(&String::from_utf8_lossy(b)) }),
    };
    match read {
        Some(Some(f)) => Ok(Val::float(f).into()),
        Some(None) => Err(fail::<S>(cx, Fail::FloatLiteral(x))),
        None => Err(fail::<S>(cx, Fail::FloatArg(x))),
    }
}

/* a float of a number off the heap or of plain ascii text */
fn new_fast(heap: &mut HeapPool, x: Val) -> Option<Val> {
    if x.is_float() { return Some(x); }
    if x.is_int() { return Some(Val::float(x.as_int() as f64)); }
    if x.is_bool() { return Some(Val::float(f64::from(u8::from(x.as_bool())))); }
    let Some(HeapObj::Str(s)) = heap.try_get(x) else { return None };
    // a long text counts what it reads as native work
    if s.len() > 64 { heap.spend(s.len()); }
    // text with no space separator or other script reads as the full path reads it
    let b = s.as_bytes();
    let plain = b.first().is_some_and(u8::is_ascii_graphic) && b.last().is_some_and(u8::is_ascii_graphic) && s.is_ascii() && !b.contains(&b'_');
    if !plain { return None; }
    s.parse::<f64>().ok().map(Val::float)
}

/* the float `s` spells with spaces trimmed and each separator `L` takes between two digits */
pub fn parse<L: Lang, D: Digits>(s: &str) -> Option<f64> {
    let t = crate::lang::number_text::<L, D>(s);
    let b = t.as_bytes();
    let Some(sep) = L::NUMERALS.separator else { return t.parse().ok() };
    if !b.contains(&sep) { return t.parse().ok(); }
    let is_sep = |c: u8| L::NUMERALS.separator == Some(c);
    // a separator stands between two digits or beside another where runs are taken
    let near = |i: Option<usize>| i.and_then(|i| b.get(i)).is_some_and(|&c| c.is_ascii_digit() || (L::NUMERALS.doubled && is_sep(c)));
    if b.iter().enumerate().any(|(i, &c)| is_sep(c) && !(near(i.checked_sub(1)) && near(Some(i + 1)))) { return None; }
    t.replace(char::from(sep), "").parse().ok()
}

fn is_whole_run(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    if !a.recv.is_float() { return Err(receiver()); }
    let f = a.recv.as_float();
    Ok(Val::bool(f.is_finite() && f == ftrunc(f)).into())
}

/* the float in hexadecimal with thirteen fraction digits and a subnormal led by a zero */
fn hex_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let f = a.recv.as_float();
    let words = <S::Lang as Style>::FLOATS;
    let text = if f.is_nan() { words.nan.into() } else if f.is_infinite() { if f > 0.0 { words.inf.into() } else { words.neg_inf.into() } } else {
        let sign = if f.is_sign_negative() { "-" } else { "" };
        let bits = f.to_bits();
        let (raw_exp, frac) = ((bits >> 52) & 0x7FF, bits & ((1 << 52) - 1));
        if raw_exp == 0 && frac == 0 { s!(str sign, "0x0.0p+0") } else {
            let (lead, exp) = if raw_exp == 0 { (0, -1022) } else { (1, raw_exp as i64 - 1023) };
            s!(str sign, "0x", int lead, ".", str &alloc::format!("{frac:013x}"), "p", str if exp < 0 { "" } else { "+" }, int exp)
        }
    };
    Ok(cx.new_str(text)?.into())
}

/* the float a hexadecimal text spells rounded half to even as any literal is */
fn from_hex_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(text) = cx.text(a.pos[0]).map(String::from) else { return Err(fail::<S>(cx, Fail::NeedText(a.pos[0]))) };
    cx.spend(text.len());
    match parse_hex_float(text.trim()) {
        Some(Ok(f)) => Ok(Val::float(f).into()),
        Some(Err(())) => Err(fail::<S>(cx, Fail::HexFloatRange)),
        None => Err(fail::<S>(cx, Fail::HexFloat)),
    }
}

/* the float hex text spells or none for bad text and an error past floats */
fn parse_hex_float(s: &str) -> Option<Result<f64, ()>> {
    let (neg, body) = match s.as_bytes().first() { Some(b'-') => (true, &s[1..]), Some(b'+') => (false, &s[1..]), _ => (false, s) };
    let signed = |f: f64| if neg { -f } else { f };
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return Some(Ok(signed(f64::INFINITY))),
        "nan" => return Some(Ok(f64::NAN)),
        _ => {}
    }
    let body = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")).unwrap_or(body);
    let (digits, exp) = match body.find(['p', 'P']) { Some(i) => (&body[..i], body[i + 1..].parse::<i64>().ok()?), None => (body, 0) };
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac_part.is_empty() { return None; }
    // up to 120 bits of digits form the mantissa and the rest only mark nonzero
    let (mut m, mut e, mut sticky) = (0u128, exp.clamp(-100_000, 100_000) as i32, false);
    for (i, c) in int_part.chars().chain(frac_part.chars()).enumerate() {
        let d = c.to_digit(16)? as u128;
        let fraction = i >= int_part.len();
        if m >> 116 == 0 { m = m << 4 | d; if fraction { e -= 4; } }
        else { sticky |= d != 0; if !fraction { e += 4; } }
    }
    if m == 0 { return Some(Ok(signed(0.0))); }
    let bits = 128 - m.leading_zeros() as i32;
    let top = bits - 1 + e;
    if top > 1023 { return Some(Err(())); }
    // a normal float keeps 53 bits and a subnormal fewer down to none
    let precision = if top >= -1022 { 53 } else { top + 1075 };
    if precision < 0 { return Some(Ok(signed(0.0))); }
    let shift = bits - precision;
    let mut kept = if shift > 0 { m >> shift } else { m };
    if shift > 0 {
        let rest = m & ((1 << shift) - 1);
        let half = 1u128 << (shift - 1);
        if rest > half || (rest == half && (sticky || kept & 1 == 1)) { kept += 1; }
    }
    let f = super::scalbn(kept as f64, e + shift.max(0));
    if f.is_infinite() { return Some(Err(())); }
    Some(Ok(signed(f)))
}
