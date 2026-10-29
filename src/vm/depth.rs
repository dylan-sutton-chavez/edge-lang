use crate::code::{Flow, OpCode, Operator, Spill};
use crate::syntax::chunk::{Instruction, SSAChunk};
#[cfg(debug_assertions)]
use super::VM;
use alloc::{vec, vec::Vec};
use OpCode::*;

// ops that never touch the operand stack
pub(crate) const fn unstacked(op: OpCode) -> bool { op.unstacked() }

// the values `op` pops and pushes falling through and none when only the run knows
pub(crate) fn effect(op: OpCode, operand: u16, chunk: &SSAChunk) -> Option<(u16, u16)> { op.effect(operand, chunk) }

/* the operands spread call `op` takes counting each spread one */
fn spread_operands(op: OpCode, operand: u16, chunk: &SSAChunk) -> Option<usize> {
    Some(match op {
        CallSpread => usize::from(operand & 0xFF) + 2 * usize::from(operand >> 8),
        CallExtern => usize::from(operand & 0xF) + 2 * usize::from(operand >> 4 & 0xF),
        CallNative => { let s = crate::syntax::chunk::native_site(operand, &chunk.native_sites)?; usize::from(s.pos) + 2 * usize::from(s.kw) }
        _ => return None,
    })
}

/* `spread_packs` walking back over straight code from the call and none past a jump */
pub(crate) fn spread_back(at: impl Fn(usize) -> (OpCode, u16), i: usize, chunk: &SSAChunk) -> Option<(u8, u32)> {
    let (op, operand) = at(i);
    let m = spread_operands(op, operand, chunk)?;
    if m > 32 { return None; }
    // the operands still to find and the values ops past here take before the call
    let (mut left, mut taken, mut packs, mut maps, mut found) = (m, 0usize, 0u32, 0u8, 0u8);
    let mut k = i;
    while left > 0 {
        k = k.checked_sub(1)?;
        let (op, operand) = at(k);
        if op.is_jump() || matches!(op, ReturnValue | Raise | UnwindFinally) { return None; }
        let (pops, pushes) = effect(op, operand, chunk)?;
        let (pops, pushes) = (usize::from(pops), usize::from(pushes));
        let gives = pushes.saturating_sub(taken);
        taken = taken.saturating_sub(pushes);
        if gives > left { return None; }
        if gives > 0 && op == UnpackArgs {
            packs |= 1 << (left - 1);
            if found >= 8 { return None; }
            maps = maps << 1 | u8::from(operand & 3 == 2);
            found += 1;
        }
        left -= gives;
        taken += pops;
    }
    Some((maps, packs))
}

/* the packed operands of spread call `i` as a mask with its maps in order */
pub(crate) fn spread_packs(src: &[Instruction], depths: &[u16], i: usize, chunk: &SSAChunk) -> Option<(u8, u32)> {
    let (op, operand) = (src.get(i)?.opcode, src[i].operand);
    let m = spread_operands(op, operand, chunk)?;
    let d = usize::from(*depths.get(i)?);
    if m > 32 || d < m || d == usize::from(u16::MAX) { return None; }
    let base = d - m;
    // the lowest depth past each spread so a pop below it ends the walk
    let (mut low, mut packs, mut maps) = (d, 0u32, alloc::vec::Vec::new());
    for k in (0..i).rev() {
        let dk = usize::from(depths[k]);
        if dk == usize::from(u16::MAX) { continue; }
        if dk <= base { break; }
        let ins = src[k];
        if ins.opcode == UnpackArgs && dk > base && dk - 1 < low {
            packs |= 1 << (dk - 1 - base);
            maps.push(ins.operand & 3 == 2);
        }
        let operand = if ins.opcode == CallMethod { src.get(k + 1).filter(|n| n.opcode == CallMethodArgs).map_or(ins.operand, |n| n.operand) } else { ins.operand };
        let pops = effect(ins.opcode, operand, chunk).map_or(0, |e| usize::from(e.0));
        low = low.min(dk.saturating_sub(pops));
    }
    // the maps in the order the packed operands lie
    if maps.len() > 8 { return None; }
    let mut x = 0u8;
    for (q, &map) in maps.iter().rev().enumerate() { if map { x |= 1 << q; } }
    Some((x, packs))
}

// the rise of an op whose operand counts what it adds
pub(crate) const WIDE: u8 = 254;

// the most `op` adds to the stack for any operand counting a handler's exception
pub(crate) const fn rise(op: OpCode) -> u8 { match op.info().rise() { Some(r) => r, None => WIDE } }

// each instruction's starting depth or the first one where the dataflow fails
pub(crate) fn depths(src: &[Instruction], chunk: &SSAChunk) -> Result<Vec<u16>, usize> {
    let n = src.len();
    let mut at = vec![u16::MAX; n + 1];
    let mut work = Vec::new();
    // two paths into one instruction must leave the same depth
    let enter = |at: &mut Vec<u16>, work: &mut Vec<usize>, t: usize, d: u16| -> bool {
        match at.get_mut(t) {
            Some(slot) if *slot == u16::MAX => { *slot = d; work.push(t); true }
            Some(slot) => *slot == d,
            None => false,
        }
    };
    enter(&mut at, &mut work, 0, 0);
    while let Some(i) = work.pop() {
        if i >= n { continue; }
        let (d, ins) = (at[i], src[i]);
        let operand = if ins.opcode == CallMethod { src.get(i + 1).filter(|n| n.opcode == CallMethodArgs).ok_or(i)?.operand } else { ins.operand };
        // a return with nothing on the stack hands back none
        let (pops, pushes) = if ins.opcode == ReturnValue && d == 0 { (0, 0) } else { effect(ins.opcode, operand, chunk).ok_or(i)? };
        let after = d.checked_sub(pops).and_then(|d| d.checked_add(pushes)).ok_or(i)?;
        // the way to a target pops or adds what its edge says
        let ok = ins.opcode.edge().taken().is_none_or(|(p, q)| d.checked_sub(u16::from(p)).is_some_and(|e| enter(&mut at, &mut work, ins.operand as usize, e + u16::from(q))));
        if !ok || !ins.opcode.ends_flow() && !enter(&mut at, &mut work, i + 1, after) { return Err(i); }
    }
    Ok(at)
}

// what lowering spills into rows under an op's operands or at every value
const UNDER: u8 = 1;
const EVERY: u8 = 2;

// what lowering spills at `op` so a run parked there resumes in any code
pub(crate) const fn spills(op: OpCode) -> u8 { match op.info().spill { Spill::None => 0, Spill::Under => UNDER, Spill::Every => EVERY } }

// each opcode's spill read with one load
static SPILLS: [u8; 256] = {
    let mut t = [0; 256];
    let mut k = 0;
    while k < OpCode::ALL.len() { t[k] = spills(OpCode::ALL[k]); k += 1; }
    t
};

// past this many values lowering spills them all as it keeps few temporaries
pub(crate) const PENDING: usize = 16;

// the depth no instruction is reached with
pub(crate) const NONE: usize = usize::MAX;

// a binary op whose step only a mark past its operand turns into a spill
pub(crate) const BINARY: u8 = 0xFE;

// an op that jumps or spills or ends the flow or varies its effect
pub(crate) const CLOSER: u8 = 0xFF;

// an op's net effect past `BIAS` for the lowering's pass or a closer look
const fn steps(op: OpCode) -> u8 {
    let info = op.info();
    let closer = op.is_jump() || !matches!(info.spill, Spill::None) || matches!(info.flow, Flow::Leave | Flow::Unwind);
    match info.fixed() {
        _ if binary(op) => BINARY,
        Some((p, q)) if !closer => BIAS + q - p,
        _ => CLOSER,
    }
}

// each opcode's step for the lowering's pass read with one load
pub(crate) static STEPS: [u8; 256] = {
    let mut t = [CLOSER; 256];
    let mut k = 0;
    while k < OpCode::ALL.len() { t[k] = steps(OpCode::ALL[k]); k += 1; }
    t
};

/* the depth past `src[i]` and the rows `need` grows to or none for the worklist */
#[inline(never)]
pub(crate) fn closer(src: &[Instruction], i: usize, chunk: &SSAChunk, stores: &impl Fn(u16) -> bool, at: &mut [u32], d: usize, mut need: usize) -> Option<(usize, usize)> {
    let (ins, n) = (src[i], src.len());
    // an underflow wraps past this bound to the worklist
    if d > 4 * n + 4 { return None; }
    let op = ins.opcode;
    let e = EFFECT[op as usize];
    // a return off an empty stack takes nothing and a varying op asks its operand
    let (pops, pushes) = if op == ReturnValue && d == 0 { (0, 0) } else if e != VARIES { (u16::from(e >> 4), u16::from(e & 15)) } else {
        let operand = if op == CallMethod { src.get(i + 1).filter(|n| n.opcode == CallMethodArgs)?.operand } else { ins.operand };
        effect(op, operand, chunk)?
    };
    let after = d.checked_sub(usize::from(pops))? + usize::from(pushes);
    // marked binary ops run as their stack form and row ops over their rows
    match if rowed(src, i, chunk, d) { EVERY } else if marked(op) && ins.operand >= 0x80 { UNDER } else { yielding(src, i).unwrap_or(SPILLS[op as usize]) } {
        UNDER => need = need.max((d - usize::from(pops)).saturating_sub(usize::from(merges(op)))),
        EVERY => need = need.max(d),
        _ => {}
    }
    if op.is_jump() {
        let t = ins.operand as usize;
        // a handler starts one past its try's depth for the exception its catch takes
        let e = match op { SetupExcept => d + 1, JumpIfFalse => d - 1, _ => d };
        if t > n { return None; }
        // a jump returning its one value at once keeps no row for it
        if !(op == Jump && e == 1 && src.get(t).is_some_and(|r| r.opcode == ReturnValue)) { need = need.max(if op == SetupExcept { d } else { e }); }
        // a loop's head the pass skipped is one only the worklist settles
        let noted = at[t] as usize;
        if t <= i && noted != e + 1 || t > i && noted != 0 && noted != e + 1 { return None; }
        if t > i { at[t] = e as u32 + 1; }
    }
    if matches!(op, Jump | ReturnValue | Raise) { return Some((NONE, need)); }
    // a block falls in past a jump unless the loop's step stores its item
    if (op.is_jump() || op == UnwindFinally) && !(op == ForIter && stepped(src, i, stores)) { need = need.max(after); }
    Some((after, need))
}

/* whether the loop step at `i` stores or unpacks or yields its item at once */
pub(crate) fn stepped(src: &[Instruction], i: usize, stores: &impl Fn(u16) -> bool) -> bool {
    src.get(i + 1).is_some_and(|s| matches!(s.opcode, UnpackSequence | UnpackEx) || s.opcode == StoreName && stores(s.operand))
        || src.get(i + 1).is_some_and(|s| s.opcode == Yield) && src.get(i + 2).is_some_and(|s| s.opcode == PopTop)
}

/* the rows the worklist finds for code the forward pass cannot settle */
#[cold]
#[inline(never)]
pub(crate) fn kept_rows(src: &[Instruction], chunk: &SSAChunk, stores: &impl Fn(u16) -> bool, at: &mut [u32]) -> Option<u16> {
    let found = depths(src, chunk).ok()?;
    for (a, &d) in at.iter_mut().zip(&found) { *a = if d == u16::MAX { 0 } else { u32::from(d) + 1 }; }
    let mut need = 0u16;
    for (i, ins) in src.iter().enumerate() {
        let d = found[i];
        if d == u16::MAX { continue; }
        let operand = if ins.opcode == CallMethod { src.get(i + 1).filter(|n| n.opcode == CallMethodArgs)?.operand } else { ins.operand };
        let k = if rowed(src, i, chunk, usize::from(d)) { EVERY } else if marked(ins.opcode) && operand >= 0x80 { UNDER } else { yielding(src, i).unwrap_or(SPILLS[ins.opcode as usize]) };
        let pops = if k == UNDER { effect(ins.opcode, operand, chunk).map_or(d, |e| e.0 + u16::from(merges(ins.opcode))) } else { 0 };
        if k != 0 { need = need.max(d.saturating_sub(pops)); }
        let returns = ins.opcode == Jump && src.get(ins.operand as usize).is_some_and(|r| r.opcode == ReturnValue);
        if ins.opcode.is_jump() && let Some(&e) = found.get(ins.operand as usize) && e != u16::MAX && !(returns && e == 1) {
            need = need.max(if ins.opcode == SetupExcept { e.saturating_sub(1) } else { e });
        }
        let fused = ins.opcode == ForIter && stepped(src, i, stores);
        if (ins.opcode.is_jump() || ins.opcode == UnwindFinally) && !fused && let Some(&e) = found.get(i + 1) && e != u16::MAX { need = need.max(e); }
        if matches!(ins.opcode, Jump | ReturnValue | Raise) && let Some(&e) = found.get(i + 1) && e != u16::MAX { need = need.max(e); }
    }
    Some(need)
}

/* whether the lowering runs op `i` over its row so its operands take rows */
#[inline(always)]
pub(crate) fn rowed(src: &[Instruction], i: usize, chunk: &SSAChunk, d: usize) -> bool {
    let a = src[i].operand;
    match src[i].opcode {
        // a call past five positionals or with keywords and a method call past four
        Call => a > 5,
        CallMethod => src.get(i + 1).is_some_and(|n| n.opcode == CallMethodArgs && n.operand > 4),
        CallNative => crate::syntax::chunk::native_site(a, &chunk.native_sites).is_none_or(|s| s.spread || s.kw != 0 || s.pos > 5),
        CallSpread | UnpackArgs => !spread_regs(src, i - (src[i].opcode == CallSpread) as usize, d),
        CallExtern | ListExtend | SetUpdate | DictUpdate => true,
        _ => false,
    }
}

/* whether the spread unpacked at `end` over `d` values lowers to its register call */
#[cold]
#[inline(never)]
fn spread_regs(src: &[Instruction], end: usize, d: usize) -> bool {
    let (Some(unpack), Some(call)) = (src.get(end), src.get(end + 1)) else { return false };
    if unpack.opcode != UnpackArgs || !matches!(unpack.operand, 1 | 2) || call.opcode != CallSpread || call.operand != 1 || end + 2 >= src.len() { return false; }
    let simple = |k: usize| matches!(src[k].opcode, LoadName | LoadConst | LoadGlobal | LoadNone | LoadTrue | LoadFalse | BuildList | BuildTuple | BuildDict);
    let mut b = end;
    while b > 0 && simple(b - 1) { b -= 1; }
    // the loads after the BeginArgs leave one value over the callee
    let Some(b) = b.checked_sub(1).filter(|&b| src[b].opcode == BeginArgs && b + 1 < end) else { return false };
    let mut depth = 0usize;
    for s in &src[b + 1..end] {
        depth = match s.opcode {
            BuildList | BuildTuple => match (depth + 1).checked_sub(s.operand as usize) { Some(v) => v, None => return false },
            BuildDict => match (depth + 1).checked_sub(2 * s.operand as usize) { Some(v) => v, None => return false },
            _ => depth + 1,
        };
    }
    let held = d.saturating_sub(1);
    // no jump lands inside the spread which the lowering takes as one step
    depth == 1 && (1..=1 + super::lower::MAX_UNDER).contains(&held) && held + end - b <= super::lower::MAX_PENDING
        && !src.iter().any(|s| s.opcode.is_jump() && (b + 1..=end + 1).contains(&(s.operand as usize)))
}

/* a yield's spill whose sent value waits in its row unless dropped */
#[inline]
fn yielding(src: &[Instruction], i: usize) -> Option<u8> {
    (src[i].opcode == Yield).then(|| if src.get(i + 1).is_some_and(|n| n.opcode == PopTop) { UNDER } else { EVERY })
}

// a merge whose island takes the container under its value as an operand too
const fn merges(op: OpCode) -> bool { matches!(op, ListExtend | SetUpdate | DictUpdate) }

// a binary op whose mark alone steps it read with one load
fn marked(op: OpCode) -> bool { STEPS[op as usize] == BINARY }

// an op the lowering writes as a register form over two values
const fn binary(op: OpCode) -> bool {
    let info = op.info();
    matches!(info.operator, Operator::Binary | Operator::Augmented) && matches!(info.spill, Spill::None)
}

// an op whose stack effect its operand or its chunk sets
const VARIES: u8 = 0xFF;

// marks an op that jumps or ends the flow or varies its effect
const BRANCH: u8 = 0x80;

// the offset a straight op's step holds past its pushes less its pops
pub(crate) const BIAS: u8 = 8;

// an op's pops and pushes as two nibbles where no operand varies them
const fn nibbles(op: OpCode) -> u8 { match op.info().fixed() { Some((p, q)) => p << 4 | q, None => VARIES } }

// an op's packed step its net effect past `BIAS` or nibbles past `BRANCH`
const fn fixed(op: OpCode) -> u8 {
    let info = op.info();
    match info.fixed() {
        None => VARIES,
        Some((p, q)) if op.is_jump() || matches!(info.flow, Flow::Leave) => BRANCH | p << 4 | q,
        Some((p, q)) => BIAS + q - p,
    }
}

// each opcode's nibbles read with one load
static EFFECT: [u8; 256] = {
    let mut t = [VARIES; 256];
    let mut k = 0;
    while k < OpCode::ALL.len() { t[k] = nibbles(OpCode::ALL[k]); k += 1; }
    t
};

// each opcode's packed effect read with one load
static FIXED: [u8; 256] = {
    let mut t = [VARIES; 256];
    let mut k = 0;
    while k < OpCode::ALL.len() { t[k] = fixed(OpCode::ALL[k]); k += 1; }
    t
};

// the deepest stack of `src` and none when only the run knows
pub(crate) fn deepest(src: &[Instruction], chunk: &SSAChunk) -> Option<u16> {
    let d = forward(src, chunk);
    debug_assert_eq!(d, settled(src, chunk), "a forward pass and the worklist differ on {:?}", chunk.qualname);
    d
}

/* a bound on the stack of `src` from one walk or none */
pub(crate) fn upper(src: &[Instruction], chunk: &SSAChunk) -> Option<u16> {
    let n = src.len();
    // the jumps ahead not reached yet with the depth each brings
    let mut ahead = [(0u16, 0u16); 8];
    let (mut held, mut next, mut d, mut deep, mut i) = (0usize, n, 0usize, 0usize, 0usize);
    while i < n {
        // straight ops up to the next target or closer step with no other check
        let mut k = BRANCH;
        for ins in &src[i..next] {
            k = FIXED[ins.opcode as usize];
            if k >= BRANCH { break; }
            // an underflow wraps past any depth the end refuses
            d = d.wrapping_add(usize::from(k)).wrapping_sub(usize::from(BIAS));
            if d > deep { deep = d; }
            i += 1;
        }
        if i == next {
            if i == n { break; }
            next = n;
            let mut q = 0;
            // a target takes the deepest stack any way in brings
            while q < held {
                let (t, e) = ahead[q];
                if usize::from(t) == i { d = d.max(usize::from(e)); held -= 1; ahead[q] = ahead[held]; } else { next = next.min(usize::from(t)); q += 1; }
            }
            if d > deep { deep = d; }
            continue;
        }
        let ins = src[i];
        let op = ins.opcode;
        let (pops, pushes) = if k == VARIES { varies(src, i, chunk)? } else { (u16::from(k >> 4 & 7), u16::from(k & 15)) };
        let after = d.saturating_sub(usize::from(pops)) + usize::from(pushes);
        if op.is_jump() {
            let t = ins.operand as usize;
            let e = match op { SetupExcept => d + 1, JumpIfFalse => d.saturating_sub(1), _ => d };
            if e > deep { deep = e; }
            // a loop's head the walk reached holding nothing takes nothing back
            if t <= i && e != 0 { return None; }
            if t > i && t < n {
                if held == ahead.len() { return None; }
                ahead[held] = (t as u16, e as u16);
                held += 1;
                next = next.min(t);
            }
        }
        // what follows a jump away holds nothing until a way in brings its depth
        d = if matches!(op, Jump | ReturnValue | Raise) { 0 } else { after };
        if d > deep { deep = d; }
        i += 1;
    }
    (deep <= 4 * n + 4).then_some(deep as u16)
}

/* the effect of the op at `i` its operand sets read in place when common */
#[inline(always)]
fn varies(src: &[Instruction], i: usize, chunk: &SSAChunk) -> Option<(u16, u16)> {
    let ins = src[i];
    let a = ins.operand;
    Some(match ins.opcode {
        // a native site written in place keeps its counts in its operand
        CallNative if a & crate::syntax::chunk::SITE_TABLE == 0 => ((a >> 9 & 7) + 2 * (a >> 8 & 1), u16::from(a & crate::syntax::chunk::SITE_VOID == 0)),
        Call | CallSpread => (1 + (a & 0xFF) + 2 * (a >> 8), 1),
        BuildList | BuildTuple | BuildSet | BuildString | BuildSlice => (a, 1),
        BuildDict => (2 * a, 1),
        CallMethod => { let n = src.get(i + 1).filter(|n| n.opcode == CallMethodArgs)?.operand; (1 + (n & 0xFF) + 2 * (n >> 8), 1) }
        op => effect(op, a, chunk)?,
    })
}

// `deepest` in one ordered pass with the worklist only for loops into skipped code
fn forward(src: &[Instruction], chunk: &SSAChunk) -> Option<u16> {
    let n = src.len();
    // the targets jumps ahead hand a depth to with the nearest last
    let mut ahead: Vec<(usize, usize)> = Vec::new();
    // the stretches the pass skipped as nothing jumps or falls into them
    let mut dead: Vec<(usize, usize)> = Vec::new();
    let (mut d, mut deep, mut next, mut i) = (0usize, 0usize, usize::MAX, 0usize);
    // the depth no instruction is reached with
    const NONE: usize = usize::MAX;
    while i < n {
        if i == next {
            while let Some(&(t, e)) = ahead.last() && t == i {
                ahead.pop();
                if d == NONE { d = e } else if d != e { return settled(src, chunk) }
            }
            next = ahead.last().map_or(NONE, |&(t, _)| t);
            deep = deep.max(d);
        }
        let ins = src[i];
        let k = FIXED[ins.opcode as usize];
        // an underflow wraps past any depth the end turns back to the worklist
        if k < BRANCH {
            d = d.wrapping_add(usize::from(k)).wrapping_sub(usize::from(BIAS));
            if d > deep { deep = d; }
            i += 1;
            continue;
        }
        let op = ins.opcode;
        let (pops, pushes) = match k {
            // a return with nothing on the stack hands back none
            _ if op == ReturnValue && d == 0 => (0, 0),
            VARIES => {
                let operand = if op == CallMethod { src.get(i + 1).filter(|n| n.opcode == CallMethodArgs)?.operand } else { ins.operand };
                effect(op, operand, chunk)?
            }
            k => (u16::from(k >> 4 & 7), u16::from(k & 15)),
        };
        let after = d.wrapping_sub(usize::from(pops)).wrapping_add(usize::from(pushes));
        if op.is_jump() {
            let t = ins.operand as usize;
            // a handler starts at its try's depth and an except one higher for the exception
            let e = match op { SetupExcept => d + 1, JumpIfFalse => d.wrapping_sub(1), _ => d };
            if t > n { return settled(src, chunk); }
            if t <= i {
                // a loop's head the pass skipped is one only the worklist settles
                if dead.iter().any(|&(from, to)| (from..to).contains(&t)) { return settled(src, chunk); }
            } else {
                let at = ahead.partition_point(|&(u, _)| u > t);
                match ahead.get(at) {
                    Some(&(u, f)) if u == t => if f != e { return settled(src, chunk) },
                    _ => ahead.insert(at, (t, e)),
                }
                next = ahead.last().map_or(NONE, |&(t, _)| t);
            }
            deep = deep.max(e);
        }
        if matches!(op, Jump | ReturnValue | Raise) {
            // the code up to the next target ahead has no way in
            let to = next.min(n);
            if to > i + 1 { dead.push((i + 1, to)); }
            (d, i) = (NONE, to);
            continue;
        }
        (d, i) = (after, i + 1);
        deep = deep.max(d);
    }
    // the depth past the end that falling off or a jump there leaves
    let end = ahead.iter().filter(|&&(t, _)| t == n).fold(if d == NONE { 0 } else { d }, |m, &(_, e)| m.max(e));
    let deep = deep.max(end);
    // no op adds more than four so a deeper stack wrapped from an underflow
    if deep > 4 * n + 4 { return settled(src, chunk); }
    Some(deep as u16)
}

// `deepest` by the worklist of `depths` for code a forward pass cannot settle
#[cold]
#[inline(never)]
fn settled(src: &[Instruction], chunk: &SSAChunk) -> Option<u16> {
    match depths(src, chunk) {
        Ok(at) => Some(at.into_iter().filter(|&d| d != u16::MAX).max().unwrap_or(0)),
        Err(k) => {
            let i = src[k];
            debug_assert!(effect(i.opcode, i.operand, chunk).is_none(), "the stack of {:?} does not settle at {k} {:?} {}", chunk.qualname, i.opcode, i.operand);
            None
        }
    }
}

#[cfg(debug_assertions)]
impl VM<'_> {
    // the deepest stack of `chunk` kept on `pool` and none when only the run knows
    pub(crate) fn max_depth(&mut self, chunk: &SSAChunk, pool: usize) -> Option<u16> {
        if let Some(d) = self.pools[pool].depth { return d; }
        let d = deepest(&chunk.instructions, chunk);
        self.pools[pool].depth = Some(d);
        d
    }
}
