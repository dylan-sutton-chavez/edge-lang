use super::*;

/* what a block keeps for its growths */
#[derive(Clone, Default)]
pub(crate) struct Grown {
    /* the instructions and assumptions it was lowered from which a growth lowers from again */
    pub src: Option<Option<Rc<Vec<Instruction>>>>,
    pub facts: Option<Rc<Facts>>,
    /* where each head laid past the end lies by its return point past the call */
    pub displaced: Vec<(u32, u32)>,
    /* each head and function met a growth was asked for so none is asked twice */
    pub tried: core::cell::RefCell<Vec<(u32, u64)>>,
    /* calls turned down for functions in no table and past `TURNS` each asks to grow */
    pub turned: Cell<u32>,
    /* head and function of the last turned-down call noted once while the same repeat */
    pub last: Cell<(u32, u64)>,
    /* heads and stubs laid past the end for section ops by span and op place */
    pub moved: Vec<(u32, u32, u32)>,
    /* stub calls turned down before `TURNS` and each function met which the first ask notes */
    pub pending: core::cell::RefCell<Vec<(u32, Val)>>,
    /* last calls and functions refused for good which turn down at once when met again */
    pub refused: Cell<[(u32, u64); 4]>,
    pub refused_at: Cell<u8>,
}

impl Grown {

    /* whether the call at `i` meeting `f` was turned down for good */
    pub(crate) fn refuses(&self, i: u32, f: u64) -> bool { self.refused.get().contains(&(i, f)) }

    /* notes the call at `i` meeting `f` as turned down for good */
    pub(crate) fn refuse(&self, i: u32, f: u64) {
        let (mut r, at) = (self.refused.get(), self.refused_at.get());
        r[at as usize & 3] = (i, f);
        self.refused.set(r);
        self.refused_at.set(at.wrapping_add(1));
    }
}

/* the places and the rows under each resume entry `packed` put together */
fn unpacked(at: &[u32]) -> (Vec<u32>, Vec<u16>) {
    (at.iter().map(|a| a & 0xFFFF).collect(), if at.iter().any(|a| a >> 16 != 0) { at.iter().map(|a| (a >> 16) as u16).collect() } else { Vec::new() })
}

/* what a turned-down head grows for such as a met function or method or rebinding */
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Grow { Value, Method, Global, Site, All, Chain }

impl Grow {

    /* every kind by its index */
    pub(crate) const ALL: [Grow; 6] = [Grow::Value, Grow::Method, Grow::Global, Grow::Site, Grow::All, Grow::Chain];
}

/* compiler instruction the head at `at` was laid for and its enclosing section's function */
pub(crate) fn head_origin(code: &Code, at: usize) -> (Option<usize>, u32) {
    match code.block.as_deref().and_then(|b| b.holding(at)) {
        Some((s, at)) => (Some(s.fi), s.orig[at - s.start as usize]),
        None => (None, code.orig(at)),
    }
}

/* whether the head at `at` is a keyword call's whose stub calls off the stack */
pub(crate) fn kw_head(ins: &[Ins], at: usize) -> bool {
    let stub = ins[at + 3].a as usize;
    ins.get(stub..).into_iter().flatten().take(32).take_while(|j| !(j.op == OpCode::Jump && j.x == 1 && j.a as usize == at + HEADER as usize))
        .any(|j| j.op == OpCode::Call && j.a >> 8 != 0 || j.op == OpCode::RowsR && j.x == OpCode::Call as u8 && j.b >> 8 != 0)
}

/* whether `h` is a binding section or constructor head that a growth may move */
fn global_head(ins: &[Ins], h: usize) -> bool {
    matches!(ins[h].op, OpCode::CallGlobalSectionR | OpCode::CallSectionR | OpCode::CallCtorSectionR)
        && ins.get(h + 2).is_some_and(|s| s.op == OpCode::SectionR && s.x & (8 | 32 | 64) == 0 && s.x & (4 | 16) != 4)
        && ins.get(h + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == STUB)
}

/* counts head `nh` laid from `from` and its stub as head `h`'s place */
fn relaid(block: &mut Block, out: &[Ins], from: usize, nh: usize, h: usize) {
    // a head in no section keeps the code's own chunk and module
    let Some(place) = block.holding(h).map(|(_, at)| at) else { return };
    let end = (nh + HEADER as usize..out.len()).find(|&j| out[j].op == OpCode::JumpR).map_or(nh + HEADER as usize, |j| j + 1);
    let st = out[nh + 3].a as usize;
    let back = (st..out.len()).find(|&j| out[j].op == OpCode::Jump && out[j].x == 1 && out[j].a as usize == nh + HEADER as usize);
    let _ = block.grown();
    if let Some(g) = block.grown.get_mut() {
        g.moved.push((from as u32, end as u32, place as u32));
        if let Some(e) = back { g.moved.push((st as u32, e as u32 + 1, place as u32)); }
    }
    block.moved = true;
}

/* grows `old` past its end with the sections head `at` asks for so nothing moves */
#[allow(clippy::too_many_arguments)]
pub(crate) fn extend<'p>(old: &Code, src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>, at: usize, kind: Grow, func: Val) -> Option<Code> {
    if kind == Grow::All { return extend_all(old, src, chunk, facts, sections, at != 0); }
    let block = old.block.as_deref()?;
    if block.callees.len() != block.sections.len() || old.unchanged() { return None; }
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = old.ins.len();
    let (mut orig, mut back) = (old.orig.clone(), old.back.clone());
    orig.pop();
    back.pop();
    let wanted: Vec<(u16, Callee<'p>)> = block.callees.clone();
    // each head's section paired with every section its table holds
    let mut alts = Vec::new();
    for (k, s) in block.sections.iter().enumerate().filter(|s| s.1.table.1 != u32::MAX) {
        let (start, mask) = (s.table.0 as usize, s.table.1 as usize);
        alts.extend(block.alt_tab[start..=start + mask].iter().filter(|e| e.1 != u32::MAX).map(|e| (k, (e.1 & !DEFAULTS) as usize)));
    }
    let (places, entry) = unpacked(&old.at);
    let mut l = Lower {
        src, chunk, facts, flags: &[], deleted: Vec::new(), names, cbase: old.cbase as usize, consts, tbase: old.cbase as usize + consts + 3,
        out: old.ins.clone(), orig, at: places, back,
        pending: Vec::new(), patches: Vec::new(), temps: 0, last: None, cur: 0, quiet: 0, bound: vec![0; names.div_ceil(64)],
        appends: Vec::new(), lists: core::cell::OnceCell::new(), sections, wanted, heads: Vec::new(), prep: Vec::new(), seeds: Vec::new(), dest: None, resume: None,
        gaps: 0, kwcall: None, kwstubs: Vec::new(), stub_row: (0, 0), kwrow: None, alts_first: alts.len(), alts, lookups: Vec::new(), chain_heads: Vec::new(), yields: Vec::new(),
        genfors: Vec::new(), gen_next: None, gen_args: Vec::new(), ret_to: None, nested: Vec::new(), gen_loops: Vec::new(), gen_steps: Vec::new(),
        keyed: false, more_args: Vec::new(), wide: false, once: false, old: Some(block), first: block.callees.len(), only: Some(func), calls: OnceCell::new(), displaced: Vec::new(), tern: None,
        s0: old.slots as usize, rows: (old.cbase as usize).saturating_sub(old.slots as usize), overflow: false, entry, pops: 0, resumes: Vec::new(), prefer: None, spread_depths: None, refused: false,
    };
    let (scope, cat) = head_origin(old, at);
    l.cur = cat;
    let k = l.out[at + 2].c as usize;
    let call = l.out[at];
    // heads moved to a binding's new section by where each lies now and that section
    let mut moved: Vec<(usize, usize, usize)> = Vec::new();
    let mut detach: Option<usize> = None;
    let mut swapped: Option<usize> = None;
    // the section holding a growing op or call and where its new head starts
    let mut enclosing: Option<usize> = None;
    let mut laid_from: Option<usize> = None;
    // each head laid again by its start and the place it stands for
    let mut relays: Vec<(usize, usize)> = Vec::new();
    // the asking head's place for sections or the section op a displaced head stands for
    let lies = block.origin(at);
    match kind {
        // a keyword call's head orders arguments by its function's parameters which no other section takes
        Grow::Method if kw_head(&l.out, at) => return None,
        // a keyword head through a register is laid again for the function met
        Grow::Value if kw_head(&l.out, at) => {
            // a class met lays its constructor whose head checks the class and makes the instance
            let mut c = (l.sections)(Want::Func(func, false))?;
            (c.twin, c.by_body) = (false, false);
            relays.push((l.out.len(), lies));
            if !l.rekeyed(at, c, u16::MAX) { return None; }
        }
        // a spreading head lays another function met and a generator loop head steps another generator
        Grow::Value if l.out[at + 2].x & 72 == 72 => {
            let c = (l.sections)(Want::Func(func, false)).filter(|c| spreads(c, call.x) && c.ctor().is_none() && !c.by_body)?;
            let arity = c.arity;
            l.keyed = true;
            let to = l.want_callee(u16::MAX, c, arity)?;
            if to == k || l.alts.contains(&(k, to)) { return None; }
            l.alts.push((k, to));
        }
        Grow::Value if let Some(&(_, exit, _, _)) = block.loops.iter().find(|g| g.3 as usize == at) => {
            let n = call.x as usize;
            let c = (l.sections)(Want::GenFunc(func)).filter(|c| c.star.is_none() && c.dstar.is_none() && n >= c.required && n <= c.arity && crate::vm::block::gen_pure(&c.body.ins))?;
            let step = block.sections[k].step_reg;
            // the loop binds its variable through the head's store or a module binding past it
            let target = (l.out.get(at + HEADER as usize).map(|i| i.op) != Some(OpCode::StoreGlobalR)).then_some(l.out[at + 1].a);
            let k2 = l.wanted.len();
            l.gen_loops.push((k2, exit as u16, target, at as u16 + HEADER, true));
            l.gen_steps.push((k2, step));
            let to = l.want_callee(u16::MAX, c, n)?;
            if to != k2 { return None; }
            l.alts.push((k, to));
        }
        Grow::Value => {
            // it lays every function its head met which its asks noted
            l.only = None;
            l.want_alts(scope, cat, k, call.x as usize);
            if l.alts.len() == l.alts_first { return None; }
            // a head whose function stopped coming adopts an equal-arity met one tabling the old
            let met = l.alts[l.alts_first..].iter().position(|a| a.0 == k && l.wanted[a.1].1.value.0 == func.0).map(|p| l.alts[l.alts_first + p].1);
            let own = &l.wanted[k].1;
            let fresh = |c: &Callee| c.arity == call.x as usize && !c.by_body;
            if let Some(to) = met && call.op == OpCode::CallValueSectionR && call.x <= 2 && fresh(own) && fresh(&l.wanted[to].1) {
                let others: Vec<usize> = l.alts.iter().filter(|a| a.0 == k && a.1 != to).map(|a| a.1).collect();
                for a in others.into_iter().chain(core::iter::once(k)) { if !l.alts.contains(&(to, a)) { l.alts.push((to, a)); } }
                l.out[at + 2].c = to as u16;
                swapped = Some(to);
            }
            // a head whose stub looks up no table checks the table first
            let stub = l.out[at + 3].a as usize;
            let looks = l.out.get(stub).is_some_and(|i| i.op == OpCode::CallValueR && i.a as usize == at);
            if call.op != OpCode::CallValueSectionR && !looks {
                let from = l.out.len();
                l.lookups.push((l.out.len(), k));
                l.emit(Ins { op: OpCode::CallValueR, x: 0, a: at as u16, b: 0, c: 0 });
                l.emit(Ins { op: OpCode::Jump, x: 1, a: stub as u16, b: 0, c: 0 });
                l.out[at + 3].a = from as u16;
            }
        }
        // a head with no table checks one past the end for the function met
        Grow::Chain => {
            let (sect, stub) = (l.out[at + 2], l.out[at + 3]);
            let (s, method) = (&block.sections[k], sect.x & 8 != 0);
            let n = call.x as usize;
            if kw_head(&l.out, at) || sect.x & (16 | 32 | 64) != 0 || n > 4 || !(method && s.on_class == 1) { return None; }
            // a call inside a section lays its head past the end for the section's walks
            enclosing = block.holding(at).map(|(s, _)| block.sections.iter().position(|t| core::ptr::eq(t, s)).unwrap_or(usize::MAX));
            // arguments the head reads or the preceding moves wrote to its section
            let param = |l: &Lower, p: usize| (at.saturating_sub(16)..at).rev().find(|&j| l.out[j].op == OpCode::Move && l.out[j].a == s.link + 1 + p as u16).map(|j| l.out[j].b);
            (l.cur, l.quiet) = (u32::from(l.orig[at]), u32::from(l.back[at]));
            let from = l.out.len();
            laid_from = Some(from);
            let mut args = [0u16; 4];
            for (i, a) in args.iter_mut().enumerate().take(n) {
                *a = if i == 0 { call.c } else { param(&l, i)? };
            }
            let c = [FIELD, STATIC].iter().flat_map(|&m| (0..ALTS).map(move |j| (m, j))).find_map(|(m, j)| (l.sections)(Want::MethodAlt(scope, cat | m, j)).filter(|c| c.value.0 == func.0))?;
            if c.star.is_some() || c.dstar.is_some() || c.by_body { return None; }
            let stat = matches!(c.on_class, 1 | 4);
            let to = l.want_callee(u16::MAX, c, n + !stat as usize).filter(|&to| to != k)?;
            // its head reads its first parameters from the call as a default writes the stub's
            if !stat && n + 1 < l.wanted[to].1.arity.min(2) || enclosing.is_some_and(|e| l.reaches(to, e)) { return None; }
            l.head(Ins { op: OpCode::CallMethodR, x: n as u8, a: call.a, b: call.b, c: args[0] }, Ins { op: OpCode::ArgsR, x: 0, a: args[1], b: args[2], c: args[3] }, l.out[at + 1].a, to, true, &[]);
            // laid beside the turned-down head it goes to that head's stub in turn
            let nh = l.out.len() - HEADER as usize;
            l.heads.pop();
            l.chain_heads.push((nh, to));
            l.out[nh + 3] = stub;
            l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + HEADER, b: 0, c: 0 });
            l.out[at + 3].a = from as u16;
        }
        Grow::Method => {
            l.only = None;
            l.want_method_alts(scope, cat, k, call.x as usize + 1);
            if l.alts.len() == l.alts_first { return None; }
            // a receiver holding the met function calls it before its section and table like lowering
            let held = l.alts[l.alts_first..].iter().find(|a| a.0 == k && l.wanted[a.1].1.on_class == 4 && l.wanted[a.1].1.value.0 == func.0).map(|a| a.1);
            if let Some(to) = held && l.wanted[k].1.on_class == 0 && matches!(call.op, OpCode::CallMethodSectionR | OpCode::CallMethodGlobalSectionR) && call.x <= 1 {
                let others: Vec<usize> = l.alts.iter().filter(|a| a.0 == k && a.1 != to).map(|a| a.1).collect();
                for a in others.into_iter().chain(core::iter::once(k)) { if !l.alts.contains(&(to, a)) { l.alts.push((to, a)); } }
                // the old head jumps to a new one past the end filling that function's parameters
                (l.cur, l.quiet) = (u32::from(l.orig[at]), u32::from(l.back[at]));
                let from = l.out.len();
                // a receiver the head read from a module binding is read into its register first
                if call.op == OpCode::CallMethodGlobalSectionR { let g = l.out[at + 3].c - 1; l.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: call.b, b: g, c: 0 }); }
                l.head(Ins { op: OpCode::CallMethodR, ..call }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, l.out[at + 1].a, to, true, &[]);
                l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + HEADER, b: 0, c: 0 });
                l.displaced_to(at, from);
                relays.push((from, lies));
            }
        }
        Grow::Global => {
            if !global_head(&l.out, at) { return None; }
            let s = &block.sections[k];
            let (g, module) = (s.global, s.bound);
            if g == u16::MAX || l.wanted[k].1.star.is_some() { return None; }
            let n = call.x as usize;
            l.wanted[k].0 = u16::MAX;
            // a constructor head lays its class with the init found now taking the instance first
            let ctor = s.ctor.is_some() as usize;
            let fresh = (l.sections)(Want::Global(module, g)).filter(|c| c.star.is_none() && c.dstar.is_none() && c.ctor().is_some() == (ctor == 1) && !c.by_body && !c.twin);
            let to = fresh.and_then(|c| l.want_callee(g, c, n + ctor));
            match to {
                // a remade closure or what a bound method or instance holds needs lowering again
                None if (l.sections)(Want::Global(module, g)).is_some_and(|c| c.twin) || (l.sections)(Want::Held(module, g)).is_some() => return None,
                None => detach = Some(k),
                Some(to) => {
                    let (arity, defaults) = (l.wanted[to].1.arity, l.wanted[to].1.defaults.clone());
                    let link = s.link as usize;
                    let default = |p: usize| defaults[p + defaults.len() - arity];
                    let heads: Vec<usize> = (0..len).filter(|&h| global_head(&l.out, h) && l.out[h + 2].c as usize == k).collect();
                    for h in heads {
                        // a keyword head is laid again for the new parameters or left to its stub
                        if kw_head(&l.out, h) { let (c, from) = (l.wanted[to].1.clone(), l.out.len()); if l.rekeyed(h, c, g) { relays.push((from, h)); } continue; }
                        let head = l.out[h];
                        let given = head.x as usize + ctor;
                        // arguments past the second that preceding moves wrote to the old section's parameters
                        let mut args = Vec::new();
                        for p in 2..arity.min(given) {
                            let m = (h.saturating_sub(16)..h).rev().find(|&j| l.out[j].op == OpCode::Move && l.out[j].a as usize == link + 1 + p);
                            args.push(l.out[m?].b);
                        }
                        let from = l.out.len();
                        l.cur = old.orig(h);
                        for p in 2..arity {
                            let at = l.out.len();
                            l.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: args.get(p - 2).copied().unwrap_or(0), c: 0 });
                            l.prep.push((at, 0, Prep::Param(to, p)));
                            if p >= given { l.prep.push((at, 1, Prep::Const(default(p)))); }
                        }
                        let at = l.out.len();
                        for f in 0..4 { let i = l.out[h + f]; l.emit(i); }
                        // a constructor head reads from `b` its first argument which fills the second parameter
                        for p in given..arity.min(2) { l.prep.push((at, (1 + p - ctor) as u8, Prep::Const(default(p)))); }
                        l.out[at + 2].c = to as u16;
                        // the section returns past the head as always then jumps on past the old head
                        l.out[at + 1].b = (at + HEADER as usize) as u16;
                        l.emit(Ins { op: OpCode::JumpR, x: 0, a: (h + HEADER as usize) as u16, b: 0, c: 0 });
                        moved.push((h, from, at));
                    }
                }
            }
        }
        // a call outside the sections no head lays gets one past the end
        Grow::Site => {
            if !block.plain_at(at) { return None; }
            enclosing = block.holding(at).map(|(s, _)| block.sections.iter().position(|t| core::ptr::eq(t, s)).unwrap_or(usize::MAX));
            // a call inside a section lays a binding's function only in the code's own module
            if let Some(e) = enclosing {
                let c = l.out[at];
                if block.sections.get(e).is_none_or(|s| s.module != l.facts.module) || !matches!(c.op, OpCode::CallGlobalR | OpCode::CallR) { return None; }
            }
            // a call that turned down several functions is hot so it lays what it meets
            laid_from = Some(l.out.len());
            if !l.displace(at, old) { return None; }
            // a call inside a section that may reenter it would need a separately laid recursion
            let start = laid_from.unwrap_or(0);
            let k = (start..l.out.len()).find(|&h| l.out.get(h + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB)).map(|h| l.out[h + 2].c as usize)?;
            if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
        }
        Grow::All => return None,
    }
    let (block, frame) = l.lay(old.frame);
    let mut block = block?;
    // an inner op's head and stub count as the op's place for the section's walks
    let mut laid_past = false;
    if enclosing.is_some() && let Some(g) = block.grown.get_mut() && let Some(from) = laid_from
        && let Some(h) = (from..l.out.len()).find(|&h| l.out.get(h + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB)) {
        let end = (h + HEADER as usize..l.out.len()).find(|&j| l.out[j].op == OpCode::JumpR).map_or(h + HEADER as usize, |j| j + 1);
        g.moved.push((from as u32, end as u32, lies as u32));
        laid_past = true;
        let st = l.out[h + 3].a as usize;
        if let Some(e) = (st..l.out.len()).find(|&j| l.out[j].op == OpCode::Jump && l.out[j].x == 1 && l.out[j].a as usize == h + HEADER as usize) { g.moved.push((st as u32, e as u32 + 1, lies as u32)); }
    }
    block.moved |= laid_past;
    if let Some(to) = swapped { (l.out[at + 2].a, l.out[at + 2].b) = (block.sections[to].start as u16, block.sections[to].link + 1); }
    for &(h, from, at) in &moved {
        let to = l.out[at + 2].c as usize;
        (l.out[at + 2].a, l.out[at + 2].b) = (block.sections[to].start as u16, block.sections[to].link + 1);
        l.out[h] = Ins { op: OpCode::JumpR, x: 0, a: from as u16, b: 0, c: 0 };
        relaid(&mut block, &l.out, from, at, h);
    }
    for &(from, place) in &relays {
        if let Some(h) = (from..l.out.len()).find(|&h| l.out.get(h + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB)) { relaid(&mut block, &l.out, from, h, place); }
    }
    if kind == Grow::Global {
        // the binding leaves the old section for the new one which the block checks
        let (g, m) = (block.sections[k].global, block.sections[k].bound);
        block.sections[k].global = u16::MAX;
        if let Some(&(_, _, at)) = moved.first() {
            let to = &mut block.sections[l.out[at + 2].c as usize];
            if to.bound != m || to.global != u16::MAX && to.global != g { return None; }
            to.global = g;
        }
        // old keyword heads go to their stubs and every head does when none is laid
        let heads: Vec<usize> = (0..len).filter(|&h| global_head(&l.out, h) && l.out[h + 2].c as usize == k && (detach.is_some() || kw_head(&l.out, h))).collect();
        for h in heads { l.out[h] = Ins { op: OpCode::Jump, x: 1, a: l.out[h + 3].a, b: 0, c: 0 }; }
    }
    specialize(&mut l.out, len);
    // the jump past the sections goes past the new ones too
    let over = (*old.at.last()? & 0xFFFF) as usize;
    if l.out.get(over).is_some_and(|i| i.op == OpCode::Jump && i.x == 0) { l.out[over].a = l.out.len() as u16; }
    for &k in &l.yields {
        let next = match l.out.get(k + 1) { Some(j) if j.op == OpCode::Jump && (j.a as usize) < l.out.len() => j.a as usize, _ => k + 1 };
        (l.out[k].b, l.out[k].c) = (l.back[next], next as u16);
    }
    if l.out.len() >= u16::MAX as usize { return None; }
    let frame = grown_frame(&l.out, len, frame, old);
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(src.len() as u16);
    l.back.push(src.len() as u16);
    let cache = old.cache.extended(l.out.len());
    for &(at, site) in l.seeds.iter().filter(|s| s.0 & SEEDED != 0) { cache.set_site(at & !SEEDED, site); }
    let plain = OnceCell::new();
    let places = packed(l.at, &l.entry);
    if let Some(p) = old.plain.get() { let _ = plain.set(p.clone()); }
    let code = Code { ins: super::seal(l.out, frame), orig: l.orig, at: places, back: l.back, frame, kinds: old.kinds.clone(), module: old.module, hot: Cell::new(0), cache, pure: Cell::new(2), block: Some(block), plain, layout: next_layout(), misses: Cell::new(old.misses.get()), read: Cell::new(old.read.get()), rows: 0, cbase: old.cbase, slots: old.slots, consts: crate::r#unsafe::Consts::NONE, #[cfg(debug_assertions)] depths: Vec::new() };
    Some(code)
}

/* grows sectionless `old` past its end by a section per site that learned its callee */
fn extend_all<'p>(old: &Code, src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>, keep: bool) -> Option<Code> {
    if old.block.is_some() || old.unchanged() || !facts.regs { return None; }
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = old.ins.len();
    let (mut orig, mut back) = (old.orig.clone(), old.back.clone());
    orig.pop();
    back.pop();
    let once = if facts.once && facts.shape & CALLS != 0 { runs_once(src) } else { Vec::new() };
    let (places, entry) = unpacked(&old.at);
    let mut l = Lower {
        src, chunk, facts, flags: &[], deleted: Vec::new(), names, cbase: old.cbase as usize, consts, tbase: old.cbase as usize + consts + 3,
        out: old.ins.clone(), orig, at: places, back,
        pending: Vec::new(), patches: Vec::new(), temps: 0, last: None, cur: 0, quiet: 0, bound: vec![0; names.div_ceil(64)],
        appends: Vec::new(), lists: core::cell::OnceCell::new(), sections, wanted: Vec::new(), heads: Vec::new(), prep: Vec::new(), seeds: Vec::new(), dest: None, resume: None,
        gaps: 0, kwcall: None, kwstubs: Vec::new(), stub_row: (0, 0), kwrow: None, alts: Vec::new(), lookups: Vec::new(), chain_heads: Vec::new(), yields: Vec::new(),
        genfors: Vec::new(), gen_next: None, gen_args: Vec::new(), ret_to: None, nested: Vec::new(), gen_loops: Vec::new(), gen_steps: Vec::new(),
        keyed: false, more_args: Vec::new(), wide: false, once: false, old: None, first: 0, alts_first: 0, only: None, calls: OnceCell::new(), displaced: Vec::new(), tern: None,
        s0: old.slots as usize, rows: (old.cbase as usize).saturating_sub(old.slots as usize), overflow: false, entry, pops: 0, resumes: Vec::new(), prefer: None, spread_depths: None, refused: false,
    };
    // the jump past the heads and sections sits where the code ended
    l.cur = old.orig(len.saturating_sub(1));
    l.emit(Ins { op: OpCode::Jump, x: 0, a: 0, b: 0, c: 0 });
    for j in 0..len {
        l.once = once.get(old.orig(j) as usize).copied().unwrap_or(false);
        l.displace(j, old);
    }
    // one its misses grew lays a sectionless block so its calls and ops grow heads
    if l.wanted.is_empty() && !keep { return None; }
    let (block, frame) = l.lay(old.frame);
    let mut block = block.or_else(|| keep.then(|| Box::new(Block { sections: Box::default(), extra: Box::default(), ranges: Box::new([(old.frame, 0)]), callees: Vec::new(), lineage: 0, grown: OnceCell::new(),
        binds: Cell::new(crate::vm::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed)), alt_tab: Box::default(), loops: Box::default(), moved: false, by_class: Box::default() })))?;
    block.lineage = old.layout;
    specialize(&mut l.out, len);
    l.out[len].a = l.out.len() as u16;
    for &k in &l.yields {
        let next = match l.out.get(k + 1) { Some(j) if j.op == OpCode::Jump && (j.a as usize) < l.out.len() => j.a as usize, _ => k + 1 };
        (l.out[k].b, l.out[k].c) = (l.back[next], next as u16);
    }
    if l.out.len() >= u16::MAX as usize { return None; }
    let frame = grown_frame(&l.out, len, frame, old);
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(src.len() as u16);
    l.back.push(src.len() as u16);
    let cache = old.cache.extended(l.out.len());
    for &(at, site) in l.seeds.iter().filter(|s| s.0 & SEEDED != 0) { cache.set_site(at & !SEEDED, site); }
    let places = packed(l.at, &l.entry);
    let code = Code { ins: super::seal(l.out, frame), orig: l.orig, at: places, back: l.back, frame, kinds: old.kinds.clone(), module: old.module, hot: Cell::new(0), cache, pure: Cell::new(2), block: Some(block), plain: OnceCell::new(), layout: next_layout(), misses: Cell::new(0), read: Cell::new(old.read.get()), rows: 0, cbase: old.cbase, slots: old.slots, consts: crate::r#unsafe::Consts::NONE, #[cfg(debug_assertions)] depths: Vec::new() };
    Some(code)
}

/* gives section calls and returns from `from` on their kind's instruction so none checks flags */
pub(super) fn specialize(out: &mut [Ins], from: usize) {
    for k in from..out.len() {
        let ins = out[k];
        match ins.op {
            // a positional binding section call keeping nothing takes the op checking only the binding
            OpCode::CallSectionR => {
                let (x, stub) = (out[k + 2].x, out[k + 3]);
                out[k].op = match x & (4 | 8 | 16 | 32 | 64) {
                    0 => OpCode::CallGlobalSectionR,
                    4 => OpCode::CallValueSectionR,
                    8 if stub.c != 0 => OpCode::CallMethodGlobalSectionR,
                    8 => OpCode::CallMethodSectionR,
                    16 => OpCode::CallCtorSectionR,
                    _ => continue,
                };
            }
            // a return inside `try` leaves through handlers while an always-bound value's return checks nothing
            OpCode::ReturnSectionR if ins.x != 0 => out[k].op = OpCode::ReturnTriedR,
            OpCode::ReturnSectionR if ins.c == 1 => out[k].op = OpCode::ReturnBoundR,
            _ => {}
        }
    }
}

impl<'p> Lower<'_, 'p> {

    /* call `j` jumps to a head laid past the end and returns past its store */
    fn displace(&mut self, j: usize, old: &Code) -> bool {
        let call = self.out[j];
        // a stack call over values a build left on the stack walks back no further
        if call.op == OpCode::Call && call.b != 0 { return false; }
        if call.op == OpCode::Call && call.a >> 8 != 0 || call.op == OpCode::RowsR && call.x == OpCode::Call as u8 && call.b >> 8 != 0 { return self.displace_kw(j, old); }
        if !matches!(call.op, OpCode::CallR | OpCode::CallGlobalR | OpCode::CallMethodR) { return false; }
        let w = wide(call) as usize;
        // only calls stored at once are laid and their heads keep pending values in registers
        let Some(&store) = self.out.get(j + 1 + w) else { return false };
        if store.op != OpCode::StoreTopR || store.c != 0 || store.b != 0 { return false; }
        let rest = if w == 1 { self.out[j + 1] } else { Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 } };
        if w == 1 && rest.op != OpCode::ArgsR { return false; }
        let mut p = j;
        while p > 0 && self.out[p - 1].op == OpCode::PushRegs && (1..=3).contains(&self.out[p - 1].x) { p -= 1; }
        let pushed: Vec<u16> = self.out[p..j].iter().flat_map(|g| [g.a, g.b, g.c].into_iter().take(g.x as usize)).collect();
        let back_at = j + 2 + w;
        let below: Option<Vec<Entry>> = pushed.iter().rev().enumerate().map(|(e, &r)| match self.out.get(back_at + e) {
            Some(t) if t.op == OpCode::PopTop => Some(Entry { r, name: true }),
            Some(t) if t.op == OpCode::StoreTopR && t.a == r && t.c == 0 => Some(Entry { r, name: false }),
            _ => None,
        }).collect();
        let Some(mut below) = below.filter(|b| b.len() <= MAX_UNDER) else { return false };
        below.reverse();
        let i = old.orig(j);
        (self.cur, self.quiet) = (i, old.back.get(j).map_or(i, |&b| u32::from(b)));
        let n = call.x as usize;
        let k = match call.op {
            OpCode::CallGlobalR => self.want(call.a, n),
            // a register call inside a section lays its met function which no code site notes
            OpCode::CallR if self.old.is_some_and(|b| b.body_at(j).is_some()) => crate::vm::block::global_of(&self.out, j, call).and_then(|g| self.want(g, n))
                .or_else(|| self.only.and_then(|f| (self.sections)(Want::Func(f, false))).and_then(|c| { let m = n + (c.on_class == 7) as usize; self.want_callee(u16::MAX, c, m) })),
            OpCode::CallR => crate::vm::block::global_of(&self.out, j, call).and_then(|g| self.want(g, n))
                .or_else(|| (self.sections)(Want::Value(i)).filter(|c| c.on_class & 3 != 3).and_then(|c| self.want_callee(u16::MAX, c, n)).inspect(|&k| self.want_alts(None, i, k, n)))
                // a function remade by its `def` each pass runs the one section its body shares
                .or_else(|| self.only.and_then(|f| (self.sections)(Want::BodyOf(f))).and_then(|c| self.want_callee(u16::MAX, c, n))),
            _ => (self.sections)(Want::Method(i)).and_then(|c| { let m = n + !matches!(c.on_class, 1 | 4) as usize; self.want_callee(u16::MAX, c, m) }).inspect(|&k| self.want_method_alts(None, i, k, n + 1)),
        };
        let Some(k) = k else { return false };
        // a head packing leftover positionals packs them its own way
        let w = &self.wanted[k].1;
        if w.star.is_some() || w.dstar.is_some() { return false; }
        // a bound method's head passes its receiver first as a method call does
        let (call, rest) = if w.on_class == 7 { called_instance(call, rest) } else { (call, rest) };
        let by_body = call.op == OpCode::CallGlobalR && (w.twin || w.by_body);
        let (from, ret) = (self.out.len(), (back_at + below.len()) as u16);
        // one body's functions bound in turn load into the section's first parameter the head checks
        let call = if by_body {
            self.prep.push((self.out.len(), 0, Prep::Param(k, 0)));
            self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: 0, b: call.a, c: 0 });
            Ins { op: OpCode::CallR, ..call }
        } else { call };
        // its section returns straight past the call and its stub past the head then onward
        self.ret_to = Some(ret);
        self.head(call, rest, store.a, k, true, &below);
        if by_body { self.prep.push((self.out.len() - HEADER as usize, 0, Prep::Param(k, 0))); }
        self.displaced.push((ret as u32, (self.out.len() - HEADER as usize) as u32));
        self.emit(Ins { op: OpCode::JumpR, x: 0, a: ret, b: 0, c: 0 });
        self.displaced_to(p, from);
        true
    }

    /* `at` and every jump to it now jump to `from` past the end */
    pub(super) fn displaced_to(&mut self, at: usize, from: usize) {
        use OpCode::*;
        self.out[at] = Ins { op: JumpR, x: 0, a: from as u16, b: 0, c: 0 };
        // a stub's way back keeps naming the head it lies past
        for i in self.out.iter_mut().filter(|i| i.a as usize == at && !(i.op == Jump && i.x == 1) && matches!(i.op, Jump | JumpR | JumpIfFalse | JumpIfFalseR | JumpIfFalseOrPop | JumpIfTrueOrPop
            | JumpUnlessEq | JumpUnlessNotEq | JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq | ForIterR | ForIter)) { i.a = from as u16; }
    }

    /* the registers row op `w` at `j` reads a spilled constant read where it lies */
    fn row_sources(&self, j: usize, w: Ins) -> Vec<u16> {
        let n = usize::from(row_op_counts(&w).0);
        let mut regs: Vec<u16> = (0..n).map(|q| w.a + q as u16).collect();
        let mut k = j;
        while k > 0 && self.out[k - 1].op == OpCode::Move && self.out[k - 1].x == 0 {
            let m = self.out[k - 1];
            if let Some(q) = usize::from(m.a).checked_sub(usize::from(w.a)).filter(|&q| q < n) && regs[q] == m.a && (self.cbase..self.tbase).contains(&usize::from(m.b)) { regs[q] = m.b; }
            k -= 1;
        }
        regs
    }

    /* `displace` for keyword call `j` whose head orders arguments by its binding's or met function */
    fn displace_kw(&mut self, j: usize, old: &Code) -> bool {
        let w = self.out[j];
        // a row call reads its row and leaves its value at the row's start
        let rowed = w.op == OpCode::RowsR;
        let op = if rowed { w.b } else { w.a };
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let mut p = j;
        while !rowed && p > 0 && self.out[p - 1].op == OpCode::PushRegs && (1..=3).contains(&self.out[p - 1].x) { p -= 1; }
        let regs: Vec<u16> = if rowed { self.row_sources(j, w) } else { self.out[p..j].iter().flat_map(|g| [g.a, g.b, g.c].into_iter().take(g.x as usize)).collect() };
        let Some(mut under) = regs.len().checked_sub(1 + np + 2 * nk).filter(|&u| u <= MAX_UNDER) else { return false };
        let Some(&store) = self.out.get(j + 1) else { return false };
        let store = if rowed { Ins { op: OpCode::StoreTopR, x: 0, a: w.a, b: 0, c: 0 } } else { store };
        // a stack result is pushed past its head leaving the pushes under it alone
        let left = !(store.op == OpCode::StoreTopR && store.c == 0 && store.b == 0);
        let mut pushed = Vec::new();
        if left {
            if !self.is_temp(regs[under] as usize) || regs.iter().filter(|&&r| r == regs[under]).count() != 1 { return false; }
            pushed = regs[..under].to_vec();
            under = 0;
        }
        let regs = regs[pushed.len()..].to_vec();
        // values pending under the call are taken back past its store and a name's dropped
        let below: Option<Vec<Entry>> = (0..under).map(|e| match self.out.get(j + 2 + under - 1 - e) {
            Some(t) if t.op == OpCode::PopTop => Some(Entry { r: regs[e], name: true }),
            Some(t) if t.op == OpCode::StoreTopR && t.a == regs[e] && t.c == 0 => Some(Entry { r: regs[e], name: false }),
            _ => None,
        }).collect();
        let Some(below) = below else { return false };
        let kw = regs[under..].to_vec();
        let i = old.orig(j);
        (self.cur, self.quiet) = (i, old.back.get(j).map_or(i, |&b| u32::from(b)));
        let g = crate::vm::block::global_of(&self.out, p, Ins { op: OpCode::CallR, x: 0, a: kw[0], b: 0, c: 0 });
        let callee = match g {
            Some(g) => (self.sections)(Want::Global(self.facts.module, g)),
            None => self.only.and_then(|f| (self.sections)(Want::Func(f, false))),
        };
        // a function its `def` keeps remaking is laid for the met one the head checks
        let Some(mut callee) = callee.filter(|c| !c.by_body) else { return false };
        callee.twin = false;
        let lits = (&self.chunk.constants[..], self.cbase);
        // one taking `*args` or `**kwargs` packs them with other parameters bound as keyword calls bind
        let packed = if callee.star.is_some() || callee.dstar.is_some() { let Some(p) = Self::packed(&kw, op, &callee, lits) else { return false }; Some(p) } else { None };
        let passed = if packed.is_some() { None } else { let Some(s) = Self::kw_slots(&kw, op, &callee, lits) else { return false }; Some(s) };
        self.keyed = true;
        let m = passed.map_or(callee.arity, |s| s.0);
        let Some(to) = self.want_callee(g.unwrap_or(u16::MAX), callee, m) else { return false };
        // a row call's stub calls over the row it lies in
        self.kwrow = rowed.then_some((w.a, true));
        let lay = |l: &mut Self, store: u16, below: &[Entry]| match (&packed, passed) {
            (Some(p), _) => l.pack_lay(&kw, op, p, store, to, below, rowed.then_some(w.a)),
            (None, Some((m, slots, first))) => l.kw_lay(kw.clone(), op, m, &slots, first, store, to, below),
            _ => {}
        };
        let from = self.out.len();
        if left {
            let f = kw[0];
            for g in pushed.chunks(3) { self.emit(Ins { op: OpCode::PushRegs, x: g.len() as u8, a: g[0], b: g.get(1).copied().unwrap_or(0), c: g.get(2).copied().unwrap_or(0) }); }
            lay(self, f, &[]);
            self.emit(Ins { op: OpCode::PushRegs, x: 1, a: f, b: 0, c: 0 });
            self.emit(Ins { op: OpCode::JumpR, x: 0, a: j as u16 + 1, b: 0, c: 0 });
        } else {
            let ret = (j + 2 + under) as u16;
            // its section returns straight past the call and its stub past the head then onward
            self.ret_to = Some(ret);
            lay(self, store.a, &below);
            self.displaced.push((ret as u32, (self.out.len() - HEADER as usize) as u32));
            self.emit(Ins { op: OpCode::JumpR, x: 0, a: ret, b: 0, c: 0 });
        }
        self.displaced_to(p, from);
        true
    }
}
