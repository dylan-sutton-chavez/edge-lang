use super::OpCode;
use crate::syntax::chunk::{SSAChunk, Instruction, Value};
use crate::lang::Policy;
use crate::value::Val;
use alloc::{vec, vec::Vec};

/* constant folding by the kernels of `policy` and no-op phi removal after SSA */
pub fn constant_fold(chunk: &mut SSAChunk, policy: &Policy) {
    // an op of constants folds only while no scope of the program binds its name
    let (mut names, mut stored) = (Vec::new(), Vec::new());
    fold_names(chunk, &mut names);
    if !names.is_empty() { stores_of(chunk, &names, &mut stored); }
    fold_tree(chunk, policy, &stored);
}

/* the names of the ops of constants `chunk` and its bodies kept to fold */
fn fold_names(chunk: &SSAChunk, out: &mut Vec<&'static str>) {
    for f in &chunk.folds { out.push(f.3); }
    for (_, body, _, _) in &chunk.functions { fold_names(body, out); }
    for body in &chunk.classes { fold_names(body, out); }
}

/* those of `names` that `chunk` its bodies and its modules store or delete */
fn stores_of(chunk: &SSAChunk, names: &[&'static str], out: &mut Vec<&'static str>) {
    for ins in &chunk.instructions {
        if !matches!(ins.opcode, OpCode::StoreName | OpCode::StoreGlobal | OpCode::StoreAttr | OpCode::Del | OpCode::DelAttr) { continue; }
        let Some(n) = chunk.names.get(usize::from(ins.operand)).map(|n| crate::syntax::ssa::ssa_strip(n)) else { continue };
        if let Some(&name) = names.iter().find(|&&f| f == n) { out.push(name); }
    }
    for (_, body, _, _) in &chunk.functions { stores_of(body, names, out); }
    for body in &chunk.classes { stores_of(body, names, out); }
    for entry in &chunk.imports { if let crate::syntax::chunk::ImportKind::Code(sub) = &entry.kind { stores_of(sub, names, out); } }
}

/* `constant_fold` of `chunk` with the names the program stores which keep their runtime ops */
fn fold_tree(chunk: &mut SSAChunk, policy: &Policy, stored: &[&str]) {
    let n = chunk.instructions.len();
    if n == 0 {
        for (_, body, _, _) in chunk.functions.iter_mut() { fold_tree(body, policy, stored); }
        for class_body in chunk.classes.iter_mut() { fold_tree(class_body, policy, stored); }
        return;
    }

    let mut dead = vec![false; n];
    for (at, pops, v, name) in if chunk.folds.is_empty() { Vec::new() } else { core::mem::take(&mut chunk.folds) } {
        let (at, pops) = (at as usize, usize::from(pops));
        if stored.contains(&name) || at < pops || chunk.instructions.get(at).is_none_or(|i| i.opcode != OpCode::RuntimeOp) || skips_into(chunk, at - pops, at) || !write_const_load(chunk, at, v) { continue; }
        for d in &mut dead[at - pops..at] { *d = true; }
    }

    for ip in 0..n {
        if dead[ip] { continue; }
        let opcode = chunk.instructions[ip].opcode;

        match opcode {
            super::folds!() => try_fold_binop(chunk, &mut dead, ip, policy),
            OpCode::Not => try_fold_unary(chunk, &mut dead, ip, |v| fold_not(v, policy)),
            OpCode::Minus => try_fold_unary(chunk, &mut dead, ip, fold_neg),
            _ => {}
        }
    }

    // pass two drops no-op phis capturing their source pairs before marking them dead
    let mut surviving_pairs: Vec<(u16, u16)> = Vec::new();
    if !chunk.phi_map.is_empty() && !chunk.phi_sources.is_empty() {
        for (ip, ins) in chunk.instructions.iter().enumerate() {
            if dead[ip] || ins.opcode != OpCode::Phi { continue; }
            let phi_idx = chunk.phi_map[ip];
            let Some(&(a, b)) = chunk.phi_sources.get(phi_idx) else { continue };
            // both sources and the destination share one slot so the phi copies nothing
            if a == b && a == ins.operand {
                dead[ip] = true;
            } else {
                surviving_pairs.push((a, b));
            }
        }
    }

    if dead.iter().any(|&d| d) {
        compact_with_jump_remap(chunk, &dead);
        // surviving phis keep their order so they pair with the kept sources in sequence
        chunk.phi_sources = surviving_pairs;
        chunk.index_phis();
    }

    for (_, body, _, _) in chunk.functions.iter_mut() {
        fold_tree(body, policy, stored);
    }
    for class_body in chunk.classes.iter_mut() {
        fold_tree(class_body, policy, stored);
    }
}

/* compacts the live instructions and remaps jumps with dead ones forwarding to the next live */
fn compact_with_jump_remap(chunk: &mut SSAChunk, dead: &[bool]) {
    let n = chunk.instructions.len();
    let alive_count: usize = dead.iter().filter(|&&d| !d).count();

    let mut remap: Vec<usize> = Vec::with_capacity(n + 1);
    let mut new_pos = 0usize;
    for &is_dead in dead {
        remap.push(new_pos);
        if !is_dead { new_pos += 1; }
    }
    remap.push(alive_count);

    // walked backwards so each dead entry forwards to the next live one in one pass
    for i in (0..n).rev() {
        if dead[i] { remap[i] = remap[i + 1]; }
    }

    for (ip, _) in dead.iter().enumerate().take(n) {
        if dead[ip] { continue; }
        let ins = &mut chunk.instructions[ip];
        if !ins.opcode.is_jump() { continue; }
        let target = ins.operand as usize;
        let new_target = if target > n { target } else { remap[target] };
        if let Ok(v) = u16::try_from(new_target) { ins.operand = v; }
    }

    // the recorded positions follow the compaction too
    for (ip_at, _) in chunk.stmt_pos.iter_mut().chain(chunk.call_byte_pos.iter_mut()) {
        let old = *ip_at as usize;
        if old < remap.len() { *ip_at = remap[old] as u32; }
    }

    let mut idx = 0usize;
    chunk.instructions.retain(|_| {
        let keep = !dead[idx];
        idx += 1;
        keep
    });
}

/* the value runtime opcode `op` folds to over the constants its call compiled from `from` */
pub(crate) fn own_value(chunk: &SSAChunk, from: usize, op: u8, ops: &crate::lang::OpSet) -> Option<Val> {
    let own = ops.ops.get(op as usize).filter(|o| o.folds)?;
    // each operand compiled to one constant so nothing jumps between them
    if chunk.instructions.len() != from + usize::from(own.pops) { return None; }
    let mut args = Vec::with_capacity(usize::from(own.pops));
    for ins in &chunk.instructions[from..] {
        args.push(match ins.opcode {
            OpCode::LoadConst => const_to_val(&chunk.constants, ins.operand)?,
            OpCode::LoadTrue => Val::bool(true),
            OpCode::LoadFalse => Val::bool(false),
            OpCode::LoadNone => Val::none(),
            _ => return None,
        });
    }
    (ops.fold)(op, &args)
}

fn write_const_load(chunk: &mut SSAChunk, pos: usize, v: Val) -> bool {
    let new_ins = if v.is_bool() {
        Instruction {
            opcode: if v.as_bool() { OpCode::LoadTrue } else { OpCode::LoadFalse },
            operand: 0,
        }
    } else if v.is_none() {
        Instruction { opcode: OpCode::LoadNone, operand: 0 }
    } else if let Some(idx) = find_or_push_const(chunk, v) {
        Instruction { opcode: OpCode::LoadConst, operand: idx }
    } else {
        return false;
    };
    chunk.instructions[pos] = new_ins;
    true
}

fn find_or_push_const(chunk: &mut SSAChunk, v: Val) -> Option<u16> {
    let target: Value = if v.is_int() {
        Value::Int(v.as_int())
    } else if v.is_float() {
        Value::Float(v.as_float())
    } else {
        return None;
    };
    let pos = chunk.constants.iter().position(|c| match (c, &target) {
        // -0.0 and 0.0 compare equal but sharing a slot would lose the folded sign
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        _ => c == &target,
    });
    if let Some(pos) = pos {
        return u16::try_from(pos).ok();
    }
    let idx = chunk.constants.len();
    if idx >= u16::MAX as usize { return None; }
    chunk.constants.push(target);
    u16::try_from(idx).ok()
}

/* the caller guarantees `v` is an int or a float */
fn as_f64(v: Val) -> f64 { if v.is_int() { v.as_int() as f64 } else { v.as_float() } }

fn const_to_val(constants: &[Value], idx: u16) -> Option<Val> {
    match constants.get(idx as usize)? {
        Value::Int(i) if (Val::INT_MIN..=Val::INT_MAX).contains(i) => Some(Val::int(*i)),
        Value::Float(f) => Some(Val::float(*f)),
        Value::Bool(b) => Some(Val::bool(*b)),
        _ => None,
    }
}

/* the nearest instruction before `from` that no inner fold marked dead */
fn prev_live(dead: &[bool], from: usize) -> Option<usize> {
    let mut i = from;
    while i > 0 {
        i -= 1;
        if !dead[i] { return Some(i); }
    }
    None
}

/* whether the jump before `start` lands in the span where boolean operators and ternaries join */
fn skips_into(chunk: &SSAChunk, start: usize, end: usize) -> bool {
    let Some(j) = start.checked_sub(1).map(|i| chunk.instructions[i]) else { return false };
    j.opcode.is_jump() && (start + 1..=end).contains(&(j.operand as usize))
}

fn try_fold_binop(chunk: &mut SSAChunk, dead: &mut [bool], ip: usize, policy: &Policy) {
    let Some(prev1_ip) = prev_live(dead, ip) else { return };
    let Some(prev2_ip) = prev_live(dead, prev1_ip) else { return };

    let p2 = chunk.instructions[prev2_ip];
    let p1 = chunk.instructions[prev1_ip];
    if p2.opcode != OpCode::LoadConst || p1.opcode != OpCode::LoadConst { return; }

    let (Some(a), Some(b)) = (
        const_to_val(&chunk.constants, p2.operand),
        const_to_val(&chunk.constants, p1.operand),
    ) else { return };

    let opcode = chunk.instructions[ip].opcode;
    let Some(result) = fold_binop(opcode, a, b, policy) else { return };
    if skips_into(chunk, prev2_ip, ip) { return; }

    if !write_const_load(chunk, prev2_ip, result) { return; }
    dead[prev1_ip] = true;
    dead[ip] = true;
}

/* folds a unary op over the preceding LoadConst with `fold` as the per-op table */
fn try_fold_unary(chunk: &mut SSAChunk, dead: &mut [bool], ip: usize, fold: impl Fn(Val) -> Option<Val>) {
    let Some(prev1_ip) = prev_live(dead, ip) else { return };
    let p1 = chunk.instructions[prev1_ip];
    if p1.opcode != OpCode::LoadConst { return; }
    let Some(v) = const_to_val(&chunk.constants, p1.operand) else { return };

    if let Some(r) = fold(v)
        && !skips_into(chunk, prev1_ip, ip)
        && write_const_load(chunk, prev1_ip, r)
    {
        dead[ip] = true;
    }
}

/* `not v` of a constant whose tag alone decides its truth under `policy` */
fn fold_not(v: Val, policy: &Policy) -> Option<Val> { policy.tag_falsy(v).map(Val::bool) }

fn fold_neg(v: Val) -> Option<Val> {
    if v.is_int() {
        let r = -(v.as_int() as i128);
        if (Val::INT_MIN as i128..=Val::INT_MAX as i128).contains(&r) { Some(Val::int(r as i64)) } else { None }
    } else if v.is_float() {
        Some(Val::float(-v.as_float()))
    } else { None }
}

fn fold_binop(op: OpCode, a: Val, b: Val, policy: &Policy) -> Option<Val> {
    if matches!(op, OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::Gt | OpCode::LtEq | OpCode::GtEq) {
        if !((a.is_int() || a.is_float()) && (b.is_int() || b.is_float())) { return None; }
        // every policy compares numbers of two kinds as one so these fold as numbers
        debug_assert!(matches!(policy.numeric_eq, crate::lang::NumEq::Unified));
        let (af, bf) = (as_f64(a), as_f64(b));
        return Some(Val::bool(match op {
            OpCode::Eq => af == bf,
            OpCode::NotEq => af != bf,
            OpCode::Lt => af < bf,
            OpCode::Gt => af > bf,
            OpCode::LtEq => af <= bf,
            OpCode::GtEq => af >= bf,
            _ => return None,
        }));
    }

    if a.is_int() && b.is_int() {
        let (ai, bi) = (a.as_int() as i128, b.as_int() as i128);
        let r = match op {
            OpCode::Add => ai.checked_add(bi)?,
            OpCode::Sub => ai.checked_sub(bi)?,
            OpCode::Mul => ai.checked_mul(bi)?,
            // rounded by the policy's own kernel as the run rounds them
            OpCode::Mod => policy.int_divmod(ai, bi)?.1,
            OpCode::FloorDiv => policy.int_divmod(ai, bi)?.0,
            OpCode::BitAnd => ai & bi,
            OpCode::BitOr => ai | bi,
            OpCode::BitXor => ai ^ bi,
            OpCode::Shl => if !(0..63).contains(&bi) { return None; } else { ai.checked_shl(bi as u32)? },
            OpCode::Shr => if !(0..63).contains(&bi) { return None; } else { ai >> bi },
            _ => return None,
        };
        if (Val::INT_MIN as i128..=Val::INT_MAX as i128).contains(&r) {
            return Some(Val::int(r as i64));
        }
        return None;
    }

    if (a.is_int() || a.is_float()) && (b.is_int() || b.is_float()) {
        let (af, bf) = (as_f64(a), as_f64(b));
        return Some(Val::float(match op {
            OpCode::Add => af + bf,
            OpCode::Sub => af - bf,
            OpCode::Mul => af * bf,
            OpCode::Div => if bf == 0.0 { return None; } else { af / bf },
            _ => return None,
        }));
    }

    None
}
