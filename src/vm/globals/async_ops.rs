use alloc::{vec, vec::Vec};

use crate::parser::{OpCode, SSAChunk};
use super::super::VM;
use super::super::types::*;
use super::super::lower::Code;

/* The helpers the module body waits inside, opened as frames of the loop resuming it, and the ones running with more kept under them by depth. */
pub(crate) struct Chain { pub(crate) ask: bool, pub(crate) next: bool, pub(crate) coro: Val, pub(crate) chains: Vec<(usize, Val, usize)> }

impl Default for Chain {
    fn default() -> Self { Self { ask: false, next: false, coro: Val::undef(), chains: Vec::new() } }
}

/* An ip no chunk reaches, the same on 32 and 64 bits so a snapshot keeps it. */
pub(crate) const FINISHED: usize = u32::MAX as usize;

impl<'a> VM<'a> {

    /* resumes the module body `callee` the host drives until it returns or raises or waits */
    pub(crate) fn resume_root(&mut self, callee: Val) -> Result<Val, VmErr> {
        (self.redo.on, self.redo.lazy) = (false, false);
        let resume_safe = core::mem::take(&mut self.pending_exec_safe);
        self.for_steps = false;
        self.wake_gen(callee, false);
        let (outer_ip, outer_layout, syncs, held) = match self.heap.get(callee) {
            HeapObj::Coroutine(c) => (c.ip, c.layout, c.syncs.len(), c.syncs.len() + c.stack.len() + c.slots.len() + c.iters.len()),
            _ => return Err(cold_type("not a coroutine")),
        };
        self.chain.ask = false;
        // the helpers it waits inside open as frames of its loop innermost first
        let outer_layout = if syncs == 0 { outer_layout } else {
            let Some(l) = self.chains_from(callee, BodyRef::Module, outer_ip, outer_layout) else { return Err(cold_runtime("a body waiting inside helpers that open no frame")) };
            if self.chain_refused(callee) { return Err(cold_depth()); }
            l
        };
        self.evict_coro(callee);
        if outer_ip == FINISHED { self.yielded = false; return Ok(Val::none()); }
        if matches!(self.heap.try_get(callee), Some(HeapObj::Coroutine(c)) if c.running) { return Err(self.running_error(callee)); }
        self.charge_steps(held)?;
        // its state moves out while it runs and a yield puts it back with the helpers it waits inside
        let (mut slots, mut stack, mut iters, mut excs, mut withs) = match self.heap.get_mut(callee) {
            HeapObj::Coroutine(c) => { c.running = true; (core::mem::take(&mut c.slots), core::mem::take(&mut c.stack), core::mem::take(&mut c.iters), core::mem::take(&mut c.excs), core::mem::take(&mut c.withs)) }
            _ => return Err(cold_type("not a coroutine")),
        };
        let (stack_len, iter_len, exc_len, with_len) = self.restore_into(&mut stack, &mut iters, &mut excs, &mut withs);
        let saved_resume_ip = self.resume_ip;
        self.yielded = false;
        self.depth += 1;
        self.pending_exec_exc_base = Some(exc_len);
        self.pending_exec_safe = resume_safe;
        if syncs != 0 { (self.chain.next, self.chain.coro) = (true, callee); }
        self.resume_layout = outer_layout;
        self.resume_ip = outer_ip;
        let pool = self.pool_of(self.chunk);
        let result = self.exec_pooled(self.chunk, pool, &mut slots, false);
        self.depth -= 1;
        let finished = match &result { Ok(_) => !self.yielded, Err(e) => !matches!(e, VmErr::HostYield(_)) };
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(callee) { c.running = false; if finished { c.ip = FINISHED; } }
        let result = match result {
            Ok(v) => v,
            Err(e) => {
                self.stack.truncate(stack_len.min(self.stack.len()));
                self.iter_stack.truncate(iter_len.min(self.iter_stack.len()));
                self.exception_stack.truncate(exc_len.min(self.exception_stack.len()));
                self.with_stack.truncate(with_len.min(self.with_stack.len()));
                (self.resume_raise, self.resume_ip) = (None, saved_resume_ip);
                return Err(e);
            }
        };
        if !self.yielded {
            self.stack.truncate(stack_len);
            self.iter_stack.truncate(iter_len);
            self.exception_stack.truncate(exc_len);
            self.resume_ip = saved_resume_ip;
            return Ok(result);
        }
        let layout = core::mem::take(&mut self.resume_layout);
        self.save_frames(stack_len, iter_len, exc_len, with_len, &mut stack, &mut iters, &mut excs, &mut withs);
        let newer = core::mem::take(&mut self.pending_sync_frames);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(callee) {
            (c.ip, c.layout, c.slots, c.stack, c.iters, c.excs, c.withs) = (self.resume_ip, layout, slots, stack, iters, excs, withs);
            // the helpers a chained run left kept lie under the ones that parked
            c.syncs.extend(newer.into_iter().rev());
        }
        self.resume_ip = saved_resume_ip;
        Ok(result)
    }

    /* The item `co` keeps pending, the next it holds taking its place. */
    pub(crate) fn take_pending(&mut self, co: Val) -> Val {
        let next = match self.held.iter().position(|h| h.0.0 == co.0) {
            Some(k) => { let v = self.held[k].1.pop().unwrap_or(Val::undef()); if self.held[k].1.is_empty() { self.held.swap_remove(k); } v }
            None => Val::undef(),
        };
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return Val::none() };
        core::mem::replace(&mut c.pending, next)
    }

    /* One step of generator `co` for a builtin or `for`, None when it finished already, raising as its resume would when it runs already. */
    #[inline]
    pub(crate) fn step_coroutine(&mut self, co: Val) -> Result<Val, VmErr> {
        match self.heap.try_get(co) {
            // One whose next item a builtin's step took ahead, or whose items a job held, hands that item out as its step.
            Some(HeapObj::Coroutine(c)) if !c.pending.is_undef() => { self.yielded = true; return Ok(self.take_pending(co)); }
            Some(HeapObj::Coroutine(c)) if c.ip == FINISHED => { self.yielded = false; return Ok(Val::none()); }
            Some(HeapObj::Coroutine(c)) if c.running => return Err(self.running_error(co)),
            _ => {}
        }
        Err(self.resumed_apart())
    }

    /* error of a generator resumed outside the loop which debug builds treat as a bug */
    #[cold]
    #[inline(never)]
    fn resumed_apart(&self) -> VmErr {
        debug_assert!(false, "a generator resumed outside the loop");
        cold_runtime("a generator resumed outside the loop")
    }

    /* The layout `body` of `co`, suspended at `ip` in helpers laid out as `layout`, resumes in once it opens them as frames, None when it cannot. */
    #[cold]
    #[inline(never)]
    fn chains_from(&mut self, co: Val, body: BodyRef, ip: usize, layout: u32) -> Option<u32> {
        if ip & crate::vm::dispatch::RESUME_RAW != 0 { return None; }
        if !matches!(self.heap.try_get(co), Some(HeapObj::Coroutine(_))) { return None; }
        let (chunk, pool): (&SSAChunk, usize) = match body { BodyRef::Fn(fi) => (&self.functions[fi].1, self.fn_pool[fi]), BodyRef::Module => (self.chunk, self.pool_of(self.chunk)) };
        // A body still in its first code resumes in that code made again with the compiler's resume points, a cold one heating once more as it does.
        let p = &self.pools[pool];
        if if p.code.is_none() { p.heat < super::super::lower::HOT_LOOP } else { p.cold && p.heat + 1 < super::super::lower::HOT_LOOP } { return Some(layout); }
        // The resume code is made now as its run would make it, kept for that run by its layout, the helpers chaining when they go on at one point.
        (self.resume_ip, self.resume_layout) = (ip, layout);
        let (code, once) = self.frame_code(chunk, pool);
        (self.resume_ip, self.resume_layout) = (0, 0);
        if code.resume(code.lowered(ip)) != ip { return None; }
        let laid = code.layout;
        if once { self.retired.push(code); }
        Some(laid)
    }

    /* whether the helpers `co` waits inside are too many or keep too much to open as frames */
    pub(crate) fn chain_refused(&self, co: Val) -> bool {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return false };
        if c.syncs.len() >= crate::vm::act::SUSPENDED_DEPTH { return true; }
        // What they keep is counted only as far as the bound, a long chain of helpers keeping little looked at once each.
        let mut kept = c.stack.len();
        for f in c.syncs.iter().rev() {
            if kept >= crate::vm::act::SUSPENDED_DEPTH { return true; }
            kept += f.stack_delta.len();
        }
        kept >= crate::vm::act::SUSPENDED_DEPTH
    }

    /* Opens the innermost helper coroutine `co` still waits inside, carrying on at `ret`, with `v` its call's result, giving where it carries on. */
    pub(crate) fn chain_next(&mut self, co: Val, ret: usize, v: Option<Val>) -> usize {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return ret };
        let Some(f) = c.syncs.pop() else { return ret };
        let more = !c.syncs.is_empty();
        let code = self.sync_code(&f);
        let at = self.enter_sync(f, code, ret);
        if let Some(v) = v { self.push(v); }
        if more {
            let h = self.head_at(self.top_pool);
            self.regs[h] = Val::int(self.regs[h].as_int() | crate::vm::act::TASK << 40);
            self.chain.chains.push((self.acts, co, ret));
        }
        at
    }

    /* The coroutine and its frame's return point when the running frame is a helper it waits inside with more kept under it, whose mark goes. */
    #[inline(never)]
    pub(crate) fn chain_top(&mut self) -> Option<(Val, usize)> {
        let &(depth, co, ret) = self.chain.chains.last()?;
        if depth != self.acts { return None; }
        self.chain.chains.pop();
        let h = self.head_at(self.top_pool);
        self.regs[h] = Val::int(self.regs[h].as_int() & !(crate::vm::act::TASK << 40));
        Some((co, ret))
    }

    /* `chain_next` after the top helper raised `e` out of `fi`, the helper under it opening and the error rising at its call, noted in the traceback. */
    #[inline(never)]
    pub(crate) fn chain_raise(&mut self, co: Val, ret: usize, fi: usize) -> usize {
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) && let Some(f) = c.syncs.last() {
            let (ip, caller) = (f.ip, &self.functions[f.fi].1);
            self.note_frame(fi, Some((ip & !crate::vm::dispatch::RESUME_RAW).saturating_sub(1) as u32), caller);
        }
        self.chain_next(co, ret, None)
    }

    /* The code helper `f` carries on in, the code it suspended in while one keeps it, else the code a resume runs it in, kept while it runs. */
    fn sync_code(&mut self, f: &ParkedFrame) -> *const Code {
        if let Some(k) = self.code_laid(f.fi, f.layout) {
            // SAFETY a code the pool or the retired keep lives as long as the VM.
            if !unsafe { (*k).unchanged() } { return k; }
            // One in its first code heats as its resume would heat it, lowered once hot, its resume points the compiler's own.
            let pool = self.fn_pool[f.fi];
            self.pools[pool].heat += 1;
            if self.pools[pool].heat < crate::vm::lower::HOT_LOOP { return k; }
            let chunk = &self.functions[f.fi].1;
            return alloc::rc::Rc::as_ptr(&self.lowered_code(chunk, pool, false));
        }
        let (chunk, pool) = (&self.functions[f.fi].1, self.fn_pool[f.fi]);
        (self.resume_ip, self.resume_layout) = (f.ip, f.layout);
        let (code, once) = self.frame_code(chunk, pool);
        (self.resume_ip, self.resume_layout) = (0, 0);
        let k = alloc::rc::Rc::as_ptr(&code);
        if once { self.retired.push(code); }
        k
    }

    /* Opens helper `f` a coroutine waits inside as a frame of the loop over its caller, which goes on at `ret` with its result, giving where it goes. */
    fn enter_sync(&mut self, f: ParkedFrame, code: *const Code, ret: usize) -> usize {
        let ParkedFrame { ip, fi, func, slots, mut stack_delta, mut iter_delta, mut exception_delta, mut withs, instance, .. } = f;
        let pool = self.fn_pool[fi];
        let chunk: &SSAChunk = &self.functions[fi].1;
        let names = chunk.names.len();
        let base = self.open_window(pool, slots.len().max(names), names, false);
        // Its constants and state may come from a restore or a later window, so it takes them again before its frame opens.
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        self.pools[pool].wcode = 0;
        self.note_window(pool);
        self.regs[base..base + slots.len()].copy_from_slice(&slots);
        // SAFETY the pool holds the code for as long as the VM lives.
        let cold = self.cold_bases(unsafe { (*code).flat() } && iter_delta.is_empty() && exception_delta.is_empty());
        let stack_base = self.stack.len();
        self.restore_into(&mut stack_delta, &mut iter_delta, &mut exception_delta, &mut withs);
        self.open_helper(fi, func, pool, code, cold, ret, stack_base, instance);
        self.run_in(pool);
        // SAFETY as above.
        let c = unsafe { &*code };
        if !self.laid(c, pool) { self.grow_frame(chunk, c, pool); }
        if ip & crate::vm::dispatch::RESUME_RAW != 0 { ip & !crate::vm::dispatch::RESUME_RAW } else { c.lowered(ip) }
    }

    /* The code of function `fi` laid out as `layout`, which its pool holds or retired. */
    pub(crate) fn code_laid(&self, fi: usize, layout: u32) -> Option<*const Code> {
        if layout == 0 { return None; }
        self.pools[self.fn_pool[fi]].code.iter().chain(&self.retired).find(|c| c.layout == layout).map(alloc::rc::Rc::as_ptr)
    }

    /* Whether the yield that just happened waits for something, an event, a host call, a preemption or a parked helper. */
    #[inline]
    pub(crate) fn waits(&self) -> bool {
        let p = &self.pending;
        p.event_wait_request || p.host_call_request || p.preempt_request || !self.pending_sync_frames.is_empty()
    }

    /* The error resuming `g` from its own body raises. */
    #[cold]
    fn running_error(&self, g: Val) -> VmErr {
        match self.coro_kind(g) {
            Some(CoroKind::Coroutine) => VmErr::Value("coroutine already executing"),
            Some(CoroKind::AsyncGen) => VmErr::Raised("RuntimeError: anext(): asynchronous generator is already running".into()),
            _ => VmErr::Value("generator already executing"),
        }
    }

    /* The error a suspension inside a generator raises, its requests dropped. */
    #[cold]
    #[inline(never)]
    pub(crate) fn suspended_in_generator(&mut self, fi: Option<usize>) -> VmErr {
        let _ = fi;
        self.suspended_in(" a generator or a coroutine")
    }

    /* The error a suspension inside `what` raises, its requests dropped. */
    #[cold]
    #[inline(never)]
    pub(crate) fn suspended_in(&mut self, what: &str) -> VmErr {
        let p = &mut self.pending;
        (p.event_wait_request, p.host_call_request, p.preempt_request) = (false, false, false);
        self.pending_sync_frames.clear();
        // The run carries on raising, not from where it would have resumed.
        (self.resume_ip, self.resume_layout) = (0, 0);
        VmErr::Raised(crate::s!("RuntimeError: receive() and host calls cannot wait inside", str what))
    }

    /* Steps the module body until it returns, raises or waits on the host, a wait handed out as a HostYield the next `run()` resumes. */
    pub(crate) fn drive_root(&mut self) -> Result<Val, VmErr> {
        loop {
            let Some(root) = self.root.as_mut() else { return Ok(Val::none()) };
            let coro = root.coro;
            match core::mem::replace(&mut root.state, CoroState::Ready) {
                CoroState::Ready => {}
                CoroState::Raising(e, exc) => {
                    self.resume_raise = Some(e);
                    self.pending.exc_val = exc;
                }
                wait => {
                    let status = if matches!(wait, CoroState::WaitingEvent) { SchedulerStatus::PendingEvent } else { SchedulerStatus::PendingHostCall };
                    root.state = wait;
                    return Err(VmErr::HostYield(status));
                }
            }
            self.pending.clear_waits();
            self.pending_exec_safe = true;
            self.chain.ask = true;
            let result = self.resume_root(coro);
            // A resume that failed early never reached the park point, so its raise must not leak.
            self.resume_raise = None;
            let yielded = core::mem::take(&mut self.yielded);
            let state = match result {
                Ok(_) if yielded => {
                    if core::mem::take(&mut self.pending.event_wait_request) { CoroState::WaitingEvent }
                    else if core::mem::take(&mut self.pending.host_call_request) { CoroState::WaitingHostCall(self.pending.host_call_id) }
                    else { CoroState::Ready }
                }
                done => { self.root = None; return done; }
            };
            if let Some(root) = self.root.as_mut() { root.state = state; }
            if core::mem::take(&mut self.pending.preempt_request) { return Err(VmErr::HostYield(SchedulerStatus::Preempted)); }
        }
    }

    /* Pop oldest queued message, if empty, park in `WaitingEvent` until `run_push_event`. */
    pub fn call_receive(&mut self) -> Result<(), VmErr> {
        if self.event_queue.is_empty() {
            self.pending.event_wait_request = true;
            self.push(Val::none());
            self.yielded = true;
        } else {
            let val = self.event_queue.remove(0);
            self.push(val);
        }
        Ok(())
    }

    /* `send(group, body)` hands one message to the host's actor scheduler, the counterpart of `receive()`. */
    pub fn call_send(&mut self) -> Result<(), VmErr> {
        let body = self.pop()?;
        let group = self.pop()?;
        let group = self.str_of(group, "send() expects a str at argument 1")?;
        let body = self.str_of(body, "send() expects a str at argument 2")?;
        match self.send_hook {
            Some(hook) if hook(&group, &body) => {
                self.push(Val::none());
                Ok(())
            }
            _ => Err(VmErr::Runtime("send() needs an actor scheduler, missing in this runtime")),
        }
    }

    /* Whether `g` is a generator, a coroutine or an async generator, None for anything else. */
    pub(crate) fn coro_kind(&self, g: Val) -> Option<CoroKind> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(g) else { return None };
        Some(match c.body {
            BodyRef::Fn(fi) if !self.is_async_fn(fi) => CoroKind::Generator,
            BodyRef::Fn(fi) if self.functions[fi].1.is_generator => CoroKind::AsyncGen,
            _ => CoroKind::Coroutine,
        })
    }

    /* The instructions of a coroutine body. */
    fn body_code(&self, body: BodyRef) -> &[crate::parser::types::Instruction] {
        match body { BodyRef::Fn(fi) => &self.functions[fi].1.instructions, BodyRef::Module => &self.chunk.instructions }
    }

    /* What `g` waits inside, the iterator a `yield from`, an `await` or an `async for` steps, undef for a builtin iterator. */
    pub(crate) fn delegate_of(&self, g: Val) -> Option<(Val, Via)> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(g) else { return None };
        if c.ip == 0 || c.ip == FINISHED || !c.syncs.is_empty() { return None; }
        let code = self.body_code(c.body);
        match code.get(c.ip).map(|i| i.opcode) {
            Some(OpCode::Await) => return c.stack.len().checked_sub(2).map(|k| (c.stack[k], Via::Await)),
            Some(OpCode::ForIter) => if let Some(&IterFrame::AsyncFor { aw, .. }) = c.iters.last() && !aw.is_undef() { return Some((aw, Via::AsyncFor)); },
            _ => {}
        }
        // A `yield from` suspends past the PopTop that follows its marked yield.
        let from = c.ip >= 2 && code.get(c.ip - 2).is_some_and(|i| i.opcode == OpCode::Yield && i.operand == crate::parser::YIELD_FROM);
        if !from { return None; }
        match c.iters.last() {
            Some(&(IterFrame::Coroutine(v) | IterFrame::UserDefined(v))) => Some((v, Via::YieldFrom)),
            Some(_) => Some((Val::undef(), Via::YieldFrom)),
            None => None,
        }
    }

    /* The step of the awaitable at `at` an `await` sends `sent` into, as `aw_step` takes it. */
    pub(crate) fn await_on(&mut self, at: usize, sent: Val, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> { self.aw_step(self.stack[at], sent, false, chunk) }

    /* The step of `sub` an `await` or `async for` (`looped`) sends `sent` into, None once it opened as a frame coming back with `await_back` set. */
    pub(crate) fn aw_step(&mut self, sub: Val, sent: Val, looped: bool, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> {
        match core::mem::take(&mut self.await_back) {
            1 => return Ok(Some(Step::Yield(sent))),
            2 => return Ok(Some(Step::Return(sent))),
            // An async generator's yield is the awaitable's value, or passes up from an await inside it.
            4 => {
                let g = self.awaitable_frame(sub).map_or(sub, |(g, ..)| g);
                if self.delegate_of(g).is_some() { return Ok(Some(Step::Yield(sent))); }
                self.set_stage(sub, 2);
                return Ok(Some(Step::Return(sent)));
            }
            6 => return Err(self.raise_named("StopAsyncIteration", chunk)),
            5 => {
                let before = self.pending.exc_val;
                let ended = Err(self.raise_named("StopAsyncIteration", chunk));
                return match self.awaitable_frame(sub) { Some((_, mode, _, default, _)) => self.aw_after(sub, ended, mode, default, before).map(Some), None => ended.map(Some) };
            }
            _ => {}
        }
        // An `async for` steps an async generator itself.
        if self.coro_kind(sub) == Some(CoroKind::AsyncGen) && !self.executing_coros.contains(&sub.0) {
            let before = self.pending.exc_val;
            self.coro_frame = Some((sub, crate::vm::jobs::Takes::Agen, before.unwrap_or(Val::undef())));
            let r = self.with_roots([sub], |vm| vm.gen_resume(sub, Sent::Value(sent), chunk));
            self.coro_frame = None;
            if matches!(r, Ok(Outcome::Opened)) { return Ok(None); }
            return self.agen_after(sub, r, false, before, chunk).map(Some);
        }
        if let Some((g, mode, arg, default, stage)) = self.awaitable_frame(sub) && matches!(mode, AwaitMode::Send | AwaitMode::Throw) && stage < 2
            && (stage == 1 || sent.is_none()) && self.coro_kind(g) == Some(CoroKind::AsyncGen) && !self.executing_coros.contains(&g.0) {
            let sent = if stage == 1 { Sent::Value(sent) } else if mode == AwaitMode::Send { Sent::Value(arg) } else { Sent::Throw(arg) };
            self.set_stage(sub, 1);
            let before = self.pending.exc_val;
            self.coro_frame = Some((g, crate::vm::jobs::Takes::Agen, before.unwrap_or(Val::undef())));
            let r = self.with_roots([sub], |vm| vm.gen_resume(g, sent, chunk));
            self.coro_frame = None;
            if matches!(r, Ok(Outcome::Opened)) { return Ok(None); }
            let step = self.agen_after(g, r, false, before, chunk);
            return self.aw_after(sub, step, mode, default, before).map(Some);
        }
        // A coroutine's `__await__` steps the coroutine itself.
        let sub = match self.awaitable_frame(sub) { Some((g, AwaitMode::Wrap, ..)) => g, _ => sub };
        // A generator an `__await__` gave, or a coroutine awaited, resumes as its `send` resumes it, its frame the job's.
        if matches!(self.coro_kind(sub), Some(CoroKind::Generator | CoroKind::Coroutine)) {
            let before = self.pending.exc_val.unwrap_or(Val::undef());
            self.coro_frame = Some((sub, if looped { crate::vm::jobs::Takes::AwaitFor } else { crate::vm::jobs::Takes::Await }, before));
            let r = self.with_roots([sub], |vm| vm.gen_resume(sub, Sent::Value(sent), chunk));
            self.coro_frame = None;
            return Ok(match r? { Outcome::Opened => None, Outcome::Yielded(v) => Some(Step::Yield(v)), Outcome::Returned(v) => Some(Step::Return(v)) });
        }
        // Any other awaitable steps its target as a frame through `aw_on`, its job giving the step back to the op.
        if self.awaitable_frame(sub).is_some() {
            let before = self.pending.exc_val.unwrap_or(Val::undef());
            self.coro_frame = Some((sub, if looped { crate::vm::jobs::Takes::AwaitFor } else { crate::vm::jobs::Takes::Await }, before));
            let r = self.with_roots([sub], |vm| vm.aw_on(sub, Sent::Value(sent), true, chunk));
            self.coro_frame = None;
            return r;
        }
        self.gen_step(sub, Sent::Value(sent), chunk).map(Some)
    }

    /* One step of what an `await`, a `yield from` or a driver steps, `sent` handed in. */
    pub(crate) fn gen_step(&mut self, it: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        match self.coro_kind(it) {
            // An async generator steps the way `__anext__` does, and nothing closes it but `aclose`.
            Some(CoroKind::AsyncGen) if matches!(sent, Sent::Close) => Ok(Step::Return(Val::none())),
            Some(CoroKind::AsyncGen) => self.agen_step(it, sent, false, chunk),
            Some(_) => Ok(match self.gen_resume(it, sent, chunk)? { Outcome::Yielded(v) => Step::Yield(v), Outcome::Returned(v) => Step::Return(v), Outcome::Opened => return Err(cold_runtime("a generator opened past its call")) }),
            None if self.awaitable_frame(it).is_some() => self.awaitable_step(it, sent, chunk),
            None => self.iter_send(it, sent, chunk),
        }
    }

    /* Resumes generator or coroutine `g` with `sent`, reaching what it waits inside first. */
    fn gen_resume(&mut self, g: Val, sent: Sent, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let kind = self.coro_kind(g).ok_or_else(|| cold_type("not a generator"))?;
        let (ip, body, running) = match self.heap.get(g) { HeapObj::Coroutine(c) => (c.ip, c.body, c.running), _ => return Err(cold_type("not a generator")) };
        if running || self.executing_coros.contains(&g.0) { return Err(self.running_error(g)); }
        if ip == FINISHED {
            // What a job held of it for a builtin that stopped short goes on as its steps.
            if matches!(sent, Sent::Value(_)) && matches!(self.heap.get(g), HeapObj::Coroutine(c) if !c.pending.is_undef()) { return Ok(Outcome::Yielded(self.take_pending(g))); }
            return match sent {
                Sent::Close => Ok(Outcome::Returned(Val::none())),
                _ if kind == CoroKind::Coroutine => Err(VmErr::Raised("RuntimeError: cannot reuse already awaited coroutine".into())),
                Sent::Throw(e) => Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))),
                Sent::Value(_) => Ok(Outcome::Returned(Val::none())),
            };
        }
        if ip == 0 {
            match sent {
                Sent::Value(v) if !v.is_none() => return Err(VmErr::TypeMsg(crate::s!("can't send non-None value to a just-started ", str kind.noun()))),
                Sent::Close => { self.finish_generator(g); return Ok(Outcome::Returned(Val::none())); }
                Sent::Throw(e) => { self.finish_generator(g); return Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))); }
                Sent::Value(_) => return self.step_coro(g),
            }
        }
        if let Some((sub, via)) = self.delegate_of(g) {
            let sub_step = match sent {
                // An `await` and an `async for` hand a sent value on themselves, as does a `yield from` resumed by `next`.
                Sent::Value(v) if via != Via::YieldFrom || v.is_none() => None,
                Sent::Value(v) => Some(self.delegate_on(sub, Sent::Value(v), Cont::After(g, via), chunk)),
                // GeneratorExit closes what it waits inside, then raises in `g`.
                Sent::Throw(e) if self.exc_name_of(e) == Some("GeneratorExit") => match self.delegate_on(sub, Sent::Close, Cont::Exit(g, via, e), chunk) {
                    Ok(None) => return Ok(Outcome::Opened),
                    r => r.err().map(Err),
                },
                Sent::Throw(e) => Some(self.delegate_on(sub, Sent::Throw(e), Cont::After(g, via), chunk)),
                // What closing the inner one raises is raised in `g` in place of GeneratorExit.
                Sent::Close => match self.delegate_on(sub, Sent::Close, Cont::Close(g, kind), chunk) {
                    Ok(None) => return Ok(Outcome::Opened),
                    Err(e) => return self.close_in(g, Some(e), kind, chunk),
                    Ok(Some(_)) => None,
                },
            };
            match sub_step {
                Some(Ok(None)) => return Ok(Outcome::Opened),
                Some(r) => return self.after_delegate(g, via, r.map(|s| s.unwrap_or(Step::Return(Val::none()))), chunk),
                None => {}
            }
        }
        match sent {
            Sent::Value(v) => {
                // A value-position `yield` left a placeholder on the saved stack that the sent value takes.
                let code = self.body_code(body);
                let at_yield = ip > 0 && code.get(ip - 1).is_some_and(|i| i.opcode == OpCode::Yield) || self.delegate_of(g).is_some_and(|(_, via)| via != Via::YieldFrom);
                if at_yield && let HeapObj::Coroutine(c) = self.heap.get_mut(g) && c.syncs.is_empty() && let Some(top) = c.stack.last_mut() { *top = v; }
                self.step_coro(g)
            }
            Sent::Throw(e) => {
                let err = self.with_roots([e], |vm| vm.raise_value(e, chunk));
                self.throw_in(g, err)
            }
            Sent::Close => self.close_in(g, None, kind, chunk),
        }
    }

    /* `delegate`, None once `sub` opened as a frame whose end runs `cont`, which a frame the call names asks for. */
    fn delegate_on(&mut self, sub: Val, sent: Sent, cont: Cont, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> {
        if self.coro_frame.is_some() && self.awaitable_frame(sub).is_some() {
            let Some((_, takes, before)) = self.coro_frame else { return Ok(None) };
            let r = self.aw_on(sub, sent, true, chunk)?;
            if r.is_none() { self.conts.push(Then { cont, takes, before }); }
            return Ok(r);
        }
        // A user iterator's `__next__`, `send`, `throw` or `close` runs as a frame of a job whose step the wait takes.
        if let Some((_, takes, before)) = self.coro_frame && matches!(takes, crate::vm::jobs::Takes::Next(_) | crate::vm::jobs::Takes::Close) && self.coro_kind(sub).is_none() && let Some((f, defining, arg)) = self.sent_hook(sub, sent) {
            let base = self.conts.len() as u32;
            let job = alloc::boxed::Box::new(SentCall { it: sub, f, defining, arg, close: matches!(sent, Sent::Close), base, before: self.pending.exc_val });
            if self.calls_run(job, true)?.is_none() { self.conts.push(Then { cont, takes, before }); return Ok(None); }
            return match self.then_out.take() { Some((_, step)) => step.map(Some), None => Err(cold_runtime("a delegation gave no step")) };
        }
        let Some((_, takes, before)) = self.coro_frame.filter(|_| matches!(self.coro_kind(sub), Some(CoroKind::Generator | CoroKind::Coroutine))) else { return self.delegate(sub, sent, chunk).map(Some) };
        self.then_ok = true;
        let r = self.with_roots([sub], |vm| vm.gen_resume(sub, sent, chunk));
        self.then_ok = false;
        match r? {
            Outcome::Opened => { self.conts.push(Then { cont, takes, before }); Ok(None) }
            Outcome::Yielded(v) => Ok(Some(Step::Yield(v))),
            Outcome::Returned(v) => Ok(Some(Step::Return(v))),
        }
    }

    /* What `cont` makes of `r`, the step of the frame it waited on, as the call that delegated would have. */
    fn run_cont(&mut self, cont: Cont, r: Result<Step, VmErr>) -> Result<Outcome, VmErr> {
        let chunk = self.chunk;
        match (cont, r) {
            (Cont::After(g, via), r) | (Cont::Exit(g, via, _), r @ Err(_)) => self.after_delegate(g, via, r, chunk),
            (Cont::Exit(g, _, e), Ok(_)) => { let err = self.with_roots([e], |vm| vm.raise_value(e, chunk)); self.throw_in(g, err) }
            (Cont::Close(g, kind), Err(e)) => self.close_in(g, Some(e), kind, chunk),
            (Cont::Close(g, kind), Ok(_)) => self.close_in(g, None, kind, chunk),
            (Cont::Aw(it), r) => {
                let Some((target, mode, _, default, _)) = self.awaitable_frame(it) else { return Err(cold_type("not an awaitable")) };
                let r = r.map(|s| match s { Step::Yield(v) => Outcome::Yielded(v), Step::Return(v) => Outcome::Returned(v) });
                let step = self.aw_inner(target, mode, r, None, chunk);
                Ok(match self.aw_after(it, step, mode, default, None)? { Step::Yield(v) => Outcome::Yielded(v), Step::Return(v) => Outcome::Returned(v) })
            }
        }
    }

    /* Where the loop goes once a `Then` job's frame gave `r`, running the waits from `base` up to the next frame they open, else the result at `ret`. */
    #[cold]
    #[inline(never)]
    pub(crate) fn then_end(&mut self, base: usize, mut r: Result<Step, VmErr>, ret: usize) -> Result<usize, (usize, VmErr)> {
        let mut waits = self.conts.split_off(base.min(self.conts.len()));
        while !waits.is_empty() {
            let t = waits.remove(0);
            let rest = self.conts.len();
            let last = waits.is_empty();
            self.conts.append(&mut waits);
            let g = match t.cont { Cont::After(g, _) | Cont::Exit(g, ..) | Cont::Close(g, _) | Cont::Aw(g) => g };
            self.coro_frame = Some((g, if last { t.takes } else { crate::vm::jobs::Takes::Then(rest as u32) }, t.before));
            let out = self.run_cont(t.cont, r);
            self.coro_frame = None;
            if matches!(out, Ok(Outcome::Opened)) {
                // A throw the frame opened for raises where this gives back, so no turn of the loop raises it again.
                (self.entered, self.yielded, self.thrown) = (false, false, false);
                self.gather_ret(ret);
                return match self.resume_raise.take() { Some(e) => Err((self.gen_entry, e)), None => Ok(self.gen_entry) };
            }
            waits = self.conts.split_off(rest);
            r = out.map(|o| match o { Outcome::Yielded(v) => Step::Yield(v), _ => match o { Outcome::Returned(v) => Step::Return(v), _ => Step::Return(Val::none()) } });
            if last { return self.then_give(t.takes, t.before, r, ret); }
        }
        Err((ret - 1, cold_runtime("a wait ended with none left")))
    }

    /* `r`, a step of the generator the first call named, given back at `ret` as that call takes it. */
    fn then_give(&mut self, takes: crate::vm::jobs::Takes, before: Val, r: Result<Step, VmErr>, ret: usize) -> Result<usize, (usize, VmErr)> {
        use crate::vm::jobs::Takes;
        let v = match (takes, r) {
            (Takes::Next(d), Ok(Step::Return(v))) if d.is_undef() => return Err((ret - 1, self.stop_iteration(v))),
            (Takes::Next(d), Ok(Step::Return(_))) => d,
            (Takes::Next(_) | Takes::Close, Ok(Step::Yield(v))) | (Takes::Close, Ok(Step::Return(v))) => v,
            (Takes::Await | Takes::AwaitFor, Ok(Step::Yield(v))) => { self.await_back = 1; v }
            (Takes::Await | Takes::AwaitFor, Ok(Step::Return(v))) => { self.await_back = 2; v }
            (Takes::AwaitFor, Err(e)) if raised_is(&e, "StopAsyncIteration") => { self.pending.exc_val = Some(before).filter(|b| !b.is_undef()); self.await_back = 6; Val::none() }
            (_, Err(e)) => return Err((ret - 1, e)),
            _ => return Err((ret - 1, cold_runtime("a wait gave a call it does not take"))),
        };
        Ok(self.deliver(v, ret, crate::vm::act::NO_DEST))
    }

    /* Steps `sub`, the iterator a suspended body waits inside, undef for a builtin iterator that answers only `next`. */
    fn delegate(&mut self, sub: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        if !sub.is_undef() { return self.gen_step(sub, sent, chunk); }
        match sent {
            Sent::Value(_) => Err(VmErr::Attribute("'iterator' object has no attribute 'send'".into())),
            Sent::Throw(e) => Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))),
            Sent::Close => Ok(Step::Return(Val::none())),
        }
    }

    /* Carries on `g` once what it waits inside took a step, a yield passed out, a return landing where `g` waits, an error raised there. */
    fn after_delegate(&mut self, g: Val, via: Via, step: Result<Step, VmErr>, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let _ = chunk;
        match step {
            Ok(Step::Yield(v)) => Ok(Outcome::Yielded(v)),
            Ok(Step::Return(v)) => { self.land(g, via, Some(v)); self.step_coro(g) }
            // An async iterator that ended ends the `async for` waiting on it.
            Err(e) if via == Via::AsyncFor && raised_is(&e, "StopAsyncIteration") => {
                self.pending.exc_val = None;
                self.land(g, via, None);
                self.step_coro(g)
            }
            Err(e) => self.throw_in(g, e),
        }
    }

    /* Settles the wait of `g` with `v`, what its `yield from` or `await` evaluates to, or the next item of its `async for`, None ending that loop. */
    fn land(&mut self, g: Val, via: Via, v: Option<Val>) {
        let exit = match self.heap.get(g) {
            HeapObj::Coroutine(c) if via == Via::YieldFrom => c.ip.checked_sub(3).and_then(|at| self.body_code(c.body).get(at)).map(|i| i.operand as usize),
            _ => None,
        };
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) else { return };
        match via {
            // The loop exit runs LoadYieldFrom, which takes the value.
            Via::YieldFrom => {
                c.iters.pop();
                if let Some(exit) = exit { c.ip = exit; }
                self.yield_from_value = v.unwrap_or_else(Val::none);
            }
            // The result takes the place of the awaitable and its placeholder, and the body carries on past the Await.
            Via::Await => {
                c.stack.pop();
                if let Some(slot) = c.stack.last_mut() { *slot = v.unwrap_or_else(Val::none); }
                c.ip += 1;
            }
            Via::AsyncFor => if let Some(IterFrame::AsyncFor { aw, out, .. }) = c.iters.last_mut() { (*aw, *out) = match v { Some(v) => (v, 1), None => (Val::undef(), 2) }; },
        }
    }

    /* Resumes `g` once, or opens it as a frame of the loop when `coro_frame` names it, its job taking the step as its caller would. */
    fn step_coro(&mut self, g: Val) -> Result<Outcome, VmErr> {
        // A generator a delegation steps opens as a frame too, its end running the waits of those it delegates for.
        if core::mem::take(&mut self.then_ok) && self.coro_frame.is_some_and(|(c, ..)| c.0 != g.0) {
            if self.gen_job(g, crate::vm::jobs::Takes::Then(self.conts.len() as u32), Vec::new(), true)? { return Ok(Outcome::Opened); }
            return self.unopened(g);
        }
        if let Some((c, takes, before)) = self.coro_frame && c.0 == g.0 {
            self.coro_frame = None;
            if self.gen_job(g, takes, Vec::new(), true)? {
                if let Some(crate::vm::jobs::Job::Gather(j)) = self.jobs.last_mut() { j.before = before; }
                return Ok(Outcome::Opened);
            }
            return self.unopened(g);
        }
        let _ = g;
        Err(self.resumed_apart())
    }

    /* The step of `g` when its frame cannot open, the item a builtin's step took ahead or the depth, memory or budget error its resume would give. */
    #[cold]
    #[inline(never)]
    fn unopened(&mut self, g: Val) -> Result<Outcome, VmErr> {
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get(g) && !c.pending.is_undef() { return Ok(Outcome::Yielded(self.take_pending(g))); }
        if self.heap.over() { return Err(crate::vm::cold_heap()); }
        if self.budget == 0 { return Err(VmErr::Budget); }
        Err(cold_depth())
    }

    /* Raises `err` where `g` waits. */
    fn throw_in(&mut self, g: Val, err: VmErr) -> Result<Outcome, VmErr> {
        self.resume_raise = Some(err);
        let r = self.step_coro(g);
        // A frame opened raises it as it enters.
        if !matches!(r, Ok(Outcome::Opened)) { self.resume_raise = None; }
        r
    }

    /* Closes `g` by raising GeneratorExit where it waits, or `err` when closing what it waits inside failed. */
    fn close_in(&mut self, g: Val, err: Option<VmErr>, kind: CoroKind, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let before = self.pending.exc_val;
        let err = match err {
            Some(e) => e,
            None => { let exit = self.builtin_value("GeneratorExit")?; self.raise_value(exit, chunk) }
        };
        match self.throw_in(g, err) {
            Ok(Outcome::Yielded(_)) => Err(VmErr::Raised(crate::s!("RuntimeError: ", str kind.noun(), " ignored GeneratorExit"))),
            // A body that returns on GeneratorExit hands its value to `close`.
            Ok(done) => Ok(done),
            Err(e) if raised_is(&e, "GeneratorExit") => { self.pending.exc_val = before; Ok(Outcome::Returned(Val::none())) }
            Err(e) => Err(e),
        }
    }

    /* One step of async generator `g` as its `asend` takes it, a plain `yield` returning its value, an `await` inside passing out, an end raising StopAsyncIteration. */
    fn agen_step(&mut self, g: Val, sent: Sent, closing: bool, chunk: &SSAChunk) -> Result<Step, VmErr> {
        let before = self.pending.exc_val;
        let r = self.gen_resume(g, sent, chunk);
        self.agen_after(g, r, closing, before, chunk)
    }

    /* Where the loop goes once the StopAsyncIteration an `async for` awaited ends the job's frame, else the error `agen_step` would raise. */
    #[cold]
    #[inline(never)]
    pub(crate) fn job_raised(&mut self, e: VmErr) -> Result<Result<usize, (usize, VmErr)>, VmErr> {
        let Some(crate::vm::jobs::Job::Gather(j)) = self.jobs.last() else { return Err(e) };
        let (takes, before) = (j.takes, j.before);
        if let crate::vm::jobs::Takes::Then(base) = takes && let Some(f) = self.top() {
            self.finish_gen();
            let e = if raised_is(&e, "StopIteration") { self.stop_out_of(Some(f.generator), e) } else { e };
            return Ok(self.then_end(base as usize, Err(e), f.ret));
        }
        if takes == crate::vm::jobs::Takes::AwaitFor && raised_is(&e, "StopAsyncIteration") && let Some(f) = self.top() {
            self.finish_gen();
            self.pending.exc_val = Some(before).filter(|b| !b.is_undef());
            self.await_back = 6;
            return Ok(Ok(self.deliver(Val::none(), f.ret, crate::vm::act::NO_DEST)));
        }
        Err(self.agen_raised(e, takes))
    }

    /* Where the loop goes on once a job's generator frame gave `r` at `ret`, through `then_end` for a `Then` job's. */
    #[inline(never)]
    pub(crate) fn job_out(&mut self, r: Result<Val, VmErr>, ret: usize) -> Result<usize, (usize, VmErr)> {
        match (self.then_out.take(), r) {
            (Some((base, step)), Ok(_)) => self.then_end(base as usize, step, ret),
            (_, Ok(r)) => Ok(self.deliver(r, ret, crate::vm::act::NO_DEST)),
            (_, Err(e)) => Err((ret - 1, e)),
        }
    }

    /* The RuntimeError `agen_step` makes of a StopIteration or StopAsyncIteration out of an async generator a job of `takes` steps. */
    fn agen_raised(&mut self, e: VmErr, takes: crate::vm::jobs::Takes) -> VmErr {
        if takes != crate::vm::jobs::Takes::Agen || !(raised_is(&e, "StopIteration") || raised_is(&e, "StopAsyncIteration")) { return e; }
        // The RuntimeError takes the place of the exception the body raised.
        self.pending.exc_val = None;
        match self.agen_after(Val::undef(), Err(e), false, None, self.chunk) { Err(e) => e, Ok(_) => cold_runtime("an async generator's error went") }
    }

    /* What `agen_step` makes of `r`, the outcome of resuming `g`, with `before` the exception pending as it began. */
    fn agen_after(&mut self, g: Val, r: Result<Outcome, VmErr>, closing: bool, before: Option<Val>, chunk: &SSAChunk) -> Result<Step, VmErr> {
        match r {
            Ok(Outcome::Yielded(v)) if self.delegate_of(g).is_some() => Ok(Step::Yield(v)),
            Ok(Outcome::Yielded(_)) if closing => Err(VmErr::Raised("RuntimeError: async generator ignored GeneratorExit".into())),
            Ok(Outcome::Yielded(v)) => Ok(Step::Return(v)),
            Ok(Outcome::Returned(_)) if closing => Ok(Step::Return(Val::none())),
            Ok(Outcome::Returned(_)) => Err(self.raise_named("StopAsyncIteration", chunk)),
            Ok(Outcome::Opened) => Err(cold_runtime("a generator opened past its call")),
            Err(e) if closing && (raised_is(&e, "GeneratorExit") || raised_is(&e, "StopAsyncIteration")) => { self.pending.exc_val = before; Ok(Step::Return(Val::none())) }
            Err(e) if raised_is(&e, "StopIteration") => Err(VmErr::Raised("RuntimeError: async generator raised StopIteration".into())),
            Err(e) if raised_is(&e, "StopAsyncIteration") => Err(VmErr::Raised("RuntimeError: async generator raised StopAsyncIteration".into())),
            Err(e) => Err(e),
        }
    }

    /* The awaitable state `it` holds, for `asend` and its kin. */
    fn awaitable_frame(&self, it: Val) -> Option<(Val, AwaitMode, Val, Val, u8)> {
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return None };
        match *frame.borrow() { IterFrame::Awaitable { target, mode, arg, default, stage } => Some((target, mode, arg, default, stage)), _ => None }
    }

    /* Moves the awaitable `it` to `stage`. */
    fn set_stage(&mut self, it: Val, to: u8) {
        if let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) && let IterFrame::Awaitable { stage, .. } = &mut *frame.borrow_mut() { *stage = to; }
    }

    /* One step of an `asend`, `athrow`, `aclose`, `anext` or `__await__` awaitable, which runs once and is then spent. */
    fn awaitable_step(&mut self, it: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        self.aw_on(it, sent, false, chunk)?.ok_or_else(|| cold_runtime("an awaitable opened past its call"))
    }

    /* `next` of the awaitable `it` with `default`, true once its target opened as a frame of the loop. */
    pub(crate) fn awaitable_next(&mut self, it: Val, default: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        self.coro_frame = Some((it, crate::vm::jobs::Takes::Next(default), self.pending.exc_val.unwrap_or(Val::undef())));
        let r = self.with_roots([it, default], |vm| vm.aw_on(it, Sent::Value(Val::none()), true, chunk));
        self.coro_frame = None;
        match r? {
            None => Ok(true),
            Some(Step::Yield(v)) => { self.push(v); Ok(true) }
            Some(Step::Return(_)) if !default.is_undef() => { self.push(default); Ok(true) }
            Some(Step::Return(v)) => Err(self.stop_iteration(v)),
        }
    }

    /* `awaitable_step`, None once its target opened as a frame of the loop, which `frame` and the call's `coro_frame` allow. */
    fn aw_on(&mut self, it: Val, sent: Sent, frame: bool, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> {
        let Some((target, mode, arg, default, stage)) = self.awaitable_frame(it) else { return Err(cold_type("not an awaitable")) };
        let before = self.pending.exc_val;
        let sent = match mode {
            AwaitMode::Wrap => sent,
            _ if stage == 2 => return Err(VmErr::Raised("RuntimeError: cannot reuse already awaited __anext__()/asend()".into())),
            // Closing the awaitable spends it and leaves its generator as it is.
            _ if matches!(sent, Sent::Close) => { self.set_stage(it, 2); return Ok(Some(Step::Return(Val::none()))); }
            _ if stage == 0 => {
                if let Sent::Value(v) = sent && !v.is_none() { return Err(VmErr::TypeMsg("can't send non-None value to a just-started coroutine".into())); }
                self.set_stage(it, 1);
                match mode {
                    AwaitMode::Send => Sent::Value(arg),
                    AwaitMode::Throw => Sent::Throw(arg),
                    _ => Sent::Throw(self.builtin_value("GeneratorExit")?),
                }
            }
            _ => sent,
        };
        let kind = self.coro_kind(target);
        let step = match self.coro_frame {
            // Its target opens as a frame of the loop, whose end gives the awaitable's step through its wait.
            Some((_, takes, b)) if frame && kind.is_some() => {
                self.then_ok = true;
                let r = self.with_roots([it], |vm| vm.gen_resume(target, sent, chunk));
                self.then_ok = false;
                if matches!(r, Ok(Outcome::Opened)) { self.conts.push(Then { cont: Cont::Aw(it), takes, before: b }); return Ok(None); }
                self.aw_inner(target, mode, r, before, chunk)
            }
            _ if mode == AwaitMode::Wrap => self.gen_step(target, sent, chunk),
            _ => self.with_roots([it], |vm| vm.agen_step(target, sent, mode == AwaitMode::Close, chunk)),
        };
        self.aw_after(it, step, mode, default, before).map(Some)
    }

    /* The step of an awaitable's `target` of `mode` that resuming it gave as `r`. */
    fn aw_inner(&mut self, target: Val, mode: AwaitMode, r: Result<Outcome, VmErr>, before: Option<Val>, chunk: &SSAChunk) -> Result<Step, VmErr> {
        if mode != AwaitMode::Wrap || self.coro_kind(target) == Some(CoroKind::AsyncGen) { return self.agen_after(target, r, mode == AwaitMode::Close, before, chunk); }
        r.map(|o| match o { Outcome::Yielded(v) => Step::Yield(v), Outcome::Returned(v) => Step::Return(v), Outcome::Opened => Step::Return(Val::none()) })
    }

    /* What the awaitable `it` makes of its target's `step`, spent once that gave a value or failed. */
    fn aw_after(&mut self, it: Val, step: Result<Step, VmErr>, mode: AwaitMode, default: Val, before: Option<Val>) -> Result<Step, VmErr> {
        if !matches!(step, Ok(Step::Yield(_))) && mode != AwaitMode::Wrap { self.set_stage(it, 2); }
        match step {
            // `anext` with a default gives it once the iterator ended.
            Err(e) if !default.is_undef() && raised_is(&e, "StopAsyncIteration") => { self.pending.exc_val = before; Ok(Step::Return(default)) }
            step => step,
        }
    }

    /* A step of any other iterator, `send`, `throw` and `close` called when it has them, StopIteration taken as its return. */
    fn iter_send(&mut self, it: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        let before = self.pending.exc_val;
        let r = match sent {
            Sent::Value(v) if v.is_none() => self.iter_next_proto(it, chunk).and_then(|r| r.ok_or_else(|| VmErr::Raised("StopIteration".into()))),
            Sent::Value(v) => self.try_call_dunder(it, "send", &[v], chunk)?.ok_or_else(|| VmErr::Attribute(crate::s!("'", str self.type_name(it), "' object has no attribute 'send'"))),
            Sent::Throw(e) => match self.try_call_dunder(it, "throw", &[e], chunk)? {
                Some(v) => Ok(v),
                None => Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))),
            },
            Sent::Close => { self.try_call_dunder(it, "close", &[], chunk)?; return Ok(Step::Return(Val::none())); }
        };
        match r {
            Ok(v) => Ok(Step::Yield(v)),
            Err(e) if raised_is(&e, "StopIteration") => Ok(Step::Return(self.swallow_stop(before))),
            Err(e) => Err(e),
        }
    }

    /* The method of user iterator `it` taking `sent`, as the plain function, its class and the argument past `it`, undef for none. */
    fn sent_hook(&self, it: Val, sent: Sent) -> Option<(Val, Val, Val)> {
        let (name, arg) = match sent { Sent::Value(v) if v.is_none() => ("__next__", Val::undef()), Sent::Value(v) => ("send", v), Sent::Throw(e) => ("throw", e), Sent::Close => ("close", Val::undef()) };
        let (f, defining) = self.instance_class(it).and_then(|c| self.lookup_class_member(c, name))?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))).then_some((f, defining, arg))
    }

    /* The name of the exception class `e` is or is an instance of. */
    fn exc_name_of(&self, e: Val) -> Option<&str> {
        match self.heap.try_get(e)? {
            HeapObj::Type(n) | HeapObj::ExcInstance(n, ..) | HeapObj::Class(n, ..) => Some(n),
            &HeapObj::Instance(cls, _) => match self.heap.try_get(cls) { Some(HeapObj::Class(n, ..)) => Some(n), _ => None },
            _ => None,
        }
    }

    /* The error raising builtin exception `name`. */
    fn raise_named(&mut self, name: &'static str, chunk: &SSAChunk) -> VmErr {
        match self.builtin_value(name) { Ok(t) => self.raise_value(t, chunk), Err(e) => e }
    }

    /* StopIteration carrying `v`, the value a generator or coroutine returned. */
    pub(crate) fn stop_iteration(&mut self, v: Val) -> VmErr {
        if v.is_none() { return VmErr::Raised("StopIteration".into()); }
        match self.heap.alloc(HeapObj::ExcInstance("StopIteration".into(), vec![v], Val::undef())) {
            Ok(exc) => { self.pending.exc_val = Some(exc); VmErr::Raised(crate::s!("StopIteration: ", str &self.display(v))) }
            Err(e) => e,
        }
    }

    /* What `await x` steps, a coroutine as it is or the iterator `__await__` gives. */
    pub(crate) fn awaitable_of(&mut self, x: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        match self.coro_kind(x) {
            Some(CoroKind::Coroutine) => {
                if matches!(self.heap.get(x), HeapObj::Coroutine(c) if c.ip == FINISHED) { return Err(VmErr::Raised("RuntimeError: cannot reuse already awaited coroutine".into())); }
                return Ok(x);
            }
            Some(_) => {}
            None if self.awaitable_frame(x).is_some() => return Ok(x),
            None => if self.instance_class(x).is_some() && let Some(it) = self.try_call_dunder(x, "__await__", &[], chunk)? {
                let cls = self.type_name(x);
                if self.coro_kind(it) == Some(CoroKind::Coroutine) { return Err(VmErr::TypeMsg(crate::s!(str cls, ".__await__() must return an iterator, not coroutine"))); }
                if !self.is_iterator(it) { return Err(VmErr::TypeMsg(crate::s!(str cls, ".__await__() must return an iterator, not ", str self.type_name(it)))); }
                return Ok(it);
            },
        }
        Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(x), "' object can't be awaited")))
    }

    /* Whether `v` steps by `next`, a generator, a builtin iterator or an instance with `__next__`. */
    pub(crate) fn is_iterator(&self, v: Val) -> bool {
        match self.heap.try_get(v) {
            Some(HeapObj::Iter(..)) => true,
            Some(HeapObj::Coroutine(..)) => self.coro_kind(v) == Some(CoroKind::Generator),
            _ => self.instance_class(v).is_some_and(|c| self.lookup_class_member(c, "__next__").is_some()),
        }
    }

    /* `aiter(x)` and the iterable of `async for`, an async generator itself or what `__aiter__` gives. */
    pub(crate) fn aiter_of(&mut self, x: Val, for_loop: bool, chunk: &SSAChunk) -> Result<Val, VmErr> {
        if self.coro_kind(x) == Some(CoroKind::AsyncGen) { return Ok(x); }
        if self.instance_class(x).is_some() && let Some(it) = self.try_call_dunder(x, "__aiter__", &[], chunk)? { return Ok(it); }
        Err(VmErr::TypeMsg(if for_loop {
            crate::s!("'async for' requires an object with __aiter__ method, got ", str self.type_name(x))
        } else {
            crate::s!("'", str self.type_name(x), "' object is not an async iterable")
        }))
    }

    /* The awaitable the next `async for` step drives, an async generator itself or what `__anext__` gives made awaitable. */
    fn anext_of(&mut self, it: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        if self.coro_kind(it) == Some(CoroKind::AsyncGen) { return Ok(it); }
        match self.try_call_dunder(it, "__anext__", &[], chunk)? {
            Some(aw) => self.with_roots([aw], |vm| vm.awaitable_of(aw, chunk)),
            None => Err(VmErr::TypeMsg(crate::s!("'async for' received an object from __aiter__ that does not implement __anext__: ", str self.type_name(it)))),
        }
    }

    /* One step of the `async for` on top, its next item, None once it ended, undef while the awaitable it waits on yielded up. */
    pub(crate) fn async_for_step(&mut self, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let Some(IterFrame::AsyncFor { it, aw, out }) = self.iter_stack.last_mut() else { return Ok(None) };
        let (it, settled) = (*it, core::mem::take(out));
        let aw = if settled == 0 { *aw } else { core::mem::replace(aw, Val::undef()) };
        // A throw the awaitable answered left the step it settled, its placeholder still on the stack.
        if settled != 0 {
            self.pop()?;
            if settled == 2 { self.iter_stack.pop(); return Ok(None); }
            return Ok(Some(aw));
        }
        let (aw, sent) = if aw.is_undef() { (self.anext_of(it, chunk)?, Val::none()) } else { (aw, self.pop()?) };
        if let Some(IterFrame::AsyncFor { aw: slot, .. }) = self.iter_stack.last_mut() { *slot = aw; }
        let before = self.pending.exc_val;
        match self.aw_step(aw, sent, true, chunk) {
            // A frame it opened comes back to this step, which runs again with what it gave.
            Ok(None) => Ok(Some(Val::undef())),
            // The yield passes up through the frame, which runs this step again with what it is sent.
            Ok(Some(Step::Yield(v))) => { self.push(v); self.yielded = true; Ok(Some(Val::undef())) }
            Ok(Some(Step::Return(v))) => {
                if let Some(IterFrame::AsyncFor { aw: slot, .. }) = self.iter_stack.last_mut() { *slot = Val::undef(); }
                Ok(Some(v))
            }
            Err(e) if raised_is(&e, "StopAsyncIteration") => { self.pending.exc_val = before; self.iter_stack.pop(); Ok(None) }
            Err(e) => Err(e),
        }
    }

    /* The awaitable over async generator `g` its `asend`, `athrow`, `aclose` or `anext` gives. */
    fn agen_awaitable(&mut self, g: Val, mode: AwaitMode, arg: Val, default: Val, name: &'static str) -> Result<(), VmErr> {
        self.push_iterator(IterFrame::Awaitable { target: g, mode, arg, default, stage: 0 }, name)
    }

    /* The exception `throw(value)` or `throw(type, value)` raises, checked to derive from BaseException. */
    fn thrown(&mut self, args: &[Val], chunk: &SSAChunk) -> Result<Val, VmErr> {
        let exc = if args.len() > 1 && matches!(self.heap.try_get(args[0]), Some(HeapObj::Type(_) | HeapObj::Class(..))) {
            self.call_with(args[0], None, &args[1..2], &[], chunk)?;
            self.pop()?
        } else { args[0] };
        let derives = match self.heap.try_get(exc) {
            Some(&HeapObj::Instance(c, _)) => self.exc_base(c).is_some(),
            Some(HeapObj::ExcInstance(..)) => true,
            Some(HeapObj::Type(_) | HeapObj::Class(..)) => self.exc_base(exc).is_some(),
            _ => false,
        };
        if !derives { return Err(VmErr::TypeMsg(crate::s!("exceptions must be classes or instances deriving from BaseException, not ", str self.type_name(exc)))); }
        Ok(exc)
    }

    /* The methods of a generator, a coroutine, an async generator and the awaitables they give. */
    pub(crate) fn coro_method(&mut self, g: Val, name: &str, args: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        use crate::vm::jobs::Takes;
        let (ok, before) = (core::mem::take(&mut self.job_ok), self.pending.exc_val.unwrap_or(Val::undef()));
        let sent = match name {
            "__next__" if self.coro_kind(g) == Some(CoroKind::Generator) => {
                if ok && self.gather_job(g, Takes::Next(Val::undef()), Vec::new())? { return Ok(()); }
                return self.resume_generator(g, None);
            }
            "__iter__" | "__aiter__" => { self.push(g); return Ok(()); }
            "__await__" if self.coro_kind(g).is_none() => { self.push(g); return Ok(()); }
            "__await__" => return self.agen_awaitable(g, AwaitMode::Wrap, Val::none(), Val::undef(), "coroutine_wrapper"),
            "asend" => return self.agen_awaitable(g, AwaitMode::Send, args[0], Val::undef(), "async_generator_asend"),
            "__anext__" => return self.agen_awaitable(g, AwaitMode::Send, Val::none(), Val::undef(), "async_generator_asend"),
            "athrow" => { let e = self.thrown(args, chunk)?; return self.agen_awaitable(g, AwaitMode::Throw, e, Val::undef(), "async_generator_athrow"); }
            "aclose" => return self.agen_awaitable(g, AwaitMode::Close, Val::none(), Val::undef(), "async_generator_athrow"),
            "send" => Sent::Value(args[0]),
            "throw" => Sent::Throw(self.thrown(args, chunk)?),
            "close" => Sent::Close,
            _ => Sent::Value(Val::none()),
        };
        let close = matches!(sent, Sent::Close);
        // A generator called where a frame may open resumes as one, its job taking the step as this call does.
        if ok && matches!(self.coro_kind(g), Some(CoroKind::Generator | CoroKind::Coroutine)) {
            self.coro_frame = Some((g, if close { Takes::Close } else { Takes::Next(Val::undef()) }, before));
            let r = self.with_roots([g], |vm| vm.gen_resume(g, sent, chunk));
            self.coro_frame = None;
            return match r? {
                Outcome::Opened => Ok(()),
                Outcome::Yielded(v) => { self.push(v); Ok(()) }
                Outcome::Returned(v) if close => { self.push(v); Ok(()) }
                Outcome::Returned(v) => Err(self.stop_iteration(v)),
            };
        }
        // An awaitable called where a frame may open steps its target as one, its job taking the step as this call does.
        if ok && self.awaitable_frame(g).is_some() {
            self.coro_frame = Some((g, if close { Takes::Close } else { Takes::Next(Val::undef()) }, before));
            let r = self.with_roots([g], |vm| vm.aw_on(g, sent, true, chunk));
            self.coro_frame = None;
            return match r? {
                None => Ok(()),
                Some(Step::Yield(v)) => { self.push(v); Ok(()) }
                Some(Step::Return(v)) if close => { self.push(v); Ok(()) }
                Some(Step::Return(v)) => Err(self.stop_iteration(v)),
            };
        }
        match self.with_roots([g], |vm| vm.gen_step(g, sent, chunk))? {
            Step::Yield(v) => { self.push(v); Ok(()) }
            Step::Return(v) if close => { self.push(v); Ok(()) }
            Step::Return(v) => Err(self.stop_iteration(v)),
        }
    }

    /* Marks generator `g` finished, so it never runs again. */
    fn finish_generator(&mut self, g: Val) {
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) { c.ip = FINISHED; }
    }

    /* `aiter(x)`. */
    pub fn call_aiter(&mut self, chunk: &SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        let it = self.with_roots([o], |vm| vm.aiter_of(o, false, chunk))?;
        self.push(it);
        Ok(())
    }

    /* `anext(it[, default])`, the awaitable of the next item, the default once the iterator ended. */
    pub fn call_anext(&mut self, argc: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let default = args.get(1).copied().unwrap_or_else(Val::undef);
        let it = args[0];
        if self.coro_kind(it) == Some(CoroKind::AsyncGen) {
            let name = if default.is_undef() { "async_generator_asend" } else { "anext_awaitable" };
            return self.agen_awaitable(it, AwaitMode::Send, Val::none(), default, name);
        }
        let Some(aw) = self.with_roots(args.iter().copied(), |vm| vm.try_call_dunder(it, "__anext__", &[], chunk))? else {
            return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(it), "' object is not an async iterator")));
        };
        if default.is_undef() { self.push(aw); return Ok(()); }
        let target = self.with_roots([aw, default], |vm| vm.awaitable_of(aw, chunk))?;
        self.agen_awaitable(target, AwaitMode::Wrap, Val::none(), default, "anext_awaitable")
    }
}

/* Whether `e` raises builtin exception `name`. */
pub(crate) fn raised_is(e: &VmErr, name: &str) -> bool {
    matches!(e, VmErr::Raised(m) if m.strip_prefix(name).is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('\n')))
}

/* Which kind of suspendable body a coroutine object runs. */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CoroKind { Generator, Coroutine, AsyncGen }

impl CoroKind {
    /* How Python names the kind in its errors. */
    fn noun(self) -> &'static str {
        match self { Self::Generator => "generator", Self::Coroutine => "coroutine", Self::AsyncGen => "async generator" }
    }
}

/* What a body waits inside, through a `yield from`, an `await` or an `async for`. */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Via { YieldFrom, Await, AsyncFor }

/* What a driver hands a suspended body, a value to send, an exception to throw, or a close. */
#[derive(Clone, Copy, Debug)]
pub(crate) enum Sent { Value(Val), Throw(Val), Close }

/* What a delegating body does once the frame it waits on ends, resume itself after it, raise GeneratorExit in itself, or close. */
#[derive(Clone, Copy)]
pub(crate) enum Cont { After(Val, Via), Exit(Val, Via, Val), Close(Val, CoroKind), Aw(Val) }

/* A wait a `Then` job's frame ends, with the takes and pending exception of the call that began the delegation. */
#[derive(Clone, Copy)]
pub(crate) struct Then { pub(crate) cont: Cont, takes: crate::vm::jobs::Takes, pub(crate) before: Val }

/* What a step of an awaited or delegated iterator gives, a value it yields up or the value it ended with. */
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step { Yield(Val), Return(Val) }

/* What resuming a body gave, a yield or a return. */
#[derive(Clone, Copy, Debug)]
enum Outcome { Yielded(Val), Returned(Val), Opened }

impl VM<'_> {
    /* The attributes a generator, a coroutine or an async generator reads about its own state, its name and code. */
    pub(crate) fn coro_attr(&self, g: Val, name: &str) -> Option<crate::vm::opcodes::attr_lookup::AttrLookup> {
        use crate::vm::opcodes::attr_lookup::AttrLookup;
        let kind = self.coro_kind(g)?;
        let HeapObj::Coroutine(c) = self.heap.get(g) else { return None };
        let BodyRef::Fn(fi) = c.body else { return None };
        let prefix = match kind { CoroKind::Generator => "gi_", CoroKind::Coroutine => "cr_", CoroKind::AsyncGen => "ag_" };
        let running = self.executing_coros.contains(&g.0);
        Some(match name {
            "__name__" => AttrLookup::Name(self.function_names.get(fi)?.clone()),
            "__qualname__" => AttrLookup::Name(self.function_qualnames.get(fi)?.clone()),
            _ => match name.strip_prefix(prefix)? {
                "running" => AttrLookup::ClassMember(Val::bool(running)),
                "suspended" => AttrLookup::ClassMember(Val::bool(!running && c.ip != 0 && c.ip != FINISHED)),
                "code" => AttrLookup::Code(fi),
                "yieldfrom" | "await" => AttrLookup::ClassMember(self.delegate_of(g).map(|(v, _)| v).filter(|v| !v.is_undef()).unwrap_or_else(Val::none)),
                "origin" if kind == CoroKind::Coroutine => AttrLookup::ClassMember(Val::none()),
                _ => return None,
            },
        })
    }
}

/* A user iterator's method a delegation calls as a frame of a job, giving the waits from `base` a yield, or a return on StopIteration and `close`. */
struct SentCall { it: Val, f: Val, defining: Val, arg: Val, close: bool, base: u32, before: Option<Val> }

impl crate::vm::jobs::Calls for SentCall {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<crate::vm::jobs::Call, VmErr> {
        let Some(r) = got else { return Ok(crate::vm::jobs::Call::Frame(self.f, self.defining, [self.it, self.arg, Val::undef()])) };
        let step = match r {
            Ok(_) if self.close => Ok(Step::Return(Val::none())),
            Ok(v) => Ok(Step::Yield(v)),
            Err(e) if !self.close && raised_is(&e, "StopIteration") => Ok(Step::Return(vm.swallow_stop(self.before))),
            Err(e) => Err(e),
        };
        Ok(crate::vm::jobs::Call::Then(self.base, step))
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { f(self.it); f(self.f); f(self.defining); f(self.arg); if let Some(b) = self.before { f(b) } }
}
