use crate::code::Ins;
use crate::code::OpCode::*;
use crate::code::Rows;

/* whether a section's instruction writes `a` plus the fields it reads and whether it jumps */
#[inline(always)]
pub(crate) fn role(ins: &Ins) -> Option<(bool, &'static [u8], bool)> {
    const READS: [&[u8]; 8] = [&[], &[0], &[1], &[0, 1], &[2], &[0, 2], &[1, 2], &[0, 1, 2]];
    let k = kinds(ins);
    (k & NONE == 0).then(|| (k & DEST != 0, READS[(k >> 1 & 7) as usize], k & JUMPS != 0))
}

/* `role` as bits being `DEST` then read fields from bit 1 then `JUMPS` or `NONE` */
pub(crate) const DEST: u8 = 1;
pub(crate) const JUMPS: u8 = 16;
pub(crate) const NONE: u8 = 32;

#[inline(always)]
pub(crate) fn kinds(ins: &Ins) -> u8 {
    const A: u8 = 2;
    const B: u8 = 4;
    const C: u8 = 8;
    // the role bits of an OpR kept apart so `kinds` stays small
    #[inline(never)]
    fn op_kinds(ins: &Ins) -> u8 {
        match rows(ins) {
            Some(([d, _], s)) => (if d.is_empty() { 0 } else { DEST }) | (if s.is_empty() { 0 } else { B }),
            // a maker writes `a` and the row moved into temporaries before it stands at `b`
            None => DEST | if ins.b == super::lower::NO_ROW { 0 } else { B },
        }
    }
    // the first `n` of the fields `f` in the order `role` lists them
    let first = |f: u8, n: usize| match (f & A != 0, n) { (_, 0) => 0, (true, 1) => A, (true, 2) => A | B, (true, _) => A | B | C, (false, 1) => B, (false, _) => B | C };
    match ins.op {
        Jump => JUMPS,
        JumpIfFalseR => B | JUMPS,
        JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq | JumpUnlessEq | JumpUnlessNotEq => B | C | JUMPS,
        ReturnR => B,
        PopTop => 0,
        PushRegs => first(A | B | C, ins.x as usize),
        StoreTopR if ins.x == 0 => DEST,
        LoadGlobalR | LoadGlobalAtR | UnbindR => DEST,
        // an unpacking reads `b` into its row from `a` and a pair reads `c`
        UnpackR => if ins.x != 0 { DEST | C } else if ins.c > 0 { DEST | B } else { B },
        // a row form names each row by its first register
        OpR => op_kinds(ins),
        // a raise reads what it raises unless it raises the handled exception again
        RaiseR => if ins.x == 0 { B } else { 0 },
        WrapCellR => DEST | A,
        CallValueR => 0,
        Move | MinusR | NotR | UnaryR => DEST | B,
        AddR | InPlaceAddR | SubR | InPlaceSubR | MulR | DivR | ModR | FloorDivR | BitAndR | BitOrR | BitXorR | ShlR | ShrR
            | LtR | LtEqR | GtR | GtEqR | EqR | NotEqR | IsR | IsNotR | PowR | InR | NotInR | GetItemR => DEST | B | C,
        // an attribute's `c` names it in the body's own chunk which its slow path reads
        LenR | GetAttrR | LoadCellR => DEST | B,
        // a loop's iterator reads its source and the value form writes `a`
        GetIterR => if ins.x as u16 == crate::syntax::parser::ITER_VALUE { DEST | B } else { B },
        StoreCellR => A | B,
        StoreItemR => A | B | C,
        SetAttrR => A | B,
        StoreGlobalR | StoreGlobalAtR => B,
        CallGlobalR => first(B | C, ins.x as usize),
        CallR => first(A | B | C, ins.x as usize + 1),
        CallMethodR => first(B | C, ins.x as usize + 1),
        // a native's call reads its arguments from `b` on and names its native in `a`
        CallNativeR => first(B | C, (ins.x & super::lower::ARGC) as usize),
        MatchExcR => DEST | B | C,
        ListToTupleR => DEST | B,
        CallSpreadR => A | B,
        ArgsR => A | B | C,
        // raising what a prior PushRegs gave leaves the section as any error does
        Raise if ins.a == 0 && ins.x == 0 => 0,
        // a string grows in the register it reads and writes which loop entry gives up
        AppendR => DEST | A | B,
        OwnResetR => A,
        // builds and runtime ops read a row from `b` that liveness takes through `rows`
        BuildR | RuntimeR => if ins.c == 0 { DEST } else { DEST | B },
        // a row op reads and writes its row from `a` which liveness takes through `rows`
        RowsR => DEST | A,
        RowsBackR => A,
        FormatR => if ins.x & 1 != 0 { DEST | B | C } else { DEST | B },
        // an accumulator a register holds is read from `a`
        ListAppendR | SetAddR | MapAddR => if ins.x == 1 { A | B | C } else { B | C },
        ListPushR => A | B,
        // a `for` step's item written to `b` counts as read so a section maps it
        ForIterR if ins.x == 0 => B | JUMPS,
        // a stack step whose item is taken off after as an unpacking `for` does
        ForIter => JUMPS,
        // a stack jump of a `try`'s handler
        JumpIfFalse | JumpIfFalseOrPop | JumpIfTrueOrPop => JUMPS,
        // a name an `except` clause binds with `as` and then deletes
        Del if ins.x == 0 => DEST,
        Del => 0,
        _ => NONE,
    }
}


/* whether lowered `ins` lays as a section with each call's result stored right after it */
pub(crate) fn pure(ins: &[Ins]) -> bool { laid(ins, false) }

/* whether a generator lays as a `for`'s section with no return skipping a `try` end */
pub(crate) fn gen_pure(ins: &[Ins]) -> bool {
    laid(ins, true) && ins.iter().enumerate().all(|(k, i)| i.op != Phi || dead_phi(ins, k, i.a))
        && !ins.iter().any(makes) && returns_outside(ins)
}

/* whether `i` makes a function by its stack form or its OpR */
pub(crate) fn makes(i: &Ins) -> bool {
    matches!(i.op, MakeFunction | MakeCoroutine) || i.op == OpR && (i.x == MakeFunction as u8 || i.x == MakeCoroutine as u8)
}

/* whether every return of the body `ins` lies outside its `try`s read in laid order */
fn returns_outside(ins: &[Ins]) -> bool {
    let mut depth = 0i32;
    for (k, i) in ins.iter().enumerate() {
        match i.op {
            SetupExcept | SetupFinally => depth += 1,
            PopExcept => depth -= 1,
            ReturnR | ReturnValue if depth != 0 => return false,
            PushRegs if depth != 0 && returns(ins, k) => return false,
            _ => {}
        }
    }
    true
}

/* whether generator body `ins` has a `try` or `with` whose blocks its section holds apart */
pub(crate) fn guarded(ins: &[Ins]) -> bool { ins.iter().any(|i| matches!(i.op, SetupExcept | SetupFinally)) }

/* whether the Phi at `k` is dead as no path reads `r` before writing it */
pub(crate) fn dead_phi(ins: &[Ins], k: usize, r: u16) -> bool {
    let mut seen = alloc::vec![false; ins.len() + 1];
    let mut todo = alloc::vec![k + 1];
    while let Some(j) = todo.pop() {
        let Some(i) = ins.get(j) else { continue };
        if core::mem::replace(&mut seen[j], true) { continue; }
        let (reads, writes) = match role(i) {
            // a `for` step writes the `b` its role names
            Some(_) if i.op == ForIterR => (false, i.b == r),
            Some((dest, reads, _)) => (reads.iter().any(|&f| field(i, f) == r) || rows(i).is_some_and(|(_, s)| s.contains(&r)), dest && i.a == r),
            // every Phi is left out so none reads
            None => (matches!(i.op, LoadName | Del) && i.a == r, i.op == StoreName && i.a == r),
        };
        if reads { return false; }
        if writes || matches!(i.op, ReturnR | ReturnValue | Raise | RaiseR) { continue; }
        if i.op != Jump { todo.push(j + 1); }
        if matches!(role(i), Some((_, _, true))) { todo.push(i.a as usize); }
    }
    true
}

/* `pure` or for a generator's body `gen_pure` before its loops are counted */
fn laid(ins: &[Ins], yields: bool) -> bool {
    let call = |k: usize| ins.get(k).is_some_and(|i| matches!(i.op, CallGlobalR | CallR | CallMethodR | CallSpreadR | CallNativeR));
    // where the call's result is stored past the ArgsR of a wide one
    let stored = |k: usize| k + 1 + ins.get(k).is_some_and(|&i| super::lower::wide(i)) as usize;
    !cfg!(feature = "coverage") && ins.iter().enumerate().all(|(k, i)| match role(i) {
        // a test jumping to its `for` step takes the next item as the back-edge does
        Some((_, _, true)) => (k + 1..=ins.len()).contains(&(i.a as usize)) || back_edge(ins, k)
            || i.op != Jump && ins.get(i.a as usize).is_some_and(|t| t.op == ForIterR && t.x == 0 && (t.a as usize) > k),
        Some(_) => match i.op {
            // a statement's native call takes nothing back
            CallNativeR if i.x & super::lower::VOID != 0 => true,
            CallGlobalR | CallR | CallMethodR | CallSpreadR | CallNativeR => ins.get(stored(k)).is_some_and(|t| t.op == StoreTopR && t.x == 0),
            ArgsR => k > 0 && call(k - 1) && super::lower::wide(ins[k - 1]),
            // a class opens a floor while imports and `yield from` values stay apart
            OpR => !matches!(crate::code::OpCode::ALL[i.x as usize], MakeClass | LoadModule | LoadYieldFrom),
            // a section holds a row op as its stack form but not a method call
            RowsR => match crate::code::OpCode::ALL[i.x as usize] {
                Call | CallNative | ListExtend | SetUpdate | DictUpdate => true,
                UnpackArgs | CallSpread => yields,
                _ => false,
            },
            _ => true,
        },
        // handler blocks and a bare raise touch no register and a yield takes nothing back
        None => matches!(i.op, SetupExcept | SetupFinally | PopExcept | BeginFinally | EndFinally | Raise | Phi | PopIter)
            || yields && (i.op == Yield && (i.x & 3 == 1 || ins.get(k + 1).is_some_and(|n| n.op == PopTop)) || matches!(i.op, BeginArgs | UnpackArgs | CallSpread)),
    })
}

/* the `for` loops around each instruction of `ins` counted from GetIter to the ForIterR exit */
pub(crate) fn loops_around(ins: &[Ins]) -> alloc::vec::Vec<usize> {
    let mut depth = alloc::vec![0; ins.len() + 1];
    for (g, i) in ins.iter().enumerate().filter(|(_, i)| matches!(i.op, GetIter | GetIterR)) {
        let _ = i;
        let Some(f) = ins[g + 1..].iter().position(|i| matches!(i.op, ForIterR | ForIter)).map(|f| g + 1 + f) else { continue };
        // a stub's GetIter past the code is followed by no loop of its own
        for d in depth.get_mut(g + 1..(ins[f].a as usize).min(ins.len())).unwrap_or_default() { *d += 1; }
    }
    depth
}

/* whether `k` is a Jump back which a section may hold as its loop's back-edge */
fn back_edge(ins: &[Ins], k: usize) -> bool {
    ins[k].op == Jump && ins[k].x == 0 && (ins[k].a as usize) <= k
}

/* whether the PushRegs at `k` feeds a ReturnValue right after it or past a Jump */
pub(crate) fn returns(ins: &[Ins], k: usize) -> bool {
    let at = |m: usize| ins.get(m).is_some_and(|i| i.op == ReturnValue);
    ins[k].op == PushRegs && ins[k].x == 1 && (at(k + 1) || ins.get(k + 1).is_some_and(|j| j.op == Jump && at(j.a as usize)))
}

/* the module binding call `j` targets by operand or the last LoadGlobalR into its callee */
pub(crate) fn global_of(ins: &[Ins], j: usize, call: Ins) -> Option<u16> {
    match call.op {
        CallGlobalR => Some(call.a),
        // a function made on the stack writes no register
        CallR => (0..j).rev().find_map(|d| { let i = &ins[d]; match role(i) {
            // a callee spilled into its row is the one the spill read
            Some((true, _, _)) if i.a == call.a && i.op == Move && i.x == 0 => Some(global_of(ins, d, Ins { a: i.b, ..call })),
            Some((true, _, _)) if i.a == call.a => Some((i.op == LoadGlobalR).then_some(i.b)),
            _ if matches!(i.op, UnpackR | OpR | RowsR) && rows(i).is_some_and(|(d, _)| d.iter().any(|d| d.contains(&call.a))) => Some(None),
            // a build a call takes at once reads as the stack build it was
            _ if i.op == BuildR && i.x & super::lower::TAKEN != 0 => Some(None),
            // a section call before it writes only the register its store names
            _ if matches!(i.op, CallSectionR | SectionR) => None,
            Some(_) => None,
            None if i.op == MakeFunction => None,
            // a handler's test on the stack writes no register
            None if i.op == MatchExc => None,
            None => Some(None),
        }})?,
        _ => None,
    }
}

/* whether `i` may call a section whose level a recursion keeps */
fn level_at(i: &Ins) -> bool { matches!(i.op, CallGlobalR | CallR | CallMethodR | CallSpreadR | Call) || i.op == RowsR && super::lower::row_calls(i) }

/* the row live past the level at `k` and the register its call stores to */
fn after(ins: &[Ins], k: usize) -> (usize, Option<u16>) {
    let i = ins[k];
    match i.op {
        CallGlobalR | CallR | CallMethodR | CallSpreadR => { let s = k + 1 + super::lower::wide(i) as usize; (s, ins.get(s).map(|t| t.a)) }
        // a row call's value lands in its row once the take after it ran
        RowsR => (k + 2, Some(i.a)),
        _ => (k + 1, None),
    }
}

/* for each call that may come back into the section the registers it must keep */
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
    // a backward pass reaches a back-edge's head after it so passes repeat until nothing changes
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
        // the value under a call keeps its register so its push and restore do nothing
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
                UnpackR | OpR | BuildR | RowsR | RuntimeR if let Some((d, s)) = rows(&i) => {
                    for r in d.into_iter().flatten() { def(&mut set, r); }
                    for r in s { used(&mut set, r); }
                }
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

/* the registers live before each instruction past `from` unless a jump lands before it */
fn live_rows(ins: &[Ins], regs: usize, from: usize) -> alloc::vec::Vec<u64> {
    let n = ins.len();
    let mut from = from.min(n);
    let is_call = |op| matches!(op, CallGlobalR | CallR | CallMethodR | CallSpreadR);
    let under = |c: usize| c > 0 && c < n && is_call(ins[c].op) && ins[c - 1].op == PushRegs && ins[c - 1].x == 1;
    let stored = |c: usize| c + 1 + super::lower::wide(ins[c]) as usize;
    let bit = |r: u16| if (r as usize) < regs { 1u64 << r } else { 0 };
    let mut live = alloc::vec![0u64; n + 1];
    // code with no jump back is settled by its first pass
    loop {
        let (mut changed, mut back, mut again) = (false, false, false);
        let mut next = 0u64;
        for k in (from..n).rev() {
            let i = ins[k];
            let kind = kinds(&i);
            let jumps = kind & (NONE | JUMPS) == JUMPS;
            // a jump back before the first row found has every row found afresh
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
                    UnpackR | OpR | BuildR | RowsR | RuntimeR if let Some((d, s)) = rows(&i) => {
                        for r in d.into_iter().flatten() { set &= !bit(r); }
                        for r in s { set |= bit(r); }
                    }
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


/* `live_across` for a frame of 64 registers at most */
fn live_word(ins: &[Ins], regs: usize, tries: &[(usize, usize)]) -> alloc::vec::Vec<alloc::vec::Vec<u16>> {
    use alloc::vec::Vec;
    let n = ins.len();
    let live = live_rows(ins, regs, first_read(ins, tries));
    let mut out: Vec<Vec<u16>> = Vec::new();
    out.resize_with(n, Vec::new);
    each_call(ins, regs, tries, &live, |k, w| out[k] = w.to_vec());
    out
}

/* liveness matters from the first call or an earlier handler of a `try` around one */
fn first_read(ins: &[Ins], tries: &[(usize, usize)]) -> usize {
    let Some(c) = ins.iter().position(level_at) else { return ins.len() };
    tries.iter().filter(|&&(s, h)| ins[s..h.min(ins.len())].iter().any(level_at)).map(|&(_, h)| h).fold(c, usize::min)
}

/* gives `f` each call's `live_across` registers from the `live_rows` rows through one scratch list */
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
        // calls in a `try` keep their handler's reads as an exception restores these registers there
        for &(_, h) in tries.iter().filter(|&&(s, h)| s < k && k < h) {
            let more = live[h.min(n)] & !seen;
            push(more, &mut regs_of);
            seen |= more;
        }
        f(k, &regs_of);
    }
}

/* the parameter registers below `arity` that `ins` writes as a bit each up to 64 */
pub(crate) fn params_written(ins: &[Ins], arity: usize) -> u64 {
    (0..arity.min(64)).filter(|&r| ins.iter().any(|i| writes(i, r as u16))).fold(0, |m, r| m | 1 << r)
}

/* which instructions a jump or `tries` handler lands on counting one past the last */
pub(crate) fn targets(ins: &[Ins], tries: &[(usize, usize)]) -> alloc::vec::Vec<bool> {
    let mut t = alloc::vec![false; ins.len() + 1];
    for i in ins.iter().filter(|i| i.op == Jump || kinds(i) & (NONE | JUMPS) == JUMPS) { if let Some(e) = t.get_mut(i.a as usize) { *e = true; } }
    for &(_, h) in tries { if let Some(e) = t.get_mut(h) { *e = true; } }
    t
}

/* a bit per parameter the recursive call at `j` passes on as it is */
pub(crate) fn passed(ins: &[Ins], j: usize, arity: usize) -> u64 {
    let c = ins[j];
    let rest = if super::lower::wide(c) { ins[j + 1] } else { c };
    let n = (c.x as usize + (c.op == CallMethodR) as usize).min(5).min(arity).min(64);
    [c.b, c.c, rest.a, rest.b, rest.c].iter().enumerate().take(n).filter(|&(q, &t)| t as usize == q).fold(0, |m, (q, _)| m | 1 << q)
}

fn writes(i: &Ins, r: u16) -> bool {
    match i.op {
        UnpackR | OpR | RowsR if let Some((d, _)) = rows(i) => d.iter().any(|d| d.contains(&r)),
        ForIterR if i.x == 0 => i.b == r,
        _ => kinds(i) & (NONE | DEST) == DEST && i.a == r,
    }
}

pub(crate) fn field(i: &Ins, f: u8) -> u16 { match f { 0 => i.a, 1 => i.b, _ => i.c } }

// the rows an unpacking an OpR or a build writes and the row it reads
pub(crate) fn rows(i: &Ins) -> Option<([core::ops::Range<u16>; 2], core::ops::Range<u16>)> {
    let one = |r: u16| r..r.saturating_add(1);
    let (pops, pushes) = match i.op.rows() {
        // a pair unpacked into two named registers
        Rows::Unpack if i.x != 0 => return Some(([one(i.a), one(i.b)], one(i.c))),
        Rows::Unpack => (1, i.c),
        Rows::Op => super::lower::op_rows(*crate::code::OpCode::ALL.get(i.x as usize)?, i.c)?,
        Rows::Items => (i.c, 1),
        // a row op takes its operands from `a` and leaves what it gives there
        Rows::Row => { let (pops, pushes) = super::lower::row_op_counts(i); return Some(([i.a..i.a.saturating_add(pushes), 0..0], i.a..i.a.saturating_add(pops))) }
        Rows::None => return None,
    };
    Some(([i.a..i.a.saturating_add(pushes), 0..0], i.b..i.b.saturating_add(pops)))
}

pub(crate) fn field_mut(i: &mut Ins, f: u8) -> &mut u16 { match f { 0 => &mut i.a, 1 => &mut i.b, _ => &mut i.c } }

