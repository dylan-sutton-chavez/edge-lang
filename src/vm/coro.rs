use alloc::{string::String, vec, vec::Vec};

use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use super::VM;
use crate::value::*;
use super::lower::Code;
use crate::value::{ExceptionFrame, BodyRef};
use super::lower;
use crate::code::Ins;
use crate::lang::{Act, Body, EngineError, Gen};

use super::frame::{Floor, RESUME_RAW};
use super::run::yield_top;
use super::exc::RAW_HANDLER;
use super::act::{HEAD, NO_POOL, BASES, GEN, CTOR, JOB, TASK, mark_of, top_of, Bases, FLAT, NO_DEST};
use crate::r#unsafe::{CodePtr, ChunkPtr, get_in, move_last, put, record_mut, reg};

/* the module body's helpers opened as loop frames noting by depth those with more under */
pub(crate) struct Chain { pub(crate) ask: bool, pub(crate) next: bool, pub(crate) coro: Val, pub(crate) chains: Vec<(usize, Val, usize)> }

impl Default for Chain {
    fn default() -> Self { Self { ask: false, next: false, coro: Val::undef(), chains: Vec::new() } }
}

/* an unreachable ip the same on 32 and 64 bits so a snapshot keeps it */
pub(crate) const FINISHED: usize = u32::MAX as usize;

/* whether `at` falls in sections or stubs or generator loops that no compiler index names */
pub(crate) fn parks_raw(code: &Code, at: usize) -> bool {
    code.block.as_deref().is_some_and(|b| b.sections.first().is_some_and(|s| at >= s.start as usize) || b.loops.iter().any(|&(from, to, ..)| (from as usize..to as usize).contains(&at)))
}

/* whether `code` resumes a park at `ip` as a compiler resume point or raw index */
fn resumes(code: &Code, ip: usize) -> bool {
    if ip & RESUME_RAW != 0 { (ip & !RESUME_RAW) < code.ins.len() } else { code.resume(code.lowered(ip)) == ip }
}

/* whether lowered `code` resumes a generator parked at compiler `ip` with rows to `top` */
fn takes_park(code: &Code, ip: usize, top: u16, inner: bool) -> bool {
    // a frame under a callee resumes in the compiler's own code
    if !inner { return false; }
    let entry = code.entry_rows(ip);
    // the rows it holds are the entry rows or one more for a sent value
    match usize::from(top).checked_sub(usize::from(code.slots)) {
        _ if top == 0 => entry == 0,
        Some(rows) => rows == entry || rows == entry + 1,
        None => false,
    }
}

/* the layout a window laid for `code` notes and 0 for unchanged code */
#[inline(always)]
fn laid_for(code: &Code) -> u32 { if code.unchanged() { 0 } else { code.layout } }

/* the ForIter of the `yield from` a body parked at `ip` waits in */
fn delegation_at(code: &[crate::syntax::chunk::Instruction], ip: usize) -> Option<usize> {
    // first-run code parks past its PopTop and lowered code at the step
    let marked = |k: usize| code.get(k).is_some_and(|i| i.opcode == OpCode::Yield && i.operand == crate::syntax::parser::YIELD_FROM);
    let step = |k: usize| code.get(k).is_some_and(|i| i.opcode == OpCode::ForIter);
    // a plain yield parks right past itself
    if ip.checked_sub(1).and_then(|k| code.get(k)).is_some_and(|i| i.opcode == OpCode::Yield) { return None; }
    if ip >= 3 && marked(ip - 2) && step(ip - 3) { return Some(ip - 3); }
    (step(ip) && marked(ip + 1)).then_some(ip)
}

impl<'a> VM<'a> {

    /* resumes the module body `callee` the host drives until it returns or raises or waits */
    pub(crate) fn resume_root(&mut self, callee: Val) -> Result<Val, VmErr> {
        (self.redo.on, self.redo.lazy) = (false, false);
        let resume_safe = core::mem::take(&mut self.pending_exec_safe);
        self.for_steps = false;
        self.wake_gen(callee);
        let (outer_ip, outer_layout, syncs, held, top) = match self.heap.get(callee) {
            HeapObj::Coroutine(c) => (c.ip, c.layout, c.syncs.len(), c.syncs.len() + c.levels.len() + c.slots.len() + c.iters.len(), c.top),
            _ => return Err(EngineError::cannot(Act::Resume(Body::Coroutine), None)),
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
        // state moves out while it runs and a yield puts it back with its helpers
        let (mut slots, mut levels, mut iters, mut excs) = match self.heap.get_mut(callee) {
            HeapObj::Coroutine(c) => { (c.running, c.top) = (true, 0); (core::mem::take(&mut c.slots), core::mem::take(&mut c.levels), core::mem::take(&mut c.iters), core::mem::take(&mut c.excs)) }
            _ => return Err(EngineError::cannot(Act::Resume(Body::Coroutine), None)),
        };
        let (level_len, iter_len, exc_len) = self.restore_into(&mut levels, &mut iters, &mut excs);
        let saved_resume_ip = self.resume_ip;
        self.yielded = false;
        self.depth += 1;
        self.pending_exec_exc_base = Some(exc_len);
        self.pending_exec_safe = resume_safe;
        if syncs != 0 { (self.chain.next, self.chain.coro) = (true, callee); }
        self.resume_layout = outer_layout;
        self.resume_ip = outer_ip;
        let pool = self.pool_of(self.chunk);
        // a body resumes with the top it parked with
        self.park_top = top as u32;
        let result = self.exec_pooled(self.chunk, pool, &mut slots, false);
        let parked = core::mem::take(&mut self.park_top) as u16;
        self.depth -= 1;
        let finished = match &result { Ok(_) => !self.yielded, Err(e) => !matches!(e, VmErr::HostYield(_)) };
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(callee) { c.running = false; if finished { c.ip = FINISHED; } }
        let result = match result {
            Ok(v) => v,
            Err(e) => {
                self.levels.truncate(level_len.min(self.levels.len()));
                self.iter_stack.truncate(iter_len.min(self.iter_stack.len()));
                self.exception_stack.truncate(exc_len.min(self.exception_stack.len()));
                (self.resume_raise, self.resume_ip) = (None, saved_resume_ip);
                return Err(e);
            }
        };
        if !self.yielded {
            // a module body that ran to its end leaves no recursion level
            debug_assert!(self.preempt_every != 0 || self.levels.len() == level_len, "the module body ended its levels at {} over {level_len}", self.levels.len());
            self.levels.truncate(level_len);
            self.iter_stack.truncate(iter_len);
            self.exception_stack.truncate(exc_len);
            self.resume_ip = saved_resume_ip;
            return Ok(result);
        }
        let layout = core::mem::take(&mut self.resume_layout);
        self.save_frames(level_len, iter_len, exc_len, &mut levels, &mut iters, &mut excs);
        let newer = core::mem::take(&mut self.pending_sync_frames);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(callee) {
            (c.ip, c.layout, c.slots, c.levels, c.iters, c.excs, c.top) = (self.resume_ip, layout, slots, levels, iters, excs, parked);
            // the helpers a chained run left kept lie under the ones that parked
            c.syncs.extend(newer.into_iter().rev());
        }
        self.resume_ip = saved_resume_ip;
        Ok(result)
    }

    /* the item `co` keeps pending with the next it holds taking its place */
    pub(crate) fn take_pending(&mut self, co: Val) -> Val {
        let next = match self.held.iter().position(|h| h.0.0 == co.0) {
            Some(k) => { let v = self.held[k].1.pop().unwrap_or(Val::undef()); if self.held[k].1.is_empty() { self.held.swap_remove(k); } v }
            None => Val::undef(),
        };
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return Val::none() };
        core::mem::replace(&mut c.pending, next)
    }

    /* one step of generator `co` giving none once finished or its resume's error while running */
    #[inline]
    pub(crate) fn step_coroutine(&mut self, co: Val) -> Result<Val, VmErr> {
        match self.heap.try_get(co) {
            // an item a builtin's step took ahead or a job held is its next step
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

    /* the layout `body` of `co` resumes in once its helpers open as frames else `None` */
    #[cold]
    #[inline(never)]
    fn chains_from(&mut self, co: Val, body: BodyRef, ip: usize, layout: u32) -> Option<u32> {
        if !matches!(self.heap.try_get(co), Some(HeapObj::Coroutine(_))) { return None; }
        let (chunk, pool): (&SSAChunk, usize) = match body { BodyRef::Fn(fi) => (&self.functions[fi].1, self.fn_pool[fi]), BodyRef::Module => (self.chunk, self.pool_of(self.chunk)) };
        // a park inside sections names a place of its own code never of first code
        let raw = ip & crate::vm::frame::RESUME_RAW != 0;
        // a body in first code resumes it remade as a cold one heats once more
        let p = &self.pools[pool];
        if !raw && if p.code.is_none() { p.heat < super::lower::HOT_LOOP } else { p.cold && p.heat + 1 < super::lower::HOT_LOOP } { return Some(layout); }
        // the resume code is made now as the run would and kept under its layout
        (self.resume_ip, self.resume_layout) = (ip, layout);
        let (code, once) = self.frame_code(chunk, pool);
        (self.resume_ip, self.resume_layout) = (0, 0);
        if raw && ip & !crate::vm::frame::RESUME_RAW >= code.ins.len() { return None; }
        // code that lays the park point otherwise resumes the body in the compiler's own code
        let top = match self.heap.try_get(co) { Some(HeapObj::Coroutine(c)) => c.top, _ => 0 };
        let (code, once) = if resumes(&code, ip) { (code, once) } else if super::vet::rows_hold(chunk, self.pools[pool].shape, ip, top, false) { (self.compiler_code(chunk, pool), true) } else { return None };
        let laid = code.layout;
        if once { self.retired.push(code); }
        Some(laid)
    }

    /* whether `co`'s helpers are too many or keep too much to open as frames */
    pub(crate) fn chain_refused(&self, co: Val) -> bool {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return false };
        if c.syncs.len() >= crate::vm::act::SUSPENDED_DEPTH { return true; }
        // what they keep counts only up to the bound looking at each helper once
        let mut kept = c.levels.len();
        for f in c.syncs.iter().rev() {
            if kept >= crate::vm::act::SUSPENDED_DEPTH { return true; }
            kept += f.levels.len();
        }
        kept >= crate::vm::act::SUSPENDED_DEPTH
    }

    /* opens the innermost helper `co` waits inside on `v` giving where it goes on */
    pub(crate) fn chain_next(&mut self, co: Val, ret: usize, dest: u16, v: Option<Val>) -> usize {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return ret };
        let Some(f) = c.syncs.pop() else { return ret };
        let more = !c.syncs.is_empty();
        // the first helper opened is the innermost which waits on no call
        let code = self.sync_code(&f, v.is_none());
        let at = self.enter_sync(f, code, (ret, dest), v);
        if more {
            let h = self.head_at(self.top_pool);
            self.regs[h] = Val::int(self.regs[h].as_int() | crate::vm::act::TASK << 40);
            self.chain.chains.push((self.acts, co, ret));
        }
        at
    }

    /* the running helper's coroutine and return point if more lie under it clearing its mark */
    #[inline(never)]
    pub(crate) fn chain_top(&mut self) -> Option<(Val, usize)> {
        let &(depth, co, ret) = self.chain.chains.last()?;
        if depth != self.acts { return None; }
        self.chain.chains.pop();
        let h = self.head_at(self.top_pool);
        self.regs[h] = Val::int(self.regs[h].as_int() & !(crate::vm::act::TASK << 40));
        Some((co, ret))
    }

    /* `chain_next` after the top helper raised in `fi` noting the call below in the traceback */
    #[inline(never)]
    pub(crate) fn chain_raise(&mut self, co: Val, ret: usize, dest: u16, fi: usize) -> usize {
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) && let Some(f) = c.syncs.last() {
            let (ip, caller) = (f.ip, &self.functions[f.fi].1);
            self.note_frame(fi, Some((ip & !crate::vm::frame::RESUME_RAW).saturating_sub(1) as u32), caller);
        }
        self.chain_next(co, ret, dest, None)
    }

    /* helper `f` resumes in its parked code if kept else new code kept while running */
    fn sync_code(&mut self, f: &ParkedFrame, inner: bool) -> CodePtr {
        if let Some(k) = self.code_laid(f.fi, f.layout) {
            if !k.get().unchanged() { return k; }
            // first code heats as its resume would and lowers once hot keeping compiler resume points
            let pool = self.fn_pool[f.fi];
            self.pools[pool].heat += 1;
            if self.pools[pool].heat < crate::vm::lower::HOT_LOOP { return k; }
            let chunk = &self.functions[f.fi].1;
            let lowered = self.lowered_code(chunk, pool, false);
            // lowered code that lays its park point otherwise leaves it in its first code
            return if resumes(&lowered, f.ip) { CodePtr::of(&lowered) } else { k };
        }
        let (chunk, pool) = (&self.functions[f.fi].1, self.fn_pool[f.fi]);
        (self.resume_ip, self.resume_layout) = (f.ip, f.layout);
        let (code, once) = self.frame_code(chunk, pool);
        (self.resume_ip, self.resume_layout) = (0, 0);
        // code that lays the park point otherwise resumes the helper in the compiler's own code
        let (code, once) = if resumes(&code, f.ip) || !super::vet::rows_hold(chunk, self.pools[pool].shape, f.ip, f.top, inner) { (code, once) } else { (self.compiler_code(chunk, pool), true) };
        let k = CodePtr::of(&code);
        if once { self.retired.push(code); }
        k
    }

    /* the compiler's own code of `chunk` remade which resumes at every resume point */
    #[cold]
    #[inline(never)]
    fn compiler_code(&mut self, chunk: &SSAChunk, pool: usize) -> alloc::rc::Rc<Code> {
        let (kinds, module) = self.chunk_kinds(chunk);
        let shape = self.pools[pool].shape;
        self.first_identity(&lower::fuse_method_calls(chunk, shape), chunk, pool, kinds, module)
    }

    /* opens helper `f` a coroutine waits in going on at `ret` with `v` */
    fn enter_sync(&mut self, f: ParkedFrame, code: CodePtr, (ret, dest): (usize, u16), v: Option<Val>) -> usize {
        let ParkedFrame { ip, fi, func, slots, mut levels, mut iter_delta, mut exception_delta, instance, top, .. } = f;
        let pool = self.fn_pool[fi];
        let chunk: &SSAChunk = &self.functions[fi].1;
        let names = chunk.names.len();
        let base = self.open_window(pool, slots.len().max(names), names, false);
        // constants and state may come from a restore or later window so it retakes them
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        self.pools[pool].wcode = 0;
        self.note_window(pool);
        self.regs[base..base + slots.len()].copy_from_slice(&slots);
        let cold = self.cold_bases(code.get().flat() && iter_delta.is_empty() && exception_delta.is_empty());
        let level_base = self.levels.len();
        self.restore_into(&mut levels, &mut iter_delta, &mut exception_delta);
        self.open_helper(fi, func, pool, code, cold, (ret, dest), level_base, instance);
        self.run_in(pool);
        let c = code.get();
        // a helper takes back the top it parked with where its code reads it
        let top = if top as usize > self.pools[pool].wlen { 0 } else { u32::from(top) };
        self.top = if c.rows != 0 { top.max(u32::from(c.rows)) } else { top };
        // the value its call gives goes where its resume takes it
        self.resume_at(chunk, c, pool, ip, v)
    }

    /* the code of function `fi` laid out as `layout` which its pool holds or retired */
    pub(crate) fn code_laid(&self, fi: usize, layout: u32) -> Option<CodePtr> {
        if layout == 0 { return None; }
        self.pools[self.fn_pool[fi]].code.iter().chain(&self.retired).find(|c| c.layout == layout).map(CodePtr::of)
    }

    /* whether the yield waits on an event or host call or preemption or parked helper */
    #[inline]
    pub(crate) fn waits(&self) -> bool {
        let p = &self.pending;
        p.event_wait_request || p.host_call_request || p.preempt_request || !self.pending_sync_frames.is_empty()
    }

    /* the error resuming `g` from its own body raises */
    #[cold]
    fn running_error(&self, g: Val) -> VmErr {
        EngineError::Generator(Gen::Running(self.coro_kind(g).unwrap_or(CoroKind::Generator).body())).into()
    }

    /* the error a suspension inside a generator raises with its requests dropped */
    #[cold]
    #[inline(never)]
    pub(crate) fn suspended_in_generator(&mut self, fi: Option<usize>) -> VmErr {
        let _ = fi;
        self.suspended_in(" a generator or a coroutine")
    }

    /* the error a suspension inside `what` raises with its requests dropped */
    #[cold]
    #[inline(never)]
    pub(crate) fn suspended_in(&mut self, what: &'static str) -> VmErr {
        let p = &mut self.pending;
        (p.event_wait_request, p.host_call_request, p.preempt_request) = (false, false, false);
        self.pending_sync_frames.clear();
        // the run carries on raising rather than from where it would have resumed
        (self.resume_ip, self.resume_layout) = (0, 0);
        EngineError::Generator(Gen::Wait { inside: what }).into()
    }

    /* steps the module body to a return or error or a `HostYield` that `run()` resumes */
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
            // a resume failing early never reached the park point so its raise must not leak
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

    /* which of a generator or a coroutine `g` is and None for anything else */
    pub(crate) fn coro_kind(&self, g: Val) -> Option<CoroKind> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(g) else { return None };
        Some(match c.body {
            BodyRef::Fn(fi) if !self.is_async_fn(fi) => CoroKind::Generator,
            _ => CoroKind::Coroutine,
        })
    }

    /* the instructions of a coroutine body */
    fn body_code(&self, body: BodyRef) -> &[crate::syntax::chunk::Instruction] {
        match body { BodyRef::Fn(fi) => &self.functions[fi].1.instructions, BodyRef::Module => &self.chunk.instructions }
    }

    /* what `g` waits inside by `yield from` or `await` with undef for a builtin iterator */
    pub(crate) fn delegate_of(&self, g: Val) -> Option<(Val, Via)> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(g) else { return None };
        if c.ip == 0 || c.ip == FINISHED || !c.syncs.is_empty() { return None; }
        let code = self.body_code(c.body);
        if code.get(c.ip).is_some_and(|i| i.opcode == OpCode::Await) { return (c.top as usize).checked_sub(2).and_then(|k| self.gen_slot_val(g, k)).map(|v| (v, Via::Await)); }
        delegation_at(code, c.ip)?;
        match c.iters.last() {
            Some(&(IterFrame::Coroutine(v) | IterFrame::UserDefined(v))) => Some((v, Via::YieldFrom)),
            Some(_) => Some((Val::undef(), Via::YieldFrom)),
            None => None,
        }
    }

    /* `aw_step` on the awaitable at slot `at` an `await` sends `sent` into */
    pub(crate) fn await_on(&mut self, at: usize, sent: Val, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> { self.aw_step(self.slot(at), sent, chunk) }

    /* the step of coroutine `sub` an `await` sends `sent` into or None once it opened */
    pub(crate) fn aw_step(&mut self, sub: Val, sent: Val, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> {
        match core::mem::take(&mut self.await_back) {
            1 => return Ok(Some(Step::Yield(sent))),
            2 => return Ok(Some(Step::Return(sent))),
            _ => {}
        }
        // an awaited coroutine resumes as its `send` resumes it with its frame the job's
        let before = self.pending.exc_val.unwrap_or(Val::undef());
        self.coro_frame = Some((sub, crate::vm::jobs::Takes::Await, before));
        let r = self.with_roots([sub], |vm| vm.gen_resume(sub, Sent::Value(sent), chunk));
        self.coro_frame = None;
        Ok(match r? { Outcome::Opened => None, Outcome::Yielded(v) => Some(Step::Yield(v)), Outcome::Returned(v) => Some(Step::Return(v)) })
    }

    /* one step of what an `await` or `yield from` or a driver steps with `sent` */
    pub(crate) fn gen_step(&mut self, it: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        match self.coro_kind(it) {
            Some(_) => Ok(match self.gen_resume(it, sent, chunk)? { Outcome::Yielded(v) => Step::Yield(v), Outcome::Returned(v) => Step::Return(v), Outcome::Opened => return Err(cold_runtime("a generator opened past its call")) }),
            None => self.iter_send(it, sent, chunk),
        }
    }

    /* resumes generator or coroutine `g` with `sent` reaching what it waits inside first */
    fn gen_resume(&mut self, g: Val, sent: Sent, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let not_one = || EngineError::cannot(Act::Resume(Body::Generator), None);
        let kind = self.coro_kind(g).ok_or_else(not_one)?;
        let (ip, body, running) = match self.heap.get(g) { HeapObj::Coroutine(c) => (c.ip, c.body, c.running), _ => return Err(not_one()) };
        if running || self.executing_coros.contains(&g.0) { return Err(self.running_error(g)); }
        if ip == FINISHED {
            // items a job held for a builtin that stopped short come out as its steps
            if matches!(sent, Sent::Value(_)) && matches!(self.heap.get(g), HeapObj::Coroutine(c) if !c.pending.is_undef()) { return Ok(Outcome::Yielded(self.take_pending(g))); }
            return match sent {
                Sent::Close => Ok(Outcome::Returned(Val::none())),
                _ if kind == CoroKind::Coroutine => Err(EngineError::Generator(Gen::Reused).into()),
                Sent::Throw(e) => Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))),
                Sent::Value(_) => Ok(Outcome::Returned(Val::none())),
            };
        }
        if ip == 0 {
            match sent {
                Sent::Value(v) if !v.is_none() => return Err(EngineError::Generator(Gen::FreshSend(kind.body())).into()),
                Sent::Close => { self.finish_generator(g); return Ok(Outcome::Returned(Val::none())); }
                Sent::Throw(e) => { self.finish_generator(g); return Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))); }
                Sent::Value(_) => return self.step_coro(g),
            }
        }
        if let Some((sub, via)) = self.delegate_of(g) {
            let sub_step = match sent {
                // `await` and `async for` and `yield from` that `next` resumes pass sent values on themselves
                Sent::Value(v) if via != Via::YieldFrom || v.is_none() => None,
                Sent::Value(v) => Some(self.delegate_on(sub, Sent::Value(v), Cont::After(g, via), chunk)),
                // the close signal closes what it waits inside then raises in `g`
                Sent::Throw(e) if self.exc_name_of(e) == Some(self.natives.names().error(ErrKind::Close)) => match self.delegate_on(sub, Sent::Close, Cont::Exit(g, via, e), chunk) {
                    Ok(None) => return Ok(Outcome::Opened),
                    r => r.err().map(Err),
                },
                Sent::Throw(e) => Some(self.delegate_on(sub, Sent::Throw(e), Cont::After(g, via), chunk)),
                // what closing the inner one raises replaces the close signal in `g`
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
                // a value-position `yield` left a placeholder on the saved stack that the sent value takes
                let code = self.body_code(body);
                let at_yield = ip > 0 && code.get(ip - 1).is_some_and(|i| i.opcode == OpCode::Yield) || self.delegate_of(g).is_some_and(|(_, via)| via != Via::YieldFrom);
                // a yield parks with its row as the slot the value takes
                if at_yield && let HeapObj::Coroutine(c) = self.heap.get(g) && c.syncs.is_empty() && let Some(at) = (c.top as usize).checked_sub(1) && let Some(slot) = self.gen_slot(g, at) { *slot = v; }
                self.step_coro(g)
            }
            Sent::Throw(e) => {
                let err = self.with_roots([e], |vm| vm.raise_value(e, chunk));
                self.throw_in(g, err)
            }
            Sent::Close => self.close_in(g, None, kind, chunk),
        }
    }

    /* `delegate` or `None` once `sub` opened as a frame whose end runs `cont` */
    fn delegate_on(&mut self, sub: Val, sent: Sent, cont: Cont, chunk: &SSAChunk) -> Result<Option<Step>, VmErr> {
        // a user iterator's `send` `throw` or `close` runs as a frame of a job
        if let Some((_, takes, before)) = self.coro_frame && matches!(takes, crate::vm::jobs::Takes::Next(_) | crate::vm::jobs::Takes::Close) && self.coro_kind(sub).is_none() && let Some((f, arg)) = self.sent_hook(sub, sent) {
            let base = self.conts.len() as u32;
            let job = alloc::boxed::Box::new(SentCall { it: sub, f, arg, close: matches!(sent, Sent::Close), base, before: self.pending.exc_val });
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

    /* what `cont` makes of the awaited frame's step `r` as the delegating call would have */
    fn run_cont(&mut self, cont: Cont, r: Result<Step, VmErr>) -> Result<Outcome, VmErr> {
        let chunk = self.chunk;
        match (cont, r) {
            (Cont::After(g, via), r) | (Cont::Exit(g, via, _), r @ Err(_)) => self.after_delegate(g, via, r, chunk),
            (Cont::Exit(g, _, e), Ok(_)) => { let err = self.with_roots([e], |vm| vm.raise_value(e, chunk)); self.throw_in(g, err) }
            (Cont::Close(g, kind), Err(e)) => self.close_in(g, Some(e), kind, chunk),
            (Cont::Close(g, kind), Ok(_)) => self.close_in(g, None, kind, chunk),
        }
    }

    /* a `Then` frame's `r` runs waits from `base` to a frame they open or `ret` */
    #[cold]
    #[inline(never)]
    pub(crate) fn then_end(&mut self, base: usize, r: Result<Step, VmErr>, ret: usize, to: crate::vm::act::Dest) -> Result<usize, (usize, VmErr)> {
        // the result and any frame the waits open go where the first call takes it
        self.delivering(to, |vm| vm.then_waits(base, r, ret))
    }

    /* `then_end` delivering to `job_to` */
    fn then_waits(&mut self, base: usize, mut r: Result<Step, VmErr>, ret: usize) -> Result<usize, (usize, VmErr)> {
        let mut waits = self.conts.split_off(base.min(self.conts.len()));
        while !waits.is_empty() {
            let t = waits.remove(0);
            let rest = self.conts.len();
            let last = waits.is_empty();
            self.conts.append(&mut waits);
            let g = match t.cont { Cont::After(g, _) | Cont::Exit(g, ..) | Cont::Close(g, _) => g };
            self.coro_frame = Some((g, if last { t.takes } else { crate::vm::jobs::Takes::Then(rest as u32) }, t.before));
            let out = self.run_cont(t.cont, r);
            self.coro_frame = None;
            if matches!(out, Ok(Outcome::Opened)) {
                // the opened frame's throw raises where this returns so no loop turn raises it again
                (self.entered, self.yielded, self.thrown) = (false, false, false);
                self.gather_ret(ret);
                return match self.resume_raise.take() { Some(e) => Err((self.gen_entry, e)), None => Ok(self.gen_entry) };
            }
            waits = self.conts.split_off(rest);
            r = out.map(|o| match o { Outcome::Yielded(v) => Step::Yield(v), _ => match o { Outcome::Returned(v) => Step::Return(v), _ => Step::Return(Val::none()) } });
            if last { return self.then_give(t.takes, r, ret); }
        }
        Err((ret - 1, cold_runtime("a wait ended with none left")))
    }

    /* gives the first call's generator step `r` back at `ret` as that call takes it */
    fn then_give(&mut self, takes: crate::vm::jobs::Takes, r: Result<Step, VmErr>, ret: usize) -> Result<usize, (usize, VmErr)> {
        use crate::vm::jobs::Takes;
        let v = match (takes, r) {
            (Takes::Next(d), Ok(Step::Return(v))) if d.is_undef() => return Err((ret - 1, self.stop_iteration(v))),
            (Takes::Next(d), Ok(Step::Return(_))) => d,
            (Takes::Next(_) | Takes::Close, Ok(Step::Yield(v))) | (Takes::Close, Ok(Step::Return(v))) => v,
            (Takes::Await, Ok(Step::Yield(v))) => { self.await_back = 1; v }
            (Takes::Await, Ok(Step::Return(v))) => { self.await_back = 2; v }
            (_, Err(e)) => return Err((ret - 1, e)),
            _ => return Err((ret - 1, cold_runtime("a wait gave a call it does not take"))),
        };
        Ok(self.deliver_to(v, ret, self.job_to))
    }

    /* steps iterator `sub` a suspended body waits in which is undef for a `next`-only builtin */
    fn delegate(&mut self, sub: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        if !sub.is_undef() { return self.gen_step(sub, sent, chunk); }
        match sent {
            Sent::Value(_) => Err(EngineError::Generator(crate::lang::Gen::Sent).into()),
            Sent::Throw(e) => Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))),
            Sent::Close => Ok(Step::Return(Val::none())),
        }
    }

    /* carries on `g` after what it waits inside took a step */
    fn after_delegate(&mut self, g: Val, via: Via, step: Result<Step, VmErr>, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let _ = chunk;
        match step {
            Ok(Step::Yield(v)) => Ok(Outcome::Yielded(v)),
            Ok(Step::Return(v)) => { self.land(g, via, v); self.step_coro(g) }
            Err(e) => self.throw_in(g, e),
        }
    }

    /* settles the wait of `g` with `v` which its `yield from` or `await` evaluates to */
    fn land(&mut self, g: Val, via: Via, v: Val) {
        let exit = match self.heap.get(g) {
            HeapObj::Coroutine(c) if via == Via::YieldFrom => { let code = self.body_code(c.body); delegation_at(code, c.ip).map(|at| code[at].operand as usize) }
            _ => None,
        };
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) else { return };
        match via {
            // the loop exit runs `LoadYieldFrom` which takes the value
            Via::YieldFrom => {
                c.iters.pop();
                if let Some(exit) = exit { c.ip = exit; }
                self.yield_from_value = v;
            }
            // the result replaces the awaitable and its placeholder and the body resumes past `Await`
            Via::Await => {
                let at = (c.top as usize).checked_sub(2);
                (c.top, c.ip) = (c.top.saturating_sub(1), c.ip + 1);
                if let Some(at) = at && let Some(slot) = self.gen_slot(g, at) { *slot = v; }
            }
        }
    }

    /* resumes `g` as a loop frame `coro_frame` names with its job taking the step */
    fn step_coro(&mut self, g: Val) -> Result<Outcome, VmErr> {
        // a delegated generator opens as a frame too and its end runs its delegators' waits
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

    /* an unopened `g` gives an item taken ahead or the depth memory or budget error */
    #[cold]
    #[inline(never)]
    fn unopened(&mut self, g: Val) -> Result<Outcome, VmErr> {
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get(g) && !c.pending.is_undef() { return Ok(Outcome::Yielded(self.take_pending(g))); }
        if self.heap.over() { return Err(crate::vm::cold_heap()); }
        if self.budget == 0 { return Err(VmErr::Budget); }
        Err(cold_depth())
    }

    /* raises `err` where `g` waits */
    fn throw_in(&mut self, g: Val, err: VmErr) -> Result<Outcome, VmErr> {
        self.resume_raise = Some(err);
        let r = self.step_coro(g);
        // an opened frame raises it as it enters
        if !matches!(r, Ok(Outcome::Opened)) { self.resume_raise = None; }
        r
    }

    /* closes `g` raising the close signal or `err` when closing its inner one failed */
    fn close_in(&mut self, g: Val, err: Option<VmErr>, kind: CoroKind, chunk: &SSAChunk) -> Result<Outcome, VmErr> {
        let (before, names) = (self.pending.exc_val, self.natives.names());
        let err = match err {
            Some(e) => e,
            None => { let exit = self.builtin_value(names.error(ErrKind::Close))?; self.raise_value(exit, chunk) }
        };
        match self.throw_in(g, err) {
            Ok(Outcome::Yielded(_)) => Err(EngineError::Generator(Gen::IgnoredClose(kind.body())).into()),
            // a body that returns on the close signal hands its value to `close`
            Ok(done) => Ok(done),
            Err(e) if raised_is(&e, names.error(ErrKind::Close)) => { self.pending.exc_val = before; Ok(Outcome::Returned(Val::none())) }
            Err(e) => Err(e),
        }
    }

    /* where the loop goes once a `Then` job's generator raised `e` or else `e` itself */
    #[cold]
    #[inline(never)]
    pub(crate) fn job_raised(&mut self, e: VmErr) -> Result<Result<usize, (usize, VmErr)>, VmErr> {
        let Some(crate::vm::jobs::Job::Gather(j)) = self.jobs.last() else { return Err(e) };
        if let crate::vm::jobs::Takes::Then(base) = j.takes && let Some(f) = self.top() {
            // a generator's record holds no dest so the value goes where its job names
            let to = j.to;
            self.finish_gen();
            return Ok(self.then_end(base as usize, Err(e), f.ret, to));
        }
        Err(e)
    }

    /* where the loop goes once a job's generator gave `r` at `ret` for `to` */
    #[inline(never)]
    pub(crate) fn job_out(&mut self, r: Result<Val, VmErr>, ret: usize, to: crate::vm::act::Dest) -> Result<usize, (usize, VmErr)> {
        match (self.then_out.take(), r) {
            (Some((base, step)), Ok(_)) => self.then_end(base as usize, step, ret, to),
            (_, Ok(r)) => Ok(self.deliver_to(r, ret, to)),
            (_, Err(e)) => Err((ret - 1, e)),
        }
    }

    /* a step of any other iterator through its protocol methods ending as a return */
    fn iter_send(&mut self, it: Val, sent: Sent, chunk: &SSAChunk) -> Result<Step, VmErr> {
        let (before, names) = (self.pending.exc_val, self.natives.names());
        let r = match sent {
            Sent::Value(v) if v.is_none() => self.next_of(it).and_then(|r| r.ok_or_else(|| VmErr::Of(ErrKind::Stop, String::new()))),
            Sent::Value(v) => self.try_call_dunder(it, names.send, &[v], chunk)?.ok_or_else(|| EngineError::NoAttribute { recv: it, name: String::from(names.send), store: false }.into()),
            Sent::Throw(e) => match self.try_call_dunder(it, names.throw, &[e], chunk)? {
                Some(v) => Ok(v),
                None => Err(self.with_roots([e], |vm| vm.raise_value(e, chunk))),
            },
            Sent::Close => { self.try_call_dunder(it, names.close, &[], chunk)?; return Ok(Step::Return(Val::none())); }
        };
        match r {
            Ok(v) => Ok(Step::Yield(v)),
            Err(e) if e.is(ErrKind::Stop, names) => Ok(Step::Return(self.swallow_stop(before))),
            Err(e) => Err(e),
        }
    }

    /* the method of user iterator it taking sent and the argument past it */
    fn sent_hook(&self, it: Val, sent: Sent) -> Option<(Val, Val)> {
        let names = self.natives.names();
        let (name, arg) = match sent { Sent::Value(v) if v.is_none() => return None, Sent::Value(v) => (names.send, v), Sent::Throw(e) => (names.throw, e), Sent::Close => (names.close, Val::undef()) };
        let (f, _) = self.instance_class(it).and_then(|c| self.lookup_class_member(c, name))?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })).then_some((f, arg))
    }

    /* the name of the exception class `e` is or is an instance of */
    fn exc_name_of(&self, e: Val) -> Option<&str> {
        self.heap.class_name(e).or_else(|| self.heap.instance_type(e))
    }

    /* the end of iteration carrying `v` as a generator or coroutine returned it */
    pub(crate) fn stop_iteration(&mut self, v: Val) -> VmErr {
        if v.is_none() { return VmErr::Of(ErrKind::Stop, String::new()); }
        match self.heap.engine_error(ErrKind::Stop, vec![v]) {
            Ok(exc) => { self.pending.exc_val = Some(exc); VmErr::Of(ErrKind::Stop, self.display(v)) }
            Err(e) => e,
        }
    }

    /* what `await x` steps which only a coroutine is */
    pub(crate) fn awaitable_of(&mut self, x: Val) -> Result<Val, VmErr> {
        if self.coro_kind(x) == Some(CoroKind::Coroutine) {
            if matches!(self.heap.get(x), HeapObj::Coroutine(c) if c.ip == FINISHED) { return Err(EngineError::Generator(Gen::Reused).into()); }
            return Ok(x);
        }
        Err(EngineError::cannot(Act::Await, Some(x)))
    }

    /* what throwing a value or a type and value raises checked against the root class */
    fn thrown(&mut self, args: &[Val], chunk: &SSAChunk) -> Result<Val, VmErr> {
        let exc = if args.len() > 1 && self.heap.is_classlike(args[0]) {
            let v = self.call_with(args[0], None, &args[1..2], &[], chunk)?;
            self.got(v)?
        } else { args[0] };
        let derives = if self.heap.is_classlike(exc) { self.exc_base(exc).is_some() } else { self.is_error(exc) };
        if !derives { return Err(EngineError::cannot(Act::Raise { throw: true }, Some(exc))); }
        Ok(exc)
    }

    /* the methods of a generator and a coroutine */
    pub(crate) fn coro_method(&mut self, g: Val, sent: crate::lang::Sent, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        use crate::vm::jobs::Takes;
        let (ok, before) = (core::mem::take(&mut self.job_ok), self.pending.exc_val.unwrap_or(Val::undef()));
        let sent = match sent {
            crate::lang::Sent::Value(v) => Sent::Value(v),
            crate::lang::Sent::Throw(t, v) if v.is_undef() => Sent::Throw(self.thrown(&[t], chunk)?),
            crate::lang::Sent::Throw(t, v) => Sent::Throw(self.thrown(&[t, v], chunk)?),
            crate::lang::Sent::Close => Sent::Close,
        };
        let close = matches!(sent, Sent::Close);
        // where frames may open a generator resumes as one with its job taking the step
        if ok && matches!(self.coro_kind(g), Some(CoroKind::Generator | CoroKind::Coroutine)) {
            self.coro_frame = Some((g, if close { Takes::Close } else { Takes::Next(Val::undef()) }, before));
            let r = self.with_roots([g], |vm| vm.gen_resume(g, sent, chunk));
            self.coro_frame = None;
            return match r? {
                Outcome::Opened => Ok(None),
                Outcome::Yielded(v) => Ok(Some(v)),
                Outcome::Returned(v) if close => Ok(Some(v)),
                Outcome::Returned(v) => Err(self.stop_iteration(v)),
            };
        }
        match self.with_roots([g], |vm| vm.gen_step(g, sent, chunk))? {
            Step::Yield(v) => Ok(Some(v)),
            Step::Return(v) if close => Ok(Some(v)),
            Step::Return(v) => Err(self.stop_iteration(v)),
        }
    }

    /* marks generator `g` finished so it never runs again */
    fn finish_generator(&mut self, g: Val) {
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) { c.ip = FINISHED; }
    }
}

/* whether `e` raises the runtime's class `name` */
pub(crate) fn raised_is(e: &VmErr, name: &str) -> bool {
    matches!(e, VmErr::Raised(m) if crate::lang::leads(m, name) && matches!(m.as_bytes().get(name.len()), None | Some(b':' | b'\n')))
}

/* which kind of suspendable body a coroutine object runs */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CoroKind { Generator, Coroutine }

impl CoroKind {
    /* the body an error names the kind by */
    fn body(self) -> Body {
        match self { Self::Generator => Body::Generator, Self::Coroutine => Body::Coroutine }
    }
}

/* what a body waits inside through a `yield from` or an `await` */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Via { YieldFrom, Await }

/* what a driver hands a suspended body to send or throw or close it */
#[derive(Clone, Copy, Debug)]
pub(crate) enum Sent { Value(Val), Throw(Val), Close }

/* what a delegating body does when its awaited frame ends resume or raise or close */
#[derive(Clone, Copy)]
pub(crate) enum Cont { After(Val, Via), Exit(Val, Via, Val), Close(Val, CoroKind) }

/* a wait a `Then` job's frame ends keeping the delegating call's takes and pending exception */
#[derive(Clone, Copy)]
pub(crate) struct Then { pub(crate) cont: Cont, takes: crate::vm::jobs::Takes, pub(crate) before: Val }

/* an awaited or delegated iterator's step as a value it yields up or ends with */
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step { Yield(Val), Return(Val) }

/* what resuming a body gave as a yield or a return */
#[derive(Clone, Copy, Debug)]
enum Outcome { Yielded(Val), Returned(Val), Opened }

impl VM<'_> {
    /* the name of the function a generator or coroutine runs */
    pub(crate) fn coro_attr(&self, g: Val, name: &str) -> Option<crate::vm::slow::attr::AttrLookup> {
        if !crate::lang::same_text(name, self.natives.names().name_field) { return None; }
        let HeapObj::Coroutine(c) = self.heap.get(g) else { return None };
        let BodyRef::Fn(fi) = c.body else { return None };
        Some(crate::vm::slow::attr::AttrLookup::Name(self.function_names.get(fi)?.clone()))
    }
}

/* a user iterator method a delegation calls as a job frame from `base` */
struct SentCall { it: Val, f: Val, arg: Val, close: bool, base: u32, before: Option<Val> }

impl crate::vm::jobs::Calls for SentCall {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<crate::vm::jobs::Call, VmErr> {
        let Some(r) = got else { return Ok(crate::vm::jobs::Call::Frame(self.f, [self.it, self.arg, Val::undef()])) };
        let step = match r {
            Ok(_) if self.close => Ok(Step::Return(Val::none())),
            Ok(v) => Ok(Step::Yield(v)),
            Err(e) if !self.close && e.is(ErrKind::Stop, vm.natives.names()) => Ok(Step::Return(vm.swallow_stop(self.before))),
            Err(e) => Err(e),
        };
        Ok(crate::vm::jobs::Call::Then(self.base, step))
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { f(self.it); f(self.f); f(self.arg); if let Some(b) = self.before { f(b) } }
}

/* the raw handler where section `s` of `code` lays the handler at compiler instruction `h` */
fn handler_in(s: &lower::Section, code: &Code, h: usize) -> Option<usize> {
    if h & RAW_HANDLER != 0 { return None; }
    // the handler one of the section's own `try`s names laid from that compiler instruction
    let (start, end) = (s.start as usize, (s.end as usize).min(code.ins.len()));
    code.ins[start..end].iter().filter(|i| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally) && i.x == 1).map(|i| i.a as usize)
        .find(|&e| (start..end).contains(&e) && s.orig.get(e - start).is_some_and(|&o| o as usize == h)).map(|e| e | RAW_HANDLER)
}

/* whether `s` adopts a generator holding no `excs` or notes stacks and lays every handler */
fn adoptable(s: &lower::Section, code: &Code, excs: &[ExceptionFrame]) -> bool {
    excs.is_empty() || s.held != u16::MAX && excs.iter().all(|f| handler_in(s, code, f.handler_ip).is_some())
}

/* how far up a section's register of noted depths packs its second depth */
const HELD_SHIFT: usize = 23;

impl<'a> VM<'a> {

    /* what a suspending frame hands out with its resume point noted */
    #[inline(never)]
    pub(crate) fn yield_out(&mut self, code: &Code, ip: usize) -> Val {
        // waits keep a none placeholder for what wakes them and parked helpers push nothing
        let event_yield = self.pending.event_wait_request;
        let sub_call_yield = !self.pending_sync_frames.is_empty();
        let host_yield = self.pending.host_call_request;
        let preempt_yield = self.pending.preempt_request;
        let regular_yield = !event_yield && !sub_call_yield && !host_yield && !preempt_yield;
        let next_is_pop = ip < code.ins.len() && code.ins[ip].op == OpCode::PopTop;
        // a register `yield` hands its register which takes what is sent unless its result drops
        let held = ip.checked_sub(1).and_then(|k| code.ins.get(k)).filter(|i| i.op == OpCode::Yield && i.x != 0).copied();
        let val = match held {
            _ if !regular_yield => Val::none(),
            Some(y) => {
                let at = self.base + y.a as usize;
                let v = self.regs[at];
                if y.x == 2 { self.regs[at] = Val::none(); }
                v
            }
            // code over rows yields its top row which takes what is sent back
            None => self.rows_yielded(next_is_pop),
        };
        let at = if regular_yield && next_is_pop && held.is_none() { ip + 1 } else { ip };
        let raw = parks_raw(code, at);
        (self.resume_ip, self.resume_layout) = if raw { (at | RESUME_RAW, code.layout) } else { (code.resume(at), 0) };
        // waiting lowered code puts its placeholder past its entry rows or in a raw take
        if (event_yield || host_yield) && !sub_call_yield && self.top == 0 && code.slots != 0 {
            let take = code.ins.get(ip).filter(|t| t.op == OpCode::StoreTopR).map(|t| u32::from(t.a));
            match (raw, take) {
                (false, _) => { let row = self.entry_top(code, self.resume_ip); self.hold_row(code, row); }
                (true, Some(row)) => self.hold_row(code, row),
                (true, None) => {}
            }
        }
        // never truncate `exception_stack` here as the resume saves it so a `try` survives the yield
        val
    }

    /* a register `yield` a builtin's job steps giving no code when its frame runs on */
    #[inline(never)]
    pub(crate) fn gather_yield(&mut self, ins: Ins, i: usize, code: &Code, floor: &Floor) -> Option<(CodePtr, ChunkPtr, usize)> {
        let v = self.regs[self.base + ins.a as usize];
        if v.is_undef() { return None; }
        // a value yield's row takes the none a builtin sends
        let slot = |vm: &mut Self| if ins.x == 2 { vm.regs[vm.base + ins.a as usize] = Val::none(); };
        if self.gather_fast(v) {
            slot(self);
            return Some((CodePtr::NONE, ChunkPtr::NONE, i + 1));
        }
        // `next` takes the value and ends with the generator suspended where it yielded
        if !matches!(self.jobs.last(), Some(super::jobs::Job::NextGen(..))) || self.budget == 0 { return None; }
        slot(self);
        let resume = ins.b as usize;
        let top = yield_top(code, ins, resume, self);
        self.gather_leave(v, (resume, top), floor.code, floor.chunk, floor.pool, floor.depth)
    }

    /* whether the builtin running the generator took register `a` its yield hands in place */
    #[inline(always)]
    pub(crate) fn gather_reg(&mut self, a: u16) -> bool {
        let v = reg(&self.regs, self.base + a as usize);
        !v.is_undef() && self.gather_fast(v)
    }

    /* `PopIter` marks and unmarks `a` section iterators by `x` or drops the loop's iterator */
    #[inline(never)]
    pub(crate) fn pop_iter(&mut self, ins: Ins) {
        match ins.x {
            // a section with a `try` notes where its stacks begin in registers from `a` on
            3 => {
                let at = self.base + ins.a as usize;
                self.regs[at] = Val::int((self.exception_stack.len() | self.unwind_stack.len() << HELD_SHIFT) as i64);
                self.regs[at + 1] = Val::int(self.handling.len() as i64);
            }
            2 if ins.c != 0 => self.hold(ins),
            // a return from its body drops what it stacked above where it began
            4 => {
                let at = self.base + ins.a as usize;
                let (e, handling) = (self.regs[at].as_int() as usize, self.regs[at + 1].as_int() as usize);
                let mask = (1 << HELD_SHIFT) - 1;
                self.exception_stack.truncate(e & mask);
                self.unwind_stack.truncate(e >> HELD_SHIFT);
                self.handling.truncate(handling);
            }
            2 => self.iter_stack.push(IterFrame::Under(ins.a as u32)),
            1 => match self.iter_stack.last() {
                Some(IterFrame::Under(_)) => { self.iter_stack.pop(); }
                Some(IterFrame::Held(..)) => if let Some(IterFrame::Held(_, mut h)) = self.iter_stack.pop() {
                    // what it held goes back in place as the loop body left the stacks unchanged
                    if !h.excs.is_empty() { self.exception_stack.extend_from_slice(&h.excs); h.excs.clear(); }
                    if !h.unwinds.is_empty() { self.unwind_stack.append(&mut h.unwinds); }
                    if !h.handling.is_empty() { self.handling.extend_from_slice(&h.handling); h.handling.clear(); }
                    self.held_pool.push(h);
                },
                _ => {}
            },
            _ => if let Some(IterFrame::Under(n) | IterFrame::Held(n, _)) = self.iter_stack.pop() { let n = self.iter_stack.len().saturating_sub(n as usize); self.iter_stack.truncate(n); },
        }
    }

    /* marks a generator section yielding in a `try` over its iterators keeping what it stacked */
    fn hold(&mut self, ins: Ins) {
        let at = self.base + ins.b as usize;
        let (e, handling) = (self.regs[at].as_int() as usize, self.regs[at + 1].as_int() as usize);
        let mask = (1 << HELD_SHIFT) - 1;
        let (exc, unwind) = (e & mask, e >> HELD_SHIFT);
        if self.exception_stack.len() <= exc && self.unwind_stack.len() <= unwind && self.handling.len() <= handling && ins.a == 1 { return; }
        let mut h = self.held_pool.pop().unwrap_or_default();
        if self.exception_stack.len() > exc { h.excs.extend_from_slice(&self.exception_stack[exc..]); self.exception_stack.truncate(exc); }
        if self.unwind_stack.len() > unwind { h.unwinds.extend(self.unwind_stack.drain(unwind..)); }
        if self.handling.len() > handling { h.handling.extend_from_slice(&self.handling[handling..]); self.handling.truncate(handling); }
        self.iter_stack.push(IterFrame::Held(ins.a as u32, h));
    }

    /* steps each generator section loop around `ip` as an ordinary loop over the run's generator */
    #[cold]
    #[inline(never)]
    pub(crate) fn enter_loops(&mut self, code: &Code, ip: usize) {
        let Some(block) = code.block.as_deref() else { return };
        for &(from, to, ..) in block.loops.iter() {
            let (from, to) = (from as usize, to as usize);
            if !(from < ip && ip < to) { continue; }
            // the move after the loop's step sends its step register back to that step
            if let Some(m) = code.ins.get(from + 1).filter(|m| m.op == OpCode::Move) { self.regs[self.base + m.a as usize] = self.regs[self.base + m.b as usize]; }
        }
    }

    /* where the generator moved into the loop at `ip` resumes or `None` to step it */
    #[cold]
    #[inline(never)]
    pub(crate) fn adopt_gen(&mut self, code: &Code, ip: usize) -> Option<usize> {
        let block = code.block.as_deref()?;
        let &(from, _, k, head) = block.loops.iter().find(|l| l.0 as usize == ip)?;
        let s = block.sections.get(k as usize)?;
        // where it resumes and its step returns past the yield with its state and loop
        let (taken, back, state, it, more, guards, co, pool) = {
                let &IterFrame::Coroutine(co) = self.iter_stack.last()? else { return None };
                let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return None };
                let BodyRef::Fn(fi) = c.body else { return None };
                if fi != s.fi || c.ip == crate::vm::coro::FINISHED || c.running || c.pinned() || !c.syncs.is_empty() || !c.levels.is_empty() || c.top != 0 || !adoptable(s, code, &c.excs) { return None; }
                let step = *s.steps.iter().find(|t| t.0 as usize == c.ip || t.1 as usize == c.ip)?;
                let (names, pool) = (self.functions[fi].1.names.len(), self.call_facts[fi].pool);
                let state: alloc::vec::Vec<Val> = match self.pools[pool].win(self.pools[pool].active as usize) {
                    Some(w) if w.owner.0 == co.0 && w.len >= names => self.regs[w.base..w.base + names].to_vec(),
                    _ if c.slots.len() >= names => c.slots[..names].to_vec(),
                    _ => return None,
                };
                let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
                // its first iterator takes the loop's place and the rest follow while none empties it
                let mut iters = core::mem::take(&mut c.iters);
                (c.ip, c.slots) = (crate::vm::coro::FINISHED, alloc::vec::Vec::new());
                let guards = core::mem::take(&mut c.excs);
                let it = (!iters.is_empty()).then(|| iters.remove(0));
                (step.2 as usize, step.3 as usize, state, it, iters, guards, co, pool)
        };
        // its function's other windowed generators give back their state to resume wherever they open next
        self.evict_pool(pool);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.slots = alloc::vec::Vec::new(); }
        let base = self.base;
        for (r, &v) in state.iter().enumerate() {
            if let Some(&to) = s.slots.get(r).filter(|&&t| t != u16::MAX) { self.regs[base + to as usize] = v; }
        }
        // its loops over other generators step them as ordinary iterators sending steps to the entry
        let (start, end) = (s.start as usize, (s.end as usize).min(code.ins.len()));
        for g in start..end {
            if code.ins[g].op == OpCode::GetIterR && code.ins[g].x == lower::ADOPT && let Some(mv) = code.ins.get(g + 2).filter(|m| m.op == OpCode::Move) {
                self.regs[base + mv.a as usize] = self.regs[base + mv.b as usize];
            }
        }
        let (store, step) = (code.ins[head as usize + 1], code.ins[from as usize + 3]);
        self.regs[base + s.link as usize] = Val::int((store.b as i64) << 16 | store.a as i64);
        // one yield place fixes where the step comes back else the next yield sets it
        let back = if s.steps.len() == 1 { back } else { taken };
        self.regs[base + step.a as usize] = Val::int((back as i64) << 16 | step.a as i64);
        // the generator's stacks begin where they stand with its handlers back on them
        if s.held != u16::MAX { self.pop_iter(Ins { op: OpCode::PopIter, x: 3, a: s.held, b: 0, c: 0 }); }
        let iter_base = self.iter_stack.len() - 1;
        match it {
            Some(it) => if let Some(last) = self.iter_stack.last_mut() { *last = it; },
            None => { self.iter_stack.pop(); }
        }
        self.iter_stack.extend(more);
        self.adopt_handlers(s, code, guards, iter_base);
        Some(taken)
    }

    /* puts back the handlers a generator taken into section `s` held as the section's own */
    fn adopt_handlers(&mut self, s: &lower::Section, code: &Code, excs: alloc::vec::Vec<ExceptionFrame>, iter_base: usize) {
        let level_base = self.levels.len();
        for f in excs {
            let Some(h) = handler_in(s, code, f.handler_ip) else { continue };
            self.exception_stack.push(ExceptionFrame { handler_ip: h, levels: f.levels + level_base, iter_depth: f.iter_depth + iter_base, turned: self.turned, ..f });
        }
    }

    /* where a section loop takes generator `obj` unstarted or at a `yield` else `None` */
    #[cold]
    #[inline(never)]
    pub(crate) fn take_gen(&mut self, code: &Code, obj: Val, rip: usize) -> Option<usize> {
        // the step right after it names the register the loop's variable takes
        let (ins, var) = (code.ins[rip], code.ins.get(rip + 1)?.b);
        let s = code.block.as_deref()?.sections.get(ins.c as usize)?;
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(obj) else { return None };
        let BodyRef::Fn(fi) = c.body else { return None };
        if fi != s.fi || c.ip == crate::vm::coro::FINISHED || c.running || c.pinned() || !c.syncs.is_empty() || !c.levels.is_empty() || c.top != 0 || !adoptable(s, code, &c.excs) { return None; }
        let at = match (c.ip, c.iters.len()) {
            (0, 0) => s.start as usize,
            (ip, 1) => s.steps.iter().find(|t| t.0 as usize == ip || t.1 as usize == ip)?.2 as usize,
            _ => return None,
        };
        let (names, pool) = (self.functions[fi].1.names.len(), self.call_facts[fi].pool);
        let state: alloc::vec::Vec<Val> = match self.pools[pool].win(self.pools[pool].active as usize) {
            Some(w) if w.owner.0 == obj.0 && w.len >= names && c.ip != 0 => self.regs[w.base..w.base + names].to_vec(),
            _ if c.slots.len() >= names => c.slots[..names].to_vec(),
            _ => return None,
        };
        let base = self.base;
        for (r, &v) in state.iter().enumerate() {
            if let Some(&to) = s.slots.get(r).filter(|&&t| t != u16::MAX) { self.regs[base + to as usize] = v; }
        }
        self.regs[base + s.link as usize] = Val::int((ins.a as i64) << 16 | var as i64);
        // one yield place fixes where the step comes back else its next `yield` sends it
        let back = if s.steps.len() == 1 { s.steps[0].3 as usize } else { at };
        self.regs[base + s.step_reg as usize] = Val::int((back as i64) << 16 | s.step_reg as i64);
        if s.held != u16::MAX { self.pop_iter(Ins { op: OpCode::PopIter, x: 3, a: s.held, b: 0, c: 0 }); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(obj) else { return None };
        let it = c.iters.pop();
        (c.ip, c.slots) = (crate::vm::coro::FINISHED, alloc::vec::Vec::new());
        let guards = core::mem::take(&mut c.excs);
        let iter_base = self.iter_stack.len();
        if let Some(it) = it { self.iter_stack.push(it); }
        self.adopt_handlers(s, code, guards, iter_base);
        Some(at)
    }

    /* whether the running call is a generator a `for` or a builtin steps */
    #[inline]
    pub(crate) fn gen_yields(&self) -> bool {
        let at = self.head_at(self.top_pool);
        (self.head(at).as_int() >> 40) & GEN != 0
    }

    /* `gen_yields` for a `for` and not for a task or a builtin's generator */
    #[inline(always)]
    pub(crate) fn gen_yields_to_for(&self) -> bool {
        let at = self.head_at(self.top_pool);
        (self.head(at).as_int() >> 40) & (GEN | TASK | JOB) == GEN
    }

    /* opens `co` as a frame for the `for` at `i` else `None` to resume nested */
    #[inline(never)]
    pub(crate) fn enter_gen(&mut self, co: Val, i: usize) -> Option<(CodePtr, ChunkPtr, usize)> {
        if self.too_deep() { return None; }
        let quiet = !self.heap.has_work();
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        // finished running and sectioned bodies resume nested and coroutines open only for their own step
        let BodyRef::Fn(fi) = c.body else { return None };
        if c.ip == FINISHED || c.running || c.pinned() || !c.syncs.is_empty() || !c.pending.is_undef() || !self.functions[fi].1.is_generator && !self.is_async_fn(fi) { return self.enter_rare(co, fi, i, quiet); }
        self.open_gen(co, fi, i, quiet, None)
    }

    /* `enter_gen` for generators parked in sections or helpers opening over their frame else `None` */
    #[cold]
    #[inline(never)]
    fn enter_rare(&mut self, co: Val, fi: usize, i: usize, quiet: bool) -> Option<(CodePtr, ChunkPtr, usize)> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        if c.ip == FINISHED || c.running || !c.pending.is_undef() || !c.pinned() && c.syncs.is_empty() { return None; }
        let (layout, ip) = (if c.pinned() { core::mem::take(&mut c.layout) } else { 0 }, c.ip);
        if layout != 0 {
            let code = self.code_laid(fi, layout)?;
            let (code, chunk, at) = self.open_gen(co, fi, i, quiet, Some(code))?;
            return Some((code, chunk, if ip & crate::vm::frame::RESUME_RAW != 0 { ip & !crate::vm::frame::RESUME_RAW } else { at }));
        }
        if self.chain_refused(co) { return None; }
        let opened = self.open_gen(co, fi, i, quiet, None)?;
        (self.chain.next, self.chain.coro) = (true, co);
        Some(opened)
    }

    /* `enter_gen` in `code` or the pool's with `quiet` when no native work awaits payment */
    #[inline(always)]
    pub(crate) fn open_gen(&mut self, co: Val, fi: usize, i: usize, quiet: bool, code: Option<CodePtr>) -> Option<(CodePtr, ChunkPtr, usize)> {
        let pool = self.call_facts[fi].pool;
        if code.is_none() && self.pools[pool].code.is_none() { self.gen_cold_code(fi); }
        if code.is_none() && self.pools[pool].cold { self.heat_gen(fi, pool); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        let gen_code = match code { Some(code) => code, None => CodePtr::of(self.pools[pool].code.as_ref()?) };
        let (level_base, iter_base, len, resume, top) = (self.levels.len(), self.iter_stack.len(), c.slots.len(), c.ip, c.top);
        // a generator still in its window with only its loop stacked runs there in place
        let level = self.pools[pool].active as usize;
        // the resume pays for its loop and handlers so the budget must cover them
        let cost = 1 + (c.iters.len() + c.excs.len()) as u64;
        if c.levels.is_empty() && c.excs.len() <= 4 && c.iters.len() <= 1 && self.budget > cost && quiet
            && self.pools[pool].win(level).is_some_and(|w| w.owner.0 == co.0) {
            c.running = true;
            self.budget -= cost;
            let cold = if gen_code.get().loops_only() && iter_base < NO_DEST as usize { FLAT } else {
                self.frame_bases.push(Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: iter_base });
                (self.frame_bases.len() - 1) as u32
            };
            move_last(&mut c.iters, &mut self.iter_stack);
            if !c.excs.is_empty() {
                for f in &c.excs { self.exception_stack.push(ExceptionFrame { levels: f.levels + level_base, iter_depth: f.iter_depth + iter_base, turned: self.turned, ..*f }); }
                c.excs.clear();
            }
            self.heap.set_budget(self.budget);
            // it keeps owning the window as no other run takes this level before it ends
            let p = &mut self.pools[pool];
            if let Some(w) = p.level_mut(level) { (p.window, p.wlen, p.wcode) = (w.base, w.len, w.code); }
            p.active += 1;
            return Some(self.run_gen(co, fi, pool, gen_code, cold, i, iter_base, (resume, top)));
        }
        let cost = 1 + (c.slots.len() + c.levels.len() + c.iters.len()) as u64;
        let cost = cost + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget = self.budget.saturating_sub(cost);
        self.heap.set_budget(self.budget);
        let kept = self.resumes_in(pool, level, co);
        let cold = self.cold_bases(gen_code.get().loops_only() && iter_base < NO_DEST as usize);
        let base = self.open_window(pool, len, self.functions[fi].1.names.len(), false);
        // a window no shorter than the saved registers keeps its laid constants
        if !kept && len > self.pools[pool].wlen {
            self.pools[pool].wcode = 0;
            self.note_window(pool);
        }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else {
            self.close_window(pool);
            if cold != FLAT { self.frame_bases.pop(); }
            return None;
        };
        // slots saved for other code move into this one at an equal point
        let moved = !kept && resume != 0 && resume & RESUME_RAW == 0 && c.layout & !SAVED != laid_for(gen_code.get());
        if !kept {
            self.regs[base..base + len].copy_from_slice(&c.slots);
            c.slots.clear();
        }
        // the levels its sections kept come back past those of the frames under it
        if !c.levels.is_empty() { self.levels.append(&mut c.levels); }
        match c.iters.len() {
            0 => {}
            1 => if let Some(f) = c.iters.pop() { self.iter_stack.push(f) },
            _ => self.iter_stack.append(&mut c.iters),
        }
        if !c.excs.is_empty() {
            for f in c.excs.iter_mut() {
                f.levels += level_base;
                f.iter_depth += iter_base;
            }
            self.exception_stack.append(&mut c.excs);
        }
        c.running = true;
        let gen_code = if moved { self.moved_code(co, fi, pool, gen_code, cold, (resume, top)) } else { gen_code };
        Some(self.run_gen(co, fi, pool, gen_code, cold, i, iter_base, (resume, top)))
    }

    /* the code `co` parked in other code resumes in with its window laid again */
    #[cold]
    #[inline(never)]
    fn moved_code(&mut self, co: Val, fi: usize, pool: usize, code: CodePtr, cold: u32, (resume, top): (usize, u16)) -> CodePtr {
        // the constants of the code it resumes in lie where its slots were
        self.pools[pool].wcode = 0;
        self.note_window(pool);
        let inner = !matches!(self.heap.try_get(co), Some(HeapObj::Coroutine(c)) if !c.syncs.is_empty());
        let (f, shape) = (self.functions[fi], self.pools[pool].shape);
        let chunk = &f.1;
        // the target takes a park it lays alike and one no other code takes
        if code.get().rows != 0 || takes_park(code.get(), resume, top, inner) || !super::vet::rows_hold(chunk, shape, resume, top, inner) { return code; }
        // else the compiler's own code takes its rows as they lie
        let first = self.compiler_code(chunk, pool);
        // a flat frame keeps no bases for code pushing handlers
        if cold == FLAT && !first.loops_only() { return code; }
        let k = CodePtr::of(&first);
        self.retired.push(first);
        k
    }

    /* heats the first code of generator function `fi` in `pool` lowering it once hot */
    #[cold]
    #[inline(never)]
    fn heat_gen(&mut self, fi: usize, pool: usize) {
        self.pools[pool].heat += 1;
        if self.pools[pool].heat >= lower::HOT_LOOP { let f = self.functions[fi]; self.lowered_code(&f.1, pool, true); }
    }

    /* `co` with window and state back becomes the running frame of the `for` at `i` */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn run_gen(&mut self, co: Val, fi: usize, pool: usize, gen_code: CodePtr, cold: u32, i: usize, iter_base: usize, (resume, top): (usize, u16)) -> (CodePtr, ChunkPtr, usize) {
        let at = self.head_at(pool);
        let flags = GEN | if cold != FLAT { BASES } else { 0 };
        let held = if cold == FLAT { iter_base as i64 } else { NO_DEST as i64 };
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        // the levels under its own and the top the frame below keeps
        let mark = self.mark();
        *record_mut::<HEAD>(&mut self.regs, at) = [Val::int(i as i64 | held << 24 | flags << 40), Val::int(below as i64 | (fi as i64) << 24), co, mark, gen_code.word()];
        (self.top_pool, self.acts) = (pool, self.acts + 1);
        // loop frames end through `yield_gen` or `finish_gen` even on error so need no `executing_coros` entry
        self.gen_frames += 1;
        self.run_in(pool);
        let c = gen_code.get();
        let fun = self.functions[fi];
        let chunk = ChunkPtr::of(&fun.1);
        // a park keeps its top and a fresh run starts at its first row
        self.top = if resume != 0 && top != 0 { u32::from(top) } else { u32::from(c.rows) };
        // a park holding rows lays them back while lowered code holding none resumes in place
        let ip = if resume != 0 && (c.rows != 0 || self.top != 0) { self.resume_at(&fun.1, c, pool, resume, None) } else {
            if !self.laid(c, pool) { self.grow_frame(&fun.1, c, pool); }
            c.lowered(resume)
        };
        (gen_code, chunk, ip)
    }

    /* ends the innermost generator frame yielding at `resume` into its object and gives its `for` */
    #[inline]
    pub(crate) fn yield_gen(&mut self, resume: usize) -> usize {
        let top = self.frame_top(resume);
        self.yield_gen_at(resume, top)
    }

    /* the top the running generator frame parks with at `resume` found from its code */
    #[cold]
    #[inline(never)]
    fn frame_top(&self, resume: usize) -> Option<u16> {
        if self.acts == 0 { return None; }
        // a frame's record holds code its pool keeps while the VM lives
        let code = CodePtr::from_word(self.head(self.head_at(self.top_pool) + 4)).get();
        Some(self.parked_top(code, resume) as u16)
    }

    /* `yield_gen` setting the top it parks with unless none */
    #[inline(always)]
    pub(crate) fn yield_gen_at(&mut self, resume: usize, top: Option<u16>) -> usize {
        if self.acts == 0 { return 0; }
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        let (link, co, w3) = (self.head(at).as_int(), self.head(at + 2), self.head(at + 3));
        let level_base = mark_of(w3);
        let (ret, cold) = ((link & 0xFF_FFFF) as usize, (link >> 40) & BASES != 0);
        let (iter_base, exc_base) = match cold {
            true => { let b = self.frame_bases[self.frame_bases.len() - 1]; (b.iter, b.exc) }
            false => (((link >> 24) & 0xFFFF) as usize, self.exception_stack.len()),
        };
        // registers stay in its window till another run takes it unless a collection returned them
        let level = self.pools[pool].active as usize - 1;
        let owned = self.pools[pool].win(level).is_some_and(|w| w.owner.0 == co.0);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            (c.ip, c.running) = (resume, false);
            if let Some(top) = top { c.top = top; }
            if !owned { c.slots.clear(); }
            if self.levels.len() > level_base { c.levels.extend(self.levels.drain(level_base..)); }
            match self.iter_stack.len().saturating_sub(iter_base) {
                0 => {}
                1 => move_last(&mut self.iter_stack, &mut c.iters),
                _ => c.iters.extend(self.iter_stack.drain(iter_base..)),
            }
            if self.exception_stack.len() > exc_base {
                for f in &self.exception_stack[exc_base..] { c.excs.push(ExceptionFrame { levels: f.levels.saturating_sub(level_base), iter_depth: f.iter_depth.saturating_sub(iter_base), ..*f }); }
                self.exception_stack.truncate(exc_base);
            }
        }
        // the frame below takes back its top
        (self.top_pool, self.acts, self.top) = ((self.head(at + 1).as_int() & 0xFF_FFFF) as usize, self.acts - 1, top_of(w3));
        if !owned { self.own(pool, level, co); }
        self.close_window(pool);
        if cold { self.frame_bases.pop(); }
        self.gen_frames -= 1;
        ret
    }

    /* opens a coroutine's helper frame of `fi` where `instance` is what a constructor call makes */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_helper(&mut self, fi: usize, func: Val, pool: usize, code: CodePtr, cold: u32, (ret, dest): (usize, u16), levels: usize, instance: Val) {
        let at = self.head_at(pool);
        let flags = if cold != FLAT { BASES } else { 0 } | if instance.is_undef() { 0 } else { CTOR };
        if !instance.is_undef() { self.instances.push(instance); }
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let top = self.top;
        *record_mut::<HEAD>(&mut self.regs, at) = [Val::int(ret as i64 | (dest as i64) << 24 | flags << 40), Val::int(below as i64 | (fi as i64) << 24), func, Val::int((levels as u64 | (top as u64) << 32) as i64), code.word()];
        (self.top_pool, self.acts) = (pool, self.acts + 1);
    }

    /* hands none to the place the wait of the parked generator `co` marked */
    pub(crate) fn wake_gen(&mut self, co: Val) {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return };
        let Some(top) = c.wait_slot().filter(|t| t.is_undef()) else { return };
        *top = Val::none();
    }

    /* where what wakes parked generator `co` goes in its innermost helper or own row */
    pub(crate) fn gen_wait(&mut self, co: Val) -> Option<&mut Val> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return None };
        if !c.syncs.is_empty() { return match self.heap.try_get_mut(co) { Some(HeapObj::Coroutine(c)) => c.wait_slot(), _ => None }; }
        let at = (c.top as usize).checked_sub(1)?;
        self.gen_slot(co, at)
    }

    /* moves what woke the `for` generator at `ip` from the top row to its wait */
    #[inline]
    pub(crate) fn woke_for(&mut self, code: &Code, ip: usize) {
        if !code.ins.get(ip).is_some_and(|i| matches!(i.op, OpCode::ForIter | OpCode::ForIterR)) { return; }
        let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() else { return };
        if !self.gen_wait(co).is_some_and(|v| v.is_undef()) { return; }
        let v = self.pop().unwrap_or_else(|_| Val::none());
        if let Some(slot) = self.gen_wait(co) { *slot = v; }
        // lowered code keeps no top past the row it took
        if code.rows == 0 { self.top = 0; }
    }

    /* parks the top generator and its helpers giving the `for` that steps it on resume */
    #[cold]
    #[inline(never)]
    pub(crate) fn park_gen(&mut self, floor: (usize, usize, CodePtr)) -> usize {
        let co = self.top().map_or(Val::undef(), |f| f.generator);
        let resume = core::mem::take(&mut self.resume_ip);
        // a frame's record holds code its pool keeps while the VM lives
        let code = (self.acts != 0).then(|| CodePtr::from_word(self.head(self.head_at(self.top_pool) + 4)).get());
        let top = code.map(|code| self.parked_top(code, resume) as u16);
        let at = self.yield_gen_at(resume, top);
        let helpers = core::mem::take(&mut self.pending_sync_frames);
        // a value wait marks a placeholder the next step fills from the `for`
        let placeholder = !self.pending.preempt_request;
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.syncs.extend(helpers.into_iter().rev()); }
        if placeholder {
            if let Some(slot) = self.gen_wait(co) { *slot = Val::undef(); }
            // the `for` keeps a row past its own for what wakes the wait
            let (depth, pool, floor_code) = floor;
            let stepping = if self.acts > depth { self.run_in(self.top_pool); CodePtr::from_word(self.head(self.head_at(self.top_pool) + 4)) } else { self.run_in(pool); floor_code };
            let stepping = stepping.get();
            if stepping.rows == 0 && self.top == 0 { self.hold_row(stepping, (stepping.slots as usize + stepping.entry_rows(stepping.resume(at))) as u32); } else { self.push(Val::none()); }
        }
        at
    }

    /* `yield` of the top generator frame handing `v` to the `for` that stepped it */
    #[inline(never)]
    pub(crate) fn yield_site(&mut self, v: Val, (resume, top): (usize, u16), floor_code: CodePtr, floor_chunk: ChunkPtr, floor_pool: usize, floor_depth: usize) -> Option<(CodePtr, ChunkPtr, usize)> {
        let at = self.yield_gen_at(resume, Some(top));
        let (code, chunk) = if self.acts > floor_depth {
            // the caller's record read in place for its code and its function's chunk
            let (pool, h) = (self.top_pool, self.head_at(self.top_pool));
            let (code, fi) = (CodePtr::from_word(self.head(h + 4)), (self.head(h + 1).as_int() >> 24) as usize);
            self.run_in(pool);
            (code, ChunkPtr::of(&self.functions[fi].1))
        } else {
            self.run_in(floor_pool);
            (floor_code, floor_chunk)
        };
        self.hand_to_for(code.get(), at, v);
        Some((code, chunk, at + 1))
    }

    /* `yield` of the top generator frame `next` runs with `v` as the call's result */
    #[inline(never)]
    pub(crate) fn gather_leave(&mut self, v: Val, (resume, top): (usize, u16), floor_code: CodePtr, floor_chunk: ChunkPtr, floor_pool: usize, floor_depth: usize) -> Option<(CodePtr, ChunkPtr, usize)> {
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        let at = self.yield_gen_at(resume, Some(top));
        let to = self.gather_to();
        self.jobs.pop();
        let (code, chunk) = if self.acts > floor_depth {
            let (pool, h) = (self.top_pool, self.head_at(self.top_pool));
            let (code, fi) = (CodePtr::from_word(self.head(h + 4)), (self.head(h + 1).as_int() >> 24) as usize);
            self.run_in(pool);
            (code, ChunkPtr::of(&self.functions[fi].1))
        } else {
            self.run_in(floor_pool);
            (floor_code, floor_chunk)
        };
        Some((code, chunk, self.deliver_to(v, at, to)))
    }

    /* the `for` at `at` of `code` takes `v` a generator yielded as its step would */
    #[inline]
    pub(crate) fn hand_to_for(&mut self, code: &Code, at: usize, v: Val) {
        // a generator's frame returns to the `for` that stepped it in its caller's code
        let step = *get_in(&code.ins, at);
        match step.op {
            // the caller's frame covers every register its code names
            OpCode::ForIterR if step.x == 0 => put(&mut self.regs, self.base + step.b as usize, v),
            OpCode::ForIterR => self.scopes[code.module].set_at(step.b as u32, v),
            _ => self.push(v),
        }
    }

    /* ends the innermost frame of a generator that returned or raised which then stays finished */
    pub(crate) fn finish_gen(&mut self) -> usize {
        let Some(f) = self.top() else { return 0 };
        let (co, ret) = (f.generator, f.ret);
        // a frame with no bases drops the loops it ended inside as `leave_frame` would
        if f.cold == FLAT { self.iter_stack.truncate(f.dest as usize); }
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.ip = FINISHED; }
        self.leave_gen();
        ret
    }

    /* drops the innermost generator frame and its running mark */
    fn leave_gen(&mut self) {
        let Some(f) = self.top() else { return };
        let (co, pool) = (f.generator, f.pool);
        let level = self.pools[pool].active as usize - 1;
        self.leave_frame();
        // a generator that ended holds nothing in the window it ran in
        if let Some(w) = self.pools[pool].level_mut(level) && w.owner.0 == co.0 {
            w.owner = Val::undef();
            self.owners -= 1;
        }
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.running = false; }
        self.gen_frames -= 1;
    }

    /* keeps the suspended innermost loop frame as a sync frame its coroutine reruns on resume */
    #[inline(never)]
    pub(crate) fn park_frame(&mut self) -> usize {
        let Some(f) = self.top() else { return 0 };
        let (fi, func, ret, pool, instance) = (f.fi, f.callee, f.ret, f.pool, f.instance);
        let (ip, layout) = (core::mem::take(&mut self.resume_ip), core::mem::take(&mut self.resume_layout));
        let code = f.code.get();
        let layout = if layout != 0 { layout } else { code.layout };
        let (mut levels, mut iter_delta, mut exception_delta) = (Vec::new(), Vec::new(), Vec::new());
        let b = self.bases_of(&f);
        self.save_frames(f.levels, b.iter, b.exc, &mut levels, &mut iter_delta, &mut exception_delta);
        let top = self.parked_top(code, ip) as u16;
        // a parked window keeps only the registers its resume reads
        let slots = self.window_prefix(pool, super::window::live_len(code, ip, top, usize::MAX));
        self.leave_frame();
        self.pending_sync_frames.push(ParkedFrame { ip, fi, func, slots, levels, iter_delta, exception_delta, instance, layout, top });
        ret
    }

    /* whether function `fi` is an `async def` */
    #[inline]
    pub(crate) fn is_async_fn(&self, fi: usize) -> bool { self.is_async.get(fi).copied().unwrap_or(false) }

    /* whether `v` is a coroutine which only `await` steps */
    pub(crate) fn is_async_object(&self, v: Val) -> bool {
        matches!(self.heap.try_get(v), Some(HeapObj::Coroutine(c)) if !matches!(c.body, BodyRef::Fn(fi) if !self.is_async_fn(fi)))
    }
}
