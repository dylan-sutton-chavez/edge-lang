use alloc::{rc::Rc, vec, vec::Vec};

use crate::parser::{OpCode, SSAChunk, ssa_strip};

use super::VM;
use super::cache::OpcodeCache;
use super::lower::{self, Code, Facts, Ins};
use super::scope::Kind;
use super::types::*;

/* The number a register holds as a float, its int widened. */
#[inline(always)]
fn as_f64(v: Val) -> f64 { if v.is_int() { v.as_int() as f64 } else { v.as_float() } }

#[inline(always)]
fn numeric(v: Val) -> bool { v.is_int() || v.is_float() }

/* The first instruction of `code` lowered from compiler instruction `at` that `is` takes, looked for where `at` was lowered first. */
fn lowered_at(code: &Code, at: u32, is: impl Fn(crate::parser::OpCode) -> bool) -> Option<usize> {
    let (from, to) = (code.lowered(at as usize), code.lowered(at as usize + 1));
    let found = |j: &usize| is(code.ins[*j].op) && code.orig(*j) == at;
    (from..to.min(code.ins.len())).find(found).or_else(|| (0..code.ins.len()).find(found))
}

/* The function the call at compiler instruction `at` of `code` found, its site's. */
fn callee_at(code: &Code, at: u32) -> Option<Val> {
    let j = lowered_at(code, at, |op| matches!(op, crate::parser::OpCode::CallR | crate::parser::OpCode::Call | crate::parser::OpCode::CallSpreadR))?;
    match code.cache.site(j) { super::sites::Site::Call { callee, .. } => Some(callee), _ => None }
}

/* The site of the method call at compiler instruction `at` of `code`, when it found a method. */
fn method_at(code: &Code, at: u32) -> Option<super::sites::Site> {
    let j = lowered_at(code, at, |op| matches!(op, crate::parser::OpCode::CallMethodR | crate::parser::OpCode::CallMethod))?;
    lower::method_of(code, j)
}

/* The site of the op at compiler instruction `at` of `code`, when it found an operator dunder of two operands. */
fn dunder_at(code: &Code, at: u32) -> Option<super::sites::Site> { dunder_op_at(code, at).map(|(s, _)| s) }

/* `dunder_at` with the op that found it, the stub's for an op laid as a call of its dunder's section. */
fn dunder_op_at(code: &Code, at: u32) -> Option<(super::sites::Site, lower::Ins)> {
    // An op laid as a call of its dunder's section keeps its site at its stub, which a resume past a call keeping values under it enters.
    let (from, to) = (code.lowered(at as usize), code.lowered(at as usize + 1));
    let range = if from < to { from..to } else { 0..code.ins.len() };
    range.filter(|&j| from < to || code.orig(j) == at).find_map(|j| {
        let j = if code.ins[j].op == crate::parser::OpCode::DunderSectionR { code.ins.get(j + 3)?.b as usize } else { j };
        let site = code.cache.site(j);
        matches!(site, super::sites::Site::Dunder { arity: 1..=3 | super::sites::REFLECTED | super::sites::INVERTS, .. }).then_some((site, *code.ins.get(j)?))
    })
}

/* The function the site an op found last holds, else the first one noted for the op. */
fn dunder_first(site: Option<super::sites::Site>, noted: &[(u32, Val)], at: u32) -> Option<Val> {
    match site { Some(super::sites::Site::Dunder { func, .. }) => Some(func), _ => noted.iter().find(|n| n.0 == at).map(|n| n.1) }
}

/* The method a method site found. */
fn site_func(site: super::sites::Site) -> Option<Val> {
    match site { super::sites::Site::Method { func, .. } => Some(Val::heap(func)), _ => None }
}

impl<'a> VM<'a> {
    /* The code a frame of `chunk` runs, and whether the pool never keeps it. */
    #[inline(always)]
    pub(crate) fn frame_code(&mut self, chunk: &SSAChunk, pool: usize) -> (Rc<Code>, bool) {
        // A suspended run resumes in the code it parked in, though its pool laid another since, and one in its first code stays there until its resumes heat it.
        let layout = core::mem::take(&mut self.resume_layout);
        if layout != 0 && self.resume_ip != 0
            && let Some(code) = self.pools[pool].code.iter().chain(&self.retired).find(|c| c.layout == layout) { return (code.clone(), false); }
        if self.pools[pool].cold && self.resume_ip != 0 {
            self.pools[pool].heat += 1;
            if self.pools[pool].heat >= lower::HOT_LOOP { return (self.lowered_code(chunk, pool, false), false); }
        }
        let p = &self.pools[pool];
        match &p.code {
            // A body that suspends keeps the code it suspended in, whose resume points its saved state names.
            Some(code) if p.laid || p.calls < lower::TIER && self.resume_ip & super::dispatch::RESUME_RAW == 0 || chunk.is_generator || self.body_to_fi.get(&(chunk as *const SSAChunk)).is_some_and(|&fi| self.is_async.get(fi).copied().unwrap_or(false)) => (code.clone(), false),
            Some(_) => (self.lowered_code(chunk, pool, true), false),
            None => self.first_code(chunk, pool),
        }
    }

    /* The code of `pool` grown by what the head at `at` asks for, None when the growth lays nothing new. */
    pub(crate) fn grown_code(&mut self, chunk: &SSAChunk, pool: usize, at: usize, kind: lower::Grow, func: Val) -> Option<Rc<Code>> {
        let before = self.pools[pool].code.clone()?;
        let code = self.lowered_or_grown(chunk, pool, true, Some((at, kind, func)));
        (!Rc::ptr_eq(&code, &before)).then_some(code)
    }

    /* The code of `pool` made again as `grown` logs it, its lowering then each growth, for a run a snapshot restored inside a section, None for one that never grew. */
    pub(crate) fn replayed(&mut self, chunk: &SSAChunk, pool: usize) -> Option<Rc<Code>> {
        let fi = (self.pools[pool].func != super::cache::NO_FN).then_some(self.pools[pool].func as usize);
        let key = self.code_key(chunk, fi);
        let log: Vec<[Val; 4]> = self.grown.chunks(4).filter(|e| e[0].as_int() == key).map(|e| [e[0], e[1], e[2], e[3]]).collect();
        let (base, rest) = log.split_first()?;
        // Code that never grew is made again from its lowering only where a restore left none, any other being the code itself.
        if base[2].as_int() >= 0 || rest.is_empty() && self.pools[pool].code.is_some() { return None; }
        let saved = core::mem::take(&mut self.learned);
        let upto = |n: i64| saved[..(n as usize * 3).min(saved.len())].to_vec();
        self.replaying = true;
        self.learned = upto(base[1].as_int());
        if let Some(old) = self.pools[pool].code.take() { self.retired.push(old); }
        self.pools[pool].cold = false;
        let mut code = self.lowered_or_grown(chunk, pool, base[2].as_int() == -1, None);
        for g in rest {
            self.learned = upto(g[1].as_int());
            let (at, kind) = ((g[2].as_int() >> 8) as usize, lower::Grow::ALL[(g[2].as_int() & 0xFF) as usize]);
            // A binding's growth lays what it held then.
            self.replay_global = (kind == lower::Grow::Global).then(|| code.block.as_deref().and_then(|b| b.sections.get(code.ins.get(at + 2)?.c as usize)).map(|s| (s.bound, s.global, g[3]))).flatten();
            if let Some(c) = self.grown_code(chunk, pool, at, kind, g[3]) { code = c; }
            self.replay_global = None;
        }
        (self.learned, self.replaying) = (saved, false);
        Some(code)
    }

    /* Gives `fi`, which never ran, its body as the compiler wrote it, so its first call runs as a frame of the loop until a lowering, its call having bound its parameters and cells. */
    #[inline(never)]
    pub(crate) fn cold_code(&mut self, fi: usize) {
        let pool = self.fn_pool[fi];
        if self.resume_ip != 0 { return; }
        let chunk = &self.functions[fi].1;
        if cfg!(feature = "coverage") {
            if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
            if self.pools[pool].code.is_none() { self.first_code(chunk, pool); }
            return;
        }
        // Its second call lowers it, as a nested one would.
        if self.pools[pool].cold { self.first_code(chunk, pool); return; }
        if self.pools[pool].code.is_some() { return; }
        if chunk.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        if self.pools[pool].consts.is_none() { return; }
        let code = Rc::new(lower::identity(&lower::fuse_method_calls(chunk, self.pools[pool].shape), chunk.names.len(), self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi)));
        let p = &mut self.pools[pool];
        (p.code, p.cold, p.ran, p.heat) = (Some(code), true, true, p.heat + 1);
    }

    /* The code a chunk without lowered code runs, unlowered until a loop heats. */
    fn first_code(&mut self, chunk: &SSAChunk, pool: usize) -> (Rc<Code>, bool) {
        let names = chunk.names.len();
        if cfg!(feature = "coverage") {
            let (kinds, module) = self.chunk_kinds(chunk);
            let code = Rc::new(lower::identity(&chunk.instructions, names, kinds, module));
            self.pools[pool].code = Some(code.clone());
            return (code, false);
        }
        // A first call, or a cold resume, runs unlowered until a loop heats.
        if self.resume_ip & super::dispatch::RESUME_RAW != 0 { return (self.replayed(chunk, pool).unwrap_or_else(|| self.lowered_code(chunk, pool, true)), false); }
        let fi = (self.pools[pool].func != super::cache::NO_FN).then_some(self.pools[pool].func as usize);
        let p = &mut self.pools[pool];
        let cold = if self.resume_ip == 0 { !core::mem::replace(&mut p.ran, true) } else { p.heat < lower::HOT_LOOP };
        if cold {
            self.pools[pool].heat += 1;
            let (kinds, module) = self.chunk_kinds(chunk);
            return (Rc::new(lower::identity(&lower::fuse_method_calls(chunk, self.pools[pool].shape), names, kinds, module)), true);
        }
        // A generator whose resumes heated it and that calls is laid at once, as it never suspends inside a section, so its resume points hold.
        let generator = chunk.is_generator && fi.is_some_and(|fi| !self.is_async.get(fi).copied().unwrap_or(false))
            && chunk.instructions.iter().any(|i| matches!(i.opcode, OpCode::Call | OpCode::CallMethodArgs));
        let laid = generator || self.pools[pool].calls >= lower::TIER || self.was_laid(chunk, fi);
        (self.lowered_code(chunk, pool, laid), false)
    }

    /* The chunk's lowered code, made once and shared by every frame after. */
    pub(crate) fn lowered_code(&mut self, chunk: &SSAChunk, pool: usize, laid: bool) -> Rc<Code> { self.lowered_or_grown(chunk, pool, laid, None) }

    /* `lowered_code`, or for `grow` the pool's code grown by what the head it names asks for. */
    fn lowered_or_grown(&mut self, chunk: &SSAChunk, pool: usize, laid: bool, grow: Option<(usize, lower::Grow, Val)>) -> Rc<Code> {
        let alone = core::mem::take(&mut self.pools[pool].whole);
        let p = &self.pools[pool];
        if grow.is_none() && let Some(code) = &p.code && !p.cold && (p.laid || !laid) { return code.clone(); }
        // Code lowered without sections that frames still run is laid by growing it, calls wanting a section moved to heads past its end, else a whole lowering is cheaper.
        let (grow, whole) = if grow.is_none() && laid && p.active > 0 && !alone && p.code.as_ref().is_some_and(|c| c.block.is_none() && !c.unchanged() && !p.cold) { (Some((0, lower::Grow::All, Val::undef())), true) } else { (grow, false) };
        let first = self.pools[pool].cold;
        let before = self.pools[pool].code.take();
        let p = &mut self.pools[pool];
        let gone = core::mem::replace(&mut p.cold, false) && p.active == 0;
        if let Some(old) = &before && !gone { self.retired.push(old.clone()); }
        let names = chunk.names.len();
        let fi = (self.pools[pool].func != super::cache::NO_FN).then_some(self.pools[pool].func as usize);
        let cached = grow.and(before.as_deref()).and_then(|c| c.block.as_deref()).and_then(|b| b.grown.get()).and_then(|g| Some((g.src.clone()?, g.facts.clone()?)));
        let fast = cached.is_some();
        let light = fast || grow.is_some_and(|g| g.1 != lower::Grow::All);
        let mut facts = match &cached { Some((_, facts)) => (**facts).clone(), None => self.lowering_facts(chunk, pool, fi) };
        let src = match &cached {
            Some((src, _)) => alloc::borrow::Cow::Borrowed(src.as_deref().map_or(&chunk.instructions[..], |v| &v[..])),
            None => lower::fuse_method_calls(chunk, facts.shape),
        };
        let steps = laid && !chunk.is_generator && chunk.instructions.windows(3)
            .any(|w| w[0].opcode == OpCode::Call && w[1].opcode == OpCode::GetIter && w[1].operand != crate::parser::ITER_VALUE && w[2].opcode == OpCode::ForIter);
        if laid && !light { self.lower_callees(&facts.kinds, facts.module, pool, 0); }
        if let Some((_, kind, func)) = grow && kind != lower::Grow::All { self.lower_callee(func, pool, 0); self.lower_closure(func, pool); }
        if laid {
            let noted: Vec<(usize, Val)> = self.learned.chunks(3).filter_map(|t| match self.heap.try_get(t[2]) { Some(&HeapObj::Func(fi, ..)) => Some((fi, t[2])), _ => None }).collect();
            for (fi, f) in noted {
                let p = self.call_facts[fi].pool;
                if self.pools[p].consts.is_none() && let Ok(consts) = super::cache::const_vals(&self.functions[fi].1, &mut self.heap) { self.pools[p].consts = Some(consts); }
                if self.replaying { self.lower_callee(f, pool, 0); }
            }
        }
        if laid && !light && let Some(o) = before.clone().or_else(|| self.hint.clone()) { self.lower_found(&o, pool); }
        if steps && !light { self.lower_made(chunk); }
        let me = fi.filter(|&fi| self.calls_itself(fi, &facts.kinds));
        if laid && fi.is_some() && self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        let old = before.clone().or_else(|| self.hint.clone());
        let key = self.code_key(chunk, fi);
        let noted: Vec<(u32, Val)> = self.learned.chunks(3).filter(|t| t[0].as_int() == key).map(|t| (t[1].as_int() as u32, t[2])).collect();
        let used = core::cell::RefCell::new(Vec::new());
        facts.noted = self.learned.chunks(3).map(|t| t[0].as_int()).collect();
        facts.noted.sort_unstable();
        facts.noted.dedup();
        facts.dunders = !noted.is_empty() || old.as_deref().is_some_and(|o| (0..o.ins.len()).any(|j| matches!(o.cache.site(j), super::sites::Site::Dunder { .. })));
        if laid && !first && facts.shape & lower::CALLS == 0 && !facts.dunders && let Some(code) = before.as_ref().filter(|c| c.block.is_none() && !c.unchanged()) {
            (self.pools[pool].code, self.pools[pool].laid) = (Some(code.clone()), true);
            return code.clone();
        }
        let gens = if steps && !light { self.gen_bodies(&facts.kinds, facts.module) } else { Vec::new() };
        if laid && !light { self.gen_consts(facts.module); }
        let lazy: core::cell::RefCell<Vec<(u64, usize, Rc<Code>)>> = core::cell::RefCell::new(Vec::new());
        let made: core::cell::OnceCell<Option<Rc<Code>>> = core::cell::OnceCell::new();
        let plain = || made.get_or_init(|| if !laid { None } else if let Some(old) = before.as_ref().and_then(|c| self.plain_of(c, chunk, || self.facts_with(chunk, facts.kinds.clone(), facts.module, facts.shape, fi))) { Some(old) }
            // Laid with no code before, as at its first call or after a snapshot, its own body is made here for the calls of itself.
            else { fi.filter(|_| me.is_some() || old.is_none()).and_then(|_| lower::lower(&src, chunk, &facts, &|_| None)).filter(|c| c.pure()).map(Rc::new) }).clone();
        let own = |f: usize| if fi != Some(f) { None } else if me.is_some() { plain() } else { old.clone().filter(|c| c.block.is_none() && c.pure()).or_else(plain) };
        let sections = |w: lower::Want| if !laid { None } else {
            match w {
                // A generator a module binding holds, for the loop that steps it.
                lower::Want::Gen(m, g) => {
                    let value = self.scopes.get(m)?.at(g as u32);
                    if let Some(&(_, fi, ref body)) = gens.iter().chain(lazy.borrow().iter()).find(|b| b.0 == value.0) { return self.gen_section_of(value, fi, body.clone()); }
                    let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(value) else { return None };
                    let body = self.gen_body(fi)?;
                    lazy.borrow_mut().push((value.0, fi, body.clone()));
                    self.gen_section_of(value, fi, body)
                }
                // The generator a call through a register made, for the loop over what it gave.
                lower::Want::GenAt(at) => {
                    let value = noted.iter().find(|n| n.0 == at | lower::GEN)?.1;
                    let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(value) else { return None };
                    let body = self.gen_body(fi)?;
                    self.gen_section_of(value, fi, body)
                }
                // A generator another loop's head met, for that loop.
                lower::Want::GenFunc(value) => {
                    if let Some(&(_, fi, ref body)) = gens.iter().chain(lazy.borrow().iter()).find(|b| b.0 == value.0) { return self.gen_section_of(value, fi, body.clone()); }
                    let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(value) else { return None };
                    let body = self.gen_body(fi)?;
                    lazy.borrow_mut().push((value.0, fi, body.clone()));
                    self.gen_section_of(value, fi, body)
                }
                lower::Want::Global(m, g) => {
                    let value = match self.replay_global { Some((rm, rg, f)) if rm == m && rg == g => f, _ => self.scopes.get(m)?.at(g as u32) };
                    let mut callee = self.section_of(value, &own)?;
                    if callee.module != m { callee.bound = m as u16; }
                    // A closure a binding holds is often one of many the binding takes in turn, a loop's variable, which run its twin.
                    callee.twin = callee.ctor().is_none() && (!callee.cells.is_empty() || self.remade(callee.fi));
                    Some(callee)
                }
                // A bound method or an instance with a `__call__` a module binding holds.
                lower::Want::Held(m, g) => match self.heap.try_get(self.scopes.get(m)?.at(g as u32)) {
                    Some(&HeapObj::BoundUserMethod(_, func, _)) => self.bound_method(func, &own),
                    Some(&HeapObj::Instance(class, _)) => self.called_instance(class, &own),
                    _ => None,
                },
                // A function taking `*args` or `**kwargs`, the only kind a call that packs its arguments lays.
                lower::Want::Spread(m, g) => {
                    let value = self.scopes.get(m)?.at(g as u32);
                    let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(value) else { return None };
                    if self.call_facts[fi].arity.is_some() { return None; }
                    let mut callee = self.section_of(value, &own)?;
                    if callee.module != m { callee.bound = m as u16; }
                    Some(callee)
                }
                lower::Want::Method(at) => {
                    // A function the receiver held by the name is called with the arguments alone.
                    if let Some(f) = noted.iter().find(|n| n.0 == at | lower::FIELD).map(|n| n.1) { return self.on_field(f, &own); }
                    let site = old.as_deref().and_then(|o| method_at(o, at));
                    let func = noted.iter().find(|n| n.0 == at).map(|n| n.1).or_else(|| site.and_then(site_func))?;
                    if matches!(self.heap.try_get(func), Some(HeapObj::Class(..))) { return self.on_class(func, chunk.names.get(src.get(at as usize)?.operand as usize)?, &own); }
                    let Some(&HeapObj::Func(mfi, ..)) = self.heap.try_get(func) else { return None };
                    let mut callee = self.section_of(func, &own)?;
                    callee.supers = self.super_of(mfi)?;
                    callee.set_site(site.filter(|s| site_func(*s).is_some_and(|f| f.0 == func.0)));
                    callee.class = match callee.site() { Some(super::sites::Site::Method { class, .. }) => Val::heap(class), _ => Val::undef() };
                    used.borrow_mut().push((key, at, func));
                    Some(callee)
                }
                lower::Want::Value(at) => {
                    let Some(func) = noted.iter().find(|n| n.0 == at).map(|n| n.1).or_else(|| old.as_deref().and_then(|o| callee_at(o, at))) else {
                        if let Some(n) = noted.iter().find(|n| n.0 == at | lower::BOUND) { return self.bound_method(n.1, &own); }
                        return noted.iter().find(|n| n.0 == at | lower::CALLED).and_then(|n| self.called_instance(n.1, &own));
                    };
                    // A body naming `super` needs the binding a method call makes, which no register call has, save a `with`'s dunder, laid for the class the `with` checks.
                    if self.body_names_super(func) {
                        let class = noted.iter().find(|n| n.0 == at | lower::WITH)?.1;
                        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
                        let mut callee = self.section_of(func, &own)?;
                        (callee.supers, callee.class) = (self.super_of(fi)?, class);
                        used.borrow_mut().push((key, at, func));
                        return Some(callee);
                    }
                    let mut callee = self.section_of(func, &own)?;
                    callee.by_body = !callee.defaults.is_empty();
                    callee.twin = !callee.by_body && (!callee.cells.is_empty() || self.remade(callee.fi));
                    used.borrow_mut().push((key, at, func));
                    Some(callee)
                }
                // Each other function a call through a register met, the first noted ones and then the one its site found last.
                lower::Want::Alt(nfi, at, j) => {
                    let mut seen: Vec<Val> = match nfi {
                        Some(nfi) => self.learned.chunks(3).filter(|t| t[0].as_int() == nfi as i64 && t[1].as_int() == at as i64).map(|t| t[2]).collect(),
                        None => noted.iter().filter(|n| n.0 == at).map(|n| n.1).collect(),
                    };
                    if nfi.is_none() && let Some(f) = old.as_deref().and_then(|o| callee_at(o, at)) && !seen.iter().any(|v| v.0 == f.0) { seen.push(f); }
                    let func = *seen.get(j as usize)?;
                    if self.body_names_super(func) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.twin = false;
                    used.borrow_mut().push((nfi.map_or(key, |f| f as i64), at, func));
                    Some(callee)
                }
                // The dunder a head of a growth met, laid as the alternatives a register or method call met are.
                lower::Want::DunderFunc(func, op, name) => {
                    let mut callee = self.dunder_callee(func, None, op, &own)?;
                    // A class's `__getattr__` takes the name the load names past the instance.
                    if op == crate::parser::OpCode::GetAttrR && callee.arity == 2 { callee.set_arg(Some(Val::heap(self.heap.string_slot(crate::parser::ssa_strip(chunk.names.get(name as usize)?))?))); }
                    Some(callee)
                }
                // The function a closure's cell holds, which a call in its section's body makes.
                lower::Want::Celled(cell) => {
                    let Some(&HeapObj::Cell(func)) = self.heap.try_get(cell) else { return None };
                    // One its `def` makes again and again is another function at each call, which the section's check turns down.
                    let Some(&HeapObj::Func(cfi, ..)) = self.heap.try_get(func) else { return None };
                    if self.body_names_super(func) || self.remade(cfi) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.twin = false;
                    Some(callee)
                }
                lower::Want::Func(func, method) => {
                    // A class's call runs its constructor, a bound method's its function with the receiver first.
                    if !method && matches!(self.heap.try_get(func), Some(HeapObj::Class(..))) { return self.section_of(func, &own); }
                    if !method && let Some(&HeapObj::BoundUserMethod(_, f, _)) = self.heap.try_get(func) { return self.bound_method(f, &own); }
                    if self.body_names_super(func) && !method { return None; }
                    let Some(&HeapObj::Func(mfi, ..)) = self.heap.try_get(func) else { return None };
                    let mut callee = self.section_of(func, &own)?;
                    if method { callee.supers = self.super_of(mfi)?; }
                    callee.twin = false;
                    Some(callee)
                }
                // Each other method a method call met, noted under the function it lies in, or under this code for its own.
                lower::Want::MethodAlt(nfi, at, j) => {
                    let nkey = nfi.map_or(key, |f| f as i64);
                    let func = self.learned.chunks(3).filter(|t| t[0].as_int() == nkey && t[1].as_int() == at as i64).map(|t| t[2]).nth(j as usize)?;
                    // An instance a register call called runs its class's `__call__`, and a function a receiver held by the name takes the arguments alone.
                    if at & lower::CALLED != 0 { return self.called_instance(func, &own); }
                    if at & lower::FIELD != 0 { return self.on_field(func, &own); }
                    if at & lower::STATIC != 0 { return self.on_class_found(func, Val::undef(), &own); }
                    let Some(&HeapObj::Func(mfi, ..)) = self.heap.try_get(func) else { return None };
                    let mut callee = self.section_of(func, &own)?;
                    callee.supers = self.super_of(mfi)?;
                    callee.twin = false;
                    Some(callee)
                }
                // A method a call inside a laid function found, noted under that function.
                lower::Want::Nested(nfi, at, seen, name) => {
                    let nkey = nfi as i64;
                    if let Some(f) = self.learned.chunks(3).find(|t| t[0].as_int() == nkey && t[1].as_int() == (at | lower::FIELD) as i64).map(|t| t[2]) { return self.on_field(f, &own); }
                    let func = self.learned.chunks(3).find(|t| t[0].as_int() == nkey && t[1].as_int() == at as i64).map(|t| t[2]).or_else(|| seen.and_then(site_func))?;
                    if matches!(self.heap.try_get(func), Some(HeapObj::Class(..))) {
                        let body = &self.functions[nfi].1;
                        return self.on_class(func, body.names.get(name as usize)?, &own);
                    }
                    let Some(&HeapObj::Func(mfi, ..)) = self.heap.try_get(func) else { return None };
                    let mut callee = self.section_of(func, &own)?;
                    callee.supers = self.super_of(mfi)?;
                    callee.set_site(seen.filter(|s| site_func(*s).is_some_and(|f| f.0 == func.0)));
                    callee.class = match callee.site() { Some(super::sites::Site::Method { class, .. }) => Val::heap(class), _ => Val::undef() };
                    used.borrow_mut().push((nkey, at, func));
                    Some(callee)
                }
                // The method a call on a `super()` proxy inside method `method` finds, past the class `method` lies in.
                lower::Want::Super(nfi, name, method, class) => {
                    let name = crate::parser::ssa_strip(self.functions[nfi].1.names.get(name as usize)?);
                    let owner = self.owner_of(method)?;
                    // Past the class the method lies in, in the order of the class of the receiver it was laid for.
                    let class = if class.is_undef() { owner } else { class };
                    let (target, _) = self.lookup_class_member_after(class, owner, name)?;
                    let Some(&HeapObj::Func(tfi, ..)) = self.heap.try_get(target) else { return None };
                    let mut callee = self.section_of(target, &own)?;
                    callee.supers = self.super_of(tfi)?;
                    callee.set_via_super(Some((owner, name.into())));
                    callee.class = class;
                    Some(callee)
                }
                // The operator dunder an op found, laid as a method is, unless its body may leave through a handler.
                lower::Want::Dunder(at, op) => {
                    let found = old.as_deref().and_then(|o| dunder_op_at(o, at));
                    let site = found.map(|f| f.0);
                    let first = dunder_first(site, &noted, at)?;
                    // A first dunder that cannot be laid yields to one noted after, and a load's two-parameter dunder is `__getattr__`, passed the name as a string.
                    let (func, mut callee) = core::iter::once(first).chain(noted.iter().filter(|n| n.0 == at && n.1.0 != first.0).map(|n| n.1))
                        .find_map(|f| Some((f, self.dunder_callee(f, if f.0 == first.0 { site } else { None }, op, &own)?)))?;
                    if op == crate::parser::OpCode::GetAttrR && matches!(site, Some(super::sites::Site::Dunder { arity: 2, .. })) {
                        let name = chunk.names.get(found?.1.c as usize)?;
                        callee.set_arg(Some(Val::heap(self.heap.string_slot(crate::parser::ssa_strip(name))?)));
                    }
                    used.borrow_mut().push((key, at, func));
                    Some(callee)
                }
                // Each other dunder the op met, noted when its stub ran the op.
                lower::Want::DunderAlt(at, j, op) => {
                    let first = dunder_first(old.as_deref().and_then(|o| dunder_at(o, at)), &noted, at);
                    let func = noted.iter().filter(|n| n.0 == at && first.is_none_or(|f| f.0 != n.1.0)).map(|n| n.1).nth(j as usize)?;
                    let callee = self.dunder_callee(func, None, op, &own)?;
                    used.borrow_mut().push((key, at, func));
                    Some(callee)
                }
                // Each other dunder an op inside a laid function met, noted under that function past the first.
                lower::Want::NestedDunderAlt(nfi, at, first, j, op) => {
                    let nkey = nfi as i64;
                    let func = self.learned.chunks(3).filter(|t| t[0].as_int() == nkey && t[1].as_int() == at as i64 && t[2].0 != first.0).map(|t| t[2]).nth(j as usize)?;
                    let callee = self.dunder_callee(func, None, op, &own)?;
                    used.borrow_mut().push((nkey, at, func));
                    Some(callee)
                }
                // The operator dunder an op inside a laid function found, noted under that function.
                lower::Want::NestedDunder(nfi, at, seen, op) => {
                    let nkey = nfi as i64;
                    let func = self.learned.chunks(3).find(|t| t[0].as_int() == nkey && t[1].as_int() == at as i64).map(|t| t[2])
                        .or(match seen { Some(super::sites::Site::Dunder { func, .. }) => Some(func), _ => None })?;
                    let callee = self.dunder_callee(func, seen, op, &own)?;
                    used.borrow_mut().push((nkey, at, func));
                    Some(callee)
                }
                // Any function of a body the code makes, the callee a call takes straight from where the code made it.
                lower::Want::Body(body) => self.body_section(body),
                // The body of a function made again and again, any function of which the section runs.
                lower::Want::BodyOf(func) => {
                    let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
                    if !self.remade(fi) { return None; }
                    self.body_section(&self.functions[fi].1 as *const SSAChunk)
                }
                // A generator a body the code makes is, made and called just before.
                lower::Want::GenBody(body) => {
                    let fi = *self.body_to_fi.get(&body)?;
                    if self.call_facts[fi].closure { return None; }
                    let body = self.gen_body(fi)?;
                    self.gen_section_of(Val::undef(), fi, body)
                }
                lower::Want::Made(body) => self.made_section(body),
                lower::Want::MadeIn(fi, k) => self.functions.get(fi).and_then(|f| f.1.functions.get(k as usize)).filter(|f| super::scope::is_comp(&f.0)).and_then(|f| self.made_section(&f.1)),
                lower::Want::NestedValue(nfi, at, seen) => {
                    let nkey = nfi as i64;
                    let (mut first, mut bound, mut called) = (None, None, None);
                    for t in self.learned.chunks(3).filter(|t| t[0].as_int() == nkey) {
                        let a = t[1].as_int() as u32;
                        if a == at { first = first.or(Some(t[2])); } else if a == at | lower::BOUND { bound = Some(t[2]); } else if a == at | lower::CALLED { called = Some(t[2]); }
                    }
                    let func = first.or(seen).filter(|&f| matches!(self.heap.try_get(f), Some(HeapObj::Func(..) | HeapObj::Class(..))));
                    let Some(func) = func else {
                        if let Some(f) = bound { return self.bound_method(f, &own); }
                        return called.and_then(|c| self.called_instance(c, &own));
                    };
                    // The `__call__` a growth met for an instance the call met is that instance's, and a bound method's function its.
                    if let Some(c) = called && self.lookup_class_member(c, "__call__").is_some_and(|(m, _)| m.0 == func.0) { return self.called_instance(c, &own); }
                    if bound.is_some_and(|f| f.0 == func.0) { return self.bound_method(func, &own); }
                    let mut callee = self.section_of(func, &own)?;
                    callee.by_body = !callee.defaults.is_empty();
                    callee.twin = !callee.by_body && (!callee.cells.is_empty() || self.remade(callee.fi));
                    used.borrow_mut().push((nkey, at, func));
                    Some(callee)
                }
            }
        };
        let mut fell = false;
        let mut code = match grow {
            None => lower::lower(&src, chunk, &facts, &sections).unwrap_or_else(|| {
                facts.regs = false;
                lower::lower(&src, chunk, &facts, &|_| None).unwrap_or_else(|| lower::identity(&chunk.instructions, names, facts.kinds.clone(), facts.module))
            }),
            Some((at, kind, func)) => {
                let grown = before.as_deref().and_then(|old| lower::extend(old, &src, chunk, &facts, &sections, at, kind, func))
                    .or_else(|| { fell = whole; whole.then(|| lower::lower(&src, chunk, &facts, &sections)).flatten() });
                // A growth that lays nothing new leaves the code as it was.
                let Some(code) = grown else {
                    if let Some(old) = &before && self.retired.last().is_some_and(|r| Rc::ptr_eq(r, old)) { self.retired.pop(); }
                    self.pools[pool].code = before.clone();
                    return before.unwrap_or_else(|| Rc::new(lower::identity(&chunk.instructions, names, facts.kinds.clone(), facts.module)));
                };
                code
            }
        };
        for (_, fi, body) in lazy.take() {
            let pool = self.call_facts[fi].pool;
            if self.pools[pool].code.is_none() { self.pools[pool].code = Some(body); }
        }
        if code.block.is_some() && let Some(plain) = made.get().cloned().flatten() { let _ = code.plain.set(Some(plain)); }
        if grow.is_some() && let Some(b) = code.block.as_mut() && b.grown().src.is_none() && let Some(g) = b.grown.get_mut() {
            g.src = Some(match src { alloc::borrow::Cow::Borrowed(_) => None, alloc::borrow::Cow::Owned(v) => Some(Rc::new(v)) });
            g.facts = Some(Rc::new(facts));
        }
        let code = Rc::new(code);
        (self.pools[pool].code, self.pools[pool].laid) = (Some(code.clone()), laid);
        for (k, at, func) in used.into_inner() {
            if !self.learned.chunks(3).any(|t| t[0].as_int() == k && t[1].as_int() == at as i64 && t[2].0 == func.0) { self.learned.extend_from_slice(&[Val::int(k), Val::int(at as i64), func]); }
        }
        let read = self.learned.len() / 3;
        match grow.filter(|_| !fell) {
            None => {
                code.read.set(read as u32);
                if !self.replaying {
                    let mut k = 0;
                    while k < self.grown.len() { if self.grown[k].as_int() == key { self.grown.drain(k..k + 4); } else { k += 4; } }
                    if code.block.is_some() { self.grown.extend_from_slice(&[Val::int(key), Val::int(read as i64), Val::int(-1), Val::undef()]); }
                }
            }
            Some((at, kind, func)) if !self.replaying => {
                if !self.grown.chunks(4).any(|e| e[0].as_int() == key) {
                    let base = before.as_deref().and_then(|c| c.block.as_deref());
                    self.grown.extend_from_slice(&[Val::int(key), Val::int(base.map_or(0, |_| before.as_deref().map_or(0, |c| c.read.get()) as i64)), Val::int(if base.is_some() { -1 } else { -2 }), Val::undef()]);
                }
                self.grown.extend_from_slice(&[Val::int(key), Val::int(read as i64), Val::int((at as i64) << 8 | kind as i64), func]);
            }
            Some(_) => {}
        }
        if laid && !self.was_laid(chunk, fi) {
            match fi {
                Some(fi) => { if self.laid_fns.len() <= fi { self.laid_fns.resize(fi + 1, false); } self.laid_fns[fi] = true; }
                None => self.laid_mods.push(chunk.path.as_str().into()),
            }
        }
        code
    }

    /* The function a call of method `name` on the class itself runs, laid as a method's section, a static method or plain function given the arguments alone, a class method the class first. */
    fn on_class(&self, class: Val, name: &str, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let (v, _) = self.lookup_class_member(class, crate::parser::ssa_strip(name))?;
        self.on_class_found(v, class, own)
    }

    /* `on_class` for what the class holds by the name, `v`. */
    fn on_class_found(&self, v: Val, class: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let (func, kind) = match *self.heap.try_get(v)? {
            HeapObj::StaticMethod(f) => (f, 1),
            HeapObj::ClassMethod(f) => (f, 2),
            HeapObj::Func(..) => (v, 1),
            _ => return None,
        };
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
        // A `super()` in it would need the binding a call on an instance makes.
        if self.super_of(fi)? .is_some() { return None; }
        let mut callee = self.section_of(func, own)?;
        if callee.ctor().is_some() { return None; }
        (callee.on_class, callee.class) = (kind, class);
        Some(callee)
    }

    /* The function `func` an instance holds, which a method call of its name runs with the arguments alone. */
    fn on_field(&self, func: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
        if self.super_of(fi)?.is_some() { return None; }
        let mut callee = self.section_of(func, own)?;
        if callee.ctor().is_some() { return None; }
        callee.on_class = 4;
        Some(callee)
    }

    /* The `__call__` of the class `class` an instance a call through a register called runs, laid as a method's section. */
    fn called_instance(&self, class: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let (func, _) = self.lookup_class_member(class, "__call__")?;
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
        if self.super_of(fi)?.is_some() { return None; }
        let mut callee = self.section_of(func, own)?;
        (callee.on_class, callee.class) = (3, class);
        Some(callee)
    }

    /* The section of `func` for the bound methods of it a call through a register calls, their receiver its first argument. */
    fn bound_method(&self, func: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return None };
        // A body naming `super` needs the class the method was bound for, which its section does not check.
        if self.body_names_super(func) || self.super_of(fi)?.is_some() { return None; }
        let mut callee = self.section_of(func, own)?;
        callee.on_class = 7;
        Some(callee)
    }

    /* Whether function `fi` is made again and again, its `def` inside a function or a loop, so a call meets many functions of its body. */
    fn remade(&self, fi: usize) -> bool {
        match self.fn_remade[fi].get() { 0 => {}, r => return r == 2 }
        if self.function_parents[fi].is_some() { self.fn_remade[fi].set(2); return true; }
        // SAFETY every chunk that defines a function outlives the VM's run.
        let parent = unsafe { &*self.fn_definer[fi] };
        let ins = &parent.instructions;
        let backs: Vec<(usize, usize)> = ins.iter().enumerate().filter(|(j, i)| i.opcode == OpCode::Jump && (i.operand as usize) <= *j).map(|(j, i)| (i.operand as usize, j)).collect();
        for (m, i) in ins.iter().enumerate().filter(|(_, i)| matches!(i.opcode, OpCode::MakeFunction | OpCode::MakeCoroutine)) {
            let Some(&g) = parent.functions.get(i.operand as usize).and_then(|d| self.body_to_fi.get(&(&d.1 as *const SSAChunk))) else { continue };
            if self.fn_remade[g].get() == 0 { self.fn_remade[g].set(1 + backs.iter().any(|&(t, j)| t <= m && m < j) as u8); }
        }
        if self.fn_remade[fi].get() == 0 { self.fn_remade[fi].set(1); }
        self.fn_remade[fi].get() == 2
    }

    /* Whether the body of the function `func` names `super`. */
    fn body_names_super(&self, func: Val) -> bool {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return false };
        self.functions[fi].1.names.iter().any(|n| crate::parser::ssa_strip(n) == "super")
    }

    /* What names `chunk`'s code across snapshots, function `fi`, or below zero its module's path. */
    fn code_key(&self, chunk: &SSAChunk, fi: Option<usize>) -> i64 { Self::code_key_of(chunk, fi) }

    /* `code_key` as a function of the chunk and its function alone. */
    pub(crate) fn code_key_of(chunk: &SSAChunk, fi: Option<usize>) -> i64 {
        match fi {
            Some(fi) => fi as i64,
            None => -1 - (crate::value::hash_key(chunk.path.as_str()) as i64 & 0x3FFF_FFFF_FFFF),
        }
    }

    /* Whether `chunk`, function `fi` or a module, had code laid with sections. */
    fn was_laid(&self, chunk: &SSAChunk, fi: Option<usize>) -> bool {
        match fi {
            Some(fi) => self.laid_fns.get(fi).copied().unwrap_or(false),
            None => self.laid_mods.iter().any(|p| p.as_str() == chunk.path.as_str()),
        }
    }

    /* The function `value` as a call may lay it as a section, a plain function whose lowered body only calls such functions. */
    fn section_of(&self, value: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        if cfg!(feature = "coverage") { return None; }
        if matches!(self.heap.try_get(value), Some(HeapObj::Class(..))) { return self.ctor_section(value, own); }
        let Some(HeapObj::Func(fi, defaults, captures, _)) = self.heap.try_get(value) else { return None };
        self.section_of_fn(value, *fi, defaults.clone(), captures.clone(), own)
    }

    /* The section every function a body made with no defaults runs the same, its cells filled from each, for a call of a function the code just made from that body. */
    fn body_section(&self, body: *const SSAChunk) -> Option<lower::Callee<'_>> {
        let fi = *self.body_to_fi.get(&body)?;
        if self.functions[fi].2 != 0 || self.call_facts[fi].arity.is_none() { return None; }
        // A closure's cells are filled from each function the body makes as it is called, as a comprehension's are.
        let cells: Vec<(usize, Val)> = if self.call_facts[fi].closure { self.fn_scope[fi].freevars.iter().map(|f| (f.0, Val::undef())).collect() } else { Vec::new() };
        let mut callee = self.section_of_fn(Val::undef(), fi, Vec::new(), cells, &|_| None)?;
        callee.by_body = true;
        Some(callee)
    }

    /* The section any comprehension a MakeFunction of `body` makes runs, its cells filled from each one as it is called. */
    fn made_section(&self, body: *const SSAChunk) -> Option<lower::Callee<'_>> {
        let fi = *self.body_to_fi.get(&body)?;
        if self.functions[fi].2 != 0 || self.call_facts[fi].arity != Some(1) { return None; }
        let cells: Vec<(usize, Val)> = self.fn_scope[fi].freevars.iter().map(|f| (f.0, Val::undef())).collect();
        let mut callee = self.section_of_fn(Val::undef(), fi, Vec::new(), cells, &|_| None)?;
        callee.by_body = true;
        Some(callee)
    }

    /* `section_of` for function `fi` with `defaults` and the cells it captured, `value` the function or undef for any of its body. */
    fn section_of_fn(&self, value: Val, fi: usize, defaults: Vec<Val>, cells: Vec<(usize, Val)>, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let f = &self.call_facts[fi];
        if self.peeks(fi) { return None; }
        use super::ParamKind::{KwOnly, Normal};
        let params = &self.param_slots[fi];
        let positional = params.iter().take_while(|p| matches!(p.0, Normal)).count();
        let marks = &self.functions[fi].0;
        let keyed = f.arity.is_none() && positional < params.len() && params[positional..].iter().all(|p| matches!(p.0, KwOnly))
            && marks.iter().skip_while(|m| !m.ends_with('=')).all(|m| m.ends_with('='));
        if f.arity.is_none() && !keyed { return self.star_section(value, fi, own); }
        let required = f.required as usize;
        let arity = f.arity.unwrap_or(params.len());
        // Only a body whose positionals fill its first slots is laid, a coroutine's call making the coroutine and running none of its body.
        let plain_slots = self.param_slots[fi].iter().enumerate().all(|(k, &(_, s))| s == k);
        if !plain_slots || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return None; }
        if f.closure && cells.is_empty() { return None; }
        let (names, module) = (self.functions[fi].1.names.len(), self.fn_module_id(fi));
        let pool = &self.pools[f.pool];
        let body = match &pool.code {
            Some(code) => self.plain_of(code, &self.functions[fi].1, || self.fn_facts(fi))?,
            None => own(fi)?,
        };
        let consts = pool.consts.as_deref()?.to_vec();
        Some(lower::Callee { value, fi, cells, defaults, required, rare: None, reflected: false, check: 0, body, arity, names, consts, module, params: &self.functions[fi].0, posonly: self.functions[fi].1.posonly as usize, literals: &self.functions[fi].1.constants, by_body: false, twin: false, supers: None, star: None, dstar: None, cellvars: self.fn_scope[fi].cellvars.clone(), positional, class: Val::undef(), on_class: 0, generator: false, exc: false, bound: u16::MAX })
    }

    /* Whether the body of `fi` may read its own frame by name, through `exec` or `eval`, which a section's registers hide. */
    fn peeks(&self, fi: usize) -> bool {
        let Some(scope) = self.scopes.get(self.fn_module_id(fi)) else { return true };
        self.fn_scope[fi].kinds.iter().any(|k| matches!(*k, Kind::Global(g) if matches!(scope.name(g), "exec" | "eval")))
    }

    /* The generator functions the module bindings of `kinds` hold, each with its body lowered without sections, its pool's code or made here. */
    fn gen_bodies(&mut self, kinds: &[Kind], module: usize) -> Vec<(u64, usize, Rc<Code>)> {
        let mut out: Vec<(u64, usize, Rc<Code>)> = Vec::new();
        for k in kinds {
            let Kind::Global(g) = *k else { continue };
            let Some(v) = self.scopes.get(module).map(|s| s.at(g)) else { continue };
            let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(v) else { continue };
            if !self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) || out.iter().any(|o| o.0 == v.0) { continue; }
            let (body, pool) = (self.functions[fi], self.call_facts[fi].pool);
            if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[pool].consts = Some(consts); }
            if self.pools[pool].consts.is_none() { continue; }
            let code = match &self.pools[pool].code {
                Some(code) if !code.unchanged() && code.block.is_none() => code.clone(),
                // Lowered once, kept as its pool's code when it has none, as for one only its sections ever run.
                _ => {
                    let facts = self.lowering_facts(&body.1, pool, Some(fi));
                    let Some(code) = lower::lower(&lower::fuse_method_calls(&body.1, facts.shape), &body.1, &facts, &|_| None) else { continue };
                    let code = Rc::new(code);
                    if self.pools[pool].code.is_none() { self.pools[pool].code = Some(code.clone()); }
                    code
                }
            };
            out.push((v.0, fi, code));
        }
        out
    }

    /* The constants of the generator functions module `m` binds, made once so a lowering may lay any of them. */
    fn gen_consts(&mut self, m: usize) {
        let Some(scope) = self.scopes.get(m) else { return };
        let fns: Vec<usize> = scope.iter().filter_map(|(_, v)| match self.heap.try_get(v) { Some(&HeapObj::Func(fi, ..)) if self.functions[fi].1.is_generator => Some(fi), _ => None }).collect();
        for fi in fns {
            let pool = self.call_facts[fi].pool;
            if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&self.functions[fi].1, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        }
    }

    /* The body of generator function `fi` lowered without sections, its pool's code or made now for one that never ran lowered. */
    fn gen_body(&self, fi: usize) -> Option<Rc<Code>> {
        if !self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return None; }
        let (body, pool) = (self.functions[fi], self.call_facts[fi].pool);
        self.pools[pool].consts.as_ref()?;
        match &self.pools[pool].code {
            Some(code) if !code.unchanged() && code.block.is_none() => Some(code.clone()),
            _ => {
                let facts = self.fn_facts(fi);
                lower::lower(&lower::fuse_method_calls(&body.1, facts.shape), &body.1, &facts, &|_| None).map(Rc::new)
            }
        }
    }

    /* A generator function a `for` steps that may be laid as the loop's section, its positionals filling its first slots, `*args` after, capturing nothing and keeping no cells. */
    fn gen_section_of(&self, value: Val, fi: usize, body: Rc<Code>) -> Option<lower::Callee<'_>> {
        use super::ParamKind::*;
        if cfg!(feature = "coverage") { return None; }
        // One a body makes, `value` undefined, takes neither defaults nor captures.
        let (defaults, captures) = match self.heap.try_get(value) { Some(HeapObj::Func(_, d, c, _)) => (d.clone(), c.len()), _ => (Vec::new(), 0) };
        let params = &self.param_slots[fi];
        let p = params.iter().take_while(|(k, _)| matches!(k, Normal)).count();
        let star = params.get(p).filter(|(k, _)| matches!(k, Star)).map(|_| p);
        let arity = p + star.is_some() as usize;
        if params.len() != arity || !params.iter().enumerate().all(|(k, &(_, s))| s == k) || captures != 0 || !self.fn_scope[fi].cellvars.is_empty()
            || star.is_some() && !defaults.is_empty() || defaults.len() > p { return None; }
        let consts = self.pools[self.call_facts[fi].pool].consts.as_deref()?.to_vec();
        Some(lower::Callee { value, fi, cells: Vec::new(), required: p - defaults.len(), defaults, rare: None, reflected: false, check: 0, body, arity, names: self.functions[fi].1.names.len(), consts,
            module: self.fn_module_id(fi), params: &self.functions[fi].0, posonly: self.functions[fi].1.posonly as usize, literals: &self.functions[fi].1.constants, by_body: false, twin: false, supers: None, star, dstar: None, cellvars: Vec::new(), positional: arity, class: Val::undef(), on_class: 0, generator: true, exc: false, bound: u16::MAX })
    }

    /* The section an op lays for its operator dunder `func`, which `site` found, unless its body may leave through a handler. */
    fn dunder_callee(&self, func: Val, site: Option<super::sites::Site>, op: crate::parser::OpCode, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let Some(&HeapObj::Func(mfi, ..)) = self.heap.try_get(func) else { return None };
        let mut callee = self.section_of(func, own)?;
        if callee.ctor().is_some() || callee.star.is_some() || callee.dstar.is_some() { return None; }
        // The check asks `str` and `repr` for a string, a `!=` with no `__ne__` to invert its `__eq__`, a `__bool__` for a bool and a `__len__` for a length.
        if matches!(op, crate::parser::OpCode::CallStr | crate::parser::OpCode::CallRepr) { callee.check = 4 + (op == crate::parser::OpCode::CallRepr) as u8; }
        callee.supers = self.super_of(mfi)?;
        callee.set_site(site.filter(|s| matches!(*s, super::sites::Site::Dunder { func: f, .. } if f.0 == func.0)));
        callee.reflected = match callee.site() {
            Some(super::sites::Site::Dunder { arity, .. }) => arity == super::sites::REFLECTED,
            _ => super::opcodes::dunder::op_names(op).is_some_and(|(l, r)| l != r && self.function_names.get(mfi).is_some_and(|n| n == r)),
        };
        callee.class = match callee.site() { Some(super::sites::Site::Dunder { class, arity, .. }) if arity != super::sites::REFLECTED => class, _ => Val::undef() };
        if op == crate::parser::OpCode::NotEqR && self.function_names.get(mfi).is_some_and(|n| n == "__eq__") { callee.check = 1; }
        if matches!(op, crate::parser::OpCode::JumpIfFalseR | crate::parser::OpCode::CallBool) {
            callee.check = match self.function_names.get(mfi).map(|n| n.as_str()) { Some("__bool__") => 2, Some("__len__") => 3, _ => return None };
        }
        Some(callee)
    }

    /* For a method `fi` that calls `super()`, the module binding its section makes the proxy for in place, Some(None) for one never naming it, None for one with its own `super`. */
    fn super_of(&self, fi: usize) -> Option<Option<u16>> {
        if !self.functions[fi].1.names.iter().any(|n| crate::parser::ssa_strip(n) == "super") { return Some(None); }
        let scope = self.scopes.get(self.fn_module_id(fi))?;
        let g = scope.entries().position(|(n, _)| n == "super")?;
        scope.at(g as u32).is_undef().then_some(Some(g as u16))
    }

    /* A plain function `fi` taking `*args` after its positionals and nothing else, laid as a section whose last parameter is the tuple of the rest. */
    fn star_section(&self, value: Val, fi: usize, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        use super::ParamKind::*;
        let Some(HeapObj::Func(_, defaults, captures, _)) = self.heap.try_get(value) else { return None };
        let (cells, defaults) = (captures.clone(), defaults.clone());
        // Its positionals, then the tuple, the parameters only a keyword passes and the dict, each where its slot is.
        let params = &self.param_slots[fi];
        let p = params.iter().take_while(|(k, _)| matches!(k, Normal)).count();
        let star = params.get(p).filter(|(k, _)| matches!(k, Star)).map(|_| p);
        let q = p + star.is_some() as usize + params[p + star.is_some() as usize..].iter().take_while(|(k, _)| matches!(k, KwOnly)).count();
        let dstar = params.get(q).filter(|(k, _)| matches!(k, DoubleStar)).map(|_| q);
        let marks = &self.functions[fi].0;
        let plain = (star.is_some() || dstar.is_some()) && params.len() == q + dstar.is_some() as usize && params.iter().enumerate().all(|(k, &(_, slot))| slot == k)
            && defaults.len() == marks.iter().filter(|m| m.ends_with('=')).count();
        let (f, body) = (&self.call_facts[fi], &self.functions[fi].1);
        if !plain || f.closure && cells.is_empty() || !self.fn_scope[fi].cellvars.is_empty() || body.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return None; }
        let pool = &self.pools[f.pool];
        let body_code = match &pool.code { Some(code) => self.plain_of(code, body, || self.fn_facts(fi))?, None => own(fi)? };
        let consts = pool.consts.as_deref()?.to_vec();
        let arity = params.len();
        let required = marks.iter().take(p).take_while(|m| !m.ends_with('=')).count();
        Some(lower::Callee { value, fi, cells, defaults, required, rare: None, reflected: false, check: 0, body: body_code, arity, names: body.names.len(), consts,
            module: self.fn_module_id(fi), params: &self.functions[fi].0, posonly: self.functions[fi].1.posonly as usize, literals: &self.functions[fi].1.constants, by_body: false, twin: false, supers: None, star, dstar, cellvars: Vec::new(), positional: p, class: Val::undef(), on_class: 0, generator: false, exc: false, bound: u16::MAX })
    }

    /* The class of the module of `method` that holds it among its members. */
    fn owner_of(&self, method: Val) -> Option<Val> {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(method) else { return None };
        self.scopes.get(self.fn_module_id(fi))?.entries().map(|(_, v)| v)
            .find(|&v| matches!(self.heap.try_get(v), Some(HeapObj::Class(_, _, m)) if m.borrow().iter().any(|(_, f)| f.0 == method.0)))
    }


    /* The `__init__` of `class` laid as the section a call of the class jumps to, for a plain class whose `__init__` never rebinds its instance and returns nothing. */
    fn ctor_section(&self, class: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        if self.native_base(class).is_some() || self.lookup_class_member(class, "__new__").is_some() { return None; }
        let (init, _) = self.lookup_class_member(class, "__init__")?;
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(init) else { return None };
        let mut callee = self.section_of(init, own)?;
        // An exception's instance takes every argument as its `args`, so each call passes them all.
        callee.exc = self.exc_base(class).is_some();
        if callee.exc && (!callee.defaults.is_empty() || callee.positional != callee.arity) { return None; }
        callee.supers = self.super_of(fi)?;
        let body = callee.body.clone();
        let none = (callee.names + callee.consts.len()) as u16;
        let writes_self = |i: &Ins| matches!(super::block::role(i), Some((true, _, _))) && i.a == 0 || i.op == OpCode::StoreTopR && i.a == 0
            || i.op == OpCode::UnpackR && [i.b, i.c].iter().take(i.x as usize - 1).any(|&r| r == 0);
        if callee.arity == 0 || body.ins.iter().any(|i| writes_self(i) || i.op == OpCode::ReturnR && i.b != none || i.op == OpCode::ReturnValue) { return None; }
        let attrs = self.ctors.get(&class.0).map(|c| c.attrs).filter(|&a| a > 0).unwrap_or_else(|| {
            let mut named: Vec<u16> = body.ins.iter().filter(|i| i.op == OpCode::SetAttrR && i.a == 0).map(|i| i.c).collect();
            named.sort_unstable();
            named.dedup();
            named.len()
        });
        (callee.value, callee.class) = (class, class);
        callee.set_ctor(Some((self.class_epoch, attrs, init)));
        Some(callee)
    }

    /* Lowers, without sections, each function of module `module` that `kinds` name and has no code yet, and those they name in turn, a few levels deep. */
    fn lower_callees(&mut self, kinds: &[Kind], module: usize, lowering: usize, depth: usize) {
        if depth > 3 { return; }
        for k in kinds {
            let Kind::Global(g) = *k else { continue };
            let Some(value) = self.scopes.get(module).map(|s| s.at(g)) else { continue };
            self.lower_callee(value, lowering, depth);
        }
    }

    /* Lowers, as `lower_callees` does, the functions and methods the calls of `code` found when they ran, and the dunders its ops found. */
    fn lower_found(&mut self, code: &Code, lowering: usize) {
        for j in 0..code.ins.len() {
            let call = matches!(code.ins[j].op, OpCode::CallR | OpCode::CallMethodR);
            let value = match code.cache.site(j) {
                super::sites::Site::Call { callee, .. } if call => callee,
                super::sites::Site::Method { func, .. } if call => Val::heap(func),
                super::sites::Site::Dunder { func, .. } => func,
                _ => continue,
            };
            self.lower_callee(value, lowering, 0);
        }
    }

    /* `lower_callees` for the function or class `value`. */
    fn lower_callee(&mut self, value: Val, lowering: usize, depth: usize) {
        // A class's call runs its `__init__`, lowered as a function is.
        let value = match self.heap.try_get(value) {
            Some(HeapObj::Class(..)) => match self.lookup_class_member(value, "__init__") { Some((init, _)) => init, None => return },
            _ => value,
        };
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(value) else { return };
        let f = self.call_facts[fi];
        if f.closure || f.pool == lowering || self.pools[f.pool].code.is_some() && !self.pools[f.pool].cold || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        let body = self.functions[fi];
        if self.pools[f.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[f.pool].consts = Some(consts); }
        if self.pools[f.pool].consts.is_none() { return; }
        self.lowered_code(&body.1, f.pool, false);
        let kinds = self.fn_scope[fi].kinds.clone();
        self.lower_callees(&kinds, self.fn_module_id(fi), lowering, depth + 1);
    }

    /* Lowers the body of the closure `value` a growth met, which `lower_callee` leaves, so the section any function of the body runs may be laid from it. */
    fn lower_closure(&mut self, value: Val, lowering: usize) {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(value) else { return };
        let f = self.call_facts[fi];
        if !f.closure || f.pool == lowering || self.pools[f.pool].code.is_some() || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        let body = self.functions[fi];
        if self.pools[f.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[f.pool].consts = Some(consts); }
        if self.pools[f.pool].consts.is_some() { self.lowered_code(&body.1, f.pool, false); }
    }

    /* Lowers, as `lower_callees` does, the bodies `chunk` makes functions of that never ran, so a call of one may lay it. */
    fn lower_made(&mut self, chunk: &SSAChunk) {
        for ins in chunk.instructions.iter().filter(|i| i.opcode == OpCode::MakeFunction) {
            let Some(desc) = chunk.functions.get(ins.operand as usize) else { continue };
            let Some(&fi) = self.body_to_fi.get(&(&desc.1 as *const SSAChunk)) else { continue };
            let f = self.call_facts[fi];
            if f.closure || !f.in_loop || self.pools[f.pool].code.as_ref().is_some_and(|c| !c.unchanged()) || desc.2 != 0 { continue; }
            let body = self.functions[fi];
            if self.pools[f.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[f.pool].consts = Some(consts); }
            if self.pools[f.pool].consts.is_none() { continue; }
            // A body that ran only unlowered, as a lambda a builtin called, is lowered now, the frames running it keeping theirs.
            if let Some(old) = self.pools[f.pool].code.take() { self.retired.push(old); }
            self.lowered_code(&body.1, f.pool, false);
        }
    }

    /* Whether the body of `fi` reads the module binding that holds it, which a call of itself does. */
    fn calls_itself(&self, fi: usize, kinds: &[Kind]) -> bool {
        let Some(scope) = self.scopes.get(self.fn_module_id(fi)) else { return false };
        kinds.iter().any(|k| matches!(*k, Kind::Global(g) if matches!(self.heap.try_get(scope.at(g)), Some(&HeapObj::Func(f, ..)) if f == fi)))
            || self.calls_own_method(fi)
    }

    /* Whether `fi`, a method a class body defines, reads its own name off an object, as `self.depth()` or `Node(n - 1).depth()` do. */
    pub(crate) fn calls_own_method(&self, fi: usize) -> bool {
        if !self.class_chunks.contains(&self.fn_definer[fi]) { return false; }
        let (Some(name), chunk) = (self.function_names.get(fi), &self.functions[fi].1) else { return false };
        chunk.instructions.iter().any(|i| i.opcode == OpCode::LoadAttr && chunk.names.get(i.operand as usize).is_some_and(|n| ssa_strip(n) == name))
    }

    /* What each slot of a chunk's frame is, and the module it binds into. */
    fn chunk_kinds(&mut self, chunk: &SSAChunk) -> (Rc<[Kind]>, usize) {
        let module = self.chunk_module_id(chunk);
        if let Some(&fi) = self.body_to_fi.get(&(chunk as *const SSAChunk)) { return (self.fn_scope[fi].kinds.clone(), module); }
        // A class body keeps its namespace in slots, its members come from them.
        if self.class_chunks.contains(&(chunk as *const SSAChunk)) { return (vec![Kind::Local; chunk.names.len()].into(), module); }
        // Only the names code reads or binds as variables take a module binding.
        let mut var = vec![false; chunk.names.len()];
        for ins in &chunk.instructions {
            if matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi | OpCode::LoadGlobal | OpCode::StoreGlobal)
                && let Some(v) = var.get_mut(ins.operand as usize) { *v = true; }
        }
        let kinds = chunk.names.iter().zip(var).map(|(name, var)| {
            let bare = ssa_strip(name);
            if !var || bare.starts_with('#') { Kind::Local } else { Kind::Global(self.scopes[module].id(bare)) }
        }).collect();
        (kinds, module)
    }

    /* The body of `code`, `chunk`'s, as a call lays it, the code itself when pure, else the body lowered without sections, made the first time one asks. */
    fn plain_of(&self, code: &Rc<Code>, chunk: &SSAChunk, facts: impl FnOnce() -> Facts) -> Option<Rc<Code>> {
        if code.pure() { return Some(code.clone()); }
        code.block.as_ref()?;
        code.plain.get_or_init(|| { let facts = facts(); lower::lower(&lower::fuse_method_calls(chunk, facts.shape), chunk, &facts, &|_| None).filter(|c| c.pure()).map(Rc::new) }).clone()
    }

    /* What the code of `chunk`, run in `pool`, holds, read once. */
    fn shape_of(&mut self, chunk: &SSAChunk, pool: usize) -> u8 {
        let p = &mut self.pools[pool];
        if p.shape == 0 { p.shape = lower::shape(&chunk.instructions); }
        p.shape
    }

    /* `lowering_facts` of the body of function `fi`, whose slots' kinds are known without the module. */
    fn fn_facts(&self, fi: usize) -> Facts {
        let chunk = &self.functions[fi].1;
        let shape = match self.pools[self.call_facts[fi].pool].shape { 0 => lower::shape(&chunk.instructions), s => s };
        self.facts_with(chunk, self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi), shape, Some(fi))
    }

    /* What a chunk's lowering may assume about its frame's slots, `fi` the function it is the body of. */
    fn lowering_facts(&mut self, chunk: &SSAChunk, pool: usize, fi: Option<usize>) -> Facts {
        // A module's code laid from the code it ran first takes the kinds that code read.
        let (kinds, module) = match (fi, self.hint.as_deref()) {
            (Some(fi), _) => (self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi)),
            (None, Some(old)) if old.kinds.len() == chunk.names.len() => (old.kinds.clone(), old.module),
            (None, _) => self.chunk_kinds(chunk),
        };
        let shape = self.shape_of(chunk, pool);
        self.facts_with(chunk, kinds, module, shape, fi)
    }

    fn facts_with(&self, chunk: &SSAChunk, kinds: Rc<[Kind]>, module: usize, shape: u8, fi: Option<usize>) -> Facts {
        let (mut bound, mut bound_far) = (0u128, Vec::new());
        if let Some(fi) = fi {
            for &(_, s) in self.param_slots[fi].iter().filter(|p| p.1 < chunk.names.len()) { if s < 128 { bound |= 1 << s; } else { bound_far.push(s); } }
        }
        // A function's body is never a class body.
        let regs = fi.is_some() || !self.class_chunks.contains(&(chunk as *const SSAChunk));
        Facts { regs, bound, bound_far, kinds, module, shape, dunders: false, noted: Vec::new(), once: fi.is_none() }
    }

    /* Lays out the window of `pool` for `code`, as long as its frame, with its constants, None, True and False past its names and the constants its sections read past their registers. */
    #[cold]
    #[inline(never)]
    pub(crate) fn grow_frame(&mut self, chunk: &SSAChunk, code: &Code, pool: usize) {
        let names = chunk.names.len();
        let consts = self.pools[pool].consts.as_deref().unwrap_or(&[]).len();
        let len = code.frame.max(names + consts + 3);
        let base = if self.pools[pool].wlen < len { self.place_window(pool, len) } else { self.pools[pool].window };
        let p = &self.pools[pool];
        let at = base + names;
        self.regs[at..at + consts].copy_from_slice(p.consts.as_deref().unwrap_or(&[]));
        self.regs[at + consts..at + consts + 3].copy_from_slice(&[Val::none(), Val::bool(true), Val::bool(false)]);
        if let Some(b) = &code.block {
            let mut from = 0;
            for &(at, n) in b.ranges.iter() { self.regs[base + at..base + at + n].copy_from_slice(&b.extra[from..from + n]); from += n; }
        }
        // A window is never laid for code as the compiler wrote it, so a call's fast entry never opens a body's first code.
        self.pools[pool].wcode = if code.unchanged() { 0 } else { code.layout as usize };
        self.note_window(pool);
        self.run_in(pool);
        let _ = base;
    }

    /* Whether the window of `pool` is laid out for `code`. */
    #[inline(always)]
    pub(crate) fn laid(&self, code: &Code, pool: usize) -> bool {
        let p = &self.pools[pool];
        // Code as the compiler wrote it reads its names from registers and nothing past them.
        p.wcode == code.layout as usize || code.unchanged() && p.window != super::window::NO_WINDOW && p.wlen >= code.frame
    }

    /* The value register `r` holds, a name read the way LoadName reads it. */
    #[inline(always)]
    pub(crate) fn reg(&self, chunk: &SSAChunk, r: u16) -> Result<Val, VmErr> {
        let v = self.regs[self.base + r as usize];
        if v.is_undef() { self.unbound(chunk, r) } else { Ok(v) }
    }

    /* What reading unbound slot `r` gives, a function local failing and any other name falling back to the builtins. */
    #[cold]
    #[inline(never)]
    pub(crate) fn unbound(&self, chunk: &SSAChunk, r: u16) -> Result<Val, VmErr> {
        if self.body_to_fi.contains_key(&(chunk as *const SSAChunk)) { return Err(self.unbound_err(chunk, r as usize)); }
        let name = chunk.names.get(r as usize).map(|n| ssa_strip(n)).unwrap_or_default();
        if self.scopes[self.chunk_module_id(chunk)].closed { return Err(VmErr::Name(name.into())); }
        self.builtin_binding(name).ok_or_else(|| VmErr::Name(name.into()))
    }

    /* The error unbound slot `s` raises, a function telling its own variables from captured ones. */
    #[cold]
    #[inline(never)]
    pub(crate) fn unbound_err(&self, chunk: &SSAChunk, s: usize) -> VmErr {
        let name = chunk.names.get(s).map(|n| ssa_strip(n)).unwrap_or_default();
        let Some(&fi) = self.body_to_fi.get(&(chunk as *const SSAChunk)) else { return VmErr::Name(name.into()) };
        if self.fn_scope[fi].freevars.iter().any(|f| f.2 == name) {
            return VmErr::Raised(crate::s!("NameError: cannot access free variable '", str name, "' where it is not associated with a value in enclosing scope"));
        }
        VmErr::Raised(crate::s!("UnboundLocalError: cannot access local variable '", str name, "' where it is not associated with a value"))
    }

    /* `a = b op c` on numbers inline, anything else through the stack opcode. */
    #[inline(always)]
    pub(crate) fn reg_binop(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk, open: Option<&Code>) -> Result<bool, VmErr> {
        self.regs[self.base + ins.a as usize] = match numeric_binop(lower::stack_form(ins.op), self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) {
            Some(v) => v,
            None => {
                let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                match self.wide_fast(lower::stack_form(ins.op), b, c)? {
                    Some(v) => v,
                    // A dunder the site learned runs as the next frame, its result going to the op's register, and code that may lay it asks for that first.
                    None if let Some(code) = open && self.op_grows(code, chunk, rip, ins) => return Ok(true),
                    None if open.is_some() && self.open_reg_dunder(ins, rip, cache)? => return Ok(false),
                    None => self.reg_stack(ins, rip, cache, chunk)?,
                }
            }
        };
        Ok(false)
    }

    /* Whether `b op c` holds, numbers compared inline. */
    #[inline(always)]
    pub(crate) fn reg_test(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if let Some(v) = numeric_binop(lower::stack_form(ins.op), self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return Ok(v.as_bool()); }
        self.reg_test_slow(ins, rip, cache, chunk)
    }

    /* The comparison the inline numbers did not settle, wide ints first and then the stack opcode, kept out of the loop. */
    #[inline(never)]
    fn reg_test_slow(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
        if let Some(v) = self.wide_fast(lower::stack_form(ins.op), b, c)? && v.is_bool() { return Ok(v.as_bool()); }
        // Its comparison hands the dunders it runs to the loop itself.
        let r = self.reg_stack(ins, rip, cache, chunk)?;
        self.truthy_op(r, chunk)
    }

    /* A register form run as its stack opcode, its result taken back. */
    #[inline(never)]
    pub(crate) fn reg_stack(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<Val, VmErr> {
        // Before the program makes a class no operand runs a dunder.
        if self.heap.users || self.redo.keep { return self.reg_stack_armed(ins, rip, cache, chunk); }
        self.reg_stack_in(ins, rip, cache, chunk)
    }

    #[inline(never)]
    fn reg_stack_armed(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<Val, VmErr> {
        // Arithmetic, comparisons and items hand their dunders to the loop themselves, the rest when an operand is a user instance or a `len` was rebound.
        let go = matches!(lower::stack_form(ins.op), OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr | OpCode::In | OpCode::NotIn | OpCode::Not | OpCode::CallLen)
            && (self.heap.user(self.regs[self.base + ins.b as usize]) || self.heap.user(self.regs[self.base + ins.c as usize]))
            || lower::stack_form(ins.op) == OpCode::CallLen && self.fused_rebound(OpCode::CallLen);
        let armed = self.redo_begin(go, rip, 0);
        let r = self.reg_stack_in(ins, rip, cache, chunk);
        self.redo_done(armed);
        r
    }

    #[inline(always)]
    fn reg_stack_in(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let op = lower::stack_form(ins.op);
        let x = self.reg(chunk, ins.b)?;
        self.push(x);
        if !matches!(op, OpCode::Minus | OpCode::Not | OpCode::CallLen) { let y = self.reg(chunk, ins.c)?; self.push(y); }
        let augmented = ins.x & lower::AUGMENTED != 0;
        match op {
            OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr => {
                let op = match (op, augmented) {
                    (OpCode::BitAnd, true) => OpCode::InPlaceBitAnd,
                    (OpCode::BitOr, true) => OpCode::InPlaceBitOr,
                    (OpCode::BitXor, true) => OpCode::InPlaceBitXor,
                    (op, _) => op,
                };
                self.handle_bitwise(op, (ins.x & !lower::AUGMENTED) as u16, chunk)?;
            }
            OpCode::In | OpCode::NotIn | OpCode::Is | OpCode::IsNot => self.handle_identity(op, chunk)?,
            OpCode::Not => self.handle_logic(op, chunk)?,
            OpCode::GetItem => self.get_item_op(rip, chunk, cache, false)?,
            OpCode::CallLen => self.handle_function(op, 1, chunk)?,
            _ => self.exec_arith_or_compare(op, ins.x as u16, rip, cache, chunk, false)?,
        }
        self.pop()
    }

    /* Whether register `r` is true, a user `__bool__` or `__len__` included. */
    #[inline(always)]
    pub(crate) fn reg_truthy(&mut self, chunk: &SSAChunk, r: u16, rip: usize) -> Result<bool, VmErr> {
        let v = self.regs[self.base + r as usize];
        if v.is_bool() { return Ok(v.as_bool()); }
        if v.is_int() { return Ok(v.as_int() != 0); }
        let v = self.reg(chunk, r)?;
        self.truth_reg(v, rip, chunk)
    }

    /* `truth_at` of a register's value, kept out of the register loop. */
    #[inline(never)]
    fn truth_reg(&mut self, v: Val, rip: usize, chunk: &SSAChunk) -> Result<bool, VmErr> { self.truth_at(v, rip, 0, chunk) }

    /* `truthy_op` of `v` for the op at `rip`, its `n` operands on the stack, an instance's dunder handed to the loop. */
    #[inline(always)]
    pub(crate) fn truth_at(&mut self, v: Val, rip: usize, n: usize, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if self.instance_class(v).is_none() { return Ok(self.truthy(v)); }
        self.truth_redone(v, rip, n, chunk)
    }

    #[inline(never)]
    fn truth_redone(&mut self, v: Val, rip: usize, n: usize, chunk: &SSAChunk) -> Result<bool, VmErr> {
        self.redone_if(true, rip, n, |vm| vm.truthy_op(v, chunk))
    }

    /* Pushes the `ins.x` registers an instruction names, deepest first. */
    #[inline(always)]
    pub(crate) fn push_regs(&mut self, ins: Ins, chunk: &SSAChunk) -> Result<(), VmErr> {
        for &r in [ins.a, ins.b, ins.c].iter().take(ins.x as usize) {
            let v = self.reg(chunk, r)?;
            self.push(v);
        }
        Ok(())
    }
}

/* The int math register forms keep inline, None when the handler must answer. */
pub(crate) fn int_add(a: i64, b: i64) -> Option<Val> { Val::int_checked(a + b) }
pub(crate) fn int_sub(a: i64, b: i64) -> Option<Val> { Val::int_checked(a - b) }
pub(crate) fn int_mul(a: i64, b: i64) -> Option<Val> { mul_exact(a, b).and_then(Val::int_checked) }
pub(crate) fn int_mod(a: i64, b: i64) -> Option<Val> {
    if b == 0 { return None; }
    let r = a % b;
    Some(Val::int(if r != 0 && (r < 0) != (b < 0) { r + b } else { r }))
}

/* `a * b` when exact, 32-bit operands skipping wasm's 128-bit overflow routine. */
#[inline(always)]
pub(crate) fn mul_exact(a: i64, b: i64) -> Option<i64> {
    if a as i32 as i64 == a && b as i32 as i64 == b { Some(a * b) } else { a.checked_mul(b) }
}
pub(crate) fn int_div(a: i64, b: i64) -> Option<Val> { (b != 0).then(|| Val::float(a as f64 / b as f64)) }
pub(crate) fn int_floordiv(a: i64, b: i64) -> Option<Val> {
    if b == 0 { return None; }
    let (q, r) = (a / b, a % b);
    Some(Val::int(if r != 0 && (r < 0) != (b < 0) { q - 1 } else { q }))
}
pub(crate) fn float_div(a: f64, b: f64) -> Option<Val> { (b != 0.0).then(|| Val::float(a / b)) }

/* A bitwise op on two inline ints, None when a shift overflows them. */
#[inline(always)]
pub(crate) fn int_bits(op: OpCode, a: i64, b: i64) -> Option<Val> {
    match op {
        OpCode::BitAndR => Some(Val::int(a & b)),
        OpCode::BitOrR => Some(Val::int(a | b)),
        OpCode::BitXorR => Some(Val::int(a ^ b)),
        OpCode::ShlR if (0..48).contains(&b) => i64::try_from((a as i128) << b).ok().and_then(Val::int_checked),
        OpCode::ShrR if b >= 0 => Some(Val::int(a >> b.min(63))),
        _ => None,
    }
}

/* `a ** b` for small ints, None for a negative exponent or an overflow. */
fn int_pow(a: i64, b: i64) -> Option<Val> {
    if !(0..64).contains(&b) { return None; }
    let (mut result, mut base, mut e) = (1i64, a, b);
    while e > 0 {
        if e & 1 == 1 { result = mul_exact(result, base)?; }
        e >>= 1;
        if e > 0 { base = mul_exact(base, base)?; }
    }
    Val::int_checked(result)
}

/* `a ** b` for floats, None for a zero base under a negative power, a complex result or an overflow. */
fn float_pow(a: f64, b: f64) -> Option<Val> {
    if a == 0.0 && b < 0.0 || crate::value::math::complex_power(a, b) { return None; }
    crate::value::math::fpowf(a, b).map(Val::float)
}

/* `x op y` for two numbers as the register forms compute it, else None. */
#[inline(always)]
pub(crate) fn numeric_binop(op: OpCode, x: Val, y: Val) -> Option<Val> {
    if !numeric(x) || !numeric(y) { return None; }
    let ints = x.is_int() && y.is_int();
    let (a, b, f, g) = (x.as_int(), y.as_int(), as_f64(x), as_f64(y));
    let test = |i: bool, fl: bool| Some(Val::bool(if ints { i } else { fl }));
    match op {
        OpCode::Add | OpCode::InPlaceAdd => if ints { int_add(a, b) } else { Some(Val::float(f + g)) },
        OpCode::Sub | OpCode::InPlaceSub => if ints { int_sub(a, b) } else { Some(Val::float(f - g)) },
        OpCode::Mul => if ints { int_mul(a, b) } else { Some(Val::float(f * g)) },
        OpCode::Div => if ints { int_div(a, b) } else { float_div(f, g) },
        OpCode::Pow => if ints { int_pow(a, b) } else { float_pow(f, g) },
        OpCode::Mod if ints => int_mod(a, b),
        OpCode::FloorDiv if ints => int_floordiv(a, b),
        OpCode::Eq => test(a == b, f == g),
        OpCode::NotEq => test(a != b, f != g),
        OpCode::Lt => test(a < b, f < g),
        OpCode::LtEq => test(a <= b, f <= g),
        OpCode::Gt => test(a > b, f > g),
        OpCode::GtEq => test(a >= b, f >= g),
        _ => None,
    }
}

