use crate::s;

use alloc::{vec, vec::Vec};

use super::super::VM;
use super::super::types::*;
use crate::parser::{OpCode, SSAChunk};


impl<'a> VM<'a> {

    /* A builtin iterator named `name` over `frame`. */
    fn alloc_iterator(&mut self, frame: IterFrame, name: &'static str) -> Result<Val, VmErr> {
        self.heap.alloc(HeapObj::Iter(alloc::rc::Rc::new(core::cell::RefCell::new(frame)), name))
    }

    /* Pushes a builtin iterator named `name` over `frame`. */
    pub(crate) fn push_iterator(&mut self, frame: IterFrame, name: &'static str) -> Result<(), VmErr> {
        let it = self.alloc_iterator(frame, name)?;
        self.push(it);
        Ok(())
    }

    /* What a dict view shows now, an items view pairing each key with its value. */
    pub(crate) fn view_items(&mut self, d: Val, kind: View) -> Result<Vec<Val>, VmErr> {
        let entries: Vec<(Val, Val)> = match self.heap.try_get(d) { Some(HeapObj::Dict(m)) => m.borrow().iter().collect(), _ => Vec::new() };
        match kind {
            View::Keys => Ok(entries.into_iter().map(|(k, _)| k).collect()),
            View::Values => Ok(entries.into_iter().map(|(_, v)| v).collect()),
            View::Items => entries.into_iter().map(|(k, v)| self.heap.alloc(HeapObj::Tuple(vec![k, v]))).collect(),
        }
    }

    /* An iterator over `o`, the one `iter(o)` gives. */
    pub(crate) fn iter_of(&mut self, o: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let ascii = self.heap.str_is_ascii(o);
        if self.heap.classed(o).is_some() && let Some(it) = self.try_call_dunder(o, "__iter__", &[], chunk)? { return Ok(it); }
        let name = match self.heap.try_get(o) {
            Some(HeapObj::Iter(..) | HeapObj::Coroutine(..)) => return Ok(o),
            Some(HeapObj::Instance(..)) => return self.try_call_dunder(o, "__iter__", &[], chunk)?.ok_or_else(|| self.not_iterable(o)),
            Some(HeapObj::List(rc)) => { let rc = rc.clone(); return self.alloc_iterator(IterFrame::List { rc, idx: 0 }, "list_iterator"); }
            Some(&HeapObj::Range(cur, end, step)) => return self.alloc_iterator(IterFrame::Range { cur, end, step }, "range_iterator"),
            Some(&HeapObj::BigRange(cur, end, step)) => return self.alloc_iterator(IterFrame::BigRange { cur, end, step }, "range_iterator"),
            Some(HeapObj::Dict(_)) => return self.alloc_iterator(self.dict_walk(o, View::Keys), "dict_keyiterator"),
            Some(&HeapObj::DictView(d, kind)) => {
                let name = match kind { View::Keys => "dict_keyiterator", View::Values => "dict_valueiterator", View::Items => "dict_itemiterator" };
                return self.alloc_iterator(self.dict_walk(d, kind), name);
            }
            Some(HeapObj::Tuple(_)) => "tuple_iterator",
            Some(HeapObj::Str(_)) => if ascii { "str_ascii_iterator" } else { "str_iterator" },
            Some(HeapObj::Set(_) | HeapObj::FrozenSet(_)) => "set_iterator",
            Some(HeapObj::Bytes(_)) => "bytes_iterator",
            _ => return Err(self.not_iterable(o)),
        };
        let items = self.extract_iter(o)?;
        let frame = self.watched(o, items);
        self.alloc_iterator(frame, name)
    }

    /* Turns the stack values from `at` into iterators in place, each rooted while the next is made. */
    fn iters_in_place(&mut self, at: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        for i in at..self.stack.len() {
            let it = self.iter_of(self.stack[i], chunk)?;
            self.stack[i] = it;
        }
        Ok(())
    }

    /* The next item of a builtin iterator, a generator or a `__next__` object, None once spent. */
    pub(crate) fn next_of(&mut self, it: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        match self.heap.try_get(it) {
            Some(HeapObj::Iter(..)) => self.iter_step(it),
            Some(HeapObj::Coroutine(..)) => {
                let v = self.with_roots([it], |vm| vm.resume_coroutine(it))?;
                Ok(core::mem::take(&mut self.yielded).then_some(v))
            }
            _ => {
                let before = self.pending.exc_val;
                match self.try_call_dunder(it, "__next__", &[], chunk) {
                    Ok(Some(v)) => Ok(Some(v)),
                    Ok(None) => Err(VmErr::TypeMsg(s!("'", str self.type_name(it), "' object is not an iterator"))),
                    Err(VmErr::Raised(m)) if m == "StopIteration" || m.starts_with("StopIteration:") => { self.swallow_stop(before); Ok(None) }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /* The next item of builtin iterator `it`, None once it is spent. */
    pub(crate) fn iter_step(&mut self, it: Val) -> Result<Option<Val>, VmErr> {
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return Ok(None); };
        let frame = frame.clone();
        if !matches!(&*frame.borrow(), IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. } | IterFrame::Call { .. }) {
            return frame.borrow_mut().next_item(&mut self.heap);
        }
        let state = frame.borrow().clone();
        // Nested lazy iterators recurse in Rust, so they count against the call depth.
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.depth += 1;
        let step = self.with_roots([it], |vm| vm.lazy_step(&frame, state));
        self.depth -= 1;
        step
    }

    /* One step of a frame calling user code, `state` read before a callback can step it again. */
    fn lazy_step(&mut self, frame: &alloc::rc::Rc<core::cell::RefCell<IterFrame>>, state: IterFrame) -> Result<Option<Val>, VmErr> {
        let chunk = self.chunk;
        match state {
            IterFrame::Map { f, its } => {
                self.push(f);
                if !self.push_next_of(&its, chunk)? { self.pop()?; return Ok(None); }
                self.exec_call(its.len() as u16, chunk)?;
                self.pop().map(Some)
            }
            IterFrame::Zip { its } => {
                let base = self.stack.len();
                if its.is_empty() || !self.push_next_of(&its, chunk)? { return Ok(None); }
                let items = self.stack.split_off(base);
                self.heap.alloc(HeapObj::Tuple(items)).map(Some)
            }
            IterFrame::Filter { f, it } => loop {
                self.charge_step()?;
                let Some(item) = self.next_of(it, chunk)? else { return Ok(None) };
                // The item waits on the stack, rooted while the predicate runs.
                self.push(item);
                let verdict = if f.is_none() { item } else {
                    self.push(f);
                    self.push(item);
                    self.exec_call(1, chunk)?;
                    self.pop()?
                };
                let keep = self.truthy_op(verdict, chunk)?;
                let item = self.pop()?;
                if keep { return Ok(Some(item)); }
            },
            IterFrame::Enumerate { it, n } => {
                let Some(item) = self.next_of(it, chunk)? else { return Ok(None) };
                self.push(item);
                let next = self.int_to_val(self.as_i128(n).and_then(|i| i.checked_add(1)))?;
                if let IterFrame::Enumerate { n: count, .. } = &mut *frame.borrow_mut() { *count = next; }
                let item = self.pop()?;
                self.heap.alloc(HeapObj::Tuple(vec![n, item])).map(Some)
            }
            IterFrame::Call { f, sentinel } => {
                self.push(f);
                self.exec_call(0, chunk)?;
                let v = self.pop()?;
                if !eq_member(v, sentinel, &self.heap) { return Ok(Some(v)); }
                // A callable iterator stays spent once it meets the sentinel.
                *frame.borrow_mut() = IterFrame::Seq { items: Vec::new().into(), idx: 0 };
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /* Pushes the next item of each iterator, false with nothing pushed once one is spent. */
    fn push_next_of(&mut self, its: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        let base = self.stack.len();
        for &it in its {
            match self.next_of(it, chunk)? {
                Some(v) => self.push(v),
                None => { self.stack.truncate(base); return Ok(false); }
            }
        }
        Ok(true)
    }

    pub fn call_len(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        // instance `__len__` takes precedence over built-in length rules.
        if let Some(r) = self.try_call_dunder(o, "__len__", &[], chunk)? {
            let n = if r.is_int() { r.as_int() as i128 }
            else if let Some(i) = crate::vm::types::as_i128(r, &self.heap) { i }
            else { return Err(cold_type("__len__ must return int")); };
            if n < 0 { return Err(cold_value("__len__() should return >= 0")); }
            let v = self.int_to_val(Some(n))?;
            self.push(v);
            return Ok(());
        }
        let n = self.builtin_len(o)?;
        let v = self.int_to_val(Some(n))?;
        self.push(v); Ok(())
    }

    /* `len(o)` of a builtin container, shared with the plugin ABI. */
    pub(crate) fn builtin_len(&mut self, o: Val) -> Result<i128, VmErr> {
        let ascii = self.heap.str_is_ascii(o);
        Ok(match self.heap.try_get(o) {
            Some(HeapObj::Str(s)) => (if ascii { s.len() } else { s.chars().count() }) as i128,
            Some(HeapObj::Bytes(b)) => b.len() as i128,
            Some(HeapObj::List(v)) => v.borrow().len() as i128,
            Some(HeapObj::Tuple(v)) => v.len() as i128,
            Some(HeapObj::Dict(v)) => v.borrow().len() as i128,
            Some(&HeapObj::DictView(d, _)) => return self.builtin_len(d),
            Some(HeapObj::Set(v)) => v.borrow().len() as i128,
            Some(HeapObj::FrozenSet(v)) => v.len() as i128,
            Some(&HeapObj::Range(_, _, 0)) => return Err(cold_value("range() arg 3 must not be zero")),
            Some(&HeapObj::Range(s, e, st)) => crate::vm::eq::range_len(s, e, st),
            Some(HeapObj::BigRange(..)) => crate::frontend::int::range_len(&self.heap, o).unwrap_or(Ok(0))?,
            _ => return Err(cold_type("object has no len()")),
        })
    }

    pub fn call_sorted(&mut self, reverse: bool, chunk: &SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        let mut items = self.extract_iter(o)?;
        self.sort_by_lt(&mut items, reverse, chunk)?;
        self.alloc_and_push_list(items)
    }

    /* sorted(iterable, key=fn, reverse=False), delegates to call_sorted when key is absent. */
    pub fn call_sorted_with_key(&mut self, key: Option<Val>, reverse: bool, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let key = match key {
            Some(k) if !k.is_none() => k,
            _ => return self.call_sorted(reverse, chunk),
        };
        let o = self.pop()?;
        let items = self.extract_iter(o)?;
        let sorted = self.sort_by_key(items, key, reverse, chunk)?;
        self.alloc_and_push_list(sorted)
    }

    /* list.sort(key=fn, reverse=False) in-place. Snapshots list before key calls so heap borrow ends before exec_call. */
    pub fn call_list_sort_keyed(&mut self, recv: Val, key: Option<Val>, reverse: bool, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let items = match self.heap.get(recv) {
            HeapObj::List(rc) => rc.borrow().clone(),
            _ => return Err(cold_type("sort: receiver is not a list")),
        };
        let result = if let Some(k) = key.filter(|k| !k.is_none()) {
            self.sort_by_key(items, k, reverse, chunk)?
        } else {
            let mut s = items;
            self.sort_by_lt(&mut s, reverse, chunk)?;
            s
        };
        let rc = match self.heap.get(recv) {
            HeapObj::List(rc) => rc.clone(),
            _ => return Err(cold_type("sort: receiver is not a list")),
        };
        *rc.borrow_mut() = result;
        self.mark_impure();
        self.push(Val::none());
        Ok(())
    }

    /* Decorate-sort-undecorate, applies key fn to each item, sorts by resulting keys, returns reordered items. */
    fn sort_by_key(&mut self, items: Vec<Val>, key: Val, reverse: bool, chunk: &crate::parser::SSAChunk) -> Result<Vec<Val>, VmErr> {
        let keys = self.call_rows(key, core::slice::from_ref(&items), items.len(), chunk)?;
        // Root both keys and items because a `__lt__` comparison can run user code that GCs.
        let order = self.sorted_order(&keys, &items, reverse, chunk)?;
        Ok(order.into_iter().map(|i| items[i]).collect())
    }

    /* Root the operands (comparators can run GC-triggering user code), stable-sort `keys` via `sort_lt`, and return the index permutation. `extra_roots` keeps caller-only values (e.g. the items in a keyed sort) alive across comparisons. First comparison error wins, later comparisons degrade to Equal. */
    fn sorted_order(&mut self, keys: &[Val], extra_roots: &[Val], reverse: bool, chunk: &SSAChunk) -> Result<Vec<usize>, VmErr> {
        if let Some(order) = plain_order(keys, reverse, &self.heap) { return Ok(order); }
        let roots_base = self.temp_roots.len();
        // Only an instance key runs user code that could collect.
        if self.any_instance(keys) { self.temp_roots.extend(keys.iter().chain(extra_roots).copied()); }
        let mut sort_err: Option<VmErr> = None;
        let order = Self::stable_sort_indices(keys.len(), |a, b| {
            if sort_err.is_some() { return core::cmp::Ordering::Equal; }
            // Descending compares flipped, so equal keys keep their order like an ascending sort.
            let (a, b) = if reverse { (b, a) } else { (a, b) };
            match self.sort_lt(keys[a], keys[b], chunk) {
                Ok(true) => core::cmp::Ordering::Less,
                Ok(false) => match self.sort_lt(keys[b], keys[a], chunk) {
                    Ok(true) => core::cmp::Ordering::Greater,
                    Ok(false) => core::cmp::Ordering::Equal,
                    Err(e) => { sort_err = Some(e); core::cmp::Ordering::Equal }
                },
                Err(e) => { sort_err = Some(e); core::cmp::Ordering::Equal }
            }
        });
        self.temp_roots.truncate(roots_base);
        if let Some(e) = sort_err { return Err(e); }
        Ok(order)
    }

    /* True when a comparison over `vals` can reach a user dunder. */
    pub(crate) fn any_instance(&self, vals: &[Val]) -> bool {
        vals.iter().any(|&v| matches!(self.heap.try_get(v), Some(HeapObj::Instance(..))))
    }

    // a < b via __lt__ when either side defines it, else the built-in comparison.
    #[inline]
    pub(crate) fn sort_lt(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if let Some(r) = self.try_compare_dunder(OpCode::Lt, a, b, chunk)? {
            return Ok(self.truthy(r));
        }
        self.values_lt(a, b, chunk)
    }

    /* In-place sort dispatching `__lt__`, roots items since a comparison can run user code that GCs. */
    pub(crate) fn sort_by_lt(&mut self, items: &mut [Val], reverse: bool, chunk: &SSAChunk) -> Result<(), VmErr> {
        let snapshot = items.to_vec();
        let order = self.sorted_order(&snapshot, &[], reverse, chunk)?;
        for (dst, &src) in order.iter().enumerate() { items[dst] = snapshot[src]; }
        Ok(())
    }

    /* Stable merge sort over `0..n`, unlike `slice::sort_by` it tolerates a non-total `cmp` (NaN keys) without aborting. */
    fn stable_sort_indices<F>(n: usize, mut cmp: F) -> Vec<usize>
    where F: FnMut(usize, usize) -> core::cmp::Ordering {
        let mut idx: Vec<usize> = (0..n).collect();
        if n < 2 { return idx; }
        let mut buf = idx.clone();
        let mut width = 1;
        while width < n {
            let mut lo = 0;
            while lo < n {
                let mid = (lo + width).min(n);
                let hi = (lo + 2 * width).min(n);
                let (mut a, mut b, mut k) = (lo, mid, lo);
                while a < mid && b < hi {
                    // Take the right run only on a strict Less, so equal keys keep input order (stable).
                    if cmp(idx[b], idx[a]) == core::cmp::Ordering::Less {
                        buf[k] = idx[b]; b += 1;
                    } else {
                        buf[k] = idx[a]; a += 1;
                    }
                    k += 1;
                }
                while a < mid { buf[k] = idx[a]; a += 1; k += 1; }
                while b < hi { buf[k] = idx[b]; b += 1; k += 1; }
                lo += 2 * width;
            }
            core::mem::swap(&mut idx, &mut buf);
            width *= 2;
        }
        idx
    }

    /* `reversed(seq)`, a list read live from its end, a range stepped backwards, other sequences read up front. */
    pub fn call_reversed(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        let name = match self.heap.try_get(o) {
            Some(HeapObj::List(rc)) => {
                let (rc, idx) = (rc.clone(), rc.borrow().len());
                return self.push_iterator(IterFrame::ListRev { rc, idx }, "list_reverseiterator");
            }
            Some(&HeapObj::Range(start, end, step)) => {
                let n = crate::vm::eq::range_len(start, end, step);
                let (last, stop) = ((start as i128) + (n - 1) * step as i128, start as i128 - step as i128);
                // A range whose backward bounds leave i64 reads its items up front.
                if let (true, Some(back), Ok(cur), Ok(end)) = (n > 0, step.checked_neg(), i64::try_from(last), i64::try_from(stop)) {
                    return self.push_iterator(IterFrame::Range { cur, end, step: back }, "range_iterator");
                }
                "range_iterator"
            }
            Some(HeapObj::BigRange(..)) => {
                let (cur, end, step) = crate::frontend::int::range_reversed(&mut self.heap, o).unwrap_or(Err(VmErr::Overflow))?;
                return self.push_iterator(IterFrame::BigRange { cur, end, step }, "range_iterator");
            }
            Some(HeapObj::Str(_) | HeapObj::Tuple(_) | HeapObj::Bytes(_)) => "reversed",
            Some(HeapObj::Dict(_) | HeapObj::DictView(_, View::Keys)) => "dict_reversekeyiterator",
            Some(HeapObj::DictView(_, View::Values)) => "dict_reversevalueiterator",
            Some(HeapObj::DictView(_, View::Items)) => "dict_reverseitemiterator",
            _ => return Err(VmErr::TypeMsg(s!("'", str self.type_name(o), "' object is not reversible"))),
        };
        let mut items = self.extract_iter(o)?;
        items.reverse();
        let frame = self.watched(o, items);
        self.push_iterator(frame, name)
    }

    pub fn call_enumerate(&mut self, op: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (positional, kw_flat) = self.parse_call_args(op)?;
        if positional.is_empty() || positional.len() > 2 {
            return Err(cold_type("enumerate() takes 1 or 2 positional arguments"));
        }
        // `start` is positional (`enumerate(xs, 5)`) or keyword (`enumerate(xs, start=5)`), default 0.
        let mut start = if positional.len() == 2 { positional[1] } else { Val::int(0) };
        for pair in kw_flat.as_chunks::<2>().0 {
            match self.kw_name(pair[0]) {
                Some("start") => start = pair[1],
                _ => return Err(cold_type("enumerate() got an unexpected keyword argument")),
            }
        }
        if self.as_i128(start).is_none() { return Err(cold_type("enumerate() start must be an integer")); }
        let it = self.with_roots([positional[0], start], |vm| vm.iter_of(positional[0], chunk))?;
        self.push_iterator(IterFrame::Enumerate { it, n: start }, "enumerate")
    }

    /* Pairs the items of N iterables into tuples as they are asked for, stopping at the shortest. */
    pub fn call_zip(&mut self, op: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let at = self.stack.len().checked_sub(op as usize).ok_or_else(|| cold_runtime("stack underflow"))?;
        self.iters_in_place(at, chunk)?;
        let its = self.pop_n(op as usize)?;
        self.push_iterator(IterFrame::Zip { its: its.into() }, "zip")
    }

    // TypeError for a non-iterable operand.
    fn not_iterable(&self, o: Val) -> VmErr {
        VmErr::TypeMsg(s!("'", str self.type_name(o), "' object is not iterable"))
    }

    /* Steps `o` lazily when a range or list, so `all`, `any` and `sum` stop early. */
    pub(in crate::vm) fn iter_cursor(&mut self, o: Val) -> Result<IterFrame, VmErr> {
        Ok(match self.heap.try_get(o) {
            Some(&HeapObj::Range(cur, end, step)) => IterFrame::Range { cur, end, step },
            Some(&HeapObj::BigRange(cur, end, step)) => IterFrame::BigRange { cur, end, step },
            Some(HeapObj::List(rc)) => IterFrame::List { rc: rc.clone(), idx: 0 },
            _ => IterFrame::Seq { items: self.extract_iter(o)?.into(), idx: 0 },
        })
    }

    /* Vec<Val> from any iterable (dict yields keys, str yields one-char strs, bytes yields ints, a generator runs to its end). */
    pub(crate) fn extract_iter(&mut self, o: Val) -> Result<Vec<Val>, VmErr> {
        if !o.is_heap() {
            return Err(self.not_iterable(o));
        }
        // An iterator yields what it has left, its items rooted on the stack as steps run user code.
        if matches!(self.heap.get(o), HeapObj::Iter(..)) {
            self.push(o);
            let base = self.stack.len();
            while let Some(v) = self.iter_step(o)? {
                self.charge_step()?;
                if (self.stack.len() - base).saturating_mul(VAL_BYTES) >= self.heap.room() { return Err(crate::vm::cold_heap()); }
                self.push(v);
            }
            let out = self.stack.split_off(base);
            self.pop()?;
            return Ok(out);
        }
        if matches!(self.heap.get(o), HeapObj::Coroutine(..)) {
            // Keep the coroutine and its yielded values rooted on the VM stack, each resume can allocate and trigger GC.
            self.push(o);
            let base = self.stack.len();
            loop {
                self.charge_step()?;
                let v = self.resume_coroutine(o)?;
                if !self.yielded { break; }
                self.yielded = false;
                self.push(v);
            }
            let out = self.stack.split_off(base.min(self.stack.len()));
            self.pop()?;
            return Ok(out);
        }
        if let HeapObj::DictView(d, kind) = *self.heap.get(o) {
            let items = self.view_items(d, kind)?;
            self.charge_steps(items.len())?;
            return Ok(items);
        }
        // A wide range steps its cursor, the count it would hold checked against the memory left.
        if let Some(&HeapObj::BigRange(cur, end, step)) = self.heap.try_get(o) {
            let n = crate::frontend::int::range_len(&self.heap, o).unwrap_or(Ok(0))?;
            if (n as u128).saturating_mul(VAL_BYTES as u128) > self.heap.room() as u128 { return Err(cold_heap()); }
            let (mut walk, mut out) = (IterFrame::BigRange { cur, end, step }, Vec::new());
            while let Some(x) = walk.next_item(&mut self.heap)? { out.push(x); }
            self.charge_steps(out.len())?;
            return Ok(out);
        }
        // Snapshot the variant out so the &self borrow ends before any allocation.
        let snapshot = match self.heap.get(o) {
            HeapObj::List(v) => Some(v.borrow().clone()),
            HeapObj::Tuple(v) => Some(v.clone()),
            HeapObj::Set(v) => Some(v.borrow().iter().cloned().collect()),
            HeapObj::FrozenSet(v) => Some(v.iter().cloned().collect()),
            HeapObj::Range(s, e, st) => {
                let (mut cur, end, step) = (*s, *e, *st);
                // Materialised length is user-controlled, cap it against the memory left.
                let span = (end as i128 - cur as i128).unsigned_abs();
                let count = if step == 0 { 0 } else { span / (step as i128).unsigned_abs() };
                if count.saturating_mul(VAL_BYTES as u128) > self.heap.room() as u128 { return Err(cold_heap()); }
                let mut out = Vec::new();
                if step > 0 {
                    while cur < end {
                        out.push(self.heap.int(cur as i128)?);
                        match cur.checked_add(step) { Some(n) => cur = n, None => break }
                    }
                } else {
                    while cur > end {
                        out.push(self.heap.int(cur as i128)?);
                        match cur.checked_add(step) { Some(n) => cur = n, None => break }
                    }
                }
                Some(out)
            }
            HeapObj::Dict(d) => Some(d.borrow().keys().collect()),
            HeapObj::Bytes(b) => Some(b.iter().map(|&x| Val::int(x as i64)).collect()),
            HeapObj::Str(_) => None, // handled below, needs heap allocation
            _ => return Err(self.not_iterable(o)),
        };
        if let Some(v) = snapshot {
            // Cost scales with element count, charge it so repeated materialisation stays bounded.
            self.charge_steps(v.len())?;
            return Ok(v);
        }
        // Str path materialises one-char heap strings via the existing helper.
        if let HeapObj::Str(s) = self.heap.get(o) {
            let s = s.clone();
            return self.str_to_char_vals(&s);
        }
        unreachable!()
    }

    /* `iter(x)`, and `iter(f, sentinel)` calling `f` as each item is asked for. */
    pub fn call_iter(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 2 {
            let sentinel = self.pop()?;
            let f = self.pop()?;
            return self.push_iterator(IterFrame::Call { f, sentinel }, "callable_iterator");
        }
        if argc != 1 { return Err(cold_type("iter() takes 1 or 2 arguments")); }
        let o = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
        let it = self.iter_of(o, chunk)?;
        *self.stack.last_mut().ok_or_else(|| cold_runtime("stack underflow"))? = it;
        Ok(())
    }

    pub fn call_next(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 || argc > 2 { return Err(cold_type("next() takes 1 or 2 arguments")); }
        // `next(it, default)` returns the 2nd arg instead of raising StopIteration on exhaustion.
        let default = if argc == 2 { Some(self.pop()?) } else { None };
        let o = self.pop()?;
        if !o.is_heap() { return Err(cold_type("next() requires an iterator")); }
        // For a user iterator, dispatch __next__, mapping StopIteration to the optional default.
        if matches!(self.heap.get(o), HeapObj::Instance(..)) {
            let before = self.pending.exc_val;
            return match self.try_call_dunder(o, "__next__", &[], chunk) {
                Ok(Some(v)) => { self.push(v); Ok(()) }
                Ok(None) => Err(cold_type("next() requires an iterator")),
                Err(VmErr::Raised(m)) if default.is_some() && (m == "StopIteration" || m.starts_with("StopIteration")) => {
                    self.swallow_stop(before);
                    self.push(default.unwrap()); Ok(())
                }
                Err(e) => Err(e),
            };
        }
        if matches!(self.heap.get(o), HeapObj::Iter(..)) {
            return match (self.iter_step(o)?, default) {
                (Some(v), _) | (None, Some(v)) => { self.push(v); Ok(()) }
                (None, None) => Err(VmErr::Raised(s!("StopIteration"))),
            };
        }
        if !matches!(self.heap.get(o), HeapObj::Coroutine(..)) {
            return Err(VmErr::TypeMsg(s!("'", str self.type_name(o), "' object is not an iterator")));
        }
        self.resume_generator(o, default)
    }

    /* Resumes generator `o`, its next value pushed, or `default` or StopIteration once it ends. */
    pub(crate) fn resume_generator(&mut self, o: Val, default: Option<Val>) -> Result<(), VmErr> {
        self.push(o); // root across resume's GC
        let result = self.resume_coroutine(o)?;
        if self.yielded {
            self.yielded = false;
            *self.stack.last_mut().unwrap() = result; // leave one result, not two
            Ok(())
        } else {
            self.pop()?; // drop the rooted coroutine
            if let Some(d) = default { self.push(d); return Ok(()); }
            // The value a generator returned rides on its StopIteration.
            if result.is_none() { return Err(VmErr::Raised(s!("StopIteration"))); }
            let exc = self.heap.alloc(HeapObj::ExcInstance("StopIteration".into(), alloc::vec![result], Val::undef()))?;
            self.pending.exc_val = Some(exc);
            Err(VmErr::Raised(s!("StopIteration: ", str &self.display(result))))
        }
    }

    /* `map(fn, *iterables)`, calling `fn` as each item is asked for and stopping at the shortest. */
    pub fn call_map(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc < 2 { return Err(cold_type("map() must have at least two arguments")); }
        let at = self.stack.len().checked_sub(argc as usize - 1).ok_or_else(|| cold_runtime("stack underflow"))?;
        self.iters_in_place(at, chunk)?;
        let its = self.pop_n(argc as usize - 1)?;
        let f = self.pop()?;
        self.push_iterator(IterFrame::Map { f, its: its.into() }, "map")
    }

    /* `filter(pred, iter)`, testing each item as it is asked for, a None predicate keeps truthy items. */
    pub fn call_filter(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let at = self.stack.len().checked_sub(1).ok_or_else(|| cold_runtime("stack underflow"))?;
        self.iters_in_place(at, chunk)?;
        let it = self.pop()?;
        let f = self.pop()?;
        self.push_iterator(IterFrame::Filter { f, it }, "filter")
    }

    /* Short-circuit truthiness scan shared by `all`/`any`, stopping at the first item whose truth is `find` and pushing it, `!find` once spent. */
    fn scan_truthy(&mut self, op: u16, find: bool, arity_err: &'static str, chunk: &SSAChunk) -> Result<(), VmErr> {
        if op != 1 { return Err(cold_type(arity_err)); }
        let o = self.pop()?;
        // An iterator steps as asked so the scan stops at the deciding item, anything else reads its container.
        let lazy = matches!(self.heap.try_get(o), Some(HeapObj::Iter(..) | HeapObj::Coroutine(..) | HeapObj::Instance(..)));
        let it = if lazy { self.iter_of(o, chunk)? } else { o };
        // The container or iterator stays on the stack, rooting the items a `__bool__` call may outlive.
        self.push(it);
        let decided = if lazy {
            loop {
                self.charge_step()?;
                let Some(v) = self.next_of(it, chunk)? else { break !find };
                if self.truth_of(v, chunk)? == find { break find; }
            }
        } else {
            let mut cur = self.iter_cursor(o)?;
            let mut decided = !find;
            while let Some(v) = cur.next_item(&mut self.heap)? {
                self.charge_step()?;
                let truth = if v.is_heap() { self.truth_of(v, chunk)? } else { self.truthy(v) };
                if truth == find { decided = find; break; }
            }
            decided
        };
        self.pop()?;
        self.push(Val::bool(decided));
        Ok(())
    }

    /* The truth of `v`, only an instance running user code for it and waiting rooted on the stack meanwhile. */
    fn truth_of(&mut self, v: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if !v.is_heap() || !matches!(self.heap.get(v), HeapObj::Instance(..)) { return Ok(self.truthy(v)); }
        self.push(v);
        let t = self.truthy_op(v, chunk)?;
        self.pop()?;
        Ok(t)
    }

    pub fn call_all(&mut self, op: u16, chunk: &SSAChunk) -> Result<(), VmErr> { self.scan_truthy(op, false, "all() takes exactly 1 argument", chunk) }

    pub fn call_any(&mut self, op: u16, chunk: &SSAChunk) -> Result<(), VmErr> { self.scan_truthy(op, true, "any() takes exactly 1 argument", chunk) }

    // Materialise an iterable to a list, strings -> chars, ranges eager, coroutines drained.
    pub fn call_list(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 { return self.alloc_and_push_list(Vec::new()); } // `list()` is the empty list.
        let o = self.pop()?;
        // user-defined iterable wins over the built-in dispatch.
        if let Some(items) = self.iter_to_vec_op(o, chunk)? {
            return self.alloc_and_push_list(items);
        }
        let items = self.extract_iter(o)?;
        self.alloc_and_push_list(items)
    }

    pub fn call_tuple(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 { return self.alloc_and_push_tuple(Vec::new()); } // `tuple()` is the empty tuple.
        let o = self.pop()?;
        if let Some(items) = self.iter_to_vec_op(o, chunk)? {
            return self.alloc_and_push_tuple(items);
        }
        let items = self.extract_iter(o)?;
        self.alloc_and_push_tuple(items)
    }

}

/* A key's kind to a comparison no user code decides, per tuple position. */
#[derive(PartialEq)]
enum Plain { Num, Str, Tuple(Vec<Plain>) }

fn plain_kind(v: Val, heap: &HeapPool, depth: usize) -> Option<Plain> {
    if v.is_int() || v.is_bool() || v.is_float() && !v.as_float().is_nan() { return Some(Plain::Num); }
    match heap.try_get(v)? {
        HeapObj::Str(_) => Some(Plain::Str),
        HeapObj::Tuple(items) if depth < 4 => items.iter().map(|&x| plain_kind(x, heap, depth + 1)).collect::<Option<Vec<_>>>().map(Plain::Tuple),
        _ => None,
    }
}

fn num(v: Val) -> f64 { if v.is_float() { v.as_float() } else if v.is_bool() { v.as_bool() as i64 as f64 } else { v.as_int() as f64 } }

/* Two plain keys of one kind, ordered as Python orders them. */
fn plain_cmp(a: Val, b: Val, heap: &HeapPool) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    if a.is_int() && b.is_int() { return a.as_int().cmp(&b.as_int()); }
    match (heap.try_get(a), heap.try_get(b)) {
        (Some(HeapObj::Str(x)), Some(HeapObj::Str(y))) => x.as_str().cmp(y.as_str()),
        (Some(HeapObj::Tuple(x)), Some(HeapObj::Tuple(y))) => {
            for (&p, &q) in x.iter().zip(y) {
                let o = plain_cmp(p, q, heap);
                if o != Ordering::Equal { return o; }
            }
            x.len().cmp(&y.len())
        }
        _ => num(a).partial_cmp(&num(b)).unwrap_or(Ordering::Equal),
    }
}

/* Stable order of keys compared without user code, None for dunders or mixes. */
fn plain_order(keys: &[Val], reverse: bool, heap: &HeapPool) -> Option<Vec<usize>> {
    let first = plain_kind(*keys.first()?, heap, 0)?;
    // Mixed kinds raise or need the full protocol, which the general path gives.
    for &k in &keys[1..] { if plain_kind(k, heap, 0)? != first { return None; } }
    let mut order: Vec<usize> = (0..keys.len()).collect();
    match first {
        Plain::Num if keys.iter().all(|k| k.is_int()) => {
            let mut v: Vec<(i64, usize)> = keys.iter().map(|k| k.as_int()).zip(0..).collect();
            if reverse { v.sort_by_key(|e| core::cmp::Reverse(e.0)); } else { v.sort_by_key(|e| e.0); }
            return Some(v.into_iter().map(|e| e.1).collect());
        }
        Plain::Num => {
            let mut v: Vec<(f64, usize)> = keys.iter().map(|&k| num(k)).zip(0..).collect();
            // No NaN reaches here, and -0.0 equals 0.0 as it does in Python.
            let by = |x: f64, y: f64| x.partial_cmp(&y).unwrap_or(core::cmp::Ordering::Equal);
            if reverse { v.sort_by(|a, b| by(b.0, a.0)); } else { v.sort_by(|a, b| by(a.0, b.0)); }
            return Some(v.into_iter().map(|e| e.1).collect());
        }
        _ if reverse => order.sort_by(|&a, &b| plain_cmp(keys[b], keys[a], heap)),
        _ => order.sort_by(|&a, &b| plain_cmp(keys[a], keys[b], heap)),
    }
    Some(order)
}
