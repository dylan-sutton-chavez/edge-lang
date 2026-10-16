use alloc::{rc::Rc, vec::Vec};

use super::VM;
use super::lower::Code;
use super::types::*;
use super::lower::{self, Ins};
use super::sites::Site;
use super::globals::async_ops::FINISHED;
use crate::value::BodyRef;
use crate::parser::{OpCode, SSAChunk};

/* A plain call the instruction loop runs in place of a nested one, what it runs and what its caller gets back. */
pub(crate) struct Frame {
    // The lowered code its pool holds for as long as the VM lives, so the frame keeps no count of it.
    pub(crate) code: *const Code,
    pub(crate) base: usize,
    pub(crate) exc_base: usize,
    pub(crate) unwind_base: usize,
    pub(crate) delta_base: usize,
    // Where the caller carries on once the call is over.
    pub(crate) ret: usize,
    // The caller register the store at `ret` would put the result in, written straight past it, or NO_DEST.
    pub(crate) dest: u16,
    pub(crate) fi: usize,
    pub(crate) callee: Val,
    // The compiler instruction that called, where the traceback and an error met in the caller point.
    pub(crate) call_ip: Option<u32>,
    pub(crate) stack_base: usize,
    pub(crate) iter_base: usize,
    pub(crate) bound: bool,
    // The instance an `__init__` frame hands back in place of what it returns, undef for any other call.
    pub(crate) instance: Val,
    // The generator a `for` loop runs as this frame, which a yield hands its value back from, undef for a call.
    pub(crate) generator: Val,
}

/* What calling a function needs to know of its body, found once. */
#[derive(Clone, Copy)]
pub(crate) struct CallFacts {
    // Its positional count when every parameter is plain.
    pub(crate) arity: Option<usize>,
    // How many of those take no default, the fewest a call may pass.
    pub(crate) required: usize,
    // Whether it captures cells, which its frame takes off the function each call.
    pub(crate) closure: bool,
    // Whether a loop can open the body as one of its frames, its positionals filling the first slots.
    pub(crate) in_loop: bool,
    pub(crate) pool: usize,
    // Whether the body names `super`, the only reader of the binding a method call keeps, found on its first method call.
    pub(crate) uses_super: Option<bool>,
}

// A frame whose caller takes its result off the stack.
pub(crate) const NO_DEST: u16 = u16::MAX;

/* Where the instruction after a call wants its result, a register or the stack. */
#[inline(always)]
pub(crate) fn dest_of(next: Option<&Ins>) -> u16 {
    match next {
        Some(s) if s.op == OpCode::StoreTopR && s.x == 0 => s.a,
        _ => NO_DEST,
    }
}

// Frames between two looks at the memory limit, which a deep recursion reaches first.
const DEPTH_CHECK: usize = 256;
// The share of the memory limit the frames of one run may take, so a runaway recursion ends long before memory does.
const FRAME_SHARE: usize = 16;
// How many frames a coroutine may hold suspended, each resume copying them all, about as deep as Python recurses by default.
pub(crate) const SUSPENDED_DEPTH: usize = 4 * MAX_CALLS;

impl<'a> VM<'a> {
    /* Opens the body of a plain call as the next frame of the loop that made it, the frame at `base` already bound. */
    #[inline(always)]
    pub(crate) fn enter_frame(&mut self, fi: usize, callee: Val, base: usize) -> Result<(), VmErr> {
        // The depth follows the memory limit, a recursion whose frames outgrow their share of it is one too deep.
        if self.frames.len() % DEPTH_CHECK == DEPTH_CHECK - 1 {
            let held = self.regs.len() * core::mem::size_of::<Val>() + self.frames.len() * core::mem::size_of::<Frame>();
            if held > self.heap.limit() / FRAME_SHARE || self.heap.over() {
                self.regs.truncate(base);
                return Err(cold_depth());
            }
        }
        let Some(code) = self.pools[self.fn_pool[fi]].code.as_ref().map(Rc::as_ptr) else { return Err(cold_runtime("a frame entered without its code")) };
        let bound = match self.pending.method_binding.take() {
            Some((class, recv)) if self.uses_super(fi) => { self.bindings.push((self.level() + 1, class, recv)); true }
            _ => false,
        };
        self.frames.push(Frame {
            code, base,
            exc_base: self.exception_stack.len(), unwind_base: self.unwind_stack.len(), delta_base: self.pending.delta_save.len(),
            ret: 0, dest: NO_DEST, fi, callee, call_ip: self.pending.call_ip.take(),
            stack_base: self.stack.len(), iter_base: self.iter_stack.len(), bound,
            instance: core::mem::replace(&mut self.pending_instance, Val::undef()), generator: Val::undef(),
        });
        self.entered = true;
        self.yielded = true;
        Ok(())
    }

    /* Opens the frame of a call its site knew with its `n` positionals, the code and chunk it runs, None when the full path must check it. */
    #[inline(always)]
    pub(crate) fn enter_site(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val]) -> Option<(*const Code, *const SSAChunk)> {
        self.open_site(i, code, fi, callee, args, Val::undef(), Val::undef())
    }

    /* `enter_site` for a frame that hands back `instance` in place of its result, and binds `super` to `owner` for its receiver, undef for neither. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn open_site(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], instance: Val, owner: Val) -> Option<(*const Code, *const SSAChunk)> {
        let n = args.len();
        let facts = self.call_facts[fi];
        let arity = facts.arity?;
        // A spent budget or a depth check take the full path.
        if !facts.in_loop || n > arity || n < facts.required
            || self.budget == 0 || self.frames.len() % DEPTH_CHECK == DEPTH_CHECK - 1 { return None; }
        if args.iter().any(|v| v.is_undef()) { return None; }
        // A method whose body names `super` keeps its binding, one not looked at yet takes the full path.
        let bound = !owner.is_undef() && facts.uses_super?;
        let callee_code = Rc::as_ptr(self.pools[facts.pool].code.as_ref()?);
        // The step pays the native work counted since the last one, as `charge_step` does.
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let base = self.regs.len();
        self.regs.extend_from_slice(&self.slot_templates[fi]);
        // Its parameters are its first slots.
        self.regs[base..base + n].copy_from_slice(args);
        if n < arity || facts.closure { self.fill_frame(callee, base, n, arity); }
        // A result the next instruction would store goes straight to its register.
        // A wide call reads its last operands from the ArgsR after it.
        let next = i + 1 + lower::wide(code.ins[i]) as usize;
        let dest = dest_of(code.ins.get(next));
        if bound { self.bindings.push((self.level() + 1, owner, args[0])); }
        self.frames.push(Frame {
            code: callee_code, base,
            exc_base: self.exception_stack.len(), unwind_base: self.unwind_stack.len(), delta_base: self.pending.delta_save.len(),
            ret: next, dest, fi, callee, call_ip: Some(code.orig(i)),
            stack_base: self.stack.len(), iter_base: self.iter_stack.len(), bound, instance, generator: Val::undef(),
        });
        self.base = base;
        let chunk = &self.functions[fi].1 as *const SSAChunk;
        // SAFETY the pool holds the code for as long as the VM lives.
        let c = unsafe { &*callee_code };
        // SAFETY the chunk lives as long as the VM.
        if self.regs.len() - base < c.frame { self.grow_frame(unsafe { &*chunk }, c, facts.pool); }
        Some((callee_code, chunk))
    }

    /* Hands the result `v` of a frame to its caller as `dest` asks, the index the caller carries on at. */
    #[inline(always)]
    pub(crate) fn deliver(&mut self, v: Val, ret: usize, dest: u16) -> usize {
        match dest {
            NO_DEST => { self.stack.push(v); ret }
            d => { self.regs[self.base + d as usize] = v; ret + 1 }
        }
    }

    /* `enter_site` or `enter_method` for a call whose operands past the first two lie in the ArgsR after it. */
    #[inline(never)]
    pub(crate) fn enter_wide(&mut self, i: usize, code: &Code, chunk: &SSAChunk, fi: usize, callee: Val) -> Option<(*const Code, *const SSAChunk)> {
        let ins = code.ins[i];
        let (regs, count) = lower::call_regs(ins, *code.ins.get(i + 1)?);
        let r = &self.regs[self.base..];
        let vals: [Val; 6] = regs.map(|k| r[k as usize]);
        if ins.op == OpCode::CallR { self.enter_site(i, code, fi, callee, &vals[1..count]) }
        else { self.enter_method(i, code, chunk, ins.a, &vals[..count]) }
    }

    /* Fills the parameters past the `n` passed from the defaults of `callee`, and the cells it captured. */
    #[inline(never)]
    pub(crate) fn fill_frame(&mut self, callee: Val, base: usize, n: usize, arity: usize) {
        let Some(HeapObj::Func(_, defaults, captures, _)) = self.heap.try_get(callee) else { return };
        // Defaults belong to the trailing parameters.
        let first = arity - defaults.len();
        self.regs[base + n..base + arity].copy_from_slice(&defaults[n - first..arity - first]);
        for &(slot, cell) in captures { if let Some(r) = self.regs.get_mut(base + slot) { *r = cell; } }
    }

    /* Opens the method a site knows for the receiver in `args`, as `enter_site` does, None when it binds `super` or the site no longer holds. */
    #[inline(always)]
    pub(crate) fn enter_method(&mut self, i: usize, code: &Code, chunk: &SSAChunk, name: u16, args: &[Val]) -> Option<(*const Code, *const SSAChunk)> {
        let (func, owner) = self.site_method(code.cache.site(i), args[0], chunk.names.get(name as usize)?)?;
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
        self.open_site(i, code, fi, func, args, Val::undef(), owner)
    }

    /* Builds an instance of the class a constructor site knows and opens its `__init__` with the `n` positionals in `passed`, None when the site no longer holds. */
    #[inline(never)]
    pub(crate) fn enter_ctor(&mut self, i: usize, code: &Code, site: Site, passed: [Val; 2], n: usize) -> Option<(*const Code, *const SSAChunk)> {
        let Site::Ctor { class, init, owner, fi, attrs, epoch } = site else { return None };
        if epoch != self.class_epoch || passed[..n].iter().any(|v| v.is_undef()) { return None; }
        let instance = self.new_instance(class, attrs as usize).ok()?;
        let args = [instance, passed[0], passed[1]];
        self.open_site(i, code, fi as usize, init, &args[..n + 1], instance, owner)
    }

    /* Ends the running frame on a plain return of `v`, the code, chunk and index its caller carries on at, None when the return needs its handler. */
    #[inline(always)]
    pub(crate) fn leave_site(&mut self, v: Val, floor_code: *const Code, floor_chunk: *const SSAChunk, floor_base: usize, floor_depth: usize) -> Option<(*const Code, *const SSAChunk, usize)> {
        let f = self.frames.last()?;
        // Cleanup and the end of a generator take the full path.
        if v.is_undef() || !f.generator.is_undef() || self.exception_stack.len() > f.exc_base || self.unwind_stack.len() > f.unwind_base { return None; }
        let (ret, dest, base, iter_base, bound, instance) = (f.ret, f.dest, f.base, f.iter_base, f.bound, f.instance);
        self.frames.truncate(self.frames.len() - 1);
        self.regs.truncate(base);
        if self.iter_stack.len() > iter_base { self.iter_stack.truncate(iter_base); }
        if bound { self.bindings.pop(); }
        // An `__init__` frame hands back its instance.
        let v = if instance.is_undef() { v } else { instance };
        let (code, chunk) = match self.frames.last() {
            Some(f) if self.frames.len() > floor_depth => { self.base = f.base; (f.code, &self.functions[f.fi].1 as *const SSAChunk) }
            _ => { self.base = floor_base; (floor_code, floor_chunk) }
        };
        Some((code, chunk, self.deliver(v, ret, dest)))
    }

    /* Whether the body of `fi` names `super`, looked up once. */
    pub(crate) fn uses_super(&mut self, fi: usize) -> bool {
        let names = &self.functions[fi].1.names;
        *self.call_facts[fi].uses_super.get_or_insert_with(|| names.iter().any(|n| crate::parser::ssa_strip(n) == "super"))
    }

    /* How deep the running frame is, nested loops and loop frames alike, which tells one method binding from another. */
    #[inline]
    pub(crate) fn level(&self) -> usize { self.depth + self.frames.len() }

    /* Whether the body of `fi` can run as a frame of the loop that calls it. */
    #[inline]
    pub(crate) fn runs_in_loop(&self, fi: usize, call_safe: bool) -> bool {
        let pool = &self.pools[self.fn_pool[fi]];
        call_safe == self.frame_safe && pool.code.is_some() && pool.consts.is_some()
    }

    /* Ends the innermost loop frame in place, dropping what it left on the live stacks. */
    #[inline]
    pub(crate) fn leave_frame(&mut self) {
        let Some(f) = self.frames.last() else { return };
        let (base, iter_base, bound) = (f.base, f.iter_base, f.bound);
        // Dropped where it lies, since moving a frame out copies every field.
        self.frames.truncate(self.frames.len() - 1);
        self.regs.truncate(base);
        if self.iter_stack.len() > iter_base { self.iter_stack.truncate(iter_base); }
        if bound { self.bindings.pop(); }
    }

    /* Ends the innermost loop frame, which returned `v`, and gives where its caller carries on, the register it takes the result in and that result. */
    #[inline(always)]
    pub(crate) fn return_frame(&mut self, v: Val) -> (usize, u16, Val) {
        let Some(f) = self.frames.last() else { return (0, NO_DEST, v) };
        let (ret, dest, instance) = (f.ret, f.dest, f.instance);
        self.leave_frame();
        (ret, dest, if instance.is_undef() { v } else { instance })
    }

    /* Opens the generator `co` a `for` at `i` steps as the next frame of the loop, the code, chunk and index it carries on at, None when its state needs the nested resume. */
    #[inline(never)]
    pub(crate) fn enter_gen(&mut self, co: Val, i: usize, code: &Code) -> Option<(*const Code, *const SSAChunk, usize)> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return None };
        // A generator parked on a wait takes what woke it, which its `for` holds on top.
        if c.syncs.is_empty() && c.stack.last().is_some_and(|v| v.is_undef()) {
            self.wake_gen(co, true);
            return self.enter_gen(co, i, code);
        }
        // A finished body, helper frames waiting inside it or a body not lowered yet take the nested resume.
        let BodyRef::Fn(fi) = c.body else { return None };
        if c.ip == FINISHED || !c.syncs.is_empty() || !self.functions[fi].1.is_generator || self.is_async_fn(fi) { return None; }
        let facts = self.call_facts[fi];
        let gen_code = Rc::as_ptr(self.pools[facts.pool].code.as_ref()?);
        if self.executing_coros.contains(&co.0) { return None; }
        // The depth follows the memory limit as a call's does, a nested resume taking only what outgrew it.
        if self.frames.len() % DEPTH_CHECK == DEPTH_CHECK - 1 {
            let held = self.regs.len() * core::mem::size_of::<Val>() + self.frames.len() * core::mem::size_of::<Frame>();
            if held > self.heap.limit() / FRAME_SHARE || self.heap.over() { return None; }
        }
        // The step pays for the state it moves, as a resume does.
        let cost = 1 + (c.slots.len() + c.stack.len() + c.iters.len()) as u64 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let (base, stack_base, iter_base, exc_base) = (self.regs.len(), self.stack.len(), self.iter_stack.len(), self.exception_stack.len());
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        let resume = c.ip;
        // The saved state moves onto the live stacks, as `restore_into` does, its buffers left empty for the next yield.
        self.regs.extend_from_slice(&c.slots);
        c.slots.clear();
        self.stack.extend_from_slice(&c.stack);
        c.stack.clear();
        self.iter_stack.append(&mut c.iters);
        for f in c.excs.iter_mut() {
            f.stack_depth += stack_base;
            f.iter_depth += iter_base;
        }
        self.exception_stack.append(&mut c.excs);
        self.frames.push(Frame {
            code: gen_code, base, exc_base, unwind_base: self.unwind_stack.len(), delta_base: self.pending.delta_save.len(),
            ret: i, dest: NO_DEST, fi, callee: co, call_ip: Some(code.orig(i)),
            stack_base, iter_base, bound: false, instance: Val::undef(), generator: co,
        });
        self.executing_coros.push(co.0);
        self.gen_frames += 1;
        self.base = base;
        let chunk = &self.functions[fi].1 as *const SSAChunk;
        // SAFETY the pool holds the code for as long as the VM lives.
        let c = unsafe { &*gen_code };
        // SAFETY the chunk lives as long as the VM.
        if self.regs.len() - base < c.frame { self.grow_frame(unsafe { &*chunk }, c, facts.pool); }
        Some((gen_code, chunk, c.lowered(resume)))
    }

    /* Ends the innermost frame, a generator that yielded at `resume`, back into its object, and gives the `for` that stepped it. */
    pub(crate) fn yield_gen(&mut self, resume: usize) -> usize {
        let Some(f) = self.frames.last() else { return 0 };
        let (co, base, stack_base, iter_base, exc_base, ret) = (f.generator, f.base, f.stack_base, f.iter_base, f.exc_base, f.ret);
        // The live stacks above the frame go back into the generator, its handler depths made relative, as `save_frames` does.
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            c.ip = resume;
            c.slots.extend_from_slice(&self.regs[base..]);
            c.stack.extend_from_slice(&self.stack[stack_base.min(self.stack.len())..]);
            c.iters.extend(self.iter_stack.drain(iter_base.min(self.iter_stack.len())..));
            let at = c.excs.len();
            c.excs.extend(self.exception_stack.drain(exc_base.min(self.exception_stack.len())..));
            for f in &mut c.excs[at..] {
                f.stack_depth = f.stack_depth.saturating_sub(stack_base);
                f.iter_depth = f.iter_depth.saturating_sub(iter_base);
            }
        }
        self.stack.truncate(stack_base);
        self.leave_gen();
        ret
    }

    /* Hands what woke the parked generator `co` to the place its wait marked, off the stack of the `for` that parked
       with it, or None when something else steps it. */
    pub(crate) fn wake_gen(&mut self, co: Val, from_for: bool) {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return };
        let Some(top) = c.wait_slot().filter(|t| t.is_undef()) else { return };
        *top = if from_for { self.stack.pop().unwrap_or_else(Val::none) } else { Val::none() };
    }

    /* Parks the generator frame on top where it waits, with the helpers waiting inside it, and gives the `for` that
       stepped it, which waits in its place and steps it again on resume. */
    #[cold]
    #[inline(never)]
    pub(crate) fn park_gen(&mut self) -> usize {
        let co = self.frames.last().map_or(Val::undef(), |f| f.generator);
        let resume = core::mem::take(&mut self.resume_ip);
        let at = self.yield_gen(resume);
        let helpers = core::mem::take(&mut self.pending_sync_frames);
        // A wait for a value leaves a placeholder, marked so the next step takes the value from the `for`.
        let placeholder = !self.pending.preempt_request;
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            c.syncs.extend(helpers.into_iter().rev());
            if placeholder && let Some(top) = c.wait_slot() { *top = Val::undef(); }
        }
        if placeholder { self.stack.push(Val::none()); }
        at
    }

    /* `yield` of the generator frame on top, its value `v` handed to the `for` that stepped it, the code, chunk and index that carry on, None when it suspends otherwise. */
    #[inline(never)]
    pub(crate) fn yield_site(&mut self, v: Val, resume: usize, floor_code: *const Code, floor_chunk: *const SSAChunk, floor_base: usize, floor_depth: usize) -> Option<(*const Code, *const SSAChunk, usize)> {
        let at = self.yield_gen(resume);
        let (code, chunk) = match self.frames.last() {
            Some(f) if self.frames.len() > floor_depth => { self.base = f.base; (f.code, &self.functions[f.fi].1 as *const SSAChunk) }
            _ => { self.base = floor_base; (floor_code, floor_chunk) }
        };
        // SAFETY the caller runs code its pool or the floor keeps.
        self.hand_to_for(unsafe { &*code }, at, v);
        Some((code, chunk, at + 1))
    }

    /* The `for` at `at` of `code` takes the value `v` a generator yielded as its own step would, into a register, a module binding or the stack. */
    pub(crate) fn hand_to_for(&mut self, code: &Code, at: usize, v: Val) {
        let step = code.ins[at];
        match step.op {
            OpCode::ForIterR if step.x == 0 => self.regs[self.base + step.b as usize] = v,
            OpCode::ForIterR => self.scopes[code.module].set_at(step.b as u32, v),
            _ => self.stack.push(v),
        }
    }

    /* Ends the innermost frame, a generator that returned or raised, which then stays finished. */
    pub(crate) fn finish_gen(&mut self) -> usize {
        let Some(f) = self.frames.last() else { return 0 };
        let (co, ret) = (f.generator, f.ret);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.ip = FINISHED; }
        self.leave_gen();
        ret
    }

    /* Drops the innermost frame, a generator's, and its running mark. */
    fn leave_gen(&mut self) {
        let co = self.frames.last().map_or(Val::undef(), |f| f.generator);
        self.leave_frame();
        if let Some(k) = self.executing_coros.iter().rposition(|&g| g == co.0) { self.executing_coros.remove(k); }
        self.gen_frames -= 1;
    }

    /* Keeps the innermost loop frame, which suspended, as a sync frame its coroutine runs again on resume, and gives where its caller carries on. */
    #[inline(never)]
    pub(crate) fn park_frame(&mut self) -> usize {
        let Some(f) = self.frames.last() else { return 0 };
        let (fi, func, ret, base, instance) = (f.fi, f.callee, f.ret, f.base, f.instance);
        let ip = core::mem::take(&mut self.resume_ip);
        let (mut stack_delta, mut iter_delta, mut exception_delta) = (Vec::new(), Vec::new(), Vec::new());
        self.save_frames(f.stack_base, f.iter_base, f.exc_base, &mut stack_delta, &mut iter_delta, &mut exception_delta);
        let slots = self.regs.split_off(base);
        self.leave_frame();
        self.pending_sync_frames.push(SyncFrame { ip, fi, func, slots, stack_delta, iter_delta, exception_delta, instance });
        ret
    }
}
