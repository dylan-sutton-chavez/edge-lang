use alloc::rc::Rc;

use crate::abi::nan_box::TAG_UNDEF;
use crate::syntax::chunk::{Instruction, SITE_TABLE, SITE_VOID, SSAChunk, native_site};
use crate::code::OpCode;
use super::VM;
use super::depth;
use super::lower;
use super::lower::Code;
use crate::code::Ins;
use super::scope::Kind;
use crate::value::*;
use alloc::vec::Vec;
use crate::r#unsafe::{CodePtr, Consts, fill_to, get_in_mut};

pub(crate) mod values;
pub(crate) mod containers;
pub(crate) mod calls;
pub(crate) mod natives;
pub(crate) mod flow;

/* one past the most `op` adds over rows and zero without an arm */
const fn mark(op: OpCode) -> u8 {
    let rise = depth::rise(op);
    // a short chunk tells a wide op from the sum of its marks alone
    assert!(rise == depth::WIDE || rise as usize * SHORT < depth::WIDE as usize);
    if !matches!(op.info().arm, crate::code::Arm::None) { 1 + rise } else { 0 }
}

// the opcodes no compiled code holds as the compiler writes each as a jump
const fn folded(op: OpCode) -> bool { matches!(op, OpCode::And | OpCode::Or) }

/* the first opcode compiled code may hold with no arm over rows */
pub(crate) const fn uncovered() -> Option<OpCode> {
    let mut k = 0;
    while k < OpCode::ALL.len() {
        let op = OpCode::ALL[k];
        if !op.is_register() && !folded(op) && mark(op) == 0 { return Some(op); }
        k += 1;
    }
    None
}

// every build fails unless each opcode compiled code may hold has an arm over rows
const _: () = assert!(uncovered().is_none(), "an opcode compiled code may hold has no arm over rows");

// each op's mark when covered and a miss bit when not
static GATE: [u64; 256] = {
    let mut gate = [1 << MISSED; 256];
    let mut k = 0;
    while k < OpCode::ALL.len() {
        if mark(OpCode::ALL[k]) != 0 { gate[k] = mark(OpCode::ALL[k]) as u64; }
        k += 1;
    }
    gate
};

// the bit a gate sum counts its ops without an arm from
const MISSED: u32 = 48;

// the mark of a covered opcode whose operand counts what it adds
const WIDE: u8 = 1 + depth::WIDE;

// the ops the gate sums between its looks for an op without an arm
const STRIDE: usize = 4;

/* `src`'s gate words summed until a stride holds an op without an arm */
#[inline(always)]
// chunks_exact compiles to a tighter loop here than as_chunks
#[allow(clippy::chunks_exact_to_as_chunks)]
fn gate(src: &[Instruction]) -> u64 {
    let mut parts = src.chunks_exact(STRIDE);
    let mut sum = 0u64;
    for part in &mut parts {
        sum = part.iter().fold(sum, |s, i| s.wrapping_add(GATE[i.opcode as usize]));
        if sum >> MISSED != 0 { return sum; }
    }
    parts.remainder().iter().fold(sum, |s, i| s.wrapping_add(GATE[i.opcode as usize]))
}

/* the rows `src` needs or none when only runs know */
#[inline(always)]
fn rows_depth(src: &[Instruction], chunk: &SSAChunk) -> Option<usize> {
    // a chunk past the longest a gate sum counts lays no rows
    if src.len() > u16::MAX as usize { return None; }
    let sum = gate(src);
    if sum >> MISSED != 0 { return None; }
    let marks = (sum & ((1 << MISSED) - 1)) as usize;
    // laying a row per rising op costs less than the exact depth's pass
    if marks - src.len() <= LAID || src.len() <= SHORT { return bound(src, chunk, marks); }
    // a deep bound takes the stack one walk finds so its frame lays no slack
    if let Some(d) = depth::upper(src, chunk) {
        debug_assert!(depth::deepest(src, chunk).is_none_or(|e| e <= d), "a walk of {:?} under its deepest stack", chunk.qualname);
        return Some(usize::from(d));
    }
    if src.len() <= ROOMY {
        let d = bound(src, chunk, marks)?;
        if d <= ROOMY { return Some(d); }
    }
    depth::deepest(src, chunk).map(usize::from)
}

// the rows a chunk lays from its bound past which a walk finds its depth
const LAID: usize = 56;

// the most ops and rows a chunk with a wide op takes from its bound
const ROOMY: usize = 256;

/* the most `src`'s ops add to the stack from their marks unless one is wide */
fn bound(src: &[Instruction], chunk: &SSAChunk, sum: usize) -> Option<usize> {
    if sum - src.len() < WIDE as usize - 1 { return Some(sum - src.len()); }
    let mut d = 0;
    for i in src {
        d += match GATE[i.opcode as usize] as u8 {
            WIDE => { let (pops, pushes) = depth::effect(i.opcode, i.operand, chunk)?; pushes.saturating_sub(pops) as usize }
            k => k as usize - 1,
        };
    }
    Some(d)
}

/* the row past `names` names at least two */
#[inline(always)]
pub(crate) const fn first_row(names: usize) -> usize { if names < 2 { 2 } else { names } }

// chunks this short take the most their ops add instead of their exact depth
const SHORT: usize = 32;

// the longest frame over rows so a row index never meets the frame dest sentinels
const LONGEST: usize = 65_000;

// undefined registers a window over rows grows by in one copy
static UNDEF: [Val; 64] = [Val(TAG_UNDEF); 64];

/* whether lowered `code` keeps every value in registers with no op over rows */
pub(crate) fn unstacked(code: &Code) -> bool {
    code.ins.iter().all(|i| i.op.is_register() && i.op != OpCode::PushRegs || i.op == OpCode::Yield && i.x != 0 || depth::unstacked(i.op))
}

impl VM<'_> {
    // lowerings refused for an op over rows and each such op with its count
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn stack_work(&self) -> (u32, alloc::vec::Vec<(OpCode, u32)>) {
        let ops = OpCode::ALL.iter().filter(|op| self.stack_ops[**op as usize] != 0).map(|&op| (op, self.stack_ops[op as usize])).collect();
        (self.stack_lowered, ops)
    }

    /* counts lowered `code` refused for an op over rows and each such op */
    #[cfg(debug_assertions)]
    pub(crate) fn note_stack_work(&mut self, code: &Code) {
        self.stack_lowered += 1;
        for i in code.ins.iter().filter(|i| !(i.op.is_register() && i.op != OpCode::PushRegs || i.op == OpCode::Yield && i.x != 0 || depth::unstacked(i.op))) { self.stack_ops[i.op as usize] += 1; }
    }

    /* `chunk`'s first code over rows or a raising stub when no rows hold it */
    #[inline(always)]
    pub(crate) fn first_identity(&mut self, src: &[Instruction], chunk: &SSAChunk, pool: usize, kinds: Rc<[Kind]>, module: usize) -> Rc<Code> {
        let mut code = Rc::new(lower::identity(src, chunk.names.len(), kinds.clone(), module));
        let laid = match (rows_depth(src, chunk), Rc::get_mut(&mut code)) { (Some(depth), Some(c)) => self.over_rows(c, src, chunk, pool, depth), _ => false };
        if laid { code } else { unrunnable(chunk, kinds, module) }
    }

    /* lays `code` over `depth` rows past its names or false past the longest frame */
    #[cold]
    #[inline(never)]
    fn over_rows(&mut self, c: &mut Code, src: &[Instruction], chunk: &SSAChunk, pool: usize, depth: usize) -> bool {
        if self.pools[pool].consts.is_none() {
            match super::cache::const_vals(chunk, &mut self.heap) { Ok(k) => self.pools[pool].consts = Some(k), Err(_) => return false }
        }
        let consts = self.pools[pool].consts.as_deref().map_or(Consts::NONE, Consts::of);
        let s0 = first_row(chunk.names.len());
        if s0 + depth >= LONGEST { return false; }
        (c.rows, c.frame, c.consts) = (s0 as u16, s0 + depth, consts);
        #[cfg(not(debug_assertions))]
        let _ = src;
        #[cfg(debug_assertions)]
        {
            c.depths = depth::depths(src, chunk).unwrap_or_else(|k| panic!("code over rows whose depth does not settle at {k}"));
            debug_assert!(c.depths.iter().all(|&d| d == u16::MAX || d as usize <= depth), "code over rows deeper than its bound");
        }
        true
    }

    /* lays `pool`'s window over rows leaving rows past names undefined and constants in the pool */
    #[inline(never)]
    pub(crate) fn lay_rows(&mut self, code: &Code, pool: usize) {
        let (len, room, frame) = (self.regs.len(), self.regs.capacity(), code.frame);
        let p = get_in_mut(&mut self.pools, pool);
        // a first level lying last with room grows in place with one window to note
        if p.active == 1 && p.window != super::window::NO_WINDOW && p.window + p.wlen == len && p.wlen < frame && p.window + frame <= room {
            // the room past the length was checked just above
            fill_to(&mut self.regs, p.window + frame);
            (p.wlen, p.wcode, p.w0.base, p.w0.len, p.w0.code) = (frame, 0, p.window, frame, 0);
            (self.pool_now, self.base) = (pool, p.window);
            return;
        }
        let (window, wlen) = (self.pools[pool].window, self.pools[pool].wlen);
        if wlen < code.frame && window != super::window::NO_WINDOW && window + wlen == self.regs.len() {
            // the window lies last so it grows in place as `place_window` would grow it
            let (need, cap) = (window + code.frame, self.regs.capacity());
            if need > cap { self.regs.reserve_exact(need.max(if cap <= 256 { cap + cap / 2 } else { cap + cap / 4 }) - self.regs.len()); }
            let grow = code.frame - wlen;
            self.regs.extend_from_slice(&UNDEF[..grow.min(UNDEF.len())]);
            if grow > UNDEF.len() { self.regs.resize(need, Val::undef()); }
            self.pools[pool].wlen = code.frame;
        } else if wlen < code.frame {
            self.place_window(pool, code.frame);
        }
        self.pools[pool].wcode = 0;
        self.note_window(pool);
        self.run_in(pool);
    }

    /* the top a frame of `code` parking to resume at `ip` keeps */
    #[inline]
    pub(crate) fn parked_top(&self, code: &Code, ip: usize) -> u32 {
        // code over rows keeps its top while it holds a row and else none
        if code.rows != 0 { return if self.top > u32::from(code.rows) { self.top } else { 0 }; }
        // a lowered frame with a row in use keeps its top
        if self.top != 0 { return self.top; }
        // a raw park resumes in the code it parked in as it lies
        if code.slots == 0 || ip & crate::vm::frame::RESUME_RAW != 0 { return 0; }
        match code.entry_rows(ip) { 0 => 0, rows => (code.slots as usize + rows) as u32 }
    }

    /* the top lowered `code` resuming at compiler instruction `ip` takes a value onto */
    #[inline]
    pub(crate) fn entry_top(&self, code: &Code, ip: usize) -> u32 {
        if self.top != 0 || code.rows != 0 { return self.top.max(code.rows as u32); }
        (code.slots as usize + code.entry_rows(ip)) as u32
    }

    /* sets the top for `pool` running its code afresh or none for lowered code */
    #[inline]
    pub(crate) fn top_fresh(&mut self, pool: usize) {
        let code = CodePtr::from_word(self.head(self.head_at(pool) + 4));
        self.top = code.get().rows as u32;
    }

    /* lays `code` to resume at `ip` putting the held value or `v` where it goes */
    #[inline(never)]
    pub(crate) fn resume_at(&mut self, chunk: &SSAChunk, code: &Code, pool: usize, ip: usize, v: Option<Val>) -> usize {
        let raw = ip & crate::vm::frame::RESUME_RAW != 0;
        let j = if raw { ip & !crate::vm::frame::RESUME_RAW } else { code.lowered(ip) };
        if code.rows != 0 {
            if !self.laid(code, pool) { self.lay_rows(code, pool); }
            // a wait whose value goes nowhere left a row its resume drops
            if j > 0 && void_call(code, chunk, j - 1) && self.top as usize == code.rows as usize + rows_at(code, chunk, j) + 1 { self.top -= 1; }
            if let Some(v) = v { self.push(v); }
            self.woke_for(code, j);
            return j;
        }
        let ins = code.ins.get(j).copied();
        let top = self.top as usize;
        // the wait's value lies one past its entry rows or in its raw take's register
        let past = match ins { Some(t) if raw && t.op == OpCode::StoreTopR => t.a as usize + 1, _ => code.slots as usize + code.entry_rows(ip) + 1 };
        // read before the window is laid since a constant may lie in that row
        let held = (top != 0 && top == past).then(|| self.regs[self.base + top - 1]);
        if !self.laid(code, pool) { self.grow_frame(chunk, code, pool); }
        // a frame parked in other code goes on over its generators as ordinary loops
        if !raw && code.block.as_deref().is_some_and(|b| !b.loops.is_empty()) { self.enter_loops(code, j); }
        match (ins, held.or(v)) {
            (Some(t), Some(v)) if t.op == OpCode::StoreTopR => {
                self.top = 0;
                if t.x == 0 { self.regs[self.base + t.a as usize] = v; } else { self.scopes[code.module].set_at(t.a as u32, v); }
                j + 1
            }
            (Some(t), Some(v)) if held.is_some() && matches!(t.op, OpCode::ForIter | OpCode::ForIterR) => {
                self.top = 0;
                if let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() && let Some(slot) = self.gen_wait(co).filter(|s| s.is_undef()) { *slot = v; }
                j
            }
            // a row op finds the held value in its row and other code drops it
            _ if held.is_some() => { self.top = 0; j }
            (_, Some(v)) => {
                self.top = self.entry_top(code, ip);
                self.push(v);
                j
            }
            // only a row op keeps its rows on a resume with no value
            (Some(t), None) if t.op != OpCode::RowsBackR => { self.top = 0; j }
            // a row call parked in other code takes its helper's value at its row's start
            (Some(t), None) if self.top == 0 => { self.top = u32::from(t.a); j }
            _ => j,
        }
    }

    /* puts a lowered wait's placeholder at `row` marking a window it overwrote for laying again */
    #[inline(never)]
    pub(crate) fn hold_row(&mut self, code: &Code, row: u32) {
        self.top = row;
        self.push(Val::none());
        if code.rows == 0 && row >= u32::from(code.cbase) && self.pools[self.pool_now].wcode != 0 {
            self.pools[self.pool_now].wcode = 0;
            self.note_window(self.pool_now);
        }
    }

    /* runs row op `w` at `i` over its row giving where the code goes on */
    #[inline(never)]
    pub(crate) fn rows_op(&mut self, w: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let (op, s0, (pops, _)) = (OpCode::ALL[w.x as usize], w.a as usize, lower::row_op_counts(&w));
        // the take after it keeps what the op's own word held past its operand
        let tail = code.ins[i + 1];
        let ins = Ins { op, x: tail.x, a: w.b, b: tail.b, c: tail.c };
        self.top = (s0 + pops as usize) as u32;
        // a call runs as its stack form so it asks for growth and notes callees
        if op == OpCode::Call { return self.call_op(Ins { x: 0, ..ins }, i, code, chunk); }
        // an op inside a section names things in the chunk it was laid from
        let chunk = if code.block.is_some() { self.chunk_at(code, chunk, i) } else { chunk };
        match op {
            OpCode::CallMethod => self.method_rows(Ins { x: 0, ..ins }, i, code, chunk),
            OpCode::CallNative => match self.native_at(i, s0, w.b, ins, code, chunk) {
                Ok(_) => Some(i + 1),
                Err(e) => { self.reg_error = Some(e); None }
            },
            OpCode::ListExtend | OpCode::SetUpdate | OpCode::DictUpdate => self.held_at(Ins { x: 0, ..ins }, i, code, chunk).then_some(i + 1),
            _ => self.calls_at(ins, i, code, chunk),
        }
    }
}

/* whether the native call at `at` of code over rows keeps no value */
pub(crate) fn void_call(code: &Code, chunk: &SSAChunk, at: usize) -> bool {
    code.ins.get(at).is_some_and(|i| i.op == OpCode::CallNative && if i.a & SITE_TABLE == 0 { i.a & SITE_VOID != 0 } else { native_site(i.a, &chunk.native_sites).is_some_and(|s| s.void) })
}

/* the rows code over rows holds as it reaches `at` */
#[cold]
#[inline(never)]
fn rows_at(code: &Code, chunk: &SSAChunk, at: usize) -> usize {
    #[cfg(debug_assertions)]
    if let Some(&d) = code.depths.get(at) { return usize::from(d); }
    let src: alloc::vec::Vec<Instruction> = code.ins.iter().map(|w| Instruction { opcode: w.op, operand: w.a }).collect();
    depth::depths(&src, chunk).ok().and_then(|d| d.get(at).copied()).map_or(usize::MAX, usize::from)
}

/* the code of `chunk` that no rows hold which raises as it runs */
#[cold]
#[inline(never)]
fn unrunnable(chunk: &SSAChunk, kinds: Rc<[Kind]>, module: usize) -> Rc<Code> {
    let mut code = lower::identity(&[Instruction { opcode: OpCode::RaiseR, operand: 0 }], chunk.names.len(), kinds, module);
    if let Some(i) = code.ins.first_mut() { i.x = lower::TOO_DEEP; }
    Rc::new(code)
}

impl<'a> VM<'a> {

    // the operand stack is the running frame's rows from its first row to `top`

    #[inline] pub(crate) fn push(&mut self, v: Val) {
        debug_assert!(self.top != 0, "a push with no rows running");
        self.regs[self.base + self.top as usize] = v;
        self.top += 1;
    }

    #[inline] pub(crate) fn pop(&mut self) -> Result<Val, VmErr> {
        if self.top == 0 { return Err(cold_runtime("stack underflow")); }
        self.top -= 1;
        Ok(self.regs[self.base + self.top as usize])
    }

    #[inline] pub(crate) fn pop2(&mut self) -> Result<(Val, Val), VmErr> {
        let b = self.pop()?; let a = self.pop()?; Ok((a, b))
    }

    #[inline] pub(crate) fn pop_n(&mut self, n: usize) -> Result<Vec<Val>, VmErr> {
        let at = self.under(n)?;
        let vals = self.vals_at(at, n).to_vec();
        self.cut_to(at);
        Ok(vals)
    }

    /* where the next value the operand stack takes goes */
    #[inline(always)] pub(crate) fn sp(&self) -> usize { self.top as usize }

    /* the value at place `i` of the operand stack */
    #[inline(always)] pub(crate) fn slot(&self, i: usize) -> Val { self.regs[self.base + i] }

    /* puts `v` at place `i` of the operand stack */
    #[inline(always)] pub(crate) fn set_slot(&mut self, i: usize, v: Val) { self.regs[self.base + i] = v; }

    /* the `n` values of the operand stack from place `i` */
    #[inline(always)] pub(crate) fn vals_at(&self, i: usize, n: usize) -> &[Val] { &self.regs[self.base + i..self.base + i + n] }

    /* drops the values of the operand stack from place `i` on */
    #[inline(always)] pub(crate) fn cut_to(&mut self, i: usize) { self.top = i as u32; }

    /* the place `k` values under the top of the operand stack or an underflow */
    #[inline(always)] pub(crate) fn under(&self, k: usize) -> Result<usize, VmErr> {
        match self.sp().checked_sub(k) { Some(at) if self.top != 0 => Ok(at), _ => Err(cold_runtime("stack underflow")) }
    }
}
