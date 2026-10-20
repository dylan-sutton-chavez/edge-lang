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
        let entries: Vec<(Val, Val)> = self.heap.dict_cell(d).map_or_else(Vec::new, |m| m.borrow().iter().collect());
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
            Some(HeapObj::Coroutine(..)) if self.is_async_object(o) => return Err(self.not_iterable(o)),
            Some(HeapObj::Iter(..) | HeapObj::Coroutine(..)) => return Ok(o),
            Some(HeapObj::Instance(..)) => return self.try_call_dunder(o, "__iter__", &[], chunk)?.ok_or_else(|| self.not_iterable(o)),
            Some(HeapObj::List(rc)) => { let rc = rc.clone(); return self.alloc_iterator(IterFrame::List { rc, idx: 0 }, "list_iterator"); }
            Some(&HeapObj::Range(cur, end, step)) => return self.alloc_iterator(IterFrame::Range { cur, end, step }, "range_iterator"),
            Some(&HeapObj::BigRange(cur, end, step)) => return self.alloc_iterator(IterFrame::BigRange { cur, end, step }, "range_iterator"),
            Some(HeapObj::Dict(_) | HeapObj::FrozenDict(_)) => return self.alloc_iterator(self.dict_walk(o, View::Keys), "dict_keyiterator"),
            Some(&HeapObj::DictView(d, kind)) => {
                let name = match kind { View::Keys => "dict_keyiterator", View::Values => "dict_valueiterator", View::Items => "dict_itemiterator" };
                return self.alloc_iterator(self.dict_walk(d, kind), name);
            }
            Some(HeapObj::Tuple(_)) => "tuple_iterator",
            Some(HeapObj::Str(_)) => if ascii { "str_ascii_iterator" } else { "str_iterator" },
            Some(HeapObj::Set(_) | HeapObj::FrozenSet(_)) => "set_iterator",
            Some(HeapObj::Bytes(_)) => "bytes_iterator",
            Some(HeapObj::ByteArray(_)) => "bytearray_iterator",
            Some(HeapObj::MemoryView(_)) => "memory_iterator",
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
        // A job made this step's item already.
        if let Some(v) = self.pended(it) { return Ok(v); }
        match self.heap.try_get(it) {
            Some(HeapObj::Iter(..)) => self.iter_step(it),
            Some(HeapObj::Coroutine(..)) if self.is_async_object(it) => Err(VmErr::TypeMsg(s!("'", str self.type_name(it), "' object is not an iterator"))),
            Some(HeapObj::Coroutine(..)) => {
                let v = self.with_roots([it], |vm| vm.step_coroutine(it))?;
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
        if let Some(v) = self.pended(it) { return Ok(v); }
        // An op stepping an iterator runs no more again, which would step it twice.
        (self.redo.on, self.redo.lazy) = (false, false);
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return Ok(None); };
        let frame = frame.clone();
        // An awaitable steps as a generator, its return value dropped.
        if matches!(&*frame.borrow(), IterFrame::Awaitable { .. }) {
            let chunk = self.chunk;
            return Ok(match self.gen_step(it, super::async_ops::Sent::Value(Val::none()), chunk)? { super::async_ops::Step::Yield(v) => Some(v), super::async_ops::Step::Return(_) => None });
        }
        if !matches!(&*frame.borrow(), IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. } | IterFrame::Call { .. }) {
            return frame.borrow_mut().next_item(&mut self.heap);
        }
        let state = frame.borrow().clone();
        // Nested lazy iterators recurse in Rust, so they count against the call depth.
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.depth += 1;
        let step = self.with_roots([it], |vm| vm.lazy_step(Some(&frame), state));
        self.depth -= 1;
        step
    }

    /* One step of a `map` or `filter` the iterator stack holds itself, as `iter_step` steps one through its object. */
    pub(crate) fn called_step(&mut self, state: IterFrame) -> Result<Option<Val>, VmErr> {
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.depth += 1;
        let step = self.lazy_step(None, state);
        self.depth -= 1;
        step
    }

    /* What a step whose callback raised `e` gives, the iterator's end for a StopIteration with the stack back at `base`, else the error. */
    fn stopped(&mut self, e: VmErr, base: usize) -> Result<Option<Val>, VmErr> {
        if !matches!(&e, VmErr::Raised(m) if m == "StopIteration" || m.starts_with("StopIteration:")) { return Err(e); }
        self.swallow_stop(None);
        (self.error_byte_pos, self.call_stack) = (None, alloc::vec::Vec::new());
        self.stack.truncate(base);
        Ok(None)
    }

    /* One step of a frame calling user code, `state` read before a callback can step it again, `frame` the one it lies in. */
    fn lazy_step(&mut self, frame: Option<&alloc::rc::Rc<core::cell::RefCell<IterFrame>>>, state: IterFrame) -> Result<Option<Val>, VmErr> {
        let chunk = self.chunk;
        match state {
            // A StopIteration out of the function ends the iterator, as it ends a `for` stepping it.
            IterFrame::Map { f, its, .. } => {
                let base = self.stack.len();
                self.push(f);
                if !self.push_next_of(&its, chunk)? { self.pop()?; return Ok(None); }
                if let Err(e) = self.exec_call(its.len() as u16, chunk) { return self.stopped(e, base); }
                self.pop().map(Some)
            }
            IterFrame::Zip { its, strict } => {
                let base = self.stack.len();
                if its.is_empty() { return Ok(None); }
                if !strict {
                    if !self.push_next_of(&its, chunk)? { return Ok(None); }
                } else {
                    // A strict zip ends only when every iterable ends together.
                    for (k, &it) in its.iter().enumerate() {
                        match self.next_of(it, chunk)? {
                            Some(v) => self.push(v),
                            None if k > 0 => { self.stack.truncate(base); return Err(Self::zip_uneven(k, false)); }
                            None => {
                                for (j, &rest) in its.iter().enumerate().skip(1) {
                                    if let Some(extra) = self.next_of(rest, chunk)? { let _ = extra; return Err(Self::zip_uneven(j, true)); }
                                }
                                return Ok(None);
                            }
                        }
                    }
                }
                let items = self.stack.split_off(base);
                self.heap.alloc(HeapObj::Tuple(items)).map(Some)
            }
            IterFrame::Filter { f, it, .. } => { let (soft, mut first) = (core::mem::take(&mut self.step_soft), true); loop {
                // A driven step leaves a generator's next item to its driver to run as a frame, and one its builtin test would step in Rust.
                if soft && !first && !matches!(self.heap.try_get(f), Some(HeapObj::Func(..) | HeapObj::Class(..) | HeapObj::Instance(..) | HeapObj::BoundUserMethod(..)))
                    && (matches!(self.peek(it, 0), crate::vm::jobs::Peek::Gen(_)) || self.steps_whole(f) && self.leaf_ahead(it, 0)) { return Ok(Some(Val::undef())); }
                first = false;
                let base = self.stack.len();
                self.charge_step()?;
                let Some(item) = self.next_of(it, chunk)? else { return Ok(None) };
                // The item waits on the stack, rooted while the predicate runs.
                self.push(item);
                let verdict = if f.is_none() { item } else {
                    self.push(f);
                    self.push(item);
                    if let Err(e) = self.exec_call(1, chunk) { return self.stopped(e, self.stack.len().min(base)); }
                    self.pop()?
                };
                let keep = self.truthy_op(verdict, chunk)?;
                let item = self.pop()?;
                if keep { return Ok(Some(item)); }
            } },
            IterFrame::Enumerate { it, n } => {
                let Some(item) = self.next_of(it, chunk)? else { return Ok(None) };
                self.push(item);
                let next = self.int_to_val(self.as_i128(n).and_then(|i| i.checked_add(1)))?;
                if let Some(frame) = frame && let IterFrame::Enumerate { n: count, .. } = &mut *frame.borrow_mut() { *count = next; }
                let item = self.pop()?;
                self.heap.alloc(HeapObj::Tuple(vec![n, item])).map(Some)
            }
            IterFrame::Call { f, sentinel } => {
                self.push(f);
                self.exec_call(0, chunk)?;
                let v = self.pop()?;
                if !eq_member(v, sentinel, &self.heap) { return Ok(Some(v)); }
                // A callable iterator stays spent once it meets the sentinel.
                if let Some(frame) = frame { *frame.borrow_mut() = IterFrame::Seq { items: Vec::new().into(), idx: 0 }; }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /* Whether `o` is a `map` or a `filter`, whose step calls its function, or an iterator over a generator. */
    #[inline]
    fn lazy_iter(&self, o: Val) -> bool {
        let Some(HeapObj::Iter(f, _)) = self.heap.try_get(o) else { return false };
        match f.try_borrow().as_deref() {
            // A zip or an enumerate may step one of them, which its job takes as it steps it, as it does a Python function's iterator.
            Ok(IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. }) => true,
            Ok(&IterFrame::Call { f, .. }) => matches!(self.heap.try_get(f), Some(HeapObj::Func(..))),
            // A container's own iterator steps no generator, the common case looked at no further.
            Ok(IterFrame::Seq { .. } | IterFrame::List { .. } | IterFrame::Range { .. } | IterFrame::BigRange { .. } | IterFrame::ListRev { .. } | IterFrame::Watched { .. } | IterFrame::DictWalk { .. }) | Err(_) => false,
            Ok(_) => matches!(self.peek(o, 0), crate::vm::jobs::Peek::Gen(_)),
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
        let ok = core::mem::take(&mut self.job_ok);
        let o = self.pop()?;
        // An instance's `__len__` runs as a frame of the loop.
        if ok && self.is_instance(o) && self.check_job(o, crate::vm::jobs::Check::Len) { return Ok(()); }
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
            Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => b.len() as i128,
            Some(HeapObj::MemoryView(m)) => if m.released { return Err(cold_value("operation forbidden on released memoryview object")) } else { m.len as i128 },
            Some(HeapObj::List(v)) => v.borrow().len() as i128,
            Some(HeapObj::Tuple(v)) => v.len() as i128,
            Some(HeapObj::Dict(v) | HeapObj::FrozenDict(v)) => v.borrow().len() as i128,
            Some(&HeapObj::DictView(d, _)) => return self.builtin_len(d),
            Some(HeapObj::Set(v)) => v.borrow().len() as i128,
            Some(HeapObj::FrozenSet(v)) => v.len() as i128,
            Some(&HeapObj::Range(_, _, 0)) => return Err(cold_value("range() arg 3 must not be zero")),
            Some(&HeapObj::Range(s, e, st)) => crate::vm::eq::range_len(s, e, st),
            Some(HeapObj::BigRange(..)) => crate::frontend::int::range_len(&self.heap, o).unwrap_or(Ok(0))?,
            _ => return Err(VmErr::TypeMsg(s!("object of type '", str self.type_name(o), "' has no len()"))),
        })
    }

    /* `sorted` with no key, its `__lt__` run as frames of the loop when `ok`, called where one may open. */
    fn sorted_of(&mut self, reverse: bool, ok: bool, chunk: &SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        let items = self.extract_iter(o)?;
        let mut items = if ok { match self.merge_job(items, reverse, Val::undef()) { Ok(()) => return Ok(()), Err(items) => items } } else { items };
        self.sort_by_lt(&mut items, reverse, chunk)?;
        self.alloc_and_push_list(items)
    }

    /* Whether the generator on top of the stack, or the one the instance there gives, runs as a frame of the loop, gathered into the list `sorted` sorts. */
    #[cold]
    #[inline(never)]
    fn sorted_gen(&mut self, key: Option<Val>, reverse: bool, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let at = self.stack.len() - 1;
        let v = self.stack[at];
        let extra = alloc::vec![key.unwrap_or(Val::undef()), Val::bool(reverse)];
        // A builtin iterator whose steps run Python is stepped by a job whose calls are frames of the loop.
        if matches!(self.heap.try_get(v), Some(HeapObj::Iter(..))) { return self.drive_iter(v, crate::vm::jobs::Takes::Collect(NativeFnId::Sorted, 1, at as u32), &extra); }
        let takes = crate::vm::jobs::Takes::Collect(NativeFnId::Sorted, 1, at as u32);
        // An instance's `__iter__` and the `__next__` of what it gives run as frames of the loop too.
        let g = match self.heap.try_get(v) { Some(HeapObj::Coroutine(..)) => v, _ => match self.gen_of_iter(v, chunk)? { Some(g) => { self.stack[at] = g; g } None => return self.pull_job(v, takes, &extra, chunk) } };
        self.gather_job(g, takes, extra)
    }

    /* sorted(iterable, key=fn, reverse=False), `sorted_of` when key is absent. */
    pub fn call_sorted_with_key(&mut self, key: Option<Val>, reverse: bool, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        // A generator or an instance sorted where a frame may open runs as frames of the loop, its items gathered as the list sorted then takes.
        if ok && let Some(&o) = self.stack.last() && o.is_heap() && matches!(self.heap.get(o), HeapObj::Coroutine(..) | HeapObj::Instance(..) | HeapObj::Iter(..)) && self.sorted_gen(key, reverse, chunk)? { return Ok(()); }
        // Any other user iterable gives its items through the nested call.
        if let Some(&o) = self.stack.last() && let Some(l) = self.lift_iterable(o, chunk)? && let Some(top) = self.stack.last_mut() { *top = l; }
        let key = match key {
            Some(k) if !k.is_none() => k,
            _ => return self.sorted_of(reverse, ok, chunk),
        };
        let o = self.pop()?;
        let items = self.extract_iter(o)?;
        // A builtin key stepping a generator among the items in Rust runs through a function of the loop, as a job stepping it.
        let key = if ok && self.steps_whole(key) && items.iter().any(|&v| self.steps_py(v)) { self.through_loop(key, 1)?.unwrap_or(key) } else { key };
        // A function key runs as frames of the loop that called, the sort done once the last key is back.
        if ok && let Some(&first) = items.first() && let Some(fi) = self.job_fn(key) {
            let keys = Vec::with_capacity(items.len());
            self.key_job(fi, key, first, super::super::jobs::Job::Sorted(alloc::boxed::Box::new(super::super::jobs::Sorted { items, keys, reverse, again: Default::default(), into: Val::undef() })));
            return Ok(());
        }
        // A class key makes each key as frames of the loop, the instance's `__init__` among them.
        if ok && self.class_keys(key, &items, reverse, Val::undef())? { return Ok(()); }
        // An `eval` or `exec` key runs each text as a floor of the loop, the sort done once the last is back.
        if ok && let Some(&first) = items.first() && self.runs_text(key) {
            let keys = Vec::with_capacity(items.len());
            return self.key_floor(key, first, super::super::jobs::Job::Sorted(alloc::boxed::Box::new(super::super::jobs::Sorted { items, keys, reverse, again: Default::default(), into: Val::undef() })));
        }
        let sorted = self.sort_by_key(items, key, reverse, chunk)?;
        self.alloc_and_push_list(sorted)
    }

    /* list.sort(key=fn, reverse=False) in-place. Snapshots list before key calls so heap borrow ends before exec_call. */
    pub fn call_list_sort_keyed(&mut self, recv: Val, key: Option<Val>, reverse: bool, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        let items = match self.heap.get(recv) {
            HeapObj::List(rc) => rc.borrow().clone(),
            _ => return Err(cold_type("sort: receiver is not a list")),
        };
        // Instances whose `__lt__` runs as frames of the loop sort there, the list taking the order once the sort is done.
        let items = if ok && key.is_none_or(|k| k.is_none()) { match self.merge_job(items, reverse, recv) { Ok(()) => return Ok(()), Err(items) => items } } else { items };
        // A builtin key stepping a generator among the items in Rust runs through a function of the loop, as a job stepping it.
        let key = match key { Some(k) if ok && self.steps_whole(k) && items.iter().any(|&v| self.steps_py(v)) => Some(self.through_loop(k, 1)?.unwrap_or(k)), k => k };
        // A function key runs as frames of the loop that called, the list taking its order once the last key is back.
        if ok && let Some(k) = key.filter(|k| !k.is_none()) && let Some(&first) = items.first() && let Some(fi) = self.job_fn(k) {
            let keys = Vec::with_capacity(items.len());
            self.key_job(fi, k, first, super::super::jobs::Job::Sorted(alloc::boxed::Box::new(super::super::jobs::Sorted { items, keys, reverse, again: Default::default(), into: recv })));
            return Ok(());
        }
        if ok && let Some(k) = key.filter(|k| !k.is_none()) && self.class_keys(k, &items, reverse, recv)? { return Ok(()); }
        if ok && let Some(k) = key.filter(|k| self.runs_text(*k)) && let Some(&first) = items.first() {
            let keys = Vec::with_capacity(items.len());
            return self.key_floor(k, first, super::super::jobs::Job::Sorted(alloc::boxed::Box::new(super::super::jobs::Sorted { items, keys, reverse, again: Default::default(), into: recv })));
        }
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

    /* Root the operands (comparators can run GC-triggering user code), sort `keys` the way Python does through `sort_lt`, and return the index permutation. `extra_roots` keeps caller-only values (e.g. the items in a keyed sort) alive across comparisons. The first comparison error ends the sort. */
    pub(crate) fn sorted_order(&mut self, keys: &[Val], extra_roots: &[Val], reverse: bool, chunk: &SSAChunk) -> Result<Vec<usize>, VmErr> {
        if let Some(order) = plain_order(keys, reverse, &self.heap) { return Ok(order); }
        let roots_base = self.temp_roots.len();
        // Only an instance key runs user code that could collect.
        if self.any_instance(keys) { self.temp_roots.extend(keys.iter().chain(extra_roots).copied()); }
        // A descending sort reverses, sorts ascending and reverses back, so equal keys keep their order.
        let mut order: Vec<usize> = if reverse { (0..keys.len()).rev().collect() } else { (0..keys.len()).collect() };
        let sorted = crate::vm::timsort::sort(&mut order, &mut |x, y| self.sort_lt(keys[x], keys[y], chunk));
        if reverse { order.reverse(); }
        let sort_err = sorted.err();
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

    /* `reversed(seq)`, a list read live from its end, a range stepped backwards, other sequences read up front. */
    #[inline(never)]
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
            Some(HeapObj::Str(_) | HeapObj::Tuple(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_)) => "reversed",
            Some(HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::DictView(_, View::Keys)) => "dict_reversekeyiterator",
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
        // `strict=` is the one keyword, its pair above the iterables.
        let (pos, kw) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let mut strict = false;
        for pair in self.pop_n(2 * kw)?.chunks(2) {
            if self.kw_name(pair[0]) != Some("strict") {
                return Err(VmErr::TypeMsg(s!("zip() got an unexpected keyword argument '", str self.kw_name(pair[0]).unwrap_or(""), "'")));
            }
            strict = self.truthy(pair[1]);
        }
        let at = self.stack.len().checked_sub(pos).ok_or_else(|| cold_runtime("stack underflow"))?;
        self.iters_in_place(at, chunk)?;
        let its = self.pop_n(pos)?;
        self.push_iterator(IterFrame::Zip { its: its.into(), strict }, "zip")
    }

    /* The error of a strict zip whose argument `k` ran out, or ran on, against those before it. */
    #[cold]
    fn zip_uneven(k: usize, longer: bool) -> VmErr {
        let before = if k == 1 { alloc::string::String::from("argument 1") } else { s!("arguments 1-", int k as i64) };
        let (n, how) = (k as i64 + 1, if longer { " is longer than " } else { " is shorter than " });
        cold_value_s(s!("zip() argument ", int n, str how, str &before))
    }

    // TypeError for a non-iterable operand.
    pub(crate) fn not_iterable(&self, o: Val) -> VmErr {
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
        // `*Ts` of a TypeVarTuple unpacks to the one `typing.Unpack[Ts]` it stands for.
        if matches!(self.heap.get(o), HeapObj::TypeVar(n) if n.starts_with('*') && !n.starts_with("**")) {
            let origin = self.heap.alloc(HeapObj::Type("typing.Unpack".into()))?;
            let args = self.with_roots([origin], |vm| vm.heap.alloc(HeapObj::Tuple(alloc::vec![o])))?;
            return Ok(alloc::vec![self.with_roots([origin, args], |vm| vm.heap.alloc(HeapObj::GenericAlias(origin, args)))?]);
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
            if self.is_async_object(o) { return Err(self.not_iterable(o)); }
            // Keep the coroutine and its yielded values rooted on the VM stack, each resume can allocate and trigger GC.
            self.push(o);
            let base = self.stack.len();
            loop {
                self.charge_step()?;
                let v = self.step_coroutine(o)?;
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
            HeapObj::Dict(d) | HeapObj::FrozenDict(d) => Some(d.borrow().keys().collect()),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => Some(b.iter().map(|&x| Val::int(x as i64)).collect()),
            HeapObj::MemoryView(_) => Some(self.heap.octets(o).unwrap_or_default().iter().map(|&x| Val::int(x as i64)).collect()),
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

    #[inline(never)]
    pub fn call_next(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        if argc == 0 || argc > 2 { return Err(cold_type("next() takes 1 or 2 arguments")); }
        // `next(it, default)` returns the 2nd arg instead of raising StopIteration on exhaustion.
        let default = if argc == 2 { Some(self.pop()?) } else { None };
        let o = self.pop()?;
        let not_iterator = |vm: &Self| VmErr::TypeMsg(crate::s!("'", str vm.type_name(o), "' object is not an iterator"));
        if !o.is_heap() { return Err(not_iterator(self)); }
        // For a user iterator, dispatch __next__, mapping StopIteration to the optional default.
        if ok && matches!(self.heap.get(o), HeapObj::Instance(..))
            && self.check_job(o, crate::vm::jobs::Check::Next(default.unwrap_or(Val::undef()), self.pending.exc_val.unwrap_or(Val::undef()))) { return Ok(()); }
        if matches!(self.heap.get(o), HeapObj::Instance(..)) {
            let before = self.pending.exc_val;
            return match self.try_call_dunder(o, "__next__", &[], chunk) {
                Ok(Some(v)) => { self.push(v); Ok(()) }
                Ok(None) => Err(not_iterator(self)),
                Err(VmErr::Raised(m)) if default.is_some() && (m == "StopIteration" || m.starts_with("StopIteration")) => {
                    self.swallow_stop(before);
                    self.push(default.unwrap()); Ok(())
                }
                Err(e) => Err(e),
            };
        }
        if matches!(self.heap.get(o), HeapObj::Iter(frame, _) if matches!(&*frame.borrow(), IterFrame::Awaitable { .. })) {
            // Called where a frame may open, its target steps as one.
            if ok && self.awaitable_next(o, default.unwrap_or(Val::undef()), chunk)? { return Ok(()); }
            return match (self.gen_step(o, super::async_ops::Sent::Value(Val::none()), chunk)?, default) {
                (super::async_ops::Step::Yield(v), _) | (_, Some(v)) => { self.push(v); Ok(()) }
                (super::async_ops::Step::Return(v), None) => Err(self.stop_iteration(v)),
            };
        }
        if matches!(self.heap.get(o), HeapObj::Iter(..)) {
            // A `map` or `filter` of a Python function called where a frame may open runs it as frames of a job.
            if ok && self.lazy_iter(o) && self.items_job(o, crate::vm::jobs::Takes::Next(default.unwrap_or(Val::undef())), chunk)? { return Ok(()); }
            return match (self.iter_step(o)?, default) {
                (Some(v), _) | (None, Some(v)) => { self.push(v); Ok(()) }
                (None, None) => Err(VmErr::Raised(s!("StopIteration"))),
            };
        }
        if !matches!(self.heap.get(o), HeapObj::Coroutine(..)) || self.is_async_object(o) {
            return Err(VmErr::TypeMsg(s!("'", str self.type_name(o), "' object is not an iterator")));
        }
        // A generator called where a frame may open runs to its next yield as a frame of the loop.
        if ok && self.gather_job(o, crate::vm::jobs::Takes::Next(default.unwrap_or(Val::undef())), Vec::new())? { return Ok(()); }
        self.resume_generator(o, default)
    }

    /* Resumes generator `o`, its next value pushed, or `default` or StopIteration once it ends. */
    pub(crate) fn resume_generator(&mut self, o: Val, default: Option<Val>) -> Result<(), VmErr> {
        self.push(o); // root across resume's GC
        let result = self.step_coroutine(o)?;
        if self.yielded {
            self.yielded = false;
            *self.stack.last_mut().unwrap() = result; // leave one result, not two
            Ok(())
        } else {
            self.pop()?; // drop the rooted coroutine
            if let Some(d) = default { self.push(d); return Ok(()); }
            // The value a generator returned rides on its StopIteration.
            Err(self.stop_iteration(result))
        }
    }

    /* `map(fn, *iterables)`, calling `fn` as each item is asked for and stopping at the shortest. */
    pub fn call_map(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc < 2 { return Err(cold_type("map() must have at least two arguments")); }
        let at = self.stack.len().checked_sub(argc as usize - 1).ok_or_else(|| cold_runtime("stack underflow"))?;
        let own = self.makes_iterator(self.stack[at]);
        self.iters_in_place(at, chunk)?;
        let its = self.pop_n(argc as usize - 1)?;
        let f = self.pop()?;
        self.push_iterator(IterFrame::Map { f, its: its.into(), own }, "map")
    }

    /* `filter(pred, iter)`, testing each item as it is asked for, a None predicate keeps truthy items. */
    pub fn call_filter(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let at = self.stack.len().checked_sub(1).ok_or_else(|| cold_runtime("stack underflow"))?;
        let own = self.makes_iterator(self.stack[at]);
        self.iters_in_place(at, chunk)?;
        let it = self.pop()?;
        let f = self.pop()?;
        self.push_iterator(IterFrame::Filter { f, it, own }, "filter")
    }

    /* Whether the iterator `iter_of` gives for `o` is one it makes, which only its caller holds, as for a builtin container. */
    fn makes_iterator(&self, o: Val) -> bool {
        matches!(self.heap.try_get(o), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Range(..) | HeapObj::Str(_) | HeapObj::Dict(_) | HeapObj::Set(_)))
            && self.heap.classed(o).is_none()
    }

    /* Short-circuit truthiness scan shared by `all`/`any`, stopping at the first item whose truth is `find` and pushing it, `!find` once spent. */
    fn scan_truthy(&mut self, op: u16, find: bool, arity_err: &'static str, chunk: &SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        if op != 1 { return Err(cold_type(arity_err)); }
        let o = self.pop()?;
        self.fold_run(o, if find { crate::vm::jobs::Takes::Any } else { crate::vm::jobs::Takes::Every }, &[], ok, chunk)
    }

    pub fn call_all(&mut self, op: u16, chunk: &SSAChunk) -> Result<(), VmErr> { self.scan_truthy(op, false, "all() takes exactly 1 argument", chunk) }

    pub fn call_any(&mut self, op: u16, chunk: &SSAChunk) -> Result<(), VmErr> { self.scan_truthy(op, true, "any() takes exactly 1 argument", chunk) }

    // Materialise an iterable to a list, strings -> chars, ranges eager, coroutines drained.
    pub fn call_list(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        if argc == 0 { return self.alloc_and_push_list(Vec::new()); } // `list()` is the empty list.
        let o = self.pop()?;
        // A generator, or a user iterator's `__next__`, called where a frame may open runs as a frame of the loop, its items gathered as they come.
        if ok && self.may_job(o) && self.items_job(o, crate::vm::jobs::Takes::All(OpCode::CallList, 1), chunk)? { return Ok(()); }
        // user-defined iterable wins over the built-in dispatch.
        if let Some(items) = self.iter_to_vec_op(o, chunk)? {
            return self.alloc_and_push_list(items);
        }
        let items = self.extract_iter(o)?;
        self.alloc_and_push_list(items)
    }

    pub fn call_tuple(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        if argc == 0 { return self.alloc_and_push_tuple(Vec::new()); } // `tuple()` is the empty tuple.
        let o = self.pop()?;
        if ok && self.may_job(o) && self.items_job(o, crate::vm::jobs::Takes::All(OpCode::CallTuple, 1), chunk)? { return Ok(()); }
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
