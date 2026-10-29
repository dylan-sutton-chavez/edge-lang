use core::cmp::Ordering;

use super::{BinOp, HeapObj, HeapPool, Val, VmErr, as_i128, cold_overflow};
use super::math::int_divmod;

impl HeapPool {
    /* whether `v` is boxed by the wide int kind */
    #[inline]
    pub fn is_wide(&self, v: Val) -> bool {
        v.is_foreign() && self.foreign.wide_int().is_some_and(|w| self.foreign_kind(v).is_some_and(|k| core::ptr::eq(k, w)))
    }

    /* the error of an int result past the inline range with no wide kind */
    #[cold]
    pub(crate) fn past_range(&self) -> VmErr { if self.foreign.wide_int().is_none() { super::cold_int_range() } else { cold_overflow() } }

    /* the wide int kind's int that `digits` in `base` spell most significant first */
    pub(crate) fn wide_digits(&mut self, neg: bool, digits: &[u8], base: u32) -> Result<Val, VmErr> {
        match self.foreign.wide_int().and_then(|k| k.from_digits) {
            Some(make) => make(self, neg, digits, base),
            None => Err(super::cold_int_range()),
        }
    }
}

/* the order of two ints of any width */
pub(crate) fn order(heap: &HeapPool, a: Val, b: Val) -> Ordering {
    match (as_i128(a, heap), as_i128(b, heap)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => heap.foreign_cmp(a, b).unwrap_or(Ordering::Equal),
    }
}

/* two ints combined by `op` in i128 when it fits or by the wide kind */
pub(crate) fn arith(heap: &mut HeapPool, op: BinOp, a: Val, b: Val) -> Result<Val, VmErr> {
    if let (Some(x), Some(y)) = (as_i128(a, heap), as_i128(b, heap)) {
        if matches!(op, BinOp::FloorDiv | BinOp::Mod) && y == 0 { return Err(VmErr::ZeroDiv); }
        let r = match op {
            BinOp::Add => x.checked_add(y),
            BinOp::Sub => x.checked_sub(y),
            BinOp::Mul => x.checked_mul(y),
            BinOp::FloorDiv => int_divmod(x, y).map(|(q, _)| q),
            BinOp::Mod => int_divmod(x, y).map(|(_, r)| r),
            _ => None,
        };
        if let Some(r) = r { return heap.int(r); }
    }
    heap.foreign_binop(op, a, b)?.ok_or_else(|| heap.past_range())
}

/* a boxed index clamped to its sign's inline extreme which no sequence length reaches */
pub(crate) fn clip_index(heap: &HeapPool, v: Val) -> Val {
    // a bool bounds as the int it is where bools count as ints
    if v.is_bool() && heap.policy.bool_is_int { return Val::int(v.as_bool() as i64); }
    if !heap.is_wide(v) { return v; }
    Val::int(if order(heap, v, Val::int(0)).is_lt() { Val::INT_MIN } else { Val::INT_MAX })
}

/* the start stop and step of a range whose bounds leave a word */
fn wide_parts(heap: &HeapPool, v: Val) -> Option<[Val; 3]> {
    match *heap.try_get(v)? { HeapObj::BigRange(s, e, st) => Some([s, e, st]), _ => None }
}

/* the start stop and step of a range of any width as values */
fn parts(heap: &mut HeapPool, v: Val) -> Option<Result<[Val; 3], VmErr>> {
    let (s, e, st) = match *heap.try_get(v)? {
        HeapObj::BigRange(s, e, st) => return Some(Ok([s, e, st])),
        HeapObj::Range(s, e, st) => (s, e, st),
        _ => return None,
    };
    Some((|| Ok([heap.int(s as i128)?, heap.int(e as i128)?, heap.int(st as i128)?]))())
}

/* whether the run from `s` to `e` by `st` holds no item */
fn empty(heap: &HeapPool, [s, e, st]: [Val; 3]) -> bool {
    if order(heap, st, Val::int(0)).is_gt() { order(heap, s, e).is_ge() } else { order(heap, s, e).is_le() }
}

/* how many items the run from `s` to `e` by `st` holds */
fn span(heap: &mut HeapPool, [s, e, st]: [Val; 3]) -> Result<Val, VmErr> {
    if empty(heap, [s, e, st]) { return Ok(Val::int(0)); }
    let rising = order(heap, st, Val::int(0)).is_gt();
    let (lo, hi, step) = if rising { (s, e, st) } else { (e, s, arith(heap, BinOp::Sub, Val::int(0), st)?) };
    let gap = arith(heap, BinOp::Sub, hi, lo)?;
    let gap = arith(heap, BinOp::Sub, gap, Val::int(1))?;
    let q = arith(heap, BinOp::FloorDiv, gap, step)?;
    arith(heap, BinOp::Add, q, Val::int(1))
}

/* the length of a wide range and an overflow past what a length holds */
pub(crate) fn range_len(heap: &mut HeapPool, v: Val) -> Option<Result<i128, VmErr>> {
    let p = wide_parts(heap, v)?;
    Some(span(heap, p).and_then(|n| as_i128(n, heap).filter(|&n| n <= i64::MAX as i128).ok_or_else(|| VmErr::Overflow)))
}

/* whether a wide range holds no item */
pub(crate) fn range_empty(heap: &HeapPool, v: Val) -> Option<bool> { wide_parts(heap, v).map(|p| empty(heap, p)) }

/* item `i` of a wide range and an index error past either end */
pub(crate) fn range_item(heap: &mut HeapPool, v: Val, i: Val) -> Option<Result<Val, VmErr>> {
    let p = wide_parts(heap, v)?;
    Some((|| {
        let n = span(heap, p)?;
        let i = if heap.policy.neg_index && order(heap, i, Val::int(0)).is_lt() { arith(heap, BinOp::Add, i, n)? } else { i };
        if order(heap, i, Val::int(0)).is_lt() || order(heap, i, n).is_ge() { return Err(crate::lang::EngineError::out_of_range(v, false)); }
        let step = arith(heap, BinOp::Mul, i, p[2])?;
        arith(heap, BinOp::Add, p[0], step)
    })())
}

/* the int a whole float stands for in a range and None for other floats */
fn whole(heap: &mut HeapPool, f: f64) -> Result<Option<Val>, VmErr> {
    if !f.is_finite() || libm::trunc(f) != f { return Ok(None); }
    if f.abs() < 9.0e15 { return heap.int(f as i128).map(Some); }
    // a whole float past 2^53 is its mantissa shifted left
    let bits = f.to_bits();
    let (exp, man) = (((bits >> 52) & 0x7ff) as i64 - 1075, (bits & ((1 << 52) - 1)) | 1 << 52);
    let m = heap.int(man as i128)?;
    let x = heap.foreign_binop(BinOp::Shl, m, Val::int(exp))?.ok_or_else(cold_overflow)?;
    if f < 0.0 { arith(heap, BinOp::Sub, Val::int(0), x).map(Some) } else { Ok(Some(x)) }
}

/* the index of `x` in a wide range with a whole float as its int */
fn range_pos(heap: &mut HeapPool, v: Val, x: Val) -> Option<Result<Option<Val>, VmErr>> {
    let [s, e, st] = wide_parts(heap, v)?;
    Some((|| {
        let x = if x.is_float() { match whole(heap, x.as_float())? { Some(x) => x, None => return Ok(None) } } else if as_i128(x, heap).is_some() || heap.is_wide(x) { x } else { return Ok(None) };
        let inside = if order(heap, st, Val::int(0)).is_gt() { order(heap, s, x).is_le() && order(heap, x, e).is_lt() } else { order(heap, e, x).is_lt() && order(heap, x, s).is_le() };
        if !inside { return Ok(None); }
        let off = arith(heap, BinOp::Sub, x, s)?;
        let rem = arith(heap, BinOp::Mod, off, st)?;
        if order(heap, rem, Val::int(0)).is_ne() { return Ok(None); }
        arith(heap, BinOp::FloorDiv, off, st).map(Some)
    })())
}

/* whether `x` is an item of a wide range */
pub(crate) fn range_has(heap: &mut HeapPool, v: Val, x: Val) -> Option<Result<bool, VmErr>> { range_pos(heap, v, x).map(|p| p.map(|p| p.is_some())) }

/* the index of `x` in a wide range and None when it is no item */
pub(crate) fn range_index(heap: &mut HeapPool, v: Val, x: Val) -> Option<Result<Option<Val>, VmErr>> { range_pos(heap, v, x) }

/* whether two ranges either wide hold the same items */
pub(crate) fn range_eq(heap: &HeapPool, a: Val, b: Val) -> bool {
    let narrow = |v: Val| -> Option<[i128; 3]> {
        match *heap.try_get(v)? {
            HeapObj::Range(s, e, st) => Some([s as i128, e as i128, st as i128]),
            HeapObj::BigRange(s, e, st) => Some([as_i128(s, heap)?, as_i128(e, heap)?, as_i128(st, heap)?]),
            _ => None,
        }
    };
    let count = |[s, e, st]: [i128; 3]| -> Option<i128> {
        let (lo, hi, step) = if st > 0 { (s, e, st) } else { (e, s, st.checked_neg()?) };
        if hi <= lo { return Some(0); }
        hi.checked_sub(lo)?.checked_sub(1)?.checked_div(step)?.checked_add(1)
    };
    if let (Some(x), Some(y)) = (narrow(a), narrow(b)) && let (Some(n), Some(m)) = (count(x), count(y)) {
        return n == m && (n == 0 || (x[0] == y[0] && (n == 1 || x[2] == y[2])));
    }
    // past i128 two ranges agree when both are empty or their bounds are the same
    let wide = |v: Val| match heap.try_get(v) { Some(&HeapObj::BigRange(s, e, st)) => Some([s, e, st]), _ => None };
    match (wide(a), wide(b)) {
        (Some(x), Some(y)) => (empty(heap, x) && empty(heap, y)) || x.iter().zip(&y).all(|(&p, &q)| order(heap, p, q).is_eq()),
        _ => false,
    }
}

/* the start of a wide range that equal ranges share and None when empty */
pub(crate) fn range_start(heap: &HeapPool, v: Val) -> Option<Option<Val>> {
    let p = wide_parts(heap, v)?;
    Some((!empty(heap, p)).then_some(p[0]))
}

/* a wide range walked back from its last item by its negated step */
pub(crate) fn range_reversed(heap: &mut HeapPool, v: Val) -> Option<Result<(Val, Val, Val), VmErr>> {
    let p = wide_parts(heap, v)?;
    Some((|| {
        let n = span(heap, p)?;
        let k = arith(heap, BinOp::Sub, n, Val::int(1))?;
        let off = arith(heap, BinOp::Mul, k, p[2])?;
        Ok((arith(heap, BinOp::Add, p[0], off)?, arith(heap, BinOp::Sub, p[0], p[2])?, arith(heap, BinOp::Sub, Val::int(0), p[2])?))
    })())
}

/* a slice of a range of any width with bounds clamped as slices clamp them */
pub(crate) fn range_slice(heap: &mut HeapPool, v: Val, start: Val, stop: Val, step: Val) -> Option<Result<Val, VmErr>> {
    let p = parts(heap, v)?;
    Some((|| {
        let p = p?;
        let [s, _, st] = p;
        let n = span(heap, p)?;
        let index = |heap: &HeapPool, x: Val| if x.is_bool() && heap.policy.bool_is_int { Ok(Val::int(x.as_bool() as i64)) } else if as_i128(x, heap).is_some() || heap.is_wide(x) { Ok(x) } else {
            Err(crate::lang::EngineError::bad_slice(x, false))
        };
        let c = if step.is_none() { Val::int(1) } else { index(heap, step)? };
        if order(heap, c, Val::int(0)).is_eq() { return Err(crate::lang::EngineError::zero_step(false)); }
        let back = order(heap, c, Val::int(0)).is_lt();
        let (lower, upper) = if back { (Val::int(-1), arith(heap, BinOp::Sub, n, Val::int(1))?) } else { (Val::int(0), n) };
        let bound = |heap: &mut HeapPool, x: Val, default: Val| -> Result<Val, VmErr> {
            if x.is_none() { return Ok(default); }
            let i = index(heap, x)?;
            if order(heap, i, Val::int(0)).is_lt() {
                // a negative bound clamps to the start where it counts from no end
                if !heap.policy.neg_index { return Ok(lower); }
                let j = arith(heap, BinOp::Add, i, n)?;
                return Ok(if order(heap, j, lower).is_lt() { lower } else { j });
            }
            Ok(if order(heap, i, upper).is_gt() { upper } else { i })
        };
        let (a, b) = if back { (bound(heap, start, upper)?, bound(heap, stop, lower)?) } else { (bound(heap, start, lower)?, bound(heap, stop, upper)?) };
        let (lo, hi) = (arith(heap, BinOp::Mul, a, st)?, arith(heap, BinOp::Mul, b, st)?);
        let (lo, hi, by) = (arith(heap, BinOp::Add, s, lo)?, arith(heap, BinOp::Add, s, hi)?, arith(heap, BinOp::Mul, st, c)?);
        let word = |heap: &HeapPool, x: Val| as_i128(x, heap).and_then(|i| i64::try_from(i).ok());
        match (word(heap, lo), word(heap, hi), word(heap, by)) {
            (Some(x), Some(y), Some(z)) => heap.alloc(HeapObj::Range(x, y, z)),
            _ => heap.alloc(HeapObj::BigRange(lo, hi, by)),
        }
    })())
}
