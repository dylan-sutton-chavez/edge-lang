use crate::s;
use alloc::{rc::Rc, string::{String, ToString}, vec::Vec};
use core::cell::RefCell;

use crate::parser::{OpCode, SSAChunk, ssa_strip};

use super::{ExceptionFrame, VM};
use super::types::*;
use super::cache::OpcodeCache;
use super::sites::Site;
use super::lower::{Code, Ins, STORE_NOTES_BUILTIN, STORE_VOIDS_CACHE};
use super::scope::Kind;
use super::opcodes::{attr_lookup::AttrLookup, function::{Args, Passed}};
use super::registers::{int_add, int_sub, int_mul, int_mod, int_div, int_floordiv, float_div, int_bits, numeric_binop};

/* The frame an instruction loop began with, which the frames it opens return to last. */
struct Floor {
    depth: usize,
    code: *const Code,
    chunk: *const SSAChunk,
    base: usize,
}

/* What a step of the loop leaves it to do with its frames. */
enum Turn {
    Enter,
    Lower,
    Yield(Val),
    Return(Val),
    Raise(VmErr),
}

/* Index `i` into `len` items, negatives from the end, None out of range. */
#[inline(always)]
fn index_of(i: i64, len: usize) -> Option<usize> {
    let j = if i < 0 { i + len as i64 } else { i };
    (0..len as i64).contains(&j).then_some(j as usize)
}

/* Whether `k` hashes without user code, an inline value or a str. */
#[inline(always)]
fn plain_key_in(heap: &HeapPool, k: Val) -> bool { !k.is_heap() && !k.is_undef() || matches!(heap.try_get(k), Some(HeapObj::Str(_))) }

/* The value a plain dict holds under a plain key, None on a miss so the slow path raises. */
#[inline(never)]
fn dict_get(heap: &HeapPool, d: &RefCell<DictMap>, k: Val) -> Option<Val> {
    if !plain_key_in(heap, k) { return None; }
    d.borrow().get(&k, heap).copied()
}

/* Stores under a plain key of a plain dict, false when the key needs the slow path. */
#[inline(never)]
fn dict_set(heap: &HeapPool, d: &RefCell<DictMap>, k: Val, v: Val) -> bool {
    if !plain_key_in(heap, k) { return false; }
    d.borrow_mut().insert(k, v, heap);
    true
}

/* Opcodes the register loop hands to `regs_aside`, done there when common. */
#[inline(always)]
fn aside_op(op: OpCode) -> bool {
    matches!(op, OpCode::PowR
        | OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR | OpCode::UnpackR
        | OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop
        | OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildString | OpCode::UnpackSequence | OpCode::FormatValue
        | OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd
        | OpCode::CallStr | OpCode::CallInt | OpCode::CallFloat | OpCode::CallBool | OpCode::CallAbs | OpCode::CallChr | OpCode::CallOrd | OpCode::CallLen
        | OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Mod | OpCode::FloorDiv | OpCode::InPlaceAdd | OpCode::InPlaceSub
        | OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::LtEq | OpCode::Gt | OpCode::GtEq)
}

/* Whether the register loop runs `op`, never under coverage so every ip is marked. */
#[inline(always)]
fn in_regs(op: OpCode) -> bool {
    !cfg!(feature = "coverage") && (op.is_register() || aside_op(op) || matches!(op, OpCode::Jump | OpCode::PopTop | OpCode::Call | OpCode::CallMethod))
}

/* What `v`'s items are when it is a tuple or list, `f` seeing them. */
#[inline(always)]
fn seq_items<R>(heap: &HeapPool, v: Val, f: impl FnOnce(&[Val]) -> R) -> Option<R> {
    match heap.try_get(v) {
        Some(HeapObj::Tuple(t)) => Some(f(t)),
        Some(HeapObj::List(l)) => Some(f(&l.borrow())),
        _ => None,
    }
}

impl<'a> VM<'a> {

    /* Runs a learned operator dunder, false with operands restored on a miss or `NotImplemented`. */
    #[inline]
    fn exec_dunder(&mut self, site: Site, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let Site::Dunder { class, func, owner, arity, epoch } = site else { return Ok(false) };
        let arity = arity as usize;
        let len = self.stack.len();
        if epoch != self.class_epoch || len < arity { return Ok(false); }
        let recv = self.stack[len - arity];
        if !matches!(self.heap.try_get(recv), Some(&HeapObj::Instance(c, _)) if c.0 == class.0) { return Ok(false); }
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let operands = Args::of(&self.stack[len - arity..]);
        self.stack.truncate(len - arity);
        self.pending.method_binding = Some((owner, recv));
        self.push(func);
        self.stack.extend_from_slice(&operands);
        self.exec_call(arity as u16, chunk)?;
        let result = self.pop()?;
        if self.heap.is_not_implemented(result) {
            // The slow handler sees its operands unchanged and tries the reflected dunder.
            self.stack.extend_from_slice(&operands);
            return Ok(false);
        }
        self.push(result);
        Ok(true)
    }

    /* Learns the operator dunder an instance resolved at `ip`, nothing for any other receiver. */
    pub(crate) fn record_dunder_hit(&self, ip: usize, cache: &OpcodeCache, recv: Val, name: &str, arity: u8) {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) else { return };
        let Some((func, owner)) = self.lookup_class_member(class, name) else { return };
        cache.set_site(ip, Site::Dunder { class, func, owner, arity, epoch: self.class_epoch });
    }

    /* Runs `chunk` on `frame` moved onto the register stack, and moves it back after. */
    pub(crate) fn exec(&mut self, chunk: &SSAChunk, frame: &mut Vec<Val>) -> Result<Val, VmErr> {
        let pool = self.pool_of(chunk);
        let base = self.regs.len();
        self.regs.append(frame);
        let result = self.exec_in(chunk, base, pool);
        frame.extend(self.regs.drain(base..));
        result
    }

    /* The index of `chunk`'s pool, made empty on its first run. */
    pub(crate) fn pool_of(&mut self, chunk: &SSAChunk) -> usize {
        let key = chunk as *const SSAChunk;
        if let Some(&p) = self.pool_ids.get(&key) { return p; }
        self.pools.push(Default::default());
        self.pool_ids.insert(key, self.pools.len() - 1);
        self.pools.len() - 1
    }

    /* Runs the frame at `base` of the register stack, its registers the top of it. */
    pub(crate) fn exec_in(&mut self, chunk: &SSAChunk, base: usize, pool: usize) -> Result<Val, VmErr> {
        // Each loop nests in Rust, so the host stack bounds how many run at once, whatever started them.
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let outer_base = core::mem::replace(&mut self.base, base);
        self.depth += 1;
        let result = self.exec_frame(chunk, pool);
        self.depth -= 1;
        self.base = outer_base;
        result
    }

    /* Runs the chunk's lowered code, register forms and stack opcodes alike, and each plain call it makes as a frame of this same loop. */
    fn exec_frame(&mut self, chunk: &SSAChunk, pool: usize) -> Result<Val, VmErr> {
        // `resume_coroutine` pre-pushes restored exception frames before calling us. Honor its override so dispatch's handler search includes them.
        let exc_base = self.pending_exec_exc_base.take().unwrap_or(self.exception_stack.len());
        let outer_safe = core::mem::replace(&mut self.frame_safe, core::mem::take(&mut self.pending_exec_safe));
        // Cleanup reasons belong to this frame's finally bodies, drop leftovers when it returns.
        let unwind_base = self.unwind_stack.len();
        // Drop spread-delta frames leaked by an aborted arg list on unwind.
        let delta_base = self.pending.delta_save.len();

        let (mut floor_code, mut once) = self.frame_code(chunk, pool);
        // Constants become values on the chunk's first run, strings in the live heap.
        if self.pools[pool].consts.is_none() {
            match super::cache::const_vals(chunk, &mut self.heap) {
                Ok(consts) => self.pools[pool].consts = Some(consts),
                Err(e) => { self.frame_safe = outer_safe; return Err(e); }
            }
        }
        if self.regs.len() - self.base < floor_code.frame { self.grow_frame(chunk, &floor_code, pool); }
        // The frames this loop opens sit above `floor`, and the running one is the innermost of them or the frame it began with.
        let (floor, floor_base, floor_chunk) = (self.frames.len(), self.base, chunk as *const SSAChunk);
        let (mut cur, mut at, mut now, mut exc, mut unwind, mut delta) = (Rc::as_ptr(&floor_code), floor_chunk, pool, exc_base, unwind_base, delta_base);
        // Takes the running frame from the top of the frames, or this loop's own once none is left.
        macro_rules! load {
            () => {
                match self.frames.last() {
                    Some(f) if self.frames.len() > floor => {
                        (cur, at, now, exc, unwind, delta) = (f.code, &self.functions[f.fi].1 as *const SSAChunk, self.fn_pool[f.fi], f.exc_base, f.unwind_base, f.delta_base);
                        self.base = f.base;
                    }
                    _ => {
                        (cur, at, now, exc, unwind, delta) = (Rc::as_ptr(&floor_code), floor_chunk, pool, exc_base, unwind_base, delta_base);
                        self.base = floor_base;
                    }
                }
            };
        }
        // SAFETY the running code is held by `floor_code` or by its frame, which outlive every use below.
        let mut ip = unsafe { &*cur }.lowered(self.resume_ip);
        self.resume_ip = 0;
        // One-shot raise at the coroutine's park point, a cancellation or an error delivered while parked.
        let mut raised = self.resume_raise.take();
        // The call a raise came back from, where it meets the frame that made it.
        let mut raised_at = None;
        // A callee that suspended suspends its caller too, at the instruction after the call.
        let mut parking = false;
        // Where the frames this loop opens return to last, made once since only a lowering changes it.
        let mut floor_at = Floor { depth: floor, code: Rc::as_ptr(&floor_code), chunk: floor_chunk, base: floor_base };
        let result = loop {
            let turn = {
                // SAFETY as above, and a frame's chunk outlives this loop, the first is its caller's and every callee lives as long as the VM.
                let (mut code, mut chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                let (mut exc_base, mut pool) = (exc, now);
                let mut cache = &code.cache;
                let mut n = code.ins.len();
                // What the turn before left to finish, a suspension to pass on or an error to raise here.
                let mut carried = if parking { Some(Ok(None)) } else { raised.take().map(Err) };
                loop {
                    let mut rip = ip;
                    let step = if let Some(step) = carried.take() {
                        if step.is_err() && let Some(r) = raised_at.take() { rip = r; }
                        step
                    } else {
                        // Only what the register loop runs enters it, and a call or return it ran in place can leave another frame running.
                        if code.ins.get(ip).is_some_and(|i| in_regs(i.op)) && self.run_frames(code, cache, chunk, &mut ip, &floor_at) {
                            load!();
                            // SAFETY as above.
                            (code, chunk) = unsafe { (&*cur, &*at) };
                            (cache, n, exc_base, pool) = (&code.cache, code.ins.len(), exc, now);
                        }
                        rip = ip;
                        // A call the loop made can fail or leave its callee parked.
                        if let Some(e) = self.reg_error.take() { Err(e) } else if self.yielded { Ok(None) } else if ip >= n { Ok(Some(Val::none())) } else {
                            let ins = code.ins[ip];
                            ip += 1;
                            #[cfg(feature = "coverage")]
                            { let at = code.orig(rip) as usize; self.executed.entry(chunk as *const _).or_insert_with(|| alloc::vec![0; chunk.instructions.len().div_ceil(64)])[at / 64] |= 1 << (at % 64); }
                            match ins.op {
                                // A return with no cleanup to run leaves at once.
                                OpCode::ReturnR if self.exception_stack.len() <= exc_base && !self.regs[self.base + ins.b as usize].is_undef() => Ok(Some(self.regs[self.base + ins.b as usize])),
                                // A plain call skips the fused-builtin checks, its operand is only counts.
                                OpCode::Call => self.call_stacked(ins.a, code.orig(rip), chunk).map(|()| None),
                                op if op.is_register() => self.exec_reg(ins, rip, chunk, cache, code, &mut ip, exc_base),
                                _ => self.dispatch(ins, rip, chunk, cache, code, pool, &mut ip, exc_base),
                            }
                        }
                    };
                    // An instruction pays for the native work it did, loop or not.
                    let step = if self.heap.has_work() && let Err(e) = self.pay_work() { Err(e) } else { step };
                    match step {
                        Ok(None) if !self.yielded => continue,
                        Ok(None) if core::mem::take(&mut self.entered) => { self.yielded = false; break Turn::Enter }
                        // A hot loop hands its frame over at the loop head.
                        Ok(None) if self.tier_up => { self.yielded = false; self.tier_up = false; break Turn::Lower }
                        Ok(None) => { parking = false; break Turn::Yield(self.yield_out(code, ip)) }
                        Ok(Some(v)) => {
                            self.exception_stack.truncate(exc_base);
                            self.unwind_stack.truncate(unwind);
                            break Turn::Return(v)
                        }
                        Err(e) => match self.catch(code, chunk, &mut ip, exc_base, delta, e, rip) {
                            Ok(()) => continue,
                            Err(e) => break Turn::Raise(e),
                        },
                    }
                }
            };
            match turn {
                Turn::Enter => {
                    if let Some(f) = self.frames.last_mut() { f.ret = ip; }
                    load!();
                    // SAFETY as above.
                    let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                    if self.regs.len() - self.base < code_ref.frame { self.grow_frame(chunk, code_ref, now); }
                    ip = code_ref.lowered(0);
                }
                Turn::Lower => {
                    // A hot loop left unlowered carries on in the lowered code from its head, in the frame running now.
                    // SAFETY as above.
                    let (chunk, pool): (&SSAChunk, usize) = (unsafe { &*at }, now);
                    let lowered = self.lowered_code(chunk, pool);
                    let in_frame = floor < self.frames.len();
                    match self.frames.last_mut() {
                        Some(f) if in_frame => f.code = Rc::as_ptr(&lowered),
                        _ => {
                            if once { self.pools[pool].heat += floor_code.hot.get(); }
                            (floor_code, once) = (lowered, false);
                            floor_at.code = Rc::as_ptr(&floor_code);
                        }
                    }
                    load!();
                    // SAFETY as above.
                    let code_ref: &Code = unsafe { &*cur };
                    if self.regs.len() - self.base < code_ref.frame { self.grow_frame(chunk, code_ref, now); }
                    ip = code_ref.lowered(ip);
                }
                Turn::Return(v) => {
                    if self.frames.len() == floor { break Ok(v); }
                    let (ret, _, v) = self.return_frame(v);
                    self.push(v);
                    ip = ret;
                    load!();
                }
                Turn::Yield(v) => {
                    if self.frames.len() == floor { break Ok(v); }
                    ip = self.park_frame();
                    load!();
                    parking = true;
                }
                Turn::Raise(e) => {
                    let Some(f) = self.frames.last().filter(|_| self.frames.len() > floor) else { break Err(e) };
                    let (fi, call_ip) = (f.fi, f.call_ip);
                    ip = f.ret;
                    self.leave_frame();
                    load!();
                    // SAFETY as above.
                    let (code_ref, caller): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                    self.note_frame(fi, call_ip, caller);
                    raised = Some(e);
                    raised_at = Some(call_ip.map_or(ip, |o| code_ref.lowered(o as usize)));
                }
            }
        };
        if once { self.pools[pool].heat += floor_code.hot.get(); }
        self.frame_safe = outer_safe;
        result
    }

    /* What a suspending frame hands out, its resume point noted. */
    fn yield_out(&mut self, code: &Code, ip: usize) -> Val {
        // Event yields keep the None placeholder (overwritten by `run_push_event` before resume). Sync sub-call yields pushed nothing, the helper's return lands on the stack when its frame completes, so don't pop and don't skip the next PopTop. Child-wait yields keep the placeholder (wake-loop overwrites it with the target's result). Host-call yields keep the placeholder (overwritten by `set_host_result_by_id`).
        let event_yield = self.pending.event_wait_request;
        let sub_call_yield = !self.pending_sync_frames.is_empty();
        let child_yield = self.pending.waiting_for_children.is_some();
        let host_yield = self.pending.host_call_request;
        let preempt_yield = self.pending.preempt_request;
        let regular_yield = !event_yield && !sub_call_yield && !child_yield && !host_yield && !preempt_yield;
        let val = if regular_yield { self.pop().unwrap_or(Val::none()) } else { Val::none() };
        let next_is_pop = ip < code.ins.len() && code.ins[ip].op == OpCode::PopTop;
        self.resume_ip = code.resume(if regular_yield && next_is_pop { ip + 1 } else { ip });
        // Value-position yield, leave the sent value (None for next()) for the consumer on resume.
        if regular_yield && !next_is_pop { self.push(Val::none()); }
        // DON'T truncate exception_stack here, frames pushed in this exec belong to active try/except blocks. The enclosing `resume_coroutine` drains them into the coroutine's saved state so `try` survives the yield.
        val
    }

    /* Jumps to this frame's handler for `e`, or hands `e` back when none. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn catch(&mut self, code: &Code, chunk: &SSAChunk, ip: &mut usize, exc_base: usize, delta_base: usize, e: VmErr, rip: usize) -> Result<(), VmErr> {
        // HostYield is a control-flow signal, not a Python exception, bypass try/except.
        if matches!(e, VmErr::HostYield(_)) { return Err(e); }
        // Cancellation runs `finally` bodies and skips `except`, propagating out when none remain.
        if self.cancelling {
            self.error_byte_pos = None;
            self.call_stack.clear();
            return match self.next_cleanup_handler(exc_base) {
                Some(h) => {
                    self.unwind_stack.push(Unwind::Reraise(e, None));
                    *ip = code.lowered(h);
                    Ok(())
                }
                None => Err(e),
            };
        }
        // Innermost frame wins, cleared below on swallow so later errors re-anchor.
        if self.error_byte_pos.is_none() {
            self.error_byte_pos = chunk.resolve(code.orig(rip));
        }
        if self.exception_stack.len() <= exc_base { return Err(e); }
        let frame = self.exception_stack.pop().unwrap();
        self.stack.truncate(frame.stack_depth);
        self.iter_stack.truncate(frame.iter_depth);
        self.with_stack.truncate(frame.with_depth);
        self.pending.pos_delta = 0;
        self.pending.kw_delta = 0;
        self.pending.delta_save.truncate(delta_base);
        // Drop partial traceback so a later error doesn't inherit stale frames.
        self.call_stack.clear();
        let msg = e.class_name();
        // Prefer the user-raised instance, synthesize one for native errors.
        let exc = if let Some(v) = self.pending.exc_val.take() {
            v
        } else {
            // A MemoryError reports the limit itself, so its objects skip the soft limit.
            let heap_err = matches!(e, VmErr::Heap);
            let alloc = |heap: &mut HeapPool, obj| if heap_err { heap.alloc_emergency(obj) } else { heap.alloc(obj) };
            let msg_val = alloc(&mut self.heap, HeapObj::Str(e.message()))?;
            alloc(&mut self.heap, HeapObj::ExcInstance(msg, alloc::vec![msg_val], Val::undef()))?
        };
        // Kept so raising the exception again reports where it was first raised.
        let at = self.error_byte_pos.take();
        // An exception rising while another is handled takes it as its context.
        if let Some(&(handled, handled_at)) = self.handling.last() {
            self.with_roots([exc], |vm| vm.link_context(exc, handled, handled_at))?;
        }
        // Drop reasons from finally bodies this exception unwinds past.
        self.unwind_stack.truncate(frame.unwind_depth);
        self.handling.truncate(frame.handling_depth);
        // A handler or a cleanup running for the exception handles it, so a bare `raise` re-raises it.
        self.handling.push((exc, at));
        match frame.kind {
            BlockKind::Except => self.push(exc),
            // finally/with run their cleanup, then re-raise via EndFinally.
            BlockKind::Finally => {
                self.pending.exc_val = Some(exc);
                self.unwind_stack.push(Unwind::Reraise(e, at));
            }
        }
        *ip = code.lowered(frame.handler_ip);
        Ok(())
    }

    pub(crate) fn exec_from(&mut self, chunk: &SSAChunk, slots: &mut Vec<Val>, start_ip: usize) -> Result<Val, VmErr> {
        self.resume_ip = start_ip;
        self.exec(chunk, slots)
    }

    /* Resolve the receiver's method and call directly, args come from CallMethodArgs. */
    #[allow(clippy::too_many_arguments)]
    fn exec_call_method(&mut self, attr_idx: u16, call_op: u16, rip: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let cache = &code.cache;
        let raw = call_op as usize;
        let num_kw = (raw >> 8) & 0xFF;
        let num_pos = raw & 0xFF;
        let total = num_pos + 2 * num_kw;

        let at = self.stack.len().checked_sub(total).ok_or_else(|| cold_runtime("stack underflow"))?;
        let obj = *self.stack.get(at.wrapping_sub(1)).ok_or_else(|| cold_runtime("stack underflow"))?;
        // Borrow, don't clone, `chunk` outlives every `&mut self` call below.
        let Some(name) = chunk.names.get(attr_idx as usize) else { return Err(VmErr::Runtime("CallMethod: bad name index")) };
        let site = cache.site(rip);
        // A known builtin method with a few positionals copies them once and takes no keyword slot.
        if num_kw == 0 && num_pos <= 3 && let Some(id) = self.site_builtin(site, obj) {
            let mut args = [Val::undef(); 3];
            args[..num_pos].copy_from_slice(&self.stack[at..at + num_pos]);
            self.stack.truncate(at - 1);
            return self.exec_bound_method(obj, id, &args[..num_pos], &[], chunk);
        }
        // Any call past a builtin one may run Python, whose frame points back here.
        self.pending.call_ip = Some(code.orig(rip));
        // A known method calls with the receiver slotted in as its first argument.
        if let Some((func, owner)) = self.site_method(site, obj, name) {
            self.pending.method_binding = Some((owner, obj));
            // The receiver and the arguments already lie in parameter order, so a plain method opens with no shift.
            if num_kw == 0 && self.open_in_loop(func, Passed::Stack { first: at - 1, to: at - 1 }, num_pos + 1)? { return Ok(()); }
            self.stack.insert(at - 1, func);
            self.pending_exec_safe = self.frame_safe;
            self.inline_call = true;
            let called = self.exec_call_n(num_pos + 1, num_kw, chunk);
            self.pending_exec_safe = false;
            return called;
        }
        if let Some(value) = self.site_get(site, obj) {
            self.stack[at - 1] = value;
            self.inline_call = true;
            return self.exec_call_n(num_pos, num_kw, chunk);
        }
        let (positional, kw_flat) = (Args::of(&self.stack[at..at + num_pos]), Args::of(&self.stack[at + num_pos..]));
        self.stack.truncate(at - 1);
        if let Some(id) = self.site_builtin(site, obj) { return self.exec_bound_method(obj, id, &positional, &kw_flat, chunk); }

        let lookup = match self.resolve_attr(obj, name) {
            Ok(l) => l,
            Err(VmErr::Attribute(msg)) => {
                //  if `__getattr__` resolves the name to a callable, invoke it with the positional args.
                if let Some(v) = self.try_getattr_fallback(obj, name, chunk)? {
                    return self.call_with(v, None, &positional, &kw_flat, chunk);
                }
                return Err(VmErr::Attribute(msg));
            }
            Err(other) => return Err(other),
        };
        match lookup {
            AttrLookup::ModuleAttr(callee) => {
                cache.set_site(rip, self.learn_get(obj, name));
                self.call_with(callee, None, &positional, &kw_flat, chunk)
            }
            AttrLookup::Builtin(name) => {
                let callee = self.builtin_value(name)?;
                self.call_with(callee, None, &positional, &kw_flat, chunk)
            }
            AttrLookup::ClassMember(callee)
            // An instance-attribute callable gets no `self`, only class-level functions bind.
            | AttrLookup::InstanceField(callee) => self.call_with(callee, None, &positional, &kw_flat, chunk),
            AttrLookup::InstanceMethod { recv, func, class } => {
                if recv.0 == obj.0 { cache.set_site(rip, self.learn_method(obj, name, func, class)); }
                // Prepend `self`, `super()` reads the binding off `pending`, and method bodies stage like plain calls.
                self.pending.method_binding = Some((class, recv));
                self.pending_exec_safe = self.frame_safe;
                self.inline_call = true;
                let called = self.call_with(func, Some(recv), &positional, &kw_flat, chunk);
                self.pending_exec_safe = false;
                called
            }
            AttrLookup::BuiltinMethod(id) => {
                cache.set_site(rip, self.learn_builtin(obj, id));
                self.exec_bound_method(obj, id, &positional, &kw_flat, chunk)
            }
            AttrLookup::BoundBuiltin(recv, id) => self.exec_bound_method(recv, id, &positional, &kw_flat, chunk),
            AttrLookup::UnboundMethod(id) => self.exec_unbound_method(id, &positional, &kw_flat, chunk),
            AttrLookup::ExcArgs(_) | AttrLookup::Name(_) | AttrLookup::TypeOf(_) => {
                // `e.args()` / `f.__name__()`, the value isn't callable, reports as missing attribute.
                let ty = self.type_name(obj);
                Err(VmErr::Attribute(s!("'", str ty, "' object has no attribute '", str &name, "'")))
            }
            AttrLookup::PropertyGet { recv, getter } => {
                // Materialise the value first, then call it with the user's args, `foo.prop(arg)` where `prop` returns a callable.
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.push(getter);
                self.push(recv);
                self.exec_call(1, chunk)?;
                let value = self.pop()?;
                self.call_with(value, None, &positional, &kw_flat, chunk)
            }
            AttrLookup::Thunk(f) => {
                // `X.__value__(...)` evaluates the value, then calls it.
                self.push(f);
                self.exec_call(0, chunk)?;
                let value = self.pop()?;
                self.call_with(value, None, &positional, &kw_flat, chunk)
            }
            AttrLookup::PropertySetterRef(prop) => {
                let v = self.heap.alloc(HeapObj::PropertySetter(prop))?;
                self.call_with(v, None, &positional, &kw_flat, chunk)
            }
        }
    }

    /* Runs register forms of one frame from `ip` until one needs its handler or a call or return changes the frame. */
    #[inline(always)]
    fn run_regs(&mut self, code: &Code, cache: &OpcodeCache, chunk: &SSAChunk, ip: &mut usize) {
        let n = code.ins.len();
        let mut i = *ip;
        let base = self.base;
        // The frame as a local slice, taken again only after a call that may grow the stack.
        let mut r = &mut self.regs[base..];
        macro_rules! arith {
            ($ins:ident, $int:expr, $float:expr) => {{
                let (x, y) = (r[$ins.b as usize], r[$ins.c as usize]);
                let v = if x.is_int() && y.is_int() { $int(x.as_int(), y.as_int()) }
                    else if x.is_float() && y.is_float() { $float(x.as_float(), y.as_float()) }
                    else { None };
                let Some(v) = v else { break };
                r[$ins.a as usize] = v;
                i += 1;
            }};
        }
        macro_rules! compare {
            ($ins:ident, $cmp:tt) => {{
                let (x, y) = (r[$ins.b as usize], r[$ins.c as usize]);
                let v = if x.is_int() && y.is_int() { x.as_int() $cmp y.as_int() }
                    else if x.is_float() && y.is_float() { x.as_float() $cmp y.as_float() }
                    else { break };
                r[$ins.a as usize] = Val::bool(v);
                i += 1;
            }};
        }
        // A taken branch costs one step of the budget.
        macro_rules! branch {
            ($ins:ident, $holds:expr) => {{
                if $holds { i += 1; }
                else if self.budget > 0 && ($ins.a as usize) <= n { self.budget -= 1; i = $ins.a as usize; }
                else { break; }
            }};
        }
        macro_rules! test {
            ($ins:ident, $cmp:tt) => {{
                let (x, y) = (r[$ins.b as usize], r[$ins.c as usize]);
                let holds = if x.is_int() && y.is_int() { x.as_int() $cmp y.as_int() }
                    else if x.is_float() && y.is_float() { x.as_float() $cmp y.as_float() }
                    else { break };
                branch!($ins, holds)
            }};
        }
        while let Some(&ins) = code.ins.get(i) {
            match ins.op {
                OpCode::AddR | OpCode::InPlaceAddR => arith!(ins, int_add, |a: f64, b: f64| Some(Val::float(a + b))),
                OpCode::SubR | OpCode::InPlaceSubR => arith!(ins, int_sub, |a: f64, b: f64| Some(Val::float(a - b))),
                OpCode::MulR => arith!(ins, int_mul, |a: f64, b: f64| Some(Val::float(a * b))),
                OpCode::DivR => arith!(ins, int_div, float_div),
                OpCode::ModR => arith!(ins, int_mod, |_, _| None),
                OpCode::FloorDivR => arith!(ins, int_floordiv, |_, _| None),
                // Integer bit operations stay in the loop, anything else leaves for the handler.
                OpCode::BitAndR | OpCode::BitOrR | OpCode::BitXorR | OpCode::ShlR | OpCode::ShrR => {
                    let (x, y) = (r[ins.b as usize], r[ins.c as usize]);
                    let v = if x.is_int() && y.is_int() { int_bits(ins.op, x.as_int(), y.as_int()) } else { None };
                    let Some(v) = v else { break };
                    r[ins.a as usize] = v;
                    i += 1;
                }
                OpCode::EqR => compare!(ins, ==),
                OpCode::NotEqR => compare!(ins, !=),
                OpCode::LtR => compare!(ins, <),
                OpCode::LtEqR => compare!(ins, <=),
                OpCode::GtR => compare!(ins, >),
                OpCode::GtEqR => compare!(ins, >=),
                OpCode::JumpUnlessEq => test!(ins, ==),
                OpCode::JumpUnlessNotEq => test!(ins, !=),
                OpCode::JumpUnlessLt => test!(ins, <),
                OpCode::JumpUnlessLtEq => test!(ins, <=),
                OpCode::JumpUnlessGt => test!(ins, >),
                OpCode::JumpUnlessGtEq => test!(ins, >=),
                // `x` set jumps on a true value instead.
                OpCode::JumpIfFalseR => {
                    let v = r[ins.b as usize];
                    let holds = if v.is_bool() { v.as_bool() } else if v.is_int() { v.as_int() != 0 } else { break };
                    branch!(ins, holds != (ins.x != 0))
                }
                OpCode::Move => {
                    let v = r[ins.b as usize];
                    if v.is_undef() { break; }
                    r[ins.a as usize] = v;
                    i += 1;
                }
                OpCode::PushRegs => {
                    let (a, b, c) = (r[ins.a as usize], r[ins.b as usize], r[ins.c as usize]);
                    match ins.x {
                        1 if !a.is_undef() => self.stack.push(a),
                        2 if !a.is_undef() && !b.is_undef() => self.stack.extend_from_slice(&[a, b]),
                        3 if !a.is_undef() && !b.is_undef() && !c.is_undef() => self.stack.extend_from_slice(&[a, b, c]),
                        _ => break,
                    }
                    i += 1;
                }
                OpCode::StoreTopR => {
                    let Some(v) = self.stack.pop() else { break };
                    if ins.x == 0 { r[ins.a as usize] = v; } else { self.scopes[code.module].set_at(ins.a as u32, v); }
                    i += 1;
                }
                OpCode::PopTop => {
                    if self.stack.pop().is_none() { break; }
                    i += 1;
                }
                OpCode::IsR | OpCode::IsNotR => {
                    let (x, y) = (r[ins.b as usize], r[ins.c as usize]);
                    if x.is_undef() || y.is_undef() { break; }
                    r[ins.a as usize] = Val::bool((x.0 == y.0) == (ins.op == OpCode::IsR));
                    i += 1;
                }
                OpCode::LoadGlobalR => {
                    let scope = &self.scopes[code.module];
                    let v = match scope.at(ins.b as u32) { v if v.is_undef() => scope.fallback(ins.b as u32), v => v };
                    if v.is_undef() { break; }
                    r[ins.a as usize] = v;
                    i += 1;
                }
                OpCode::StoreGlobalR if ins.x == 0 => {
                    let v = r[ins.b as usize];
                    if v.is_undef() { break; }
                    self.scopes[code.module].set_at(ins.a as u32, v);
                    i += 1;
                }
                OpCode::GetItemR => {
                    let (o, k) = (r[ins.b as usize], r[ins.c as usize]);
                    if !o.is_heap() || !k.is_int() { break; }
                    let hit = match self.heap.plain(o) {
                        Some(HeapObj::List(v)) => { let b = v.borrow(); index_of(k.as_int(), b.len()).map(|j| b[j]) }
                        Some(HeapObj::Tuple(v)) => index_of(k.as_int(), v.len()).map(|j| v[j]),
                        _ => None,
                    };
                    let Some(v) = hit else { break };
                    r[ins.a as usize] = v;
                    i += 1;
                }
                OpCode::StoreItemR => {
                    let (o, k) = (r[ins.a as usize], r[ins.b as usize]);
                    if !o.is_heap() || !k.is_int() { break; }
                    let Some(HeapObj::List(v)) = self.heap.plain(o) else { break };
                    let mut b = v.borrow_mut();
                    let Some(j) = index_of(k.as_int(), b.len()) else { break };
                    b[j] = r[ins.c as usize];
                    drop(b);
                    self.mark_impure();
                    r = &mut self.regs[base..];
                    i += 1;
                }
                // Ranges and lists step here, the rest and an ended loop in the handler.
                OpCode::ForIterR if self.budget > 0 && !self.heap.needs_gc() => {
                    let item = match self.iter_stack.last_mut() {
                        Some(IterFrame::Range { cur, end, step }) if *step > 0 && *cur < *end && (Val::INT_MIN..=Val::INT_MAX).contains(cur) => {
                            let v = *cur;
                            *cur = cur.checked_add(*step).unwrap_or(i64::MAX);
                            Val::int(v)
                        }
                        Some(IterFrame::List { rc, idx }) => match rc.borrow().get(*idx) {
                            Some(&v) => { *idx += 1; v }
                            None => break,
                        },
                        Some(IterFrame::Seq { items, idx }) => match items.get(*idx) {
                            Some(&v) => { *idx += 1; v }
                            None => break,
                        },
                        Some(IterFrame::Watched { items, idx, of, len }) if watched_len(&self.heap, *of) == *len => match items.get(*idx) {
                            Some(&v) => { *idx += 1; v }
                            None => break,
                        },
                        // A failure sticks, so the handler meets it again and raises it.
                        Some(f @ IterFrame::DictWalk { .. }) => match f.next_item(&mut self.heap) {
                            Ok(Some(v)) => v,
                            _ => break,
                        },
                        _ => break,
                    };
                    self.budget -= 1;
                    if ins.x == 0 { r[ins.b as usize] = item; } else { self.scopes[code.module].set_at(ins.b as u32, item); }
                    i += 1;
                }
                OpCode::GetAttrR => {
                    let (site, o) = (cache.site(i), r[ins.b as usize]);
                    let v = match self.site_get(site, o) {
                        Some(v) => v,
                        // A builtin type's method binds its receiver, the same bound object each time.
                        None => match self.site_builtin(site, o) {
                            Some(id) => match self.heap.alloc(HeapObj::BoundMethod(o, id)) { Ok(v) => v, Err(_) => break },
                            None => break,
                        },
                    };
                    r = &mut self.regs[base..];
                    r[ins.a as usize] = v;
                    i += 1;
                }
                OpCode::SetAttrR => {
                    let (o, v) = (r[ins.a as usize], r[ins.b as usize]);
                    if v.is_undef() || !self.site_store(cache.site(i), o, v) { break; }
                    r = &mut self.regs[base..];
                    i += 1;
                }
                // A back-edge costs two steps, and only its handler collects or preempts.
                OpCode::Jump if (ins.a as usize) <= n
                    && self.budget >= 2
                    && ((ins.a as usize) > i || (self.preempt_left == 0 && !self.heap.needs_gc() && !code.unchanged())) => {
                    self.budget -= 1 + ((ins.a as usize) <= i) as u64;
                    i = ins.a as usize;
                }
                OpCode::MinusR | OpCode::NotR => {
                    let x = r[ins.b as usize];
                    let v = match ins.op {
                        OpCode::MinusR if x.is_int() => Val::int_checked(-x.as_int()),
                        OpCode::MinusR if x.is_float() => Some(Val::float(-x.as_float())),
                        OpCode::NotR if x.is_bool() => Some(Val::bool(!x.as_bool())),
                        OpCode::NotR if x.is_int() => Some(Val::bool(x.as_int() == 0)),
                        OpCode::NotR if x.is_none() => Some(Val::bool(true)),
                        _ => None,
                    };
                    let Some(v) = v else { break };
                    r[ins.a as usize] = v;
                    i += 1;
                }
                // Probing a key, a length or a cell runs apart, keeping this loop small.
                OpCode::InR | OpCode::NotInR | OpCode::LenR | OpCode::LoadCellR | OpCode::StoreCellR => {
                    if !self.heap_fast(ins) { break; }
                    r = &mut self.regs[base..];
                    i += 1;
                }
                // Calls stay in this loop, leaving when one fails or parks its callee.
                OpCode::Call | OpCode::CallMethod | OpCode::CallR | OpCode::CallMethodR => match self.call_aside(ins, i, code, chunk) {
                    Some(next) => { i = next; if self.yielded { break; } r = &mut self.regs[base..]; }
                    None => break,
                },
                op if aside_op(op) => match self.regs_aside(ins, n, i) {
                    Some(next) => { i = next; r = &mut self.regs[base..]; }
                    None => break,
                },
                _ => break,
            }
        }
        *ip = i;
    }

    /* Runs register forms from `ip`, entering and leaving the frames plain calls open above `floor`, true when another frame runs after. */
    #[inline(never)]
    fn run_frames(&mut self, code: &Code, cache: &OpcodeCache, chunk: &SSAChunk, ip: &mut usize, floor: &Floor) -> bool {
        let (mut code, mut cache, mut chunk, mut switched) = (code, cache, chunk, false);
        loop {
            self.run_regs(code, cache, chunk, ip);
            let i = *ip;
            if self.entered && self.reg_error.is_none() {
                // A call it let in runs next, in the frame it opened.
                (self.entered, self.yielded) = (false, false);
                // A result the next instruction would store goes straight to its register.
                let dest = match code.ins.get(i) { Some(n) if n.op == OpCode::StoreTopR && n.x == 0 => n.a, _ => super::frame::NO_DEST };
                let (c, k) = self.regs_enter(i, dest);
                // SAFETY the frame just opened runs code its pool keeps, and its chunk lives as long as the VM.
                (code, chunk) = unsafe { (&*c, &*k) };
                *ip = code.lowered(0);
            } else if self.frames.len() > floor.depth && let Some(&ins) = code.ins.get(i) && matches!(ins.op, OpCode::ReturnR | OpCode::ReturnValue) && let Some((ret, c, k)) = self.regs_return(ins, floor) {
                // SAFETY the caller runs code its pool or the floor keeps, and every chunk outlives the run.
                (code, chunk) = unsafe { (&*c, &*k) };
                *ip = ret;
            } else {
                return switched;
            }
            cache = &code.cache;
            switched = true;
        }
    }

    /* Makes the frame a call from the register loop just opened the running one, its code and chunk. */
    #[inline(always)]
    fn regs_enter(&mut self, ret: usize, dest: u16) -> (*const Code, *const SSAChunk) {
        let Some(f) = self.frames.last_mut() else { return (core::ptr::null(), core::ptr::null()) };
        (f.ret, f.dest) = (ret, dest);
        let (code, chunk, pool) = (f.code, &self.functions[f.fi].1 as *const SSAChunk, self.fn_pool[f.fi]);
        self.base = f.base;
        // SAFETY the frame just pushed holds its code, and its chunk lives as long as the VM.
        let (c, k): (&Code, &SSAChunk) = unsafe { (&*code, &*chunk) };
        if self.regs.len() - self.base < c.frame { self.grow_frame(k, c, pool); }
        (code, chunk)
    }

    /* Ends the innermost frame on a plain return of a register or the stack top, where its caller carries on and the code and chunk it runs, None when the return needs its handler. */
    #[inline(always)]
    fn regs_return(&mut self, ins: Ins, floor: &Floor) -> Option<(usize, *const Code, *const SSAChunk)> {
        let f = self.frames.last()?;
        let v = match ins.op {
            OpCode::ReturnR => self.regs[f.base + ins.b as usize],
            _ if self.stack.len() > f.stack_base => self.stack[self.stack.len() - 1],
            _ => return None,
        };
        // A return under a handler runs its cleanup first, which the slow path does.
        if v.is_undef() || self.exception_stack.len() > f.exc_base { return None; }
        if ins.op == OpCode::ReturnValue { self.stack.pop(); }
        if self.unwind_stack.len() > f.unwind_base { let unwind = f.unwind_base; self.unwind_stack.truncate(unwind); }
        let (ret, dest, v) = self.return_frame(v);
        let (code, chunk) = match self.frames.last() {
            Some(f) if self.frames.len() > floor.depth => { self.base = f.base; (f.code, &self.functions[f.fi].1 as *const SSAChunk) }
            _ => { self.base = floor.base; (floor.code, floor.chunk) }
        };
        if dest == super::frame::NO_DEST { self.push(v); return Some((ret, code, chunk)); }
        self.regs[self.base + dest as usize] = v;
        Some((ret + 1, code, chunk))
    }

    /* A call from the register loop, its next index or None, the error kept. */
    #[inline(never)]
    fn call_aside(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let called = if ins.op == OpCode::CallR {
            self.pending.call_ip = Some(code.orig(i));
            let (r, n) = (&self.regs[self.base..], ins.x as usize);
            let vals = [r[ins.a as usize], r[ins.b as usize], r[ins.c as usize]];
            // An unbound operand leaves for the handler, which names it.
            if vals[..=n].iter().any(|v| v.is_undef()) { return None; }
            let passed = Passed::Regs([vals[1], vals[2], Val::undef()]);
            // The function this call opened last opens again unchecked, while its body may still run as a frame.
            if let Site::Call { callee, fi } = code.cache.site(i) && callee.0 == vals[0].0 && self.call_facts[fi as usize].in_loop {
                return self.open_frame(fi as usize, callee, &passed, n, 0).map(|()| i + 1).map_err(|e| self.reg_error = Some(e)).ok();
            }
            match self.open_in_loop(vals[0], passed, n) {
                Ok(true) => {
                    // Only a plain function without cells keeps its site, never a constructor, a memo or a closure.
                    if self.entered && let Some(f) = self.frames.last() && f.callee.0 == vals[0].0 && f.instance.is_undef() && f.memo.is_none()
                        && matches!(self.heap.try_get(vals[0]), Some(HeapObj::Func(_, _, captures, _)) if captures.is_empty()) {
                        code.cache.set_site(i, Site::Call { callee: vals[0], fi: f.fi as u32 });
                    }
                    Ok(())
                }
                // Any other callee takes them off the stack as a stack call would.
                Ok(false) => { self.stack.extend_from_slice(&vals[..=n]); self.call_general(n as u16, chunk) }
                Err(e) => Err(e),
            }.map(|()| i + 1)
        } else if ins.op == OpCode::Call {
            self.call_stacked(ins.a, code.orig(i), chunk).map(|()| i + 1)
        } else if ins.op == OpCode::CallMethodR {
            let (r, n) = (&self.regs[self.base..], ins.x as usize);
            let vals = [r[ins.b as usize], r[ins.c as usize]];
            if vals[..=n].iter().any(|v| v.is_undef()) { return None; }
            // A method the site knows opens straight from the registers, the receiver first.
            if let Some(name) = chunk.names.get(ins.a as usize) && let Some((func, owner)) = self.site_method(code.cache.site(i), vals[0], name) {
                (self.pending.method_binding, self.pending.call_ip) = (Some((owner, vals[0])), Some(code.orig(i)));
                match self.open_in_loop(func, Passed::Regs([vals[0], vals[1], Val::undef()]), n + 1) {
                    Ok(true) => return Some(i + 1),
                    Ok(false) => self.pending.method_binding = None,
                    Err(e) => { self.reg_error = Some(e); return None; }
                }
            }
            // Any other receiver and argument go where the method call of the stack finds them.
            self.stack.extend_from_slice(&vals[..=n]);
            self.exec_call_method(ins.a, n as u16, i, code, chunk).map(|()| i + 1)
        } else {
            // An unlowered pair steps over its second half.
            let next = i + 1 + (ins.x == 1) as usize;
            self.exec_call_method(ins.a, ins.b, i, code, chunk).map(|()| next)
        };
        called.map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* A call whose callee and `count` positionals are on the stack, made at compiler instruction `orig`, opening a plain function as a frame of the loop. */
    #[inline(always)]
    fn call_stacked(&mut self, count: u16, orig: u32, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.pending.call_ip = Some(orig);
        if self.call_in_loop(count)? { return Ok(()); }
        self.call_general(count, chunk)
    }

    /* The general call of a callee and `count` positionals on the stack, which may still run in the loop. */
    #[inline(always)]
    fn call_general(&mut self, count: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.pending_exec_safe = self.frame_safe;
        self.inline_call = true;
        let called = self.exec_call(count, chunk);
        self.pending_exec_safe = false;
        called
    }

    /* Register-loop opcodes kept apart from the hot ones, the next index or None. */
    #[inline(never)]
    fn regs_aside(&mut self, ins: Ins, n: usize, i: usize) -> Option<usize> {
        match ins.op {
            OpCode::PowR => self.regs[self.base + ins.a as usize] = numeric_binop(OpCode::Pow, self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize])?,
            // A stack value tested by plain truth, a short-circuit keeping it to jump.
            OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop => if self.stack_jump(ins, n)? { return Some(ins.a as usize) },
            OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR => if !self.accumulate(ins, self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return None },
            OpCode::UnpackR => {
                let fits = seq_items(&self.heap, *self.stack.last()?, |items| {
                    if items.len() != ins.x as usize { return false; }
                    for (&k, &v) in [ins.a, ins.b, ins.c].iter().zip(items) { self.regs[self.base + k as usize] = v; }
                    true
                });
                if fits != Some(true) { return None; }
                self.stack.pop();
            }
            // Common stack opcodes stay in the loop, leaving untouched when they miss.
            _ => if !self.stack_fast(ins) { return None },
        }
        Some(i + 1)
    }

    /* Whether a stack-tested branch jumps, None for user code or a spent budget. */
    #[inline]
    fn stack_jump(&mut self, ins: Ins, n: usize) -> Option<bool> {
        let t = self.plain_truth(*self.stack.last()?)?;
        let jump = t == (ins.op == OpCode::JumpIfTrueOrPop);
        if jump && !(self.budget > 0 && (ins.a as usize) <= n) { return None; }
        if !jump || ins.op == OpCode::JumpIfFalse { self.stack.pop(); }
        if jump { self.budget -= 1; }
        Some(jump)
    }

    /* A value's truth when no user code decides it. */
    #[inline(always)]
    fn plain_truth(&self, v: Val) -> Option<bool> {
        if v.is_bool() { return Some(v.as_bool()); }
        if v.is_int() { return Some(v.as_int() != 0); }
        if v.is_none() { return Some(false); }
        match self.heap.try_get(v) { Some(HeapObj::Str(s)) => Some(!s.is_empty()), _ => None }
    }

    /* A stack opcode's common case done in place, false and untouched on a miss. */
    #[inline]
    fn stack_fast(&mut self, ins: Ins) -> bool {
        let len = self.stack.len();
        let n = ins.a as usize;
        let v = match ins.op {
            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildString => {
                if len < n { return false; }
                let items = &self.stack[len - n..];
                let obj = match ins.op {
                    OpCode::BuildList => HeapObj::List(Rc::new(RefCell::new(items.to_vec()))),
                    OpCode::BuildTuple => HeapObj::Tuple(items.to_vec()),
                    _ => {
                        let mut s = String::new();
                        for &p in items {
                            match self.heap.try_get(p) {
                                Some(HeapObj::Str(part)) => s.push_str(part),
                                // A bare field leaves an inline value, rendered only here.
                                None if !p.is_undef() => s.push_str(&self.display(p)),
                                _ => return false,
                            }
                        }
                        HeapObj::Str(s)
                    }
                };
                let Ok(v) = self.heap.alloc(obj) else { return false };
                self.stack.truncate(len - n);
                v
            }
            OpCode::UnpackSequence => {
                let Some(&top) = self.stack.last() else { return false };
                let stack = &mut self.stack;
                return seq_items(&self.heap, top, |items| {
                    if items.len() != n { return false; }
                    stack.pop();
                    stack.extend(items.iter().rev());
                    true
                }) == Some(true);
            }
            OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd => {
                let arity = if ins.op == OpCode::MapAdd { 2 } else { 1 };
                if len < arity + 1 { return false; }
                let (key, value) = (self.stack[len - arity], self.stack[len - 1]);
                self.stack.truncate(len - arity);
                let form = match ins.op { OpCode::ListAppend => OpCode::ListAppendR, OpCode::SetAdd => OpCode::SetAddR, _ => OpCode::MapAddR };
                if self.accumulate(Ins { op: form, ..ins }, key, value) { return true; }
                self.stack.extend_from_slice(&[key, value][2 - arity..]);
                return false;
            }
            // Two numbers on the stack, as the register forms compute them.
            OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Mod | OpCode::FloorDiv | OpCode::InPlaceAdd | OpCode::InPlaceSub
            | OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::LtEq | OpCode::Gt | OpCode::GtEq => {
                if len < 2 { return false; }
                let Some(v) = numeric_binop(ins.op, self.stack[len - 2], self.stack[len - 1]) else { return false };
                self.stack.truncate(len - 2);
                v
            }
            OpCode::FormatValue | OpCode::CallStr | OpCode::CallInt | OpCode::CallFloat | OpCode::CallBool
            | OpCode::CallAbs | OpCode::CallChr | OpCode::CallOrd | OpCode::CallLen => {
                // A fused builtin takes one bare argument, a rebound name runs the binding.
                let unary = if ins.op == OpCode::FormatValue { n == 0 } else { n == 1 && !self.fused_rebound(ins.op) };
                let Some(&x) = self.stack.last().filter(|_| unary) else { return false };
                let Some(v) = self.unary_fast(ins.op, x) else { return false };
                self.stack.pop();
                v
            }
            _ => return false,
        };
        self.stack.push(v);
        true
    }

    /* Adds `key` and `value` to the stack-top accumulator, false when the handler must check the key. */
    fn accumulate(&mut self, ins: Ins, key: Val, value: Val) -> bool {
        let Some(&acc) = self.stack.last() else { return false };
        if value.is_undef() { return false; }
        let plain = self.plain_key(key);
        match (ins.op, self.heap.try_get(acc)) {
            (OpCode::ListAppendR, Some(HeapObj::List(rc))) => rc.borrow_mut().push(value),
            (OpCode::SetAddR, Some(HeapObj::Set(rc))) if plain => { rc.borrow_mut().insert(key, &self.heap); }
            (OpCode::MapAddR, Some(HeapObj::Dict(rc))) if plain => rc.borrow_mut().insert(key, value, &self.heap),
            _ => return false,
        }
        true
    }

    /* A one-argument builtin or bare format on a plain value, else None. */
    fn unary_fast(&mut self, op: OpCode, x: Val) -> Option<Val> {
        let text = |vm: &mut Self, x: Val| match vm.heap.try_get(x) {
            Some(HeapObj::Str(_)) => Some(x),
            None if !x.is_undef() => { let s = vm.display(x); vm.heap.alloc(HeapObj::Str(s)).ok() }
            _ => None,
        };
        match op {
            // A bare field keeps an inline value, the string that holds it renders it.
            OpCode::FormatValue if !x.is_heap() && !x.is_undef() => Some(x),
            OpCode::FormatValue | OpCode::CallStr => text(self, x),
            OpCode::CallInt if x.is_int() => Some(x),
            OpCode::CallInt if x.is_bool() => Some(Val::int(x.as_bool() as i64)),
            OpCode::CallInt if x.is_float() && x.as_float().is_finite() && x.as_float().abs() < Val::INT_MAX as f64 => Some(Val::int(x.as_float() as i64)),
            OpCode::CallFloat if x.is_float() => Some(x),
            OpCode::CallFloat if x.is_int() => Some(Val::float(x.as_int() as f64)),
            OpCode::CallBool => self.plain_truth(x).map(Val::bool),
            OpCode::CallAbs if x.is_int() => Val::int_checked(x.as_int().abs()),
            OpCode::CallAbs if x.is_float() => Some(Val::float(x.as_float().abs())),
            OpCode::CallChr if x.is_int() => {
                let c = u32::try_from(x.as_int()).ok().and_then(char::from_u32)?;
                self.heap.alloc(HeapObj::Str(c.into())).ok()
            }
            OpCode::CallOrd => match self.heap.try_get(x) {
                Some(HeapObj::Str(s)) => { let mut cs = s.chars(); let c = cs.next()?; cs.next().is_none().then(|| Val::int(c as i64)) }
                _ => None,
            },
            OpCode::CallLen => self.plain_len(x).map(|n| Val::int(n as i64)),
            _ => None,
        }
    }

    /* A key that is hashable by its kind alone, so a fast path skips the hashability check. */
    #[inline(always)]
    fn plain_key(&self, k: Val) -> bool { plain_key_in(&self.heap, k) }

    /* A builtin container's length or an ASCII string's, None when a dunder may decide. */
    #[inline(always)]
    fn plain_len(&self, v: Val) -> Option<usize> {
        Some(match self.heap.plain(v)? {
            HeapObj::List(v) => v.borrow().len(),
            HeapObj::Tuple(v) => v.len(),
            HeapObj::Dict(d) => d.borrow().len(),
            HeapObj::Set(s) => s.borrow().len(),
            HeapObj::Str(s) if s.is_ascii() => s.len(),
            _ => return None,
        })
    }

    /* The fast path for a subscript of a plain list, tuple or dict, false when it misses. */
    #[inline(never)]
    fn item_fast(&mut self, ins: Ins) -> bool {
        let r = &self.regs[self.base..];
        if ins.op == OpCode::GetItemR {
            let (o, k) = (r[ins.b as usize], r[ins.c as usize]);
            let hit = match self.heap.plain(o) {
                Some(HeapObj::List(v)) if k.is_int() => { let b = v.borrow(); index_of(k.as_int(), b.len()).map(|j| b[j]) }
                Some(HeapObj::Tuple(v)) if k.is_int() => index_of(k.as_int(), v.len()).map(|j| v[j]),
                Some(HeapObj::Dict(d)) => dict_get(&self.heap, d, k),
                _ => None,
            };
            let Some(v) = hit else { return false };
            self.regs[self.base + ins.a as usize] = v;
            return true;
        }
        let (o, k, v) = (r[ins.a as usize], r[ins.b as usize], r[ins.c as usize]);
        match self.heap.plain(o) {
            Some(HeapObj::List(l)) if k.is_int() => {
                let mut b = l.borrow_mut();
                let Some(j) = index_of(k.as_int(), b.len()) else { return false };
                b[j] = v;
            }
            Some(HeapObj::Dict(d)) => if !dict_set(&self.heap, d, k, v) { return false },
            _ => return false,
        }
        self.mark_impure();
        true
    }

    /* The fast path for a key, length or cell probe, false when it misses. */
    #[inline(never)]
    fn heap_fast(&mut self, ins: Ins) -> bool {
        match ins.op {
            OpCode::InR | OpCode::NotInR => {
                let (item, container) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                // A plain key probes a plain dict or set, anything else runs the protocol.
                if !self.plain_key(item) { return false; }
                let hit = match self.heap.try_get(container) {
                    Some(HeapObj::Set(rc)) => rc.borrow().contains(item, &self.heap),
                    Some(HeapObj::Dict(rc)) => rc.borrow().contains_key(&item, &self.heap),
                    Some(HeapObj::FrozenSet(s)) => s.contains(item, &self.heap),
                    _ => return false,
                };
                self.regs[self.base + ins.a as usize] = Val::bool(hit == (ins.op == OpCode::InR));
            }
            OpCode::LenR => {
                if self.builtins_rebound & (1 << NativeFnId::Len as u32) != 0 { return false; }
                let Some(n) = self.plain_len(self.regs[self.base + ins.b as usize]) else { return false };
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

    /* Register forms whose fast path missed, run as their stack opcode. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn exec_reg(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, cache: &OpcodeCache, code: &Code, ip: &mut usize, exc_base: usize) -> Result<Option<Val>, VmErr> {
        let n = code.ins.len();
        let op = ins.a;
        match ins.op {
            // A comparison keeps what a user dunder returned, which need not be a bool.
            OpCode::AddR | OpCode::InPlaceAddR | OpCode::SubR | OpCode::InPlaceSubR | OpCode::MulR | OpCode::DivR | OpCode::ModR | OpCode::FloorDivR | OpCode::PowR
            | OpCode::EqR | OpCode::NotEqR | OpCode::LtR | OpCode::LtEqR | OpCode::GtR | OpCode::GtEqR => self.reg_binop(ins, rip, cache, chunk)?,
            OpCode::JumpUnlessEq | OpCode::JumpUnlessNotEq | OpCode::JumpUnlessLt | OpCode::JumpUnlessLtEq | OpCode::JumpUnlessGt | OpCode::JumpUnlessGtEq
                => if !self.reg_test(ins, rip, cache, chunk)? { *ip = self.checked_jump(op as usize, n)?; },
            OpCode::JumpIfFalseR => if self.reg_truthy(chunk, ins.b)? == (ins.x != 0) { *ip = self.checked_jump(op as usize, n)?; },
            // A dict subscript the register loop left skips the generic protocol.
            OpCode::GetItemR | OpCode::StoreItemR if self.item_fast(ins) => {}
            OpCode::LenR | OpCode::MinusR | OpCode::NotR | OpCode::GetItemR | OpCode::InR | OpCode::NotInR | OpCode::IsR | OpCode::IsNotR
            | OpCode::BitAndR | OpCode::BitOrR | OpCode::BitXorR | OpCode::ShlR | OpCode::ShrR => {
                if ins.op == OpCode::LenR { self.pending.call_ip = Some(code.orig(rip)); }
                self.regs[self.base + ins.a as usize] = self.reg_stack(ins, rip, cache, chunk)?;
            }
            OpCode::UnpackR => {
                self.unpack_iterable(ins.x as usize, None, chunk)?;
                for &t in [ins.a, ins.b, ins.c].iter().take(ins.x as usize) { self.regs[self.base + t as usize] = self.pop()?; }
            }
            OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR => {
                let v = self.reg(chunk, ins.c)?;
                if ins.op == OpCode::MapAddR { let k = self.reg(chunk, ins.b)?; self.push(k); }
                self.push(v);
                let form = match ins.op { OpCode::ListAppendR => OpCode::ListAppend, OpCode::SetAddR => OpCode::SetAdd, _ => OpCode::MapAdd };
                self.handle_comprehension(form)?;
            }
            OpCode::Move => self.regs[self.base + ins.a as usize] = self.reg(chunk, ins.b)?,
            OpCode::PushRegs => self.push_regs(ins, chunk)?,
            OpCode::CallR => {
                self.push_regs(Ins { x: ins.x + 1, ..ins }, chunk)?;
                self.call_stacked(ins.x as u16, code.orig(rip), chunk)?;
            }
            OpCode::CallMethodR => {
                self.push_regs(Ins { x: ins.x + 1, a: ins.b, b: ins.c, ..ins }, chunk)?;
                self.exec_call_method(ins.a, ins.x as u16, rip, code, chunk)?;
            }
            OpCode::StoreItemR => {
                let (o, k, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?, self.reg(chunk, ins.c)?);
                self.stack.extend_from_slice(&[o, k, v]);
                self.mark_impure();
                self.store_item(chunk)?;
            }
            OpCode::GetAttrR => {
                let o = self.reg(chunk, ins.b)?;
                let Some(name) = chunk.names.get(ins.c as usize) else { return Err(VmErr::Runtime("LoadAttr: bad name index")) };
                self.load_attr(o, name, chunk)?;
                let v = self.pop()?;
                self.regs[self.base + ins.a as usize] = v;
                let site = match self.heap.try_get(v) {
                    Some(&HeapObj::BoundMethod(recv, id)) if recv.0 == o.0 => self.learn_builtin(o, id),
                    _ => self.learn_get(o, name),
                };
                cache.set_site(rip, site);
            }
            OpCode::SetAttrR => {
                let (o, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                self.store_attr_at(o, ins.c, v, rip, cache, chunk)?;
            }
            OpCode::LoadGlobalR => self.regs[self.base + ins.a as usize] = self.global_at(code.module, ins.b as u32)?,
            OpCode::StoreTopR => {
                let v = self.pop()?;
                if ins.x == 0 { self.regs[self.base + ins.a as usize] = v; } else { self.scopes[code.module].set_at(ins.a as u32, v); }
            }
            OpCode::StoreGlobalR => {
                let v = self.reg(chunk, ins.b)?;
                self.scopes[code.module].set_at(ins.a as u32, v);
                if ins.x & STORE_VOIDS_CACHE != 0 { self.templates.clear(); }
                if ins.x & STORE_NOTES_BUILTIN != 0 && let Some(id) = self.scopes[code.module].names_builtin(ins.a as u32) { self.rebind_builtin(id); }
            }
            OpCode::LoadCellR => {
                let v = self.deref(self.regs[self.base + ins.b as usize]);
                if v.is_undef() { return Err(self.unbound_err(chunk, ins.b as usize)); }
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::StoreCellR => {
                let v = self.reg(chunk, ins.b)?;
                if !self.set_cell(self.regs[self.base + ins.a as usize], v) { self.regs[self.base + ins.a as usize] = v; }
            }
            OpCode::ForIterR => match self.for_step(chunk)? {
                Some(item) if ins.x == 0 => self.regs[self.base + ins.b as usize] = item,
                Some(item) => self.scopes[code.module].set_at(ins.b as u32, item),
                None => {
                    if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                    *ip = op as usize;
                }
            },
            OpCode::ReturnR => {
                let result = self.reg(chunk, ins.b)?;
                if self.exception_stack.len() > exc_base
                    && let Some(h) = self.next_cleanup_handler(exc_base)
                {
                    self.unwind_stack.push(Unwind::Return(result));
                    *ip = code.lowered(h);
                    return Ok(None);
                }
                return Ok(Some(result));
            }
            _ => return Err(cold_runtime("not a register form")),
        }
        Ok(None)
    }

    /* Stack opcodes, apart from the loop so a frame's entry stays cheap. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn dispatch(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, cache: &OpcodeCache, code: &Code, pool: usize, ip: &mut usize, exc_base: usize) -> Result<Option<Val>, VmErr> {
        let n = code.ins.len();
        let op = ins.a;
        match ins.op {
            // Short-circuit jumps, instance `__bool__` / `__len__` may run via `truthy_op`.
            OpCode::JumpIfFalseOrPop => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                if !self.truthy_op(v, chunk)? { *ip = op as usize; }
                else { self.pop()?; }
            }
            OpCode::JumpIfTrueOrPop => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                if self.truthy_op(v, chunk)? { *ip = op as usize; }
                else { self.pop()?; }
            }

            // Hot opcodes.
            OpCode::LoadName => {
                let v = match code.kinds.get(op as usize) {
                    Some(&Kind::Global(g)) => self.global_at(code.module, g)?,
                    Some(Kind::Cell) => {
                        let v = self.deref(self.regs.get(self.base + op as usize).copied().unwrap_or(Val::undef()));
                        if v.is_undef() { return Err(self.unbound_err(chunk, op as usize)); }
                        v
                    }
                    // Malformed bytecode can carry an out-of-range slot, treat it as unbound.
                    _ => match self.regs.get(self.base + op as usize).copied().filter(|v| !v.is_undef()) {
                        Some(v) => v,
                        None => self.unbound(chunk, op)?,
                    },
                };
                self.push(v);
            }
            OpCode::StoreName => match code.kinds.get(op as usize) {
                Some(&Kind::Global(g)) => {
                    let v = self.pop()?;
                    self.scopes[code.module].set_at(g, v);
                    if let Some(id) = self.scopes[code.module].names_builtin(g) { self.rebind_builtin(id); }
                }
                Some(Kind::Cell) => {
                    let v = self.pop()?;
                    if !self.set_cell(self.regs[self.base + op as usize], v) { self.regs[self.base + op as usize] = v; }
                }
                _ => self.handle_store(op)?,
            },
            OpCode::LoadGlobal => {
                let v = match code.kinds.get(op as usize) {
                    Some(&Kind::Global(g)) => self.global_at(code.module, g)?,
                    _ => {
                        let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("LoadGlobal: name index out of bounds"))?;
                        self.scopes[code.module].get(name).or_else(|| self.global(name)).ok_or_else(|| VmErr::Name(name.clone()))?
                    }
                };
                self.push(v);
            }
            OpCode::StoreGlobal => {
                let v = self.pop()?;
                let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("StoreGlobal: name index out of bounds"))?;
                // A `global` store rebinds a name some cached result may have read.
                self.templates.clear();
                self.note_builtin_binding(name);
                self.scopes[code.module].set(name, v);
            }
            OpCode::Del => match code.kinds.get(op as usize) {
                Some(&Kind::Global(g)) => {
                    let name = ssa_strip(&chunk.names[op as usize]);
                    if self.scopes[code.module].at(g).is_undef() { return Err(VmErr::Name(name.into())); }
                    self.scopes[code.module].set_at(g, Val::undef());
                    // A cached result may have read it, and a builtin name falls back.
                    self.templates.clear();
                    self.note_builtin_binding(name);
                }
                Some(Kind::Cell) => {
                    let cell = self.regs[self.base + op as usize];
                    if self.deref(cell).is_undef() { return Err(self.unbound_err(chunk, op as usize)); }
                    self.set_cell(cell, Val::undef());
                }
                _ => self.handle_side(OpCode::Del, op, chunk)?,
            },
            OpCode::LoadConst => {
                // Constants are pre-materialised at exec entry, so this is a single bounds-checked index instead of a Value->Val conversion.
                let v = *self.pools[pool].consts.as_deref().and_then(|c| c.get(op as usize))
                    .ok_or_else(|| cold_runtime("constant index out of bounds"))?;
                self.push(v);
            }

            // Extracted to exec_arith_or_compare so VM::dispatch doesn't fuse the IC/deopt cycle into its own symbol.
            OpCode::Add | OpCode::Sub | OpCode::Mul
            | OpCode::Mod | OpCode::FloorDiv
            | OpCode::Eq | OpCode::Lt | OpCode::NotEq
            | OpCode::Gt | OpCode::LtEq | OpCode::GtEq
            | OpCode::Div | OpCode::Pow | OpCode::Minus | OpCode::Pos | OpCode::InPlaceAdd | OpCode::InPlaceSub => {
                self.exec_arith_or_compare(ins.op, op, rip, cache, chunk)?;
            }

            OpCode::Jump => {
                let target = self.checked_jump(op as usize, n)?;
                // Backward jumps are loop back-edges, charge them so `while` is bounded like `for`.
                if target <= rip {
                    self.charge_step()?;
                    if self.heap.needs_gc() { self.collect_point()?; }
                    // Back-edges are the only preempt sampling point.
                    if self.preempt_left != 0 {
                        self.preempt_left -= 1;
                        if self.preempt_left == 0 {
                            if self.frame_safe {
                                self.preempt_left = self.preempt_every;
                                self.pending.preempt_request = true;
                                self.yielded = true;
                            } else {
                                // Unpreemptible here, retry next back-edge.
                                self.preempt_left = 1;
                            }
                        }
                    }
                    #[cfg(not(feature = "coverage"))]
                    if code.unchanged() && !self.yielded {
                        code.hot.set(code.hot.get() + 1);
                        if code.hot.get() == super::lower::HOT_LOOP { self.tier_up = true; self.yielded = true; }
                    }
                }
                *ip = target;
            }
            OpCode::JumpIfFalse => {
                let v = self.pop()?;
                if !self.truthy_op(v, chunk)? { *ip = self.checked_jump(op as usize, n)?; }
            }
            OpCode::ForIter => self.exec_for_iter(op, ip, n, chunk)?,
            OpCode::PopTop => { self.pop()?; }
            OpCode::ReturnValue => {
                let result = if self.stack.is_empty() { Val::none() } else { self.pop()? };
                // Run any enclosing finally/with cleanup before the value leaves the frame.
                if self.exception_stack.len() > exc_base
                    && let Some(h) = self.next_cleanup_handler(exc_base)
                {
                    self.unwind_stack.push(Unwind::Return(result));
                    *ip = code.lowered(h);
                    return Ok(None);
                }
                return Ok(Some(result));
            }

            // Warm opcodes.
            OpCode::GetItem => self.get_item_op(rip, chunk, cache)?,

            OpCode::CallSpread | OpCode::CallPrint | OpCode::CallLen | OpCode::CallAbs
            | OpCode::CallStr | OpCode::CallInt | OpCode::CallFloat | OpCode::CallBool
            | OpCode::CallType | OpCode::CallChr | OpCode::CallOrd
            | OpCode::CallList | OpCode::CallTuple | OpCode::CallEnumerate | OpCode::CallIsInstance
            | OpCode::CallRange | OpCode::CallRound | OpCode::CallMin | OpCode::CallMax
            | OpCode::CallSum | OpCode::CallZip | OpCode::CallDict | OpCode::CallSet
            | OpCode::CallInput | OpCode::MakeFunction | OpCode::MakeCoroutine
            | OpCode::CallAll | OpCode::CallAny | OpCode::CallBin | OpCode::CallOct
            | OpCode::CallHex | OpCode::CallDivmod | OpCode::CallPow | OpCode::CallRepr
            | OpCode::CallReversed | OpCode::CallCallable
            | OpCode::CallExtern => {
                // Snapshot call-site byte_pos for the new CallFrame, falls back to enclosing stmt.
                self.pending.call_ip = Some(code.orig(rip));
                // Only plain user calls stage frames.
                self.pending_exec_safe = self.frame_safe && matches!(ins.op, OpCode::Call | OpCode::CallSpread);
                let dispatched = self.handle_function(ins.op, op, chunk);
                // Cleared so `true` cannot leak onward.
                self.pending_exec_safe = false;
                dispatched?;
            }

            OpCode::GetIter if op == crate::parser::ITER_VALUE => {
                let o = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                // A list reads live and the others cannot change, so each iterates the same handed over as is.
                if !matches!(self.heap.plain(o), Some(HeapObj::List(_) | HeapObj::Range(..) | HeapObj::Tuple(_) | HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::FrozenSet(_))) {
                    let it = self.iter_of(o, chunk)?;
                    if let Some(top) = self.stack.last_mut() { *top = it; }
                }
            }
            OpCode::GetIter => {
                let obj = self.pop()?;
                let user = op == crate::parser::ITER_AS_IS && matches!(self.heap.try_get(obj), Some(HeapObj::Instance(..)));
                let frame = if user { IterFrame::UserDefined(obj) } else { self.make_iter_frame(obj, chunk)? };
                self.iter_stack.push(frame);
            }
            OpCode::LoadTrue => self.push(Val::bool(true)),
            OpCode::LoadFalse => self.push(Val::bool(false)),
            OpCode::LoadNone => self.push(Val::none()),
            OpCode::Not => self.handle_logic(OpCode::Not, chunk)?,

            OpCode::Phi => {
                // The phi tables index the compiler instructions, not the lowered ones.
                self.exec_phi(op, code.orig(rip) as usize, &chunk.phi_map, &chunk.phi_sources);
            }

            OpCode::LoadAttr => {
                let obj = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                match self.site_get(cache.site(rip), obj) {
                    Some(v) => { self.pop()?; self.push(v); }
                    None => {
                        self.handle_load_attr(op, chunk)?;
                        if let Some(name) = chunk.names.get(op as usize) { cache.set_site(rip, self.learn_get(obj, name)); }
                    }
                }
            }

            // Fused method call, the call's counts in the second operand.
            OpCode::CallMethod => {
                if ins.x == 1 { *ip += 1; }
                self.exec_call_method(op, ins.b, rip, code, chunk)?
            }
            OpCode::CallMethodArgs => {
                // Always folded into CallMethod, reaching here is a bytecode bug.
                return Err(cold_runtime("CallMethodArgs reached dispatch unpaired"));
            }

            // Cold opcodes.
            OpCode::And | OpCode::Or => {
                // Parser should short-circuit these via JumpIf*OrPop, reaching here is a codegen bug.
                return Err(cold_runtime("And/Or reached VM dispatch (should be short-circuited)"));
            }

            OpCode::MakeClass => self.exec_make_class(op, code.orig(rip) as usize + 1, chunk)?,
            OpCode::StoreAttr => {
                let value = self.pop()?;
                let obj = self.pop()?;
                self.store_attr_at(obj, op, value, rip, cache, chunk)?;
            }

            OpCode::LoadModule => {
                let entry = chunk.imports.get(op as usize).ok_or_else(|| cold_runtime("LoadModule: import index out of range"))?;
                let v = *self.module_table.get(&entry.spec).ok_or_else(|| cold_runtime("LoadModule: module not initialised"))?;
                self.push(v);
            }

            other => return self.dispatch_generic(other, op, chunk, code, ip, exc_base),
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_generic(&mut self, opcode: OpCode, operand: u16, chunk: &SSAChunk, code: &Code, ip: &mut usize, exc_base: usize) -> Result<Option<Val>, VmErr> {
        match opcode {
            OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor
            | OpCode::BitNot | OpCode::Shl | OpCode::Shr
            | OpCode::InPlaceBitOr | OpCode::InPlaceBitAnd | OpCode::InPlaceBitXor => self.handle_bitwise(opcode, operand, chunk)?,
            OpCode::MatMul => self.handle_matmul(operand, chunk)?,
            OpCode::MakeTypeAlias => {
                let value = self.pop()?;
                let name = chunk.names.get(operand as usize).ok_or_else(|| cold_runtime("MakeTypeAlias: bad name index"))?.clone();
                let alias = self.heap.alloc(HeapObj::TypeAlias(name, value))?;
                self.push(alias);
            }
            OpCode::MakeTypeVar => {
                let name = chunk.names.get(operand as usize).ok_or_else(|| cold_runtime("MakeTypeVar: bad name index"))?.clone();
                let v = self.heap.alloc(HeapObj::TypeVar(name))?;
                self.push(v);
            }
            OpCode::In | OpCode::NotIn | OpCode::Is | OpCode::IsNot => self.handle_identity(opcode, chunk)?,

            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildDict
            | OpCode::BuildString | OpCode::BuildSet | OpCode::BuildSlice => self.handle_build(opcode, operand)?,

            OpCode::StoreItem => { self.mark_impure(); self.store_item(chunk)?; }
            OpCode::DelItem => { self.mark_impure(); self.del_item(chunk)?; }
            OpCode::DelAttr => { self.mark_impure(); self.exec_del_attr(operand, chunk)?; }
            OpCode::UnpackSequence | OpCode::UnpackEx | OpCode::FormatValue => self.handle_container(opcode, operand, chunk)?,

            OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd => self.handle_comprehension(opcode)?,
            OpCode::DictUpdate | OpCode::SetUpdate | OpCode::ListExtend => self.handle_spread_merge(opcode, chunk)?,

            // The yielded value stays on the stack for the resumer.
            OpCode::Yield => self.yielded = true,
            OpCode::LoadYieldFrom => self.push(self.yield_from_value),
            OpCode::LoadEllipsis => {
                let v = self.heap.alloc(HeapObj::Ellipsis)?;
                self.push(v);
            }
            OpCode::Dup => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                self.push(v);
            }
            OpCode::MatchSeq => {
                // Sequence patterns match only list/tuple, not str/bytes.
                let v = self.pop()?;
                let is_seq = v.is_heap() && matches!(self.heap.get(v), HeapObj::List(_) | HeapObj::Tuple(_));
                self.push(Val::bool(is_seq));
            }
            OpCode::MatchClass => self.match_class(operand as usize, chunk)?,
            OpCode::MatchMap => {
                let v = self.pop()?;
                let is_map = v.is_heap() && matches!(self.heap.get(v), HeapObj::Dict(_));
                self.push(Val::bool(is_map));
            }
            OpCode::Dup2 => {
                let b = self.pop()?; let a = self.pop()?;
                self.push(a); self.push(b); self.push(a); self.push(b);
            }
            OpCode::Swap => {
                let b = self.pop()?; let a = self.pop()?;
                self.push(b); self.push(a);
            }
            OpCode::Rot3 => {
                let c = self.pop()?; let b = self.pop()?; let a = self.pop()?;
                self.push(b); self.push(c); self.push(a);
            }
            OpCode::Assert | OpCode::Del | OpCode::Global | OpCode::Nonlocal
            | OpCode::Raise | OpCode::RaiseFrom | OpCode::Await => {
                self.handle_side(opcode, operand, chunk)?;
            }
            // A finally frame runs on every exit path, its handler the finally body or WithExit.
            OpCode::SetupExcept | OpCode::SetupFinally => {
                self.exception_stack.push(ExceptionFrame {
                    kind: if opcode == OpCode::SetupExcept { BlockKind::Except } else { BlockKind::Finally },
                    handler_ip: operand as usize,
                    stack_depth: self.stack.len(),
                    iter_depth: self.iter_stack.len(),
                    with_depth: self.with_stack.len(),
                    unwind_depth: self.unwind_stack.len(),
                    handling_depth: self.handling.len(),
                });
            }
            OpCode::WithEnter => {
                let cm = self.pop()?;
                // Both dunders resolve up front, like Python.
                let (enter_fn, class) = self.with_dunder(cm, "__enter__")?;
                self.with_dunder(cm, "__exit__")?;
                self.with_stack.push(cm);
                self.pending.method_binding = Some((class, cm));
                self.push(enter_fn);
                self.push(cm);
            }
            // Marks a normal fall-through into a finally body so EndFinally balances its pop.
            OpCode::BeginFinally => self.unwind_stack.push(Unwind::Normal),
            // Stages `__exit__` for the plain Call behind it, parking-safe.
            OpCode::WithExit => {
                let cm = self.with_stack.pop().ok_or_else(|| cold_runtime("WithExit without matching WithEnter"))?;
                let (exit_fn, class) = self.with_dunder(cm, "__exit__")?;
                // Reraise selects `__exit__(type, exc, None)`, other exits pass three Nones.
                let (exc_type, exc) = if matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) {
                    let exc = self.pending.exc_val.unwrap_or(Val::none());
                    let exc_name = self.exc_type_name(exc);
                    (self.heap.alloc(HeapObj::Type(exc_name))?, exc)
                } else {
                    (Val::none(), Val::none())
                };
                self.pending.method_binding = Some((class, cm));
                self.push(exit_fn);
                self.push(cm);
                self.push(exc_type);
                self.push(exc);
                self.push(Val::none());
            }
            OpCode::WithJudge => {
                let r = self.pop()?;
                // A truthy `__exit__` suppresses the exception, but never a cancel.
                if !self.cancelling && matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) && self.truthy(r) {
                    // Suppress, turn the re-raise into a normal exit and drop the exc identity.
                    if let Some(top) = self.unwind_stack.last_mut() { *top = Unwind::Normal; }
                    if let (Some(&(h, _)), Some(e)) = (self.handling.last(), self.pending.exc_val.take()) && h.0 == e.0 { self.handling.pop(); }
                }
            }
            // End of a finally body / WithExit, pop its reason and resume the exit it carried.
            OpCode::EndFinally => {
                match self.unwind_stack.pop() {
                    None | Some(Unwind::Normal) => {}
                    Some(Unwind::Return(v)) => {
                        if let Some(h) = self.next_cleanup_handler(exc_base) {
                            self.unwind_stack.push(Unwind::Return(v));
                            *ip = code.lowered(h);
                        } else {
                            return Ok(Some(v));
                        }
                    }
                    Some(Unwind::Goto { target, remaining }) => {
                        if remaining > 0 && let Some(h) = self.next_cleanup_handler(exc_base) {
                            self.unwind_stack.push(Unwind::Goto { target, remaining: remaining - 1 });
                            *ip = code.lowered(h);
                        } else {
                            *ip = code.lowered(target);
                        }
                    }
                    Some(Unwind::Reraise(e, at)) => {
                        self.error_byte_pos = at;
                        return Err(e);
                    }
                }
            }
            // break/continue across N finally/with blocks, the following Jump completes the transfer.
            OpCode::UnwindFinally => {
                let target = code.resume(*ip);
                if operand > 0 && let Some(h) = self.next_cleanup_handler(exc_base) {
                    self.unwind_stack.push(Unwind::Goto { target, remaining: operand - 1 });
                    *ip = code.lowered(h);
                }
            }
            OpCode::BeginArgs => {
                self.pending.delta_save.push((self.pending.pos_delta, self.pending.kw_delta));
                self.pending.pos_delta = 0;
                self.pending.kw_delta = 0;
            }
            OpCode::UnpackArgs => {
                let val = self.pop()?;
                let kw_before = (operand >> 2) as usize;
                match operand & 0x3 {
                    1 => {
                        let items = self.iterable_items(val, chunk)?;
                        let n = items.len() as i32;
                        // Insert below preceding kw pairs so positionals stay contiguous.
                        let at = self.stack.len() - 2 * kw_before;
                        for (off, v) in items.into_iter().enumerate() { self.stack.insert(at + off, v); }
                        self.pending.pos_delta += n - 1;
                    }
                    2 => {
                        let pairs = self.mapping_to_kw_pairs(val)?;
                        let n = pairs.len() as i32;
                        for (k, v) in pairs { self.push(k); self.push(v); }
                        self.pending.pos_delta -= 1;
                        self.pending.kw_delta += n;
                    }
                    _ => return Err(cold_runtime("UnpackArgs: bad operand")),
                }
            }
            OpCode::PopExcept => if let Some(f) = self.exception_stack.pop() { self.handling.truncate(f.handling_depth); },
            // Emitted by `break` to drop the abandoned for-loop iterator.
            OpCode::PopIter => { self.iter_stack.pop(); }
            _ => return Err(cold_runtime("unexpected opcode in generic dispatch")),
        }
        Ok(None)
    }

    /* Pops Except frames and the next Finally frame, returns its handler IP, or None at base. */
    fn next_cleanup_handler(&mut self, exc_base: usize) -> Option<usize> {
        while self.exception_stack.len() > exc_base {
            let frame = self.exception_stack.pop().unwrap();
            self.stack.truncate(frame.stack_depth);
            self.iter_stack.truncate(frame.iter_depth);
            self.with_stack.truncate(frame.with_depth);
            // Discard reasons from finally bodies skipped while seeking this handler.
            self.unwind_stack.truncate(frame.unwind_depth);
            self.handling.truncate(frame.handling_depth);
            if frame.kind == BlockKind::Finally {
                return Some(frame.handler_ip);
            }
        }
        None
    }

    /* MRO-bound dunder pair for a context manager, Err when unmet. */
    fn with_dunder(&mut self, cm: Val, name: &str) -> Result<(Val, Val), VmErr> {
        if cm.is_heap()
            && let HeapObj::Instance(cls_val, _) = self.heap.get(cm) {
            let cls = *cls_val;
            if let Some((func, class)) = self.lookup_class_member(cls, name)
                && func.is_heap()
                && matches!(self.heap.get(func), HeapObj::Func(..))
            {
                return Ok((func, class));
            }
        }
        Err(VmErr::TypeMsg(s!("'", str self.type_name(cm), "' object does not support the context manager protocol")))
    }

    /* Exception class name for a raised value, defaults to "Exception" for non-instances. */
    pub(crate) fn exc_type_name(&self, exc: Val) -> String {
        if !exc.is_heap() { return "Exception".into(); }
        match self.heap.get(exc) {
            HeapObj::ExcInstance(n, ..) => n.clone(),
            HeapObj::Instance(cls, _) => {
                if cls.is_heap() && let HeapObj::Class(name, _, _) = self.heap.get(*cls) { name.clone() } else { "Exception".into() }
            }
            _ => "Exception".into(),
        }
    }

    /* Heavy arms extracted out of `dispatch` so wasm-opt can dedup prologues and the dispatcher itself stays compact. */

    #[inline(never)]
    pub(crate) fn exec_arith_or_compare(&mut self, opcode: OpCode, operand: u16, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        // The register loop already tried two plain numbers.
        let site = cache.site(rip);
        if matches!(site, Site::Dunder { .. }) {
            if self.exec_dunder(site, chunk)? { return Ok(()); }
            cache.set_site(rip, Site::Empty);
        }
        if matches!(opcode, OpCode::Eq | OpCode::Lt | OpCode::NotEq | OpCode::Gt | OpCode::LtEq | OpCode::GtEq) {
            self.handle_compare(opcode, rip, cache, chunk)
        } else {
            self.handle_arith(opcode, operand, rip, cache, chunk)
        }
    }

    /* `obj[idx]` on the stack, a monomorphic `__getitem__` site skipping the class lookup. */
    pub(crate) fn get_item_op(&mut self, rip: usize, chunk: &SSAChunk, cache: &OpcodeCache) -> Result<(), VmErr> {
        let site = cache.site(rip);
        if matches!(site, Site::Dunder { .. }) {
            if self.exec_dunder(site, chunk)? { return Ok(()); }
            cache.set_site(rip, Site::Empty);
        }
        self.get_item(rip, chunk, cache)
    }

    /* One unit and the native work the heap counted, for native loops that bypass the back-edge counter. */
    #[inline]
    pub(crate) fn charge_step(&mut self) -> Result<(), VmErr> {
        self.charge_steps(1 + self.heap.take_work())
    }

    /* The native work counted since the last charge, paid before the next instruction runs. */
    #[inline]
    pub(crate) fn pay_work(&mut self) -> Result<(), VmErr> {
        let w = self.heap.take_work();
        if w == 0 { Ok(()) } else { self.charge_steps(w) }
    }

    /* Charge `n` units at once for native builtins (sort, materialise) whose cost scales with input size. */
    #[inline]
    pub(crate) fn charge_steps(&mut self, n: usize) -> Result<(), VmErr> {
        let n = n as u64;
        if self.budget < n { self.budget = 0; return Err(cold_budget()); }
        self.budget -= n;
        Ok(())
    }

    #[inline(never)]
    fn exec_for_iter(&mut self, op: u16, ip: &mut usize, n: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        match self.for_step(chunk)? {
            Some(item) => self.push(item),
            None => {
                if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                *ip = op as usize;
            }
        }
        Ok(())
    }

    /* The innermost iterator's next item, None once it ended and left the iterator stack. */
    #[inline(never)]
    fn for_step(&mut self, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        self.charge_step()?;
        if self.heap.needs_gc() { self.collect_point()?; }
        // The next item, or the value a `yield from` over this frame evaluates to once it ends.
        let step = match self.iter_stack.last() {
            // Resume directly so `yielded` distinguishes a yielded value (even None) from exhaustion.
            Some(&IterFrame::Coroutine(cv)) => {
                let result = self.resume_coroutine(cv)?;
                if core::mem::take(&mut self.yielded) { Ok(result) } else { Err(result) }
            }
            // A user iterator steps `__next__`, `StopIteration` ends the loop and `raise StopIteration(v)` carries `v`.
            Some(&IterFrame::UserDefined(iter)) => {
                let before = self.pending.exc_val;
                match self.iter_next_proto(iter, chunk) {
                    Ok(Some(item)) => Ok(item),
                    Ok(None) => Err(Val::none()),
                    Err(VmErr::Raised(m)) if m == "StopIteration" || m.starts_with("StopIteration:") => Err(self.swallow_stop(before)),
                    Err(e) => return Err(e),
                }
            }
            // Split-borrow heap so the Range step can promote to LongInt.
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

    /* The pre-registered value for a genuine builtin name, None for user names, so deleted user bindings stay deleted. */
    pub(crate) fn builtin_binding(&self, bare: &str) -> Option<Val> {
        if NativeFnId::from_name(bare).is_none() && crate::parser::builtin_type(bare).is_none() {
            return None;
        }
        self.global(bare)
    }

    #[inline(never)]
    fn exec_make_class(&mut self, op: u16, ip: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        // Operand layout mirrors `class_def_with`, low byte = class chunk index, high byte = base count.
        let class_idx = (op & 0xFF) as usize;
        let num_bases = (op >> 8) as usize;
        // Pop bases first so a misencoded operand fails before we touch the body.
        let mut bases = self.pop_n(num_bases)?;
        // An `object` base is a no-op.
        bases.retain(|&b| !(b.is_heap() && matches!(self.heap.get(b), HeapObj::Type(n) if n == "object")));
        // A builtin exception joins as a base, so the class enters its tree, and so do `dict` and `list`.
        for &b in &bases {
            let ok = match self.heap.try_get(b) {
                Some(HeapObj::Class(..)) => true,
                Some(HeapObj::Type(n)) => crate::vm::globals::matches_exc_class(n, "BaseException") || matches!(n.as_str(), "dict" | "list"),
                _ => false,
            };
            if !ok { return Err(cold_type("base class must be a class object")); }
        }
        // An instance takes one builtin layout at most, an exception, a dict or a list.
        let mut layouts: Vec<Val> = bases.iter().filter_map(|&b| if self.exc_base(b).is_some() { Some(Val::none()) } else { self.container_base(b) }).collect();
        layouts.dedup_by_key(|v| v.0);
        if layouts.len() > 1 { return Err(cold_type("multiple bases have instance lay-out conflict")); }
        let Some(body) = chunk.classes.get(class_idx) else {
            return Err(cold_runtime("class index out of range"));
        };
        // The cells around the class, its defining function's or its enclosing class's.
        let around: Vec<(String, Val)> = match self.body_to_fi.get(&(chunk as *const SSAChunk)) {
            Some(&fi) => chunk.names.iter().enumerate()
                .filter(|&(i, _)| self.fn_scope[fi].kinds.get(i) == Some(&Kind::Cell))
                .filter_map(|(i, n)| self.regs.get(self.base + i).filter(|&&c| matches!(self.heap.try_get(c), Some(HeapObj::Cell(_)))).map(|&c| (String::from(ssa_strip(n)), c)))
                .collect(),
            None if self.class_chunks.contains(&(chunk as *const SSAChunk)) => self.class_cells.last().cloned().unwrap_or_default(),
            None => Vec::new(),
        };
        // A class body reads enclosing variables first, then its module, then the builtins.
        let module = self.chunk_module_id(body);
        let mut class_slots = self.fill_builtins(&body.names);
        for (i, name) in body.names.iter().enumerate() {
            let bare = ssa_strip(name);
            if let Some(&(_, c)) = around.iter().find(|(n, _)| n == bare) {
                class_slots[i] = self.deref(c);
            } else if class_slots[i].is_undef() && let Some(gv) = self.scopes[module].get(bare).or_else(|| self.global(bare)) {
                class_slots[i] = gv;
            }
        }
        self.class_cells.push(around);
        let exec_result = self.exec(body, &mut class_slots);
        self.class_cells.pop();
        exec_result?;
        // Members are exactly the slots the body itself stores, loads of builtins or injected globals never leak into the class namespace.
        let mut member_slots: crate::util::hash::FxHashSet<u16> = crate::util::hash::FxHashSet::default();
        for ins in &body.instructions {
            if matches!(ins.opcode, OpCode::StoreName | OpCode::Phi) {
                member_slots.insert(ins.operand);
            }
        }
        let mut methods: Vec<(String, Val)> = Vec::new();
        for (i, name) in body.names.iter().enumerate() {
            if !member_slots.contains(&(i as u16)) { continue; }
            if let Some(&v) = class_slots.get(i)
                && !v.is_undef() {
                    let base = ssa_strip(name);
                    if let Some(pos) = methods.iter().position(|(n, _)| n == base) {
                        methods[pos].1 = v;
                    } else {
                        methods.push((base.to_string(), v));
                    }
                }
        }
        // Name comes from the StoreName target, skip any decorator `Call`s emitted between.
        let written = &chunk.instructions;
        let mut j = ip;
        while matches!(written.get(j).map(|i| i.opcode), Some(OpCode::Call)) { j += 1; }
        let name_str = written.get(j)
            .filter(|i| i.opcode == OpCode::StoreName)
            .and_then(|i| chunk.names.get(i.operand as usize))
            .map(|n| ssa_strip(n))
            .unwrap_or("?")
            .to_string();
        // C3 linearize before allocating, an inconsistent hierarchy raises here (Python parity), so no half-built class escapes.
        let mro_tail = self.c3_merge(&bases)?;
        let cls = self.heap.alloc(HeapObj::Class(name_str, bases, alloc::rc::Rc::new(core::cell::RefCell::new(methods))))?;
        // A site keyed on a swept class's slot must not match its successor.
        self.class_epoch = self.class_epoch.wrapping_add(1);
        let mut mro = Vec::with_capacity(mro_tail.len() + 1);
        mro.push(cls);
        mro.extend(mro_tail);
        self.mro_cache.insert(cls.0, alloc::rc::Rc::new(mro));
        self.push(cls);
        Ok(())
    }

    /* `obj.name = value` at a site, which keeps where an instance's attribute went. */
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_attr_at(&mut self, obj: Val, name_idx: u16, value: Val, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        if self.site_store(cache.site(rip), obj, value) { return Ok(()); }
        let name = chunk.names.get(name_idx as usize).ok_or_else(|| cold_runtime("StoreAttr: bad name index"))?;
        let before = match self.heap.try_get(obj) { Some(HeapObj::Instance(_, a)) => { let a = a.borrow(); (a.len(), a.entry_count()) } _ => (0, 0) };
        self.store_attr(obj, name, value, chunk)?;
        cache.set_site(rip, self.learn_store(obj, name, before));
        Ok(())
    }

    /* `obj.name = value`, shared by StoreAttr and `setattr()`. */
    pub(crate) fn store_attr(&mut self, obj: Val, name: &str, value: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        if !obj.is_heap() { return Err(cold_type("cannot set attribute")); }
        if let Some(cls_val) = self.instance_class(obj)
            && let Some((member, _)) = self.lookup_class_member(cls_val, name)
            && member.is_heap()
            && let HeapObj::Property(_, setter) = self.heap.get(member) {
            let setter = *setter;
            if setter.is_none() {
                return Err(VmErr::Attribute(s!("can't set attribute '", str name, "'")));
            }
            if self.depth >= self.max_calls { return Err(cold_depth()); }
            self.push(setter);
            self.push(obj);
            self.push(value);
            self.exec_call(2, chunk)?;
            self.pop()?;
            return Ok(());
        }
        // Class attribute, insert or replace in the mutable members store.
        if let HeapObj::Class(_, _, members) = self.heap.get(obj) {
            set_member(members, name, value);
            self.class_epoch = self.class_epoch.wrapping_add(1);
            return Ok(());
        }
        if let HeapObj::Func(_, _, _, attrs) = self.heap.get(obj) {
            set_member(attrs, name, value);
            // A cached result may have read the old attribute.
            self.templates.clear();
            return Ok(());
        }
        let key = self.heap.intern_str(name)?;
        let attrs = match self.heap.get(obj) {
            HeapObj::Instance(_, attrs) => attrs,
            _ => match self.heap.classed(obj) { Some((_, attrs)) => attrs, None => return Err(cold_type("cannot set attribute on this type")) },
        };
        attrs.borrow_mut().insert(key, value, &self.heap);
        Ok(())
    }
}
