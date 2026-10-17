use alloc::{vec, vec::Vec};

use crate::parser::{OpCode, SSAChunk};
use super::super::VM;
use super::super::types::*;

/* An ip no chunk reaches, the same on 32 and 64 bits so a snapshot keeps it. */
pub(crate) const FINISHED: usize = u32::MAX as usize;

impl<'a> VM<'a> {

    // Resume coroutine, persist state on yield, restore caller on return. Suspended sync sub-frames run innermost-first, each pushing its result onto the next frame's stack at the Call site. The coro's `exception_frames` are restored before its body runs and saved back on yield, so `try`/`except` survives suspensions.
    pub fn resume_coroutine(&mut self, callee: Val) -> Result<Val, VmErr> {
        // Scheduler-driven resumes have nothing native above.
        let resume_safe = core::mem::take(&mut self.pending_exec_safe);
        let for_steps = core::mem::take(&mut self.for_steps);
        self.wake_gen(callee, for_steps);
        let (outer_ip, outer_body, syncs, held) = match self.heap.get(callee) {
            HeapObj::Coroutine(c) => (c.ip, c.body, c.syncs.len(), c.syncs.len() + c.stack.len() + c.slots.len() + c.iters.len()),
            _ => return Err(cold_type("not a coroutine")),
        };
        if outer_ip == FINISHED { self.yielded = false; return Ok(Val::none()); }

        // Nested resumes from mutual awaits recurse in Rust, and each resume copies every suspended frame, so both stay bounded.
        if syncs >= crate::vm::frame::SUSPENDED_DEPTH || self.depth >= self.max_calls { return Err(cold_depth()); }
        // Re-entrant resume (`yield from g` inside g, `next(g)` from g's own body).
        if self.executing_coros.contains(&callee.0) { return Err(self.running_error(callee)); }
        // Charge the whole saved state (stack/slots/iters/frames), not just frame count.
        self.charge_steps(held)?;
        // With no suspended helper the state moves out, a yield puts it back and any error finishes the coroutine. Helper frames wait outside every root, so the coroutine keeps them until it saves.
        let (mut outer_slots, mut outer_stack, mut outer_iters, mut sync_frames, mut outer_exc) = match self.heap.get_mut(callee) {
            HeapObj::Coroutine(c) if syncs == 0 => (core::mem::take(&mut c.slots), core::mem::take(&mut c.stack), core::mem::take(&mut c.iters), Vec::new(), core::mem::take(&mut c.excs)),
            HeapObj::Coroutine(c) => (c.slots.clone(), c.stack.clone(), c.iters.clone(), c.syncs.clone(), c.excs.clone()),
            _ => return Err(cold_type("not a coroutine")),
        };

        self.executing_coros.push(callee.0);

        // Stored depths are relative to the saved stacks of the coroutine, restoring lifts them to absolute positions.
        let (saved_stack_len, saved_iter_len, saved_exc_len) = self.restore_into(&mut outer_stack, &mut outer_iters, &mut outer_exc);
        let saved_yielded = self.yielded;
        let saved_resume_ip = self.resume_ip; // don't leak into next exec()
        self.yielded = false;
        self.depth += 1;

        // Walk frames inside-out, then the outer. `outer_ran` tracks whether `outer_ip` should be overwritten by `resume_ip` on save, a re-yield inside a sync frame leaves the outer pristine.
        let mut outer_ran = false;
        let mut pending_ret: Option<Val> = None;
        let result: Result<Val, VmErr> = 'drive: loop {
            if let Some(frame) = sync_frames.pop() {
                let SyncFrame { ip, fi, func, mut slots, mut stack_delta, mut iter_delta, mut exception_delta, instance } = frame;
                let (frame_stack_base, frame_iter_base, frame_exc_base) = self.restore_into(&mut stack_delta, &mut iter_delta, &mut exception_delta);
                // Inner result lands on this frame's stack.
                if let Some(v) = pending_ret.take() { self.push(v); }
                self.pending_exec_exc_base = Some(frame_exc_base);
                self.pending_exec_safe = resume_safe;
                let (_, body, _, _) = self.functions[fi];
                let ran = self.exec_from(body, &mut slots, ip);
                match ran {
                    Err(e @ VmErr::HostYield(_)) => break 'drive Err(e),
                    // An escaping error re-raises at the caller's call site, so its handlers and the traceback note follow.
                    Err(e) => {
                        self.stack.truncate(frame_stack_base);
                        self.iter_stack.truncate(frame_iter_base);
                        self.exception_stack.truncate(frame_exc_base);
                        let (caller, resume_at): (&'a SSAChunk, usize) = match sync_frames.last() {
                            Some(f) => (&self.functions[f.fi].1, f.ip),
                            None => match outer_body {
                                BodyRef::Fn(ofi) => (&self.functions[ofi].1, outer_ip),
                                BodyRef::Module => (self.chunk, outer_ip),
                            },
                        };
                        self.note_frame(fi, Some(resume_at.saturating_sub(1) as u32), caller);
                        self.resume_raise = Some(e);
                    }
                    Ok(val) if self.yielded => {
                        self.save_frames(frame_stack_base, frame_iter_base, frame_exc_base, &mut stack_delta, &mut iter_delta, &mut exception_delta);
                        sync_frames.push(SyncFrame { ip: self.resume_ip, fi, func, slots, stack_delta, iter_delta, exception_delta, instance });
                        // Reverse so pop re-enters innermost first.
                        let newer = core::mem::take(&mut self.pending_sync_frames);
                        sync_frames.extend(newer.into_iter().rev());
                        break 'drive Ok(val);
                    }
                    // A finished helper drops the loops a return left open, as run_call does.
                    Ok(val) => {
                        self.iter_stack.truncate(frame_iter_base);
                        pending_ret = Some(if instance.is_undef() { val } else { instance });
                    }
                }
            } else {
                let body: &SSAChunk = match outer_body {
                    BodyRef::Fn(fi) => &self.functions[fi].1,
                    BodyRef::Module => self.chunk,
                };
                outer_ran = true;
                self.pending_exec_exc_base = Some(saved_exc_len);
                self.pending_exec_safe = resume_safe;
                if let Some(v) = pending_ret.take() { self.push(v); }
                match self.exec_from(body, &mut outer_slots, outer_ip) {
                    Err(e) => break 'drive Err(e),
                    Ok(val) => {
                        if self.yielded {
                            let newer = core::mem::take(&mut self.pending_sync_frames);
                            sync_frames.extend(newer.into_iter().rev());
                        }
                        break 'drive Ok(val);
                    }
                }
            }
        };

        self.depth -= 1;
        self.executing_coros.retain(|&id| id != callee.0);
        // A generator waits with the `for` that steps it, anything else that steps a generator or a coroutine has no way to wait.
        let parked = result.is_ok() && self.yielded && self.waits() && matches!(outer_body, BodyRef::Fn(fi) if self.functions[fi].1.is_generator || self.is_async_fn(fi));
        let result = match result {
            Ok(_) if parked && !for_steps => {
                self.yielded = false;
                Err(self.suspended_in_generator())
            }
            r => r,
        };
        // A body that returned or raised is finished, so a later resume must not run its tail again.
        let finished = match &result { Ok(_) => !self.yielded, Err(e) => !matches!(e, VmErr::HostYield(_)) };
        if finished && let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(callee) { c.ip = FINISHED; }
        let result = match result {
            Ok(v) => v,
            Err(e) => {
                self.stack.truncate(saved_stack_len.min(self.stack.len()));
                self.iter_stack.truncate(saved_iter_len.min(self.iter_stack.len()));
                self.exception_stack.truncate(saved_exc_len.min(self.exception_stack.len()));
                self.resume_raise = None;
                self.resume_ip = saved_resume_ip;
                return Err(e);
            }
        };

        if self.yielded {
            let resume_ip = if outer_ran { self.resume_ip } else { outer_ip };
            // Handler depths become relative, clamped when the coroutine left a shorter stack.
            self.save_frames(saved_stack_len, saved_iter_len, saved_exc_len, &mut outer_stack, &mut outer_iters, &mut outer_exc);
            // An inline-awaited coro isn't a scheduler root, so its body's GC may have freed it, if so skip the save (a freed coro is unreachable and won't resume).
            if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(callee) {
                c.ip = resume_ip;
                c.slots = outer_slots;
                c.stack = outer_stack;
                c.iters = outer_iters;
                c.syncs = sync_frames;
                c.excs = outer_exc;
                // The placeholder of a wait is marked, so the next step of the `for` hands over what woke it.
                if parked && !self.pending.preempt_request && let Some(top) = c.wait_slot() { *top = Val::undef(); }
            }
            self.resume_ip = saved_resume_ip; // restore the caller's scratch
            Ok(result)
        } else {
            self.stack.truncate(saved_stack_len);
            self.iter_stack.truncate(saved_iter_len);
            self.exception_stack.truncate(saved_exc_len);
            self.yielded = saved_yielded;
            self.resume_ip = saved_resume_ip; // restore the caller's scratch
            Ok(result)
        }
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
    pub(crate) fn suspended_in_generator(&mut self) -> VmErr {
        let p = &mut self.pending;
        (p.event_wait_request, p.host_call_request, p.preempt_request) = (false, false, false);
        self.pending_sync_frames.clear();
        VmErr::Raised(alloc::string::String::from("RuntimeError: receive() and host calls cannot wait inside a generator or a coroutine"))
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
            let result = self.resume_coroutine(coro);
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

    /* One step of what an `await`, a `yield from` or a driver steps, `sent` handed in. */
    pub(crate) fn gen_step(&mut self, it: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        match self.coro_kind(it) {
            // An async generator steps the way `__anext__` does, and nothing closes it but `aclose`.
            Some(CoroKind::AsyncGen) if matches!(sent, Sent::Close) => Ok(Step::Return(Val::none())),
            Some(CoroKind::AsyncGen) => self.agen_step(it, sent, false, chunk),
            Some(_) => Ok(match self.gen_resume(it, sent, chunk)? { Outcome::Yielded(v) => Step::Yield(v), Outcome::Returned(v) => Step::Return(v) }),
            None if self.awaitable_frame(it).is_some() => self.awaitable_step(it, sent, chunk),
            None => self.iter_send(it, sent, chunk),
        }
    }

    /* Resumes generator or coroutine `g` with `sent`, reaching what it waits inside first. */
    fn gen_resume(&mut self, g: Val, sent: Sent, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let kind = self.coro_kind(g).ok_or_else(|| cold_type("not a generator"))?;
        if self.executing_coros.contains(&g.0) { return Err(self.running_error(g)); }
        let (ip, body) = match self.heap.get(g) { HeapObj::Coroutine(c) => (c.ip, c.body), _ => return Err(cold_type("not a generator")) };
        if ip == FINISHED {
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
                Sent::Value(v) => Some(self.delegate(sub, Sent::Value(v), chunk)),
                // GeneratorExit closes what it waits inside, then raises in `g`.
                Sent::Throw(e) if self.exc_name_of(e) == Some("GeneratorExit") => self.delegate(sub, Sent::Close, chunk).err().map(Err),
                Sent::Throw(e) => Some(self.delegate(sub, Sent::Throw(e), chunk)),
                // What closing the inner one raises is raised in `g` in place of GeneratorExit.
                Sent::Close => match self.delegate(sub, Sent::Close, chunk) {
                    Err(e) => return self.close_in(g, Some(e), kind, chunk),
                    Ok(_) => None,
                },
            };
            if let Some(r) = sub_step { return self.after_delegate(g, via, r, chunk); }
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

    /* Resumes `g` once, what it yielded or returned. */
    fn step_coro(&mut self, g: Val) -> Result<Outcome, VmErr> {
        let result = self.with_roots([g], |vm| vm.resume_coroutine(g))?;
        Ok(if core::mem::take(&mut self.yielded) { Outcome::Yielded(result) } else { Outcome::Returned(result) })
    }

    /* Raises `err` where `g` waits. */
    fn throw_in(&mut self, g: Val, err: VmErr) -> Result<Outcome, VmErr> {
        self.resume_raise = Some(err);
        let r = self.step_coro(g);
        self.resume_raise = None;
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
        match self.gen_resume(g, sent, chunk) {
            Ok(Outcome::Yielded(v)) if self.delegate_of(g).is_some() => Ok(Step::Yield(v)),
            Ok(Outcome::Yielded(_)) if closing => Err(VmErr::Raised("RuntimeError: async generator ignored GeneratorExit".into())),
            Ok(Outcome::Yielded(v)) => Ok(Step::Return(v)),
            Ok(Outcome::Returned(_)) if closing => Ok(Step::Return(Val::none())),
            Ok(Outcome::Returned(_)) => Err(self.raise_named("StopAsyncIteration", chunk)),
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
        let Some((target, mode, arg, default, stage)) = self.awaitable_frame(it) else { return Err(cold_type("not an awaitable")) };
        let before = self.pending.exc_val;
        let step = match mode {
            AwaitMode::Wrap => self.gen_step(target, sent, chunk),
            _ if stage == 2 => return Err(VmErr::Raised("RuntimeError: cannot reuse already awaited __anext__()/asend()".into())),
            // Closing the awaitable spends it and leaves its generator as it is.
            _ if matches!(sent, Sent::Close) => { self.set_stage(it, 2); return Ok(Step::Return(Val::none())); }
            _ => {
                let sent = if stage == 0 {
                    if let Sent::Value(v) = sent && !v.is_none() { return Err(VmErr::TypeMsg("can't send non-None value to a just-started coroutine".into())); }
                    self.set_stage(it, 1);
                    match mode {
                        AwaitMode::Send => Sent::Value(arg),
                        AwaitMode::Throw => Sent::Throw(arg),
                        _ => Sent::Throw(self.builtin_value("GeneratorExit")?),
                    }
                } else { sent };
                self.with_roots([it], |vm| vm.agen_step(target, sent, mode == AwaitMode::Close, chunk))
            }
        };
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
    fn is_iterator(&self, v: Val) -> bool {
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
        match self.gen_step(aw, Sent::Value(sent), chunk) {
            // The yield passes up through the frame, which runs this step again with what it is sent.
            Ok(Step::Yield(v)) => { self.push(v); self.yielded = true; Ok(Some(Val::undef())) }
            Ok(Step::Return(v)) => {
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
        let sent = match name {
            "__next__" if self.coro_kind(g) == Some(CoroKind::Generator) => return self.resume_generator(g, None),
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

/* What a step of an awaited or delegated iterator gives, a value it yields up or the value it ended with. */
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step { Yield(Val), Return(Val) }

/* What resuming a body gave, a yield or a return. */
#[derive(Clone, Copy, Debug)]
enum Outcome { Yielded(Val), Returned(Val) }

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
