
use super::{HeapObj, HeapPool, Val, ValSet, VmErr, as_i128};

pub(crate) fn eq_seq(a: &[Val], b: &[Val], mut eq: impl FnMut(Val,Val)->bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x,y)| eq(*x,*y))
}
/* same size and every item of `a` found in `b` under the hash it stored */
pub(crate) fn eq_set(a: &ValSet, b: &ValSet, heap: &HeapPool, mut eq: impl FnMut(Val,Val)->bool) -> bool {
    a.len() == b.len() && a.iter_hashed().all(|(h, x)| b.slot(h, heap, |y| eq(x, y)).is_some())
}

/* the items a walk reads before it walks again keeping the answer of each container */
const KEEP_AFTER: usize = 1 << 20;

/* where a walk keeps what each container gave so shared parts read once */
pub(crate) trait Kept<K, V> {
    // whether the walk keeps answers and so needs no bound
    const ON: bool;
    fn get(&self, k: &K) -> Option<V>;
    fn put(&mut self, k: K, v: V);
}

/* a walk that keeps nothing and stops once it reads past its bound */
#[derive(Default)]
pub(crate) struct Bounded;

impl<K, V> Kept<K, V> for Bounded {
    const ON: bool = false;
    #[inline(always)]
    fn get(&self, _: &K) -> Option<V> { None }
    #[inline(always)]
    fn put(&mut self, _: K, _: V) {}
}

impl<K: core::hash::Hash + Eq, V: Copy> Kept<K, V> for crate::util::hash::FxHashMap<K, V> {
    const ON: bool = true;
    fn get(&self, k: &K) -> Option<V> { hashbrown::HashMap::get(self, k).copied() }
    fn put(&mut self, k: K, v: V) { self.insert(k, v); }
}

/* the items a walk read and the answers it keeps */
#[derive(Default)]
pub(crate) struct Walk<M> { pub(crate) read: usize, long: bool, pub(crate) kept: M }

impl<M> Walk<M> {
    /* counts `n` more items and false once a walk that keeps nothing passed its bound */
    #[inline(always)]
    pub(crate) fn read<K, V>(&mut self, n: usize) -> bool where M: Kept<K, V> {
        if M::ON { self.read = self.read.saturating_add(n); return true; }
        // the count stays at most the bound so adding what memory holds never wraps
        if n > KEEP_AFTER - self.read { (self.read, self.long) = (KEEP_AFTER, true); return false; }
        self.read += n;
        true
    }

    /* whether a bounded walk stopped so its answer needs the walk that keeps answers */
    #[inline(always)]
    pub(crate) fn long(&self) -> bool { self.long }
}

/* a walk keeping the answer of each container it read at each depth */
pub(crate) type Keeping<K, V> = Walk<crate::util::hash::FxHashMap<K, V>>;

/* the items an equality walk read above its cap and bound flags */
#[derive(Default)]
struct EqCx { read: usize }

// the cap flag and the bound flag sit below the count
const RICH: usize = 1;
const LONG: usize = 2;
// each item read counts this much so the flags keep the low bits
const ITEM: usize = 4;

// a pair of containers compared at a depth
type PairKey = (u64, u64, usize);

/* a keeping walk's cap flag and each pair's answer at each depth */
struct KeptCx { rich: bool, kept: crate::util::hash::FxHashMap<PairKey, (bool, bool)> }

/* the recursion cap so a self-referential container falls back instead of overflowing the stack */
pub(crate) const EQ_DEPTH_MAX: usize = 100;

pub fn eq_vals_with_heap(a: Val, b: Val, heap: &HeapPool) -> bool {
    eq_vals_depth(a, b, heap, 0, &mut EqCx::default())
}

/* a container member compares by identity first and by content after */
#[inline(always)]
pub fn eq_member(a: Val, b: Val, heap: &HeapPool) -> bool { a.0 == b.0 || eq_content(a, b, heap) }

/* the content half of `eq_member` kept apart so every probe loop stays small */
#[inline(never)]
fn eq_content(a: Val, b: Val, heap: &HeapPool) -> bool {
    // two strings compare their text straight away as the common dict and set key
    if let (Some(HeapObj::Str(x)), Some(HeapObj::Str(y))) = (heap.try_get(a), heap.try_get(b)) { return eq_text(x.as_bytes(), y.as_bytes(), heap); }
    eq_vals_depth(a, b, heap, 0, &mut EqCx::default())
}

/* a long pair counts the bytes it reads as native work */
#[inline]
pub(crate) fn eq_text(x: &[u8], y: &[u8], heap: &HeapPool) -> bool {
    if x.len() == y.len() && x.len() > 64 { heap.spend(x.len()); }
    x == y
}

/* content equality from `depth` levels in or none past the cap */
#[inline]
pub fn eq_checked(a: Val, b: Val, heap: &HeapPool, depth: usize) -> Option<bool> {
    let mut cx = EqCx::default();
    let r = eq_vals_depth(a, b, heap, depth, &mut cx);
    if !r && cx.read & (RICH | LONG) != 0 { None } else { Some(r) }
}

/* the content hash where values that compare equal hash equal across number types */
pub fn hash_val_with_heap(v: Val, heap: &HeapPool) -> u64 {
    hash_depth(v, heap, 0)
}

/* the hash a string `Val` holds taken from the text alone */
pub fn hash_str(s: &str) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
    h.write_u8(1);
    h.write(s.as_bytes());
    h.finish()
}

/* the hash a table keyed by text probes with folded so similar texts spread */
#[inline]
pub fn hash_key(s: &str) -> u64 {
    let h = hash_str(s);
    (h ^ h >> 32).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}
fn hash_depth(v: Val, heap: &HeapPool, depth: usize) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
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
        // a long text counts what it hashes as native work
        HeapObj::Str(s) => { if s.len() > 64 { heap.spend(s.len()); } h.write_u8(1); h.write(s.as_bytes()); }
        HeapObj::Bytes(b) => { if b.len() > 64 { heap.spend(b.len()); } h.write_u8(2); h.write(b); }
        HeapObj::Tuple(t) => return hash_top(v, t, heap, depth),
        // a range hashes its start which equal ranges share and nothing when it is empty
        &HeapObj::Range(s, e, st) => {
            h.write_u8(7);
            h.write_i64(if range_len(s, e, st) == 0 { 0 } else { hash_int(s as i128) });
        }
        // a range hashes its start which equal ranges share and nothing when it is empty
        HeapObj::BigRange(..) => {
            h.write_u8(7);
            match super::wide::range_start(heap, v).flatten() {
                Some(s) => h.write_i64(as_i128(s, heap).map_or_else(|| heap.foreign_hash(s).unwrap_or(0) as i64, hash_int)),
                None => h.write_i64(0),
            }
        }
        _ => h.write_u64(v.0),
    }
    h.finish()
}

/* the hash of a top tuple which a long walk computes again keeping answers */
#[inline(never)]
fn hash_top(v: Val, t: &[Val], heap: &HeapPool, depth: usize) -> u64 {
    let mut walk = Walk::<Bounded>::default();
    let h = hash_tuple(v, t, heap, depth, &mut walk);
    if walk.long() { hash_kept(v, t, heap, depth) } else { h }
}

/* the tuple hashed again keeping the hash of each tuple it reads at each depth */
#[cold]
#[inline(never)]
fn hash_kept(v: Val, t: &[Val], heap: &HeapPool, depth: usize) -> u64 { hash_tuple(v, t, heap, depth, &mut Keeping::default()) }

/* the hash of tuple `v` where a long walk keeps each tuple's hash */
fn hash_tuple<M: Kept<(u64, usize), u64>>(v: Val, t: &[Val], heap: &HeapPool, depth: usize, walk: &mut Walk<M>) -> u64 {
    use core::hash::Hasher;
    if let Some(h) = walk.kept.get(&(v.0, depth)) { return h; }
    if !walk.read(t.len()) { return 0; }
    // every tuple counts what it hashes as native work
    heap.spend(t.len());
    let mut h = crate::util::hash::FxHasher::default();
    h.write_u8(3);
    h.write_usize(t.len());
    for &e in t {
        // a tuple item walks on with this walk and any other item hashes alone
        let x = match (e.is_heap() && depth < EQ_DEPTH_MAX).then(|| heap.get(e)) { Some(HeapObj::Tuple(items)) => hash_tuple(e, items, heap, depth + 1, walk), _ => hash_depth(e, heap, depth + 1) };
        h.write_u64(x);
    }
    let r = h.finish();
    walk.kept.put((v.0, depth), r);
    r
}

/* tables hash every number modulo two to these bits less one */
pub const HASH_BITS: u32 = 61;

// every number hashes by its value modulo this prime so equal numbers hash alike
const MODULUS: u64 = (1 << HASH_BITS) - 1;

/* `n` modulo the prime with its sign and -1 kept for errors */
pub(crate) fn hash_int(n: i128) -> i64 {
    if n.unsigned_abs() < MODULUS as u128 { return hash_small(n as i64); }
    hash_modulo(n < 0, (n.unsigned_abs() % MODULUS as u128) as u64)
}

/* an int's table hash from its sign and its magnitude modulo the prime */
#[inline]
pub fn hash_modulo(neg: bool, m: u64) -> i64 {
    debug_assert!(m < MODULUS);
    hash_small(if neg { -(m as i64) } else { m as i64 })
}

/* an int below the prime hashes as itself except the -1 kept for errors */
#[inline(always)]
pub(crate) fn hash_small(n: i64) -> i64 { if n == -1 { -2 } else { n } }

/* a float hashes like the int of the same value */
pub(crate) fn hash_float(v: f64) -> i64 {
    if v.is_infinite() { return if v > 0.0 { 314_159 } else { -314_159 }; }
    let (mut m, mut e) = frexp(v.abs());
    let mut x: u64 = 0;
    while m != 0.0 {
        x = ((x << 28) & MODULUS) | x >> (HASH_BITS - 28);
        m *= 268_435_456.0;
        e -= 28;
        let y = m as u64;
        m -= y as f64;
        x += y;
        if x >= MODULUS { x -= MODULUS; }
    }
    let bits = HASH_BITS as i32;
    let e = if e >= 0 { e % bits } else { bits - 1 - ((-1 - e) % bits) } as u32;
    x = ((x << e) & MODULUS) | x >> (HASH_BITS - e);
    hash_modulo(v < 0.0, x)
}

/* a positive float's mantissa of a half or more below one and its exponent */
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 { return (0.0, 0); }
    let exp = ((x.to_bits() >> 52) & 0x7ff) as i32;
    if exp == 0 { let (m, e) = frexp(x * 18_014_398_509_481_984.0); return (m, e - 54); }
    (f64::from_bits((x.to_bits() & !(0x7ff << 52)) | (1022 << 52)), exp - 1022)
}

/* `None` for a non-number and an error for an int too large for a float */
pub(crate) fn num_f64(v: Val, heap: &HeapPool) -> Option<Result<f64, VmErr>> {
    if v.is_float() { Some(Ok(v.as_float())) }
    else if v.is_int() { Some(Ok(v.as_int() as f64)) }
    else if v.is_bool() { heap.policy.bool_is_int.then_some(Ok(v.as_bool() as i64 as f64)) }
    else { heap.foreign_f64(v) }
}

/* `num_f64` with an int too large for a float as `None` too */
pub(crate) fn num_as_f64(v: Val, heap: &HeapPool) -> Option<f64> { num_f64(v, heap)?.ok() }

/* what `$walk` answers for `$n` items counted toward the walk's bound */
macro_rules! bounded {
    ($cx:ident, $a:ident, $b:ident, $depth:ident, $n:expr, $heap:ident, $walk:expr) => {{
        let n = $n;
        $heap.spend(n);
        if $depth != 0 {
            // a pair inside a pair counts toward the bound which one pair alone never passes
            let read = $cx.read + n * ITEM;
            if read > KEEP_AFTER * ITEM + (RICH | LONG) { $cx.read = $cx.read & RICH | LONG | KEEP_AFTER * ITEM; return false; }
            $cx.read = read;
            $walk
        } else {
            let r = $walk;
            if !r && $cx.read & LONG != 0 { eq_kept($a, $b, $heap, &mut $cx.read) } else { r }
        }
    }};
}

fn eq_vals_depth(a: Val, b: Val, heap: &HeapPool, depth: usize, cx: &mut EqCx) -> bool {
    // past the cap only identity answers and `cx` flags a distinct pair for the caller
    if depth > EQ_DEPTH_MAX { cx.read |= RICH * (a.0 != b.0) as usize; return a.0 == b.0; }
    // an element equals itself so a nan inside `[n] == [n]` matches
    if depth > 0 && a.0 == b.0 { return true; }

    // two inline ints compare their bits since each value has one encoding
    if a.is_int() && b.is_int() { return a.0 == b.0; }
    // a heap pair skips the number checks since no heap object is a number
    if !a.is_heap() || !b.is_heap() {
        // a foreign value answers through its kind and is false across families
        if (a.is_foreign() || b.is_foreign()) && !a.is_heap() && !b.is_heap() && let Some(e) = heap.foreign_eq(a, b) { return e; }

        if let (Some(ai), Some(bi)) = (as_i128(a, heap), as_i128(b, heap)) {
            return ai == bi;
        }

        // one side is a float here so floats compare by value with ints and bools
        if let (Some(af), Some(bf)) = (num_as_f64(a, heap), num_as_f64(b, heap)) {
            return af == bf;
        }
        return !a.is_heap() && !b.is_heap() && a.0 == b.0;
    }

    // a heap object equals itself which stops a self-referential container before the walk
    if a.0 == b.0 { return true; }

    let d = depth + 1;
    // each walk over elements counts their number as native work
    match (heap.get(a), heap.get(b)) {
        (HeapObj::Str(x), HeapObj::Str(y)) => eq_text(x.as_bytes(), y.as_bytes(), heap),
        (HeapObj::Bytes(x) | HeapObj::ByteArray(x), HeapObj::Bytes(y) | HeapObj::ByteArray(y)) => eq_text(x, y, heap),
        (HeapObj::Tuple(x), HeapObj::Tuple(y)) => bounded!(cx, a, b, depth, x.len(), heap, eq_seq(x, y, |a,b| eq_vals_depth(a, b, heap, d, cx))),
        (HeapObj::List(x), HeapObj::List(y)) => bounded!(cx, a, b, depth, x.borrow().len(), heap, eq_seq(&x.borrow(), &y.borrow(), |a,b| eq_vals_depth(a, b, heap, d, cx))),
        (HeapObj::Set(x), HeapObj::Set(y)) => bounded!(cx, a, b, depth, x.borrow().len(), heap, eq_tables(&x.borrow(), &y.borrow(), heap, d, cx)),
        (HeapObj::Dict(x), HeapObj::Dict(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            bounded!(cx, a, b, depth, x.len(), heap, x.len() == y.len() && x.iter().all(|(k, v)| y.get(&k, heap).is_some_and(|&v2| v.0 == v2.0 || eq_vals_depth(v, v2, heap, d, cx))))
        }
        // two type objects compare by name since interning also makes `is` hold
        (HeapObj::Object { .. }, HeapObj::Object { .. }) => heap.type_object_name(a).is_some_and(|x| heap.type_object_name(b) == Some(x)),
        (HeapObj::BigRange(..), HeapObj::Range(..) | HeapObj::BigRange(..)) | (HeapObj::Range(..), HeapObj::BigRange(..)) => super::wide::range_eq(heap, a, b),
        (HeapObj::Range(s1,e1,t1), HeapObj::Range(s2,e2,t2)) => {
            // equal lengths then the start when non-empty and the step past one item
            let (l1, l2) = (range_len(*s1,*e1,*t1), range_len(*s2,*e2,*t2));
            l1 == l2 && (l1 == 0 || (s1 == s2 && (l1 == 1 || t1 == t2)))
        }
        // other type pairs are unequal so bytes never equal a str of the same utf-8
        _ => false,
    }
}

fn eq_tables(x: &ValSet, y: &ValSet, heap: &HeapPool, d: usize, cx: &mut EqCx) -> bool {
    eq_set(x, y, heap, |a, b| a.0 == b.0 || eq_vals_depth(a, b, heap, d, cx))
}

/* the top pair compared again keeping answers once its plain walk passed the bound */
#[cold]
#[inline(never)]
fn eq_kept(a: Val, b: Val, heap: &HeapPool, read: &mut usize) -> bool {
    let mut cx = KeptCx { rich: false, kept: crate::util::hash::FxHashMap::default() };
    let r = eq_vals_kept(a, b, heap, 0, &mut cx);
    *read = cx.rich as usize;
    r
}

/* the equality walk keeping the answer of each container pair so shared parts read once */
fn eq_vals_kept(a: Val, b: Val, heap: &HeapPool, depth: usize, cx: &mut KeptCx) -> bool {
    if depth > EQ_DEPTH_MAX { cx.rich |= a.0 != b.0; return a.0 == b.0; }
    // a pair holding no items answers as the plain walk does
    if a.0 == b.0 || !a.is_heap() || !b.is_heap() { return eq_vals_depth(a, b, heap, depth, &mut EqCx::default()); }
    let d = depth + 1;
    // a pair met before at this depth gives its kept answer without reading it again
    macro_rules! kept {
        ($n:expr, $walk:expr) => {{
            let key = (a.0, b.0, depth);
            if let Some(&(r, rich)) = cx.kept.get(&key) { cx.rich |= rich; return r; }
            heap.spend($n);
            let outer = core::mem::take(&mut cx.rich);
            let r = $walk;
            let rich = cx.rich;
            cx.rich |= outer;
            cx.kept.insert(key, (r, rich));
            r
        }};
    }
    match (heap.get(a), heap.get(b)) {
        (HeapObj::Tuple(x), HeapObj::Tuple(y)) => kept!(x.len(), eq_seq(x, y, |a, b| eq_vals_kept(a, b, heap, d, cx))),
        (HeapObj::List(x), HeapObj::List(y)) => kept!(x.borrow().len(), eq_seq(&x.borrow(), &y.borrow(), |a, b| eq_vals_kept(a, b, heap, d, cx))),
        (HeapObj::Set(x), HeapObj::Set(y)) => kept!(x.borrow().len(), eq_set(&x.borrow(), &y.borrow(), heap, |a, b| a.0 == b.0 || eq_vals_kept(a, b, heap, d, cx))),
        (HeapObj::Dict(x), HeapObj::Dict(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            kept!(x.len(), x.len() == y.len() && x.iter().all(|(k, v)| y.get(&k, heap).is_some_and(|&v2| v.0 == v2.0 || eq_vals_kept(v, v2, heap, d, cx))))
        }
        _ => eq_vals_depth(a, b, heap, depth, &mut EqCx::default()),
    }
}

/* how many values a range yields where the step is never zero */
pub(crate) fn range_len(s: i64, e: i64, t: i64) -> i128 {
    let (lo, hi) = if t > 0 { (s, e) } else { (e, s) };
    // the span and the step fit a u64 so the count needs no wide division
    if hi > lo { i128::from((hi.abs_diff(lo) - 1) / t.unsigned_abs() + 1) } else { 0 }
}
