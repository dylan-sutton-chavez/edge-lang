use alloc::{rc::Rc, vec::Vec};

use super::VM;
use super::lower::Code;
use super::types::*;

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
    // Whether the call watches its body for effects, for a memoized callee or one running under it.
    pub(crate) tracked: bool,
    // Where the arguments a memoized call keys its result on start in `memo_args`, and how many.
    pub(crate) memo: Option<(u32, u32)>,
    // The instance an `__init__` frame hands back in place of what it returns, undef for any other call.
    pub(crate) instance: Val,
}

/* What calling a function needs to know of its body, found once. */
#[derive(Clone, Copy)]
pub(crate) struct CallFacts {
    // Its positional count when every parameter is plain.
    pub(crate) arity: Option<usize>,
    // Whether a loop can open the body as one of its frames, its positionals filling the first slots.
    pub(crate) in_loop: bool,
    pub(crate) pool: usize,
    // Whether the body names `super`, the only reader of the binding a method call keeps, found on its first method call.
    pub(crate) uses_super: Option<bool>,
}

// A frame whose caller takes its result off the stack.
pub(crate) const NO_DEST: u16 = u16::MAX;
// Frames between two looks at the memory limit, which a deep recursion reaches first.
const DEPTH_CHECK: usize = 256;
// The share of the memory limit the frames of one run may take, so a runaway recursion ends long before memory does.
const FRAME_SHARE: usize = 16;
// How many frames a coroutine may hold suspended, each resume copying them all, about as deep as Python recurses by default.
pub(crate) const SUSPENDED_DEPTH: usize = 4 * MAX_CALLS;
// How deep memoized calls nest in Rust before they too run as loop frames.
pub(crate) const MEMO_NESTS: usize = MAX_CALLS / 2;

impl<'a> VM<'a> {
    /* Opens the body of a plain call as the next frame of the loop that made it, the frame at `base` already bound. */
    #[inline(always)]
    pub(crate) fn enter_frame(&mut self, fi: usize, callee: Val, base: usize, memo: Option<&[Val]>) -> Result<(), VmErr> {
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
        // Effects matter only to a memoizable body or to one running under it, so other calls track none.
        let tracked = self.memo_ok[fi] || !self.observed_impure.is_empty();
        if tracked { self.observed_impure.push(self.fn_scope[fi].declares); }
        let memo = memo.map(|args| {
            let at = self.memo_args.len();
            self.memo_args.extend_from_slice(args);
            (at as u32, args.len() as u32)
        });
        self.frames.push(Frame {
            code, base,
            exc_base: self.exception_stack.len(), unwind_base: self.unwind_stack.len(), delta_base: self.pending.delta_save.len(),
            ret: 0, dest: NO_DEST, fi, callee, call_ip: self.pending.call_ip.take(),
            stack_base: self.stack.len(), iter_base: self.iter_stack.len(), bound, tracked, memo,
            instance: core::mem::replace(&mut self.pending_instance, Val::undef()),
        });
        self.entered = true;
        self.yielded = true;
        Ok(())
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

    /* Ends the innermost loop frame in place, dropping what it left on the live stacks, the effect it showed the answer. */
    #[inline]
    pub(crate) fn leave_frame(&mut self) -> bool {
        let Some(f) = self.frames.last() else { return false };
        let (base, iter_base, bound, memo, tracked) = (f.base, f.iter_base, f.bound, f.memo, f.tracked);
        // Dropped where it lies, since moving a frame out copies every field.
        self.frames.truncate(self.frames.len() - 1);
        self.regs.truncate(base);
        if self.iter_stack.len() > iter_base { self.iter_stack.truncate(iter_base); }
        if bound { self.bindings.pop(); }
        if let Some((at, _)) = memo { self.memo_args.truncate(at as usize); }
        tracked && self.observed_impure.pop().unwrap_or(true)
    }

    /* Ends the innermost loop frame, which returned `v`, and gives where its caller carries on, the register it takes the result in and that result. */
    #[inline(always)]
    pub(crate) fn return_frame(&mut self, v: Val) -> (usize, u16, Val) {
        let Some(f) = self.frames.last() else { return (0, NO_DEST, v) };
        let (fi, callee, ret, dest, memo, tracked, instance) = (f.fi, f.callee, f.ret, f.dest, f.memo, f.tracked, f.instance);
        match memo {
            Some((at, n)) if !tracked || !self.observed_impure.last().copied().unwrap_or(true) => {
                if self.functions[fi].1.is_pure { self.keep_frame(fi, callee, at as usize, n as usize, v); }
                self.leave_frame();
            }
            _ => if self.leave_frame() { self.showed_effect(fi); },
        }
        (ret, dest, if instance.is_undef() { v } else { instance })
    }

    /* Keeps the result of a memoized frame that showed no effect, keyed on the arguments it kept. */
    #[inline(never)]
    fn keep_frame(&mut self, fi: usize, callee: Val, at: usize, n: usize, v: Val) {
        // A function with defaults keys on itself and on them.
        let (defaults, owner) = match self.heap.get(callee) {
            HeapObj::Func(_, defaults, ..) if !defaults.is_empty() => (defaults.clone(), callee),
            _ => (Vec::new(), Val::none()),
        };
        let args = core::mem::take(&mut self.memo_args);
        self.memo_keep(fi, callee, &args[at..at + n], &defaults, owner, v);
        self.memo_args = args;
        // A memo that stopped paying is off for good, sparing every later hash.
        if self.templates.dead(fi) { self.memo_ok[fi] = false; }
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
        if self.leave_frame() { self.showed_effect(fi); }
        self.pending_sync_frames.push(SyncFrame { ip, fi, func, slots, stack_delta, iter_delta, exception_delta, instance });
        ret
    }

    /* A body that showed an effect keeps no result, so its calls stop trying and its caller shows it too. */
    pub(crate) fn showed_effect(&mut self, fi: usize) {
        self.mark_impure();
        if let Some(m) = self.memo_ok.get_mut(fi) { *m = false; }
    }
}
