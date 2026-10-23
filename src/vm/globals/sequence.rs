use crate::s;

use alloc::vec::Vec;

use super::super::VM;
use super::super::types::*;
use crate::parser::SSAChunk;
use crate::vm::natives::Builtins;


impl<'a> VM<'a> {





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
            IterFrame::Zip { its, strict } => self.zip_step(&its, strict, chunk),
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
            IterFrame::Enumerate { it, n } => self.enumerate_step(frame, it, n, chunk),
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










    /* Vec<Val> from any iterable (dict yields keys, str yields one-char strs, bytes yields ints, a generator runs to its end). */
    // Items of iterators, generators and TypeVarTuples and none for a builtin container.
    pub(crate) fn vm_items(&mut self, o: Val) -> Result<Option<Vec<Val>>, VmErr> {
        // `*Ts` of a TypeVarTuple unpacks to the one `typing.Unpack[Ts]` it stands for.
        if matches!(self.heap.get(o), HeapObj::TypeVar(n) if n.starts_with('*') && !n.starts_with("**")) {
            let origin = self.heap.alloc(HeapObj::Type("typing.Unpack".into()))?;
            let args = self.with_roots([origin], |vm| vm.heap.alloc(HeapObj::Tuple(alloc::vec![o])))?;
            return Ok(Some(alloc::vec![self.with_roots([origin, args], |vm| vm.heap.alloc(HeapObj::GenericAlias(origin, args)))?]));
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
            return Ok(Some(out));
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
            return Ok(Some(out));
        }
        Ok(None)
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
        if matches!(self.heap.try_get(o), Some(HeapObj::Instance(..))) && !self.is_iterator(it) { return Err(self.non_iterator(it)); }
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
pub(crate) fn plain_order(keys: &[Val], reverse: bool, heap: &HeapPool) -> Option<Vec<usize>> {
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
