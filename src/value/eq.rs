
use super::{HeapObj, HeapPool, Val, ValSet, View, VmErr, as_i128};

pub(crate) fn eq_seq(a: &[Val], b: &[Val], mut eq: impl FnMut(Val,Val)->bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x,y)| eq(*x,*y))
}
/* Content set-equality, same size and every item of `a` found in `b` under its stored hash. */
pub(crate) fn eq_set(a: &ValSet, b: &ValSet, mut eq: impl FnMut(Val,Val)->bool) -> bool {
    a.len() == b.len() && a.iter_hashed().all(|(h, x)| b.slot(h, |y| eq(x, y)).is_some())
}

/* Recursion cap so self-referential containers fall back instead of overflowing the stack. */
pub(crate) const EQ_DEPTH_MAX: usize = 100;

pub fn eq_vals_with_heap(a: Val, b: Val, heap: &HeapPool) -> bool {
    eq_vals_depth(a, b, heap, 0, &mut false)
}

/* Equality of a container member, identity first as in `x is e or x == e`. */
#[inline]
pub fn eq_member(a: Val, b: Val, heap: &HeapPool) -> bool {
    if a.0 == b.0 { return true; }
    // Two strings, the common dict and set key, compare their text straight away.
    if let (Some(HeapObj::Str(x)), Some(HeapObj::Str(y))) = (heap.try_get(a), heap.try_get(b)) { return eq_text(x.as_bytes(), y.as_bytes(), heap); }
    eq_vals_depth(a, b, heap, 0, &mut false)
}

/* Text or bytes equality, a long pair counting the bytes it reads as native work. */
#[inline]
fn eq_text(x: &[u8], y: &[u8], heap: &HeapPool) -> bool {
    if x.len() == y.len() && x.len() > 64 { heap.spend(x.len()); }
    x == y
}

/* Content equality from `depth` levels in, None when only a user `__eq__` or the cap can settle it. */
#[inline]
pub fn eq_checked(a: Val, b: Val, heap: &HeapPool, depth: usize) -> Option<bool> {
    let mut rich = false;
    let r = eq_vals_depth(a, b, heap, depth, &mut rich);
    if !r && rich { None } else { Some(r) }
}

/* The class or a base defines `__eq__`. */
fn class_defines_eq(cls: Val, heap: &HeapPool, depth: usize) -> bool {
    if depth > EQ_DEPTH_MAX { return false; }
    let Some(HeapObj::Class(_, bases, methods)) = heap.try_get(cls) else { return false };
    methods.borrow().iter().any(|(n, _)| n == "__eq__") || bases.iter().any(|&b| class_defines_eq(b, heap, depth + 1))
}

/* Tuple hash from its item hashes. */
fn hash_tuple_parts(parts: &[u64]) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
    h.write_u8(3);
    h.write_usize(parts.len());
    for &p in parts { h.write_u64(p); }
    h.finish()
}

/* Order-free set hash from its item hashes, so equal frozensets hash equal. */
fn hash_set_parts(parts: impl Iterator<Item = u64>) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
    h.write_u8(4);
    h.write_u64(parts.fold(0u64, |a, p| a.wrapping_add(p)));
    h.finish()
}

/* Content hash, consistent with eq_vals_with_heap. Values that compare equal hash equal (numeric unified). */
pub fn hash_val_with_heap(v: Val, heap: &HeapPool) -> u64 {
    hash_depth(v, heap, 0)
}

/* The hash a string Val holds, taken from the text alone. */
pub fn hash_str(s: &str) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
    h.write_u8(1);
    h.write(s.as_bytes());
    h.finish()
}

/* The hash a table keyed by text probes with, folded so similar texts spread. */
#[inline]
pub fn hash_key(s: &str) -> u64 {
    let h = hash_str(s);
    (h ^ h >> 32).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}
fn hash_depth(v: Val, heap: &HeapPool, depth: usize) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
    // Every number hashes by its value modulo the prime, so equal ints, bools, floats and wide ints probe alike.
    if v.is_int() { h.write_i64(hash_small(v.as_int())); return h.finish(); }
    if v.is_bool() { h.write_i64(v.as_bool() as i64); return h.finish(); }
    if v.is_float() {
        let f = v.as_float();
        if f.is_nan() { h.write_u64(f.to_bits()); } else { h.write_i64(hash_float(f)); }
        return h.finish();
    }
    if let Some(k) = heap.foreign_hash(v) { h.write_u64(k); return h.finish(); }
    if !v.is_heap() || depth > EQ_DEPTH_MAX { h.write_u64(v.0); return h.finish(); }
    match heap.get(v) {
        // A long text and every tuple count what they hash as native work.
        HeapObj::Str(s) => { if s.len() > 64 { heap.spend(s.len()); } h.write_u8(1); h.write(s.as_bytes()); }
        HeapObj::Bytes(b) => { if b.len() > 64 { heap.spend(b.len()); } h.write_u8(2); h.write(b); }
        HeapObj::Tuple(t) => { heap.spend(t.len()); return hash_tuple_parts(&t.iter().map(|&e| hash_depth(e, heap, depth + 1)).collect::<alloc::vec::Vec<_>>()) }
        // A frozenset sums the hashes it stored, and a set probes like the frozenset it equals.
        HeapObj::FrozenSet(s) => return hash_set_parts(s.iter_hashed().map(|(h, _)| h)),
        HeapObj::Set(s) => return hash_set_parts(s.borrow().iter_hashed().map(|(h, _)| h)),
        HeapObj::GenericAlias(o, a) => { h.write_u8(5); h.write_u64(hash_depth(*o, heap, depth + 1)); h.write_u64(hash_depth(*a, heap, depth + 1)); }
        // Order-independent, `int | str` and `str | int` are one union.
        HeapObj::Union(a) => if let HeapObj::Tuple(t) = heap.get(*a) { h.write_u8(6); h.write_u64(t.iter().fold(0u64, |acc, &e| acc.wrapping_add(hash_depth(e, heap, depth + 1)))); },
        // A range hashes its length, then the start and step its length shows, so equal ranges probe alike.
        &HeapObj::Range(s, e, st) => {
            let n = range_len(s, e, st);
            h.write_u8(7);
            for x in [hash_int(n), if n == 0 { 0 } else { hash_int(s as i128) }, if n <= 1 { 0 } else { hash_int(st as i128) }] { h.write_i64(x); }
        }
        HeapObj::BigRange(..) => { h.write_u8(7); for x in crate::frontend::int::range_hashes(heap, v, 0).unwrap_or_default() { h.write_i64(x); } }
        _ => h.write_u64(v.0),
    }
    h.finish()
}

// Numbers hash by their value modulo this prime, so equal ints, floats and bools hash alike.
const MODULUS: u64 = (1 << 61) - 1;

/* The hash of `n`, its value modulo the prime with its sign, -1 kept for errors. */
pub(crate) fn hash_int(n: i128) -> i64 {
    if n.unsigned_abs() < MODULUS as u128 { return hash_small(n as i64); }
    let h = (n.unsigned_abs() % MODULUS as u128) as i64;
    hash_small(if n < 0 { -h } else { h })
}

/* The hash of an int below the prime, itself unless it is the -1 kept for errors. */
#[inline(always)]
pub(crate) fn hash_small(n: i64) -> i64 { if n == -1 { -2 } else { n } }

/* The hash of a finite float, equal to the hash of an int of the same value. */
pub(crate) fn hash_float(v: f64) -> i64 {
    if v.is_infinite() { return if v > 0.0 { 314_159 } else { -314_159 }; }
    let (mut m, mut e) = frexp(v.abs());
    let mut x: u64 = 0;
    while m != 0.0 {
        x = ((x << 28) & MODULUS) | x >> (61 - 28);
        m *= 268_435_456.0;
        e -= 28;
        let y = m as u64;
        m -= y as f64;
        x += y;
        if x >= MODULUS { x -= MODULUS; }
    }
    let e = if e >= 0 { e % 61 } else { 61 - 1 - ((-1 - e) % 61) } as u32;
    x = ((x << e) & MODULUS) | x >> (61 - e);
    let h = if v < 0.0 { x.wrapping_neg() } else { x } as i64;
    if h == -1 { -2 } else { h }
}

/* The mantissa in [0.5, 1) and the exponent of a positive float. */
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 { return (0.0, 0); }
    let exp = ((x.to_bits() >> 52) & 0x7ff) as i32;
    if exp == 0 { let (m, e) = frexp(x * 18_014_398_509_481_984.0); return (m, e - 54); }
    (f64::from_bits((x.to_bits() & !(0x7ff << 52)) | (1022 << 52)), exp - 1022)
}

/* f64 view of any number, None for anything else and an error for an int too large for a float. */
pub(crate) fn num_f64(v: Val, heap: &HeapPool) -> Option<Result<f64, VmErr>> {
    if v.is_float() { Some(Ok(v.as_float())) }
    else if v.is_int() { Some(Ok(v.as_int() as f64)) }
    else if v.is_bool() { Some(Ok(v.as_bool() as i64 as f64)) }
    else { heap.foreign_f64(v) }
}

/* f64 view of any number, None for anything else and for an int too large for a float. */
pub(crate) fn num_as_f64(v: Val, heap: &HeapPool) -> Option<f64> { num_f64(v, heap)?.ok() }

fn eq_vals_depth(a: Val, b: Val, heap: &HeapPool, depth: usize, rich: &mut bool) -> bool {
    // Past the cap only identity answers, and a distinct pair goes to a caller that can raise.
    if depth > EQ_DEPTH_MAX { *rich |= a.0 != b.0; return a.0 == b.0; }
    // An element equals itself, so a NaN inside `[n] == [n]` matches.
    if depth > 0 && a.0 == b.0 { return true; }

    // Two inline ints, the common scan, compare their bits since each value has one encoding.
    if a.is_int() && b.is_int() { return a.0 == b.0; }
    // A FOREIGN value answers through its kind, false across families.
    if (a.is_foreign() || b.is_foreign()) && !a.is_heap() && !b.is_heap() && let Some(e) = heap.foreign_eq(a, b) { return e; }

    // Inline ints and bools compare by value.
    if let (Some(ai), Some(bi)) = (as_i128(a, heap), as_i128(b, heap)) {
        return ai == bi;
    }

    // One side is a float here (all-integer handled above), compare numerically so float unifies with int/bool, e.g. `1.0 == True`.
    if let (Some(af), Some(bf)) = (num_as_f64(a, heap), num_as_f64(b, heap)) {
        return af == bf;
    }

    if !a.is_heap() || !b.is_heap() {
        // An instance against an immediate still answers through its own `__eq__`.
        let other = if a.is_heap() { a } else if b.is_heap() { b } else { return a.0 == b.0 };
        *rich |= matches!(heap.get(other), HeapObj::Instance(c, _) if class_defines_eq(*c, heap, 0));
        return false;
    }

    // A heap object equals itself, short-circuits self-referential containers before the element walk.
    if a.0 == b.0 { return true; }

    let d = depth + 1;
    // Each walk over elements counts their number as native work.
    match (heap.get(a), heap.get(b)) {
        (HeapObj::Str(x), HeapObj::Str(y)) => eq_text(x.as_bytes(), y.as_bytes(), heap),
        (HeapObj::Bytes(x), HeapObj::Bytes(y)) => eq_text(x, y, heap),
        (HeapObj::Tuple(x), HeapObj::Tuple(y)) => { heap.spend(x.len()); eq_seq(x, y, |a,b| eq_vals_depth(a, b, heap, d, rich)) }
        (HeapObj::List(x), HeapObj::List(y)) => { heap.spend(x.borrow().len()); eq_seq(&x.borrow(), &y.borrow(), |a,b| eq_vals_depth(a, b, heap, d, rich)) }
        (HeapObj::Set(x), HeapObj::Set(y)) => eq_tables(&x.borrow(), &y.borrow(), heap, d, rich),
        (HeapObj::FrozenSet(x), HeapObj::FrozenSet(y)) => eq_tables(x, y, heap, d, rich),
        (HeapObj::Set(x), HeapObj::FrozenSet(y)) => eq_tables(&x.borrow(), y, heap, d, rich),
        (HeapObj::FrozenSet(x), HeapObj::Set(y)) => eq_tables(x, &y.borrow(), heap, d, rich),
        (HeapObj::Dict(x), HeapObj::Dict(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            heap.spend(x.len());
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(&k, heap).is_some_and(|&v2| v.0 == v2.0 || eq_vals_depth(v, v2, heap, d, rich)))
        }
        // An instance with its own `__eq__` decides in user code, the VM asks it.
        (HeapObj::Instance(..), _) | (_, HeapObj::Instance(..)) => {
            *rich |= [a, b].iter().any(|&v| matches!(heap.get(v), HeapObj::Instance(c, _) if class_defines_eq(*c, heap, 0)));
            false
        }
        (HeapObj::Type(x), HeapObj::Type(y)) => x == y, // by name, interning also makes `is` hold
        (HeapObj::GenericAlias(o1, a1), HeapObj::GenericAlias(o2, a2)) => eq_vals_depth(*o1, *o2, heap, d, rich) && eq_vals_depth(*a1, *a2, heap, d, rich),
        (HeapObj::Union(a1), HeapObj::Union(a2)) => match (heap.get(*a1), heap.get(*a2)) {
            (HeapObj::Tuple(x), HeapObj::Tuple(y)) => x.len() == y.len() && x.iter().all(|&m| y.iter().any(|&n| eq_vals_depth(m, n, heap, d, rich))),
            _ => false,
        },
        (HeapObj::BigRange(..), HeapObj::Range(..) | HeapObj::BigRange(..)) | (HeapObj::Range(..), HeapObj::BigRange(..)) => crate::frontend::int::range_eq(heap, a, b).unwrap_or(false),
        (HeapObj::Range(s1,e1,t1), HeapObj::Range(s2,e2,t2)) => {
            // Python semantics, equal length, then matching start/step only when non-empty.
            let (l1, l2) = (range_len(*s1,*e1,*t1), range_len(*s2,*e2,*t2));
            l1 == l2 && (l1 == 0 || (s1 == s2 && (l1 == 1 || t1 == t2)))
        }
        // Keys views compare as key sets and items views as dicts, a values view only to itself.
        (HeapObj::DictView(x, View::Keys), HeapObj::DictView(y, View::Keys)) => match (heap.get(*x), heap.get(*y)) {
            (HeapObj::Dict(x), HeapObj::Dict(y)) => { let (x, y) = (x.borrow(), y.borrow()); x.len() == y.len() && x.keys().all(|k| y.contains_key(&k, heap)) }
            _ => false,
        },
        (HeapObj::DictView(x, View::Items), HeapObj::DictView(y, View::Items)) => eq_vals_depth(*x, *y, heap, d, rich),
        // Cross-type comparisons fall through to false. Notably `bytes == str` is False, even when the bytes are valid UTF-8 of the str.
        _ => false,
    }
}

/* Set equality, each item matched under the hash it stored. */
fn eq_tables(x: &ValSet, y: &ValSet, heap: &HeapPool, d: usize, rich: &mut bool) -> bool {
    heap.spend(x.len());
    eq_set(x, y, |a, b| a.0 == b.0 || eq_vals_depth(a, b, heap, d, rich))
}

/* Count of values range(start, stop, step) yields, step is never zero. */
pub(crate) fn range_len(s: i64, e: i64, t: i64) -> i128 {
    let (lo, hi, step) = if t > 0 { (s as i128, e as i128, t as i128) } else { (e as i128, s as i128, -(t as i128)) };
    if hi > lo { (hi - lo + step - 1) / step } else { 0 }
}
