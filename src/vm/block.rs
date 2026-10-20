use super::lower::Ins;
use crate::parser::OpCode::*;

/* What a section's instruction does with `a` to `c` (0 to 2), whether `a` is written, which fields are read registers, and whether `a` is a jump target. */
pub(crate) fn role(ins: &Ins) -> Option<(bool, &'static [u8], bool)> {
    const READS: [&[u8]; 8] = [&[], &[0], &[1], &[0, 1], &[2], &[0, 2], &[1, 2], &[0, 1, 2]];
    let k = kinds(ins);
    (k & NONE == 0).then(|| (k & DEST != 0, READS[(k >> 1 & 7) as usize], k & JUMPS != 0))
}

/* `role` as bits, `DEST`, the fields read from bit 1 on, `JUMPS`, or `NONE`. */
pub(crate) const DEST: u8 = 1;
pub(crate) const JUMPS: u8 = 16;
pub(crate) const NONE: u8 = 32;

#[inline(always)]
pub(crate) fn kinds(ins: &Ins) -> u8 {
    const A: u8 = 2;
    const B: u8 = 4;
    const C: u8 = 8;
    // The first `n` of the fields `f`, as `role` lists them.
    let first = |f: u8, n: usize| { let mut m = 0; let mut left = n; for b in [A, B, C] { if f & b != 0 && left > 0 { m |= b; left -= 1; } } m };
    match ins.op {
        Jump => JUMPS,
        JumpIfFalseR => B | JUMPS,
        JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq | JumpUnlessEq | JumpUnlessNotEq => B | C | JUMPS,
        ReturnR => B,
        PopTop => 0,
        PushRegs => first(A | B | C, ins.x as usize),
        StoreTopR if ins.x == 0 => DEST,
        LoadGlobalR | LoadGlobalAtR | UnbindR => DEST,
        // An unpacking writes `a` on and as many after it as it unpacks into, `b` and `c`, which a section maps apart.
        UnpackR => DEST,
        SuperR => DEST | B | C,
        WrapCellR => DEST | A,
        CallValueR => 0,
        Move | MinusR | NotR => DEST | B,
        AddR | InPlaceAddR | SubR | InPlaceSubR | MulR | DivR | ModR | FloorDivR | BitAndR | BitOrR | BitXorR | ShlR | ShrR
            | LtR | LtEqR | GtR | GtEqR | EqR | NotEqR | IsR | IsNotR | PowR | InR | NotInR | GetItemR => DEST | B | C,
        // An attribute's `c` names it in the body's own chunk, which its slow path reads.
        LenR | GetAttrR | LoadCellR => DEST | B,
        StoreCellR => A | B,
        StoreItemR => A | B | C,
        SetAttrR => A | B,
        StoreGlobalR | StoreGlobalAtR => B,
        CallGlobalR => first(B | C, ins.x as usize),
        CallR => first(A | B | C, ins.x as usize + 1),
        CallMethodR => first(B | C, ins.x as usize + 1),
        CallSpreadR => A | B,
        ArgsR => A | B | C,
        // An error it raises leaves the section as any error does, from the value a PushRegs before it gives.
        Raise if ins.a == 0 && ins.x == 0 => 0,
        // A string a loop appends to grows in the register it reads and writes, which a loop's entry gives up.
        AppendR => DEST | A | B,
        OwnResetR => A,
        ListAppendR | SetAddR | MapAddR => B | C,
        ListPushR => A | B,
        // A `for` step writes its item to `b`, read here so a section maps it, and leaves the loop at `a`.
        ForIterR if ins.x == 0 => B | JUMPS,
        // A step left on the stack, its item taken off it after, as an unpacking `for` does.
        ForIter => JUMPS,
        // A stack jump of a `try`'s handler.
        JumpIfFalse | JumpIfFalseOrPop | JumpIfTrueOrPop => JUMPS,
        // A name an `except ... as` binds and deletes.
        Del if ins.x == 0 => DEST,
        Del => 0,
        _ => NONE,
    }
}


/* Whether lowered code `ins` may be laid as a section, each instruction one a section holds or stack work naming no slot, each call's result stored right after it. */
pub(crate) fn pure(ins: &[Ins]) -> bool { laid(ins, false) }

/* Whether a generator's body may be laid as the section a `for` steps, each `yield` taking no value back and no return inside a `try` or `with`, whose ends it skips. */
pub(crate) fn gen_pure(ins: &[Ins]) -> bool {
    laid(ins, true) && ins.iter().enumerate().all(|(k, i)| i.op != Phi || dead_phi(ins, k, i.a))
        && !ins.iter().any(|i| matches!(i.op, MakeFunction | MakeCoroutine)) && returns_outside(ins)
}

/* Whether every return of the body `ins` lies outside its `try`s and `with`s, read in the order they are laid. */
fn returns_outside(ins: &[Ins]) -> bool {
    let mut depth = 0i32;
    for (k, i) in ins.iter().enumerate() {
        match i.op {
            SetupExcept | SetupFinally => depth += 1,
            PopExcept => depth -= 1,
            // A `with` its body falls out of pops its block as it exits.
            WithExit if i.x == 2 => depth -= 1,
            ReturnR | ReturnValue if depth != 0 => return false,
            PushRegs if depth != 0 && returns(ins, k) => return false,
            _ => {}
        }
    }
    true
}

/* Whether the generator's body `ins` has a `try` or a `with`, whose blocks its section holds apart while its loop's body runs. */
pub(crate) fn guarded(ins: &[Ins]) -> bool { ins.iter().any(|i| matches!(i.op, SetupExcept | SetupFinally | WithEnter)) }

/* Whether the slot `r` the Phi at `k` writes is read on no path from it before a write, so a section leaves it out (most often a loop variable's join past its end). */
pub(crate) fn dead_phi(ins: &[Ins], k: usize, r: u16) -> bool {
    let mut seen = alloc::vec![false; ins.len() + 1];
    let mut todo = alloc::vec![k + 1];
    while let Some(j) = todo.pop() {
        let Some(i) = ins.get(j) else { continue };
        if core::mem::replace(&mut seen[j], true) { continue; }
        let (reads, writes) = match role(i) {
            // A `for` step writes the `b` its role names.
            Some(_) if i.op == ForIterR => (false, i.b == r),
            Some((dest, reads, _)) => (reads.iter().any(|&f| field(i, f) == r), dest && i.a == r),
            // Every Phi is left out, so none reads.
            None => (matches!(i.op, LoadName | Del) && i.a == r, i.op == StoreName && i.a == r),
        };
        if reads { return false; }
        if writes || matches!(i.op, ReturnR | ReturnValue | Raise) { continue; }
        if i.op != Jump { todo.push(j + 1); }
        if matches!(role(i), Some((_, _, true))) { todo.push(i.a as usize); }
    }
    true
}

/* `pure`, or for a generator's body `gen_pure` before its loops are counted. */
fn laid(ins: &[Ins], yields: bool) -> bool {
    let call = |k: usize| ins.get(k).is_some_and(|i| matches!(i.op, CallGlobalR | CallR | CallMethodR | CallSpreadR));
    // Where the call's result is stored, past the ArgsR of a wide one.
    let stored = |k: usize| k + 1 + ins.get(k).is_some_and(|&i| super::lower::wide(i)) as usize;
    !cfg!(feature = "coverage") && ins.iter().enumerate().all(|(k, i)| match role(i) {
        // A test that goes back to the step of a `for` it lies in skips to its next item, as the loop's back-edge does.
        Some((_, _, true)) => (k + 1..=ins.len()).contains(&(i.a as usize)) || back_edge(ins, k)
            || i.op != Jump && ins.get(i.a as usize).is_some_and(|t| t.op == ForIterR && t.x == 0 && (t.a as usize) > k),
        Some(_) => match i.op {
            CallGlobalR | CallR | CallMethodR | CallSpreadR => ins.get(stored(k)).is_some_and(|t| t.op == StoreTopR && t.x == 0),
            ArgsR => k > 0 && call(k - 1) && super::lower::wide(ins[k - 1]),
            _ => true,
        },
        // A generator's `yield` takes no value back and its calls may spread, a `yield from` whose value goes unused leaving its push to the PopTop after it.
        None => stack_work(i.op) || yields && (i.op == Yield && (i.x == 1 || ins.get(k + 1).is_some_and(|n| n.op == PopTop)) || matches!(i.op, BeginArgs | UnpackArgs | CallSpread)
            || i.op == LoadYieldFrom && ins.get(k + 1).is_some_and(|n| n.op == PopTop)),
    })
}

/* Whether laid code `ins` leaves nothing of its own on the stack between instructions, but the value a PushRegs hands the Raise right after it. */
pub(crate) fn flat(ins: &[Ins]) -> bool {
    ins.iter().enumerate().all(|(k, i)| match i.op {
        PushRegs => ins.get(k + 1).is_some_and(|n| n.op == Raise),
        Jump | Raise => true,
        op => op.is_register(),
    })
}

/* Whether stack opcode `op` may run inside a section, reading and writing only the stack, names in the chunk it was laid from, or the handlers of a `try` and a `with`. */
fn stack_work(op: crate::parser::OpCode) -> bool {
    matches!(op, SetupExcept | SetupFinally | PopExcept | BeginFinally | EndFinally | Dup | Dup2 | CallIsInstance | PopTop | Raise | StoreTopR
        | PushRegs | ReturnValue | Call | CallPrint | CallStr | CallInt | CallFloat | CallBool | CallAbs | CallChr | CallOrd | CallLen | CallRepr | CallType
        | CallMin | CallMax | CallSum | CallRound | CallList | CallTuple | CallDict | CallSet | CallBin | CallOct | CallHex | CallDivmod | CallPow
        | CallAll | CallAny | BuildString | FormatValue | BuildTuple | BuildList | BuildDict | BuildSet | BuildSlice | GetItem | StoreItem | DelItem
        | Add | Sub | Mul | Div | Mod | Pow | FloorDiv | Eq | NotEq | Lt | Gt | LtEq | GtEq | In | NotIn | Is | IsNot | BitAnd | BitOr | BitXor
        | BitNot | Shl | Shr | Minus | Not | LoadTrue | LoadFalse | LoadNone | LoadEllipsis | LoadAttr | StoreAttr | DelAttr | CallMethod
        | CallMethodArgs | UnpackSequence | WithEnter | WithExit | WithJudge | Assert | GetIter | PopIter | CallRange | CallEnumerate | CallZip | CallReversed | CallInput | Phi | ListAppend | SetAdd | MapAdd | InPlaceAdd | InPlaceSub | MakeFunction | MakeCoroutine | StoreName
        | MatchSeq | MatchMap | MatchClass | Swap | DictUpdate | SetUpdate | ListExtend)
}

/* For each instruction of `ins`, how many `for` loops it lies inside, from a GetIter until its loop's ForIterR leaves. */
pub(crate) fn loops_around(ins: &[Ins]) -> alloc::vec::Vec<usize> {
    let mut depth = alloc::vec![0; ins.len() + 1];
    for (g, i) in ins.iter().enumerate().filter(|(_, i)| i.op == GetIter) {
        let _ = i;
        let Some(f) = ins[g + 1..].iter().position(|i| matches!(i.op, ForIterR | ForIter)).map(|f| g + 1 + f) else { continue };
        // A stub's GetIter, past the code, is followed by no loop of its own.
        for d in depth.get_mut(g + 1..(ins[f].a as usize).min(ins.len())).unwrap_or_default() { *d += 1; }
    }
    depth
}

/* Whether the instruction at `k` is a loop's back-edge a section may hold, a Jump back in a body whose loops step by ForIterR or ForIter. */
fn back_edge(ins: &[Ins], k: usize) -> bool {
    ins[k].op == Jump && ins[k].x == 0 && (ins[k].a as usize) <= k
}

/* Whether the PushRegs at `k` pushes the value a ReturnValue returns, right after it or past a Jump. */
pub(crate) fn returns(ins: &[Ins], k: usize) -> bool {
    let at = |m: usize| ins.get(m).is_some_and(|i| i.op == ReturnValue);
    ins[k].op == PushRegs && ins[k].x == 1 && (at(k + 1) || ins.get(k + 1).is_some_and(|j| j.op == Jump && at(j.a as usize)))
}

/* The module binding the call at `j` of `ins` calls, its own operand or the LoadGlobalR that last wrote its callee register. */
pub(crate) fn global_of(ins: &[Ins], j: usize, call: Ins) -> Option<u16> {
    match call.op {
        CallGlobalR => Some(call.a),
        // A function made on the stack writes no register, and one pending under an earlier call, pushed before it and taken back past it, is the one it was.
        CallR => (0..j).rev().find_map(|d| { let i = &ins[d]; match role(i) {
            Some((true, _, _)) if i.a == call.a && i.op == StoreTopR && let Some(p) = pushed_back(ins, d, i.a) => Some(global_of(ins, p, call)),
            Some((true, _, _)) if i.a == call.a => Some((i.op == LoadGlobalR).then_some(i.b)),
            _ if i.op == UnpackR && [i.b, i.c].iter().take(i.x as usize - 1).any(|&r| r == call.a) => Some(None),
            // A section call before it writes only the register its store names.
            _ if matches!(i.op, CallSectionR | SectionR) => None,
            Some(_) => None,
            None if i.op == MakeFunction => None,
            // An `isinstance` on the stack writes no register.
            None if i.op == CallIsInstance => None,
            None => Some(None),
        }})?,
        _ => None,
    }
}

/* For the StoreTopR at `d` taking register `r` back off the stack past a call, the PushRegs before that call that pushed the values it left pending. */
fn pushed_back(ins: &[Ins], d: usize, r: u16) -> Option<usize> {
    let first = (0..d).rev().find(|&k| ins[k].op != StoreTopR)?;
    let (call, stores) = (first - (ins[first].op == ArgsR) as usize, d - first);
    if !matches!(ins[call].op, CallR | CallGlobalR | CallMethodR) { return None; }
    let push = ins.get(call.checked_sub(1)?)?;
    if push.op != PushRegs || push.x as usize + 1 != stores { return None; }
    // Each store takes back the value pushed in the place it pops, the last pushed first.
    let regs = [push.a, push.b, push.c];
    (0..push.x as usize).all(|k| ins[first + 2 + k].op == StoreTopR && ins[first + 2 + k].a == regs[push.x as usize - 1 - k]).then_some(call - 1).filter(|_| regs[..push.x as usize].contains(&r))
}

/* Whether `i` may be laid as a call of a section whose level a recursion keeps, a call, an op a dunder may run, a load a getter may run or a truth test. */
fn level_at(i: &Ins) -> bool { matches!(i.op, CallGlobalR | CallR | CallMethodR | CallSpreadR | JumpIfFalseR | GetAttrR | Call) || super::lower::dunder_op(i.op) }

/* For the level at `k`, the row of registers live past it and the register it writes, a call's store, an op's result or none for a truth test. */
fn after(ins: &[Ins], k: usize) -> (usize, Option<u16>) {
    let i = ins[k];
    match i.op {
        JumpIfFalseR => (k, None),
        Call => (k + 1, None),
        CallGlobalR | CallR | CallMethodR | CallSpreadR => { let s = k + 1 + super::lower::wide(i) as usize; (s, ins.get(s).map(|t| t.a)) }
        _ => (k + 1, Some(i.a)),
    }
}

/* For each call `pure` takes and each op or truth test that may call a section, the registers written before and read after it, which a call coming back into it must keep. */
pub(crate) fn live_across(ins: &[Ins], regs: usize, tries: &[(usize, usize)]) -> alloc::vec::Vec<alloc::vec::Vec<u16>> {
    use alloc::vec::Vec;
    if regs <= 64 { return live_word(ins, regs, tries); }
    let n = ins.len();
    let call = |k: usize| ins.get(k).is_some_and(|i| matches!(i.op, CallGlobalR | CallR | CallMethodR | CallSpreadR));
    let under = |k: usize| call(k) && k > 0 && ins[k - 1].op == PushRegs && ins[k - 1].x == 1;
    let words = regs.div_ceil(64).max(1);
    let mut live = alloc::vec![0u64; (n + 1) * words];
    let mut set = alloc::vec![0u64; words];
    let mut out = alloc::vec![Vec::new(); n];
    let def = |set: &mut [u64], r: u16| if (r as usize) < regs { set[r as usize / 64] &= !(1 << (r % 64)) };
    let used = |set: &mut [u64], r: u16| if (r as usize) < regs { set[r as usize / 64] |= 1 << (r % 64) };
    let stored = |k: usize| k + 1 + super::lower::wide(ins[k]) as usize;
    // A pass from the end sees each forward target before its jumps but a back-edge's head only after, so code with a back-edge repeats passes until nothing changes.
    let roles: Vec<_> = ins.iter().map(role).collect();
    let back = (0..n).any(|k| (ins[k].op == Jump || roles[k].is_some_and(|r| r.2)) && (ins[k].a as usize) <= k);
    let mut changed = true;
    while core::mem::take(&mut changed) {
    for k in (0..n).rev() {
        let i = ins[k];
        let row = |at: usize| at.min(n) * words;
        match i.op {
            ReturnR => set.fill(0),
            Jump => set.copy_from_slice(&live[row(i.a as usize)..][..words]),
            _ => match roles[k] {
                Some((_, _, true)) => for w in 0..words { set[w] = live[row(k + 1) + w] | live[row(i.a as usize) + w]; },
                _ => set.copy_from_slice(&live[row(k + 1)..][..words]),
            },
        }
        // The value pending under a call stays in its register in a section, so its push and its restore do nothing.
        let restores = |c: usize| under(c) && stored(c) + 1 == k;
        let skipped = i.op == PushRegs && under(k + 1) || matches!(i.op, StoreTopR | PopTop) && (k > 1 && restores(k - 2) || k > 2 && restores(k - 3));
        if level_at(&i) {
            let (s, dest) = after(ins, k);
            let past = &live[row(s)..][..words];
            out[k] = (0..regs).filter(|&r| past[r / 64] & 1 << (r % 64) != 0 && Some(r as u16) != dest).map(|r| r as u16).collect();
        }
        if !skipped {
            match i.op {
                ReturnR => used(&mut set, i.b),
                StoreTopR => def(&mut set, i.a),
                UnpackR => for &r in [i.a, i.b, i.c].iter().take(i.x as usize) { def(&mut set, r) },
                // A `with` writes its dunder and what it is called with to the registers from `b` on.
                WithEnter | WithExit if i.x != 0 => {
                    for r in i.b..i.b + if i.op == WithEnter { 2 } else { 5 } { def(&mut set, r) }
                    if i.op == WithEnter && i.x == 2 { used(&mut set, i.c); }
                }
                WithJudge if i.x == 1 => used(&mut set, i.a),
                _ => if let Some((dest, reads, _)) = roles[k] {
                    if dest { def(&mut set, i.a); }
                    for &f in reads { used(&mut set, field(&i, f)); }
                },
            }
        }
        let at = &mut live[k * words..][..words];
        if *at != *set { changed = back; at.copy_from_slice(&set); }
    }
    }
    for (k, o) in out.iter_mut().enumerate().filter(|(k, _)| level_at(&ins[*k])) {
        for &(_, h) in tries.iter().filter(|&&(s, h)| s < k && k < h) {
            let row = &live[h.min(n) * words..][..words];
            let more: Vec<u16> = (0..regs).filter(|&r| row[r / 64] & 1 << (r % 64) != 0 && !o.contains(&(r as u16))).map(|r| r as u16).collect();
            o.extend(more);
        }
    }
    out
}

/* For a frame of 64 registers at most, the registers live before each instruction of `ins`, found by passes from the end until nothing changes, only past `from` unless a jump goes back before it. */
fn live_rows(ins: &[Ins], regs: usize, from: usize) -> alloc::vec::Vec<u64> {
    let n = ins.len();
    let mut from = from.min(n);
    let is_call = |op| matches!(op, CallGlobalR | CallR | CallMethodR | CallSpreadR);
    let under = |c: usize| c > 0 && c < n && is_call(ins[c].op) && ins[c - 1].op == PushRegs && ins[c - 1].x == 1;
    let stored = |c: usize| c + 1 + super::lower::wide(ins[c]) as usize;
    let bit = |r: u16| if (r as usize) < regs { 1u64 << r } else { 0 };
    let mut live = alloc::vec![0u64; n + 1];
    // Code with no jump back is settled by its first pass.
    loop {
        let (mut changed, mut back, mut again) = (false, false, false);
        let mut next = 0u64;
        for k in (from..n).rev() {
            let i = ins[k];
            let kind = kinds(&i);
            let jumps = kind & (NONE | JUMPS) == JUMPS;
            // A jump back before the first row found has every row found, afresh.
            if (i.op == Jump || jumps) && (i.a as usize) < from { (from, again) = (0, true); break; }
            let target = || live[(i.a as usize).min(n)];
            let mut set = match i.op { ReturnR => 0, Jump => target(), _ if jumps => next | target(), _ => next };
            back |= (i.op == Jump || jumps) && (i.a as usize) <= k;
            let skipped = match i.op {
                PushRegs => under(k + 1),
                StoreTopR | PopTop => k > 1 && under(k - 2) && stored(k - 2) + 1 == k || k > 2 && under(k - 3) && stored(k - 3) + 1 == k,
                _ => false,
            };
            if !skipped {
                match i.op {
                    ReturnR => set |= bit(i.b),
                    StoreTopR => set &= !bit(i.a),
                    UnpackR => for &r in [i.a, i.b, i.c].iter().take(i.x as usize) { set &= !bit(r) },
                    // A `with` writes its dunder and what it is called with to the registers from `b` on.
                    WithEnter | WithExit if i.x != 0 => {
                        for r in i.b..i.b + if i.op == WithEnter { 2 } else { 5 } { set &= !bit(r) }
                        if i.op == WithEnter && i.x == 2 { set |= bit(i.c); }
                    }
                    WithJudge if i.x == 1 => set |= bit(i.a),
                    _ if kind & NONE == 0 => {
                        if kind & DEST != 0 { set &= !bit(i.a); }
                        if kind & 2 != 0 { set |= bit(i.a); }
                        if kind & 4 != 0 { set |= bit(i.b); }
                        if kind & 8 != 0 { set |= bit(i.c); }
                    }
                    _ => {}
                }
            }
            if live[k] != set { live[k] = set; changed = true; }
            next = set;
        }
        if again { live.fill(0); continue; }
        if !(back && changed) {
            return live;
        }
    }
}


/* `live_across` for a frame of 64 registers at most. */
fn live_word(ins: &[Ins], regs: usize, tries: &[(usize, usize)]) -> alloc::vec::Vec<alloc::vec::Vec<u16>> {
    use alloc::vec::Vec;
    let n = ins.len();
    let live = live_rows(ins, regs, first_read(ins, tries));
    let mut out: Vec<Vec<u16>> = Vec::new();
    out.resize_with(n, Vec::new);
    each_call(ins, regs, tries, &live, |k, w| out[k] = w.to_vec());
    out
}

/* The first instruction past which a call of `ins` reads its live registers, past the first call or the handler of a `try` around one. */
fn first_read(ins: &[Ins], tries: &[(usize, usize)]) -> usize {
    let Some(c) = ins.iter().position(level_at) else { return ins.len() };
    tries.iter().filter(|&&(s, h)| ins[s..h.min(ins.len())].iter().any(level_at)).map(|&(_, h)| h).fold(c, usize::min)
}

/* For each call of `ins`, by the rows `live_rows` found, the registers `live_across` gives it, past a scratch list. */
fn each_call(ins: &[Ins], regs: usize, tries: &[(usize, usize)], live: &[u64], mut f: impl FnMut(usize, &[u16])) {
    let n = ins.len();
    let bit = |r: u16| if (r as usize) < regs { 1u64 << r } else { 0 };
    let mut regs_of = alloc::vec::Vec::new();
    let push = |w: u64, out: &mut alloc::vec::Vec<u16>| { let mut w = w; while w != 0 { out.push(w.trailing_zeros() as u16); w &= w - 1; } };
    for (k, i) in ins.iter().enumerate() {
        if !level_at(i) { continue; }
        let (s, dest) = after(ins, k);
        regs_of.clear();
        let main = live[s.min(n)] & !dest.map_or(0, bit);
        push(main, &mut regs_of);
        let mut seen = main;
        // A call inside a `try` also keeps what its handler reads, which an exception out of a call back into the section reaches with this level's registers taken back.
        for &(_, h) in tries.iter().filter(|&&(s, h)| s < k && k < h) {
            let more = live[h.min(n)] & !seen;
            push(more, &mut regs_of);
            seen |= more;
        }
        f(k, &regs_of);
    }
}

/* The parameters, the first `arity` registers, that some instruction of `ins` writes, a bit each for the first 64. */
pub(crate) fn params_written(ins: &[Ins], arity: usize) -> u64 {
    (0..arity.min(64)).filter(|&r| ins.iter().any(|i| writes(i, r as u16))).fold(0, |m, r| m | 1 << r)
}

/* Which instructions of `ins` a jump lands on, or the handler of one of `tries`, one past the last too. */
pub(crate) fn targets(ins: &[Ins], tries: &[(usize, usize)]) -> alloc::vec::Vec<bool> {
    let mut t = alloc::vec![false; ins.len() + 1];
    for i in ins.iter().filter(|i| i.op == Jump || kinds(i) & (NONE | JUMPS) == JUMPS) { if let Some(e) = t.get_mut(i.a as usize) { *e = true; } }
    for &(_, h) in tries { if let Some(e) = t.get_mut(h) { *e = true; } }
    t
}

/* For the call at `j` back into the body it lies in, whose `arity` parameters are its first registers, the parameters it passes on as they are, a bit each. */
pub(crate) fn passed(ins: &[Ins], j: usize, arity: usize) -> u64 {
    let c = ins[j];
    let rest = if super::lower::wide(c) { ins[j + 1] } else { c };
    let n = (c.x as usize + (c.op == CallMethodR) as usize).min(5).min(arity).min(64);
    [c.b, c.c, rest.a, rest.b, rest.c].iter().enumerate().take(n).filter(|&(q, &t)| t as usize == q).fold(0, |m, (q, _)| m | 1 << q)
}

/* For the stack call at `j` of `n` positionals, the PushRegs that pushed its function and register, found walking back over ops of plain stack effect. */
pub(crate) fn stack_callee(ins: &[Ins], j: usize, n: usize) -> Option<(usize, u16)> {
    let mut need = n + 1;
    for k in (0..j).rev() {
        let i = ins[k];
        let (pops, pushes) = match i.op {
            PushRegs => (0, i.x as usize),
            StoreTopR if i.x == 0 => (1, 0),
            PopTop => (1, 0),
            Dup => (1, 2),
            BuildSlice | BuildList | BuildTuple | BuildSet => (i.a as usize, 1),
            GetItem | Add | Sub | Mul | Div | FloorDiv | Mod | Pow | BitAnd | BitOr | BitXor | Shl | Shr | Eq | NotEq | Lt | LtEq | Gt | GtEq | In | NotIn | Is | IsNot => (2, 1),
            Minus | Not | LoadAttr => (1, 1),
            Call if i.a < 0x100 => (i.a as usize + 1, 1),
            _ if role(&i).is_some() => (0, 0),
            _ => return None,
        };
        if need <= pushes { return (i.op == PushRegs).then(|| (k, [i.a, i.b, i.c][pushes - need])); }
        need = need - pushes + pops;
    }
    None
}

fn writes(i: &Ins, r: u16) -> bool {
    match i.op {
        UnpackR => [i.a, i.b, i.c].iter().take(i.x as usize).any(|&t| t == r),
        WithEnter | WithExit if i.x != 0 => (i.b..i.b + if i.op == WithEnter { 2 } else { 5 }).contains(&r),
        ForIterR if i.x == 0 => i.b == r,
        _ => kinds(i) & (NONE | DEST) == DEST && i.a == r,
    }
}

pub(crate) fn field(i: &Ins, f: u8) -> u16 { match f { 0 => i.a, 1 => i.b, _ => i.c } }

pub(crate) fn field_mut(i: &mut Ins, f: u8) -> &mut u16 { match f { 0 => &mut i.a, 1 => &mut i.b, _ => &mut i.c } }

