use alloc::{boxed::Box, vec::Vec};

use super::VM;
use super::act::REDO;
use crate::value::*;
use crate::r#unsafe::known;

/* an op calling user code reruns after its frame replaying results so none runs twice */
pub(crate) struct Redo {
    // an op's handler hands its calls to the loop now
    pub(crate) on: bool,
    // a builtin runs that arms once its calls need it and `st` holds its op
    pub(crate) lazy: bool,
    // the op runs again with its last call just back
    pub(crate) keep: bool,
    // the rest made at the first arming so a program that never arms pays nothing
    st: Option<Box<State>>,
}

/* what an op handing its calls to the loop keeps between them */
struct State {
    // the op whose handler runs now
    arm: Arm,
    // the dispatching call op and its rerun so a builtin it calls can arm it
    site: (u32, Rerun),
    // what the calls of running or waiting ops gave in order with outer ops first
    log: Vec<Done>,
    // one per frame such a call opened with the innermost last
    saved: Vec<Saved>,
    // the method a handler calls by instance and name so a rerun finds its call
    hint: Option<Key>,
    // the op of the running builtin that arms lazily with its operands in `held`
    lazy: Lazy,
    held: [Val; OPS],
}

/* an instance and the name of a method called on it */
pub(crate) type Key = (u64, usize, usize);

/* an op whose handler may hand its calls to the loop and its saved operands */
#[derive(Clone, Copy)]
pub(crate) struct Arm { fresh: bool, depth: usize, acts: usize, rip: usize, len: usize, ops: [Val; OPS], n: u8, start: usize, pos: usize }

/* a builtin's op armed only once a call it makes needs it */
#[derive(Clone, Copy)]
pub(crate) struct Lazy { rip: usize, rerun: Rerun, len: usize, n: u8, depth: usize, acts: usize }

/* what `redo_defer_held` did for `redo_undefer` to end */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Defer { No, Yes }

/* how an op reruns if at all with its operands restacked or left in registers */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Rerun { No, Stack, Regs }

/* each handed call replays the ones before so past this many calls run in place */
pub(crate) const CALLS: usize = 64;

/* the most operands an op handing its calls to the loop keeps */
pub(crate) const OPS: usize = 8;

/* a frame an op's handler call opened with what an error out of it restores */
struct Saved { acts: usize, start: usize, end: usize, before: Option<Val>, stack: usize, iters: usize, key: Option<Key> }

/* a call an op's handler made with its function and method and what it gave */
struct Done { func: Val, key: Option<Key>, v: Result<Val, (VmErr, Option<Val>)> }

/* copies `vals` into `held` from `at` singly since so few never repay a block copy */
#[inline(always)]
fn hold(held: &mut [Val; OPS], at: usize, vals: &[Val]) {
    let mut k = 0;
    while k < vals.len() && at + k < OPS { held[at + k] = vals[k]; k += 1; }
}

impl Redo {
    pub(crate) fn new() -> Self { Redo { on: false, lazy: false, keep: false, st: None } }

    /* marks what the ops' calls gave which they take running again */
    pub(crate) fn mark(&self, heap: &mut HeapPool) {
        let Some(st) = &self.st else { return };
        if self.lazy { for &v in &st.held { heap.mark(v); } }
        for d in &st.log {
            heap.mark(d.func);
            match d.v { Ok(v) | Err((_, Some(v))) => heap.mark(v), Err((_, None)) => {} }
        }
        for v in st.saved.iter().filter_map(|s| s.before) { heap.mark(v); }
        for &v in &st.arm.ops { heap.mark(v); }
    }

    /* what the ops keep made on the first that arms */
    #[inline(always)]
    fn st(&mut self) -> &mut State {
        if self.st.is_none() { self.make(); }
        // made just above when it was not
        known(self.st.as_deref_mut())
    }

    #[cold]
    #[inline(never)]
    fn make(&mut self) {
        let arm = Arm { fresh: false, depth: 0, acts: 0, rip: 0, len: 0, ops: [Val::undef(); OPS], n: 0, start: 0, pos: 0 };
        let lazy = Lazy { rip: 0, rerun: Rerun::No, len: 0, n: 0, depth: 0, acts: 0 };
        self.st = Some(Box::new(State { arm, site: (0, Rerun::No), log: Vec::new(), saved: Vec::new(), hint: None, lazy, held: [Val::undef(); OPS] }));
    }

    /* notes the dispatching call op so a builtin it calls can arm it */
    #[inline(never)]
    pub(crate) fn set_site(&mut self, site: (u32, Rerun)) { self.st().site = site; }

    /* takes the call op `set_site` noted or none when no call noted one */
    #[inline(always)]
    pub(crate) fn take_site(&mut self) -> (u32, Rerun) {
        match self.st.as_deref_mut() { Some(st) => core::mem::replace(&mut st.site, (0, Rerun::No)), None => (0, Rerun::No) }
    }

    /* ends the general call `set_site` noted */
    #[inline(always)]
    pub(crate) fn end_site(&mut self) { if let Some(st) = self.st.as_deref_mut() { st.site.1 = Rerun::No; } }

    /* keeps the method a handler calls now for its call to note */
    #[inline(always)]
    pub(crate) fn set_hint(&mut self, key: Option<Key>) { if let Some(st) = self.st.as_deref_mut() { st.hint = key; } }
}

impl<'a> VM<'a> {
    /* arms op `rip` over `n` stacked operands or false when an outer op already armed */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_arm(&mut self, rip: usize, n: usize) -> bool {
        // an op of more operands than it keeps makes its calls nested
        if self.redo.on || n > OPS { return false; }
        let len = self.sp();
        let n = n.min(len);
        let mut ops = [Val::undef(); OPS];
        ops[..n].copy_from_slice(self.vals_at(len - n, n));
        self.redo_arm_at(rip, len, &ops[..n])
    }

    /* arms op `rip` from its handler per `rerun` keeping `ops` for stack ops alone */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_late(&mut self, rip: usize, rerun: Rerun, len: usize, ops: &[Val]) -> bool {
        match rerun {
            Rerun::No => false,
            Rerun::Stack => self.redo_arm_at(rip, len, ops),
            Rerun::Regs => self.redo_arm_at(rip, len - ops.len(), &[]),
        }
    }

    /* `redo_arm` of the op whose stack began `len` long with `ops` on top */
    fn redo_arm_at(&mut self, rip: usize, len: usize, ops: &[Val]) -> bool {
        let n = ops.len();
        if self.redo.on || n > OPS { return false; }
        if !self.redo.keep {
            let (depth, acts) = (self.depth, self.acts);
            let a = &mut self.redo.st().arm;
            (a.fresh, a.depth, a.acts, a.rip, a.len, a.n) = (true, depth, acts, rip, len, n as u8);
            a.ops[..n].copy_from_slice(ops);
            self.redo.on = true;
            return true;
        }
        let keep = core::mem::take(&mut self.redo.keep);
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        while let Some(s) = st.saved.last() && s.acts >= acts { st.log.truncate(s.start); st.saved.pop(); }
        let a = &mut st.arm;
        // a rerun op keeps where its calls begin and another's begin past the outer ops
        if !keep || a.rip != rip || a.acts != acts || a.depth != depth {
            a.start = st.saved.last().map_or(0, |s| s.end);
            st.log.truncate(a.start);
        }
        (a.fresh, a.pos, a.depth, a.acts, a.rip, a.len, a.n) = (false, a.start, depth, acts, rip, len, n as u8);
        a.ops[..n].copy_from_slice(ops);
        self.redo.on = true;
        true
    }

    /* `redo_arm` for an op that never reruns finding its log start at its first call */
    #[inline(always)]
    fn redo_arm_quick(&mut self, rip: usize, n: usize) -> bool {
        if self.redo.on { return false; }
        if self.redo.keep || n > OPS { return self.redo_arm(rip, n); }
        let len = self.sp();
        let n = n.min(len);
        let (depth, acts) = (self.depth, self.acts);
        let a = &mut self.redo.st().arm;
        (a.fresh, a.depth, a.acts, a.rip, a.len, a.n) = (true, depth, acts, rip, len, n as u8);
        // most ops' few operands copy singly since a block copy would not pay
        let s = &self.regs[self.base..];
        match n {
            0 => {}
            1 => a.ops[0] = s[len - 1],
            2 => (a.ops[0], a.ops[1]) = (s[len - 2], s[len - 1]),
            _ => a.ops[..n].copy_from_slice(&s[len - n..len]),
        }
        self.redo.on = true;
        true
    }

    /* readies op `rip` with operands `obj` `pos` and `kw` for its builtin to arm later */
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

    /* ends what `redo_defer_held` began */
    #[inline(always)]
    pub(crate) fn redo_undefer(&mut self, d: Defer) { if d == Defer::Yes { (self.redo.lazy, self.redo.on) = (false, false); } }

    /* arms the op `redo_defer_held` readied for its call now and false from a deeper frame */
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

    /* arms op `rip` and gives true when this armed it for `redo_done` to end */
    #[inline(always)]
    pub(crate) fn redo_begin(&mut self, go: bool, rip: usize, n: usize) -> bool { (go || self.redo.keep) && self.redo_arm_quick(rip, n) }

    #[inline(always)]
    pub(crate) fn redo_done(&mut self, armed: bool) { if armed { self.redo.on = false; } }

    /* whether a rerunning op takes its handler's earlier calls so they open no frame anew */
    #[inline(always)]
    pub(crate) fn redo_replays(&self) -> bool {
        self.redo.keep || self.redo.on && self.redo.st.as_deref().is_some_and(|st| !st.arm.fresh && st.arm.pos < st.log.len())
    }

    /* what the handler's call of `callee` gave before or None for a first call */
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
        // a call it makes as it is like a generator's gives nothing to take
        if d.func.0 != callee.0 { return None; }
        a.pos += 1;
        Some(match d.v.clone() { Ok(v) => Ok(v), Err((e, exc)) => { self.pending.exc_val = exc; Err(e) } })
    }

    /* what method `name` of `recv` gave before or None checked ahead of a fresh lookup */
    #[inline(always)]
    pub(crate) fn redo_dunder(&mut self, recv: Val, name: &str) -> Option<Result<Val, VmErr>> {
        if !self.redo.on { return None; }
        self.redo_dunder_take((recv.0, crate::lang::fnv(name) as usize, name.len()))
    }

    #[cold]
    #[inline(never)]
    fn redo_dunder_take(&mut self, key: Key) -> Option<Result<Val, VmErr>> {
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st();
        let a = &mut st.arm;
        if a.fresh || a.pos >= st.log.len() || a.depth != depth || a.acts != acts { return None; }
        let d = &st.log[a.pos];
        if d.key != Some(key) { return None; }
        a.pos += 1;
        Some(match d.v.clone() { Ok(v) => Ok(v), Err((e, exc)) => { self.pending.exc_val = exc; Err(e) } })
    }

    /* the handler's call of `fi` as a loop frame or None so it runs nested */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_call(&mut self, fi: usize, callee: Val) -> Option<Result<Val, VmErr>> {
        let (depth, acts) = (self.depth, self.acts);
        let st = self.redo.st.as_deref_mut()?;
        if st.arm.depth != depth || st.arm.acts != acts { return None; }
        if st.arm.fresh {
            // a frame a call opened that an error unwound past left what its op had
            while let Some(s) = st.saved.last() && s.acts >= acts { st.log.truncate(s.start); st.saved.pop(); }
            st.arm.start = st.saved.last().map_or(0, |s| s.end);
            (st.arm.pos, st.arm.fresh) = (st.arm.start, false);
        }
        if st.arm.pos - st.arm.start >= CALLS { return None; }
        let pool = self.fn_pool[fi];
        if self.pools[pool].code.is_none() { self.cold_code(fi); }
        if self.pools[pool].code.is_none() || self.pools[pool].consts.is_none() { return None; }
        // made when the op armed
        let st = known(self.redo.st.as_deref_mut());
        // state the calls changed took the handler elsewhere so what came after is made again
        st.log.truncate(st.arm.pos);
        let a = &st.arm;
        let (len, n, rip, start) = (a.len, a.n as usize, a.rip, a.start);
        let ops = a.ops;
        // the op's operands lie back in its rows as it began
        self.cut_to(len - n);
        for &v in &ops[..n] { self.push(v); }
        self.redo.on = false;
        if !self.redo_block(fi, callee, rip) {
            if let Err(e) = self.enter_frame(fi, callee) { return Some(Err(e)); }
            self.mark_dunder(rip, REDO);
        }
        let (before, iters, acts) = (self.pending.exc_val, self.iter_stack.len(), self.acts - 1);
        let st = known(self.redo.st.as_deref_mut());
        if st.saved.capacity() == 0 { st.saved.reserve_exact(2); }
        let saved = Saved { acts, start, end: st.log.len(), before, stack: len, iters, key: st.hint.take() };
        st.saved.push(saved);
        Some(Err(VmErr::Redo))
    }

    /* opens warm `fi` as a block whose return reruns `rip` or false for a frame */
    #[inline(never)]
    fn redo_block(&mut self, fi: usize, callee: Val, rip: usize) -> bool {
        let f = &self.call_facts[fi];
        let (pool, arity) = (f.pool, f.arity);
        let Some(k) = arity else { return false };
        if k > 8 || !f.in_loop || f.closure { return false; }
        // below its tier a body takes a first call's frame and past it is relowered
        if self.pools[pool].calls < super::lower::TIER { return false; }
        if self.pools[pool].cold || !self.pools[pool].laid { self.relower(fi); }
        let p = &self.pools[pool];
        if p.cold || p.code.is_none() || p.consts.is_none() { return false; }
        let base = p.window;
        let mut args = [Val::undef(); 8];
        args[..k].copy_from_slice(&self.regs[base..base + k]);
        self.close_window(pool);
        let link = rip as i64 | (REDO as i64) << 24 | super::act::DUNDER << 40;
        if self.open_block(fi, callee, &args[..k], link).is_some() {
            self.pending.call_ip = None;
            return true;
        }
        let names = self.functions[fi].1.names.len();
        let base = self.open_window(pool, names + self.functions[fi].1.constants.len() + 3, names, true);
        self.regs[base..base + k].copy_from_slice(&args[..k]);
        false
    }

    /* a call of op `ret`'s handler ended as `v` which the rerun op takes */
    #[cold]
    #[inline(never)]
    pub(crate) fn redo_back(&mut self, func: Val, ret: usize, v: Result<Val, VmErr>) -> usize {
        let st = self.redo.st();
        let Some(Saved { start, end, before, stack, iters, key, .. }) = st.saved.pop() else { return ret };
        st.log.truncate(end);
        // a raising call's op reruns as it began and takes the error with its exception
        let done = match v {
            Ok(v) => Ok(v),
            Err(e) => {
                let left = core::mem::replace(&mut self.pending.exc_val, before);
                self.cut_to(stack);
                self.iter_stack.truncate(iters);
                Err((e, left))
            }
        };
        let (acts, depth) = (self.acts, self.depth);
        let st = self.redo.st();
        if st.log.capacity() == 0 { st.log.reserve_exact(3); }
        st.log.push(Done { func, key, v: done });
        let a = &mut st.arm;
        (a.rip, a.acts, a.depth, a.start) = (ret, acts, depth, start);
        self.redo.keep = true;
        ret
    }

    /* keeps method `name` of `recv` a handler calls so its call notes which it was */
    #[inline(always)]
    pub(crate) fn redo_hint(&mut self, recv: Val, name: &str) { if self.redo.on { self.redo.set_hint(Some((recv.0, crate::lang::fnv(name) as usize, name.len()))); } }
}
