use crate::parser::{OpCode, Instruction, SSAChunk, ssa_strip};
use alloc::{borrow::Cow, rc::Rc, vec, vec::Vec};
use core::cell::Cell;

use super::scope::Kind;

/* One instruction as the VM runs it, an opcode and up to three operands. */
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ins {
    pub op: OpCode,
    pub x: u8,
    pub a: u16,
    pub b: u16,
    pub c: u16,
}

impl Ins {
    fn of(i: Instruction) -> Self { Self { op: i.opcode, x: 0, a: i.operand, b: 0, c: 0 } }
}

/* What a chunk's lowering may assume about the slots of its frame. */
pub(crate) struct Facts {
    /* Names lower to registers, which a class body's namespace never does. */
    pub regs: bool,
    /* Slots bound before the body runs. */
    pub bound: Vec<bool>,
    /* What each slot is, a local, a closure cell or a module binding. */
    pub kinds: Rc<[Kind]>,
    /* The module whose bindings the globals index. */
    pub module: usize,
}

/* A chunk's code as the VM runs it, mapped back to the compiler's instructions. */
pub(crate) struct Code {
    pub ins: Vec<Ins>,
    /* The compiler instruction each one came from, empty for unchanged code. */
    orig: Vec<u32>,
    /* Where each compiler instruction starts in `ins`, plus one past the end. */
    at: Vec<u32>,
    /* The compiler instruction a run stopped before `ins[i]` resumes from. */
    back: Vec<u32>,
    /* Frame length, the names then the constants then the temporaries. */
    pub frame: usize,
    pub kinds: Rc<[Kind]>,
    pub module: usize,
    /* Back-edges an unlowered frame took, its loop lowers once they reach `HOT_LOOP`. */
    pub hot: Cell<u32>,
    /* The sites its instructions learned, which every frame running it shares. */
    pub cache: super::cache::OpcodeCache,
}

/* Back-edges after which an unlowered loop carries on as lowered code. */
pub(crate) const HOT_LOOP: u32 = 16;

impl Code {
    /* Whether this is the compiler's code as written, each ip its own. */
    #[inline]
    pub fn unchanged(&self) -> bool { self.at.is_empty() }

    /* Where compiler instruction `ip` starts, the end past the last one. */
    #[inline]
    pub fn lowered(&self, ip: usize) -> usize {
        if self.at.is_empty() { ip.min(self.ins.len()) } else { self.at.get(ip).map_or(self.ins.len(), |&i| i as usize) }
    }

    /* The compiler instruction a run stopped before `ip` resumes from. */
    #[inline]
    pub fn resume(&self, ip: usize) -> usize {
        if self.back.is_empty() { ip.min(self.ins.len()) } else { self.back.get(ip).map_or(self.at.len() - 1, |&i| i as usize) }
    }

    /* The compiler instruction `ins[i]` came from. */
    #[inline]
    pub fn orig(&self, i: usize) -> u32 { self.orig.get(i).copied().unwrap_or(i as u32) }
}

/* The compiler's own code, unchanged so each ip it marks is that instruction. */
pub(crate) fn identity(src: &[Instruction], names: usize, kinds: Rc<[Kind]>, module: usize) -> Code {
    let mut ins: Vec<Ins> = src.iter().map(|&i| Ins::of(i)).collect();
    // A fused call keeps both halves, the first carrying the counts past the second.
    for k in 1..ins.len() {
        if ins[k].op == OpCode::CallMethodArgs && ins[k - 1].op == OpCode::CallMethod { ins[k - 1].b = ins[k].a; ins[k - 1].x = 1; }
    }
    let cache = super::cache::OpcodeCache::new(ins.len());
    Code { ins, orig: Vec::new(), at: Vec::new(), back: Vec::new(), frame: names, kinds, module, hot: Cell::new(0), cache }
}

// Past this many pending values the lowering spills them, so temporaries stay few.
const MAX_PENDING: usize = 16;
// Values pending under a register call it copies past itself, more go to the stack with the call as before.
const MAX_UNDER: usize = 1;

/* A stack value kept in a register until the stack needs it. */
#[derive(Clone, Copy, PartialEq)]
struct Entry {
    r: u16,
    // A local read put off until its use, bound by then.
    name: bool,
}

impl Entry {
    const fn reg(r: u16) -> Self { Self { r, name: false } }
}

/* Lowers fused stack code to register code, None when the frame cannot fit. */
pub(crate) fn lower(src: &[Instruction], chunk: &SSAChunk, facts: &Facts) -> Option<Code> {
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = src.len();
    let targets = entered(src);
    let mut starts = targets.clone();
    starts[0] = true;
    for (i, ins) in src.iter().enumerate() {
        if ins.opcode.is_jump() || ends_flow(ins.opcode) { starts[i + 1] = true; }
    }
    let mut deleted = Vec::new();
    for ins in src.iter().filter(|i| i.opcode == OpCode::Del) {
        deleted.resize(names, false);
        if let Some(s) = deleted.get_mut(ins.operand as usize) { *s = true; }
    }
    let bound_at = facts.regs.then(|| bound_on_entry(src, chunk, names, facts, &starts));
    let (appends, resets) = if facts.regs { append_loops(src, facts) } else { (Vec::new(), Vec::new()) };
    let cbase = names;
    let tbase = names + consts + 3;
    let mut l = Lower {
        src, chunk, facts, targets: &targets, deleted, names, cbase, consts, tbase,
        out: Vec::with_capacity(len), orig: Vec::with_capacity(len + 1), at: vec![0; len + 1], back: Vec::with_capacity(len + 1),
        pending: Vec::with_capacity(MAX_PENDING), patches: Vec::new(), temps: 0, last: None, cur: 0, takes: false, under: Vec::new(), quiet: 0, bound: Vec::new(),
        appends,
    };
    let mut i = 0;
    while i < len {
        // What falls into a block lands on the stack under the instruction before it.
        if starts[i] {
            l.flush();
            l.quiet = i as u32;
            match &bound_at {
                Some(b) => b.entry(i, &mut l.bound),
                None => { l.bound.clear(); l.bound.resize(names.div_ceil(64), 0); }
            }
            // Entering a loop that appends to a local gives up the string it owned, a back-edge skips this.
            for &(_, slot) in resets.iter().filter(|r| r.0 == i) { l.emit(Ins { op: OpCode::OwnResetR, x: 0, a: slot, b: 0, c: 0 }); }
        }
        l.cur = i as u32;
        l.at[i] = l.out.len() as u32;
        if core::mem::take(&mut l.takes) {
            // What the call left under it comes back off the stack, into its temporaries since a resume in unlowered code never wrote them.
            let under = core::mem::take(&mut l.under);
            l.pending.extend_from_slice(&under);
            let t = l.temp();
            l.produce(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
            for e in under.iter().rev() {
                l.emit(if e.name { Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 } } else { Ins { op: OpCode::StoreTopR, x: 0, a: e.r, b: 0, c: 0 } });
            }
        }
        let before = l.out.len();
        let step = l.one(i);
        // A fused group's later instructions start where its last instruction does.
        for k in 1..step { l.at[i + k] = before.max(l.out.len().saturating_sub(1)) as u32; }
        for (k, &ins) in src[i..i + step].iter().enumerate() { transfer(&mut l.bound, ins, i + k, chunk); }
        i += step;
        if l.pending.is_empty() { l.quiet = i as u32; }
    }
    l.flush();
    l.at[len] = l.out.len() as u32;
    if l.out.len() >= u16::MAX as usize { return None; }
    for &p in &l.patches {
        let t = l.out[p].a as usize;
        l.out[p].a = if t <= len { l.at[t] as u16 } else { u16::MAX };
    }
    let frame = if facts.regs { tbase + l.temps } else { names };
    // Registers are u16 operands.
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(len as u32);
    l.back.push(len as u32);
    let cache = super::cache::OpcodeCache::new(l.out.len());
    Some(Code { ins: l.out, orig: l.orig, at: l.at, back: l.back, frame, kinds: facts.kinds.clone(), module: facts.module, hot: Cell::new(0), cache })
}

/* Instructions a jump, handler or finished `finally` enters, which no fusion may swallow. */
fn entered(src: &[Instruction]) -> Vec<bool> {
    let mut entered = vec![false; src.len() + 1];
    for (k, ins) in src.iter().enumerate() {
        if ins.opcode.is_jump() && (ins.operand as usize) <= src.len() { entered[ins.operand as usize] = true; }
        // Unwind::Goto resumes at the instruction after UnwindFinally.
        if ins.opcode == OpCode::UnwindFinally { entered[k + 1] = true; }
    }
    entered
}

/* The InPlaceAdd instructions that append to a local a loop uses for nothing else, and the loop heads with the slots they reset. */
fn append_loops(src: &[Instruction], facts: &Facts) -> (Vec<bool>, Vec<(usize, u16)>) {
    let mut appends = vec![false; src.len()];
    let mut resets = Vec::new();
    let refers = |ins: &Instruction| matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi);
    for (end, back) in src.iter().enumerate().filter(|(j, b)| b.opcode == OpCode::Jump && (b.operand as usize) <= *j) {
        let head = back.operand as usize;
        // A jump from outside into the loop would skip the reset at its head.
        let outside = |k: usize| k < head || k > end;
        if src.iter().enumerate().any(|(k, ins)| outside(k) && ins.opcode.is_jump() && (head..=end).contains(&(ins.operand as usize))) { continue; }
        // Each local an append in the loop stores, looked at once.
        let mut slots: Vec<u16> = (head..end).filter(|&k| src[k].opcode == OpCode::InPlaceAdd && src[k + 1].opcode == OpCode::StoreName)
            .map(|k| src[k + 1].operand).filter(|&s| facts.kinds.get(s as usize) == Some(&Kind::Local)).collect();
        slots.sort_unstable();
        slots.dedup();
        for slot in slots {
            // Every use of the slot in the loop is a load an append adds to and that append's store.
            let uses: Vec<usize> = (head..=end).filter(|&u| refers(&src[u]) && src[u].operand == slot).collect();
            let paired = uses.chunks(2).all(|p| matches!(p, [load, st] if src[*load].opcode == OpCode::LoadName && src[*st].opcode == OpCode::StoreName
                && *st >= 1 && src[*st - 1].opcode == OpCode::InPlaceAdd));
            if !uses.len().is_multiple_of(2) || !paired { continue; }
            for p in uses.chunks(2) { appends[p[1] - 1] = true; }
            resets.push((head, slot));
        }
    }
    (appends, resets)
}

/* No instruction after it runs from it. */
fn ends_flow(op: OpCode) -> bool { matches!(op, OpCode::Jump | OpCode::ReturnValue | OpCode::Raise | OpCode::RaiseFrom) }

fn transfer(bound: &mut [u64], ins: Instruction, k: usize, chunk: &SSAChunk) {
    let s = ins.operand as usize;
    let has = |b: &[u64], r: u16| b.get(r as usize / 64).is_some_and(|w| w & 1 << (r % 64) != 0);
    // A Phi binds its slot only when one of its sources is bound.
    if ins.opcode == OpCode::Phi && !chunk.phi_map.get(k).and_then(|&p| chunk.phi_sources.get(p)).is_some_and(|&(a, b)| has(bound, a) || has(bound, b)) { return; }
    let Some(w) = bound.get_mut(s / 64) else { return };
    match ins.opcode {
        OpCode::StoreName | OpCode::Phi => *w |= 1 << (s % 64),
        OpCode::Del => *w &= !(1 << (s % 64)),
        _ => {}
    }
}

/* The slots bound on every path into each block, a bit row per block. */
struct Bound {
    words: usize,
    // Each block start's row, u32::MAX for any other instruction.
    rows: Vec<u32>,
    bits: Vec<u64>,
    // Whether any path reaches the block yet.
    seen: Vec<bool>,
}

impl Bound {
    /* The slots bound entering the block at `i`, none when no path reaches it. */
    fn entry(&self, i: usize, out: &mut Vec<u64>) {
        out.clear();
        match self.rows.get(i) {
            Some(&r) if r != u32::MAX && self.seen[r as usize] => out.extend_from_slice(&self.bits[r as usize * self.words..][..self.words]),
            _ => out.resize(self.words, 0),
        }
    }

    /* Narrows the block at `t` to what `s` binds, true when that changed it. */
    fn merge(&mut self, t: usize, s: &[u64]) -> bool {
        let Some(&r) = self.rows.get(t) else { return false };
        if r == u32::MAX { return false; }
        let row = &mut self.bits[r as usize * self.words..][..self.words];
        if !core::mem::replace(&mut self.seen[r as usize], true) { row.copy_from_slice(s); return true; }
        let mut changed = false;
        for (o, n) in row.iter_mut().zip(s) { if *o & n != *o { *o &= n; changed = true; } }
        changed
    }
}

fn bound_on_entry(src: &[Instruction], chunk: &SSAChunk, names: usize, facts: &Facts, starts: &[bool]) -> Bound {
    let len = src.len();
    let words = names.div_ceil(64);
    let mut rows = vec![u32::MAX; starts.len()];
    let mut blocks = 0;
    for (r, _) in rows.iter_mut().zip(starts).filter(|(_, s)| **s) { *r = blocks; blocks += 1; }
    let mut b = Bound { words, rows, bits: vec![0; blocks as usize * words], seen: vec![false; blocks as usize] };
    let mut s = vec![0u64; words];
    for (k, _) in facts.bound.iter().enumerate().filter(|(_, b)| **b) { s[k / 64] |= 1 << (k % 64); }
    b.merge(0, &s);
    let mut work = vec![0usize];
    while let Some(start) = work.pop() {
        b.entry(start, &mut s);
        let mut i = start;
        while i < len {
            let ins = src[i];
            transfer(&mut s, ins, i, chunk);
            if ins.opcode.is_jump() && (ins.operand as usize) <= len && b.merge(ins.operand as usize, &s) { work.push(ins.operand as usize); }
            if ends_flow(ins.opcode) { break; }
            i += 1;
            if starts[i] { if b.merge(i, &s) { work.push(i); } break; }
        }
    }
    b
}

struct Lower<'c> {
    src: &'c [Instruction],
    chunk: &'c SSAChunk,
    facts: &'c Facts,
    targets: &'c [bool],
    deleted: Vec<bool>,
    names: usize,
    cbase: usize,
    consts: usize,
    tbase: usize,
    out: Vec<Ins>,
    orig: Vec<u32>,
    at: Vec<u32>,
    back: Vec<u32>,
    pending: Vec<Entry>,
    // Lowered jumps whose target is still a compiler index.
    patches: Vec<usize>,
    temps: usize,
    // The last instruction, when it wrote a fresh temporary a store may take over.
    last: Option<usize>,
    cur: u32,
    // A register call left its result on the stack, which the next instruction takes into a register first.
    takes: bool,
    // The values pending under that call, copied to the stack in case it suspends.
    under: Vec<Entry>,
    // The InPlaceAdd instructions that append to a local their loop only ever appends to.
    appends: Vec<bool>,
    // The first compiler instruction whose work no emitted instruction has finished.
    quiet: u32,
    bound: Vec<u64>,
}

impl Lower<'_> {
    /* Lowers instruction `i`, the count of compiler instructions it took. */
    fn one(&mut self, i: usize) -> usize {
        let ins = self.src[i];
        let op = ins.operand;
        let regs = self.facts.regs;
        match ins.opcode {
            OpCode::LoadName if regs && (op as usize) < self.names => self.load_name(op),
            OpCode::LoadConst if regs && (op as usize) < self.consts => self.push(Entry::reg((self.cbase + op as usize) as u16)),
            OpCode::LoadNone if regs => self.push(Entry::reg((self.cbase + self.consts) as u16)),
            OpCode::LoadTrue if regs => self.push(Entry::reg((self.cbase + self.consts + 1) as u16)),
            OpCode::LoadFalse if regs => self.push(Entry::reg((self.cbase + self.consts + 2) as u16)),
            OpCode::StoreName if regs && (op as usize) < self.names && !self.pending.is_empty() => self.store_name(op),
            // A value left on the stack reaches its variable in the register loop.
            OpCode::StoreName if regs && (op as usize) < self.names => match self.kind(op) {
                Some(Kind::Local) => self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: op, b: 0, c: 0 }),
                Some(Kind::Global(g)) if self.builtin_flag(op) == 0 => self.emit(Ins { op: OpCode::StoreTopR, x: 1, a: g as u16, b: 0, c: 0 }),
                _ => self.emit_orig(ins),
            },
            // A `global` name reads and writes its module binding by index.
            OpCode::LoadGlobal if regs && let Some(Kind::Global(g)) = self.kind(op) => {
                let t = self.temp();
                self.produce(Ins { op: OpCode::LoadGlobalR, x: 0, a: t, b: g as u16, c: 0 });
            }
            OpCode::StoreGlobal if regs && !self.pending.is_empty() && let Some(Kind::Global(g)) = self.kind(op) => {
                let v = self.pop();
                // A builtin's store redirects its fused calls.
                self.emit(Ins { op: OpCode::StoreGlobalR, x: self.builtin_flag(op), a: g as u16, b: v.r, c: 0 });
            }
            OpCode::PopTop if !self.pending.is_empty() => { self.pending.pop(); }
            // A declaration does nothing at run time.
            OpCode::Global | OpCode::Nonlocal => {}
            OpCode::Dup if !self.pending.is_empty() => { let e = self.pending[self.pending.len() - 1]; self.push(e); }
            OpCode::Swap if self.pending.len() >= 2 => { let k = self.pending.len(); self.pending.swap(k - 1, k - 2); }
            OpCode::Rot3 if self.pending.len() >= 3 => {
                let k = self.pending.len();
                self.pending[k - 3..].rotate_left(1);
            }
            // A value a call left on the stack moves into a temporary, so the store after takes registers.
            OpCode::Swap | OpCode::Rot3 if regs && self.pending.len() + 1 == if ins.opcode == OpCode::Swap { 2 } else { 3 } => {
                let t = self.temp();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            OpCode::Dup2 if self.pending.len() >= 2 => {
                let k = self.pending.len();
                let (a, b) = (self.pending[k - 2], self.pending[k - 1]);
                self.push(a);
                self.push(b);
            }
            // `s += t` on a local its loop only appends to grows the string it owns.
            OpCode::InPlaceAdd if self.appends.get(i).copied().unwrap_or(false) && self.pending.len() >= 2
                && self.pending[self.pending.len() - 2] == (Entry { r: self.src[i + 1].operand, name: true }) => {
                let (_, t) = self.pop2();
                self.settle();
                self.emit(Ins { op: OpCode::AppendR, x: 0, a: self.src[i + 1].operand, b: t.r, c: 0 });
                return 2;
            }
            code if binary_form(code).is_some() && self.pending.len() >= 2 && op < AUGMENTED as u16 => return self.binary(i, code, op as u8),
            OpCode::Minus | OpCode::Not if !self.pending.is_empty() => {
                let v = self.pop();
                let t = self.temp();
                let form = if ins.opcode == OpCode::Minus { OpCode::MinusR } else { OpCode::NotR };
                self.produce(Ins { op: form, x: 0, a: t, b: v.r, c: 0 });
            }
            OpCode::JumpIfFalse if !self.pending.is_empty() => {
                let c = self.pop();
                self.flush();
                self.jump(OpCode::JumpIfFalseR, op, c.r, 0);
            }
            // The accumulator stays on the stack, what it takes comes from registers.
            OpCode::ListAppend | OpCode::SetAdd if !self.pending.is_empty() => {
                let v = self.pop();
                self.flush();
                let form = if ins.opcode == OpCode::ListAppend { OpCode::ListAppendR } else { OpCode::SetAddR };
                self.emit(Ins { op: form, x: 0, a: 0, b: v.r, c: v.r });
            }
            OpCode::MapAdd if self.pending.len() >= 2 => {
                let (k, v) = self.pop2();
                self.flush();
                self.emit(Ins { op: OpCode::MapAddR, x: 0, a: 0, b: k.r, c: v.r });
            }
            // A short-circuit feeding a test jumps where the test would, its value never stacked.
            OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop if !self.pending.is_empty()
                && let Some(test) = self.threaded_test(ins) => {
                let c = self.pop();
                self.flush();
                if ins.opcode == OpCode::JumpIfFalseOrPop {
                    self.jump(OpCode::JumpIfFalseR, self.src[test].operand, c.r, 0);
                } else {
                    self.jump(OpCode::JumpIfFalseR, test as u16 + 1, c.r, 0);
                    if let Some(j) = self.out.last_mut() { j.x = 1; }
                }
            }
            OpCode::ReturnValue if !self.pending.is_empty() => {
                let v = self.pop();
                // What stays below the value only leaves with the frame.
                self.pending.clear();
                self.emit(Ins { op: OpCode::ReturnR, x: 0, a: 0, b: v.r, c: 0 });
            }
            // One positional and no spread, which fused `len` carries as its bare count.
            OpCode::CallLen if op == 1 && !self.pending.is_empty() => {
                let o = self.pop();
                let t = self.temp();
                self.produce(Ins { op: OpCode::LenR, x: 0, a: t, b: o.r, c: 0 });
            }
            OpCode::GetItem if self.pending.len() >= 2 => {
                let (o, k) = self.pop2();
                let t = self.temp();
                self.produce(Ins { op: OpCode::GetItemR, x: 0, a: t, b: o.r, c: k.r });
            }
            OpCode::StoreItem if self.pending.len() >= 3 => {
                let (k, v) = self.pop2();
                let o = self.pop();
                self.emit(Ins { op: OpCode::StoreItemR, x: 0, a: o.r, b: k.r, c: v.r });
            }
            OpCode::LoadAttr if !self.pending.is_empty() => {
                let o = self.pop();
                let t = self.temp();
                self.produce(Ins { op: OpCode::GetAttrR, x: 0, a: t, b: o.r, c: op });
            }
            OpCode::StoreAttr if self.pending.len() >= 2 => {
                let (o, v) = self.pop2();
                self.emit(Ins { op: OpCode::SetAttrR, x: 0, a: o.r, b: v.r, c: op });
            }
            // The loop variable takes each item straight from the iterator.
            OpCode::ForIter if regs
                && let Some(&store) = self.src.get(i + 1)
                && store.opcode == OpCode::StoreName
                && (store.operand as usize) < self.names
                && !self.targets[i + 1]
                && let Some(target) = match self.kind(store.operand) {
                    Some(Kind::Local) => Some((0, store.operand)),
                    Some(Kind::Global(g)) if self.builtin_flag(store.operand) == 0 => Some((1, g as u16)),
                    _ => None,
                } => {
                self.settle();
                self.patches.push(self.out.len());
                self.emit(Ins { op: OpCode::ForIterR, x: target.0, a: op, b: target.1, c: 0 });
                return 2;
            }
            // A tuple built only to unpack hands its values straight to the stores.
            OpCode::BuildTuple if (op as usize) <= self.pending.len()
                && op > 0
                && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::UnpackSequence && n.operand == op)
                && !self.targets[i + 1] => {
                let k = self.pending.len() - op as usize;
                self.pending[k..].reverse();
                return 2;
            }
            // Unpacking straight into locals writes each one, nothing left on the stack between.
            OpCode::UnpackSequence if (1..=3).contains(&op) && regs && self.unpacks_to_locals(i, op as usize) => {
                self.settle();
                let t = |k: usize| self.src.get(i + 1 + k).map_or(0, |s| s.operand);
                self.emit(Ins { op: OpCode::UnpackR, x: op as u8, a: t(0), b: t(1), c: t(2) });
                return 1 + op as usize;
            }
            // A call of up to five positionals reads its operands from registers, what is pending under it goes to the stack first.
            OpCode::Call if regs && op <= 5 && self.pending.len() > op as usize && self.pending.len() <= op as usize + 1 + MAX_UNDER && i + 1 < self.src.len() && !self.targets[i + 1] => {
                self.flush_under(op as usize + 1);
                let r = |k: usize| self.pending.get(k).map_or(0, |e| e.r);
                let call = Ins { op: OpCode::CallR, x: op as u8, a: r(0), b: r(1), c: r(2) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(3), b: r(4), c: r(5) };
                self.pending.clear();
                self.settle();
                self.emit(call);
                if wide(call) { self.emit(rest); }
                self.takes = true;
            }
            // A method call of up to four positionals reads its receiver and arguments from registers, what is pending under it goes to the stack first.
            OpCode::CallMethod if regs && let Some(args) = self.src.get(i + 1) && args.opcode == OpCode::CallMethodArgs
                && args.operand <= 4 && self.pending.len() > args.operand as usize && self.pending.len() <= args.operand as usize + 1 + MAX_UNDER && i + 2 < self.src.len() && !self.targets[i + 2] => {
                self.flush_under(args.operand as usize + 1);
                let r = |k: usize| self.pending.get(k).map_or(0, |e| e.r);
                let call = Ins { op: OpCode::CallMethodR, x: args.operand as u8, a: op, b: r(0), c: r(1) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
                self.pending.clear();
                self.settle();
                self.emit(call);
                if wide(call) { self.emit(rest); }
                self.takes = true;
                return 2;
            }
            OpCode::CallMethod if self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::CallMethodArgs) => {
                self.settle();
                self.emit(Ins { op: OpCode::CallMethod, x: 0, a: op, b: self.src[i + 1].operand, c: 0 });
                return 2;
            }
            _ => {
                self.settle();
                self.emit_orig(ins);
            }
        }
        1
    }

    fn kind(&self, s: u16) -> Option<Kind> { self.facts.kinds.get(s as usize).copied() }

    /* Whether the `n` instructions after `i` store into plain locals no jump lands between. */
    fn unpacks_to_locals(&self, i: usize, n: usize) -> bool {
        (1..=n).all(|k| self.src.get(i + k).is_some_and(|s| s.opcode == OpCode::StoreName && (s.operand as usize) < self.names
            && self.kind(s.operand) == Some(Kind::Local) && !self.targets[i + k]))
    }

    /* The test a short-circuit reaches through a chain of its own kind. */
    fn threaded_test(&self, ins: Instruction) -> Option<usize> {
        let mut t = ins.operand as usize;
        for _ in 0..16 {
            match self.src.get(t)?.opcode {
                OpCode::JumpIfFalse => return Some(t),
                op if op == ins.opcode => t = self.src[t].operand as usize,
                _ => return None,
            }
        }
        None
    }

    /* The store flag a builtin's name carries, since rebinding one redirects its fused calls. */
    fn builtin_flag(&self, s: u16) -> u8 {
        let bare = self.chunk.names.get(s as usize).map_or("", |n| ssa_strip(n));
        if crate::value::NativeFnId::from_name(bare).is_some() { STORE_NOTES_BUILTIN } else { 0 }
    }

    /* A local's read waits for its use, since only the frame writes it. */
    fn deferrable(&self, s: usize) -> bool {
        !self.deleted.get(s).is_some_and(|&d| d) && self.bound.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0)
    }

    fn load_name(&mut self, s: u16) {
        let read = match self.kind(s) {
            Some(Kind::Global(g)) => Ins { op: OpCode::LoadGlobalR, x: 0, a: 0, b: g as u16, c: 0 },
            Some(Kind::Cell) => Ins { op: OpCode::LoadCellR, x: 0, a: 0, b: s, c: 0 },
            _ if self.deferrable(s as usize) => return self.push(Entry { r: s, name: true }),
            // Read now, so its error or its value is the one of this point.
            _ => Ins { op: OpCode::Move, x: 1, a: 0, b: s, c: 0 },
        };
        let t = self.temp();
        self.produce(Ins { a: t, ..read });
    }

    fn store_name(&mut self, s: u16) {
        match self.kind(s) {
            Some(Kind::Global(g)) => {
                let v = self.pop();
                let x = self.builtin_flag(s);
                return self.emit(Ins { op: OpCode::StoreGlobalR, x, a: g as u16, b: v.r, c: 0 });
            }
            Some(Kind::Cell) => {
                let v = self.pop();
                return self.emit(Ins { op: OpCode::StoreCellR, x: 0, a: s, b: v.r, c: 0 });
            }
            _ => {}
        }
        let spilled = self.spill(s);
        let top = self.pop();
        let fresh = !top.name && top.r as usize >= self.tbase && !self.pending.contains(&top);
        match self.last {
            Some(k) if !spilled && fresh && k + 1 == self.out.len() && self.out[k].a == top.r => self.out[k].a = s,
            _ => self.emit(Ins { op: OpCode::Move, x: top.name as u8, a: s, b: top.r, c: 0 }),
        }
        self.last = None;
    }

    /* Pending reads of `s` copy its value out before a store rebinds it. */
    fn spill(&mut self, s: u16) -> bool {
        if !self.pending.iter().any(|e| e.name && e.r == s) { return false; }
        let t = self.temp();
        self.emit(Ins { op: OpCode::Move, x: 1, a: t, b: s, c: 0 });
        for e in &mut self.pending { if e.name && e.r == s { *e = Entry::reg(t); } }
        true
    }

    /* The two topmost pending values, the deeper one first. */
    fn pop2(&mut self) -> (Entry, Entry) {
        let b = self.pop();
        let a = self.pop();
        (a, b)
    }

    /* The topmost pending value. */
    fn pop(&mut self) -> Entry { self.pending.pop().unwrap_or(Entry::reg(0)) }

    fn binary(&mut self, i: usize, code: OpCode, operand: u8) -> usize {
        let (a, b) = self.pop2();
        if let Some(branch) = branch_form(code)
            && let Some(next) = self.src.get(i + 1)
            && next.opcode == OpCode::JumpIfFalse
            && !self.targets[i + 1]
        {
            self.flush();
            self.jump(branch, next.operand, a.r, b.r);
            return 2;
        }
        let t = self.temp();
        let augmented = if matches!(code, OpCode::InPlaceBitAnd | OpCode::InPlaceBitOr | OpCode::InPlaceBitXor) { AUGMENTED } else { 0 };
        self.produce(Ins { op: binary_form(code).unwrap_or(code), x: operand | augmented, a: t, b: a.r, c: b.r });
        1
    }

    /* Emits an instruction writing temporary `ins.a` and leaves that temporary pending. */
    fn produce(&mut self, ins: Ins) {
        if self.pending.len() >= MAX_PENDING { self.flush(); }
        self.emit(ins);
        self.last = Some(self.out.len() - 1);
        self.pending.push(Entry::reg(ins.a));
    }

    /* The lowest temporary no pending value holds. */
    fn temp(&mut self) -> u16 {
        let mut t = self.tbase;
        while self.pending.iter().any(|e| !e.name && e.r as usize == t) { t += 1; }
        self.temps = self.temps.max(t + 1 - self.tbase);
        t as u16
    }

    fn push(&mut self, e: Entry) {
        if self.pending.len() >= MAX_PENDING { self.flush(); }
        self.pending.push(e);
    }

    /* Flushes before an opcode that reads only the stack, so it resumes from itself. */
    fn settle(&mut self) {
        self.flush();
        self.quiet = self.cur;
    }

    /* Copies the pending values under the top `keep` to the stack, deepest first, so a call that suspends resumes at itself. */
    fn flush_under(&mut self, keep: usize) {
        let under = self.pending.len() - keep;
        if under == 0 { return; }
        self.under = self.pending.drain(..under).collect();
        let below = core::mem::take(&mut self.under);
        self.push_regs(&below);
        self.under = below;
    }

    /* Puts every pending value on the stack, deepest first. */
    fn flush(&mut self) {
        if self.pending.is_empty() { return; }
        let mut pending = core::mem::take(&mut self.pending);
        self.push_regs(&pending);
        // The buffer comes back, so lowering allocates it once.
        pending.clear();
        self.pending = pending;
    }

    fn push_regs(&mut self, regs: &[Entry]) {
        for group in regs.chunks(3) {
            let r = |k: usize| group.get(k).map_or(0, |e| e.r);
            self.emit(Ins { op: OpCode::PushRegs, x: group.len() as u8, a: r(0), b: r(1), c: r(2) });
        }
        if !regs.is_empty() { self.last = None; }
    }

    fn jump(&mut self, op: OpCode, target: u16, b: u16, c: u16) {
        self.patches.push(self.out.len());
        self.emit(Ins { op, x: 0, a: target, b, c });
    }

    fn emit_orig(&mut self, ins: Instruction) {
        if matches!(ins.opcode, OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop | OpCode::ForIter) {
            self.patches.push(self.out.len());
        }
        self.emit(Ins::of(ins));
    }

    fn emit(&mut self, ins: Ins) {
        self.out.push(ins);
        self.orig.push(self.cur);
        self.back.push(self.quiet);
        self.last = None;
    }
}

/* Whether a register call holds more operands than fit it, the ArgsR after it holding the rest. */
#[inline(always)]
pub(crate) fn wide(call: Ins) -> bool { call.x > if call.op == OpCode::CallR { 2 } else { 1 } }

/* The registers of a register call, the callee or receiver first, and how many of them it passes. */
#[inline(always)]
pub(crate) fn call_regs(call: Ins, rest: Ins) -> ([u16; 6], usize) {
    if call.op == OpCode::CallR { ([call.a, call.b, call.c, rest.a, rest.b, rest.c], call.x as usize + 1) }
    else { ([call.b, call.c, rest.a, rest.b, rest.c, 0], call.x as usize + 1) }
}

/* The StoreGlobalR flag of a store into a builtin name, which redirects its fused calls. */
pub(crate) const STORE_NOTES_BUILTIN: u8 = 1;

/* The register form of a binary opcode. */
fn binary_form(op: OpCode) -> Option<OpCode> {
    Some(match op {
        OpCode::Add => OpCode::AddR, OpCode::Sub => OpCode::SubR, OpCode::Mul => OpCode::MulR,
        OpCode::Div => OpCode::DivR, OpCode::Mod => OpCode::ModR, OpCode::FloorDiv => OpCode::FloorDivR,
        OpCode::InPlaceAdd => OpCode::InPlaceAddR, OpCode::InPlaceSub => OpCode::InPlaceSubR,
        OpCode::Eq => OpCode::EqR, OpCode::NotEq => OpCode::NotEqR, OpCode::Lt => OpCode::LtR,
        OpCode::LtEq => OpCode::LtEqR, OpCode::Gt => OpCode::GtR, OpCode::GtEq => OpCode::GtEqR,
        OpCode::In => OpCode::InR, OpCode::NotIn => OpCode::NotInR, OpCode::Is => OpCode::IsR, OpCode::IsNot => OpCode::IsNotR,
        OpCode::BitAnd | OpCode::InPlaceBitAnd => OpCode::BitAndR, OpCode::BitOr | OpCode::InPlaceBitOr => OpCode::BitOrR,
        OpCode::BitXor | OpCode::InPlaceBitXor => OpCode::BitXorR, OpCode::Shl => OpCode::ShlR, OpCode::Shr => OpCode::ShrR,
        OpCode::Pow => OpCode::PowR,
        _ => return None,
    })
}

/* The `x` bit marking a bitwise register form from an augmented opcode. */
pub(crate) const AUGMENTED: u8 = 0x80;

/* The compare-and-branch form of a comparison. */
fn branch_form(op: OpCode) -> Option<OpCode> {
    Some(match op {
        OpCode::Eq => OpCode::JumpUnlessEq, OpCode::NotEq => OpCode::JumpUnlessNotEq, OpCode::Lt => OpCode::JumpUnlessLt,
        OpCode::LtEq => OpCode::JumpUnlessLtEq, OpCode::Gt => OpCode::JumpUnlessGt, OpCode::GtEq => OpCode::JumpUnlessGtEq,
        _ => return None,
    })
}

/* The stack opcode a register form falls back to. */
pub(crate) fn stack_form(op: OpCode) -> OpCode {
    match op {
        OpCode::AddR => OpCode::Add, OpCode::SubR => OpCode::Sub, OpCode::MulR => OpCode::Mul,
        OpCode::DivR => OpCode::Div, OpCode::ModR => OpCode::Mod, OpCode::FloorDivR => OpCode::FloorDiv,
        OpCode::InPlaceAddR => OpCode::InPlaceAdd, OpCode::InPlaceSubR => OpCode::InPlaceSub,
        OpCode::BitAndR => OpCode::BitAnd, OpCode::BitOrR => OpCode::BitOr, OpCode::BitXorR => OpCode::BitXor,
        OpCode::ShlR => OpCode::Shl, OpCode::ShrR => OpCode::Shr, OpCode::PowR => OpCode::Pow,
        OpCode::EqR | OpCode::JumpUnlessEq => OpCode::Eq, OpCode::NotEqR | OpCode::JumpUnlessNotEq => OpCode::NotEq,
        OpCode::LtR | OpCode::JumpUnlessLt => OpCode::Lt, OpCode::LtEqR | OpCode::JumpUnlessLtEq => OpCode::LtEq,
        OpCode::GtR | OpCode::JumpUnlessGt => OpCode::Gt, OpCode::GtEqR | OpCode::JumpUnlessGtEq => OpCode::GtEq,
        OpCode::MinusR => OpCode::Minus, OpCode::NotR => OpCode::Not, OpCode::GetItemR => OpCode::GetItem, OpCode::LenR => OpCode::CallLen,
        OpCode::InR => OpCode::In, OpCode::NotInR => OpCode::NotIn, OpCode::IsR => OpCode::Is, OpCode::IsNotR => OpCode::IsNot,
        other => other,
    }
}

/* Fuse LoadAttr + [single-push arg loads] + Call into CallMethod+CallMethodArgs. Arg loads shift left one slot so the pair sits adjacent at the Call. Only pure single-push opcodes relocate, and never across a jump target. */
pub(crate) fn fuse_method_calls(chunk: &SSAChunk) -> Cow<'_, [Instruction]> {
    let src = &chunk.instructions;
    if !src.iter().any(|i| i.opcode == OpCode::LoadAttr) { return Cow::Borrowed(src); }
    let n = src.len();
    let mut out = src.clone();
    let targeted = entered(src);
    const MAX_WINDOW: usize = 8;
    let mut i = 0;
    while i + 1 < n {
        if src[i].opcode != OpCode::LoadAttr { i += 1; continue; }
        // Scan the run of relocatable single-push arg loads after the LoadAttr.
        let mut j = i + 1;
        while j < n
            && j - i - 1 < MAX_WINDOW
            && !targeted[j]
            && matches!(src[j].opcode, OpCode::LoadConst | OpCode::LoadName | OpCode::LoadTrue | OpCode::LoadFalse | OpCode::LoadNone)
        {
            j += 1;
        }
        if j >= n || src[j].opcode != OpCode::Call || targeted[j] { i += 1; continue; }
        // Every arg must be exactly one allowed push, else stack layout breaks.
        let raw = src[j].operand as usize;
        if (raw & 0xFF) + 2 * ((raw >> 8) & 0xFF) != j - i - 1 { i += 1; continue; }
        out[i..(j - 1)].copy_from_slice(&src[(i + 1)..j]);
        out[j - 1] = Instruction { opcode: OpCode::CallMethod, operand: src[i].operand };
        out[j] = Instruction { opcode: OpCode::CallMethodArgs, operand: src[j].operand };
        i = j + 1;
    }
    Cow::Owned(out)
}
