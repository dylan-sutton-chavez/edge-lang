use alloc::{vec, vec::Vec, rc::Rc};
use core::cell::RefCell;

use crate::parser::SSAChunk;
use super::super::VM;
use super::super::types::*;

/* An ip no chunk reaches, the same on 32 and 64 bits so a snapshot keeps it. */
const FINISHED: usize = u32::MAX as usize;

impl<'a> VM<'a> {

    // Resume coroutine, persist state on yield, restore caller on return. Suspended sync sub-frames run innermost-first, each pushing its result onto the next frame's stack at the Call site. The coro's `exception_frames` are restored before its body runs and saved back on yield, so `try`/`except` survives suspensions.
    pub fn resume_coroutine(&mut self, callee: Val) -> Result<Val, VmErr> {
        // Scheduler-driven resumes have nothing native above.
        let resume_safe = core::mem::take(&mut self.pending_exec_safe);
        let (outer_ip, outer_body, syncs, held) = match self.heap.get(callee) {
            HeapObj::Coroutine(c) => (c.ip, c.body, c.syncs.len(), c.syncs.len() + c.stack.len() + c.slots.len() + c.iters.len()),
            _ => return Err(cold_type("not a coroutine")),
        };
        if outer_ip == FINISHED { self.yielded = false; return Ok(Val::none()); }

        // Nested resumes from mutual awaits recurse in Rust, and each resume copies every suspended frame, so both stay bounded.
        if syncs >= crate::vm::frame::SUSPENDED_DEPTH || self.depth >= self.max_calls { return Err(cold_depth()); }
        // Re-entrant resume (`yield from g` inside g, `next(g)` from g's own body).
        if self.executing_coros.contains(&callee.0) {
            return Err(VmErr::Value("generator already executing"));
        }
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

    /* Live (non-terminal) coroutine count, the concurrency that bounds scheduler work. */
    fn scheduler_active(&self) -> usize {
        self.scheduler.iter().filter(|h| !h.state.is_terminal()).count()
    }

    /* Adds each of `coros` the scheduler does not hold yet, ready to run. */
    fn schedule(&mut self, coros: &[Val]) {
        for &coro in coros {
            if !self.scheduler.iter().any(|h| h.coro == coro) { self.scheduler.push(CoroutineHandle { coro, state: CoroState::Ready }); }
        }
    }

    /* Parks the running coroutine on `tasks` behind a placeholder the wake-loop overwrites with the outcome `kind` picks. */
    fn park_on(&mut self, tasks: Vec<Val>, kind: WaitKind) {
        self.push(Val::none());
        self.pending.waiting_for_children = Some((tasks, kind));
        self.yielded = true;
    }

    /* The scheduled state of `coro`, None once it left the scheduler. */
    fn state_of(&self, coro: Val) -> Option<CoroState> {
        self.scheduler.iter().find(|h| h.coro == coro).map(|h| h.state.clone())
    }

    /* The coroutines among `vals`, the only values a scheduler runs. */
    fn coroutines(&self, vals: Vec<Val>) -> Vec<Val> {
        vals.into_iter().filter(|&v| matches!(self.heap.try_get(v), Some(HeapObj::Coroutine(..)))).collect()
    }

    /* `await coro` and calling a coroutine `c()` parks the current coro on `target` (single-driver, like `run(target)`) and yields. The driving top loop resolves `target` and the wake-loop overwrites the placeholder with its value (or raises its error). Non-coroutine awaitables pass through unchanged at the call site. */
    pub(crate) fn await_coroutine(&mut self, target: Val) -> Result<(), VmErr> {
        if self.scheduler_active() >= self.max_calls {
            return Err(cold_depth());
        }
        self.schedule(&[target]);
        self.park_on(alloc::vec![target], WaitKind::Run(target));
        Ok(())
    }

    /* `run(*coros)`, single-driver model, pushes the targets into the global scheduler, parks the outer in `WaitingForChildren` with `WaitKind::Run(target)`, and yields. The top loop drains the children and wakes the outer when all are terminal. */
    pub fn call_run(&mut self, argc: u16) -> Result<(), VmErr> {
        // Cap live concurrency like call depth, unbounded task spawning is recursion-shaped.
        if self.scheduler_active() >= self.max_calls {
            return Err(cold_depth());
        }
        let raw_tasks = self.pop_n(argc as usize)?;
        if raw_tasks.is_empty() {
            self.push(Val::none());
            return Ok(());
        }
        let target = raw_tasks[0];
        if self.time_hook.is_none() { self.virtual_clock_ns = 0; }
        let coros = self.coroutines(raw_tasks);
        // `run(non_coro)` waits for nothing and gives None.
        if coros.is_empty() { self.push(Val::none()); return Ok(()); }
        self.schedule(&coros);
        self.park_on(coros, WaitKind::Run(target));
        Ok(())
    }

    // Sweep `WaitingForChildren` outers, enforce timeouts, then wake any whose tracked tasks are all terminal, finalizing per `WaitKind`. Gated by `waiting_for_children_count` so the common (no-nested-run) tick is one comparison.
    fn wake_waiting_outers(&mut self) {
        if self.waiting_for_children_count == 0 { return; }

        // Timeout enforcement, mark non-terminal tasks as CancelPending when their parent's deadline expired.
        let now = self.now_ns();
        let expired: Vec<Val> = self.scheduler.iter().filter_map(|h| {
            if let CoroState::WaitingForChildren { tasks, kind: WaitKind::Timeout { deadline_ns, .. } } = &h.state
                && now >= *deadline_ns {
                Some(tasks.clone())
            } else { None }
        }).flatten().collect();
        for t in expired {
            if let Some(h) = self.scheduler.iter_mut().find(|h| h.coro == t)
                && !h.state.is_terminal() && !matches!(h.state, CoroState::CancelPending) {
                h.state = CoroState::CancelPending;
            }
        }

        // Wake outers whose tasks are all terminal.
        loop {
            let candidate = self.scheduler.iter().find_map(|h| {
                let CoroState::WaitingForChildren { tasks, kind } = &h.state else { return None; };
                let all_terminal = tasks.iter().all(|t| {
                    self.scheduler.iter().find(|c| c.coro == *t)
                        .is_none_or(|c| c.state.is_terminal())
                });
                if !all_terminal { return None; }
                Some((h.coro, tasks.clone(), kind.clone()))
            });
            let Some((outer, tasks, kind)) = candidate else { return; };
            let new_state = self.compute_wake_outcome(outer, &tasks, &kind);
            self.scheduler.retain(|h| h.coro == outer || !tasks.contains(&h.coro));
            let idx = self.scheduler.iter().position(|h| h.coro == outer).unwrap();
            self.waiting_for_children_count -= 1;
            self.scheduler[idx].state = new_state;
        }
    }

    // Settles the outer coro from its finished tasks, a task error raises at the outer's park point.
    fn compute_wake_outcome(&mut self, outer: Val, tasks: &[Val], kind: &WaitKind) -> CoroState {
        match kind {
            WaitKind::Run(target) => {
                match self.state_of(*target) {
                    Some(CoroState::Errored(e)) => CoroState::Raising(e, self.pending.exc_val.take()),
                    Some(CoroState::Done(v)) => {
                        self.splice_outer_placeholder(outer, v);
                        CoroState::Ready
                    }
                    _ => {
                        self.splice_outer_placeholder(outer, Val::none());
                        CoroState::Ready
                    }
                }
            }
            WaitKind::Gather => {
                let mut first_err: Option<VmErr> = None;
                let mut results = Vec::with_capacity(tasks.len());
                for t in tasks {
                    match self.state_of(*t) {
                        Some(CoroState::Errored(e)) => {
                            if first_err.is_none() { first_err = Some(e); }
                            results.push(Val::none());
                        }
                        Some(CoroState::Done(v)) => results.push(v),
                        _ => results.push(Val::none()),
                    }
                }
                if let Some(e) = first_err {
                    // pending.exc_val may hold a later child's instance, force a rebuild from the first-in-order error.
                    self.pending.exc_val = None;
                    return CoroState::Raising(e, None);
                }
                match self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(results)))) {
                    Ok(list) => { self.splice_outer_placeholder(outer, list); CoroState::Ready }
                    Err(e) => CoroState::Raising(e, None),
                }
            }
            WaitKind::Timeout { deadline_ns, target } => {
                let deadline_hit = self.now_ns() >= *deadline_ns;
                match self.state_of(*target) {
                    Some(CoroState::Errored(e)) => CoroState::Raising(e, self.pending.exc_val.take()),
                    Some(CoroState::Done(v)) if !deadline_hit => {
                        self.splice_outer_placeholder(outer, v);
                        CoroState::Ready
                    }
                    _ => CoroState::Raising(VmErr::Raised("TimeoutError".into()), None),
                }
            }
        }
    }

    fn splice_outer_placeholder(&mut self, outer: Val, value: Val) {
        if let HeapObj::Coroutine(c) = self.heap.get_mut(outer) {
            let c = &mut **c;
            // Parked inside a plain helper, the placeholder sits on the innermost helper's stack.
            let top = match c.syncs.last_mut() {
                Some(frame) => frame.stack_delta.last_mut(),
                None => c.stack.last_mut(),
            };
            if let Some(top) = top { *top = value; }
        }
    }

    /* Single scheduler driver, picks a Ready coro and steps it. On no Ready, classifies the wait-state and yields to the host (PendingTimer / PendingHostCall / PendingEvent) or returns Ok when nothing alive remains. */
    pub(crate) fn top_loop(&mut self) -> Result<(), VmErr> {
        loop {
            // Charge the full scheduler scan so accumulating coroutines stay bounded.
            self.charge_steps(self.scheduler.len().max(1))?;
            self.wake_waiting_outers();
            let mut next_ready: Option<usize> = None;
            let mut min_wake: Option<u64> = None;
            let mut any_event = false;
            let mut any_host_call = false;
            let mut alive = false;
            for (i, h) in self.scheduler.iter().enumerate() {
                match &h.state {
                    CoroState::Ready | CoroState::CancelPending | CoroState::Raising(..) => { next_ready = Some(i); alive = true; break; }
                    CoroState::Sleeping(w) => {
                        alive = true;
                        if min_wake.is_none_or(|m| *w < m) { min_wake = Some(*w); }
                    }
                    CoroState::WaitingForChildren { kind: WaitKind::Timeout { deadline_ns, .. }, .. } => {
                        alive = true;
                        if min_wake.is_none_or(|m| *deadline_ns < m) { min_wake = Some(*deadline_ns); }
                    }
                    CoroState::WaitingEvent => { any_event = true; alive = true; }
                    CoroState::WaitingHostCall(_) => { any_host_call = true; alive = true; }
                    CoroState::WaitingForChildren { .. } => { alive = true; }
                    CoroState::Done(_) | CoroState::Errored(_) | CoroState::Cancelled => {}
                }
            }
            if !alive { return Ok(()); }
            if let Some(i) = next_ready {
                self.scheduler_step(i);
                // Coro stays Ready, re-entering resumes it.
                if core::mem::take(&mut self.pending.preempt_request) {
                    return Err(VmErr::HostYield(SchedulerStatus::Preempted));
                }
                continue;
            }
            // Yield priority order, sleep/timeout deadline > host call > event.
            match min_wake {
                // On the virtual clock a host call takes no time, so it answers before any deadline.
                Some(_) if any_host_call && self.time_hook.is_none() => return Err(VmErr::HostYield(SchedulerStatus::PendingHostCall)),
                Some(w) => {
                    let now = self.now_ns();
                    if w > now {
                        if self.time_hook.is_some() {
                            return Err(VmErr::HostYield(SchedulerStatus::PendingTimer(w)));
                        }
                        self.virtual_clock_ns = w;
                    }
                    let now = self.now_ns();
                    for h in self.scheduler.iter_mut() {
                        if let CoroState::Sleeping(w) = h.state && w <= now {
                            h.state = CoroState::Ready;
                        }
                    }
                }
                None => {
                    if any_host_call { return Err(VmErr::HostYield(SchedulerStatus::PendingHostCall)); }
                    if any_event { return Err(VmErr::HostYield(SchedulerStatus::PendingEvent)); }
                    return Ok(());
                }
            }
        }
    }

    /* Run a CancelPending coroutine's `finally` bodies then terminate it, skipping `except`. */
    fn run_cancellation(&mut self, coro: Val) -> CoroState {
        // A suspended sync helper holds cleanup this unwind can't reach.
        let has_sync = matches!(self.heap.get(coro),
            HeapObj::Coroutine(c) if !c.syncs.is_empty());
        if has_sync {
            return CoroState::Errored(VmErr::Runtime(
                "cannot cancel a coroutine suspended inside a synchronous helper"));
        }
        self.pending.clear_waits();
        self.pending_exec_safe = true;
        self.cancelling = true;
        self.resume_raise = Some(VmErr::Raised("CancelledError".into()));
        let result = self.resume_coroutine(coro);
        let yielded = self.yielded;
        self.yielded = false;
        self.cancelling = false;
        self.resume_raise = None;
        match result {
            Err(VmErr::Raised(ref s)) if s == "CancelledError" => CoroState::Cancelled,
            Ok(_) if yielded => CoroState::Errored(VmErr::Runtime(
                "cannot suspend (await/sleep) in a finally during cancellation")),
            Err(e) => CoroState::Errored(e),
            Ok(_) => CoroState::Cancelled,
        }
    }

    fn scheduler_step(&mut self, idx: usize) {
        let coro = self.scheduler[idx].coro;
        if matches!(self.scheduler[idx].state, CoroState::CancelPending) {
            self.scheduler[idx].state = self.run_cancellation(coro);
            return;
        }
        // Snapshot before resume so a yield during sleep / receive / run can read it.
        self.pending.clear_waits();
        self.pending_exec_safe = true;
        if let CoroState::Raising(e, exc) = core::mem::replace(&mut self.scheduler[idx].state, CoroState::Ready) {
            self.resume_raise = Some(e);
            self.pending.exc_val = exc;
        }
        let result = self.resume_coroutine(coro);
        // An early resume failure never reached the park point, so the raise must not leak.
        self.resume_raise = None;
        let yielded = self.yielded;
        self.yielded = false;
        let new_state = match result {
            Err(e) => CoroState::Errored(e),
            Ok(_) if yielded => {
                // Suspension precedence order, sleep > receive > host-call > children > bare yield.
                if let Some(until) = self.pending.sleep_until_ns.take() {
                    CoroState::Sleeping(until)
                } else if core::mem::replace(&mut self.pending.event_wait_request, false) {
                    CoroState::WaitingEvent
                } else if core::mem::replace(&mut self.pending.host_call_request, false) {
                    CoroState::WaitingHostCall(self.pending.host_call_id)
                } else if let Some((tasks, kind)) = self.pending.waiting_for_children.take() {
                    self.waiting_for_children_count += 1;
                    CoroState::WaitingForChildren { tasks, kind }
                } else {
                    CoroState::Ready
                }
            }
            Ok(v) => CoroState::Done(v),
        };
        self.scheduler[idx].state = new_state;
    }

    /* Suspend until `s` real seconds elapse. */
    pub fn call_sleep(&mut self) -> Result<(), VmErr> {
        let n = self.pop()?;
        let secs = num_as_f64(n, &self.heap).ok_or_else(|| cold_type("sleep() requires a number"))?.max(0.0);
        let until = self.now_ns().saturating_add((secs * 1_000_000_000.0) as u64);
        self.pending.sleep_until_ns = Some(until);
        // Push None as the yield value, the scheduler ignores it.
        self.push(Val::none());
        self.yielded = true;
        Ok(())
    }

    /* `gather(*coros)`, single-driver, pushes all targets, parks the outer in `WaitingForChildren` with `WaitKind::Gather`, and yields. Wake-loop builds the result list (or raises the first child error). */
    pub fn call_gather(&mut self, argc: u16) -> Result<(), VmErr> {
        // Cap live concurrency like call depth, unbounded task spawning is recursion-shaped.
        if self.scheduler_active() >= self.max_calls {
            return Err(cold_depth());
        }
        let tasks = self.pop_n(argc as usize)?;
        let coros = self.coroutines(tasks);
        if coros.is_empty() { return self.alloc_and_push_list(Vec::new()); }
        self.schedule(&coros);
        self.park_on(coros, WaitKind::Gather);
        Ok(())
    }

    /* `with_timeout(seconds, coro)`, single-driver, pushes the target, parks the outer with `WaitKind::Timeout { deadline_ns, target }`, and yields. The top loop enforces the deadline by marking the target CancelPending when it expires. The wake-loop returns the target's value or `TimeoutError`. */
    pub fn call_with_timeout(&mut self) -> Result<(), VmErr> {
        let coro = self.pop()?;
        let secs_v = self.pop()?;
        if !matches!(self.heap.try_get(coro), Some(HeapObj::Coroutine(..))) {
            return Err(cold_type("with_timeout() requires a coroutine"));
        }
        let secs = num_as_f64(secs_v, &self.heap).ok_or_else(|| cold_type("with_timeout() seconds must be a number"))?;
        let deadline_ns = self.now_ns().saturating_add((secs.max(0.0) * 1_000_000_000.0) as u64);
        self.schedule(&[coro]);
        self.park_on(vec![coro], WaitKind::Timeout { deadline_ns, target: coro });
        Ok(())
    }

    /* cancel(coro) flags the coroutine for cancellation, ignored once it is terminal. */
    pub fn call_cancel(&mut self) -> Result<(), VmErr> {
        let coro = self.pop()?;
        if let Some(h) = self.scheduler.iter_mut().find(|h| h.coro == coro)
            && !h.state.is_terminal() {
            h.state = CoroState::CancelPending;
        }
        self.push(Val::none()); Ok(())
    }

    /* Pop oldest queued message, if empty, park in `WaitingEvent` until `run_push_event`. */
    pub fn call_receive(&mut self) -> Result<(), VmErr> {
        if !self.event_queue.is_empty() {
            let val = self.event_queue.remove(0);
            self.push(val);
        } else {
            self.pending.event_wait_request = true;
            self.push(Val::none());
            self.yielded = true;
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
}
