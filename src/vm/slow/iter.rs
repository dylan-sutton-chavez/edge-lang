use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::vm::VM;
use crate::r#unsafe::pinned;
use crate::value::*;
use crate::code::Ins;
use crate::vm::exc::is_stop;
use crate::lang::{Act, EngineError};

impl<'a> VM<'a> {
    /* the next item of a builtin iterator or a generator or None once spent */
    pub(crate) fn next_of(&mut self, it: Val) -> Result<Option<Val>, VmErr> {
        // a job made this step's item already
        if let Some(v) = self.pended(it) { return Ok(v); }
        match self.heap.try_get(it) {
            Some(HeapObj::Iter(..)) => self.iter_step(it),
            Some(HeapObj::Coroutine(..)) if self.is_async_object(it) => Err(EngineError::cannot(Act::Next, Some(it))),
            Some(HeapObj::Coroutine(..)) => {
                let v = self.with_roots([it], |vm| vm.step_coroutine(it))?;
                Ok(core::mem::take(&mut self.yielded).then_some(v))
            }
            _ => Err(EngineError::cannot(Act::Next, Some(it))),
        }
    }

    /* the next item of builtin iterator `it` or None once it is spent */
    pub(crate) fn iter_step(&mut self, it: Val) -> Result<Option<Val>, VmErr> {
        if let Some(v) = self.pended(it) { return Ok(v); }
        // an op stepping an iterator runs no more again since that would step it twice
        (self.redo.on, self.redo.lazy) = (false, false);
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return Ok(None); };
        let frame = frame.clone();
        if !matches!(&*frame.borrow(), IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. } | IterFrame::Call { .. }) {
            return frame.borrow_mut().next_item(&mut self.heap);
        }
        let state = frame.borrow().clone();
        // nested lazy iterators recurse in Rust so they count against the call depth
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.depth += 1;
        let step = self.with_roots([it], |vm| vm.lazy_step(Some(&frame), state));
        self.depth -= 1;
        step
    }

    /* the `iter_step` of a `map` or `filter` the iterator stack holds itself */
    pub(crate) fn called_step(&mut self, state: IterFrame) -> Result<Option<Val>, VmErr> {
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.depth += 1;
        let step = self.lazy_step(None, state);
        self.depth -= 1;
        step
    }

    /* a step whose callback raised `e` ends the iterator or gives the error */
    fn stopped(&mut self, e: VmErr) -> Result<Option<Val>, VmErr> {
        if !e.is(crate::lang::ErrKind::Stop, self.natives.names()) { return Err(e); }
        self.swallow_stop(None);
        (self.error_byte_pos, self.call_stack) = (None, alloc::vec::Vec::new());
        Ok(None)
    }

    /* `v` passing test `f` or its truth for undef and None when the test stops */
    pub(crate) fn passes(&mut self, f: Val, v: Val) -> Result<Option<bool>, VmErr> {
        if f.is_undef() { return Ok(Some(self.truthy(v))); }
        let chunk = self.chunk;
        let verdict = match self.with_roots([v], |vm| vm.call_with(f, None, &[v], &[], chunk)) {
            Ok(r) => self.got(r)?,
            Err(e) => return self.stopped(e).map(|_| None),
        };
        Ok(Some(self.truthy(verdict)))
    }

    /* steps a frame calling user code from `state` read before a callback can step `frame` */
    fn lazy_step(&mut self, frame: Option<&alloc::rc::Rc<core::cell::RefCell<IterFrame>>>, state: IterFrame) -> Result<Option<Val>, VmErr> {
        let chunk = self.chunk;
        match state {
            // an end of iteration out of the function ends the iterator like a `for`
            IterFrame::Map { f, its, .. } => {
                // the function and the items it takes stay rooted while later steps run user code
                let args = self.with_roots([f], |vm| {
                    let base = vm.temp_roots.len();
                    Ok::<_, VmErr>(vm.next_of_into(&its)?.then(|| crate::vm::calls::Args::of(&vm.temp_roots[base..])))
                })?;
                let Some(args) = args else { return Ok(None) };
                match self.call_with(f, None, &args, &[], chunk) {
                    Ok(v) => self.got(v).map(Some),
                    Err(e) => self.stopped(e),
                }
            }
            IterFrame::Zip { its, strict } => self.zip_step(&its, strict),
            IterFrame::Filter { f, it, .. } => { let (soft, mut first) = (core::mem::take(&mut self.step_soft), true); loop {
                // driven steps leave the driver what generators give or builtin tests would step in Rust
                if soft && !first && !(matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })) || self.heap.bound_of(f).is_some_and(|(_, g)| g.is_heap()) || self.heap.is_class(f))
                    && (matches!(self.peek(it, 0), crate::vm::jobs::Peek::Gen(_)) || self.steps_whole(f) && self.leaf_ahead(it, 0)) { return Ok(Some(Val::undef())); }
                first = false;
                self.charge_step()?;
                let Some(item) = self.next_of(it)? else { return Ok(None) };
                // the item stays rooted while the predicate runs
                let verdict = if f.is_undef() { item } else {
                    match self.with_roots([item], |vm| vm.call_with(f, None, &[item], &[], chunk)) {
                        Ok(v) => self.got(v)?,
                        Err(e) => return self.stopped(e),
                    }
                };
                if self.truthy(verdict) { return Ok(Some(item)); }
            } },
            IterFrame::Enumerate { it, n } => self.enumerate_step(frame, it, n),
            IterFrame::Call { f, sentinel } => {
                let v = self.call_with(f, None, &[], &[], chunk)?;
                let v = self.got(v)?;
                if !eq_member(v, sentinel, &self.heap) { return Ok(Some(v)); }
                // a callable iterator stays spent once it meets the sentinel
                if let Some(frame) = frame { *frame.borrow_mut() = IterFrame::Seq { items: Vec::new().into(), idx: 0 }; }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    // items of iterators and generators and none for a builtin container
    pub(crate) fn vm_items(&mut self, o: Val) -> Result<Option<Vec<Val>>, VmErr> {
        // an iterator yields what it has left with items rooted as steps run user code
        if matches!(self.heap.get(o), HeapObj::Iter(..)) {
            return self.with_roots([o], |vm| {
                let base = vm.temp_roots.len();
                while let Some(v) = vm.iter_step(o)? {
                    vm.charge_step()?;
                    if (vm.temp_roots.len() - base).saturating_mul(VAL_BYTES) >= vm.heap.room() { return Err(crate::vm::cold_heap()); }
                    vm.temp_roots.push(v);
                }
                Ok(Some(vm.temp_roots[base..].to_vec()))
            });
        }
        if matches!(self.heap.get(o), HeapObj::Coroutine(..)) {
            if self.is_async_object(o) { return Err(self.not_iterable(o)); }
            // the coroutine and its yielded values stay rooted as each resume may collect
            return self.with_roots([o], |vm| {
                let base = vm.temp_roots.len();
                loop {
                    vm.charge_step()?;
                    let v = vm.step_coroutine(o)?;
                    if !vm.yielded { break; }
                    vm.yielded = false;
                    vm.temp_roots.push(v);
                }
                Ok(Some(vm.temp_roots[base..].to_vec()))
            });
        }
        Ok(None)
    }

    /* the iterator over `o` that `iter(o)` gives */
    pub(crate) fn iter_of(&mut self, o: Val) -> Result<Val, VmErr> {
        use crate::lang::iters as it;
        let ascii = self.heap.str_is_ascii(o);
        let names = &self.natives.names().iters;
        let name = match self.heap.try_get(o) {
            Some(HeapObj::Coroutine(..)) if self.is_async_object(o) => return Err(self.not_iterable(o)),
            Some(HeapObj::Iter(..) | HeapObj::Coroutine(..)) => return Ok(o),
            Some(HeapObj::List(rc)) => { let rc = rc.clone(); return self.alloc_iterator(IterFrame::List { rc, idx: 0 }, names[it::LIST]); }
            Some(&HeapObj::Range(cur, end, step)) => return self.alloc_iterator(IterFrame::Range { cur, end, step }, names[it::RANGE]),
            Some(&HeapObj::BigRange(cur, end, step)) => return self.alloc_iterator(IterFrame::BigRange { cur, end, step }, names[it::RANGE]),
            Some(HeapObj::Dict(_)) => return self.alloc_iterator(self.dict_walk(o, DictPart::Keys), names[it::KEYS]),
            Some(HeapObj::Tuple(_)) => names[it::TUPLE],
            Some(HeapObj::Str(_)) => if ascii { names[it::ASCII] } else { names[it::TEXT] },
            Some(HeapObj::Set(_)) => names[it::SET],
            Some(HeapObj::Bytes(_)) => names[it::BYTES],
            Some(HeapObj::ByteArray(_)) => names[it::BYTE_ARRAY],
            // a foreign kind's frame steps as an iterator named by the frame it is
            _ => return match self.heap.foreign_frame(o) {
                Some(frame) => {
                    let name = match frame { IterFrame::Range { .. } | IterFrame::BigRange { .. } => names[it::RANGE], IterFrame::Octets { .. } => names[it::BYTES], IterFrame::Seq { .. } | IterFrame::List { .. } => names[it::TUPLE], _ => names[it::SET] };
                    self.alloc_iterator(frame, name)
                }
                None => Err(self.not_iterable(o)),
            },
        };
        // octets and a long text are walked in place and the rest read up front
        let frame = match self.heap.try_get(o) {
            Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => IterFrame::Octets { bytes: b.as_slice().into(), idx: 0 },
            Some(HeapObj::Str(s)) if s.len() > SHORT_TEXT => IterFrame::Chars { text: s.as_str().into(), at: 0 },
            _ => { let items = self.extract_iter(o)?; self.watched(o, items) }
        };
        self.alloc_iterator(frame, name)
    }

    /* whether `o` is a function a class a builtin or a foreign value that calls */
    pub(crate) fn is_callable(&self, o: Val) -> bool {
        matches!(self.heap.try_get(o), Some(HeapObj::Func { .. } | HeapObj::Native(_) | HeapObj::Bound { .. })) || self.heap.is_classlike(o)
            || self.heap.foreign_kind(o).is_some_and(|k| k.call.is_some())
    }

    // one zip step where a strict zip raises once lengths differ
    fn zip_step(&mut self, its: &[Val], strict: bool) -> Result<Option<Val>, VmErr> {
        if its.is_empty() { return Ok(None); }
        // the items taken stay rooted while later steps run user code
        self.with_roots(core::iter::empty(), |vm| {
            let base = vm.temp_roots.len();
            if !strict {
                if !vm.next_of_into(its)? { return Ok(None); }
            } else {
                // a strict zip ends only when every iterable ends together
                for (k, &it) in its.iter().enumerate() {
                    match vm.next_of(it)? {
                        Some(v) => vm.temp_roots.push(v),
                        None if k > 0 => return Err(EngineError::Uneven { arg: k as u32, longer: false }.into()),
                        None => {
                            for (j, &rest) in its.iter().enumerate().skip(1) {
                                if vm.next_of(rest)?.is_some() { return Err(EngineError::Uneven { arg: j as u32, longer: true }.into()); }
                            }
                            return Ok(None);
                        }
                    }
                }
            }
            let items = vm.temp_roots[base..].to_vec();
            vm.heap.alloc(HeapObj::Tuple(items)).map(Some)
        })
    }

    // one enumerate step moving the count on in `frame`
    fn enumerate_step(&mut self, frame: Option<&Rc<RefCell<IterFrame>>>, it: Val, n: Val) -> Result<Option<Val>, VmErr> {
        let Some(item) = self.next_of(it)? else { return Ok(None) };
        // a count past i128 goes on through the wide int kind
        let next = self.with_roots([item], |vm| match vm.as_i128(n).and_then(|i| i.checked_add(1)) { Some(i) => vm.int_to_val(Some(i)), None => vm.add_vals(n, Val::int(1)) })?;
        if let Some(frame) = frame && let IterFrame::Enumerate { n: count, .. } = &mut *frame.borrow_mut() { *count = next; }
        self.heap.alloc(HeapObj::Tuple(alloc::vec![n, item])).map(Some)
    }

    /* roots each iterator's next item and gives false rooting nothing once one is spent */
    fn next_of_into(&mut self, its: &[Val]) -> Result<bool, VmErr> {
        let base = self.temp_roots.len();
        for &it in its {
            match self.next_of(it) {
                Ok(Some(v)) => self.temp_roots.push(v),
                Ok(None) => { self.temp_roots.truncate(base); return Ok(false); }
                Err(e) => { self.temp_roots.truncate(base); return Err(e); }
            }
        }
        Ok(true)
    }
}

/* the state of the builtin iterator `it` or None for any other value */
#[inline(always)]
fn iter_cell<'x>(heap: &HeapPool, it: Val) -> Option<&'x RefCell<IterFrame>> {
    // the heap keeps a live iterator and a step running no user code frees nothing
    match heap.try_get(it) { Some(HeapObj::Iter(g, _)) => Some(pinned(g)), _ => None }
}

impl<'a> VM<'a> {

    /* `iter_step` of an iterator running no user code or None with nothing taken */
    #[inline(never)]
    pub(crate) fn ready_next(&mut self, it: Val) -> Option<Result<Val, VmErr>> {
        let frame = iter_cell(&self.heap, it)?;
        let mut f = frame.try_borrow_mut().ok()?;
        match &*f {
            IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Call { .. } => None,
            IterFrame::Zip { .. } | IterFrame::Enumerate { .. } => {
                let mut seen = ([core::ptr::null(); 8], 1);
                seen.0[0] = core::ptr::from_ref(frame);
                if !self.tree_ready(&f, &mut seen, 0) { return None; }
                Some(self.take_tree(&mut f))
            }
            // a spent one ends in the handler whose step ends it again
            _ => match f.next_item(&mut self.heap) { Ok(Some(v)) => Some(Ok(v)), Ok(None) => None, Err(e) => Some(Err(e)) },
        }
    }

    /* whether `f` steps with no user code or end or failure and each iterator once */
    fn tree_ready(&self, f: &IterFrame, seen: &mut ([*const RefCell<IterFrame>; 8], usize), level: usize) -> bool {
        let leaf = |v: Val, seen: &mut ([*const RefCell<IterFrame>; 8], usize)| {
            let Some(g) = iter_cell(&self.heap, v) else { return false };
            if seen.1 == seen.0.len() || seen.0[..seen.1].contains(&core::ptr::from_ref(g)) { return false; }
            (seen.0[seen.1], seen.1) = (core::ptr::from_ref(g), seen.1 + 1);
            g.try_borrow().is_ok_and(|g| self.tree_ready(&g, seen, level + 1))
        };
        match f {
            IterFrame::Enumerate { it, n } => self.depth + level < self.max_calls && n.is_int() && n.as_int() < Val::INT_MAX && leaf(*it, seen),
            IterFrame::Zip { its, strict: false } => self.depth + level < self.max_calls && !its.is_empty() && its.iter().all(|&v| leaf(v, seen)),
            f => f.ready(&self.heap).is_some(),
        }
    }

    /* the step of `f` that `tree_ready` found ready with iterators stepped in the handler's order */
    fn take_tree(&mut self, f: &mut IterFrame) -> Result<Val, VmErr> {
        let next = |vm: &mut Self, v: Val| -> Result<Val, VmErr> {
            let g = iter_cell(&vm.heap, v).ok_or_else(|| cold_runtime("iterator gone"))?;
            // every iterator of the tree is a distinct one
            vm.take_tree(&mut g.borrow_mut())
        };
        match f {
            IterFrame::Enumerate { it, n } => {
                let item = next(self, *it)?;
                let count = *n;
                *n = Val::int(count.as_int() + 1);
                self.heap.alloc(HeapObj::Tuple(alloc::vec![count, item]))
            }
            IterFrame::Zip { its, .. } => {
                let its = its.clone();
                let mut items = Vec::with_capacity(its.len());
                for &v in its.iter() { items.push(next(self, v)?); }
                self.heap.alloc(HeapObj::Tuple(items))
            }
            f => Ok(f.next_ready(&self.heap).unwrap_or(Val::none())),
        }
    }

    /* the iterator a GetIterR makes pushed for a loop or written to `a` */
    pub(crate) fn reg_iter(&mut self, ins: Ins) -> Result<(), VmErr> {
        let o = self.regs[self.base + ins.b as usize];
        if ins.x as u16 == crate::syntax::parser::ITER_VALUE { self.regs[self.base + ins.a as usize] = self.iter_value(o)?; return Ok(()); }
        self.get_iter(o)
    }

    /* pushes the iterator a loop over `obj` steps */
    pub(crate) fn get_iter(&mut self, obj: Val) -> Result<(), VmErr> {
        let frame = self.make_iter_frame(obj)?;
        self.iter_stack.push(frame);
        Ok(())
    }

    /* what a comprehension takes of `o` as its iterator */
    pub(crate) fn iter_value(&mut self, o: Val) -> Result<Val, VmErr> {
        // a list reads live and the others never change so each goes as it is
        if matches!(self.heap.try_get(o), Some(HeapObj::List(_) | HeapObj::Range(..) | HeapObj::Tuple(_) | HeapObj::Str(_) | HeapObj::Bytes(_))) || self.heap.foreign_fixed(o) { return Ok(o); }
        self.iter_of(o)
    }

    #[inline(never)]
    pub(crate) fn exec_for_iter(&mut self, op: u16, ip: &mut usize, n: usize) -> Result<(), VmErr> {
        match self.for_step()? {
            // the `for` parked with its generator runs again on resume
            Some(item) if item.is_undef() => *ip -= 1,
            Some(item) => self.push(item),
            None => {
                if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                *ip = op as usize;
            }
        }
        Ok(())
    }

    /* the innermost iterator's next item or None once popped or undef while its generator waits */
    #[inline(never)]
    pub(crate) fn for_step(&mut self) -> Result<Option<Val>, VmErr> {
        self.charge_step()?;
        if self.heap.needs_gc() { self.collect_point()?; }
        // the next item or the value `yield from` over this frame gives once it ends
        let step = match self.iter_stack.last() {
            // resuming directly lets `yielded` tell a yielded value even None from exhaustion
            Some(&IterFrame::Coroutine(_)) if self.too_deep() => return Err(crate::vm::cold_depth()),
            Some(&IterFrame::Coroutine(cv)) => {
                self.for_steps = true;
                let result = self.step_coroutine(cv);
                self.for_steps = false;
                let result = result?;
                // a generator waiting parks the `for` in place with a placeholder taking what wakes it
                if self.yielded && self.waits() {
                    if !self.pending.preempt_request && self.top != 0 { self.push(Val::none()); }
                    return Ok(Some(Val::undef()));
                }
                if core::mem::take(&mut self.yielded) { Ok(result) } else { Err(result) }
            }
            // a builtin iterator steps as itself and its end of iteration ends the loop
            Some(&IterFrame::UserDefined(iter)) => {
                let before = self.pending.exc_val;
                match self.iter_step(iter) {
                    Ok(Some(item)) => Ok(item),
                    Ok(None) => Err(Val::none()),
                    Err(e) if is_stop(&e, self.natives.names()) => Err(self.swallow_stop(before)),
                    Err(e) => return Err(e),
                }
            }
            // a `map` or `filter` calls its function whose end of iteration ends the loop
            Some(f @ (IterFrame::Map { .. } | IterFrame::Filter { .. })) => {
                let (state, before) = (f.clone(), self.pending.exc_val);
                // a filter may leave the `for` a generator's next item to run as a frame
                self.step_soft = matches!(state, IterFrame::Filter { .. });
                let stepped = self.called_step(state);
                self.step_soft = false;
                match stepped {
                    Ok(Some(item)) => Ok(item),
                    Ok(None) => Err(Val::none()),
                    Err(e) if is_stop(&e, self.natives.names()) => Err(self.swallow_stop(before)),
                    Err(e) => return Err(e),
                }
            }
            // iterator stack and heap borrow apart so a range step can box a wide int
            _ => match self.iter_stack.last_mut() {
                Some(f) => f.next_item(&mut self.heap)?.ok_or(Val::none()),
                None => Err(Val::none()),
            },
        };
        Ok(match step {
            Ok(item) => Some(item),
            Err(done) => {
                self.yield_from_value = done;
                self.iter_stack.pop();
                None
            }
        })
    }

    pub(crate) fn make_iter_frame(&mut self, obj: Val) -> Result<IterFrame, VmErr> {
        // a new loop's iterator may sit where a freed one `lazy_over` passed by was
        self.lazy_seen = Val::undef();
        if !obj.is_heap() {
            // a foreign kind gives the frame its values step by
            return self.heap.foreign_frame(obj).ok_or_else(|| EngineError::cannot(Act::Iterate, Some(obj)));
        }
        Ok(match self.heap.get(obj) {
            HeapObj::Range(s, e, st) => IterFrame::Range { cur: *s, end: *e, step: *st },
            &HeapObj::BigRange(cur, end, step) => IterFrame::BigRange { cur, end, step },
            HeapObj::List(v) => IterFrame::List { rc: v.clone(), idx: 0 },
            HeapObj::Tuple(v) => IterFrame::Seq { items: v.as_slice().into(), idx: 0 },
            HeapObj::Dict(_) => self.dict_walk(obj, DictPart::Keys),
            HeapObj::Set(s) => { let items = s.borrow().iter().cloned().collect(); self.watched(obj, items) }
            // a text and the octets of bytes are copied once and read one a step
            HeapObj::Str(s) => IterFrame::Chars { text: s.as_str().into(), at: 0 },
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => IterFrame::Octets { bytes: b.as_slice().into(), idx: 0 },
            // a coroutine steps only under `await`
            HeapObj::Coroutine(..) if !self.is_async_object(obj) => return Ok(IterFrame::Coroutine(obj)),
            // a builtin iterator advances in place so the loop spends it
            HeapObj::Iter(frame, _) => return Ok(match &*frame.borrow() {
                f @ (IterFrame::Map { .. } | IterFrame::Filter { .. }) => f.clone(),
                _ => IterFrame::UserDefined(obj),
            }),
            _ => return Err(self.not_iterable(obj)),
        })
    }

    /* `it` as op `rip` takes it or None when a loop job takes its items */
    #[cold]
    #[inline(never)]
    pub(crate) fn lazy_items(&mut self, it: Val, rip: usize) -> Result<Option<Val>, VmErr> {
        if !matches!(self.heap.try_get(it), Some(HeapObj::Iter(..))) { return Ok(Some(it)); }
        // a register op takes the list in `retake` as it runs again
        if self.job_to == crate::vm::act::Dest::Retake {
            if self.list_job_of(it, rip)? { return Ok(None); }
            // a list gathered at once lies in `retake` as a job would leave it
            return Ok(Some(match core::mem::replace(&mut self.retake, Val::undef()) { v if v.is_undef() => it, v => v }));
        }
        self.push(it);
        if self.list_job(rip)? { return Ok(None); }
        self.pop().map(Some)
    }

    /* plain or starred unpacking with `after` targets past the star */
    pub(crate) fn unpack_iterable(&mut self, n: usize, after: Option<usize>, rip: Option<usize>) -> Result<(), VmErr> {
        let obj = self.pop()?;
        if let Some(vals) = self.unpack_into(obj, n, after, rip)? { for v in vals { self.push(v); } }
        Ok(())
    }

    // unpacks the source of `ins` into its targets a lazy source running as frames first
    #[inline(never)]
    pub(crate) fn unpack_reg(&mut self, ins: crate::code::Ins, rip: usize, chunk: &crate::syntax::chunk::SSAChunk, n: usize, after: Option<usize>) -> Result<(), VmErr> {
        let pair = ins.op == crate::code::OpCode::UnpackR && ins.x != 0;
        // the items frames gathered of a lazy source come back as the op runs again
        let obj = match core::mem::replace(&mut self.retake, Val::undef()) { v if v.is_undef() => self.reg(chunk, if pair { ins.c } else { ins.b })?, v => v };
        let Some(vals) = self.delivering(crate::vm::act::Dest::Retake, |vm| vm.unpack_into(obj, n, after, Some(rip)))? else { return Ok(()) };
        // a pair sets both targets and a row form lays them from `a` deepest first
        if pair { (self.regs[self.base + ins.a as usize], self.regs[self.base + ins.b as usize]) = (vals[1], vals[0]); return Ok(()); }
        let at = self.base + ins.a as usize;
        self.regs[at..at + vals.len()].copy_from_slice(&vals);
        Ok(())
    }

    // the values of `obj` deepest first and none once a job takes them
    pub(crate) fn unpack_into(&mut self, obj: Val, n: usize, after: Option<usize>, rip: Option<usize>) -> Result<Option<Vec<Val>>, VmErr> {
        let open = rip.is_some();
        let (items, sized) = match self.heap.try_get(obj) {
            Some(HeapObj::Tuple(t)) if t.len() <= 64 => (t.clone(), true),
            Some(HeapObj::List(l)) if l.borrow().len() <= 64 => (l.borrow().clone(), true),
            // a long one copies as iteration does refused past the memory left and paid
            Some(HeapObj::Tuple(_) | HeapObj::List(_)) => (self.extract_iter(obj)?, true),
            // a generator runs as a loop frame where allowed and its items rerun the op
            Some(HeapObj::Coroutine(_)) if open => {
                // a plain unpack takes one item past its targets to find there are too many
                let limit = if after.is_none() { (n + 1).min(u16::MAX as usize) as u16 } else { 0 };
                let stacked = self.job_to != crate::vm::act::Dest::Retake;
                if stacked { self.push(obj); }
                if self.retry_of(obj, limit)? { return Ok(None); }
                if stacked { self.pop()?; }
                (self.extract_iter(obj)?, false)
            }
            // a `map` or `filter` whose steps run user code unpacks the items a job takes
            Some(HeapObj::Iter(..)) if let Some(rip) = rip => match self.lazy_items(obj, rip)? {
                Some(obj) => (self.extract_iter(obj)?, false),
                None => return Ok(None),
            },
            _ => (self.extract_iter(obj)?, false),
        };
        let tail = after.unwrap_or(0);
        if items.len() < n + tail {
            return Err(EngineError::Unpack { want: (n + tail) as u32, got: Some(items.len() as u32), more: false, rest: after.is_some() }.into());
        }
        // only a list or tuple tells how many it had
        if after.is_none() && items.len() > n {
            return Err(EngineError::Unpack { want: n as u32, got: sized.then_some(items.len() as u32), more: true, rest: false }.into());
        }
        let mid = items.len() - tail;
        // the starred list copies the middle again so a long one is checked first
        if mid - n > 64 { self.heap.reserve((mid - n).saturating_mul(VAL_BYTES))?; }
        let star = if after.is_some() { Some(self.alloc_list(items[n..mid].to_vec())?) } else { None };
        Ok(Some(items[mid..].iter().rev().copied().chain(star).chain(items[..n].iter().rev().copied()).collect()))
    }
}
