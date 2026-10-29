use alloc::collections::BTreeMap;

use edge::lang::{Args, Cx, Ret, Sig};
use edge::value::{HeapObj, HeapPool, UnOp, Val, VmErr, fabs, hash_val_with_heap};

use super::big;
use crate::iter::items::range_len;
use crate::{Kernel, Site};

/* a hash by value for numbers text tuples and ranges and by identity otherwise */
pub const fn hash<S: Site, H: Scheme>() -> Kernel {
    assert!(H::BITS > 32 && H::BITS < 64, "a scheme hashes numbers modulo two to 33 to 63 bits less one");
    Kernel::new(Sig::exact(1), hash_run::<H>).fast(hash_fast::<H>)
}

/* the constants of a hash where equal numbers of any kind hash alike */
pub trait Scheme: 'static {
    // numbers hash by their value modulo two to this many bits less one
    const BITS: u32;
    // what nil hashes to whatever its bits
    const NIL: i64;
    // what positive infinity hashes to with negative infinity its negation
    const INF: i64;
    // the value no hash gives and what a hash that lands on it gives instead
    const RESERVED: Option<(i64, i64)>;
    // what a tuple hash that lands on the reserved value gives instead
    const RESERVED_TUPLE: i64;
    // the seed a tuple hash starts from and the primes each item mixes by
    const SEED: u64;
    const LANE: (u64, u64);
    // how far each item's mix turns and what a tuple's length mixes with
    const ROTATE: u32;
    const LENGTH: u64;
}

/* the modulus numbers hash by under scheme `H` */
#[inline(always)]
const fn prime<H: Scheme>() -> u64 { (1 << H::BITS) - 1 }

// the items a hash reads before it hashes again keeping each tuple's hash
const KEEP_AFTER: usize = 1 << 20;

/* the items a hash read and the tuple hashes it keeps at each depth */
#[derive(Default)]
struct Walk<'k> { read: usize, long: bool, kept: Option<&'k mut BTreeMap<(u64, usize), i64>> }

fn hash_run<H: Scheme>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let x = a.pos[0];
    if let Some(v) = hash_fast::<H>(cx.heap_mut(), x) { return Ok(v.into()); }
    cx.require_hashable(x)?;
    let mut walk = Walk::default();
    let mut h = hash_of::<H>(cx, x, 0, &mut walk)?;
    // a walk past its bound hashes again so a part shared many times hashes once
    if walk.long { h = hash_kept::<H>(cx, x)?; }
    // many items count what they hash as native work as a long text does
    else if walk.read > 64 { cx.spend(walk.read); }
    Ok(cx.new_int(i128::from(h))?.into())
}

/* the hash of `x` again keeping each tuple it reads at each depth */
#[cold]
#[inline(never)]
fn hash_kept<H: Scheme>(cx: &mut Cx<'_, '_>, x: Val) -> Result<i64, VmErr> {
    let mut walk = Walk { kept: Some(&mut BTreeMap::new()), ..Walk::default() };
    let h = hash_of::<H>(cx, x, 0, &mut walk)?;
    cx.spend(KEEP_AFTER.saturating_add(walk.read));
    Ok(h)
}

/* the hash of a number or of text which no check refuses */
fn hash_fast<H: Scheme>(heap: &mut HeapPool, x: Val) -> Option<Val> {
    let h = if x.is_int() { hash_int::<H>(i128::from(x.as_int())) }
        else if x.is_bool() { i64::from(x.as_bool()) }
        else if x.is_float() { hash_float::<H>(x) }
        else if x.is_none() { H::NIL }
        else if matches!(heap.try_get(x), Some(HeapObj::Str(_) | HeapObj::Bytes(_))) { content_hash::<H>(heap, x) }
        else { return None };
    heap.int(i128::from(h)).ok()
}

/* the hash of `v` at `depth` where a tuple mixes the hashes of its items */
fn hash_of<H: Scheme>(cx: &mut Cx<'_, '_>, v: Val, depth: usize, walk: &mut Walk<'_>) -> Result<i64, VmErr> {
    if depth > 200 { return Err(VmErr::CallDepth); }
    if v.is_bool() { return Ok(i64::from(v.as_bool())); }
    if v.is_none() { return Ok(H::NIL); }
    if v.is_float() { return Ok(hash_float::<H>(v)); }
    if v.is_int() { return Ok(hash_int::<H>(i128::from(v.as_int()))); }
    if v.is_foreign() {
        if let Some(n) = cx.int(v) { return Ok(hash_int::<H>(n)); }
        // a wide int past an i128 hashes by the scheme and not its kind
        if let Some((neg, m)) = big::modulo(cx, v, H::BITS) { return Ok(plain_hash::<H>(if neg { -(m as i64) } else { m as i64 })); }
        if let Some(h) = cx.heap().foreign_hash(v) { return Ok(h as i64); }
    }
    Ok(match cx.heap().try_get(v) {
        Some(HeapObj::Tuple(items)) => {
            let n = items.len();
            match &walk.kept {
                Some(kept) => match kept_hash(kept, v, depth) { Some(h) => return Ok(h), None => walk.read = walk.read.saturating_add(n) },
                // the count stays at most the bound so adding what memory holds never wraps
                None if n > KEEP_AFTER - walk.read => { (walk.read, walk.long) = (KEEP_AFTER, true); return Ok(0); }
                None => walk.read += n,
            }
            // each item is read afresh since hashing one may need the context
            let mut acc = H::SEED;
            for i in 0..n {
                let Some(item) = (match cx.heap().try_get(v) { Some(HeapObj::Tuple(t)) => t.get(i).copied(), _ => None }) else { break };
                acc = lane::<H>(acc, hash_of::<H>(cx, item, depth + 1, walk)?);
            }
            let h = lanes_done::<H>(acc, n);
            if let Some(kept) = &mut walk.kept { keep_hash(kept, v, depth, h); }
            h
        }
        // a range hashes as its length then the start and step its length shows
        Some(&HeapObj::Range(start, stop, step)) => {
            let len = range_len(start, stop, step);
            let start = if len == 0 { H::NIL } else { hash_int::<H>(i128::from(start)) };
            let step = if len <= 1 { H::NIL } else { hash_int::<H>(i128::from(step)) };
            tuple_hash::<H>(&[hash_int::<H>(i128::from(len)), start, step])
        }
        Some(&HeapObj::BigRange(start, stop, step)) => wide_range_hash::<H>(cx, start, stop, step)?,
        Some(HeapObj::Str(_) | HeapObj::Bytes(_)) | None => content_hash::<H>(cx.heap(), v),
        Some(_) => identity_hash::<H>(v),
    })
}

/* the hash a walk kept for tuple `v` at `depth` */
#[cold]
#[inline(never)]
fn kept_hash(kept: &BTreeMap<(u64, usize), i64>, v: Val, depth: usize) -> Option<i64> { kept.get(&(v.raw(), depth)).copied() }

/* keeps hash `h` of tuple `v` at `depth` for a walk that meets it again */
#[cold]
#[inline(never)]
fn keep_hash(kept: &mut BTreeMap<(u64, usize), i64>, v: Val, depth: usize, h: i64) { kept.insert((v.raw(), depth), h); }

/* a range past a word hashed through int arithmetic on its bounds */
fn wide_range_hash<H: Scheme>(cx: &mut Cx<'_, '_>, start: Val, stop: Val, step: Val) -> Result<i64, VmErr> {
    // the length rounds the gap over the step up with both positive under any rule
    let (lo, hi, by) = if cx.lt(Val::int(0), step)? { (start, stop, step) } else { (stop, start, negated(cx, step)?) };
    let back = negated(cx, lo)?;
    let gap = cx.add(hi, back)?;
    let len = if cx.lt(Val::int(0), gap)? {
        let over = cx.add(gap, by)?;
        let over = cx.add(over, Val::int(-1))?;
        cx.floor_div(over, by)?
    } else { Val::int(0) };
    let (empty, one) = (cx.int(len) == Some(0), cx.int(len).is_some_and(|n| n <= 1));
    let walk = &mut Walk::default();
    let start = if empty { H::NIL } else { hash_of::<H>(cx, start, 1, walk)? };
    let step = if one { H::NIL } else { hash_of::<H>(cx, step, 1, walk)? };
    let len = hash_of::<H>(cx, len, 1, walk)?;
    Ok(tuple_hash::<H>(&[len, start, step]))
}

/* the negation of an int inline or wide */
fn negated(cx: &mut Cx<'_, '_>, v: Val) -> Result<Val, VmErr> {
    if v.is_int() { return cx.new_int(-i128::from(v.as_int())); }
    cx.heap_mut().foreign_unop(UnOp::Neg, v)?.ok_or(VmErr::Type("a range bound that no negation reads"))
}

/* `n` modulo the prime keeping its sign with the reserved value left out */
fn hash_int<H: Scheme>(n: i128) -> i64 {
    let h = (n.unsigned_abs() % u128::from(prime::<H>())) as i64;
    plain_hash::<H>(if n < 0 { -h } else { h })
}

/* a float hashed as the int of the same value where nan hashes by identity */
fn hash_float<H: Scheme>(v: Val) -> i64 {
    let f = v.as_float();
    if f.is_nan() { return identity_hash::<H>(v); }
    if f.is_infinite() { return if f > 0.0 { H::INF } else { -H::INF }; }
    let (p, bits) = (prime::<H>(), H::BITS as i32);
    let (mut m, mut e) = frexp(fabs(f));
    let mut x: u64 = 0;
    // the mantissa feeds 28 bits at a time and turns within the prime's bits
    while m != 0.0 {
        x = ((x << 28) & p) | x >> (bits - 28);
        m *= 268_435_456.0;
        e -= 28;
        let y = m as u64;
        m -= y as f64;
        x += y;
        if x >= p { x -= p; }
    }
    let e = if e >= 0 { e % bits } else { bits - 1 - ((-1 - e) % bits) } as u32;
    x = ((x << e) & p) | x >> (H::BITS - e);
    plain_hash::<H>(if f < 0.0 { x.wrapping_neg() } else { x } as i64)
}

/* the mantissa in a half to one and the exponent of a positive float */
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 { return (0.0, 0); }
    let exp = ((x.to_bits() >> 52) & 0x7ff) as i32;
    if exp == 0 {
        let (m, e) = frexp(x * 18_014_398_509_481_984.0);
        return (m, e - 54);
    }
    (f64::from_bits((x.to_bits() & !(0x7ff << 52)) | (1022 << 52)), exp - 1022)
}

/* the hash of a tuple from the hashes of its items in order */
fn tuple_hash<H: Scheme>(lanes: &[i64]) -> i64 { lanes_done::<H>(lanes.iter().fold(H::SEED, |acc, &h| lane::<H>(acc, h)), lanes.len()) }

/* a tuple hash taking the hash of its next item */
fn lane<H: Scheme>(acc: u64, h: i64) -> u64 { acc.wrapping_add((h as u64).wrapping_mul(H::LANE.0)).rotate_left(H::ROTATE).wrapping_mul(H::LANE.1) }

/* a tuple hash closed with its length where the reserved value never comes out */
fn lanes_done<H: Scheme>(acc: u64, n: usize) -> i64 {
    let acc = acc.wrapping_add(n as u64 ^ H::LENGTH);
    match H::RESERVED { Some((r, _)) if acc as i64 == r => H::RESERVED_TUPLE, _ => acc as i64 }
}

/* the content hash the heap gives text and bytes and other values off the heap */
fn content_hash<H: Scheme>(heap: &HeapPool, v: Val) -> i64 { plain_hash::<H>(hash_val_with_heap(v, heap) as i64) }

/* the hash of an object compared by identity with its fixed low bits rotated away */
fn identity_hash<H: Scheme>(v: Val) -> i64 { plain_hash::<H>(v.raw().rotate_right(4) as i64) }

/* a hash with the scheme's reserved value moved to its stand in */
fn plain_hash<H: Scheme>(h: i64) -> i64 { match H::RESERVED { Some((r, to)) if h == r => to, _ => h } }

