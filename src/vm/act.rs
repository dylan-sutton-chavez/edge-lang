use alloc::vec::Vec;

use super::VM;
use super::lower::Code;
use crate::value::*;
use super::lower;
use crate::code::Ins;
use super::sites::Site;
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::r#unsafe::{CodePtr, ChunkPtr, get_in, get_in_mut, open, put, record, record_mut, reg};

/* an in-place call kept in `HEAD` registers before its window so a call pushes nothing */
#[derive(Clone, Copy)]
pub(crate) struct Act {
    // the lowered code its pool holds for as long as the VM lives
    pub(crate) code: CodePtr,
    // the pool whose window it runs in
    pub(crate) pool: usize,
    // `FLAT` or 0 for one whose bases are the last of `frame_bases`
    pub(crate) cold: u32,
    // where the caller carries on once the call is over
    pub(crate) ret: usize,
    // the caller register the result goes to or `NO_DEST` but a flat generator's iterator base
    pub(crate) dest: u16,
    pub(crate) fi: usize,
    pub(crate) callee: Val,
    // the compiler instruction that called where tracebacks and errors met in the caller point
    pub(crate) call_ip: Option<u32>,
    // the recursion levels kept as it opened which its end cuts back to
    pub(crate) levels: usize,
    // the instance an init call hands back instead of its return and undef otherwise
    pub(crate) instance: Val,
    // the generator a `for` runs as this call or undef for a plain call
    pub(crate) generator: Val,
    // whether the builtin atop `jobs` takes the result and the caller waits until it finishes
    pub(crate) job: bool,
    // whether it runs a hook of the op at `ret` whose `dest` takes its result
    pub(crate) dunder: bool,
}

// the registers before a window holding its call record from return point to code
pub(crate) const HEAD: usize = 5;
// a call with no call below it
pub(crate) const NO_POOL: usize = 0xFF_FFFF;
pub(crate) const BASES: i64 = 2;
pub(crate) const GEN: i64 = 4;
pub(crate) const CTOR: i64 = 8;
pub(crate) const JOB: i64 = 16;
pub(crate) const DUNDER: i64 = 32;
// a coroutine the scheduler switched to or a helper a coroutine waits inside
pub(crate) const TASK: i64 = 64;
// the bits of a record's fourth word that mark the levels under its frame
const MARK: u64 = 0xFFFF_FFFF;

/* the levels under a frame that its record's fourth word keeps */
#[inline(always)]
pub(crate) fn mark_of(w: Val) -> usize { (w.0 & MARK) as usize }

/* the top of the frame below that a record's fourth word keeps */
#[inline(always)]
pub(crate) fn top_of(w: Val) -> u32 { (w.0 >> 32 & 0xFFFF) as u32 }

/* the stack lengths a frame began at and cuts back to as it ends */
#[derive(Clone, Copy)]
pub(crate) struct Bases { pub(crate) exc: usize, pub(crate) unwind: usize, pub(crate) delta: usize, pub(crate) iter: usize }

// a frame whose bases are the stacks' lengths now as its body never grows them
pub(crate) const FLAT: u32 = u32::MAX;

/* what calling a function needs to know of its body found once */
#[derive(Clone, Copy)]
pub(crate) struct CallFacts {
    // its positional count when every parameter is plain
    pub(crate) arity: Option<usize>,
    // how many of those take no default which is the fewest a call may pass
    pub(crate) required: u16,
    // whether it captures cells which its frame takes off the function each call
    pub(crate) closure: bool,
    // whether loops can open the body as a frame with positionals in its first slots
    pub(crate) in_loop: bool,
    pub(crate) pool: usize,
    // its body's names which take the first registers of its window
    pub(crate) names: u32,
    // whether passing all positionals opens it with nothing filled having no defaults or captured cells
    pub(crate) plain: bool,
    // `star_slot` and `full_slots` before their coroutine check or `NO_SLOTS` found as the function is made
    pub(crate) star: u8,
    pub(crate) full: u8,
    // set with opens when a positional call may open its block in the loop
    pub(crate) bits: u8,
}

pub(crate) const NO_SLOTS: u8 = u8::MAX;
pub(crate) const OPENS: u8 = 4;

impl CallFacts {
    #[inline(always)]
    pub(crate) fn opens(&self) -> bool { self.bits & OPENS != 0 }
}

/* where the value a call or a job ends with goes */
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(align(4))]
pub(crate) enum Dest {
    // the register its op names with the caller carrying on at its return point
    Reg(u16),
    // the register the dest word at the return point names which the caller then skips
    Word(u16),
    // the next row of the frame the value returns to
    Top,
    // `retake` which a register op takes as it runs again at the return point
    Retake,
    // nowhere as a statement's call keeps no value
    Void,
}

/* the dest a frame record's register names as its dest word does */
#[inline(always)]
pub(crate) fn word_of(dest: u16) -> Dest { if dest == NO_DEST { Dest::Top } else { Dest::Word(dest) } }

/* a job's own destination or else the one its frame record names */
#[inline(always)]
pub(crate) fn job_dest(to: Dest, dest: u16) -> Dest { if to == Dest::Top { word_of(dest) } else { to } }

// a frame whose caller takes its result onto its top row
pub(crate) const NO_DEST: u16 = u16::MAX;
// an alias value's frame a load opened whose value goes onto its top row
pub(crate) const PUSH: u16 = u16::MAX - 1;
// a frame whose value its op takes from `retake` as it runs again
pub(crate) const AGAIN: u16 = u16::MAX - 2;
// the frame of an op handler's call after which the op runs again
pub(crate) const REDO: u16 = u16::MAX - 3;
// the frame of a statement's call whose value goes nowhere
pub(crate) const VOID: u16 = u16::MAX - 4;

// marks `call_ip` as the caller's lowered index mapped to the compiler's only for an error
pub(crate) const RAW_IP: u32 = 1 << 31;

/* the register or top row where the instruction after a call wants its result */
#[inline(always)]
pub(crate) fn dest_of(next: Option<&Ins>) -> u16 {
    match next {
        Some(s) if s.op == OpCode::StoreTopR && s.x == 0 => s.a,
        _ => NO_DEST,
    }
}

// frames between two looks at the memory limit
pub(crate) const DEPTH_CHECK: usize = 256;
// the frames a suspended coroutine may hold since each resume copies them all
pub(crate) const SUSPENDED_DEPTH: usize = 4 * MAX_CALLS;

impl<'a> VM<'a> {
    /* opens a plain call's body as its loop's next frame in an already bound window */
    #[inline(always)]
    pub(crate) fn enter_frame(&mut self, fi: usize, callee: Val) -> Result<(), VmErr> {
        // plain frames reach a laid call's depth as its section's first entry keeps no level
        if self.near_limit() && (self.acts + self.turned > self.deepest || self.heap.over()) {
            self.close_window(self.fn_pool[fi]);
            return Err(cold_depth());
        }
        let p = &self.pools[self.fn_pool[fi]];
        if !p.laid && (p.calls >= lower::TIER || p.active >= lower::LAY_DEPTH) { self.relower(fi); }
        let Some(rc) = self.pools[self.fn_pool[fi]].code.as_ref() else { return Err(cold_runtime("a frame entered without its code")) };
        let (code, flat) = (CodePtr::of(rc), rc.flat());
        let cold = self.cold_bases(flat);
        let (call_ip, instance) = (self.pending.call_ip.take(), core::mem::replace(&mut self.pending_instance, Val::undef()));
        self.push_act(Act {
            code, pool: self.fn_pool[fi], cold,
            ret: 0, dest: NO_DEST, fi, callee, call_ip,
            levels: self.levels.len(), instance, generator: Val::undef(), dunder: false, job: false,
        });
        self.entered = true;
        self.yielded = true;
        Ok(())
    }

    /* opens plain `fi` on all positionals in its free first window laid for its code */
    #[inline(always)]
    pub(crate) fn enter_block(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(CodePtr, ChunkPtr)> {
        let f = self.call_facts[fi];
        let n = args.len();
        if !f.plain || f.arity != Some(n) || self.near_limit() || args.iter().any(|v| v.is_undef()) { return self.enter_site_apart(i, code, fi, callee, args, wide); }
        debug_assert!(f.pool < self.pools.len());
        let p = get_in_mut(&mut self.pools, f.pool);
        let Some(rc) = p.code.as_ref() else { return self.enter_site_apart(i, code, fi, callee, args, wide) };
        let (c, layout, frame, flat) = (CodePtr::of(rc), rc.layout as usize, rc.frame, rc.flat());
        let w = p.w0;
        if p.active != 0 || !p.laid && p.calls >= lower::TIER || !w.owner.is_undef() || w.base == super::window::NO_WINDOW || w.code != layout || w.len < frame || !flat {
            return self.enter_block_apart(i, code, fi, callee, args, wide);
        }
        // the step pays the native work counted since the last one as `charge_step` does
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let p = get_in_mut(&mut self.pools, f.pool);
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
            self.mark(),
            c.word(),
        ];
        open(&mut self.regs, base, Some(&h), args, f.names as usize);
        (self.top_pool, self.acts, self.pool_now, self.base) = (f.pool, self.acts + 1, f.pool, base);
        Some((c, ChunkPtr::of(&self.functions[fi].1)))
    }

    /* `open_block` kept out of its caller's code for the rare call that takes it */
    #[inline(never)]
    fn open_block_apart(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(CodePtr, ChunkPtr)> { self.open_block(fi, callee, args, link) }

    /* opens the block of plain function `fi` on `args` as `enter_block` does or gives none */
    #[inline(always)]
    pub(crate) fn open_block(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(CodePtr, ChunkPtr)> {
        let f = self.call_facts[fi];
        let n = args.len();
        let star = f.arity.is_none() && self.full_slots(fi) == Some(n);
        let arity = if star { n } else { f.arity? };
        if n > arity || !star && n < f.required as usize || self.near_limit() || args.iter().any(|v| v.is_undef()) { return None; }
        if !star && !f.in_loop && !self.cells_only(fi) { return None; }
        // cells for variables nested functions share are made first so a failed one opens nothing
        let cells = if f.in_loop || star { Vec::new() } else { self.made_cells(fi, callee, args)? };
        let p = &self.pools[f.pool];
        if !p.laid && (p.calls >= lower::TIER || p.active >= lower::LAY_DEPTH) { self.relower(fi); }
        debug_assert!(f.pool < self.pools.len());
        let p = get_in_mut(&mut self.pools, f.pool);
        let rc = p.code.as_ref()?;
        let (c, layout, frame, flat) = (CodePtr::of(rc), rc.layout as usize, rc.frame, rc.flat());
        let w = p.w0;
        if !p.laid && p.calls >= lower::TIER { return None; }
        if p.active != 0 || !w.owner.is_undef() || w.base == super::window::NO_WINDOW || w.code != layout && (w.code != 0 || layout == 0) || w.len < frame {
            if p.cold || p.consts.is_none() || layout == 0 { return None; }
            let opened = self.open_level(fi, callee, args, link)?;
            // what the body takes past its arguments goes in its level's window as it opened
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
        let p = get_in_mut(&mut self.pools, f.pool);
        (p.window, p.wlen, p.wcode, p.active) = (w.base, w.len, w.code, 1);
        if p.calls < lower::TIER { p.calls += 1; }
        // the window takes the layout of its code on the rare laying path apart
        if w.code != layout { self.grow_pool(&self.functions[fi].1, f.pool); }
        let base = self.pools[f.pool].window;
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let h = [
            Val::int(link),
            Val::int(below as i64 | (fi as i64) << 24),
            callee,
            self.mark(),
            c.word(),
        ];
        open(&mut self.regs, base, Some(&h), args, f.names as usize);
        if n < arity || f.closure { self.fill_frame(callee, base, n, arity); }
        for &(slot, cell) in &cells { self.regs[base + slot] = cell; }
        (self.top_pool, self.acts, self.pool_now, self.base) = (f.pool, self.acts + 1, f.pool, base);
        Some((c, ChunkPtr::of(&self.functions[fi].1)))
    }

    /* `fi`'s `*args` slot when undefaulted positionals fill its first slots with no cell or generator */
    #[inline(always)]
    pub(crate) fn star_slot(&self, fi: usize) -> Option<usize> {
        let p = self.call_facts[fi].star;
        (p != NO_SLOTS && !self.is_async.get(fi).copied().unwrap_or(false)).then_some(p as usize)
    }

    /* the slots `fi`'s params take in order so a call filling each opens its block */
    #[inline(always)]
    pub(crate) fn full_slots(&self, fi: usize) -> Option<usize> {
        let n = self.call_facts[fi].full;
        (n != NO_SLOTS && !self.is_async.get(fi).copied().unwrap_or(false)).then_some(n as usize)
    }

    /* whether only the cells its nested functions share keep `fi` from opening in the loop */
    pub(crate) fn cells_only(&self, fi: usize) -> bool {
        let f = &self.call_facts[fi];
        f.arity.is_some() && !self.fn_scope[fi].cellvars.is_empty() && !self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false)
            && self.param_slots[fi].iter().enumerate().all(|(k, &(_, s))| s == k)
    }

    /* cells `fi` makes for shared variables each holding an argument or default or nothing */
    #[cold]
    fn made_cells(&mut self, fi: usize, callee: Val, args: &[Val]) -> Option<Vec<(usize, Val)>> {
        let arity = self.call_facts[fi].arity?;
        let defaults = match self.heap.try_get(callee) { Some(HeapObj::Func { defaults: d, .. }) => d.to_vec(), _ => return None };
        let mut cells = Vec::with_capacity(self.fn_scope[fi].cellvars.len());
        for k in 0..self.fn_scope[fi].cellvars.len() {
            let s = self.fn_scope[fi].cellvars[k];
            let v = if s < args.len() { args[s] } else if s < arity { defaults[s + defaults.len() - arity] } else { Val::undef() };
            cells.push((s, self.heap.alloc(HeapObj::Cell(v)).ok()?));
        }
        Some(cells)
    }

    /* `open_block` in a level's own window when a recursion or other run holds the first */
    #[inline(never)]
    fn open_level(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(CodePtr, ChunkPtr)> { self.open_level_in(fi, callee, args, link) }

    /* `open_level` inlined into its busiest call which is a recursion's from `enter_block_apart` */
    #[inline(always)]
    fn open_level_in(&mut self, fi: usize, callee: Val, args: &[Val], link: i64) -> Option<(CodePtr, ChunkPtr)> {
        let f = self.call_facts[fi];
        let n = args.len();
        let rc = self.pools[f.pool].code.as_ref()?;
        let (c, flat) = (CodePtr::of(rc), rc.flat());
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let link = if flat { link } else { self.cold_bases(false); link | BASES << 40 };
        let chunk = &self.functions[fi].1;
        let names = chunk.names.len();
        let base = self.open_call(f.pool, names + chunk.constants.len() + 3, names, n, true);
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let mark = self.mark();
        *record_mut::<HEAD>(&mut self.regs, base - HEAD) = [Val::int(link), Val::int(below as i64 | (fi as i64) << 24), callee, mark, c.word()];
        open::<0>(&mut self.regs, base, None, args, n);
        (self.top_pool, self.acts, self.pool_now, self.base) = (f.pool, self.acts + 1, f.pool, base);
        self.run_in(f.pool);
        if !self.laid(c.get(), f.pool) { self.grow_pool(chunk, f.pool); }
        Some((c, ChunkPtr::of(chunk)))
    }

    /* `enter_block` for a held first window or a body with handlers or loops else `enter_site` */
    #[inline(never)]
    fn enter_block_apart(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(CodePtr, ChunkPtr)> {
        let next = i + 1 + wide as usize;
        let link = next as i64 | (dest_of(code.ins.get(next)) as i64) << 24;
        // `enter_block` already checked the call and the body's code and laying before coming here
        let p = &self.pools[self.call_facts[fi].pool];
        let opened = if p.code.is_none() || !p.laid && p.calls >= lower::TIER { None }
            else if p.active != 0 && !p.cold && p.consts.is_some() && p.code.as_ref().is_some_and(|c| c.layout != 0) { self.open_level_in(fi, callee, args, link) }
            else { self.open_block_apart(fi, callee, args, link) };
        if opened.is_some() || self.budget == 0 { return opened; }
        self.enter_site(i, code, fi, callee, args, wide)
    }

    /* `enter_site` kept out of the loop calling `enter_block` for calls `enter_block` does not take */
    #[inline(never)]
    fn enter_site_apart(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(CodePtr, ChunkPtr)> {
        self.enter_site(i, code, fi, callee, args, wide)
    }

    /* opens a call its site knew or gives none so the full path checks it */
    #[inline]
    pub(crate) fn enter_site(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], wide: bool) -> Option<(CodePtr, ChunkPtr)> {
        self.open_site(i, code, fi, callee, args, Val::undef(), wide)
    }

    /* enter_site for a frame handing back the instance in place of its result */
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn open_site(&mut self, i: usize, code: &Code, fi: usize, callee: Val, args: &[Val], instance: Val, wide: bool) -> Option<(CodePtr, ChunkPtr)> {
        let n = args.len();
        let facts = self.call_facts[fi];
        let arity = facts.arity?;
        if !facts.in_loop || n > arity || n < facts.required as usize
            || self.budget == 0 || self.near_limit() { return None; }
        if args.iter().any(|v| v.is_undef()) { return None; }
        let p = &self.pools[facts.pool];
        if !p.laid && (p.calls >= lower::TIER || p.active >= lower::LAY_DEPTH) { self.relower(fi); }
        if self.pools[facts.pool].cold { return None; }
        if instance.is_undef() {
            let next = i + 1 + wide as usize;
            if let Some(opened) = self.open_block(fi, callee, args, next as i64 | (dest_of(code.ins.get(next)) as i64) << 24) { return Some(opened); }
            if self.budget == 0 { return None; }
        }
        let rc = self.pools[facts.pool].code.as_ref()?;
        let (callee_code, flat) = (CodePtr::of(rc), rc.flat());
        let cost = 1 + self.heap.take_work() as u64;
        if self.budget < cost { self.budget = 0; return None; }
        self.budget -= cost;
        self.heap.set_budget(self.budget);
        let names = self.functions[fi].1.names.len();
        let base = self.open_call(facts.pool, names + self.functions[fi].1.constants.len() + 3, names, n, true);
        open::<0>(&mut self.regs, base, None, args, n);
        if n < arity || facts.closure { self.fill_frame(callee, base, n, arity); }
        let next = i + 1 + wide as usize;
        let dest = dest_of(code.ins.get(next));
        let cold = self.cold_bases(flat);
        self.push_act(Act {
            code: callee_code, pool: facts.pool, cold,
            ret: next, dest, fi, callee, call_ip: Some(i as u32 | RAW_IP),
            levels: self.levels.len(), instance, generator: Val::undef(), dunder: false, job: false,
        });
        self.run_in(facts.pool);
        let fun = self.functions[fi];
        if !self.laid(callee_code.get(), facts.pool) { self.grow_pool(&fun.1, facts.pool); }
        Some((callee_code, ChunkPtr::of(&fun.1)))
    }

    /* hands `v` to the caller as `dest` asks giving the index it carries on at */
    #[inline(always)]
    pub(crate) fn deliver(&mut self, v: Val, ret: usize, dest: u16) -> usize {
        match dest {
            NO_DEST => { self.push(v); ret }
            d => { self.regs[self.base + d as usize] = v; ret + 1 }
        }
    }

    /* a new record's fourth word holding the levels kept and the caller's top */
    #[inline(always)]
    pub(crate) fn mark(&self) -> Val { Val::int((self.levels.len() as u64 | (self.top as u64) << 32) as i64) }

    /* where a job opened by a register call taking its value at `take` hands it */
    #[inline(always)]
    pub(crate) fn take_dest(code: &Code, take: usize) -> Dest {
        match code.ins.get(take) { Some(t) if t.op == OpCode::StoreTopR && t.x == 0 => Dest::Word(t.a), _ => Dest::Top }
    }

    /* hands `v` to `to` and gives where the caller carries on from return point `ret` */
    #[inline(always)]
    pub(crate) fn deliver_to(&mut self, v: Val, ret: usize, to: Dest) -> usize {
        match to {
            Dest::Reg(r) => { self.regs[self.base + r as usize] = v; ret }
            Dest::Word(r) => { self.regs[self.base + r as usize] = v; ret + 1 }
            Dest::Top => { self.push(v); ret }
            Dest::Retake => { self.retake = v; ret }
            Dest::Void => ret,
        }
    }

    /* runs `f` with each job it opens delivering to `to` */
    #[inline(always)]
    pub(crate) fn delivering<R>(&mut self, to: Dest, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = core::mem::replace(&mut self.job_to, to);
        let r = f(self);
        self.job_to = outer;
        r
    }

    /* `enter_site` or `enter_method` for a call with operands past two in the `ArgsR` after it */
    #[inline(never)]
    pub(crate) fn enter_wide(&mut self, i: usize, code: &Code, chunk: &SSAChunk, fi: usize, callee: Val) -> Option<(CodePtr, ChunkPtr)> {
        let ins = code.ins[i];
        let (regs, count) = lower::call_regs(ins, *code.ins.get(i + 1)?);
        let r = &self.regs[self.base..];
        let vals: [Val; 6] = regs.map(|k| r[k as usize]);
        if ins.op != OpCode::CallMethodR { self.enter_site(i, code, fi, callee, &vals[1..count], true) }
        else { self.enter_method(i, code, chunk, ins.a, &vals[..count], true) }
    }

    /* fills the parameters past the `n` passed from `callee`'s defaults then sets its captured cells */
    #[inline(never)]
    pub(crate) fn fill_frame(&mut self, callee: Val, base: usize, n: usize, arity: usize) {
        let Some(HeapObj::Func { defaults, cells: captures, .. }) = self.heap.try_get(callee) else { return };
        // defaults belong to the trailing parameters
        let first = arity - defaults.len();
        self.regs[base + n..base + arity].copy_from_slice(&defaults[n - first..arity - first]);
        for &(slot, cell) in captures { if let Some(r) = self.regs.get_mut(base + slot) { *r = cell; } }
    }

    /* opens the method a site knows for the receiver as enter_site does */
    #[inline]
    pub(crate) fn enter_method(&mut self, i: usize, code: &Code, chunk: &SSAChunk, name: u16, args: &[Val], wide: bool) -> Option<(CodePtr, ChunkPtr)> {
        let func = self.site_method(code.cache.site(i), args[0], chunk.names.get(name as usize)?)?;
        let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(func) else { return None };
        self.open_site(i, code, fi, func, args, Val::undef(), wide)
    }

    /* builds an instance and opens its init on `n` positionals or None when stale */
    #[inline(never)]
    pub(crate) fn enter_ctor(&mut self, i: usize, code: &Code, site: Site, passed: [Val; 2], n: usize) -> Option<(CodePtr, ChunkPtr)> {
        let Site::Ctor { class, init, fi, attrs, epoch } = site else { return None };
        if epoch != self.layout_epoch || passed[..n].iter().any(|v| v.is_undef()) { return None; }
        let instance = self.new_instance(Val::heap(class), attrs as usize).ok()?;
        let args = [instance, passed[0], passed[1]];
        // a plain init opens like a function and the call gives back its instance
        let dest = dest_of(code.ins.get(i + 1));
        if let Some(opened) = self.open_block(fi as usize, init, &args[..n + 1], (i + 1) as i64 | (dest as i64) << 24 | CTOR << 40) {
            self.instances.push(instance);
            return Some(opened);
        }
        if self.budget == 0 { return None; }
        self.open_site(i, code, fi as usize, init, &args[..n + 1], instance, false)
    }

    /* ends the running frame on a plain return of `v` unless it needs its handler */
    #[inline(always)]
    pub(crate) fn leave_site(&mut self, v: Val, floor_code: CodePtr, floor_chunk: ChunkPtr, floor_pool: usize, floor_depth: usize) -> Option<(CodePtr, ChunkPtr, usize)> {
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
        let (ret, dest, pool, instance, cold, dunder) = (f.ret, f.dest, f.pool, f.instance, f.cold, f.dunder.then_some(f.callee));
        if v.is_undef() || !f.generator.is_undef() || f.job || link >> 40 & TASK != 0 { return None; }
        // a flat frame left the stacks as found but others are checked against their bases
        if cold != FLAT {
            let b = self.frame_bases[self.frame_bases.len() - 1];
            if self.exception_stack.len() > b.exc || self.unwind_stack.len() > b.unwind { return None; }
            self.frame_bases.pop();
            if self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        }
        self.pop_act();
        self.close_window(pool);
        let v = if instance.is_undef() { v } else { self.init_back(instance, pool) };
        let (code, chunk) = match self.top() {
            Some(f) if self.acts > floor_depth => { let (pool, at) = (f.pool, (f.code, ChunkPtr::of(&self.functions[f.fi].1))); self.run_in(pool); at }
            _ => { self.run_in(floor_pool); (floor_code, floor_chunk) }
        };
        let next = match dunder { Some(func) => self.dunder_back(v, ret, dest, func), None => self.deliver(v, ret, dest) };
        Some((code, chunk, next))
    }

    /* where the caller of a hook frame that returned `v` carries on past its op */
    #[inline(never)]
    pub(crate) fn dunder_back(&mut self, v: Val, ret: usize, dest: u16, func: Val) -> usize {
        if dest == VOID { return ret + 1; }
        if dest == PUSH { self.push(v); return ret + 1; }
        if dest == REDO { return self.redo_back(func, ret, Ok(v)); }
        if dest == AGAIN { self.retake = v; return ret; }
        if dest == NO_DEST {
            // the value takes the place of the op's two operands
            self.cut_to(self.sp().saturating_sub(2));
            self.push(v);
        } else { self.regs[self.base + dest as usize] = v; }
        ret + 1
    }

    /* makes the frame just opened the op method at `ret` returning to `dest` */
    #[inline]
    pub(crate) fn mark_dunder(&mut self, ret: usize, dest: u16) {
        let at = self.head_at(self.top_pool);
        let flags = self.head(at).as_int() >> 40 | DUNDER;
        self.regs[at] = Val::int(ret as i64 | (dest as i64) << 24 | flags << 40);
    }

    /* whether loop frames can park with no op method whose op would miss its result */
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

    /* ends the running flat frame whose record at `at` begins with `link` handing `v` back */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn leave_plain(&mut self, v: Val, pool: usize, at: usize, link: i64, floor_code: CodePtr, floor_chunk: ChunkPtr, floor_pool: usize, floor_depth: usize) -> (CodePtr, ChunkPtr, usize) {
        let below = (self.head(at + 1).as_int() & 0xFF_FFFF) as usize;
        (self.top_pool, self.acts, self.top) = (below, self.acts - 1, top_of(self.head(at + 3)));
        self.close_window(pool);
        let (code, chunk) = if self.acts > floor_depth {
            self.run_in(below);
            let h = self.head_at(below);
            let fi = (self.head(h + 1).as_int() >> 24) as usize;
            (CodePtr::from_word(self.head(h + 4)), ChunkPtr::of(&self.functions[fi].1))
        } else {
            self.run_in(floor_pool);
            (floor_code, floor_chunk)
        };
        (code, chunk, self.deliver(v, (link & 0xFF_FFFF) as usize, (link >> 24) as u16))
    }

    /* the bases of frame `f` which for a flat one are the stacks' lengths now */
    #[inline]
    pub(crate) fn bases_of(&self, f: &Act) -> Bases {
        if f.cold == FLAT { Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: self.iter_stack.len() } }
        else { self.frame_bases[self.frame_bases.len() - 1] }
    }

    /* the `cold` of a frame opening now as `FLAT` or its bases kept in `frame_bases` */
    #[inline(always)]
    pub(crate) fn cold_bases(&mut self, flat: bool) -> u32 {
        if flat { return FLAT; }
        self.frame_bases.push(Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: self.iter_stack.len() });
        (self.frame_bases.len() - 1) as u32
    }

    /* gives a flat running frame bases once code lowered or grown under it pushes handlers */
    #[cold]
    #[inline(never)]
    pub(crate) fn unflat_top(&mut self, code: &Code) {
        let at = self.head_at(self.top_pool);
        let link = self.head(at).as_int();
        let generator = link >> 40 & GEN != 0;
        if link >> 40 & BASES != 0 || if generator { code.loops_only() } else { code.flat() } { return; }
        // a flat generator's frame keeps its iterator base where a call keeps its register
        let iter = if generator { (link >> 24 & 0xFFFF) as usize } else { self.iter_stack.len() };
        self.frame_bases.push(Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter });
        let link = if generator { link & !(0xFFFF << 24) | (NO_DEST as i64) << 24 } else { link };
        self.regs[at] = Val::int(link | BASES << 40);
    }

    /* makes `a` the running call with its fields written before its window */
    #[inline(always)]
    pub(crate) fn push_act(&mut self, a: Act) {
        // generic counts tiered frames from tiered callers that no block or job or section took
        #[cfg(any(debug_assertions, feature = "coverage"))]
        {
            let warm_caller = self.acts != 0 && self.top().is_some_and(|f| { let p = &self.pools[self.fn_pool[f.fi]]; !p.laid && p.calls < lower::TIER });
            let p = &self.pools[a.pool];
            self.loops.generic += (!p.cold && p.calls >= lower::TIER && !warm_caller) as u64;
        }
        let at = self.head_at(a.pool);
        let flags = if a.cold != FLAT { BASES } else { 0 } | if a.generator.is_undef() { 0 } else { GEN }
            | if a.instance.is_undef() { 0 } else { CTOR } | if a.job { JOB } else { 0 } | if a.dunder { DUNDER } else { 0 };
        if !a.instance.is_undef() { self.instances.push(a.instance); }
        let below = if self.acts == 0 { NO_POOL } else { self.top_pool };
        let h = [
            Val::int(a.ret as i64 | (a.dest as i64) << 24 | flags << 40),
            Val::int(below as i64 | (a.fi as i64) << 24),
            a.callee,
            Val::int((a.levels as u64 | (self.top as u64) << 32) as i64),
            a.code.word(),
        ];
        open(&mut self.regs, at + HEAD, Some(&h), &[], 0);
        (self.top_pool, self.acts) = (a.pool, self.acts + 1);
    }

    /* the running call read from before its window or `None` outside every call */
    #[inline(always)]
    pub(crate) fn top(&self) -> Option<Act> {
        if self.acts == 0 { return None; }
        let pool = self.top_pool;
        let at = self.head_at(pool);
        let h = record::<HEAD>(&self.regs, at);
        let (link, below) = (h[0].as_int(), h[1].as_int());
        let flags = link >> 40;
        let ret = (link & 0xFF_FFFF) as usize;
        // calls precede their return point while a generator's `for` or method-calling op is that point
        let call_ip = if flags & (GEN | DUNDER) != 0 { ret } else { ret.saturating_sub(1) } as u32 | RAW_IP;
        Some(Act {
            code: CodePtr::from_word(h[4]), pool,
            cold: if flags & BASES != 0 { 0 } else { FLAT },
            ret, dest: (link >> 24) as u16, fi: (below >> 24) as usize, callee: h[2],
            call_ip: Some(call_ip), levels: mark_of(h[3]),
            instance: if flags & CTOR != 0 { self.instances.last().copied().unwrap_or(Val::undef()) } else { Val::undef() },
            generator: if flags & GEN != 0 { h[2] } else { Val::undef() },
            job: flags & JOB != 0,
            dunder: flags & DUNDER != 0,
        })
    }

    /* the running call's code and function and pool and bases read off its record alone */
    #[inline(always)]
    pub(crate) fn top_run(&self) -> (CodePtr, usize, usize, Bases) {
        let at = self.head_at(self.top_pool);
        let b = if self.head(at).as_int() >> 40 & BASES != 0 { self.frame_bases[self.frame_bases.len() - 1] }
            else { Bases { exc: self.exception_stack.len(), unwind: self.unwind_stack.len(), delta: self.pending.delta_save.len(), iter: self.iter_stack.len() } };
        (CodePtr::from_word(self.head(at + 4)), (self.head(at + 1).as_int() >> 24) as usize, self.top_pool, b)
    }

    /* whether the running call is a job's read off its record alone */
    #[inline]
    pub(crate) fn top_job(&self) -> bool { self.acts != 0 && self.head(self.head_at(self.top_pool)).as_int() >> 40 & JOB != 0 }

    /* ends the running call's record so the call below it runs next */
    #[inline(always)]
    pub(crate) fn pop_act(&mut self) {
        let at = self.head_at(self.top_pool);
        let flags = self.head(at).as_int() >> 40;
        if flags & CTOR != 0 { self.instances.pop(); }
        if flags & JOB != 0 { self.jobs.pop(); }
        // the frame below takes back its top
        self.top = top_of(self.head(at + 3));
        let below = (self.head(at + 1).as_int() & 0xFF_FFFF) as usize;
        (self.top_pool, self.acts) = (below, self.acts - 1);
    }

    /* where the record of the call running in `pool`'s window starts */
    #[inline(always)]
    pub(crate) fn head_at(&self, pool: usize) -> usize {
        let w = get_in(&self.pools, pool).window;
        // a release check here costs generator recursion four percent so debug builds hold it
        debug_assert!(w >= HEAD && w <= self.regs.len(), "a record read where no window lies");
        w - HEAD
    }

    /* the record register at `at` */
    #[inline(always)]
    pub(crate) fn head(&self, at: usize) -> Val {
        reg(&self.regs, at)
    }

    /* where the running call's caller carries on and the register it takes the result in */
    #[inline(always)]
    pub(crate) fn set_ret(&mut self, ret: usize, dest: u16) {
        if self.acts == 0 { return; }
        self.set_ret_at(self.head_at(self.top_pool), ret, dest);
    }

    /* `set_ret` for the record at `at` of a call known open */
    #[inline(always)]
    pub(crate) fn set_ret_at(&mut self, at: usize, ret: usize, dest: u16) {
        let (link, flags) = (self.head(at).as_int(), self.head(at).as_int() >> 40);
        // frames `map` or `filter` or jobs or op methods opened already know where they return
        if link & (0xFF_FFFF | DUNDER << 40) != 0 { return; }
        // a generator keeps its iterator base where a call keeps its register
        if flags & GEN != 0 { put(&mut self.regs, at, Val::int(link | ret as i64)); return; }
        put(&mut self.regs, at, Val::int(ret as i64 | (dest as i64) << 24 | flags << 40));
    }

    /* lowers the body of `fi` again laying what it calls as sections */
    #[cold]
    #[inline(never)]
    pub(crate) fn relower(&mut self, fi: usize) {
        let f = self.functions[fi];
        self.lowered_code(&f.1, self.fn_pool[fi], true);
    }

    /* whether the next frame passes the recursion or memory limit checking memory every `DEPTH_CHECK` frames */
    #[inline(always)]
    pub(crate) fn too_deep(&self) -> bool { self.near_limit() && (self.acts + self.turned >= self.deepest || self.heap.over()) }

    /* whether the next frame may go too deep so its call checks that apart */
    #[inline(always)]
    pub(crate) fn near_limit(&self) -> bool { self.acts % DEPTH_CHECK == DEPTH_CHECK - 1 || self.acts + self.turned >= self.deepest }

    /* whether the body of `fi` can run as a frame of the loop calling it */
    #[inline]
    pub(crate) fn runs_in_loop(&self, fi: usize, call_safe: bool) -> bool {
        let pool = &self.pools[self.fn_pool[fi]];
        call_safe == self.frame_safe && pool.code.is_some() && pool.consts.is_some()
    }

    /* ends the innermost loop frame in place dropping what it left on the live stacks */
    #[inline]
    pub(crate) fn leave_frame(&mut self) {
        let Some(f) = self.top() else { return };
        let (pool, cold) = (f.pool, f.cold);
        // dropped where it lies since moving a frame out copies every field
        self.pop_act();
        // the levels its sections kept end with it
        if self.levels.len() > f.levels { self.levels.truncate(f.levels); }
        self.close_window(pool);
        if cold != FLAT && let Some(b) = self.frame_bases.pop() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
    }

    /* ends the innermost loop frame giving where result `v` goes and its op method */
    #[inline(always)]
    pub(crate) fn return_frame(&mut self, v: Val) -> (usize, u16, Val, Option<Val>) {
        let Some(f) = self.top() else { return (0, NO_DEST, v, None) };
        let (ret, dest, instance) = (f.ret, f.dest, f.instance);
        self.leave_frame();
        (ret, dest, if instance.is_undef() { v } else { self.init_back(instance, f.pool) }, f.dunder.then_some(f.callee))
    }

    /* what an init frame of `pool` hands back whose fields size its class's later instances */
    #[inline]
    fn init_back(&mut self, instance: Val, pool: usize) -> Val {
        if self.pools[pool].calls <= 2 { self.note_attrs(instance); }
        instance
    }

    /* keeps once how many fields `instance` ended with to size its class's next instances */
    #[cold]
    #[inline(never)]
    fn note_attrs(&mut self, instance: Val) {
        if let Some((class, d)) = self.heap.instance(instance) && let Some(c) = self.ctors.get_mut(&class.0) && c.attrs == 0 { c.attrs = d.borrow().len(); }
    }
}
