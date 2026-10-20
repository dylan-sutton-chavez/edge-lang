use alloc::{boxed::Box, vec::Vec};

use super::VM;
use super::act::REDO;
use super::types::*;
use super::jobs::Check;

/* An op whose handler calls a Python function runs it as a frame of the loop and runs again once it returns or raises, taking what each earlier call gave in order, so none is made twice. */
pub(crate) struct Redo {
    // An op's handler hands its calls to the loop now.
    pub(crate) on: bool,
    // A builtin runs now that arms only once a call it makes needs it, its op kept in `st`.
    pub(crate) lazy: bool,
    // The op runs again, its last call just back.
    pub(crate) keep: bool,
    // The rest, made for the first op that arms, so a program none of whose ops does pays nothing for it.
    st: Option<Box<State>>,
}

/* What an op handing its calls to the loop keeps between them. */
struct State {
    // The op whose handler runs now.
    arm: Arm,
    // The call op whose callee a general call is about to dispatch, and how it runs again, for a builtin it calls to arm.
    site: (u32, Rerun),
    // What the calls of each op running or waiting for a call gave, in order, each op's after those of the ops under it.
    log: Vec<Done>,
    // For each frame such a call opened, innermost last.
    saved: Vec<Saved>,
    // The instance and name of the dunder a handler calls now, which its call keeps so the handler run again finds it by them.
    hint: Option<Key>,
    // The op of the builtin running now that arms only once a call it makes needs it, and its operands.
    lazy: Lazy,
    // The conversions of a builtin a job runs again, while it runs.
    conv: Option<Box<Conv>>,
    held: [Val; OPS],
}

/* What the dunders a builtin run as a job calls gave, in order, for its handler run again, and the first one past them, which the job runs as a frame. */
pub(crate) struct Conv { log: Vec<Val>, pos: usize, req: Option<(Val, Check)> }

impl Conv { pub(crate) fn new() -> Self { Conv { log: Vec::new(), pos: 0, req: None } } }

/* An instance and the name of a dunder called on it. */
pub(crate) type Key = (u64, usize, usize);

/* The op whose handler may hand its calls to the loop, its depths, index, stack length with its `n` operands on top, where its calls begin in the log and how far its walk got. */
#[derive(Clone, Copy)]
pub(crate) struct Arm { fresh: bool, depth: usize, acts: usize, rip: usize, len: usize, ops: [Val; OPS], n: u8, start: usize, pos: usize, then: Then, walk: Walk }

/* How far the walk of the op running again got before, the pairs found equal and where their calls end in the log, so it carries on past them. */
#[derive(Clone, Copy, Default)]
pub(crate) struct Walk { done: u16, pos: u16 }

/* What an op makes of its handler's one call's value when it need not run again, push or store it, or for a stack op push it or its truth negated, as a `!=` takes an `__eq__`'s. */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Then { Again, Push, Reg(u16), Global(u16, u32), Done, Not }

/* A builtin's op armed only once a call it makes needs it, with its index, how it runs again, its stack length, its operands and its depths. */
#[derive(Clone, Copy)]
pub(crate) struct Lazy { rip: usize, rerun: Rerun, len: usize, n: u8, depth: usize, acts: usize }

/* What `redo_defer` did, for `redo_undefer` to end. */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Defer { No, Yes }

/* How the op whose handler runs may run again, not at all, as a stack op with its operands put back, or as a register op's stack form with them left in registers. */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Rerun { No, Stack, Regs }

/* The longest containers whose items' dunders an op hands to the loop, as each call makes it walk again what came before. */
pub(crate) const WALK: usize = 16;

/* The most calls an op hands to the loop, each running the op again over those before, so replays stay a few thousand and later calls run in place. */
pub(crate) const CALLS: usize = 64;

/* The most operands an op handing its calls to the loop keeps. */
pub(crate) const OPS: usize = 8;

/* A frame a call of an op's handler opened, with its caller's depth, the op's calls' span in the log, the pending exception and the stack lengths an error out of it restores. */
struct Saved { acts: usize, start: usize, end: usize, before: Option<Val>, stack: usize, n: usize, iters: usize, withs: usize, key: Option<Key>, then: Then, walk: Walk }

/* A call an op's handler made, its function, the dunder it was, and what it gave (an error with the exception it left pending). */
struct Done { func: Val, key: Option<Key>, v: Result<Val, (VmErr, Option<Val>)> }

/* Copies `vals` into `held` from `at`, one by one, a call's few values too few for a block copy to pay. */
#[inline(always)]
fn hold(held: &mut [Val; OPS], at: usize, vals: &[Val]) {
    let mut k = 0;
    while k < vals.len() && at + k < OPS { held[at + k] = vals[k]; k += 1; }
}

impl Redo {
    pub(crate) fn new() -> Self { Redo { on: false, lazy: false, keep: false, st: None } }

    /* Marks what the ops' calls gave, which they take running again. */
    pub(crate) fn mark(&self, heap: &mut HeapPool) {
        let Some(st) = &self.st else { return };
        if self.lazy { for &v in &st.held { heap.mark(v); } }
        for d in &st.log {
            heap.mark(d.func);
            match d.v { Ok(v) | Err((_, Some(v))) => heap.mark(v), Err((_, None)) => {} }
        }
        for v in st.saved.iter().filter_map(|s| s.before) { heap.mark(v); }
        for &v in &st.arm.ops { heap.mark(v); }
        if let Some(c) = &st.conv { for &v in &c.log { heap.mark(v); } if let Some((v, k)) = c.req { heap.mark(v); if let Some(a) = k.arg() { heap.mark(a); } } }
    }

    /* What the ops keep, made on the first that arms. */
    #[inline(always)]
    fn st(&mut self) -> &mut State {
        if self.st.is_none() { self.make(); }
        // SAFETY made just above when it was not.
        unsafe { self.st.as_deref_mut().unwrap_unchecked() }
    }

    #[cold]
    #[inline(never)]
    fn make(&mut self) {
        let arm = Arm { fresh: false, depth: 0, acts: 0, rip: 0, len: 0, ops: [Val::undef(); OPS], n: 0, start: 0, pos: 0, then: Then::Again, walk: Walk::default() };
        let lazy = Lazy { rip: 0, rerun: Rerun::No, len: 0, n: 0, depth: 0, acts: 0 };
        self.st = Some(Box::new(State { arm, site: (0, Rerun::No), conv: None, log: Vec::new(), saved: Vec::new(), hint: None, lazy, held: [Val::undef(); OPS] }));
    }

    /* Notes the call op whose callee a general call dispatches now, for a builtin it calls to arm. */
    #[inline(never)]
    pub(crate) fn set_site(&mut self, site: (u32, Rerun)) { self.st().site = site; }

    /* Takes the call op `set_site` noted, none when no call noted one. */
    #[inline(always)]
    pub(crate) fn take_site(&mut self) -> (u32, Rerun) {
        match self.st.as_deref_mut() { Some(st) => core::mem::replace(&mut st.site, (0, Rerun::No)), None => (0, Rerun::No) }
    }

    /* Ends the general call `set_site` noted. */
    #[inline(always)]
    pub(crate) fn end_site(&mut self) { if let Some(st) = self.st.as_deref_mut() { st.site.1 = Rerun::No; } }

    /* Keeps the dunder a handler calls now, for its call to note. */
    #[inline(always)]
    pub(crate) fn set_hint(&mut self, key: Option<Key>) { if let Some(st) = self.st.as_deref_mut() { st.hint = key; } }
}

impl<'a> VM<'a> {
    /* Marks the op at `rip` with `n` operands on the stack as handing its calls to the loop, true when this call armed it, false when an op around it already did. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_arm(&mut self, rip: usize, n: usize) -> bool {
        // An op of more operands than it keeps makes its calls nested.
        if self.redo.on || n > OPS { return false; }
        let len = self.stack.len();
        let n = n.min(len);
        let mut ops = [Val::undef(); OPS];
        ops[..n].copy_from_slice(&self.stack[len - n..]);
        self.redo_arm_at(rip, len, &ops[..n])
    }

    /* Arms from inside its handler the op at `rip` that `rerun` says runs again, a stack op with `ops` atop a stack `len` long or a register op's stack form, none put back. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_late(&mut self, rip: usize, rerun: Rerun, len: usize, ops: &[Val]) -> bool {
        match rerun {
            Rerun::No => false,
            Rerun::Stack => self.redo_arm_at(rip, len, ops),
            Rerun::Regs => self.redo_arm_at(rip, len - ops.len(), &[]),
        }
    }

    /* `redo_arm` of the op whose stack began `len` long, `ops` on its top. */
    fn redo_arm_at(&mut self, rip: usize, len: usize, ops: &[Val]) -> bool {
        let n = ops.len();
        if self.redo.on || n > OPS { return false; }
        if !self.redo.keep {
            let (depth, acts) = (self.depth, self.acts);
            let a = &mut self.redo.st().arm;
            (a.fresh, a.depth, a.acts, a.rip, a.len, a.n, a.then, a.walk) = (true, depth, acts, rip, len, n as u8, Then::Again, Walk::default());
            a.ops[..n].copy_from_slice(ops);
            self.redo.on = true;
            return true;
        }
        let keep = core::mem::take(&mut self.redo.keep);
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        while let Some(s) = st.saved.last() && s.acts >= acts { st.log.truncate(s.start); st.saved.pop(); }
        let a = &mut st.arm;
        // An op run again takes its calls from where they begin, any other's begin past those of the ops under it.
        if !keep || a.rip != rip || a.acts != acts || a.depth != depth {
            a.start = st.saved.last().map_or(0, |s| s.end);
            st.log.truncate(a.start);
            a.walk = Walk::default();
        }
        (a.fresh, a.pos, a.depth, a.acts, a.rip, a.len, a.n, a.then) = (false, a.start, depth, acts, rip, len, n as u8, Then::Again);
        a.ops[..n].copy_from_slice(ops);
        self.redo.on = true;
        true
    }

    /* `redo_arm` for an op that does not run again, where its calls begin in the log found once it makes one. */
    #[inline(always)]
    fn redo_arm_quick(&mut self, rip: usize, n: usize) -> bool {
        if self.redo.on { return false; }
        if self.redo.keep || n > OPS { return self.redo_arm(rip, n); }
        let len = self.stack.len();
        let n = n.min(len);
        let (depth, acts) = (self.depth, self.acts);
        let a = &mut self.redo.st().arm;
        (a.fresh, a.depth, a.acts, a.rip, a.len, a.n, a.then, a.walk) = (true, depth, acts, rip, len, n as u8, Then::Again, Walk::default());
        // The few operands most ops have copied one by one, too few for a block copy to pay.
        let s = &self.stack;
        match n {
            0 => {}
            1 => a.ops[0] = s[len - 1],
            2 => (a.ops[0], a.ops[1]) = (s[len - 2], s[len - 1]),
            _ => a.ops[..n].copy_from_slice(&s[len - n..]),
        }
        self.redo.on = true;
        true
    }

    /* Finds where the calls of the op `redo_arm_quick` armed begin in the log, past those of the ops under it. */
    #[cold]
    #[inline(never)]
    fn redo_settle(&mut self) {
        let acts = self.acts;
        let st = self.redo.st();
        if !st.arm.fresh { return; }
        // A frame a call opened that an error unwound past left what its op had.
        while let Some(s) = st.saved.last() && s.acts >= acts { st.log.truncate(s.start); st.saved.pop(); }
        st.arm.start = st.saved.last().map_or(0, |s| s.end);
        st.log.truncate(st.arm.start);
        (st.arm.pos, st.arm.fresh) = (st.arm.start, false);
    }

    /* Whether one of the `n` values on top of the stack is a user instance or class, whose op may run its dunders. */
    #[inline(always)]
    pub(crate) fn redo_top(&self, n: usize) -> bool {
        let len = self.stack.len();
        if !self.heap.users { return false; }
        match n.min(len) {
            0 => false,
            1 => self.heap.user(self.stack[len - 1]),
            2 => self.heap.user(self.stack[len - 2]) || self.heap.user(self.stack[len - 1]),
            k => self.redo_calls(k),
        }
    }

    /* Whether one of the `k` values on top of the stack is a user instance or class. */
    #[inline(never)]
    fn redo_calls(&self, k: usize) -> bool { self.stack[self.stack.len() - k..].iter().any(|&v| self.heap.user(v)) }


    /* Readies the call op at `rip`, its stack `len` long with `ops` on top, for the builtin it calls to arm once a call it makes needs it, one running again arming at once. */
    #[inline(never)]
    pub(crate) fn redo_defer(&mut self, rip: usize, rerun: Rerun, len: usize, ops: &[Val]) -> Defer {
        let n = ops.len();
        if rerun == Rerun::No || self.redo.on || self.redo.lazy || n > OPS { return Defer::No; }
        if self.redo.keep { return if self.redo_late(rip, rerun, len, ops) { Defer::Yes } else { Defer::No }; }
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        hold(&mut st.held, 0, ops);
        st.lazy = Lazy { rip, rerun, len, n: n as u8, depth, acts };
        self.redo.lazy = true;
        Defer::Yes
    }

    /* `redo_defer` for an op whose `n` operands lie on top of the stack, which its builtin takes off, so they are copied. */
    #[inline(never)]
    pub(crate) fn redo_defer_top(&mut self, rip: usize, n: usize) -> Defer {
        let len = self.stack.len();
        if n > len || n > OPS { return Defer::No; }
        let mut ops = [Val::undef(); OPS];
        ops[..n].copy_from_slice(&self.stack[len - n..]);
        self.redo_defer(rip, Rerun::Stack, len, &ops[..n])
    }

    /* `redo_defer` of the operands `obj`, `pos` and `kw`, copied straight where the op keeps them. */
    #[inline(never)]
    pub(crate) fn redo_defer_held(&mut self, rip: usize, rerun: Rerun, len: usize, obj: Val, pos: &[Val], kw: &[Val]) -> Defer {
        let n = 1 + pos.len() + kw.len();
        if rerun == Rerun::No || self.redo.on || self.redo.lazy || n > OPS { return Defer::No; }
        if self.redo.keep {
            let mut ops = [Val::undef(); OPS];
            ops[0] = obj;
            ops[1..1 + pos.len()].copy_from_slice(pos);
            ops[1 + pos.len()..n].copy_from_slice(kw);
            return if self.redo_late(rip, rerun, len, &ops[..n]) { Defer::Yes } else { Defer::No };
        }
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        st.held[0] = obj;
        hold(&mut st.held, 1, pos);
        hold(&mut st.held, 1 + pos.len(), kw);
        st.lazy = Lazy { rip, rerun, len, n: n as u8, depth, acts };
        self.redo.lazy = true;
        Defer::Yes
    }

    /* Ends what `redo_defer` began. */
    #[inline(always)]
    pub(crate) fn redo_undefer(&mut self, d: Defer) { if d == Defer::Yes { (self.redo.lazy, self.redo.on) = (false, false); } }

    /* Arms the builtin's op `redo_defer` readied for the call it makes now, false when it makes it from a deeper frame. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_wake(&mut self) -> bool {
        if !self.redo.lazy { return false; }
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        let l = st.lazy;
        if l.depth != depth || l.acts != acts { return false; }
        let ops = st.held;
        self.redo.lazy = false;
        self.redo_late(l.rip, l.rerun, l.len, &ops[..l.n as usize])
    }

    /* Whether a `%` format of the tuple `t` may run Python code, its items user instances whose conversions it calls. */
    #[inline(never)]
    pub(crate) fn redo_fmt(&self, t: Val) -> bool {
        matches!(self.heap.try_get(t), Some(HeapObj::Tuple(items)) if items.iter().any(|&v| self.heap.user(v)))
    }

    /* Runs `f`, the handler of the op at `rip` with `n` operands on the stack, as one whose calls go to the loop when one of them may call. */
    #[inline(always)]
    pub(crate) fn redone<R>(&mut self, rip: usize, n: usize, f: impl FnOnce(&mut Self) -> Result<R, VmErr>) -> Result<R, VmErr> {
        // Before the program makes a class with dunders no op calls one.
        let armed = self.heap.users && self.redo_enter(rip, n);
        let r = f(self);
        self.redo_done(armed);
        r
    }

    /* `redo_enter` for an op the dispatch's generic arm runs, with as many operands as `redo_arity` reads off it. */
    #[inline(never)]
    pub(crate) fn redo_enter_op(&mut self, op: crate::parser::OpCode, arg: u16, rip: usize) -> bool {
        use crate::parser::OpCode::*;
        let Some(n) = super::dispatch::redo_arity(op, arg) else { return false };
        // A key a class hashes may lie inside a tuple, which the container hashes too.
        let keys = matches!(op, SetAdd | MapAdd | BuildSet | BuildDict) && n <= self.stack.len() && self.user_keys(&self.stack[self.stack.len() - n..]);
        if keys { self.redo_begin(true, rip, n) } else { self.redo_enter(rip, n) }
    }

    /* Arms the op at `rip` when its top `n` operands may call, out of line so the ops that use `redone` stay small. */
    #[inline(never)]
    fn redo_enter(&mut self, rip: usize, n: usize) -> bool {
        let go = self.redo_top(n);
        self.redo_begin(go, rip, n)
    }

    /* `redone` for an op whose operands `go` says may call, or that runs again. */
    #[inline(always)]
    pub(crate) fn redone_if<R>(&mut self, go: bool, rip: usize, n: usize, f: impl FnOnce(&mut Self) -> Result<R, VmErr>) -> Result<R, VmErr> {
        let armed = self.redo_begin(go, rip, n);
        let r = f(self);
        self.redo_done(armed);
        r
    }

    /* Arms the op at `rip` as `redone_if` does, true when this armed it, for `redo_done` to end. */
    #[inline(always)]
    pub(crate) fn redo_begin(&mut self, go: bool, rip: usize, n: usize) -> bool { (go || self.redo.keep) && self.redo_arm_quick(rip, n) }

    #[inline(always)]
    pub(crate) fn redo_done(&mut self, armed: bool) { if armed { self.redo.on = false; } }

    /* `redone` for an op that, when the one call its handler makes gives a value, takes it as `then` says instead of running again. */
    #[inline(always)]
    pub(crate) fn redone_to<R>(&mut self, rip: usize, n: usize, then: Then, f: impl FnOnce(&mut Self) -> Result<R, VmErr>) -> Result<R, VmErr> {
        let armed = self.redo_arm_quick(rip, n);
        if armed { self.redo.st().arm.then = then; }
        let r = f(self);
        self.redo_done(armed);
        r
    }

    /* Makes `t` what the op running now makes of its handler's first call, for a stack op whose operands the call's value replaces. */
    #[inline(always)]
    pub(crate) fn redo_then(&mut self, t: Then) {
        if self.redo.on && let Some(st) = self.redo.st.as_deref_mut() && st.arm.n > 0 && st.arm.pos == st.arm.start { st.arm.then = t; }
    }

    /* Whether the op about to run again takes the calls its handler made before, so no frame opens for them but through it. */
    #[inline(always)]
    pub(crate) fn redo_replays(&self) -> bool {
        self.redo.keep || self.redo.on && self.redo.st.as_deref().is_some_and(|st| !st.arm.fresh && st.arm.pos < st.log.len())
    }

    /* What the call of `callee` the running handler makes gave when it ran before, None for a first call. */
    #[inline(always)]
    pub(crate) fn redo_taken(&mut self, callee: Val) -> Option<Result<Val, VmErr>> {
        if !self.redo.on { return None; }
        self.redo_take(callee)
    }

    #[cold]
    #[inline(never)]
    fn redo_take(&mut self, callee: Val) -> Option<Result<Val, VmErr>> {
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        let a = &mut st.arm;
        if a.fresh || a.pos >= st.log.len() || a.depth != depth || a.acts != acts { return None; }
        let d = &st.log[a.pos];
        // A call it makes as it is, a generator's say, gives nothing to take.
        if d.func.0 != callee.0 { return None; }
        a.pos += 1;
        Some(match d.v.clone() { Ok(v) => Ok(v), Err((e, exc)) => { self.pending.exc_val = exc; Err(e) } })
    }

    /* What the dunder `name` of `recv` the running handler calls gave when it ran before, None for a first call, taken before the dunder is looked up again. */
    #[inline(always)]
    pub(crate) fn redo_dunder(&mut self, recv: Val, name: &str) -> Option<Result<Val, VmErr>> {
        if !self.redo.on { return None; }
        self.redo_dunder_take((recv.0, name.as_ptr() as usize, name.len()))
    }

    #[cold]
    #[inline(never)]
    fn redo_dunder_take(&mut self, key: Key) -> Option<Result<Val, VmErr>> {
        // A job's handler takes its conversions where its dunder is found.
        if self.redo.st.as_deref().is_some_and(|st| st.conv.is_some()) { return None; }
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        let a = &mut st.arm;
        if a.fresh || a.pos >= st.log.len() || a.depth != depth || a.acts != acts { return None; }
        let d = &st.log[a.pos];
        if d.key != Some(key) { return None; }
        a.pos += 1;
        Some(match d.v.clone() { Ok(v) => Ok(v), Err((e, exc)) => { self.pending.exc_val = exc; Err(e) } })
    }

    /* Opens the first call of `fi` the running handler makes as a frame of the loop, the op set back as it began, None when the call runs nested. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_call(&mut self, fi: usize, callee: Val) -> Option<Result<Val, VmErr>> {
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st.as_deref_mut()?;
        if st.conv.is_some() || st.arm.depth != depth || st.arm.acts != acts { return None; }
        if st.arm.fresh {
            // A frame a call opened that an error unwound past left what its op had.
            while let Some(s) = st.saved.last() && s.acts >= acts { st.log.truncate(s.start); st.saved.pop(); }
            st.arm.start = st.saved.last().map_or(0, |s| s.end);
            (st.arm.pos, st.arm.fresh) = (st.arm.start, false);
        }
        if st.arm.pos - st.arm.start >= CALLS { return None; }
        let pool = self.fn_pool[fi];
        if self.pools[pool].code.is_none() { self.cold_code(fi); }
        if self.pools[pool].code.is_none() || self.pools[pool].consts.is_none() { return None; }
        // SAFETY made when the op armed.
        let st = unsafe { self.redo.st.as_deref_mut().unwrap_unchecked() };
        // State the calls changed took the handler elsewhere, so what came after is made again.
        st.log.truncate(st.arm.pos);
        let a = &st.arm;
        let (len, n, rip, start) = (a.len, a.n as usize, a.rip, a.start);
        let then = if a.pos == start { a.then } else { Then::Again };
        self.stack.truncate(len - n);
        self.stack.extend_from_slice(&a.ops[..n]);
        self.redo.on = false;
        if !self.redo_block(fi, callee, rip) {
            if let Err(e) = self.enter_frame(fi, callee) { return Some(Err(e)); }
            self.mark_dunder(rip, REDO);
        }
        let (before, iters, withs, acts) = (self.pending.exc_val, self.iter_stack.len(), self.with_stack.len(), self.acts - 1);
        // SAFETY as above.
        let st = unsafe { self.redo.st.as_deref_mut().unwrap_unchecked() };
        if st.saved.capacity() == 0 { st.saved.reserve_exact(2); }
        let saved = Saved { acts, start, end: st.log.len(), before, stack: len, n, iters, withs, key: st.hint.take(), then, walk: st.arm.walk };
        st.saved.push(saved);
        Some(Err(VmErr::Redo))
    }

    /* Readies the op running now, its stack as it began, for a job its handler opens in place of its next call and that runs the op again once it ends, giving the op's index or None. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_job(&mut self) -> Option<usize> {
        if !(self.redo.on || self.redo.lazy && self.redo_wake()) { return None; }
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st.as_deref_mut()?;
        if st.conv.is_some() || st.arm.depth != depth || st.arm.acts != acts { return None; }
        if st.arm.fresh {
            while let Some(s) = st.saved.last() && s.acts >= acts { st.log.truncate(s.start); st.saved.pop(); }
            st.arm.start = st.saved.last().map_or(0, |s| s.end);
            (st.arm.pos, st.arm.fresh) = (st.arm.start, false);
        }
        st.log.truncate(st.arm.pos);
        let a = st.arm;
        let (len, n) = (a.len, a.n as usize);
        self.stack.truncate(len - n);
        self.stack.extend_from_slice(&a.ops[..n]);
        self.redo.on = false;
        let (before, iters, withs) = (self.pending.exc_val, self.iter_stack.len(), self.with_stack.len());
        // SAFETY made when the op armed.
        let st = unsafe { self.redo.st.as_deref_mut().unwrap_unchecked() };
        st.saved.push(Saved { acts, start: a.start, end: st.log.len(), before, stack: len, n, iters, withs, key: None, then: Then::Again, walk: a.walk });
        Some(a.rip)
    }

    /* Whether the op armed now runs again after a call of its own, rather than afresh. */
    #[inline]
    pub(crate) fn redo_again(&self) -> bool { self.redo.on && self.redo.st.as_deref().is_some_and(|st| !st.arm.fresh) }

    /* Runs the op at `rip` again once the job `redo_job` readied for it ended, taking the calls it made before the job. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_rerun(&mut self, rip: usize) {
        let (acts, depth) = (self.acts, self.depth);
        let st = self.redo.st();
        if !st.saved.last().is_some_and(|s| s.acts == acts) { return; }
        let Some(Saved { start, end, walk, .. }) = st.saved.pop() else { return };
        st.log.truncate(end);
        let a = &mut st.arm;
        (a.rip, a.acts, a.depth, a.start, a.walk) = (rip, acts, depth, start, walk);
        self.redo.keep = true;
    }

    /* Opens the warm body of `fi` as a block of the loop, the op at `rip` running again once it returns, false with its window bound again when the body takes a frame. */
    #[inline(never)]
    fn redo_block(&mut self, fi: usize, callee: Val, rip: usize) -> bool {
        let f = &self.call_facts[fi];
        let (pool, arity) = (f.pool, f.arity);
        let Some(k) = arity else { return false };
        if k > 8 || !f.in_loop || f.closure { return false; }
        // A body below its tier opens its frame as a first call does, one past it is lowered as its frame would lower it.
        if self.pools[pool].calls < super::lower::TIER { return false; }
        if self.pools[pool].cold || !self.pools[pool].laid { self.relower(fi); }
        let p = &self.pools[pool];
        if p.cold || p.code.is_none() || p.consts.is_none() { return false; }
        let base = p.window;
        let mut args = [Val::undef(); 8];
        args[..k].copy_from_slice(&self.regs[base..base + k]);
        self.close_window(pool);
        let link = rip as i64 | (REDO as i64) << 24 | super::act::DUNDER << 40;
        let binding = self.pending.method_binding.filter(|_| self.uses_super(fi));
        let opened = match binding { Some(b) => self.redo_block_bound(fi, callee, &args[..k], link, b), None => self.open_block(fi, callee, &args[..k], link).is_some() };
        if opened {
            (self.pending.call_ip, self.pending.method_binding) = (None, None);
            return true;
        }
        let names = self.functions[fi].1.names.len();
        let base = self.open_window(pool, names + self.functions[fi].1.constants.len() + 3, names, true);
        self.regs[base..base + k].copy_from_slice(&args[..k]);
        false
    }

    /* `open_block` of a body naming `super` with the binding `(class, recv)` its call made, kept while the block runs. */
    #[inline(never)]
    fn redo_block_bound(&mut self, fi: usize, callee: Val, args: &[Val], link: i64, (class, recv): (Val, Val)) -> bool {
        if self.open_block_as(fi, callee, args, link | super::act::BOUND << 40, true).is_none() { return false; }
        self.bindings.push((self.level(), class, recv));
        true
    }

    /* A call the handler of the op at `ret` made ended as `v`, which the op run again takes, giving where the caller carries on. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_back(&mut self, func: Val, ret: usize, v: Result<Val, VmErr>) -> usize {
        let st = self.redo.st();
        let Some(Saved { start, end, before, stack, n, iters, withs, key, then, walk, .. }) = st.saved.pop() else { return ret };
        st.log.truncate(end);
        let done = match (&v, then) {
            (_, Then::Again) | (Err(_), _) => false,
            (&Ok(v), Then::Done) => !self.heap.is_not_implemented(v),
            (&Ok(v), Then::Not) => !self.heap.is_not_implemented(v) && self.instance_class(v).is_none(),
            _ => true,
        };
        if done && let Ok(v) = v {
            self.redo.st().log.truncate(start);
            match then {
                Then::Push => self.stack.push(v),
                Then::Reg(r) => self.regs[self.base + r as usize] = v,
                Then::Global(m, slot) => self.scopes[m as usize].set_at(slot, v),
                Then::Done => { self.stack.truncate(stack - n); self.stack.push(v); }
                Then::Not => { self.stack.truncate(stack - n); let t = !self.truthy(v); self.stack.push(Val::bool(t)); }
                Then::Again => {}
            }
            return ret + 1;
        }
        // An op whose call raised runs again as it began, the exception the call left given back as it takes the error.
        let done = match v {
            Ok(v) => Ok(v),
            Err(e) => {
                let left = core::mem::replace(&mut self.pending.exc_val, before);
                self.stack.truncate(stack);
                self.iter_stack.truncate(iters);
                self.with_stack.truncate(withs);
                Err((e, left))
            }
        };
        let (acts, depth) = (self.acts, self.depth);
        let st = self.redo.st();
        if st.log.capacity() == 0 { st.log.reserve_exact(3); }
        st.log.push(Done { func, key, v: done });
        let a = &mut st.arm;
        (a.rip, a.acts, a.depth, a.start, a.walk) = (ret, acts, depth, start, walk);
        self.redo.keep = true;
        ret
    }

    /* Notes that the handler running now passed over `func`, whose frame of the loop declined, as a call that gave NotImplemented. */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_declined(&mut self, func: Val, ni: Val) {
        if !self.redo.on { return; }
        self.redo_settle();
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        let a = &mut st.arm;
        if a.depth != depth || a.acts != acts { return; }
        // Run again, the handler passes over it again.
        if st.log.get(a.pos).is_some_and(|d| d.func.0 == func.0) { a.pos += 1; return; }
        st.log.truncate(a.pos);
        st.log.push(Done { func, key: None, v: Ok(ni) });
        a.pos += 1;
    }

    /* Runs `f`, a builtin's handler a job runs again, its dunder calls answered in order from `conv`, Ok(Err) with the first unmade one's value and conversion for the job to run as a frame. */
    pub(crate) fn conv_run<R>(&mut self, conv: &mut Option<Box<Conv>>, f: impl FnOnce(&mut Self) -> Result<R, VmErr>) -> Result<Result<R, (Val, Check)>, VmErr> {
        let (on, lazy) = (self.redo.on, self.redo.lazy);
        let mut c = conv.take().unwrap_or_else(|| Box::new(Conv::new()));
        (c.pos, c.req) = (0, None);
        self.redo.st().conv = Some(c);
        (self.redo.on, self.redo.lazy) = (true, false);
        let r = f(self);
        (self.redo.on, self.redo.lazy) = (on, lazy);
        let c = self.redo.st().conv.take();
        let req = c.as_ref().and_then(|c| c.req);
        *conv = c;
        match (r, req) {
            (Err(VmErr::Redo), Some(q)) => Ok(Err(q)),
            (r, _) => r.map(Ok),
        }
    }

    /* The dunder `name` of `recv` that a job's handler run again calls with `args`, what it gave before or the handler stopped for the job to run it as a frame, None when no conversion names it. */
    #[cold]
    #[inline(never)]
    pub(crate) fn conv_dunder(&mut self, recv: Val, name: &str, args: &[Val]) -> Option<Result<Val, VmErr>> {
        let c = self.redo.st.as_deref_mut().and_then(|st| st.conv.as_deref_mut())?;
        if let Some(&v) = c.log.get(c.pos) { c.pos += 1; return Some(Ok(v)); }
        let check = match (name, args.first().copied()) {
            ("__getitem__", Some(a)) => Check::GetItem(a),
            ("__missing__", Some(a)) => Check::Missing(a),
            ("__format__", Some(a)) => Check::Format(a),
            ("__str__", None) => Check::Str(false),
            ("__repr__", None) => Check::Str(true),
            _ => return None,
        };
        c.req = Some((recv, check));
        Some(Err(VmErr::Redo))
    }

    /* Notes what the conversion the handler of a job asked for last gave, which it takes as it runs again. */
    pub(crate) fn conv_took(conv: &mut Option<Box<Conv>>, v: Val) { conv.get_or_insert_with(|| Box::new(Conv::new())).log.push(v); }

    /* The values a job's conversions noted, which a collection keeps. */
    pub(crate) fn conv_vals(conv: &Option<Box<Conv>>, f: &mut dyn FnMut(Val)) { if let Some(c) = conv { for &v in &c.log { f(v) } } }

    /* The pairs of its walk the running op found equal before, the log taken past their calls, none for an op running for the first time. */
    pub(crate) fn redo_walk(&mut self) -> usize {
        if !self.redo.on { return 0; }
        self.redo_settle();
        let a = &mut self.redo.st().arm;
        if a.walk.done == 0 { return 0; }
        a.pos = a.walk.pos as usize;
        a.walk.done as usize
    }

    /* Notes that the walk of the op running now found its first `done` pairs equal, the calls they made ending where the log is. */
    pub(crate) fn redo_walked(&mut self, done: usize) {
        if !self.redo.on { return; }
        let a = &mut self.redo.st().arm;
        // A log too long to note where it got walks again from its first pair.
        a.walk = match (u16::try_from(done), u16::try_from(a.pos)) { (Ok(done), Ok(pos)) => Walk { done, pos }, _ => Walk::default() };
    }

    /* Keeps the dunder `name` of `recv` a handler calls now, so its call notes which it was. */
    #[inline(always)]
    pub(crate) fn redo_hint(&mut self, recv: Val, name: &str) { if self.redo.on { self.redo.set_hint(Some((recv.0, name.as_ptr() as usize, name.len()))); } }
}
