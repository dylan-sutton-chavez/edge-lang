use super::*;

/* appends to locals a loop only appends to and the heads with slots they reset */
pub(super) fn append_loops(src: &[Instruction], facts: &Facts) -> (Vec<bool>, Vec<(usize, u16)>) {
    // code that never adds into a local it stores at once has nothing to grow
    if !src.windows(2).any(|w| w[0].opcode == OpCode::InPlaceAdd && w[1].opcode == OpCode::StoreName && facts.kinds.get(w[1].operand as usize) == Some(&Kind::Local)) { return (Vec::new(), Vec::new()); }
    let mut appends = vec![false; src.len()];
    let mut resets = Vec::new();
    let refers = |ins: &Instruction| matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi);
    for (end, back) in src.iter().enumerate().filter(|(j, b)| b.opcode == OpCode::Jump && (b.operand as usize) <= *j) {
        let head = back.operand as usize;
        // a jump from outside into the loop would skip the reset at its head
        let outside = |k: usize| k < head || k > end;
        if src.iter().enumerate().any(|(k, ins)| outside(k) && ins.opcode.is_jump() && (head..=end).contains(&(ins.operand as usize))) { continue; }
        // the locals appends in the loop store with each looked at once
        let mut slots: Vec<u16> = (head..end).filter(|&k| src[k].opcode == OpCode::InPlaceAdd && src[k + 1].opcode == OpCode::StoreName)
            .map(|k| src[k + 1].operand).filter(|&s| facts.kinds.get(s as usize) == Some(&Kind::Local)).collect();
        slots.sort_unstable();
        slots.dedup();
        for slot in slots {
            // every use of the slot in the loop is an append's load or store
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

/* locals stored once from new lists and never deleted or merged hold them wherever read */
pub(super) fn list_locals(src: &[Instruction], flags: &[u32], facts: &Facts, names: usize) -> Vec<bool> {
    let mut stores = vec![0u8; names];
    let mut list = vec![false; names];
    for (k, ins) in src.iter().enumerate() {
        let s = ins.operand as usize;
        if s >= names { continue; }
        match ins.opcode {
            OpCode::StoreName => {
                stores[s] = stores[s].saturating_add(1);
                list[s] = k > 0 && src[k - 1].opcode == OpCode::BuildList && !flags.get(k).is_some_and(|f| f & TARGET != 0);
            }
            OpCode::Del | OpCode::Phi => stores[s] = 2,
            _ => {}
        }
    }
    (0..names).map(|s| stores[s] == 1 && list[s] && facts.kinds.get(s) == Some(&Kind::Local)).collect()
}

#[inline]
pub(super) fn transfer(bound: &mut [u64], ins: Instruction, k: usize, chunk: &SSAChunk) {
    if !matches!(ins.opcode, OpCode::StoreName | OpCode::Phi | OpCode::Del) { return; }
    let s = ins.operand as usize;
    let has = |b: &[u64], r: u16| b.get(r as usize / 64).is_some_and(|w| w & 1 << (r % 64) != 0);
    // a Phi binds its slot only when one of its sources is bound
    if ins.opcode == OpCode::Phi && !chunk.phi_map.get(k).and_then(|&p| chunk.phi_sources.get(p)).is_some_and(|&(a, b)| has(bound, a) || has(bound, b)) { return; }
    let Some(w) = bound.get_mut(s / 64) else { return };
    if ins.opcode == OpCode::Del { *w &= !(1 << (s % 64)); } else { *w |= 1 << (s % 64); }
}

/* the slots bound on every path into each block as a bit row per block */
pub(super) struct Bound {
    words: usize,
    // per block the slot words then a word set once any path reaches the block
    bits: Vec<u64>,
}

impl Bound {

    /* the slots bound entering the block at `i` and none when no path reaches it */
    #[inline]
    pub(super) fn entry(&self, flags: &[u32], i: usize, out: &mut [u64]) { entry(&self.bits, self.words, flags, i, out) }
}

/* the slots bound entering each block of `src` noting each start's row in its `flags` */
#[inline(never)]
pub(super) fn bound_on_entry(src: &[Instruction], chunk: &SSAChunk, names: usize, facts: &Facts, flags: &mut [u32]) -> Bound {
    let len = src.len();
    let words = names.div_ceil(64);
    let mut blocks = 0;
    for f in flags.iter_mut() { if *f & START != 0 { *f |= blocks << ROW; blocks += 1; } }
    // the rows then the slots of the block being walked
    let at = blocks as usize * (words + 1);
    let mut bits = vec![0; at + words];
    let (rows, s) = bits.split_at_mut(at);
    facts.bind(s);
    merge(rows, words, flags, 0, s);
    // passes repeat until no back-edge narrows a loop head and skip blocks not yet reached
    loop {
        let (mut again, mut live) = (false, false);
        for i in 0..len {
            if flags[i] & START != 0 {
                let at = (flags[i] >> ROW) as usize * (words + 1);
                live = rows[at + words] != 0;
                if live { entry(rows, words, flags, i, s); }
            }
            if !live { continue; }
            let ins = src[i];
            transfer(s, ins, i, chunk);
            if ins.opcode.is_jump() && (ins.operand as usize) <= len && merge(rows, words, flags, ins.operand as usize, s) && ins.operand as usize <= i { again = true; }
            if ends_flow(ins.opcode) { live = false; continue; }
            if flags[i + 1] & START != 0 { merge(rows, words, flags, i + 1, s); }
        }
        if !again { break; }
    }
    Bound { words, bits }
}

/* a register-lowered ternary as `tern` holds it with its arms and join and pending values */
pub(super) struct Tern { pub(super) start_a: usize, pub(super) else_at: usize, pub(super) end_a: usize, pub(super) join: usize, pub(super) saved: Vec<Entry>, pub(super) t: Option<u16>, pub(super) bail: bool }

/* the value a ternary arm left atop `saved` or None if it left anything else */
pub(super) fn arm_value(pending: &[Entry], saved: &[Entry]) -> Option<Entry> {
    let n = saved.len();
    (pending.len() == n + 1 && pending[..n] == saved[..]).then(|| pending[n])
}

impl<'p> Lower<'_, 'p> {

    /* the first arm's jump and join of a ternary at `i` with simple one-value arms */
    pub(super) fn ternary(&self, i: usize) -> Option<(usize, usize)> {
        let else_at = self.src[i].operand as usize;
        let end_a = else_at.checked_sub(1)?;
        let jump = self.src.get(end_a)?;
        if end_a <= i || jump.opcode != OpCode::Jump || (jump.operand as usize) <= else_at { return None; }
        let join = jump.operand as usize;
        // each arm pushes one value of its own and reads nothing under it
        let arm = |from: usize, to: usize| -> Option<usize> {
            let (mut depth, mut most) = (0i32, 0i32);
            for k in from..to {
                if k != else_at && self.target(k) { return None; }
                let ins = self.src[k];
                depth += match ins.opcode {
                    OpCode::LoadName if (ins.operand as usize) < self.names && matches!(self.kind(ins.operand), Some(Kind::Local | Kind::Global(_))) => 1,
                    OpCode::LoadConst if (ins.operand as usize) < self.consts => 1,
                    OpCode::LoadNone | OpCode::LoadTrue | OpCode::LoadFalse => 1,
                    OpCode::LoadAttr => if depth < 1 { return None } else { 0 },
                    OpCode::Minus | OpCode::Not => if depth < 1 { return None } else { 0 },
                    code if binary_form(code).is_some() && ins.operand < AUGMENTED as u16 => if depth < 2 { return None } else { -1 },
                    _ => return None,
                };
                most = most.max(depth);
            }
            (depth == 1).then_some(most as usize)
        };
        let (a, b) = (arm(i + 1, end_a)?, arm(else_at, join)?);
        // only the first arm's jump and the second's fall through enter the join
        let others = self.src.iter().enumerate().any(|(k, n)| k != end_a && k != i && matches!(n.opcode, OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop | OpCode::ForIter | OpCode::SetupExcept | OpCode::SetupFinally) && matches!(n.operand as usize, t if t == else_at || t == join));
        (!others && self.pending.len() + a.max(b) + 1 < MAX_PENDING).then_some((end_a, join))
    }

    /* joins ternary arms putting the second's value in the first's register pending atop `saved` */
    pub(super) fn join_tern(&mut self) {
        let Some(mut tern) = self.tern.take() else { return };
        let (Some(v), Some(t)) = (arm_value(&self.pending, &tern.saved), tern.t) else { tern.bail = true; self.tern = Some(tern); return };
        if v.name || v.r != t { self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: v.r, c: 0 }); }
        self.pending.clone_from(&tern.saved);
        self.pending.push(Entry::reg(t));
        self.last = None;
    }
}
