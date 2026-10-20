use alloc::{rc::Rc, vec::Vec};

use super::VM;
use super::lower::Code;
use super::types::*;
use super::lower::{self, Ins};
use super::sites::Site;
use super::globals::async_ops::FINISHED;
use crate::value::BodyRef;
use crate::parser::{OpCode, SSAChunk};

/* A call the loop runs in place of a nested one, kept in the `HEAD` registers before its window so a call pushes nothing, and a recursion keeps it aside. */
#[derive(Clone, Copy)]
pub(crate) struct Act {
    // The lowered code its pool holds for as long as the VM lives.
    pub(crate) code: *const Code,
    // The pool whose window it runs in.
    pub(crate) pool: usize,
    // FLAT, or 0 for one whose bases are the last of `frame_bases`.
    pub(crate) cold: u32,
    // Where the caller carries on once the call is over.
    pub(crate) ret: usize,
    // The caller register the result goes to, or NO_DEST, where a flat generator frame keeps the iterator stack's length it began at instead.
    pub(crate) dest: u16,
    pub(crate) fi: usize,
    pub(crate) callee: Val,
    // The compiler instruction that called, where the traceback and an error met in the caller point.
    pub(crate) call_ip: Option<u32>,
    pub(crate) stack_base: usize,
    pub(crate) bound: bool,
    // The instance an `__init__` call hands back in place of what it returns, undef for any other call.
    pub(crate) instance: Val,
    // The generator a `for` loop runs as this call, which a yield hands its value back from, undef for a call.
    pub(crate) generator: Val,
    // Whether the builtin on top of `jobs` takes what it returns, its caller carrying on only once the builtin is done.
    pub(crate) job: bool,
    // Whether it runs the operator dunder of the op at `ret`, whose register `dest` takes its result.
    pub(crate) dunder: bool,
}

// The registers before a window that hold its call, as its return point, the call below and its function, its callee, its stack base and its code.
pub(crate) const HEAD: usize = 5;
// A call with no call below it.
const NO_POOL: usize = 0xFF_FFFF;
pub(crate) const BOUND: i64 = 1;
pub(crate) const BASES: i64 = 2;
pub(crate) const GEN: i64 = 4;
pub(crate) const CTOR: i64 = 8;
pub(crate) const JOB: i64 = 16;
pub(crate) const DUNDER: i64 = 32;
// A coroutine the scheduler switched to, or on a plain frame a function exec defined in a dict, which hands the dict what it bound as it ends.
pub(crate) const TASK: i64 = 64;

/* The lengths of the handler, unwind, spread and iterator stacks a frame began at, each truncated back to them as it ends. */
#[derive(Clone, Copy)]
pub(crate) struct Bases { pub(crate) exc: usize, pub(crate) unwind: usize, pub(crate) delta: usize, pub(crate) iter: usize, pub(crate) with: usize }

// A frame whose bases are the stacks' lengths now, since its body never pushes on them.
pub(crate) const FLAT: u32 = u32::MAX;

/* What calling a function needs to know of its body, found once. */
#[derive(Clone, Copy)]
pub(crate) struct CallFacts {
    // Its positional count when every parameter is plain.
    pub(crate) arity: Option<usize>,
    // How many of those take no default, the fewest a call may pass.
    pub(crate) required: u16,
    // Whether it captures cells, which its frame takes off the function each call.
    pub(crate) closure: bool,
    // Whether a loop can open the body as one of its frames, its positionals filling the first slots.
    pub(crate) in_loop: bool,
    pub(crate) pool: usize,
    // The names of its body, the first registers of its window.
    pub(crate) names: u32,
    // Whether a call passing all its positionals opens it with nothing filled in, every parameter plain without a default and no cell captured.
    pub(crate) plain: bool,
    // `star_slot` and `full_slots` but for a coroutine, `NO_SLOTS` for none, found as the function is made.
    pub(crate) star: u8,
    pub(crate) full: u8,
    // `NAMED_SUPER` once `SUPER_KNOWN` tells whether the body names `super`, and `OPENS` whether a positional call may open its block in the loop, in one byte.
    pub(crate) bits: u8,
}

pub(crate) const NO_SLOTS: u8 = u8::MAX;
pub(crate) const SUPER_KNOWN: u8 = 1;
pub(crate) const NAMED_SUPER: u8 = 2;
pub(crate) const OPENS: u8 = 4;

impl CallFacts {
    /* Whether the body names `super`, None before its first look. */
    #[inline(always)]
    pub(crate) fn named_super(&self) -> Option<bool> { (self.bits & SUPER_KNOWN != 0).then_some(self.bits & NAMED_SUPER != 0) }

    #[inline(always)]
    pub(crate) fn opens(&self) -> bool { self.bits & OPENS != 0 }
}

// A frame whose caller takes its result off the stack.
pub(crate) const NO_DEST: u16 = u16::MAX;
// The dunder frame of a property getter a stack load opened, its value pushed, and of a setter, its value dropped.
pub(crate) const PUSH: u16 = u16::MAX - 1;
pub(crate) const DISCARD: u16 = u16::MAX - 2;
// The frame of a call an op's handler made, the op running again once it returns.
pub(crate) const REDO: u16 = u16::MAX - 3;

// Marks a frame's `call_ip` as the caller's own lowered index, turned into the compiler's only when an error needs it.
pub(crate) const RAW_IP: u32 = 1 << 31;

/* Where the instruction after a call wants its result, a register or the stack. */
#[inline(always)]
pub(crate) fn dest_of(next: Option<&Ins>) -> u16 {
    match next {
        Some(s) if s.op == OpCode::StoreTopR && s.x == 0 => s.a,
        _ => NO_DEST,
    }
}

/* Moves the last item of `from` onto `to`, copied once where it lands. */
#[inline]
fn move_last<T>(from: &mut Vec<T>, to: &mut Vec<T>) {
    let Some(n) = from.len().checked_sub(1) else { return };
    to.reserve(1);
    // SAFETY `n` lies in `from`, whose length drops past it so the item has one owner, and `to` has room for one more.
    unsafe {
        core::ptr::copy_nonoverlapping(from.as_ptr().add(n), to.as_mut_ptr().add(to.len()), 1);
        from.set_len(n);
        to.set_len(to.len() + 1);
    }
}

// Frames between two looks at the memory limit.
pub(crate) const DEPTH_CHECK: usize = 256;
// How deep Python code may call, as Python's default recursion limit, the module, each loop frame and each level of a recursion through sections counting one.
pub(crate) const RECURSION_LIMIT: usize = 1000;
// How many frames a coroutine may hold suspended, each resume copying them all, about as deep as Python recurses by default.
pub(crate) const SUSPENDED_DEPTH: usize = 4 * MAX_CALLS;

impl<'a> VM<'a> {
    /* Opens the body of a plain call as the next frame of the loop that made it, its window already bound. */
    #[inline(always)]
    pub(crate) fn enter_frame(&mut self, fi: usize, callee: Val) -> Result<(), VmErr> {
        if self.too_deep() {
            self.close_window(self.fn_pool[fi]);
            return Err(cold_depth());
        }
        let p = &self.pools[self.fn_pool[fi]];
        if !p.laid && (p.calls >= lower::TIER || p.active >= lower::LAY_DEPTH) { self.relower(fi); }
        let Some(code) = self.pools[self.fn_pool[fi]].code.as_ref().map(Rc::as_ptr) else { return Err(cold_runtime("a frame entered without its code")) };
        let bound = match self.pending.method_binding.take() {
            Some((class, recv)) if self.uses_super(fi) => { self.bindings.push((self.level() + 1, class, recv)); true }
            _ => false,
        };
        // SAFETY the pool holds the code for as long as the VM lives.
        let cold = self.cold_bases(unsafe { (*code).flat() });
        let (call_ip, instance) = (self.pending.call_ip.take(), core::mem::replace(&mut self.pending_instance, Val::undef()));
        self.push_act(Act {
            code, pool: self.fn_pool[fi], cold,
            ret: 0, dest: NO_DEST, fi, callee, call_ip,
            stack_base: self.stack.len(), bound, instance, generator: Val::undef(), dunder: false, job: false,
        });
        self.entered = true;
        self.yielded = true;
        Ok(())
    }

    /* Opens the block of the plain function `fi` called at `i` with all its positionals, in its pool's first window when that is free and laid for its code. */
    #[inline(always)]
    pub(crate) fn enter_block(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(*const Code, *const SSAChunk)> {
        let f = self.call_facts[fi];
        let n = args.len();
        if !f.plain || f.arity != Some(n) || self.near_limit() || args.iter().any(|v| v.is_undef()) { return self.enter_site_apart(i, code, fi, callee, args, wide); }
        debug_assert!(f.pool < self.pools.len());
        // SAFETY a function's pool is one of `pools`.
        let p = unsafe { self.pools.get_unchecked_mut(f.pool) };
        let Some(c) = p.code.as_ref().map(Rc::as_ptr) else { return self.enter_site_apart(i, code, fi, callee, args, wide) };
        let w = p.w0;
        // SAFETY the pool holds the code for as long as the VM lives.
        let (layout, frame, flat) = unsafe { ((*c).layout as usize, (*c).frame, (*c).flat()) };
        if p.active != 0 || !p.laid && p.calls >= lower::TIER || !w.owner.is_undef() || w.base == super::window::NO_WINDOW || w.code != layout || w.len < frame || !flat {
            return self.enter_block_apart(i, code, fi, callee, args, wide);
        }
        // The step pays the native work counted since the last one, as `charge_step` does.
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let p = unsafe { self.pools.get_unchecked_mut(f.pool) };
        (p.window, p.wlen, p.wcode, p.active) = (w.base, w.len, w.code, 1);
        if p.calls < lower::TIER { p.calls += 1; }
        let base = w.base;
        let next = i + 1 + wide as usize;
        let dest = dest_of(code.ins.get(next));
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let h = [
            Val::int(next as i64 | (dest as i64) << 24),
            Val::int(below as i64 | (fi as i64) << 24),
            callee,
            Val::int(self.stack.len() as i64),
            Val::int(c as usize as i64),
        ];
        debug_assert!(base + f.names as usize <= self.regs.len());
        // SAFETY the window holds its record before it and its names, the first `n` of them the parameters.
        unsafe {
            let r = self.regs.as_mut_ptr().add(base);
            core::ptr::copy_nonoverlapping(h.as_ptr(), r.sub(HEAD), HEAD);
            core::ptr::copy_nonoverlapping(args.as_ptr(), r, n);
            for k in n..f.names as usize { *r.add(k) = Val::undef(); }
        }
        (self.top_pool, self.acts, self.pool_now, self.base) = (f.pool, self.acts + 1, f.pool, base);
        Some((c, &self.functions[fi].1 as *const SSAChunk))
    }

    /* Opens the block of the plain function `fi` on `args` as `enter_block` does, `link` its record's first register, None when it takes another path. */
    #[inline(always)]
    pub(crate) fn open_block(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(*const Code, *const SSAChunk)> { self.open_block_as(fi, callee, args, link, false) }

    /* `open_block` kept out of the code that calls it, for a call that rarely takes it. */
    #[inline(never)]
    fn open_block_apart(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(*const Code, *const SSAChunk)> { self.open_block_as(fi, callee, args, link, false) }

    /* `open_block`, `super_ok` for a body that names `super`, whose binding its caller keeps. */
    #[inline(always)]
    pub(crate) fn open_block_as(&mut self, fi: usize, callee: Val, args: &[Val], link: i64, super_ok: bool) -> Option<(*const Code, *const SSAChunk)> {
        let f = self.call_facts[fi];
        let n = args.len();
        let star = f.arity.is_none() && self.full_slots(fi) == Some(n);
        let arity = if star { n } else { f.arity? };
        if n > arity || !star && n < f.required as usize || self.near_limit() || args.iter().any(|v| v.is_undef()) { return None; }
        if !star && !f.in_loop && !self.cells_only(fi) { return None; }
        if !f.plain && !super_ok && self.uses_super(fi) { return None; }
        // The cells it makes for its variables nested functions share are made first, so nothing has opened when one cannot be.
        let cells = if f.in_loop || star { Vec::new() } else { self.made_cells(fi, callee, args)? };
        let p = &self.pools[f.pool];
        if !p.laid && (p.calls >= lower::TIER || p.active >= lower::LAY_DEPTH) { self.relower(fi); }
        debug_assert!(f.pool < self.pools.len());
        // SAFETY a function's pool is one of `pools`.
        let p = unsafe { self.pools.get_unchecked_mut(f.pool) };
        let c = p.code.as_ref().map(Rc::as_ptr)?;
        let w = p.w0;
        // SAFETY the pool holds the code for as long as the VM lives.
        let (layout, frame, flat) = unsafe { ((*c).layout as usize, (*c).frame, (*c).flat()) };
        if !p.laid && p.calls >= lower::TIER { return None; }
        if p.active != 0 || !w.owner.is_undef() || w.base == super::window::NO_WINDOW || w.code != layout && (w.code != 0 || layout == 0) || w.len < frame {
            if p.cold || p.consts.is_none() || layout == 0 { return None; }
            let opened = self.open_level(fi, callee, args, link)?;
            // What the body takes past its arguments goes in its level's window as it opened.
            if n < arity || f.closure || !cells.is_empty() {
                let base = self.base;
                if n < arity || f.closure { self.fill_frame(callee, base, n, arity); }
                for &(slot, cell) in &cells { self.regs[base + slot] = cell; }
            }
            return Some(opened);
        }
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let link = if flat { link } else { self.cold_bases(false); link | BASES << 40 };
        let p = unsafe { self.pools.get_unchecked_mut(f.pool) };
        (p.window, p.wlen, p.wcode, p.active) = (w.base, w.len, w.code, 1);
        if p.calls < lower::TIER { p.calls += 1; }
        // SAFETY the pool holds the code for as long as the VM lives.
        if w.code != layout { self.grow_frame(&self.functions[fi].1, unsafe { &*c }, f.pool); }
        let base = self.pools[f.pool].window;
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let h = [
            Val::int(link),
            Val::int(below as i64 | (fi as i64) << 24),
            callee,
            Val::int(self.stack.len() as i64),
            Val::int(c as usize as i64),
        ];
        debug_assert!(base + f.names as usize <= self.regs.len());
        // SAFETY the window holds its record before it and its names, the first `n` of them the parameters.
        unsafe {
            let r = self.regs.as_mut_ptr().add(base);
            core::ptr::copy_nonoverlapping(h.as_ptr(), r.sub(HEAD), HEAD);
            core::ptr::copy_nonoverlapping(args.as_ptr(), r, n);
            for k in n..f.names as usize { *r.add(k) = Val::undef(); }
        }
        if n < arity || f.closure { self.fill_frame(callee, base, n, arity); }
        for &(slot, cell) in &cells { self.regs[base + slot] = cell; }
        (self.top_pool, self.acts, self.pool_now, self.base) = (f.pool, self.acts + 1, f.pool, base);
        Some((c, &self.functions[fi].1 as *const SSAChunk))
    }

    /* The `*args` slot of `fi`, when its undefaulted positionals fill its first slots and it makes no cell and no generator. */
    #[inline(always)]
    pub(crate) fn star_slot(&self, fi: usize) -> Option<usize> {
        let p = self.call_facts[fi].star;
        (p != NO_SLOTS && !self.is_async.get(fi).copied().unwrap_or(false)).then_some(p as usize)
    }

    /* How many slots the plain, keyword-only and `*args` parameters of `fi` take in order, making no cell and no generator, so a call filling each opens its block. */
    #[inline(always)]
    pub(crate) fn full_slots(&self, fi: usize) -> Option<usize> {
        let n = self.call_facts[fi].full;
        (n != NO_SLOTS && !self.is_async.get(fi).copied().unwrap_or(false)).then_some(n as usize)
    }

    /* Whether the function `fi` keeps from opening in the loop only for the cells its nested functions share. */
    pub(crate) fn cells_only(&self, fi: usize) -> bool {
        let f = &self.call_facts[fi];
        f.arity.is_some() && !self.fn_scope[fi].cellvars.is_empty() && !self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false)
            && self.param_slots[fi].iter().enumerate().all(|(k, &(_, s))| s == k)
    }

    /* The cells `fi` called as `callee` on `args` makes for its shared variables, each around an argument, a default or nothing. */
    #[cold]
    fn made_cells(&mut self, fi: usize, callee: Val, args: &[Val]) -> Option<Vec<(usize, Val)>> {
        let arity = self.call_facts[fi].arity?;
        let defaults = match self.heap.try_get(callee) { Some(HeapObj::Func(_, d, _, _)) => d.clone(), _ => return None };
        let mut cells = Vec::with_capacity(self.fn_scope[fi].cellvars.len());
        for k in 0..self.fn_scope[fi].cellvars.len() {
            let s = self.fn_scope[fi].cellvars[k];
            let v = if s < args.len() { args[s] } else if s < arity { defaults[s + defaults.len() - arity] } else { Val::undef() };
            cells.push((s, self.heap.alloc(HeapObj::Cell(v)).ok()?));
        }
        Some(cells)
    }

    /* `open_block` for a function whose first window another run holds, most often a recursion, opened in a window of a level of its own. */
    #[inline(never)]
    fn open_level(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(*const Code, *const SSAChunk)> { self.open_level_in(fi, callee, args, link) }

    /* `open_level` inlined into the call that takes it most, a recursion's from `enter_block_apart`. */
    #[inline(always)]
    fn open_level_in(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(*const Code, *const SSAChunk)> {
        let f = self.call_facts[fi];
        let n = args.len();
        let c = self.pools[f.pool].code.as_ref().map(Rc::as_ptr)?;
        // SAFETY the pool holds the code for as long as the VM lives.
        let flat = unsafe { (*c).flat() };
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let link = if flat { link } else { self.cold_bases(false); link | BASES << 40 };
        let chunk = &self.functions[fi].1;
        let names = chunk.names.len();
        let base = self.open_call(f.pool, names + chunk.constants.len() + 3, names, n, true);
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        // SAFETY the window holds its record before it and its names, the first `n` of them the parameters.
        unsafe {
            let r = self.regs.as_mut_ptr().add(base);
            let h = r.sub(HEAD);
            (*h, *h.add(1), *h.add(2), *h.add(3), *h.add(4)) = (Val::int(link), Val::int(below as i64 | (fi as i64) << 24), callee, Val::int(self.stack.len() as i64), Val::int(c as usize as i64));
            core::ptr::copy_nonoverlapping(args.as_ptr(), r, n);
        }
        (self.top_pool, self.acts, self.pool_now, self.base) = (f.pool, self.acts + 1, f.pool, base);
        self.run_in(f.pool);
        // SAFETY as above.
        if !self.laid(unsafe { &*c }, f.pool) { self.grow_frame(chunk, unsafe { &*c }, f.pool); }
        Some((c, chunk as *const SSAChunk))
    }

    /* `enter_block` for a body whose first window another run holds or that pushes handlers or loops, opened at a level of its own, else as `enter_site` does. */
    #[inline(never)]
    fn enter_block_apart(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(*const Code, *const SSAChunk)> {
        let next = i + 1 + wide as usize;
        let link = next as i64 | (dest_of(code.ins.get(next)) as i64) << 24;
        // `enter_block` checked the call, the body's code and its laying before it came here.
        let p = &self.pools[self.call_facts[fi].pool];
        let opened = if p.code.is_none() || !p.laid && p.calls >= lower::TIER { None }
            else if p.active != 0 && !p.cold && p.consts.is_some() && p.code.as_ref().is_some_and(|c| c.layout != 0) { self.open_level_in(fi, callee, args, link) }
            else { self.open_block_apart(fi, callee, args, link) };
        if opened.is_some() || self.budget == 0 { return opened; }
        self.enter_site(i, code, fi, callee, args, wide)
    }

    /* `enter_site` kept out of the loop that calls `enter_block`, for the calls it does not take. */
    #[inline(never)]
    fn enter_site_apart(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(*const Code, *const SSAChunk)> {
        self.enter_site(i, code, fi, callee, args, wide)
    }

    /* Opens the frame of a call its site knew with its `n` positionals, the code and chunk it runs, None when the full path must check it. */
    #[inline]
    pub(crate) fn enter_site(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(*const Code, *const SSAChunk)> {
        self.open_site(i, code, fi, callee, args, Val::undef(), Val::undef(), wide)
    }

    /* `enter_site` for a frame that hands back `instance` in place of its result, and binds `super` to `owner` for its receiver, undef for neither. */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn open_site(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], instance: Val, owner: Val, wide: bool) -> Option<(*const Code, *const SSAChunk)> {
        let n = args.len();
        let facts = self.call_facts[fi];
        let arity = facts.arity?;
        if !facts.in_loop || n > arity || n < facts.required as usize
            || self.budget == 0 || self.near_limit() { return None; }
        if args.iter().any(|v| v.is_undef()) { return None; }
        // A method whose body names `super` keeps its binding, one not looked at yet takes the full path.
        let bound = !owner.is_undef() && facts.named_super()?;
        let p = &self.pools[facts.pool];
        if !p.laid && (p.calls >= lower::TIER || p.active >= lower::LAY_DEPTH) { self.relower(fi); }
        if self.pools[facts.pool].cold { return None; }
        if instance.is_undef() {
            let next = i + 1 + wide as usize;
            let flag = if bound { BOUND << 40 } else { 0 };
            if let Some(opened) = self.open_block_as(fi, callee, args, next as i64 | (dest_of(code.ins.get(next)) as i64) << 24 | flag, bound) {
                if bound { self.bindings.push((self.level(), owner, args[0])); }
                return Some(opened);
            }
            if self.budget == 0 { return None; }
        }
        let callee_code = Rc::as_ptr(self.pools[facts.pool].code.as_ref()?);
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let names = self.functions[fi].1.names.len();
        let base = self.open_call(facts.pool, names + self.functions[fi].1.constants.len() + 3, names, n, true);
        // SAFETY the window holds its names, the first `n` of them the parameters.
        unsafe { core::ptr::copy_nonoverlapping(args.as_ptr(), self.regs.as_mut_ptr().add(base), n); }
        if n < arity || facts.closure { self.fill_frame(callee, base, n, arity); }
        let next = i + 1 + wide as usize;
        let dest = dest_of(code.ins.get(next));
        if bound { self.bindings.push((self.level() + 1, owner, args[0])); }
        // SAFETY the pool holds the code for as long as the VM lives.
        let cold = self.cold_bases(unsafe { (*callee_code).flat() });
        self.push_act(Act {
            code: callee_code, pool: facts.pool, cold,
            ret: next, dest, fi, callee, call_ip: Some(i as u32 | RAW_IP),
            stack_base: self.stack.len(), bound, instance, generator: Val::undef(), dunder: false, job: false,
        });
        self.run_in(facts.pool);
        let chunk = &self.functions[fi].1 as *const SSAChunk;
        // SAFETY the pool holds the code for as long as the VM lives.
        let c = unsafe { &*callee_code };
        // SAFETY the chunk lives as long as the VM.
        if !self.laid(c, facts.pool) { self.grow_frame(unsafe { &*chunk }, c, facts.pool); }
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
        if ins.op != OpCode::CallMethodR { self.enter_site(i, code, fi, callee, &vals[1..count], true) }
        else { self.enter_method(i, code, chunk, ins.a, &vals[..count], true) }
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
    #[inline]
    pub(crate) fn enter_method(&mut self, i: usize, code: &Code, chunk: &SSAChunk, name: u16, args: &[Val], wide: bool) -> Option<(*const Code, *const SSAChunk)> {
        let (func, owner) = self.site_method(code.cache.site(i), args[0], chunk.names.get(name as usize)?)?;
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
        self.open_site(i, code, fi, func, args, Val::undef(), owner, wide)
    }

    /* Builds an instance of a constructor site's class and opens its `__init__` on the `n` positionals in `passed`, None when the site no longer holds. */
    #[inline(never)]
    pub(crate) fn enter_ctor(&mut self, i: usize, code: &Code, site: Site, passed: [Val; 2], n: usize) -> Option<(*const Code, *const SSAChunk)> {
        let Site::Ctor { class, init, owner, fi, attrs, epoch } = site else { return None };
        if epoch != self.class_epoch || passed[..n].iter().any(|v| v.is_undef()) { return None; }
        let instance = self.new_instance(Val::heap(class), attrs as usize).ok()?;
        let args = [instance, passed[0], passed[1]];
        // A plain `__init__` opens its block as a plain function does, its instance what the call gives back.
        let dest = dest_of(code.ins.get(i + 1));
        if let Some(opened) = self.open_block(fi as usize, init, &args[..n + 1], (i + 1) as i64 | (dest as i64) << 24 | CTOR << 40) {
            self.instances.push(instance);
            return Some(opened);
        }
        if self.budget == 0 { return None; }
        self.open_site(i, code, fi as usize, init, &args[..n + 1], instance, owner, false)
    }

    /* Ends the running frame on a plain return of `v`, the code, chunk and index its caller carries on at, None when the return needs its handler. */
    #[inline(always)]
    pub(crate) fn leave_site(&mut self, v: Val, floor_code: *const Code, floor_chunk: *const SSAChunk, floor_pool: usize, floor_depth: usize) -> Option<(*const Code, *const SSAChunk, usize)> {
        if self.acts == 0 || v.is_undef() { return None; }
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        let link = self.head(at).as_int();
        if link >> 40 == 0 { return Some(self.leave_plain(v, pool, at, link, floor_code, floor_chunk, floor_pool, floor_depth)); }
        if link >> 40 == JOB {
            return match self.job_again(v)? {
                Ok(again) => Some(again),
                Err(item) if (link >> 24) as u16 != NO_DEST => { self.jobs.pop(); Some(self.leave_plain(item, pool, at, link, floor_code, floor_chunk, floor_pool, floor_depth)) }
                Err(_) => None,
            };
        }
        let f = self.top()?;
        let (ret, dest, pool, bound, instance, cold, dunder) = (f.ret, f.dest, f.pool, f.bound, f.instance, f.cold, f.dunder.then_some(f.callee));
        if v.is_undef() || !f.generator.is_undef() || f.job || link >> 40 & TASK != 0 { return None; }
        // A flat frame left the stacks as it found them, any other is checked against its bases.
        if cold != FLAT {
            let b = self.frame_bases[self.frame_bases.len() - 1];
            if self.exception_stack.len() > b.exc || self.unwind_stack.len() > b.unwind { return None; }
            self.frame_bases.pop();
            if self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        }
        self.pop_act();
        self.close_window(pool);
        if bound { self.bindings.pop(); }
        let v = if instance.is_undef() { v } else { self.init_back(instance, pool) };
        let (code, chunk) = match self.top() {
            Some(f) if self.acts > floor_depth => { let (pool, at) = (f.pool, (f.code, &self.functions[f.fi].1 as *const SSAChunk)); self.run_in(pool); at }
            _ => { self.run_in(floor_pool); (floor_code, floor_chunk) }
        };
        let next = match dunder { Some(func) => self.dunder_back(v, ret, dest, func), None => self.deliver(v, ret, dest) };
        Some((code, chunk, next))
    }

    /* Where the caller of an operator dunder that returned `v` carries on, past its op with `v` stored, or at the op again when it declined so the reflected dunder runs. */
    #[inline]
    pub(crate) fn dunder_back(&mut self, v: Val, ret: usize, dest: u16, func: Val) -> usize {
        // A property's setter gives nothing its store takes, its getter on the stack its value alone, NotImplemented as any other.
        if dest == DISCARD { return ret + 1; }
        if dest == PUSH { self.stack.push(v); return ret + 1; }
        if dest == REDO { return self.redo_back(func, ret, Ok(v)); }
        if self.heap.is_not_implemented(v) { self.declined = func; return ret; }
        if dest == NO_DEST {
            self.stack.truncate(self.stack.len().saturating_sub(2));
            self.stack.push(v);
        } else { self.regs[self.base + dest as usize] = v; }
        ret + 1
    }

    /* Makes the frame just opened the dunder of the op at `ret`, whose result goes to register `dest`, or the stack for NO_DEST. */
    #[inline]
    pub(crate) fn mark_dunder(&mut self, ret: usize, dest: u16) {
        let at = self.head_at(self.top_pool);
        let flags = self.head(at).as_int() >> 40 | DUNDER;
        self.regs[at] = Val::int(ret as i64 | (dest as i64) << 24 | flags << 40);
    }

    /* Whether the frames of the loop can park, none of them a dunder, whose op would have nothing to take its result on resume. */
    pub(crate) fn frames_park(&self) -> bool {
        let (mut pool, mut k) = (self.top_pool, self.acts);
        while k > 0 && pool != NO_POOL {
            let at = self.head_at(pool);
            if self.head(at).as_int() >> 40 & DUNDER != 0 { return false; }
            pool = (self.head(at + 1).as_int() & 0xFF_FFFF) as usize;
            k -= 1;
        }
        true
    }

    /* Ends the running flat frame, its record at `at` starting with `link`, handing `v` to its caller, and gives the code, chunk and index it carries on at. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn leave_plain(&mut self, v: Val, pool: usize, at: usize, link: i64, floor_code: *const Code, floor_chunk: *const SSAChunk, floor_pool: usize, floor_depth: usize) -> (*const Code, *const SSAChunk, usize) {
        let below = (self.head(at + 1).as_int() & 0xFF_FFFF) as usize;
        (self.top_pool, self.acts) = (below, self.acts - 1);
        self.close_window(pool);
        let (code, chunk) = if self.acts > floor_depth {
            self.run_in(below);
            let h = self.head_at(below);
            let fi = (self.head(h + 1).as_int() >> 24) as usize;
            (self.head(h + 4).as_int() as usize as *const Code, &self.functions[fi].1 as *const SSAChunk)
        } else {
            self.run_in(floor_pool);
            (floor_code, floor_chunk)
        };
        (code, chunk, self.deliver(v, (link & 0xFF_FFFF) as usize, (link >> 24) as u16))
    }

    /* The bases of frame `f`, the stacks' lengths now for a flat one. */
    #[inline]
    pub(crate) fn bases_of(&self, f: &Act) -> Bases {
        if f.cold == FLAT { Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: self.iter_stack.len(), with: self.with_stack.len() } }
        else { self.frame_bases[self.frame_bases.len() - 1] }
    }

    /* The `cold` of a frame opening now, FLAT or its bases kept in `frame_bases`. */
    #[inline(always)]
    pub(crate) fn cold_bases(&mut self, flat: bool) -> u32 {
        if flat { return FLAT; }
        self.frame_bases.push(Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: self.iter_stack.len(), with: self.with_stack.len() });
        (self.frame_bases.len() - 1) as u32
    }

    /* Gives a flat running frame bases once code lowered or grown under it pushes handlers, the stacks' lengths now its own. */
    #[cold]
    #[inline(never)]
    pub(crate) fn unflat_top(&mut self, code: &Code) {
        let at = self.head_at(self.top_pool);
        let link = self.head(at).as_int();
        let generator = link >> 40 & GEN != 0;
        if link >> 40 & BASES != 0 || if generator { code.loops_only() } else { code.flat() } { return; }
        // A flat generator's frame keeps its iterator base where a call keeps its register.
        let iter = if generator { (link >> 24 & 0xFFFF) as usize } else { self.iter_stack.len() };
        self.frame_bases.push(Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter, with: self.with_stack.len() });
        let link = if generator { link & !(0xFFFF << 24) | (NO_DEST as i64) << 24 } else { link };
        self.regs[at] = Val::int(link | BASES << 40);
    }

    /* Makes `a` the running call, its fields written before its window. */
    #[inline(always)]
    pub(crate) fn push_act(&mut self, a: Act) {
        // A frame a caller past its tier opens for a body past its own, which no block, job or section took, counts as generic.
        #[cfg(any(debug_assertions, feature = "coverage"))]
        {
            let warm_caller = self.acts != 0 && self.top().is_some_and(|f| { let p = &self.pools[self.fn_pool[f.fi]]; !p.laid && p.calls < lower::TIER });
            let p = &self.pools[a.pool];
            self.loops.generic += (!p.cold && p.calls >= lower::TIER && !warm_caller) as u64;
        }
        let at = self.head_at(a.pool);
        let flags = if a.bound { BOUND } else { 0 } | if a.cold != FLAT { BASES } else { 0 } | if a.generator.is_undef() { 0 } else { GEN }
            | if a.instance.is_undef() { 0 } else { CTOR } | if a.job { JOB } else { 0 } | if a.dunder { DUNDER } else { 0 };
        if !a.instance.is_undef() { self.instances.push(a.instance); }
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let h = [
            Val::int(a.ret as i64 | (a.dest as i64) << 24 | flags << 40),
            Val::int(below as i64 | (a.fi as i64) << 24),
            a.callee,
            Val::int(a.stack_base as i64),
            Val::int(a.code as usize as i64),
        ];
        debug_assert!(at + HEAD <= self.regs.len());
        // SAFETY every window has `HEAD` registers before it.
        unsafe { core::ptr::copy_nonoverlapping(h.as_ptr(), self.regs.as_mut_ptr().add(at), HEAD); }
        (self.top_pool, self.acts) = (a.pool, self.acts + 1);
    }

    /* The running call, read from before its window, None outside every call. */
    #[inline(always)]
    pub(crate) fn top(&self) -> Option<Act> {
        if self.acts == 0 { return None; }
        let pool = self.top_pool;
        let at = self.head_at(pool);
        debug_assert!(at + HEAD <= self.regs.len());
        // SAFETY a running call's window has its record before it.
        let h = unsafe { self.regs.get_unchecked(at..at + HEAD) };
        let (link, below) = (h[0].as_int(), h[1].as_int());
        let flags = link >> 40;
        let ret = (link & 0xFF_FFFF) as usize;
        // The call is the instruction before where its caller carries on, a generator's `for` or a dunder's op that instruction itself.
        let call_ip = if flags & (GEN | DUNDER) != 0 { ret } else { ret.saturating_sub(1) } as u32 | RAW_IP;
        Some(Act {
            code: h[4].as_int() as usize as *const Code, pool,
            cold: if flags & BASES != 0 { 0 } else { FLAT },
            ret, dest: (link >> 24) as u16, fi: (below >> 24) as usize, callee: h[2],
            call_ip: Some(call_ip), stack_base: h[3].as_int() as usize,
            bound: flags & BOUND != 0, instance: if flags & CTOR != 0 { self.instances.last().copied().unwrap_or(Val::undef()) } else { Val::undef() },
            generator: if flags & GEN != 0 { h[2] } else { Val::undef() },
            job: flags & JOB != 0,
            dunder: flags & DUNDER != 0,
        })
    }

    /* What the running call runs, read off its record alone, its code, function, pool and bases. */
    #[inline(always)]
    pub(crate) fn top_run(&self) -> (*const Code, usize, usize, Bases) {
        let at = self.head_at(self.top_pool);
        let b = if self.head(at).as_int() >> 40 & BASES != 0 { self.frame_bases[self.frame_bases.len() - 1] }
            else { Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: self.iter_stack.len(), with: self.with_stack.len() } };
        (self.head(at + 4).as_int() as usize as *const Code, (self.head(at + 1).as_int() >> 24) as usize, self.top_pool, b)
    }

    /* Whether the running call is a job's, read off its record alone. */
    #[inline]
    pub(crate) fn top_job(&self) -> bool { self.acts != 0 && self.head(self.head_at(self.top_pool)).as_int() >> 40 & JOB != 0 }

    /* Ends the running call's record, the call below it running next. */
    #[inline(always)]
    pub(crate) fn pop_act(&mut self) {
        let at = self.head_at(self.top_pool);
        let flags = self.head(at).as_int() >> 40;
        if flags & CTOR != 0 { self.instances.pop(); }
        if flags & JOB != 0 { self.jobs.pop(); }
        let below = (self.head(at + 1).as_int() & 0xFF_FFFF) as usize;
        (self.top_pool, self.acts) = (below, self.acts - 1);
    }

    /* Whether the running call is a generator a `for` steps, yielding into a register when `reg`, else onto the stack above where it began. */
    #[inline]
    pub(crate) fn gen_yields(&self, reg: bool) -> bool {
        let at = self.head_at(self.top_pool);
        (self.head(at).as_int() >> 40) & GEN != 0 && (reg || self.stack.len() > self.head(at + 3).as_int() as usize)
    }

    /* `gen_yields` for a `for`, false for a generator the scheduler runs as a task, whose value goes to the scheduler instead. */
    #[inline(always)]
    pub(crate) fn gen_yields_to_for(&self, reg: bool) -> bool {
        let at = self.head_at(self.top_pool);
        (self.head(at).as_int() >> 40) & (GEN | TASK) == GEN && (reg || self.stack.len() > self.head(at + 3).as_int() as usize)
    }

    /* The stack's length the running call began at. */
    #[inline(always)]
    pub(crate) fn top_stack_base(&self) -> usize { self.head(self.head_at(self.top_pool) + 3).as_int() as usize }

    /* Where the record of the call running in `pool`'s window starts. */
    #[inline(always)]
    pub(crate) fn head_at(&self, pool: usize) -> usize {
        debug_assert!(pool < self.pools.len());
        // SAFETY a pool a call runs in is one of `pools`.
        unsafe { self.pools.get_unchecked(pool) }.window - HEAD
    }

    /* The record register at `at`. */
    #[inline(always)]
    pub(crate) fn head(&self, at: usize) -> Val {
        debug_assert!(at < self.regs.len());
        // SAFETY `at` lies in a running call's record.
        unsafe { *self.regs.get_unchecked(at) }
    }

    /* Where the running call's caller carries on, and the register it takes the result in. */
    #[inline(always)]
    pub(crate) fn set_ret(&mut self, ret: usize, dest: u16) {
        if self.acts == 0 { return; }
        let at = self.head_at(self.top_pool);
        let (link, flags) = (self.head(at).as_int(), self.head(at).as_int() >> 40);
        // A frame a `map`, `filter`, job or dunder opened knows where it returns already, and a generator keeps its iterator base where a call keeps its register.
        if link & (0xFF_FFFF | DUNDER << 40) != 0 { return; }
        if flags & GEN != 0 { unsafe { *self.regs.get_unchecked_mut(at) = Val::int(link | ret as i64); } return; }
        // SAFETY as for `head`.
        unsafe { *self.regs.get_unchecked_mut(at) = Val::int(ret as i64 | (dest as i64) << 24 | flags << 40); }
    }

    /* Lowers the body of `fi` again, laying what it calls as sections. */
    #[cold]
    #[inline(never)]
    pub(crate) fn relower(&mut self, fi: usize) {
        let f = self.functions[fi];
        self.lowered_code(&f.1, self.fn_pool[fi], true);
    }

    /* Whether the body of `fi` names `super`, looked up once. */
    #[inline]
    pub(crate) fn uses_super(&mut self, fi: usize) -> bool {
        if let Some(u) = self.call_facts[fi].named_super() { return u; }
        self.find_super(fi)
    }

    #[cold]
    #[inline(never)]
    fn find_super(&mut self, fi: usize) -> bool {
        let names = &self.functions[fi].1.names;
        let named = names.iter().any(|n| crate::parser::ssa_strip(n) == "super");
        self.call_facts[fi].bits |= SUPER_KNOWN | if named { NAMED_SUPER } else { 0 };
        named
    }

    /* Whether the frame about to open goes past the recursion or the memory limit, the latter looked at once every DEPTH_CHECK frames. */
    #[inline(always)]
    pub(crate) fn too_deep(&self) -> bool { self.near_limit() && (self.acts + self.turned + 2 >= RECURSION_LIMIT || self.heap.over()) }

    /* Whether a frame about to open may go too deep, which its call then looks at apart. */
    #[inline(always)]
    pub(crate) fn near_limit(&self) -> bool { self.acts % DEPTH_CHECK == DEPTH_CHECK - 1 || self.acts + self.turned + 2 >= RECURSION_LIMIT }

    /* How deep the running frame is, nested loops and loop frames alike, which tells one method binding from another. */
    #[inline]
    pub(crate) fn level(&self) -> usize { self.depth + self.acts }

    /* Whether the body of `fi` can run as a frame of the loop that calls it. */
    #[inline]
    pub(crate) fn runs_in_loop(&self, fi: usize, call_safe: bool) -> bool {
        let pool = &self.pools[self.fn_pool[fi]];
        call_safe == self.frame_safe && pool.code.is_some() && pool.consts.is_some()
    }

    /* Ends the innermost loop frame in place, dropping what it left on the live stacks. */
    #[inline]
    pub(crate) fn leave_frame(&mut self) {
        let Some(f) = self.top() else { return };
        let (pool, bound, cold, generator) = (f.pool, f.bound, f.cold, !f.generator.is_undef());
        // Dropped where it lies, since moving a frame out copies every field.
        self.pop_act();
        let _ = generator;
        self.close_window(pool);
        if cold != FLAT && let Some(b) = self.frame_bases.pop() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        if bound { self.bindings.pop(); }
    }

    /* Ends the innermost loop frame, which returned `v`, giving its caller's return point and register, that result and the dunder it ran, if any. */
    #[inline(always)]
    pub(crate) fn return_frame(&mut self, v: Val) -> (usize, u16, Val, Option<Val>) {
        let Some(f) = self.top() else { return (0, NO_DEST, v, None) };
        let (ret, dest, instance) = (f.ret, f.dest, f.instance);
        self.leave_frame();
        (ret, dest, if instance.is_undef() { v } else { self.init_back(instance, f.pool) }, f.dunder.then_some(f.callee))
    }

    /* The instance an `__init__` frame of `pool` hands back, whose fields size the next ones its class builds after its first calls. */
    #[inline]
    fn init_back(&mut self, instance: Val, pool: usize) -> Val {
        if self.pools[pool].calls <= 2 { self.note_attrs(instance); }
        instance
    }

    /* Keeps how many fields `instance` ended with as the size its class's constructor gives the next, once. */
    #[cold]
    #[inline(never)]
    fn note_attrs(&mut self, instance: Val) {
        if let Some(HeapObj::Instance(class, d)) = self.heap.try_get(instance) && let Some(c) = self.ctors.get_mut(&class.0) && c.attrs == 0 { c.attrs = d.borrow().len(); }
    }

    /* Opens the generator `co` a `for` at `i` steps as the next loop frame, giving the code, chunk and index it carries on at, None when it needs the nested resume. */
    #[inline(never)]
    pub(crate) fn enter_gen(&mut self, co: Val, i: usize) -> Option<(*const Code, *const SSAChunk, usize)> {
        if self.too_deep() { return None; }
        let quiet = !self.heap.has_work();
        if matches!(self.heap.try_get(co), Some(HeapObj::Coroutine(c)) if c.syncs.is_empty() && c.stack.last().is_some_and(|v| v.is_undef())) { self.wake_gen(co, true); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        // A finished, running or sectioned body takes the nested resume, a coroutine or an async generator opening only for its own step.
        let BodyRef::Fn(fi) = c.body else { return None };
        if c.ip == FINISHED || c.running || c.layout != 0 || !c.syncs.is_empty() || !c.pending.is_undef() || !self.functions[fi].1.is_generator && !self.is_async_fn(fi) { return self.enter_rare(co, fi, i, quiet); }
        self.open_gen(co, fi, i, quiet, None)
    }

    /* `enter_gen` for a generator suspended inside a section or waiting inside helpers, which open over its frame as it enters, None for any other it refused. */
    #[cold]
    #[inline(never)]
    fn enter_rare(&mut self, co: Val, fi: usize, i: usize, quiet: bool) -> Option<(*const Code, *const SSAChunk, usize)> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        if c.ip == FINISHED || c.running || !c.pending.is_undef() || c.layout == 0 && c.syncs.is_empty() { return None; }
        let (layout, ip) = (core::mem::take(&mut c.layout), c.ip);
        if layout != 0 {
            let code = self.code_laid(fi, layout)?;
            let (code, chunk, at) = self.open_gen(co, fi, i, quiet, Some(code))?;
            return Some((code, chunk, if ip & super::dispatch::RESUME_RAW != 0 { ip & !super::dispatch::RESUME_RAW } else { at }));
        }
        if self.chain_refused(co) { return None; }
        self.wake_gen(co, true);
        let opened = self.open_gen(co, fi, i, quiet, None)?;
        (self.chain.next, self.chain.coro) = (true, co);
        Some(opened)
    }

    /* `enter_gen` for the body `fi` of `co`, `quiet` when no native work waits to be paid, in `code` when given, else in its pool's. */
    #[inline(always)]
    pub(crate) fn open_gen(&mut self, co: Val, fi: usize, i: usize, quiet: bool, code: Option<*const Code>) -> Option<(*const Code, *const SSAChunk, usize)> {
        let pool = self.call_facts[fi].pool;
        if code.is_none() && self.pools[pool].code.is_none() { self.gen_cold_code(fi); }
        if code.is_none() && self.pools[pool].cold { self.heat_gen(fi, pool); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
        let gen_code = match code { Some(code) => code, None => Rc::as_ptr(self.pools[pool].code.as_ref()?) };
        let (stack_base, iter_base, len, resume) = (self.stack.len(), self.iter_stack.len(), c.slots.len(), c.ip);
        // A generator whose window still holds its state and that left only its loop on the stacks runs on in the window as it lies.
        let level = self.pools[pool].active as usize;
        if c.stack.is_empty() && c.excs.len() <= 4 && c.iters.len() <= 1 && self.budget >= 2 && quiet
            && self.pools[pool].win(level).is_some_and(|w| w.owner.0 == co.0) {
            c.running = true;
            self.budget -= 1 + (c.iters.len() + c.excs.len()) as u64;
            // SAFETY the pool holds the code for as long as the VM lives.
            let cold = if unsafe { (*gen_code).loops_only() } && iter_base < NO_DEST as usize { FLAT } else {
                self.frame_bases.push(Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: iter_base, with: self.with_stack.len() });
                (self.frame_bases.len() - 1) as u32
            };
            move_last(&mut c.iters, &mut self.iter_stack);
            if !c.excs.is_empty() {
                let with_base = self.with_stack.len();
                for f in &c.excs { self.exception_stack.push(ExceptionFrame { stack_depth: f.stack_depth + stack_base, iter_depth: f.iter_depth + iter_base, with_depth: f.with_depth + with_base, turned: self.turned, ..*f }); }
                c.excs.clear();
                if !c.withs.is_empty() { self.with_stack.append(&mut c.withs); }
            }
            self.heap.set_budget(self.budget);
            // The window stays its own as it runs there, since no other run takes this level until it ends.
            let p = &mut self.pools[pool];
            if let Some(w) = p.level_mut(level) { (p.window, p.wlen, p.wcode) = (w.base, w.len, w.code); }
            p.active += 1;
            return Some(self.run_gen(co, fi, pool, gen_code, cold, i, stack_base, iter_base, resume));
        }
        let cost = 1 + (c.slots.len() + c.stack.len() + c.iters.len()) as u64;
        let cost = cost + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget = self.budget.saturating_sub(cost);
        self.heap.set_budget(self.budget);
        let kept = self.resumes_in(pool, level, co);
        // SAFETY the pool holds the code for as long as the VM lives.
        let cold = self.cold_bases(unsafe { (*gen_code).loops_only() } && iter_base < NO_DEST as usize);
        let base = self.open_window(pool, len, self.functions[fi].1.names.len(), false);
        if !kept && len != self.pools[pool].wlen {
            self.pools[pool].wcode = 0;
            self.note_window(pool);
        }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else {
            self.close_window(pool);
            if cold != FLAT { self.frame_bases.pop(); }
            return None;
        };
        if !kept {
            self.regs[base..base + len].copy_from_slice(&c.slots);
            c.slots.clear();
        }
        if !c.stack.is_empty() {
            self.stack.extend_from_slice(&c.stack);
            c.stack.clear();
        }
        match c.iters.len() {
            0 => {}
            1 => if let Some(f) = c.iters.pop() { self.iter_stack.push(f) },
            _ => self.iter_stack.append(&mut c.iters),
        }
        if !c.excs.is_empty() {
            let with_base = self.with_stack.len();
            for f in c.excs.iter_mut() {
                f.stack_depth += stack_base;
                f.iter_depth += iter_base;
                f.with_depth += with_base;
            }
            self.exception_stack.append(&mut c.excs);
            if !c.withs.is_empty() { self.with_stack.append(&mut c.withs); }
        }
        c.running = true;
        Some(self.run_gen(co, fi, pool, gen_code, cold, i, stack_base, iter_base, resume))
    }

    /* Heats the first code of the generator function `fi`, run in `pool`, lowering it once hot. */
    #[cold]
    #[inline(never)]
    fn heat_gen(&mut self, fi: usize, pool: usize) {
        self.pools[pool].heat += 1;
        if self.pools[pool].heat >= lower::HOT_LOOP { let f = self.functions[fi]; self.lowered_code(&f.1, pool, true); }
    }

    /* Makes the generator `co`, its window open and its state back, the running frame of the `for` at `i`, giving the code, chunk and index it carries on at. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn run_gen(&mut self, co: Val, fi: usize, pool: usize, gen_code: *const Code, cold: u32, i: usize, stack_base: usize, iter_base: usize, resume: usize) -> (*const Code, *const SSAChunk, usize) {
        let at = self.head_at(pool);
        let flags = GEN | if cold != FLAT { BASES } else { 0 };
        let held = if cold == FLAT { iter_base as i64 } else { NO_DEST as i64 };
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        debug_assert!(at + HEAD <= self.regs.len());
        // SAFETY every window has `HEAD` registers before it.
        unsafe {
            let h = self.regs.as_mut_ptr().add(at);
            *h = Val::int(i as i64 | held << 24 | flags << 40);
            *h.add(1) = Val::int(below as i64 | (fi as i64) << 24);
            *h.add(2) = co;
            *h.add(3) = Val::int(stack_base as i64);
            *h.add(4) = Val::int(gen_code as usize as i64);
        }
        (self.top_pool, self.acts) = (pool, self.acts + 1);
        // A loop frame always ends through `yield_gen` or `finish_gen`, an error included, so its running mark needs no entry in `executing_coros`.
        self.gen_frames += 1;
        self.run_in(pool);
        // SAFETY the pool holds the code for as long as the VM lives.
        let c = unsafe { &*gen_code };
        let chunk = &self.functions[fi].1 as *const SSAChunk;
        // SAFETY the chunk lives as long as the VM.
        if !self.laid(c, pool) { self.grow_frame(unsafe { &*chunk }, c, pool); }
        (gen_code, chunk, c.lowered(resume))
    }

    /* Ends the innermost frame, a generator that yielded at `resume`, back into its object, and gives the `for` that stepped it. */
    pub(crate) fn yield_gen(&mut self, resume: usize) -> usize {
        if self.acts == 0 { return 0; }
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        let (link, co, stack_base) = (self.head(at).as_int(), self.head(at + 2), self.head(at + 3).as_int() as usize);
        let (ret, cold) = ((link & 0xFF_FFFF) as usize, (link >> 40) & BASES != 0);
        let (iter_base, exc_base, with_base) = match cold {
            true => { let b = self.frame_bases[self.frame_bases.len() - 1]; (b.iter, b.exc, b.with) }
            false => (((link >> 24) & 0xFFFF) as usize, self.exception_stack.len(), self.with_stack.len()),
        };
        // Its registers stay in the window it owns until another run takes it, unless a collection gave its state back while it ran.
        let level = self.pools[pool].active as usize - 1;
        let owned = self.pools[pool].win(level).is_some_and(|w| w.owner.0 == co.0);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            (c.ip, c.running) = (resume, false);
            if !owned { c.slots.clear(); }
            if self.stack.len() > stack_base { c.stack.extend_from_slice(&self.stack[stack_base..]); }
            match self.iter_stack.len().saturating_sub(iter_base) {
                0 => {}
                1 => move_last(&mut self.iter_stack, &mut c.iters),
                _ => c.iters.extend(self.iter_stack.drain(iter_base..)),
            }
            // The `with`s it sits inside go with it too, so the code stepping it neither runs nor ends their exits.
            if self.exception_stack.len() > exc_base {
                for f in &self.exception_stack[exc_base..] { c.excs.push(ExceptionFrame { stack_depth: f.stack_depth.saturating_sub(stack_base), iter_depth: f.iter_depth.saturating_sub(iter_base), with_depth: f.with_depth.saturating_sub(with_base), ..*f }); }
                self.exception_stack.truncate(exc_base);
                if self.with_stack.len() > with_base { c.withs.extend(self.with_stack.drain(with_base..)); }
            }
        }
        self.stack.truncate(stack_base);
        (self.top_pool, self.acts) = ((self.head(at + 1).as_int() & 0xFF_FFFF) as usize, self.acts - 1);
        if !owned { self.own(pool, level, co); }
        self.close_window(pool);
        if cold { self.frame_bases.pop(); }
        self.gen_frames -= 1;
        ret
    }

    /* Opens the helper frame of `fi` a coroutine waits inside, `func` the function called, in `code` of `pool`, returning to `ret`, `instance` what a constructor makes. */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_helper(&mut self, fi: usize, func: Val, pool: usize, code: *const Code, cold: u32, ret: usize, stack_base: usize, instance: Val) {
        let at = self.head_at(pool);
        let flags = if cold != FLAT { BASES } else { 0 } | if instance.is_undef() { 0 } else { CTOR };
        if !instance.is_undef() { self.instances.push(instance); }
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        debug_assert!(at + HEAD <= self.regs.len());
        // SAFETY every window has `HEAD` registers before it.
        unsafe {
            let h = self.regs.as_mut_ptr().add(at);
            *h = Val::int(ret as i64 | (NO_DEST as i64) << 24 | flags << 40);
            *h.add(1) = Val::int(below as i64 | (fi as i64) << 24);
            *h.add(2) = func;
            *h.add(3) = Val::int(stack_base as i64);
            *h.add(4) = Val::int(code as usize as i64);
        }
        (self.top_pool, self.acts) = (pool, self.acts + 1);
    }

    /* Hands what woke the parked generator `co` to the place its wait marked, off the stack of the `for` that parked with it when `from_for`. */
    pub(crate) fn wake_gen(&mut self, co: Val, from_for: bool) {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return };
        let Some(top) = c.wait_slot().filter(|t| t.is_undef()) else { return };
        *top = if from_for { self.stack.pop().unwrap_or_else(Val::none) } else { Val::none() };
    }

    /* Parks the generator frame on top with the helpers waiting inside it, and gives the `for` that stepped it, which steps it again on resume. */
    #[cold]
    #[inline(never)]
    pub(crate) fn park_gen(&mut self) -> usize {
        let co = self.top().map_or(Val::undef(), |f| f.generator);
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

    /* `yield` of the generator frame on top, `v` handed to the `for` that stepped it, giving the code, chunk and index that carry on, None when it suspends otherwise. */
    #[inline(never)]
    pub(crate) fn yield_site(&mut self, v: Val, resume: usize, floor_code: *const Code, floor_chunk: *const SSAChunk, floor_pool: usize, floor_depth: usize) -> Option<(*const Code, *const SSAChunk, usize)> {
        let at = self.yield_gen(resume);
        let (code, chunk) = if self.acts > floor_depth {
            // The caller's record read in place, its code and the chunk of its function.
            let (pool, h) = (self.top_pool, self.head_at(self.top_pool));
            let (code, fi) = (self.head(h + 4).as_int() as usize as *const Code, (self.head(h + 1).as_int() >> 24) as usize);
            self.run_in(pool);
            (code, &self.functions[fi].1 as *const SSAChunk)
        } else {
            self.run_in(floor_pool);
            (floor_code, floor_chunk)
        };
        // SAFETY the caller runs code its pool or the floor keeps.
        self.hand_to_for(unsafe { &*code }, at, v);
        Some((code, chunk, at + 1))
    }

    /* `yield` of the generator frame on top that `next` runs, `v` the call's result, giving the code, chunk and index that carry on. */
    #[inline(never)]
    pub(crate) fn gather_leave(&mut self, v: Val, resume: usize, floor_code: *const Code, floor_chunk: *const SSAChunk, floor_pool: usize, floor_depth: usize) -> Option<(*const Code, *const SSAChunk, usize)> {
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        let at = self.yield_gen(resume);
        self.jobs.pop();
        let (code, chunk) = if self.acts > floor_depth {
            let (pool, h) = (self.top_pool, self.head_at(self.top_pool));
            let (code, fi) = (self.head(h + 4).as_int() as usize as *const Code, (self.head(h + 1).as_int() >> 24) as usize);
            self.run_in(pool);
            (code, &self.functions[fi].1 as *const SSAChunk)
        } else {
            self.run_in(floor_pool);
            (floor_code, floor_chunk)
        };
        Some((code, chunk, self.deliver(v, at, NO_DEST)))
    }

    /* The `for` at `at` of `code` takes the value `v` a generator yielded as its own step would, into a register, a module binding or the stack. */
    #[inline]
    pub(crate) fn hand_to_for(&mut self, code: &Code, at: usize, v: Val) {
        debug_assert!(at < code.ins.len());
        // SAFETY a generator's frame returns to the `for` that stepped it, an instruction of its caller's code.
        let step = unsafe { *code.ins.get_unchecked(at) };
        match step.op {
            OpCode::ForIterR if step.x == 0 => {
                debug_assert!(self.base + (step.b as usize) < self.regs.len());
                // SAFETY the caller's frame covers every register its code names.
                unsafe { *self.regs.get_unchecked_mut(self.base + step.b as usize) = v; }
            }
            OpCode::ForIterR => self.scopes[code.module].set_at(step.b as u32, v),
            _ => self.stack.push(v),
        }
    }

    /* Ends the innermost frame, a generator that returned or raised, which then stays finished. */
    pub(crate) fn finish_gen(&mut self) -> usize {
        let Some(f) = self.top() else { return 0 };
        let (co, ret) = (f.generator, f.ret);
        // A frame with no bases leaves the loops it ended inside, which its base drops as `leave_frame` would.
        if f.cold == FLAT { self.iter_stack.truncate(f.dest as usize); }
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.ip = FINISHED; }
        self.leave_gen();
        ret
    }

    /* Drops the innermost frame, a generator's, and its running mark. */
    fn leave_gen(&mut self) {
        let Some(f) = self.top() else { return };
        let (co, pool) = (f.generator, f.pool);
        let level = self.pools[pool].active as usize - 1;
        self.leave_frame();
        // A generator that ended holds nothing in the window it ran in.
        if let Some(w) = self.pools[pool].level_mut(level) && w.owner.0 == co.0 {
            w.owner = Val::undef();
            self.owners -= 1;
        }
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.running = false; }
        self.gen_frames -= 1;
    }



    /* Keeps the innermost loop frame, which suspended, as a sync frame its coroutine runs again on resume, and gives where its caller carries on. */
    #[inline(never)]
    pub(crate) fn park_frame(&mut self) -> usize {
        let Some(f) = self.top() else { return 0 };
        let (fi, func, ret, pool, instance) = (f.fi, f.callee, f.ret, f.pool, f.instance);
        let (ip, layout) = (core::mem::take(&mut self.resume_ip), core::mem::take(&mut self.resume_layout));
        // SAFETY a frame's code lives as long as the VM.
        let layout = if layout != 0 { layout } else { unsafe { (*f.code).layout } };
        let (mut stack_delta, mut iter_delta, mut exception_delta, mut withs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (stack_base, b) = (f.stack_base, self.bases_of(&f));
        self.save_frames(stack_base, b.iter, b.exc, b.with, &mut stack_delta, &mut iter_delta, &mut exception_delta, &mut withs);
        let slots = self.window_slots(pool);
        self.leave_frame();
        self.pending_sync_frames.push(ParkedFrame { ip, fi, func, slots, stack_delta, iter_delta, exception_delta, withs, instance, layout });
        ret
    }
}
