use super::VM;
use super::types::*;
use crate::parser::SSAChunk;
use crate::parser::types::OpCode;

use alloc::vec::Vec;

/* Where a walk of two builtin lists or tuples stops without user code, at an instance's pair, a differing pair, a pair needing its own walk, or the shorter's end. */
pub(crate) enum PairScan { Dunder(usize), Differ(usize), Deep(usize), Ended }

impl<'a> VM<'a> {
    /* Walks the pairs of `a` and `b` from `from` as `PairScan` says, items read as they are now, None unless both are builtin lists or both builtin tuples. */
    pub(crate) fn pair_scan(&self, a: Val, b: Val, from: usize) -> Option<PairScan> {
        let (xs, ys): (&[Val], &[Val]) = match (self.heap.plain(a)?, self.heap.plain(b)?) {
            (HeapObj::Tuple(x), HeapObj::Tuple(y)) => (x, y),
            (HeapObj::List(x), HeapObj::List(y)) => return Some(scan_pairs(&x.borrow(), &y.borrow(), from, &self.heap)),
            _ => return None,
        };
        Some(scan_pairs(xs, ys, from, &self.heap))
    }

    /* `a == b`, the content compare first and the user dunders only where it cannot settle. */
    #[inline]
    pub(crate) fn values_eq(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        match eq_checked(a, b, &self.heap, 0) {
            Some(r) => Ok(r),
            None => self.with_roots([a, b], |vm| vm.rich_eq(a, b, chunk, 0)),
        }
    }

    /* `values_eq` for the op at `rip`, which runs again as `rerun` says once a dunder the walk of two small containers calls, a frame of the loop, comes back. */
    pub(crate) fn values_eq_op(&mut self, a: Val, b: Val, chunk: &SSAChunk, rip: usize, rerun: super::redo::Rerun) -> Result<bool, VmErr> {
        // Run again, the op carries on its walk, what its calls changed read as it goes.
        match if self.redo.keep { None } else { eq_checked(a, b, &self.heap, 0) } {
            Some(r) => Ok(r),
            None => {
                let armed = self.heap.users && self.small_pair(a, b) && self.redo_late(rip, rerun, self.stack.len() + 2, &[a, b]);
                let r = self.with_roots([a, b], |vm| if armed { vm.walk_eq(a, b, chunk) } else { vm.rich_eq(a, b, chunk, 0) });
                self.redo_done(armed);
                r
            }
        }
    }

    /* `values_lt` for the op at `rip`, as `values_eq_op` is, `ops` its operands as it took them. */
    pub(crate) fn values_lt_op(&mut self, a: Val, b: Val, chunk: &SSAChunk, rip: usize, rerun: super::redo::Rerun, ops: [Val; 2]) -> Result<bool, VmErr> {
        // Run again, the op carries on its walk.
        let r = if self.redo.keep && self.is_sequence(a) && self.is_sequence(b) { Err(VmErr::TypeMsg(Default::default())) } else { self.lt_vals(a, b) };
        match r {
            Err(VmErr::TypeMsg(_)) if self.is_sequence(a) && self.is_sequence(b) => {
                let armed = self.heap.users && self.small_pair(a, b) && self.redo_late(rip, rerun, self.stack.len() + 2, &ops);
                let r = self.with_roots([a, b], |vm| if armed { vm.walk_lt(a, b, chunk) } else { vm.rich_lt(a, b, chunk, 0) });
                self.redo_done(armed);
                r
            }
            r => r,
        }
    }

    /* The items of two lists or two tuples, None for any other pair. */
    fn seq_pair(&self, a: Val, b: Val) -> Option<(Vec<Val>, Vec<Val>)> {
        match (self.heap.try_get(a), self.heap.try_get(b)) {
            (Some(HeapObj::List(x)), Some(HeapObj::List(y))) => Some((x.borrow().clone(), y.borrow().clone())),
            (Some(HeapObj::Tuple(x)), Some(HeapObj::Tuple(y))) => Some((x.clone(), y.clone())),
            _ => None,
        }
    }

    /* Whether `a` and `b` are both builtin lists or both builtin tuples, no subclass of them, whose dunders an op asks first. */
    fn plain_seqs(&self, a: Val, b: Val) -> bool {
        matches!((self.heap.plain(a), self.heap.plain(b)), (Some(HeapObj::List(_)), Some(HeapObj::List(_))) | (Some(HeapObj::Tuple(_)), Some(HeapObj::Tuple(_))))
    }

    /* Item `k` of the list or tuple `v` as it is now, None past its end. */
    fn item_at(&self, v: Val, k: usize) -> Option<Val> {
        match self.heap.try_get(v) {
            Some(HeapObj::List(x)) => x.borrow().get(k).copied(),
            Some(HeapObj::Tuple(x)) => x.get(k).copied(),
            _ => None,
        }
    }

    /* The length of the list or tuple `v` as it is now. */
    fn seq_len(&self, v: Val) -> usize {
        match self.heap.try_get(v) { Some(HeapObj::List(x)) => x.borrow().len(), Some(HeapObj::Tuple(x)) => x.len(), _ => 0 }
    }

    /* The first unequal pair from `k` of two lists or tuples, read at each step as CPython does, None when the shorter ends first, a rerun going on past pairs found equal. */
    fn walk_from(&mut self, a: Val, b: Val, mut k: usize, chunk: &SSAChunk) -> Result<Option<(Val, Val)>, VmErr> {
        while let (Some(x), Some(y)) = (self.item_at(a, k), self.item_at(b, k)) {
            if !self.with_roots([x, y], |vm| vm.member_eq_d(x, y, chunk, 1))? { return Ok(Some((x, y))); }
            k += 1;
            self.redo_walked(k);
        }
        Ok(None)
    }

    /* `rich_eq` of the op armed now, the lengths compared once as the walk begins. */
    fn walk_eq(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if !self.plain_seqs(a, b) { return self.rich_eq(a, b, chunk, 0); }
        if self.seq_len(a) != self.seq_len(b) && !self.redo_replays() { return Ok(false); }
        let from = self.redo_walk();
        Ok(self.walk_from(a, b, from, chunk)?.is_none() && self.seq_len(a) == self.seq_len(b))
    }

    /* `rich_lt` of the op armed now, decided by the first pair that differs, else the lengths. */
    fn walk_lt(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if !self.plain_seqs(a, b) { return self.rich_lt(a, b, chunk, 0); }
        let from = self.redo_walk();
        match self.walk_from(a, b, from, chunk)? {
            Some((x, y)) => self.with_roots([x, y], |vm| vm.rich_lt(x, y, chunk, 1)),
            None => Ok(self.seq_len(a) < self.seq_len(b)),
        }
    }

    /* Whether two containers are short enough for a walking op to run again after each dunder it calls, taking earlier calls from the log, as a longer one runs them in place. */
    fn small_pair(&self, a: Val, b: Val) -> bool {
        let len = |v: Val| match self.heap.try_get(v) {
            Some(HeapObj::List(x)) => x.borrow().len(),
            Some(HeapObj::Tuple(x)) => x.len(),
            Some(HeapObj::Dict(x)) => x.borrow().len(),
            _ => usize::MAX,
        };
        len(a).max(len(b)) <= super::redo::WALK
    }

    /* Equality of a container member, identity first as in `x is e or x == e`, inlined into the walks that call it per item. */
    #[inline]
    pub(crate) fn member_eq(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if a.0 == b.0 { return Ok(true); }
        self.values_eq(a, b, chunk)
    }

    fn rich_eq(&mut self, a: Val, b: Val, chunk: &SSAChunk, depth: usize) -> Result<bool, VmErr> {
        // A distinct pair past the cap is a comparison too deep to finish.
        if depth > EQ_DEPTH_MAX { return if a.0 == b.0 { Ok(true) } else { Err(cold_depth()) }; }
        let inst = |vm: &Self, v: Val| matches!(vm.heap.try_get(v), Some(HeapObj::Instance(..)));
        if inst(self, a) || inst(self, b) {
            return Ok(match self.try_compare_dunder(OpCode::Eq, a, b, chunk)? { Some(r) => self.truthy(r), None => a.0 == b.0 });
        }
        if let Some((xs, ys)) = self.seq_pair(a, b) {
            if xs.len() != ys.len() { return Ok(false); }
            // A `__eq__` can shrink either list, so the items it walks stay rooted.
            return self.with_roots(xs.iter().chain(&ys).copied(), |vm| {
                for (&x, &y) in xs.iter().zip(&ys) {
                    if !vm.member_eq_d(x, y, chunk, depth + 1)? { return Ok(false); }
                }
                Ok(true)
            });
        }
        // A key finds its pair by hash, so only the values may need a user `__eq__`.
        let dict_pairs = match (self.heap.try_get(a), self.heap.try_get(b)) {
            (Some(HeapObj::Dict(x) | HeapObj::FrozenDict(x)), Some(HeapObj::Dict(y) | HeapObj::FrozenDict(y))) => {
                let (x, y) = (x.borrow(), y.borrow());
                if x.len() != y.len() { return Ok(false); }
                let Some(pairs) = x.iter().map(|(k, v)| y.get(&k, &self.heap).map(|&w| (v, w))).collect::<Option<Vec<_>>>() else { return Ok(false) };
                Some(pairs)
            }
            _ => None,
        };
        if let Some(pairs) = dict_pairs {
            return self.with_roots(pairs.iter().flat_map(|&(v, w)| [v, w]), |vm| {
                for &(v, w) in &pairs {
                    if !vm.member_eq_d(v, w, chunk, depth + 1)? { return Ok(false); }
                }
                Ok(true)
            });
        }
        Ok(eq_vals_with_heap(a, b, &self.heap))
    }

    fn member_eq_d(&mut self, a: Val, b: Val, chunk: &SSAChunk, depth: usize) -> Result<bool, VmErr> {
        if a.0 == b.0 { return Ok(true); }
        match eq_checked(a, b, &self.heap, depth) { Some(r) => Ok(r), None => self.rich_eq(a, b, chunk, depth) }
    }

    /* `a < b`, sequences holding instances compare their items through the user dunders. */
    pub(crate) fn values_lt(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        match self.lt_vals(a, b) {
            Err(VmErr::TypeMsg(_)) if self.is_sequence(a) && self.is_sequence(b) => self.with_roots([a, b], |vm| vm.rich_lt(a, b, chunk, 0)),
            r => r,
        }
    }

    fn is_sequence(&self, v: Val) -> bool { matches!(self.heap.try_get(v), Some(HeapObj::List(_) | HeapObj::Tuple(_))) }

    fn rich_lt(&mut self, a: Val, b: Val, chunk: &SSAChunk, depth: usize) -> Result<bool, VmErr> {
        if depth > EQ_DEPTH_MAX { return Err(cold_depth()); }
        if let Some(r) = self.try_compare_dunder(OpCode::Lt, a, b, chunk)? { return Ok(self.truthy(r)); }
        let Some((xs, ys)) = self.seq_pair(a, b) else { return self.lt_vals(a, b) };
        // The first pair that differs decides, as for the builtin sequences.
        self.with_roots(xs.iter().chain(&ys).copied(), |vm| {
            for (&x, &y) in xs.iter().zip(&ys) {
                if vm.member_eq_d(x, y, chunk, depth + 1)? { continue; }
                return vm.rich_lt(x, y, chunk, depth + 1);
            }
            Ok(xs.len() < ys.len())
        })
    }

    /* A set of `items`, each one hashable. */
    pub(crate) fn valset_of(&mut self, items: &[Val]) -> Result<ValSet, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; self.keyed(v)?; }
        Ok(ValSet::from_vals(items, &self.heap))
    }

    /* A dict of `pairs` in order, a later equal key overwriting the value and keeping the first key. */
    pub(crate) fn dictmap_of(&mut self, pairs: Vec<(Val, Val)>) -> Result<DictMap, VmErr> {
        for &(k, _) in pairs.iter().filter(|p| p.0.is_heap()) { self.require_key(k)?; self.keyed(k)?; }
        Ok(DictMap::from_pairs(pairs, &self.heap))
    }

    /* A dict of the key and value pairs laid flat in `flat`, as a display builds it. */
    pub(crate) fn dictmap_of_flat(&mut self, flat: &[Val]) -> Result<DictMap, VmErr> {
        let mut dm = DictMap::with_capacity(flat.len() / 2);
        for p in flat.as_chunks::<2>().0 {
            if p[0].is_heap() { self.require_key(p[0])?; self.keyed(p[0])?; }
            dm.insert(p[0], p[1], &self.heap);
        }
        Ok(dm)
    }

    /* `{*src}` into set `acc`. */
    pub(crate) fn spread_into(&mut self, acc: Val, src: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        let items: Vec<Val> = match self.heap.try_get(src) {
            Some(HeapObj::Set(rc)) => rc.borrow().iter().copied().collect(),
            Some(HeapObj::FrozenSet(rc)) => rc.iter().copied().collect(),
            _ => self.iterable_items(src, chunk)?,
        };
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; self.keyed(v)?; }
        if let Some(HeapObj::Set(rc)) = self.heap.try_get(acc) { let mut s = rc.borrow_mut(); for v in items { s.insert(v, &self.heap); } }
        Ok(())
    }

    /* `{**src}` merged into dict `acc`, later keys overwriting earlier values. */
    pub(crate) fn dict_spread_into(&mut self, acc: Val, src: Val) -> Result<(), VmErr> {
        let entries: Vec<(Val, Val)> = match self.heap.try_get(src) {
            Some(HeapObj::Dict(rc) | HeapObj::FrozenDict(rc)) => rc.borrow().iter().collect(),
            _ => return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(src), "' object is not a mapping"))),
        };
        if let Some(HeapObj::Dict(rc)) = self.heap.try_get(acc) { let mut d = rc.borrow_mut(); for (k, v) in entries { d.insert(k, v, &self.heap); } }
        Ok(())
    }
}

/* `pair_scan` over the items `xs` and `ys`. */
fn scan_pairs(xs: &[Val], ys: &[Val], from: usize, heap: &crate::value::HeapPool) -> PairScan {
    for k in from..xs.len().min(ys.len()) {
        let (x, y) = (xs[k], ys[k]);
        if x.0 == y.0 { continue; }
        if matches!(heap.try_get(x), Some(HeapObj::Instance(..))) || matches!(heap.try_get(y), Some(HeapObj::Instance(..))) { return PairScan::Dunder(k); }
        match eq_checked(x, y, heap, 1) { Some(true) => {}, Some(false) => return PairScan::Differ(k), None => return PairScan::Deep(k) }
    }
    PairScan::Ended
}
