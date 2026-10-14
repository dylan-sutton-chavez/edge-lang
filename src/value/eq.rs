use core::cmp::Ordering;

use super::{HeapObj, HeapPool, Val, ValSet, View, as_i128, as_long_int};

/* 2^127 exactly, the saturation guard for f64-to-i128 casts. */
const TWO_POW_127: f64 = 170141183460469231731687303715884105728.0;

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
    // Numeric unification, int / bool / integral-float / in-range LongInt all hash as the same i64.
    if v.is_int() { h.write_i64(v.as_int()); return h.finish(); }
    if v.is_bool() { h.write_i64(v.as_bool() as i64); return h.finish(); }
    if v.is_float() {
        let f = v.as_float();
        // Integral floats hash exactly as the equal int or LongInt, so equal keys probe identically.
        match float_exact_i128(f) {
            Some(i) => write_i128(&mut h, i),
            None => h.write_u64(f.to_bits()),
        }
        return h.finish();
    }
    if !v.is_heap() || depth > EQ_DEPTH_MAX { h.write_u64(v.0); return h.finish(); }
    match heap.get(v) {
        HeapObj::LongInt(i) => write_i128(&mut h, i.get()),
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
        _ => h.write_u64(v.0),
    }
    h.finish()
}

/* i64-range values hash like the equal inline int, wider values hash their two halves. */
fn write_i128(h: &mut crate::util::hash::FxHasher, i: i128) {
    use core::hash::Hasher;
    match i64::try_from(i) {
        Ok(n) => h.write_i64(n),
        Err(_) => { h.write_u64(i as u64); h.write_u64((i >> 64) as u64); }
    }
}

/* Exact i128 view of an integral in-range f64, `as` saturates so the 2^127 bound is mandatory. */
pub(crate) fn float_exact_i128(f: f64) -> Option<i128> {
    if f.abs() < TWO_POW_127 && libm::trunc(f) == f { Some(f as i128) } else { None }
}

/* Exact float vs wide-int ordering, None on NaN. The float converts to i128 since the wide int would round in f64. */
pub(crate) fn float_cmp_int(f: f64, i: i128) -> Option<Ordering> {
    if f.is_nan() { return None; }
    if let Some(fi) = float_exact_i128(f) { return Some(fi.cmp(&i)); }
    // Out-of-range floats exceed any i128, fractional in-range floats break a truncation tie by sign.
    if f.abs() >= TWO_POW_127 { return Some(if f > 0.0 { Ordering::Greater } else { Ordering::Less }); }
    let t = libm::trunc(f) as i128;
    Some(match t.cmp(&i) {
        Ordering::Equal => if f > 0.0 { Ordering::Greater } else { Ordering::Less },
        o => o,
    })
}

/* f64 view of any numeric Val (int/bool/float/LongInt), None for non-numerics. */
pub(crate) fn num_as_f64(v: Val, heap: &HeapPool) -> Option<f64> {
    if v.is_float() { Some(v.as_float()) }
    else if v.is_int() { Some(v.as_int() as f64) }
    else if v.is_bool() { Some(v.as_bool() as i64 as f64) }
    else if v.is_heap() { if let HeapObj::LongInt(i) = heap.get(v) { Some(i.get() as f64) } else { None } }
    else { None }
}

fn eq_vals_depth(a: Val, b: Val, heap: &HeapPool, depth: usize, rich: &mut bool) -> bool {
    // Past the cap only identity answers, and a distinct pair goes to a caller that can raise.
    if depth > EQ_DEPTH_MAX { *rich |= a.0 != b.0; return a.0 == b.0; }
    // An element equals itself, so a NaN inside `[n] == [n]` matches.
    if depth > 0 && a.0 == b.0 { return true; }

    // Unify all int-flavoured pairs through i128 (LongInt, inline int, bool).
    if let (Some(ai), Some(bi)) = (as_i128(a, heap), as_i128(b, heap)) {
        return ai == bi;
    }

    if a.is_float() && let Some(bi) = as_long_int(b, heap) {
        return float_cmp_int(a.as_float(), bi) == Some(Ordering::Equal);
    }
    if b.is_float() && let Some(ai) = as_long_int(a, heap) {
        return float_cmp_int(b.as_float(), ai) == Some(Ordering::Equal);
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
