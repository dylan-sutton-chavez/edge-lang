use super::VM;
use super::lower;
use super::lower::{Code, Section, HEADER, LINK};
use crate::code::Ins;
use crate::value::Val;
use crate::code::OpCode;
use crate::r#unsafe::{Room, put, reg};

/* the UnturnR words the head at `h` keeps falling back to a relaid head's original */
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

/* the level kept for a call pending when its head's short stub raised at `rip` */
pub(crate) fn pending_level(code: &Code, rip: usize) -> Option<&[Ins]> {
    let ins = &code.ins;
    let j = ins.get(rip..ins.len().min(rip + 32))?.iter().find(|i| matches!(i.op, OpCode::Jump) && i.x == 1)?;
    let h = (j.a as usize).checked_sub(HEADER as usize)?;
    let laid = ins.get(h).is_some_and(|i| lower::section_call(i.op));
    if !laid || !ins.get(h + 3).is_some_and(|t| t.x == lower::STUB && t.a as usize <= rip) { return None; }
    level_words(code, h)
}

/* how many values the level of `words` keeps */
pub(crate) fn kept(words: &[Ins]) -> usize { words.iter().map(|w| if w.x & LINK != 0 { 1 + (w.x & 3) as usize } else { w.x as usize }).sum() }

/* the TurnR of `words` on `regs` keeping its level on `levels` before the section call */
#[inline(always)]
pub(crate) fn turn(regs: &[Val], levels: &mut alloc::vec::Vec<Val>, words: &[Ins]) {
    // registers lie inside the frame by `reg_bound` and each word keeps at most three
    macro_rules! reg { ($r:expr) => { reg(regs, $r as usize) }; }
    let mut room = Room::reserve(levels, 3 * words.len());
    macro_rules! push { ($v:expr) => { room.push($v) }; }
    for w in words[1..].iter().rev() {
        push!(reg!(w.a));
        if w.x > 1 { push!(reg!(w.b)); }
        if w.x > 2 { push!(reg!(w.c)); }
    }
    let w = words[0];
    if w.x & 3 != 0 { push!(reg!(w.b)); }
    if w.x & 3 == 2 { push!(reg!(w.c)); }
    push!(reg!(w.a));
    room.close();
    // the store stays here so the loop's lone level writes its own length unmerged
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/* takes the UnturnR level below `sp` into `regs` giving its start link and word count */
#[inline(always)]
pub(crate) fn take_level(regs: &mut [Val], levels: &[Val], words: &[Ins], sp: usize) -> (usize, Val, usize) {
    // as for `turn`
    macro_rules! put { ($r:expr, $v:expr) => {{ let (r, v) = ($r as usize, $v); put(regs, r, v) }}; }
    let mut at = sp;
    macro_rules! take { () => {{ at = at.wrapping_sub(1); levels.get(at).copied().unwrap_or_else(Val::undef) }}; }
    // the first word names the link kept last
    let w = words[0];
    let link = take!();
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
    (at.min(sp), link, n)
}

/* the walk's last head by where its call carries on with its section and level */
type Seen<'c> = (usize, usize, Option<&'c Section>, Option<(&'c [Ins], usize)>);

impl VM<'_> {
    /* the TurnR of `words` on the running frame false past the recursion or memory limit */
    pub(crate) fn turn(&mut self, words: &[Ins]) -> bool {
        let Some(left) = self.deepest.checked_sub(self.acts + self.turned).filter(|&l| l > 0 && !self.heap.over()) else { return false };
        let len = self.levels.len();
        // the span ends at the depth limit or memory check or the room held now
        self.levels.reserve(8);
        let (low, room) = (len.saturating_sub(super::act::DEPTH_CHECK), self.levels.capacity() - len - 3);
        (self.turn_low, self.turn_span, self.turn_acts) = (low, len - low + left.min(super::act::DEPTH_CHECK).min(room), self.acts);
        turn(&self.regs[self.base..], &mut self.levels, words);
        self.turned += 1;
        true
    }

    /* takes the running frame's level off the levels giving how many UnturnR words there were */
    pub(crate) fn unturn(&mut self, words: &[Ins]) -> usize {
        let (start, _, n) = take_level(&mut self.regs[self.base..], &self.levels, words, self.levels.len());
        self.levels.truncate(start);
        self.turned = self.turned.saturating_sub(1);
        n
    }

    /* the UnturnR words from the first of `words` the loop leaves for short levels */
    #[inline(never)]
    pub(crate) fn unturn_apart(&mut self, words: &[Ins]) -> usize {
        let (w, len) = (words[0], self.levels.len());
        // a lone level holds its link and two values at most on the levels' top
        if w.x & lower::LONE == 0 || len <= (w.x & 3) as usize { return self.unturn(words); }
        let mut sp = len - 1;
        self.regs[self.base + w.a as usize] = self.levels[sp];
        if w.x & 3 == 2 { sp -= 1; self.regs[self.base + w.c as usize] = self.levels[sp]; }
        if w.x & 3 != 0 { sp -= 1; self.regs[self.base + w.b as usize] = self.levels[sp]; }
        self.levels.truncate(sp);
        self.turned = self.turned.saturating_sub(1);
        1
    }

    /* walks a raise's levels at `rip` innermost first down to `floor` restoring them if `write` */
    pub(crate) fn walk_levels(&mut self, code: &Code, rip: usize, write: bool, floor: usize, mut each: impl FnMut(&mut Self, &Section, usize, Option<&Section>, usize) -> bool) {
        let Some(block) = code.block.as_deref() else { return };
        let moved = block.moved;
        let held = |at: usize| block.sections.partition_point(|s| s.start as usize <= at).checked_sub(1).and_then(|k| block.sections.get(k)).filter(|s| at < s.end as usize)
            .or_else(|| if moved { block.moved_holding(at) } else { None });
        let Some(mut s) = held(rip) else { return };
        let reg = |vm: &Self, r: u16| vm.regs.get(vm.base + r as usize).copied().unwrap_or_else(Val::undef);
        let (mut sp, mut link) = (self.levels.len(), reg(self, s.link));
        let level = |vm: &mut Self, words: &[Ins], n: usize, sp: usize| -> Option<(usize, usize, Val)> {
            let link = vm.levels.get(sp.checked_sub(1)?).copied()?;
            let start = sp.checked_sub(n).filter(|&b| b >= floor)?;
            if !write { return Some((start, start, link)); }
            let base = vm.base;
            let (_, l, _) = take_level(&mut vm.regs[base..], &vm.levels, words, sp);
            Some((start, start, l))
        };
        let mut low = sp;
        if let Some(words) = pending_level(code, rip) {
            let Some((b, l0, l)) = level(self, words, kept(words), sp) else { return };
            (sp, low, link) = (b, l0, l);
        }
        // bounded as each level is met once and `seen` spares a recursion's repeated lookups
        let mut seen: Option<Seen> = None;
        for _ in 0..self.levels.len() + block.sections.len() + 1 {
            if !link.is_int() { return; }
            let back = (link.as_int() as usize >> 16) & 0xFFFF;
            let (site, outer, words) = match seen {
                Some((b, site, outer, words)) if b == back => (site, outer, words),
                _ => {
                    let Some(site) = crate::vm::sections::head_of(code, back) else { return };
                    let (outer, words) = (held(site), level_words(code, site).map(|w| (w, kept(w))));
                    seen = Some((back, site, outer, words));
                    (site, outer, words)
                }
            };
            if !each(self, s, site, outer, if outer.is_some() { sp } else { low }) { return; }
            let Some(outer) = outer else { return };
            match words {
                Some((words, n)) => { let Some((b, l0, l)) = level(self, words, n, sp) else { return }; (sp, low, link) = (b, l0, l); }
                // a call that keeps no level leaves the levels below for the sections outside it
                None => { link = reg(self, outer.link); low = sp; }
            }
            s = outer;
        }
    }
}
