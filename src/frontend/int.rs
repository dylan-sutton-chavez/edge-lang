/* `python.bigint`, the FOREIGN kind that holds every int too wide for an inline Val. A value that fits 48 bits
   is never boxed, one that fits i128 sits inline in its entry and only a wider one is boxed as limbs, so each
   int has one representation and the common wide int allocates nothing. */
use alloc::{borrow::Cow, rc::Rc, string::String, vec::Vec};
use core::any::Any;
use core::cmp::Ordering;

use super::bigint::BigInt;
use crate::value::foreign::{BinOp, Kind, KindToken, Snapshot, Transfer, UnOp};
use crate::value::{HeapObj, HeapPool, Val, VmErr, as_i128, cold_heap, cold_value};

// Decimal text past this many digits raises, so a conversion never costs quadratic time on huge ints.
pub const MAX_STR_DIGITS: usize = 4300;

pub static KIND: Kind = Kind {
    name: "python.bigint",
    version: 1,
    type_name: "int",
    family: "python.number",
    immutable: true,
    repr: |p| match small(p) { Some(i) => Ok(String::from(itoa::Buffer::new().format(i))), None => decimal(&of(p)) },
    // Zero is always inline, so every boxed int is true.
    truthy: |_| true,
    hash: |p| match small(p) { Some(i) => crate::value::eq::hash_int(i) as u64, None => of(p).hash_mod() as u64 },
    eq: |heap, p, other| order(heap, p, other) == Some(Ordering::Equal),
    cmp: Some(order),
    binop: Some(binop),
    unop: Some(unop),
    to_f64: Some(|p| match small(p) { Some(i) => Ok(i as f64), None => of(p).to_f64().ok_or_else(|| raised("OverflowError: int too large to convert to float")) }),
    to_i128: Some(small),
    from_i128: Some(|heap, i| { let t = token_or_register(heap); heap.foreign_box_int(t, i) }),
    trace: None,
    snapshot: Snapshot::Serialize { save: |p, out| of(p).encode(out), load: |b| BigInt::decode(b).map(|v| match v.to_i128() { Some(i) => Rc::new(i) as Rc<dyn Any>, None => Rc::new(v) }) },
    transfer: Transfer::Copy,
};

/* The value of a payload, built from the i128 a narrow box holds. */
fn of(p: &dyn Any) -> Cow<'_, BigInt> {
    match small(p) { Some(i) => Cow::Owned(BigInt::from_i128(i)), None => Cow::Borrowed(p.downcast_ref::<BigInt>().expect("python.bigint payload")) }
}

/* The i128 a narrow box holds inline, None for limbs, which never fit one. */
fn small(p: &dyn Any) -> Option<i128> { p.downcast_ref::<crate::value::foreign::Inline>().map(|n| n.get()) }

fn raised(m: &str) -> VmErr { VmErr::Raised(m.into()) }

fn token(heap: &HeapPool) -> Option<KindToken> { heap.foreign.token_of(&KIND) }

/* The int `v` holds, inline, bool or boxed, None for anything else. */
pub fn of_val(heap: &HeapPool, v: Val) -> Option<BigInt> { view(heap, v).map(Cow::into_owned) }

/* The int `v` holds, borrowed from its box when it has limbs, None for anything else. */
fn view(heap: &HeapPool, v: Val) -> Option<Cow<'_, BigInt>> {
    if let Some(i) = as_i128(v, heap).filter(|_| !v.is_foreign() || is_big(heap, v)) { return Some(Cow::Owned(BigInt::from_i128(i))); }
    heap.foreign_payload::<BigInt>(token(heap)?, v).map(Cow::Borrowed)
}

/* Whether `v` is a boxed int. */
pub fn is_big(heap: &HeapPool, v: Val) -> bool { heap.foreign_kind(v).is_some_and(|k| core::ptr::eq(k, &KIND)) }

/* `b` as the narrowest Val, inline when it fits 48 bits and an i128 box when it fits that. */
pub fn val(heap: &mut HeapPool, b: BigInt) -> Result<Val, VmErr> {
    match b.to_i128() { Some(i) => heap.int(i), None => boxed(heap, Rc::new(b)) }
}

fn boxed(heap: &mut HeapPool, payload: Rc<dyn Any>) -> Result<Val, VmErr> {
    let t = token_or_register(heap);
    heap.foreign_box(t, payload)
}

fn token_or_register(heap: &mut HeapPool) -> KindToken { match token(heap) { Some(t) => t, None => heap.register_kind(&KIND) } }

/* Refuses a result of `limbs` limbs before it is built, and counts the work that builds it. */
fn admit(heap: &HeapPool, limbs: u64, work: u64) -> Result<(), VmErr> {
    let work = usize::try_from(work).unwrap_or(usize::MAX);
    heap.afford(work)?;
    heap.reserve(usize::try_from(limbs.saturating_mul(4)).unwrap_or(usize::MAX))?;
    heap.spend(work);
    Ok(())
}

/* The work of multiplying `a` by `b` limbs, schoolbook below the Karatsuba threshold and n^1.585 above it. */
fn mul_work(a: u64, b: u64) -> u64 {
    let (lo, hi) = (a.min(b), a.max(b));
    if lo < super::bigint::KARATSUBA as u64 { return lo.saturating_mul(hi) / 8; }
    (hi as f64 * libm::pow(lo as f64, 0.585) / 8.0) as u64
}

/* The order of a payload against an int or a float, exact for both, in i128 when both fit one. */
fn order(heap: &HeapPool, p: &dyn Any, other: Val) -> Option<Ordering> {
    if let Some(i) = small(p) && !other.is_float() && let Some(j) = as_i128(other, heap) { return Some(i.cmp(&j)); }
    let b = of(p);
    if other.is_float() { return b.cmp_f64(other.as_float()); }
    Some((*b).cmp(&*view(heap, other)?))
}

/* Decimal text, refused past `MAX_STR_DIGITS` digits. */
pub fn decimal(b: &BigInt) -> Result<String, VmErr> {
    // Past this many bits the digits surely exceed the limit, so the slow conversion never starts.
    if b.bit_length() > (MAX_STR_DIGITS as u64 * 3322 / 1000) + 64 { return Err(digits_err(None)); }
    let s = b.to_radix(10);
    if s.len() > MAX_STR_DIGITS { return Err(digits_err(None)); }
    Ok(if b.is_neg() { crate::s!("-", str &s) } else { s })
}

pub fn digits_err(count: Option<usize>) -> VmErr {
    match count {
        Some(n) => raised(&crate::s!("ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has ", int n as i64, " digits; use sys.set_int_max_str_digits() to increase the limit")),
        None => raised("ValueError: Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit"),
    }
}

/* The magnitude digits of an int in `radix` and its sign, the decimal limit applied to radixes not a power of two. */
pub fn radix_digits(heap: &HeapPool, v: Val, radix: u32) -> Option<Result<(bool, String), VmErr>> {
    let b = of_val(heap, v)?;
    if radix.is_power_of_two() { return Some(Ok((b.is_neg(), b.to_radix(radix)))); }
    Some(decimal(&b.abs()).map(|s| (b.is_neg(), s)))
}

/* An int from digits already checked for `radix`, most significant first. */
pub fn from_digits(heap: &mut HeapPool, neg: bool, digits: &[u8], radix: u32) -> Result<Val, VmErr> {
    if !radix.is_power_of_two() && digits.len() > MAX_STR_DIGITS { return Err(digits_err(Some(digits.len()))); }
    admit(heap, digits.len() as u64 / 9 + 1, (digits.len() as u64).pow(2) / 64)?;
    val(heap, BigInt::from_digits(neg, digits, radix))
}

/* The int a finite integral float holds. */
pub fn from_f64(heap: &mut HeapPool, f: f64) -> Result<Val, VmErr> { val(heap, BigInt::from_f64(f)) }

/* What an operation gives before it is boxed, an int or the float of a true division or a negative power. */
enum Out { Int(BigInt), Float(f64) }

fn binop(heap: &mut HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
    match compute(heap, op, a, b)? {
        Some(Out::Int(r)) => val(heap, r).map(Some),
        Some(Out::Float(f)) => Ok(Some(Val::float(f))),
        None => Ok(None),
    }
}

/* `op` on the operands borrowed from their boxes, None when either is not an int. */
fn compute(heap: &HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Out>, VmErr> {
    let (Some(x), Some(y)) = (view(heap, a), view(heap, b)) else { return Ok(None) };
    let (lx, ly) = (x.limbs() as u64, y.limbs() as u64);
    let r = match op {
        BinOp::Add => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.add(&y) }
        BinOp::Sub => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.sub(&y) }
        BinOp::Mul => { admit(heap, lx + ly, mul_work(lx, ly))?; x.mul(&y) }
        BinOp::FloorDiv | BinOp::Mod => {
            admit(heap, lx + 1, lx * ly / 8)?;
            let Some((q, r)) = x.divmod(&y) else { return Err(raised(if op == BinOp::Mod { "ZeroDivisionError: integer modulo by zero" } else { "ZeroDivisionError: integer division or modulo by zero" })) };
            if op == BinOp::Mod { r } else { q }
        }
        BinOp::TrueDiv => {
            if y.is_zero() { return Err(VmErr::ZeroDiv); }
            admit(heap, lx + ly + 3, lx * ly / 8)?;
            return x.true_div(&y).map(|f| Some(Out::Float(f))).ok_or_else(|| raised("OverflowError: integer division result too large for a float"));
        }
        BinOp::Pow => return pow(heap, &x, &y).map(Some),
        BinOp::And => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.bitwise(&y, |p, q| p & q) }
        BinOp::Or => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.bitwise(&y, |p, q| p | q) }
        BinOp::Xor => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.bitwise(&y, |p, q| p ^ q) }
        BinOp::Shl | BinOp::Shr => {
            if y.is_neg() { return Err(cold_value("negative shift count")); }
            let n = y.to_i128().and_then(|n| u64::try_from(n).ok());
            match (op, n) {
                _ if x.is_zero() => BigInt::zero(),
                (BinOp::Shr, None) => BigInt::from_i128(if x.is_neg() { -1 } else { 0 }),
                (BinOp::Shr, Some(n)) => x.shr(n),
                (_, None) => return Err(cold_heap()),
                (_, Some(n)) => { admit(heap, lx + n / 32 + 1, lx + n / 32)?; x.shl(n) }
            }
        }
    };
    Ok(Some(Out::Int(r)))
}

/* `x ** y`, a float for a negative exponent and the bases that never grow answered without work. */
fn pow(heap: &HeapPool, x: &BigInt, y: &BigInt) -> Result<Out, VmErr> {
    if y.is_neg() {
        let (fx, fy) = (x.to_f64(), y.to_f64());
        let fx = fx.ok_or_else(|| raised("OverflowError: int too large to convert to float"))?;
        if fx == 0.0 { return Err(raised("ZeroDivisionError: 0.0 cannot be raised to a negative power")); }
        return Ok(Out::Float(libm::pow(fx, fy.unwrap_or(f64::NEG_INFINITY))));
    }
    match x.to_i128() {
        Some(0 | 1) => return Ok(Out::Int(x.clone())),
        Some(-1) => return Ok(Out::Int(BigInt::from_i128(if y.is_odd() { -1 } else { 1 }))),
        _ => {}
    }
    let e = y.to_i128().and_then(|e| u64::try_from(e).ok()).ok_or_else(cold_heap)?;
    // The result takes about e times the bits of the base, refused before it is built.
    // The last squaring dominates the work, so it is counted at the size of the result.
    let limbs = x.bit_length().saturating_mul(e) / 32 + 1;
    admit(heap, limbs, mul_work(limbs, limbs))?;
    Ok(Out::Int(x.pow(e)))
}

fn unop(heap: &mut HeapPool, op: UnOp, v: Val) -> Result<Option<Val>, VmErr> {
    let Some(x) = of_val(heap, v) else { return Ok(None) };
    let r = match op { UnOp::Neg => x.neg(), UnOp::Pos => x, UnOp::Abs => x.abs(), UnOp::Invert => x.invert() };
    val(heap, r).map(Some)
}

/* `pow(base, exp, modulus)` on ints of any size, a negative exponent raising the inverse of the base. */
pub fn pow_mod(heap: &mut HeapPool, base: Val, exp: Val, modulus: Val) -> Option<Result<Val, VmErr>> {
    let (b, e, m) = (of_val(heap, base)?, of_val(heap, exp)?, of_val(heap, modulus)?);
    Some((|| {
        if m.is_zero() { return Err(cold_value("pow() 3rd argument cannot be 0")); }
        let am = m.abs();
        admit(heap, am.limbs() as u64 * 2 + 2, (am.limbs() as u64).saturating_pow(2).saturating_mul(e.bit_length().max(1)) / 4)?;
        let modp = |x: &BigInt| x.divmod(&am).map(|(_, r)| r).unwrap_or_default();
        let mut base = modp(&b);
        if e.is_neg() { base = inverse(&base, &am).ok_or_else(|| cold_value("base is not invertible for the given modulus"))?; }
        let mut acc = modp(&BigInt::from_i128(1));
        for i in (0..e.abs().bit_length()).rev() {
            acc = modp(&acc.mul(&acc));
            if e.bit(i) { acc = modp(&acc.mul(&base)); }
        }
        // The result takes the sign of the modulus.
        if m.is_neg() && !acc.is_zero() { acc = acc.sub(&am); }
        val(heap, acc)
    })())
}

/* The inverse of `a` modulo `m`, None when they share a factor. */
fn inverse(a: &BigInt, m: &BigInt) -> Option<BigInt> {
    let (mut r0, mut r1) = (m.clone(), a.clone());
    let (mut t0, mut t1) = (BigInt::zero(), BigInt::from_i128(1));
    while !r1.is_zero() {
        let (q, r) = r0.divmod(&r1)?;
        (r0, r1) = (r1, r);
        let t = t0.sub(&q.mul(&t1));
        (t0, t1) = (t1, t);
    }
    (r0 == BigInt::from_i128(1)).then(|| t0.divmod(m).map(|(_, r)| r)).flatten()
}

/* `int.bit_length()` and `int.bit_count()` of any int. */
pub fn bits(heap: &HeapPool, v: Val) -> Option<(u64, u64)> { of_val(heap, v).map(|b| (b.bit_length(), b.abs().bit_count())) }

/* `int.to_bytes(length, byteorder, signed)`. */
pub fn to_bytes(heap: &HeapPool, v: Val, len: usize, big_endian: bool, signed: bool) -> Option<Result<Vec<u8>, VmErr>> {
    let b = of_val(heap, v)?;
    if b.is_neg() && !signed { return Some(Err(raised("OverflowError: can't convert negative int to unsigned"))); }
    Some(match b.to_bytes(len, signed) {
        Some(mut out) => { if big_endian { out.reverse(); } Ok(out) }
        None => Err(raised("OverflowError: int too big to convert")),
    })
}

/* `int.from_bytes(bytes, byteorder, signed)`. */
pub fn from_bytes(heap: &mut HeapPool, bytes: &[u8], big_endian: bool, signed: bool) -> Result<Val, VmErr> {
    let mut le = bytes.to_vec();
    if big_endian { le.reverse(); }
    admit(heap, le.len() as u64 / 4 + 2, le.len() as u64)?;
    val(heap, BigInt::from_bytes_le(&le, signed))
}

/* `round(x, ndigits)` for a negative `ndigits`, half to even at the dropped digits. */
pub fn round_tens(heap: &mut HeapPool, v: Val, digits: u64) -> Option<Result<Val, VmErr>> {
    let x = of_val(heap, v)?;
    Some((|| {
        // Past the digits of `x` every rounding gives zero, so no power of ten is built.
        if digits as f64 * core::f64::consts::LOG2_10 > x.bit_length() as f64 + 2.0 { return val(heap, BigInt::zero()); }
        admit(heap, digits / 9 + x.limbs() as u64 + 2, digits.saturating_mul(x.limbs() as u64))?;
        let unit = BigInt::from_i128(10).pow(digits);
        let Some((q, r)) = x.divmod(&unit) else { return Err(VmErr::ZeroDiv) };
        let twice = r.add(&r);
        let up = match twice.cmp(&unit) { Ordering::Greater => true, Ordering::Less => false, Ordering::Equal => q.is_odd() };
        let q = if up { q.add(&BigInt::from_i128(1)) } else { q };
        val(heap, q.mul(&unit))
    })())
}

/* The start, stop and step of a range whose bounds leave i64, as ints. */
fn range_parts(heap: &HeapPool, v: Val) -> Option<(BigInt, BigInt, BigInt)> {
    let &HeapObj::BigRange(s, e, st) = heap.try_get(v)? else { return None };
    Some((of_val(heap, s)?, of_val(heap, e)?, of_val(heap, st)?))
}

/* The item count of a range from its parts. */
fn span(s: &BigInt, e: &BigInt, st: &BigInt) -> BigInt {
    let one = BigInt::from_i128(1);
    let (lo, hi, step) = if st.is_neg() { (e, s, st.neg()) } else { (s, e, st.clone()) };
    if hi <= lo { return BigInt::zero(); }
    hi.sub(lo).sub(&one).divmod(&step).map_or_else(BigInt::zero, |(q, _)| q.add(&one))
}

/* `range(start, stop, step)` when a bound leaves i64, None when every bound fits one. */
pub fn new_range(heap: &mut HeapPool, s: Val, e: Val, st: Val) -> Option<Result<Val, VmErr>> {
    let fits = |v: Val| as_i128(v, heap).is_some_and(|i| i64::try_from(i).is_ok());
    let ints = [s, e, st].iter().all(|&v| v.is_int() || v.is_bool() || is_big(heap, v));
    if !ints || [s, e, st].iter().all(|&v| fits(v)) { return None; }
    if of_val(heap, st).is_some_and(|b| b.is_zero()) { return Some(Err(cold_value("range() arg 3 must not be zero"))); }
    let norm = |v: Val| if v.is_bool() { Val::int(v.as_bool() as i64) } else { v };
    Some(heap.alloc(HeapObj::BigRange(norm(s), norm(e), norm(st))))
}

/* The length of a wide range, an error past what `len` can hold. */
pub fn range_len(heap: &HeapPool, v: Val) -> Option<Result<i128, VmErr>> {
    let (s, e, st) = range_parts(heap, v)?;
    Some(span(&s, &e, &st).to_i128().filter(|&n| n <= i64::MAX as i128).ok_or(VmErr::Overflow))
}

/* Whether a wide range has no items. */
pub fn range_empty(heap: &HeapPool, v: Val) -> Option<bool> { range_parts(heap, v).map(|(s, e, st)| span(&s, &e, &st).is_zero()) }

/* Item `i` of a wide range, an IndexError past either end. */
pub fn range_item(heap: &mut HeapPool, v: Val, i: Val) -> Option<Result<Val, VmErr>> {
    let (s, e, st) = range_parts(heap, v)?;
    let (n, i) = (span(&s, &e, &st), of_val(heap, i)?);
    let i = if i.is_neg() { i.add(&n) } else { i };
    if i.is_neg() || i >= n { return Some(Err(VmErr::Raised("IndexError: range object index out of range".into()))); }
    Some(val(heap, s.add(&i.mul(&st))))
}

/* Where `x` sits in a wide range, a whole float counting as its int, None outside or for a range that is not wide. */
fn range_pos(heap: &HeapPool, v: Val, x: Val) -> Option<Option<BigInt>> {
    let (s, e, st) = range_parts(heap, v)?;
    let x = if x.is_float() && x.as_float().is_finite() && libm::trunc(x.as_float()) == x.as_float() { BigInt::from_f64(x.as_float()) } else { match of_val(heap, x) { Some(x) => x, None => return Some(None) } };
    let inside = if st.is_neg() { e < x && x <= s } else { s <= x && x < e };
    Some(x.sub(&s).divmod(&st).filter(|(_, r)| inside && r.is_zero()).map(|(q, _)| q))
}

/* Whether `x` is an item of a wide range. */
pub fn range_has(heap: &HeapPool, v: Val, x: Val) -> Option<bool> { range_pos(heap, v, x).map(|p| p.is_some()) }

/* The index of `x` in a wide range, None when it is not an item. */
pub fn range_index(heap: &mut HeapPool, v: Val, x: Val) -> Option<Result<Option<Val>, VmErr>> {
    let p = range_pos(heap, v, x)?;
    Some(p.map(|i| val(heap, i)).transpose())
}

/* The text of a wide range, `range(start, stop)` with the step only when it is not one. */
pub fn range_repr(heap: &HeapPool, v: Val) -> Option<Result<String, VmErr>> {
    let (s, e, st) = range_parts(heap, v)?;
    Some((|| {
        let tail = if st == BigInt::from_i128(1) { String::new() } else { crate::s!(", ", str &decimal(&st)?) };
        Ok(crate::s!("range(", str &decimal(&s)?, ", ", str &decimal(&e)?, str &tail, ")"))
    })())
}

/* Whether two ranges, either wide, hold the same items. */
pub fn range_eq(heap: &HeapPool, a: Val, b: Val) -> Option<bool> {
    let parts = |v: Val| match heap.try_get(v)? {
        &HeapObj::Range(s, e, st) => Some((BigInt::from_i128(s as i128), BigInt::from_i128(e as i128), BigInt::from_i128(st as i128))),
        _ => range_parts(heap, v),
    };
    let ((s1, e1, t1), (s2, e2, t2)) = (parts(a)?, parts(b)?);
    let (l1, l2) = (span(&s1, &e1, &t1), span(&s2, &e2, &t2));
    let one = BigInt::from_i128(1);
    Some(l1 == l2 && (l1.is_zero() || (s1 == s2 && (l1 == one || t1 == t2))))
}

/* The hashes of the length, start and step of a wide range, `none` for a part its length leaves out. */
pub fn range_hashes(heap: &HeapPool, v: Val, none: i64) -> Option<[i64; 3]> {
    let (s, e, st) = range_parts(heap, v)?;
    let n = span(&s, &e, &st);
    let one = BigInt::from_i128(1);
    Some([n.hash_mod(), if n.is_zero() { none } else { s.hash_mod() }, if n <= one { none } else { st.hash_mod() }])
}

/* The cursor of a wide range walked backward, its last item, the bound before its first and the negated step. */
pub fn range_reversed(heap: &mut HeapPool, v: Val) -> Option<Result<(Val, Val, Val), VmErr>> {
    let (s, e, st) = range_parts(heap, v)?;
    let n = span(&s, &e, &st);
    let last = s.add(&n.sub(&BigInt::from_i128(1)).mul(&st));
    Some((|| Ok((val(heap, last)?, val(heap, s.sub(&st))?, val(heap, st.neg())?)))())
}

/* `v[start:stop:step]` for a range of any width, bounds clamped like `slice.indices`, a plain range when it fits i64. */
pub fn range_slice(heap: &mut HeapPool, v: Val, start: Val, stop: Val, step: Val) -> Option<Result<Val, VmErr>> {
    let (s, e, st) = match *heap.try_get(v)? {
        HeapObj::Range(a, b, c) => (BigInt::from_i128(a as i128), BigInt::from_i128(b as i128), BigInt::from_i128(c as i128)),
        _ => range_parts(heap, v)?,
    };
    Some((|| {
        let n = span(&s, &e, &st);
        let index = |x: Val| match of_val(heap, x) { Some(i) => Ok(i), None => Err(VmErr::Type("slice indices must be integers or None or have an __index__ method")) };
        let c = if step.is_none() { BigInt::from_i128(1) } else { index(step)? };
        if c.is_zero() { return Err(cold_value("slice step cannot be zero")); }
        let (lower, upper) = if c.is_neg() { (BigInt::from_i128(-1), n.sub(&BigInt::from_i128(1))) } else { (BigInt::zero(), n.clone()) };
        let bound = |x: Val, default: &BigInt| -> Result<BigInt, VmErr> {
            if x.is_none() { return Ok(default.clone()); }
            let i = index(x)?;
            Ok(if i.is_neg() { let j = i.add(&n); if j < lower { lower.clone() } else { j } } else if i > upper { upper.clone() } else { i })
        };
        let (a, b) = if c.is_neg() { (bound(start, &upper)?, bound(stop, &lower)?) } else { (bound(start, &lower)?, bound(stop, &upper)?) };
        let (lo, hi, by) = (s.add(&a.mul(&st)), s.add(&b.mul(&st)), st.mul(&c));
        let (x, y, z) = (val(heap, lo)?, val(heap, hi)?, val(heap, by)?);
        match new_range(heap, x, y, z) {
            Some(r) => r,
            // Every bound fits i64 here, some of them boxed past the inline range.
            None => { let i = |v: Val| as_i128(v, heap).unwrap_or(0) as i64; let r = HeapObj::Range(i(x), i(y), i(z)); heap.alloc(r) }
        }
    })())
}

/* A slice index with a boxed int clamped to the inline extreme of its sign, which no sequence length reaches. */
pub fn clip_index(heap: &HeapPool, v: Val) -> Val {
    if !is_big(heap, v) { return v; }
    Val::int(if view(heap, v).is_some_and(|b| b.is_neg()) { Val::INT_MIN } else { Val::INT_MAX })
}
