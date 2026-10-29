use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::any::Any;
use core::cmp::Ordering;

use edge::value::foreign::{BinOp, Kind, Snapshot, Transfer, UnOp};
use edge::value::{HeapPool, Val, VmErr, as_i128};

use super::{Dates, Duration, Time, duration_val, held, part_of, time_val};
use crate::fail::{Fail, bare};

/* the kind a runtime boxes its times in under the type name `type_name` */
pub const fn time_kind<D: Dates>(type_name: &'static str) -> Kind {
    Kind {
        name: "date.time",
        version: 1,
        type_name,
        family: "date.time",
        immutable: true,
        repr: |p| Ok(p.downcast_ref::<Time>().map_or_else(String::new, Time::text)),
        truthy: |_, _| true,
        hash: |_, p| p.downcast_ref::<Time>().map_or(0, |t| mix(t.nanos)),
        // two texts naming one instant in other offsets are equal times
        eq: |heap, p, other| order::<Time>(heap, p, other) == Some(Ordering::Equal),
        cmp: Some(order::<Time>),
        binop: Some(binop::<D>),
        unop: None,
        to_f64: None,
        to_i128: None,
        from_i128: None,
        from_digits: None,
        trace: None,
        get: Some(field),
        snapshot: Snapshot::Serialize { save: save_time, load: load_time },
        transfer: Transfer::Deny,
        ..Kind::PLAIN
    }
}

/* the kind a runtime boxes its durations in under the type name `type_name` */
pub const fn duration_kind<D: Dates>(type_name: &'static str) -> Kind {
    Kind {
        name: "date.duration",
        version: 1,
        type_name,
        family: "date.duration",
        immutable: true,
        repr: |p| Ok(p.downcast_ref::<Duration>().map_or_else(String::new, Duration::text)),
        truthy: |_, p| p.downcast_ref::<Duration>().is_some_and(|d| d.nanos != 0),
        hash: |_, p| p.downcast_ref::<Duration>().map_or(0, |d| mix(d.nanos)),
        eq: |heap, p, other| order::<Duration>(heap, p, other) == Some(Ordering::Equal),
        cmp: Some(order::<Duration>),
        binop: Some(binop::<D>),
        unop: Some(unop::<D>),
        to_f64: None,
        to_i128: None,
        from_i128: None,
        from_digits: None,
        trace: None,
        get: Some(field),
        snapshot: Snapshot::Serialize { save: save_duration, load: load_duration },
        transfer: Transfer::Deny,
        ..Kind::PLAIN
    }
}

/* a time's civil field or a duration's total read by its neutral name */
fn field(heap: &mut HeapPool, v: Val, name: &str) -> Result<Option<Val>, VmErr> { Ok(part_of(heap, v, name)) }

/* a payload ordered by its nanoseconds */
trait Nanos: Any {
    fn nanos(&self) -> i128;
}

impl Nanos for Time {
    fn nanos(&self) -> i128 { self.nanos }
}

impl Nanos for Duration {
    fn nanos(&self) -> i128 { self.nanos }
}

/* the order of a payload against a value holding a payload of its type */
fn order<T: Nanos>(heap: &HeapPool, p: &dyn Any, other: Val) -> Option<Ordering> {
    Some(p.downcast_ref::<T>()?.nanos().cmp(&held::<T>(heap, other)?.nanos()))
}

/* a hash of nanoseconds which equal values share */
fn mix(n: i128) -> u64 { ((n as u64) ^ ((n >> 64) as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15) }

/* what an operator gives before it is boxed */
enum Out { Time(Time), Span(i128), Float(f64) }

fn binop<D: Dates>(heap: &mut HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
    match compute::<D>(heap, op, a, b)? {
        Some(Out::Time(t)) => time_val::<D>(heap, t).map(Some),
        Some(Out::Span(nanos)) => duration_val::<D>(heap, Duration { nanos }).map(Some),
        Some(Out::Float(f)) => Ok(Some(Val::float(f))),
        None => Ok(None),
    }
}

/* `op` on a time or duration and another operand or none so the engine raises */
fn compute<D: Dates>(heap: &HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Out>, VmErr> {
    let (ta, tb) = (held::<Time>(heap, a).copied(), held::<Time>(heap, b).copied());
    let (da, db) = (held::<Duration>(heap, a).map(|d| d.nanos), held::<Duration>(heap, b).map(|d| d.nanos));
    let span = |n: Option<i128>| n.map(Out::Span).ok_or_else(long::<D>);
    let scale = |span: i128, by: Val, divide: bool| number::<D>(heap, by).map(|n| n.and_then(|n| scaled::<D>(span, n, divide)).map(Out::Span));
    Ok(Some(match (op, ta, tb, da, db) {
        (BinOp::Sub, Some(x), Some(y), ..) => span(x.nanos.checked_sub(y.nanos))?,
        (BinOp::Add, Some(t), _, _, Some(d)) | (BinOp::Add, _, Some(t), Some(d), _) => Out::Time(moved::<D>(t, Some(d))?),
        (BinOp::Sub, Some(t), _, _, Some(d)) => Out::Time(moved::<D>(t, d.checked_neg())?),
        (BinOp::Add, _, _, Some(x), Some(y)) => span(x.checked_add(y))?,
        (BinOp::Sub, _, _, Some(x), Some(y)) => span(x.checked_sub(y))?,
        (BinOp::TrueDiv, _, _, Some(_), Some(0)) => return Err(VmErr::ZeroDiv),
        (BinOp::TrueDiv, _, _, Some(x), Some(y)) => Out::Float(x as f64 / y as f64),
        (BinOp::Mul, None, None, Some(x), None) => match scale(x, b, false) { Some(r) => r?, None => return Ok(None) },
        (BinOp::Mul, None, None, None, Some(y)) => match scale(y, a, false) { Some(r) => r?, None => return Ok(None) },
        (BinOp::TrueDiv, None, None, Some(x), None) => match scale(x, b, true) { Some(r) => r?, None => return Ok(None) },
        _ => return Ok(None),
    }))
}

/* a number a span scales by */
enum Num { Int(i128), Float(f64) }

/* the number `v` holds and none for a value that is no number */
fn number<D: Dates>(heap: &HeapPool, v: Val) -> Option<Result<Num, VmErr>> {
    if v.is_float() { return Some(Ok(Num::Float(v.as_float()))); }
    if let Some(i) = as_i128(v, heap) { return Some(Ok(Num::Int(i))); }
    // an int past an i128 takes a span past the range
    heap.is_wide(v).then(|| Err(long::<D>()))
}

/* `span` times or over a number rounded half to even to whole nanoseconds */
fn scaled<D: Dates>(span: i128, n: Num, divide: bool) -> Result<i128, VmErr> {
    let r = match n {
        Num::Int(0) if divide => return Err(VmErr::ZeroDiv),
        Num::Float(f) if divide && f == 0.0 => return Err(VmErr::ZeroDiv),
        Num::Int(i) if divide => rounded(span, i),
        Num::Int(i) => span.checked_mul(i),
        Num::Float(f) if !f.is_finite() => None,
        Num::Float(f) => {
            let (m, e) = split(f);
            if divide { over(span, m, e) } else { times(span, m, e) }
        }
    };
    r.ok_or_else(long::<D>)
}

/* a finite float as a signed odd mantissa and the power of two scaling it */
fn split(f: f64) -> (i128, i32) {
    let bits = f.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = i128::from(bits & ((1 << 52) - 1));
    let (m, e) = if exp == 0 { (frac, -1074) } else { (frac | (1 << 52), exp - 1075) };
    if m == 0 { return (0, 0); }
    let z = m.trailing_zeros();
    let m = m >> z;
    (if bits >> 63 == 1 { -m } else { m }, e + z as i32)
}

/* `span` times `m` times two to the `e` rounded half to even */
fn times(span: i128, m: i128, e: i32) -> Option<i128> {
    let p = span.checked_mul(m)?;
    if p == 0 { return Some(0); }
    if e >= 0 { return p.checked_mul(power(e.unsigned_abs())?); }
    Some(halved(p, e.unsigned_abs()))
}

/* `span` over `m` times two to the `e` rounded half to even */
fn over(span: i128, m: i128, e: i32) -> Option<i128> {
    if span == 0 { return Some(0); }
    if e <= 0 { return rounded(span.checked_mul(power(e.unsigned_abs())?)?, m); }
    // a divisor past an i128 leaves under a nanosecond
    let Some(den) = power(e.unsigned_abs()).and_then(|s| m.checked_mul(s)) else { return Some(0) };
    rounded(span, den)
}

/* two to the `s` while it fits an i128 */
fn power(s: u32) -> Option<i128> { 1i128.checked_shl(s).filter(|&p| p > 0) }

/* `p` over a nonzero `den` rounded half to even */
fn rounded(p: i128, den: i128) -> Option<i128> {
    let (p, den) = if den < 0 { (p.checked_neg()?, den.checked_neg()?) } else { (p, den) };
    if den == 0 { return None; }
    let (q, r) = (p / den, p % den);
    // a remainder of half the divisor goes to the even quotient
    let (twice, whole) = (r.unsigned_abs() * 2, den.unsigned_abs());
    let away = twice > whole || (twice == whole && q % 2 != 0);
    Some(if !away { q } else if p < 0 { q - 1 } else { q + 1 })
}

/* `p` over two to the `s` rounded half to even */
fn halved(p: i128, s: u32) -> i128 {
    if s == 0 { return p; }
    if s >= 128 { return 0; }
    let a = p.unsigned_abs();
    let (q, r, half) = (a >> s, a & ((1u128 << s) - 1), 1u128 << (s - 1));
    let q = (q + u128::from(r > half || (r == half && q & 1 == 1))) as i128;
    if p < 0 { -q } else { q }
}

/* `t` moved by `span` and refused past the years a time reads in */
fn moved<D: Dates>(t: Time, span: Option<i128>) -> Result<Time, VmErr> {
    let t = Time { nanos: span.and_then(|s| t.nanos.checked_add(s)).ok_or_else(far::<D>)?, offset: t.offset };
    if t.valid() { Ok(t) } else { Err(far::<D>()) }
}

/* a time past the years one reads in raised in the runtime's words */
#[cold]
fn far<D: Dates>() -> VmErr { bare::<D>(Fail::TimeRange) }

/* a duration past what one holds raised in the runtime's words */
#[cold]
fn long<D: Dates>() -> VmErr { bare::<D>(Fail::DurationRange) }

fn unop<D: Dates>(heap: &mut HeapPool, op: UnOp, v: Val) -> Result<Option<Val>, VmErr> {
    let Some(&Duration { nanos }) = held::<Duration>(heap, v) else { return Ok(None) };
    let nanos = match op {
        UnOp::Neg => nanos.checked_neg(),
        UnOp::Abs => nanos.checked_abs(),
        // a duration is immutable so plus gives the value itself
        UnOp::Pos => return Ok(Some(v)),
        UnOp::Invert => return Ok(None),
    };
    duration_val::<D>(heap, Duration { nanos: nanos.ok_or_else(long::<D>)? }).map(Some)
}

/* `n` onto `out` as a zigzag varint of seven bits a byte */
fn put(out: &mut Vec<u8>, n: i128) {
    let mut z = ((n << 1) ^ (n >> 127)) as u128;
    loop {
        let byte = (z & 0x7f) as u8;
        z >>= 7;
        if z == 0 { return out.push(byte); }
        out.push(byte | 0x80);
    }
}

/* the zigzag varint at the front of `b` and the bytes after it */
fn take(b: &[u8]) -> Option<(i128, &[u8])> {
    let mut z = 0u128;
    for (i, &byte) in b.iter().enumerate().take(19) {
        z |= u128::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 { return Some(((z >> 1) as i128 ^ -((z & 1) as i128), b.get(i + 1..)?)); }
    }
    None
}

fn save_time(p: &dyn Any, out: &mut Vec<u8>) {
    if let Some(t) = p.downcast_ref::<Time>() {
        put(out, t.nanos);
        put(out, i128::from(t.offset));
    }
}

/* a saved time checked as a parse checks one */
fn load_time(b: &[u8]) -> Option<Rc<dyn Any>> {
    let (nanos, b) = take(b)?;
    let (offset, b) = take(b)?;
    let t = Time { nanos, offset: i16::try_from(offset).ok()? };
    (b.is_empty() && t.valid()).then(|| Rc::new(t) as Rc<dyn Any>)
}

fn save_duration(p: &dyn Any, out: &mut Vec<u8>) {
    if let Some(d) = p.downcast_ref::<Duration>() { put(out, d.nanos); }
}

fn load_duration(b: &[u8]) -> Option<Rc<dyn Any>> {
    let (nanos, b) = take(b)?;
    b.is_empty().then(|| Rc::new(Duration { nanos }) as Rc<dyn Any>)
}
