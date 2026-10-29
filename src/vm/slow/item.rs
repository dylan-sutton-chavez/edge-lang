use alloc::vec::Vec;

use crate::vm::VM;
pub(crate) use crate::value::host::{normalize_index, slice_bounds};
use core::cell::RefCell;
use crate::code::OpCode;
use crate::value::{Val, VmErr, HeapObj, HeapPool, DictMap, Host, cold_depth, eq_checked};
use crate::code::Ins;
use crate::lang::{Act, EngineError};


/* slice store or delete on any vector where an extended slice needs an exact count */
fn splice<T>(b: &mut Vec<T>, (s, e, st): (i64, i64, i64), items: Option<Vec<T>>) -> Result<(), VmErr> {
    if st == 1 {
        b.splice(s as usize..e.max(s) as usize, items.unwrap_or_default());
        return Ok(());
    }
    // selected positions sit `st` apart from `s` and strictly before `e`
    let picked = |k: i64| (if st > 0 { k < e && k >= s } else { k > e && k <= s }) && (k - s) % st == 0;
    let Some(items) = items else {
        let mut k = -1;
        b.retain(|_| { k += 1; !picked(k) });
        return Ok(());
    };
    let span = if st > 0 { (e - s + st - 1) / st } else { (s - e - st - 1) / -st };
    if items.len() as i64 != span.max(0) {
        return Err(EngineError::SliceSize { size: items.len() as u32, span: span.max(0) as u32 }.into());
    }
    for (k, v) in items.into_iter().enumerate() { b[(s + k as i64 * st) as usize] = v; }
    Ok(())
}


impl<'a> VM<'a> {

    pub(crate) fn get_item(&mut self) -> Result<(), VmErr> {
        let (obj, idx) = self.pop2()?;
        let v = self.item_value(obj, idx)?;
        self.push(v);
        Ok(())
    }

    /* `obj[idx]` with an in-range int index on a list or tuple needing no coercion */
    #[inline(always)]
    pub(crate) fn item_value(&mut self, obj: Val, idx: Val) -> Result<Val, VmErr> {
        if idx.is_int() && obj.is_heap() {
            let i = idx.as_int();
            let hit = match self.heap.try_get(obj) {
                Some(HeapObj::List(v)) => { let b = v.borrow(); b.get(normalize_index(i, b.len(), self.heap.policy)).copied() }
                Some(HeapObj::Tuple(v)) => v.get(normalize_index(i, v.len(), self.heap.policy)).copied(),
                _ => None,
            };
            if let Some(v) = hit { return Ok(v); }
        }
        self.get_item_of(obj, idx)
    }

    /* `obj[idx]` past the plain list and tuple indexes */
    #[inline(always)]
    fn get_item_of(&mut self, obj: Val, idx: Val) -> Result<Val, VmErr> {
        let idx = self.plain_index(obj, idx).unwrap_or(idx);
        if let Some(p) = self.heap.dict_cell(obj) {
            let found = p.borrow().get(&idx, &self.heap).copied();
            let Some(v) = found else { self.require_key(idx)?; return Err(self.key_error(idx)) };
            return Ok(v);
        }
        self.get_item_builtin(obj, idx)
    }

    /* a key error holding the missing key itself whose text is the key's repr */
    pub(crate) fn key_error(&mut self, key: Val) -> VmErr {
        let msg = self.shown(key, crate::lang::Mode::Repr);
        match self.heap.engine_error(crate::lang::ErrKind::Key, alloc::vec![key]) {
            Ok(exc) => { self.pending.exc_val = Some(exc); VmErr::Of(crate::lang::ErrKind::Key, msg) }
            Err(e) => e,
        }
    }













    pub fn store_item(&mut self) -> Result<(), VmErr> {
        let value = self.pop()?;
        let (cont, idx_val) = self.pop2()?;
        self.item_store(cont, idx_val, value)
    }

    /* `cont[idx_val] = value` with an in-range int index on a list needing no coercion */
    pub(crate) fn item_store(&mut self, cont: Val, idx_val: Val, value: Val) -> Result<(), VmErr> {
        if idx_val.is_int() && cont.is_heap() && let Some(HeapObj::List(v)) = self.heap.try_get(cont) {
            let mut b = v.borrow_mut();
            let ui = normalize_index(idx_val.as_int(), b.len(), self.heap.policy);
            if ui < b.len() {
                b[ui] = value;
                return Ok(());
            }
        }
        if !cont.is_heap() { return Err(EngineError::cannot(Act::AssignItem, None)); }
        let idx_val = self.plain_index(cont, idx_val).unwrap_or(idx_val);
        self.store_item_builtin(cont, idx_val, value)
    }

    /* builtin item assignment for callers without a bytecode frame and for `store_item` */
    pub fn store_item_builtin(&mut self, cont: Val, idx_val: Val, value: Val) -> Result<(), VmErr> {
        if !cont.is_heap() { return Err(EngineError::cannot(Act::AssignItem, None)); }
        // a slice store splices the iterable's items in place but a map keys by it
        if let Some([start, stop, step]) = self.heap.slice_of(idx_val) && self.heap.dict_cell(cont).is_none() {
            let new_items = self.extract_iter(value)?;
            return self.store_slice(cont, start, stop, step, Some(new_items));
        }
        // rejects mutable keys before borrowing the container mutably below
        if matches!(self.heap.get(cont), HeapObj::Dict(_)) {
            self.require_key(idx_val)?;
        }
        match self.heap.get(cont) {
            HeapObj::List(v) => {
                if !idx_val.is_int() { return Err(EngineError::bad_index(cont, idx_val)); }
                let mut b = v.borrow_mut();
                let i = idx_val.as_int();
                let ui = normalize_index(i, b.len(), self.heap.policy);
                if ui >= b.len() { return Err(EngineError::out_of_range(cont, true)); }
                b[ui] = value;
            }
            HeapObj::Dict(p) => p.borrow_mut().insert(idx_val, value, &self.heap),
            HeapObj::ByteArray(_) => {
                if !idx_val.is_int() { return Err(EngineError::bad_index(cont, idx_val)); }
                let byte = self.byte_value(value)?;
                let policy = self.heap.policy;
                let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(cont) else { return Ok(()) };
                let ui = normalize_index(idx_val.as_int(), b.len(), policy);
                if ui >= b.len() { return Err(EngineError::out_of_range(cont, true)); }
                b[ui] = byte;
            }
            _ => return Err(EngineError::cannot(Act::AssignItem, Some(cont))),
        }
        Ok(())
    }

    pub fn del_item(&mut self, cont: Val, idx_val: Val) -> Result<(), VmErr> {
        if !cont.is_heap() { return Err(EngineError::cannot(Act::DeleteItem, None)); }
        let idx_val = self.plain_index(cont, idx_val).unwrap_or(idx_val);
        // a map deletes a slice as the key it is
        if let Some([start, stop, step]) = self.heap.slice_of(idx_val) && self.heap.dict_cell(cont).is_none() {
            return self.store_slice(cont, start, stop, step, None);
        }
        match self.heap.get(cont) {
            HeapObj::List(v) => {
                if !idx_val.is_int() { return Err(EngineError::bad_index(cont, idx_val)); }
                let mut b = v.borrow_mut();
                let ui = normalize_index(idx_val.as_int(), b.len(), self.heap.policy);
                if ui >= b.len() { return Err(EngineError::out_of_range(cont, true)); }
                b.remove(ui);
            }
            HeapObj::Dict(p) => {
                if p.borrow_mut().remove(&idx_val, &self.heap).is_none() {
                    self.require_key(idx_val)?;
                    return Err(self.key_error(idx_val));
                }
            }
            HeapObj::ByteArray(_) => {
                if !idx_val.is_int() { return Err(EngineError::bad_index(cont, idx_val)); }
                let policy = self.heap.policy;
                let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(cont) else { return Ok(()) };
                let ui = normalize_index(idx_val.as_int(), b.len(), policy);
                if ui >= b.len() { return Err(EngineError::out_of_range(cont, true)); }
                b.remove(ui);
            }
            _ => return Err(EngineError::cannot(Act::DeleteItem, Some(cont))),
        }
        Ok(())
    }

    /* the `n` registers of a cut's row from `b` with nil past them */
    pub(crate) fn cut_row(&self, chunk: &crate::syntax::chunk::SSAChunk, b: u16, n: u16) -> Result<[Val; 5], VmErr> {
        let (mut row, n, at) = ([Val::none(); 5], usize::from(n.min(5)), self.base + b as usize);
        row[..n].copy_from_slice(&self.regs[at..at + n]);
        // an unbound register reads as its own read does
        if row[..n].iter().any(|v| v.is_undef()) { row[..n].copy_from_slice(&self.reg_row(chunk, b, n)?); }
        Ok(row)
    }

    /* a cut read or delete or a store whose row leads with its value */
    #[inline(never)]
    pub(crate) fn cut_op(&mut self, op: OpCode, row: [Val; 5], retaken: Val, rip: usize) -> Result<Option<Val>, VmErr> {
        let at = usize::from(op == OpCode::StoreCut);
        let (obj, [start, stop, step]) = (row[at], [row[at + 1], row[at + 2], row[at + 3]]);
        // a map reads stores and deletes the slice the bounds make as its key
        if self.heap.dict_cell(obj).is_some() {
            let key = self.heap.new_slice([start, stop, step])?;
            return match op {
                OpCode::Cut => self.getitem_val(obj, key).map(Some),
                OpCode::StoreCut => self.store_item_builtin(obj, key, row[0]).map(|()| None),
                _ => self.del_item(obj, key).map(|()| None),
            };
        }
        // a cut paces the collector once as the slice it no longer builds did
        if retaken.is_undef() { self.heap.pace(); }
        if op == OpCode::Cut { return self.slice_val(obj, start, stop, step).map(Some); }
        let items = if op == OpCode::DelCut { None } else {
            let v = if !retaken.is_undef() { retaken } else if self.may_job(row[0]) && self.steps_user(row[0]) {
                match self.retake_job(row[0], rip)? { Some(v) => v, None => return Ok(None) }
            } else { row[0] };
            Some(v)
        };
        if !obj.is_heap() { return Err(EngineError::cannot(if items.is_some() { Act::AssignItem } else { Act::DeleteItem }, None)); }
        let items = match items { Some(v) => Some(self.extract_iter(v)?), None => None };
        self.store_slice(obj, start, stop, step, items).map(|_| None)
    }

    /* slice store or with no items delete where an extended slice needs an exact count */
    fn store_slice(&mut self, cont: Val, start: Val, stop: Val, step: Val, items: Option<Vec<Val>>) -> Result<(), VmErr> {
        let clip = |v: Val| crate::value::wide::clip_index(&self.heap, v);
        let (start, stop, step) = (clip(start), clip(stop), clip(step));
        if matches!(self.heap.get(cont), HeapObj::ByteArray(_)) {
            let bytes = match items { Some(items) => Some(items.into_iter().map(|v| self.byte_value(v)).collect::<Result<Vec<u8>, VmErr>>()?), None => None };
            let neg = self.heap.policy.neg_index;
            let Some(HeapObj::ByteArray(b)) = self.heap.try_get_mut(cont) else { return Ok(()) };
            let bounds = slice_bounds(start, stop, step, b.len() as i64, neg)?;
            return splice(b, bounds, bytes);
        }
        let HeapObj::List(rc) = self.heap.get(cont) else {
            return Err(EngineError::cannot(Act::AssignSlice, Some(cont)));
        };
        let mut b = rc.borrow_mut();
        let bounds = slice_bounds(start, stop, step, b.len() as i64, self.heap.policy.neg_index)?;
        splice(&mut b, bounds, items)
    }


}

/* index `i` into `len` items counting a negative one from the end only with `neg` */
#[inline(always)]
pub(crate) fn index_of(i: i64, len: usize, neg: bool) -> Option<usize> {
    let j = if i < 0 && neg { i + len as i64 } else { i };
    (0..len as i64).contains(&j).then_some(j as usize)
}

/* whether `k` hashes without user code as an inline value or a str */
#[inline(always)]
fn plain_key_in(heap: &HeapPool, k: Val) -> bool { !k.is_heap() && !k.is_undef() || matches!(heap.try_get(k), Some(HeapObj::Str(_))) }

/* whether k hashes unchecked as a plain key or a tuple of them */
#[inline]
pub(crate) fn sure_key(heap: &HeapPool, k: Val) -> bool {
    plain_key_in(heap, k) || matches!(heap.try_get(k), Some(HeapObj::Tuple(items)) if items.iter().all(|&i| plain_key_in(heap, i)))
}

/* a plain dict's value under a plain key or None so the slow path raises */
#[inline(never)]
pub(crate) fn dict_get(heap: &HeapPool, d: &RefCell<DictMap>, k: Val) -> Option<Val> {
    if !plain_key_in(heap, k) { return None; }
    d.borrow().get(&k, heap).copied()
}

/* stores a plain key's value in a plain dict or false for the slow path */
#[inline(never)]
pub(crate) fn dict_set(heap: &HeapPool, d: &RefCell<DictMap>, k: Val, v: Val) -> bool {
    if !plain_key_in(heap, k) { return false; }
    d.borrow_mut().insert(k, v, heap);
    true
}

impl<'a> VM<'a> {

    /* a list tuple or string cut by slice k and none for the handler */
    #[inline(never)]
    pub(crate) fn cut(&mut self, o: Val, k: Val) -> Option<Val> {
        let [start, stop, step] = self.heap.slice_of(k)?;
        if !matches!(self.heap.try_get(o), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Str(_))) { return None; }
        self.slice_val(o, start, stop, step).ok()
    }

    /* whether `k` hashes by its kind alone so a fast path skips the hashability check */
    #[inline(always)]
    pub(crate) fn plain_key(&self, k: Val) -> bool { plain_key_in(&self.heap, k) }

    /* a plain list or tuple or dict subscript fast path giving false on a miss */
    #[inline(never)]
    pub(crate) fn item_fast(&mut self, ins: Ins) -> bool {
        let r = &self.regs[self.base..];
        if ins.op == OpCode::GetItemR {
            let (o, k) = (r[ins.b as usize], r[ins.c as usize]);
            let hit = match self.heap.try_get(o) {
                Some(HeapObj::List(v)) if k.is_int() => { let b = v.borrow(); index_of(k.as_int(), b.len(), k.as_int() >= 0 || self.heap.policy.neg_index).map(|j| b[j]) }
                Some(HeapObj::Tuple(v)) if k.is_int() => index_of(k.as_int(), v.len(), k.as_int() >= 0 || self.heap.policy.neg_index).map(|j| v[j]),
                Some(HeapObj::Dict(d)) => dict_get(&self.heap, d, k),
                _ => None,
            };
            let Some(v) = hit else { return false };
            self.regs[self.base + ins.a as usize] = v;
            return true;
        }
        let (o, k, v) = (r[ins.a as usize], r[ins.b as usize], r[ins.c as usize]);
        match self.heap.try_get(o) {
            Some(HeapObj::List(l)) if k.is_int() => {
                let mut b = l.borrow_mut();
                let Some(j) = index_of(k.as_int(), b.len(), k.as_int() >= 0 || self.heap.policy.neg_index) else { return false };
                b[j] = v;
            }
            Some(HeapObj::Dict(d)) => if !dict_set(&self.heap, d, k, v) { return false },
            _ => return false,
        }
        true
    }

    /* the key or length or cell probe fast path giving false on a miss */
    #[inline(never)]
    pub(crate) fn heap_fast(&mut self, ins: Ins) -> bool {
        match ins.op {
            OpCode::InR | OpCode::NotInR => {
                let (item, container) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                // a plain key probes a plain dict or set and anything else runs the protocol
                if !self.plain_key(item) { return false; }
                let hit = match self.heap.try_get(container) {
                    Some(HeapObj::Set(rc)) => rc.borrow().contains(item, &self.heap),
                    Some(HeapObj::Dict(rc)) => rc.borrow().contains_key(&item, &self.heap),
                    _ => return false,
                };
                self.regs[self.base + ins.a as usize] = Val::bool(hit == (ins.op == OpCode::InR));
            }
            OpCode::LenR => {
                if self.native_rebound(ins.c) { return false; }
                let Some(n) = self.heap.plain_len(self.regs[self.base + ins.b as usize]) else { return false };
                self.regs[self.base + ins.a as usize] = Val::int(n as i64);
            }
            OpCode::LoadCellR => {
                let Some(&HeapObj::Cell(v)) = self.heap.try_get(self.regs[self.base + ins.b as usize]) else { return false };
                if v.is_undef() { return false; }
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::StoreCellR => {
                let v = self.regs[self.base + ins.b as usize];
                if v.is_undef() { return false; }
                let Some(HeapObj::Cell(inner)) = self.heap.try_get_mut(self.regs[self.base + ins.a as usize]) else { return false };
                *inner = v;
            }
            _ => return false,
        }
        true
    }

    /* membership by content equality over containers views iterators and generators */
    pub(crate) fn contains_op(&mut self, container: Val, item: Val) -> Result<bool, VmErr> {
        // a dict or set probes by hash
        let probe = match self.heap.try_get(container) {
            Some(HeapObj::Dict(rc)) => Some((rc.borrow().contains_key(&item, &self.heap), false)),
            Some(HeapObj::Set(rc)) => Some((rc.borrow().contains(item, &self.heap), true)),
            Some(HeapObj::List(_) | HeapObj::Tuple(_)) => return self.seq_contains(container, item),
            _ => None,
        };
        // a miss still rejects an unhashable probe though a set probing a set finds nothing
        if let Some((hit, set)) = probe {
            if !hit { if set { self.require_set_probe(item)?; } else { self.require_key(item)?; } }
            return Ok(hit);
        }
        self.contains_rest(container, item)
    }

    /* `contains_op` of an iterator or a generator or anything else */
    fn contains_rest(&mut self, container: Val, item: Val) -> Result<bool, VmErr> {
        // an iterator steps until a match and leaves the rest unconsumed
        if matches!(self.heap.try_get(container), Some(HeapObj::Iter(..))) {
            return self.with_roots([container, item, Val::none()], |vm| loop {
                vm.charge_step()?;
                let Some(v) = vm.iter_step(container)? else { return Ok(false) };
                if let Some(slot) = vm.temp_roots.last_mut() { *slot = v; }
                if vm.member_eq(item, v)? { return Ok(true); }
            });
        }
        if matches!(self.heap.try_get(container), Some(HeapObj::Coroutine(..))) {
            return self.with_roots([container, item, Val::none()], |vm| loop {
                vm.charge_step()?;
                let v = vm.step_coroutine(container)?;
                if !vm.yielded { return Ok(false); }
                vm.yielded = false;
                if let Some(slot) = vm.temp_roots.last_mut() { *slot = v; }
                if vm.member_eq(item, v)? { return Ok(true); }
            });
        }
        self.contains(container, item)
    }

    /* `x in seq` read live by identity then content equality */
    fn seq_contains(&mut self, seq: Val, item: Val) -> Result<bool, VmErr> {
        // the scan counts the items it may read as native work
        let scan = |items: &[Val], heap: &HeapPool| -> Result<bool, VmErr> {
            heap.spend(items.len());
            for &x in items {
                // distinct bits of two inline ints are distinct values
                if x.0 == item.0 { return Ok(true); }
                if x.is_int() && item.is_int() { continue; }
                match eq_checked(x, item, heap, 0) { Some(true) => return Ok(true), Some(false) => {}, None => return Err(cold_depth()) }
            }
            Ok(false)
        };
        match self.heap.try_get(seq) {
            Some(HeapObj::List(rc)) => scan(&rc.borrow(), &self.heap),
            Some(HeapObj::Tuple(t)) => scan(t, &self.heap),
            _ => Ok(false),
        }
    }
}
