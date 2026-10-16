use super::VM;
use super::types::*;
use crate::parser::SSAChunk;
use crate::parser::types::OpCode;

use alloc::vec::Vec;

impl<'a> VM<'a> {
    /* `a == b`, the content compare first and the user dunders only where it cannot settle. */
    #[inline]
    pub(crate) fn values_eq(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        match eq_checked(a, b, &self.heap, 0) {
            Some(r) => Ok(r),
            None => self.with_roots([a, b], |vm| vm.rich_eq(a, b, chunk, 0)),
        }
    }

    /* Equality of a container member, identity first as in `x is e or x == e`. */
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
        let seqs = match (self.heap.try_get(a), self.heap.try_get(b)) {
            (Some(HeapObj::List(x)), Some(HeapObj::List(y))) => Some((x.borrow().clone(), y.borrow().clone())),
            (Some(HeapObj::Tuple(x)), Some(HeapObj::Tuple(y))) => Some((x.clone(), y.clone())),
            _ => None,
        };
        if let Some((xs, ys)) = seqs {
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
        let seqs = match (self.heap.try_get(a), self.heap.try_get(b)) {
            (Some(HeapObj::List(x)), Some(HeapObj::List(y))) => (x.borrow().clone(), y.borrow().clone()),
            (Some(HeapObj::Tuple(x)), Some(HeapObj::Tuple(y))) => (x.clone(), y.clone()),
            _ => return self.lt_vals(a, b),
        };
        let (xs, ys) = seqs;
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
