use alloc::{rc::Rc, vec::Vec};

use super::VM;
use super::lower::Code;
use super::types::*;
use super::lower::{self, Ins};
use super::sites::Site;
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
            instance: core::mem::replace(&mut self.pending_instance, Val::undef()),
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
            stack_base: self.stack.len(), iter_base: self.iter_stack.len(), bound, instance,
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
        // Cleanup takes the full path.
        if v.is_undef() || self.exception_stack.len() > f.exc_base || self.unwind_stack.len() > f.unwind_base { return None; }
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
