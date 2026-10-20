use super::VM;
use super::lower::{self, Code, Ins, Section, HEADER, LINK, BASED};
use super::types::Val;
use crate::parser::OpCode;

// Where how far the stack had grown in a level as it began keeping, its low byte, lies in the link it keeps.
const GROWN: u32 = 32;

/* The link a level keeps, its `link` and how far the stack, `len` long as it began keeping, grew since `begun`, read for a BASED word from the register before the link. */
#[inline(always)]
fn kept_link(link: Val, w: Ins, len: usize, begun: Val) -> Val {
    let grown = if w.x & BASED != 0 { (len as u64).wrapping_sub(begun.0) & 0xFF } else { 0 };
    Val(link.0 | grown << GROWN)
}

/* The link and how far the stack grew in the level, from the link it kept. */
#[inline(always)]
pub(crate) fn split_link(kept: Val) -> (Val, usize) { (Val(kept.0 & !(0xFF << GROWN)), (kept.0 >> GROWN & 0xFF) as usize) }

/* Where a level whose values start at `start` began, from how far the stack grew in it. */
#[inline(always)]
pub(crate) fn begun_at(start: usize, grown: usize) -> Val { Val::int(start.wrapping_sub(grown) as i64) }

/* The UnturnR words of the level the head at `h` of `code` keeps (those its TurnR names past its parameter moves, or a relaid head's original's), None for a call that keeps none. */
pub(crate) fn level_words(code: &Code, h: usize) -> Option<&[Ins]> {
    let ins = &code.ins;
    match ins.get(..h)?.iter().rev().find(|i| i.op != OpCode::Move) {
        Some(t) if t.op == OpCode::TurnR => ins.get(t.a as usize..t.a as usize + t.b as usize),
        _ => {
            let back = ins.get(h + HEADER as usize).filter(|j| j.op == OpCode::JumpR && (j.a as usize) < h)?;
            level_words(code, (back.a as usize).checked_sub(HEADER as usize)?)
        }
    }
}

/* The level the one that raised at `rip` of `code` kept for a call still pending, that of the head whose stub, a few instructions long, made the call. */
fn pending_level(code: &Code, rip: usize) -> Option<&[Ins]> {
    let ins = &code.ins;
    let j = ins.get(rip..ins.len().min(rip + 32))?.iter().find(|i| matches!(i.op, OpCode::Jump) && i.x == 1)?;
    let h = (j.a as usize).checked_sub(HEADER as usize)?;
    let laid = ins.get(h).is_some_and(|i| lower::section_call(i.op) || i.op == OpCode::DunderSectionR);
    if !laid || !ins.get(h + 3).is_some_and(|t| t.x == lower::STUB && t.a as usize <= rip) { return None; }
    level_words(code, h)
}

/* How many values the level of `words` keeps. */
fn kept(words: &[Ins]) -> usize { words.iter().map(|w| if w.x & LINK != 0 { 1 + (w.x & 3) as usize } else { w.x as usize }).sum() }

/* Where a level's stack began, `base` as its section's register before the link holds it, or `or` for one no level wrote. */
fn base_or(base: Val, or: usize) -> usize {
    if base.is_int() && base.as_int() >= 0 && base.as_int() as usize <= or { base.as_int() as usize } else { or }
}

/* The TurnR of `words` on frame `regs`, keeping on `stack` a level of at least `n` values before a call of the section whose register `callee` notes where its level begins. */
#[inline(always)]
pub(crate) fn turn(regs: &mut [Val], stack: &mut alloc::vec::Vec<Val>, words: &[Ins], n: usize, callee: u16) {
    // SAFETY every register a word names lies inside the frame, as `lower::reg_bound` keeps it, and the level keeps no more than its values, the room reserved first.
    macro_rules! reg { ($r:expr) => {{ let r = $r as usize; debug_assert!(r < regs.len()); unsafe { *regs.get_unchecked(r) } }}; }
    stack.reserve(n);
    let (top, len) = (stack.as_mut_ptr(), stack.len());
    let mut at = len;
    macro_rules! push { ($v:expr) => {{ let v = $v; unsafe { top.add(at).write(v); } at += 1; }}; }
    for w in words[1..].iter().rev() {
        push!(reg!(w.a));
        if w.x > 1 { push!(reg!(w.b)); }
        if w.x > 2 { push!(reg!(w.c)); }
    }
    let w = words[0];
    if w.x & 3 != 0 { push!(reg!(w.b)); }
    if w.x & 3 == 2 { push!(reg!(w.c)); }
    push!(kept_link(reg!(w.a), w, len, reg!(w.a - 1)));
    unsafe { stack.set_len(at); }
    // The section the call enters begins its level where this one ends.
    if let Some(r) = regs.get_mut(callee as usize) { *r = Val::int(at as i64); }
}

/* Takes the level of the UnturnR words from the first of `words`, its values below `sp` of `stack`, back into frame `regs`, giving where its values start, the link and its word count. */
#[inline(always)]
pub(crate) fn take_level(regs: &mut [Val], stack: &[Val], words: &[Ins], sp: usize) -> (usize, Val, usize) {
    // SAFETY as for `turn`.
    macro_rules! put { ($r:expr, $v:expr) => {{ let (r, v) = ($r as usize, $v); debug_assert!(r < regs.len()); unsafe { *regs.get_unchecked_mut(r) = v; } }}; }
    let mut at = sp;
    macro_rules! take { () => {{ at = at.wrapping_sub(1); stack.get(at).copied().unwrap_or_else(Val::undef) }}; }
    // The first word names the link, kept last with where the level began.
    let w = words[0];
    let (link, grown) = split_link(take!());
    put!(w.a, link);
    if w.x & 3 == 2 { put!(w.c, take!()); }
    if w.x & 3 != 0 { put!(w.b, take!()); }
    let mut n = 1;
    for w in &words[1..] {
        if w.op != OpCode::UnturnR || w.x & LINK != 0 { break; }
        n += 1;
        if w.x > 2 { put!(w.c, take!()); }
        if w.x > 1 { put!(w.b, take!()); }
        put!(w.a, take!());
    }
    let at = at.min(sp);
    if words[0].x & BASED != 0 { put!(words[0].a - 1, begun_at(at, grown)); }
    (at, link, n)
}

/* The head a walk met last by where its call carries on, with the section it lies in and its level's words and how many values they keep. */
type Seen<'c> = (usize, usize, Option<&'c Section>, Option<(&'c [Ins], usize)>);

impl VM<'_> {
    /* The TurnR with `x`, `words` and callee register `callee` on the running frame, false past the recursion limit (a level counting as a frame) or the memory limit. */
    pub(crate) fn turn(&mut self, words: &[Ins], x: u8, callee: u16) -> bool {
        let Some(left) = super::act::RECURSION_LIMIT.checked_sub(self.acts + self.turned + 2).filter(|&l| l > 0 && !self.heap.over()) else { return false };
        let len = self.stack.len();
        // The loop keeps levels up to the limit, as far as frames look at memory, and within the room the stack holds now.
        self.stack.reserve(8);
        let (low, room) = (len.saturating_sub(super::act::DEPTH_CHECK), self.stack.capacity() - len - 3);
        (self.turn_low, self.turn_span, self.turn_acts) = (low, len - low + left.min(super::act::DEPTH_CHECK).min(room), self.acts);
        turn(&mut self.regs[self.base..], &mut self.stack, words, (x & lower::COUNT) as usize, callee);
        self.turned += 1;
        true
    }

    /* The UnturnR words from the first of `words` on the running frame, taking the level back off the stack and giving how many words there were. */
    pub(crate) fn unturn(&mut self, words: &[Ins]) -> usize {
        let (start, _, n) = take_level(&mut self.regs[self.base..], &self.stack, words, self.stack.len());
        self.stack.truncate(start);
        self.turned = self.turned.saturating_sub(1);
        n
    }

    /* Walks the levels a raise at `rip` leaves, innermost first, `each` getting each one's section, head, outer section and stack, levels at or above `floor` written back when `write`. */
    pub(crate) fn walk_levels(&mut self, code: &Code, rip: usize, write: bool, floor: usize, mut each: impl FnMut(&mut Self, &Section, usize, Option<&Section>, usize) -> bool) {
        let Some(block) = code.block.as_deref() else { return };
        let moved = block.moved;
        let held = |at: usize| block.sections.partition_point(|s| s.start as usize <= at).checked_sub(1).and_then(|k| block.sections.get(k)).filter(|s| at < s.end as usize)
            .or_else(|| if moved { block.moved_holding(at) } else { None });
        let Some(mut s) = held(rip) else { return };
        let reg = |vm: &Self, r: u16| vm.regs.get(vm.base + r as usize).copied().unwrap_or_else(Val::undef);
        let begun = |vm: &Self, s: &Section| if s.levels { base_or(reg(vm, s.link - 1), vm.stack.len()) } else { vm.stack.len() };
        let (mut sp, mut link) = (begun(self, s), reg(self, s.link));
        let level = |vm: &mut Self, words: &[Ins], n: usize, sp: usize| -> Option<(usize, usize, Val)> {
            let top = vm.stack.get(sp.checked_sub(1)?).copied()?;
            let start = sp.checked_sub(n).filter(|&b| b >= floor)?;
            let (link, grown) = split_link(top);
            let begun = base_or(begun_at(start, grown), start);
            if !write { return Some((begun, start, link)); }
            let base = vm.base;
            let (_, l, _) = take_level(&mut vm.regs[base..], &vm.stack, words, sp);
            Some((begun, start, l))
        };
        let mut low = sp;
        if let Some(words) = pending_level(code, rip) {
            let Some((b, l0, l)) = level(self, words, kept(words), sp) else { return };
            (sp, low, link) = (b, l0, l);
        }
        // The loop is bounded as each level is met once, and a recursion's head, section and words are looked up once as it comes back to one call.
        let mut seen: Option<Seen> = None;
        for _ in 0..self.stack.len() + block.sections.len() + 1 {
            if !link.is_int() { return; }
            let back = (link.as_int() as usize >> 16) & 0xFFFF;
            let (site, outer, words) = match seen {
                Some((b, site, outer, words)) if b == back => (site, outer, words),
                _ => {
                    let Some(site) = super::dispatch::head_of(code, back) else { return };
                    let (outer, words) = (held(site), level_words(code, site).map(|w| (w, kept(w))));
                    seen = Some((back, site, outer, words));
                    (site, outer, words)
                }
            };
            if !each(self, s, site, outer, if outer.is_some() { sp } else { low }) { return; }
            let Some(outer) = outer else { return };
            match words {
                Some((words, n)) => { let Some((b, l0, l)) = level(self, words, n, sp) else { return }; (sp, low, link) = (b, l0, l); }
                None => { (sp, link) = (begun(self, outer), reg(self, outer.link)); low = sp; }
            }
            s = outer;
        }
    }
}
