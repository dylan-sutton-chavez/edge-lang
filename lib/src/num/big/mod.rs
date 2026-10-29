pub mod bigint;

use alloc::{borrow::Cow, rc::Rc, string::String, vec::Vec};
use core::any::Any;
use core::cmp::Ordering;

use edge::lang::Cx;
use edge::value::foreign::{BinOp, Kind, KindToken, Snapshot, Transfer, UnOp, words_int};
use edge::value::{HASH_BITS, HeapPool, Val, VmErr, as_i128, cold_heap, fpowf, hash_modulo};

use crate::Lang;
use crate::fail::{Fail, bare};

pub use bigint::BigInt;

/* the kind boxing every int too wide to stay inline under the runtime's name */
pub const fn kind<L: Lang>(type_name: &'static str) -> Kind {
    Kind {
        name: "wide.int",
        version: 1,
        type_name,
        family: "number",
        immutable: true,
        repr: |p| match small(p) { Some(i) => Ok(String::from(itoa::Buffer::new().format(i))), None => decimal::<L>(&held(p)) },
        // zero is always inline so every boxed int is true
        truthy: |_, _| true,
        hash: table_hash,
        eq: |heap, p, other| order::<L>(heap, p, other) == Some(Ordering::Equal),
        cmp: Some(order::<L>),
        binop: Some(binop::<L>),
        unop: Some(unop::<L>),
        to_f64: Some(|p| match small(p) { Some(i) => Ok(i as f64), None => held(p).to_f64().ok_or_else(|| bare::<L>(Fail::WideToFloat)) }),
        to_i128: Some(small),
        from_i128: Some(|heap, i| { let t = token_or_register::<L>(heap)?; heap.foreign_box_int(t, i) }),
        from_digits: Some(digits_val::<L>),
        trace: None,
        get: None,
        snapshot: Snapshot::Serialize { save: |p, out| held(p).encode(out), load: |b| BigInt::decode(b).map(|v| Rc::new(v) as Rc<dyn Any>) },
        transfer: Transfer::Copy,
        ..Kind::PLAIN
    }
}

/* the value of a payload with a narrow box read from its i128 */
fn held(p: &dyn Any) -> Cow<'_, BigInt> {
    match small(p) {
        Some(i) => Cow::Owned(BigInt::from_i128(i)),
        None => p.downcast_ref::<BigInt>().map_or_else(|| Cow::Owned(BigInt::zero()), Cow::Borrowed),
    }
}

/* the i128 a narrow box holds and None for limbs which never fit one */
fn small(p: &dyn Any) -> Option<i128> { p.downcast_ref::<[u64; 3]>().map(words_int) }

/* the hash engine tables give an equal number by the engine's own prime and rule */
fn table_hash(heap: &HeapPool, p: &dyn Any) -> u64 {
    let (neg, m) = match small(p) {
        Some(i) => (i < 0, (i.unsigned_abs() % ((1 << HASH_BITS) - 1)) as u64),
        None => { let b = held(p); long(heap, b.limbs()); (b.is_neg(), b.mersenne(HASH_BITS)) }
    };
    hash_modulo(neg, m) as u64
}

fn token<L: Lang>(heap: &HeapPool) -> Option<KindToken> { L::WIDE.and_then(|k| Cx::kind_token(heap, k)) }

/* the token of the language's wide kind where naming none holds no int */
fn token_or_register<L: Lang>(heap: &mut HeapPool) -> Result<KindToken, VmErr> {
    if let Some(t) = token::<L>(heap) { return Ok(t); }
    L::WIDE.ok_or_else(edge::value::cold_int_range).and_then(|k| heap.register_kind(k))
}

/* whether `v` is boxed by this kind */
fn ours<L: Lang>(heap: &HeapPool, v: Val) -> bool { L::WIDE.is_some_and(|w| heap.foreign_kind(v).is_some_and(|k| core::ptr::eq(k, w))) }

/* the int `v` holds borrowed from its box and none for anything else */
fn view<L: Lang>(heap: &HeapPool, v: Val) -> Option<Cow<'_, BigInt>> {
    if let Some(i) = as_i128(v, heap).filter(|_| !v.is_foreign() || ours::<L>(heap, v)) { return Some(Cow::Owned(BigInt::from_i128(i))); }
    heap.foreign_payload::<BigInt>(token::<L>(heap)?, v).map(Cow::Borrowed)
}

/* `b` as the narrowest value inline or in an i128 box when either fits */
fn narrow<L: Lang>(heap: &mut HeapPool, b: BigInt) -> Result<Val, VmErr> {
    match b.to_i128() {
        Some(i) => heap.int(i),
        None => { let t = token_or_register::<L>(heap)?; heap.foreign_box(t, Rc::new(b)) }
    }
}

/* refuses a result of `limbs` limbs before it is built and counts its work */
pub fn admit(heap: &HeapPool, limbs: u64, work: u64) -> Result<(), VmErr> {
    let work = usize::try_from(work).unwrap_or(usize::MAX);
    heap.afford(work)?;
    heap.reserve(usize::try_from(limbs.saturating_mul(4)).unwrap_or(usize::MAX))?;
    heap.spend(work);
    Ok(())
}

/* the work of an `a` by `b` limb product as schoolbook or karatsuba counts it */
fn mul_work(a: u64, b: u64) -> u64 {
    let (lo, hi) = (a.min(b), a.max(b));
    if lo < bigint::KARATSUBA as u64 { return lo.saturating_mul(hi) / 8; }
    (hi as f64 * fpowf(lo as f64, 0.585).unwrap_or(f64::INFINITY) / 8.0) as u64
}

/* counts the limbs a scan reads as native work once they are many */
#[inline]
fn long(heap: &HeapPool, limbs: usize) { if limbs > 16 { heap.spend(limbs); } }

/* the exact order of a payload against an int or a float */
fn order<L: Lang>(heap: &HeapPool, p: &dyn Any, other: Val) -> Option<Ordering> {
    if let Some(i) = small(p) && !other.is_float() && let Some(j) = as_i128(other, heap) { return Some(i.cmp(&j)); }
    let b = held(p);
    if other.is_float() { return b.cmp_f64(other.as_float()); }
    let o = view::<L>(heap, other)?;
    // two ints of one length read their limbs from the top until one differs
    if b.limbs() == o.limbs() { long(heap, b.limbs()); }
    Some((*b).cmp(&*o))
}

/* decimal text refused past the runtime's digit limit */
pub fn decimal<L: Lang>(b: &BigInt) -> Result<String, VmErr> {
    if let Some(limit) = L::DIGITS {
        // past this many bits the digits pass the limit so the slow conversion never starts
        if b.bit_length() > (limit as u64 * 3322 / 1000) + 64 { return Err(bare::<L>(Fail::Digits(None))); }
    }
    let s = b.to_radix(10);
    if L::DIGITS.is_some_and(|limit| s.len() > limit) { return Err(bare::<L>(Fail::Digits(None))); }
    Ok(if b.is_neg() { edge::s!("-", str &s) } else { s })
}

/* what an operation gives before boxing as an int or a float */
enum Out { Int(BigInt), Float(f64) }

fn binop<L: Lang>(heap: &mut HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
    match compute::<L>(heap, op, a, b)? {
        Some(Out::Int(r)) => narrow::<L>(heap, r).map(Some),
        Some(Out::Float(f)) => Ok(Some(Val::float(f))),
        None => Ok(None),
    }
}

/* `op` on the operands borrowed from their boxes and None when either is no int */
fn compute<L: Lang>(heap: &HeapPool, op: BinOp, a: Val, b: Val) -> Result<Option<Out>, VmErr> {
    let (Some(x), Some(y)) = (view::<L>(heap, a), view::<L>(heap, b)) else { return Ok(None) };
    let (lx, ly) = (x.limbs() as u64, y.limbs() as u64);
    let r = match op {
        BinOp::Add => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.add(&y) }
        BinOp::Sub => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.sub(&y) }
        BinOp::Mul => { admit(heap, lx + ly, mul_work(lx, ly))?; x.mul(&y) }
        BinOp::FloorDiv | BinOp::Mod => {
            admit(heap, lx + 1, lx * ly / 8)?;
            let Some((q, r)) = divmod_as(heap, &x, &y) else { return Err(VmErr::ZeroDiv) };
            if op == BinOp::Mod { r } else { q }
        }
        BinOp::TrueDiv => {
            if y.is_zero() { return Err(VmErr::ZeroDiv); }
            admit(heap, lx + ly + 3, lx * ly / 8)?;
            return x.true_div(&y).map(|f| Some(Out::Float(f))).ok_or_else(|| bare::<L>(Fail::WideDivFloat));
        }
        BinOp::Pow => return pow::<L>(heap, &x, &y).map(Some),
        BinOp::And => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.bitwise(&y, |p, q| p & q) }
        BinOp::Or => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.bitwise(&y, |p, q| p | q) }
        BinOp::Xor => { admit(heap, lx.max(ly) + 1, lx + ly)?; x.bitwise(&y, |p, q| p ^ q) }
        BinOp::Shl | BinOp::Shr => {
            if y.is_neg() { return Err(bare::<L>(Fail::NegativeShift)); }
            // a shift past what an index holds means too many digits
            let n = y.to_i128().and_then(|n| i64::try_from(n).ok()).map(|n| n as u64);
            match (op, n) {
                _ if x.is_zero() => BigInt::zero(),
                (BinOp::Shr, None) => BigInt::from_i128(if x.is_neg() { -1 } else { 0 }),
                (BinOp::Shr, Some(n)) => { if lx > 16 { admit(heap, lx.saturating_sub(n / 32) + 1, lx)?; } x.shr(n) }
                (_, None) => return Err(bare::<L>(Fail::ShiftDigits)),
                (_, Some(n)) => { admit(heap, lx + n / 32 + 1, lx + n / 32)?; x.shl(n) }
            }
        }
        // ints have no matrix product
        BinOp::MatMul => return Ok(None),
    };
    Ok(Some(Out::Int(r)))
}

/* `x ** y` which is a float for a negative exponent */
fn pow<L: Lang>(heap: &HeapPool, x: &BigInt, y: &BigInt) -> Result<Out, VmErr> {
    if y.is_neg() {
        let (fx, fy) = (x.to_f64(), y.to_f64());
        let fx = fx.ok_or_else(|| bare::<L>(Fail::WideToFloat))?;
        if fx == 0.0 { return Err(bare::<L>(Fail::ZeroNegativePower)); }
        let fy = fy.unwrap_or(f64::NEG_INFINITY);
        return Ok(Out::Float(fpowf(fx, fy).unwrap_or(0.0)));
    }
    match x.to_i128() {
        Some(0 | 1) => return Ok(Out::Int(x.clone())),
        Some(-1) => return Ok(Out::Int(BigInt::from_i128(if y.is_odd() { -1 } else { 1 }))),
        _ => {}
    }
    let e = y.to_i128().and_then(|e| u64::try_from(e).ok()).ok_or_else(cold_heap)?;
    // the result takes about e times the base's bits and its last squaring dominates
    let limbs = x.bit_length().saturating_mul(e) / 32 + 1;
    admit(heap, limbs, mul_work(limbs, limbs))?;
    Ok(Out::Int(x.pow(e)))
}

fn unop<L: Lang>(heap: &mut HeapPool, op: UnOp, v: Val) -> Result<Option<Val>, VmErr> {
    let Some(x) = view::<L>(heap, v) else { return Ok(None) };
    // every unary op copies the limbs and an inverse may carry past the top one
    let lx = x.limbs() as u64;
    if lx > 16 { admit(heap, lx + 1, lx)?; }
    let x = x.into_owned();
    let r = match op { UnOp::Neg => x.neg(), UnOp::Pos => x, UnOp::Abs => x.abs(), UnOp::Invert => x.invert() };
    narrow::<L>(heap, r).map(Some)
}

// the ints this kind boxes past what an inline value holds
pub(crate) type Wide = BigInt;

/* the wide int `v` boxes borrowed in place */
#[inline(always)]
fn wide<'h>(cx: &'h Cx<'_, '_>, v: Val) -> Option<&'h Wide> {
    let kind = cx.heap().foreign_kind(v)?;
    cx.heap().foreign_payload::<Wide>(Cx::kind_token(cx.heap(), kind)?, v)
}

/* the sign and magnitude modulo two to `bits` less one of an int past i128 */
pub(crate) fn modulo(cx: &Cx<'_, '_>, v: Val, bits: u32) -> Option<(bool, u64)> {
    let b = wide(cx, v)?;
    long(cx.heap(), b.limbs());
    Some((b.is_neg(), b.mersenne(bits)))
}

/* the int `v` holds of any width borrowed when boxed and None for anything else */
pub(crate) fn of<'h>(cx: &'h Cx<'_, '_>, v: Val) -> Option<Cow<'h, Wide>> {
    if let Some(i) = cx.int(v) { return Some(Cow::Owned(Wide::from_i128(i))); }
    wide(cx, v).map(Cow::Borrowed)
}

/* `b` as the narrowest int value the running heap boxes */
pub(crate) fn val<L: Lang>(cx: &mut Cx<'_, '_>, b: Wide) -> Result<Val, VmErr> { narrow::<L>(cx.heap_mut(), b) }

/* the int a literal's digits in `radix` spell refused past the memory left */
fn digits_val<L: Lang>(heap: &mut HeapPool, neg: bool, digits: &[u8], radix: u32) -> Result<Val, VmErr> {
    heap.reserve(digits.len() / 2 + 8)?;
    narrow::<L>(heap, BigInt::from_digits(neg, digits, radix))
}

/* an int's sign and digits in `radix` where a power of two has no limit */
pub fn radix_digits<L: Lang>(cx: &Cx<'_, '_>, v: Val, radix: u32) -> Option<Result<(bool, String), VmErr>> {
    let b = of(cx, v)?;
    if radix.is_power_of_two() {
        // the digits and their copy are refused before either is built
        let digits = b.bit_length() / u64::from(radix.trailing_zeros()) + 1;
        if digits > 256 && let Err(e) = admit(cx.heap(), digits / 2 + 1, digits) { return Some(Err(e)); }
        return Some(Ok((b.is_neg(), b.to_radix(radix))));
    }
    if let Err(e) = charge_text::<L>(cx, v) { return Some(Err(e)); }
    Some(decimal::<L>(&b.abs()).map(|s| (b.is_neg(), s)))
}

/* charges the decimal text of wide int `v` by its digits before the conversion starts */
pub fn charge_text<L: Lang>(cx: &Cx<'_, '_>, v: Val) -> Result<(), VmErr> {
    if !v.is_foreign() { return Ok(()); }
    // an int an i128 holds sits inline with no limbs and prints in one pass
    let heap = cx.heap();
    let Some(bits) = token::<L>(heap).and_then(|t| heap.foreign_payload::<Wide>(t, v)).map(BigInt::bit_length) else { return Ok(()) };
    // a value past the digit limit refuses before converting so it costs nothing
    if L::DIGITS.is_some_and(|limit| bits > limit as u64 * 3322 / 1000 + 64) { return Ok(()); }
    // the conversion divides every limb once per nine digits as reading them multiplies
    let digits = bits * 30103 / 100_000 + 1;
    let work = digits.saturating_mul(digits) / 64;
    // work this small is charged after it runs as any native work is
    if work < 1 << 16 { cx.spend(work as usize); return Ok(()); }
    admit(cx.heap(), digits / 4 + 1, work)
}

/* an int from digits checked for `radix` most significant first */
pub fn from_digits<L: Lang>(cx: &mut Cx<'_, '_>, neg: bool, digits: &[u8], radix: u32) -> Result<Val, VmErr> {
    if !radix.is_power_of_two() && L::DIGITS.is_some_and(|limit| digits.len() > limit) { return Err(bare::<L>(Fail::Digits(Some(digits.len())))); }
    admit(cx.heap(), digits.len() as u64 / 9 + 1, (digits.len() as u64).pow(2) / 64)?;
    val::<L>(cx, Wide::from_digits(neg, digits, radix))
}

/* the int a finite integral float holds */
pub fn from_f64<L: Lang>(cx: &mut Cx<'_, '_>, f: f64) -> Result<Val, VmErr> { val::<L>(cx, Wide::from_f64(f)) }

/* the quotient and remainder of wide ints rounded by the runtime's policy */
fn divmod_as(heap: &HeapPool, x: &Wide, y: &Wide) -> Option<(Wide, Wide)> {
    match heap.policy().int_mod {
        edge::lang::IntMod::Truncated => x.divmod_trunc(y),
        edge::lang::IntMod::Floored => x.divmod(y),
    }
}

/* modular power on ints of any width with none for a base with no inverse */
pub fn pow_mod<L: Lang>(cx: &mut Cx<'_, '_>, base: Val, exp: Val, modulus: Val) -> Option<Result<Option<Val>, VmErr>> {
    let truncated = matches!(cx.policy().int_mod, edge::lang::IntMod::Truncated);
    let r = { let (b, e, m) = (of(cx, base)?, of(cx, exp)?, of(cx, modulus)?); pow_mod_of(cx.heap(), &b, &e, &m, truncated) };
    Some(match r { Ok(Some(r)) => val::<L>(cx, r).map(Some), Ok(None) => Ok(None), Err(e) => Err(e) })
}

/* the power modulo a nonzero `m` signed by the remainder rule or none without inverse */
fn pow_mod_of(heap: &HeapPool, b: &Wide, e: &Wide, m: &Wide, truncated: bool) -> Result<Option<Wide>, VmErr> {
    let am = m.abs();
    admit(heap, am.limbs() as u64 * 2 + 2, (am.limbs() as u64).saturating_pow(2).saturating_mul(e.bit_length().max(1)) / 4)?;
    let modp = |x: &Wide| x.divmod(&am).map(|(_, r)| r).unwrap_or_default();
    let mut base = modp(b);
    if e.is_neg() { match inverse(&base, &am) { Some(inv) => base = inv, None => return Ok(None) } }
    let mut acc = modp(&Wide::from_i128(1));
    for i in (0..e.abs().bit_length()).rev() {
        acc = modp(&acc.mul(&acc));
        if e.bit(i) { acc = modp(&acc.mul(&base)); }
    }
    // a floored result takes the modulus's sign and a truncated one the power's
    let negative = if truncated { b.is_neg() && e.bit(0) } else { m.is_neg() };
    if negative && !acc.is_zero() { acc = acc.sub(&am); }
    Ok(Some(acc))
}

/* the inverse of `a` modulo `m` and None when they share a factor */
fn inverse(a: &Wide, m: &Wide) -> Option<Wide> {
    let (mut r0, mut r1) = (m.clone(), a.clone());
    let (mut t0, mut t1) = (Wide::zero(), Wide::from_i128(1));
    while !r1.is_zero() {
        let (q, r) = r0.divmod(&r1)?;
        (r0, r1) = (r1, r);
        let t = t0.sub(&q.mul(&t1));
        (t0, t1) = (t1, t);
    }
    (r0 == Wide::from_i128(1)).then(|| t0.divmod(m).map(|(_, r)| r)).flatten()
}

/* the bit length of any int read off its top limb */
pub fn bit_length(cx: &Cx<'_, '_>, v: Val) -> Option<u64> { of(cx, v).map(|b| b.bit_length()) }

/* the count of ones of any int paying for the limbs it reads */
pub fn bit_count(cx: &Cx<'_, '_>, v: Val) -> Option<u64> {
    let b = of(cx, v)?;
    if b.limbs() > 64 { cx.spend(b.limbs()); }
    Some(b.bit_count())
}

/* the bytes of an int in `len` bytes of either order signed or not */
pub fn to_bytes<L: Lang>(cx: &Cx<'_, '_>, v: Val, len: usize, big_endian: bool, signed: bool) -> Option<Result<Vec<u8>, VmErr>> {
    let b = of(cx, v)?;
    if b.is_neg() && !signed { return Some(Err(bare::<L>(Fail::NegativeUnsigned))); }
    Some(match b.to_bytes(len, signed) {
        Some(mut out) => { if big_endian { out.reverse(); } Ok(out) }
        None => Err(bare::<L>(Fail::TooBig)),
    })
}

/* the int bytes spell little-endian signed or not */
pub fn from_bytes_le<L: Lang>(cx: &mut Cx<'_, '_>, le: &[u8], signed: bool) -> Result<Val, VmErr> {
    admit(cx.heap(), le.len() as u64 / 4 + 2, le.len() as u64)?;
    val::<L>(cx, Wide::from_bytes_le(le, signed))
}

/* an int rounded to a multiple of 10^digits where `away` breaks ties outward */
pub fn round_tens<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, digits: u64, away: bool) -> Option<Result<Val, VmErr>> {
    let r = { let x = of(cx, v)?; round_tens_of(cx.heap(), &x, digits, away) };
    Some(r.and_then(|r| val::<L>(cx, r)))
}

fn round_tens_of(heap: &HeapPool, x: &Wide, digits: u64, away: bool) -> Result<Wide, VmErr> {
    // past the digits of `x` every rounding gives zero with no power built
    if digits as f64 * core::f64::consts::LOG2_10 > x.bit_length() as f64 + 2.0 { return Ok(Wide::zero()); }
    admit(heap, digits / 9 + x.limbs() as u64 + 2, digits.saturating_mul(x.limbs() as u64))?;
    let unit = Wide::from_i128(10).pow(digits);
    let Some((q, r)) = x.divmod(&unit) else { return Err(VmErr::ZeroDiv) };
    let twice = r.add(&r);
    let up = match twice.cmp(&unit) { Ordering::Greater => true, Ordering::Less => false, Ordering::Equal if away => !x.is_neg(), Ordering::Equal => q.is_odd() };
    let q = if up { q.add(&Wide::from_i128(1)) } else { q };
    Ok(q.mul(&unit))
}
