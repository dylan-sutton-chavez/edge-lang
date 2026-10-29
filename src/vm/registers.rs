use alloc::{rc::Rc, vec, vec::Vec};

use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::syntax::ssa::ssa_strip;

use super::VM;
use super::act::Dest;
use super::lower;
use super::lower::{Code, Facts};
use crate::code::Ins;
use super::scope::Kind;
use crate::value::*;
use crate::lang::Policy;
use super::resolve::{callee_at, method_at, site_func};

/* the kind of a `grown` entry naming a function the lowering before it laid */
pub(crate) const LAID_ENTRY: i64 = -3;

/* the number a register holds as a float with an int widened */
#[inline(always)]
fn as_f64(v: Val) -> f64 { if v.is_int() { v.as_int() as f64 } else { v.as_float() } }

#[inline(always)]
fn numeric(v: Val) -> bool { v.is_int() || v.is_float() }

impl<'a> VM<'a> {
    /* the code a frame of `chunk` runs and whether the pool never keeps it */
    #[inline(always)]
    pub(crate) fn frame_code(&mut self, chunk: &SSAChunk, pool: usize) -> (Rc<Code>, bool) {
        // a suspended run resumes the code it parked in though its pool laid another since
        let layout = core::mem::take(&mut self.resume_layout);
        if layout != 0 && self.resume_ip != 0
            && let Some(code) = self.pools[pool].code.iter().chain(&self.retired).find(|c| c.layout == layout) { return (code.clone(), false); }
        // a run parked in its first code stays there until its resumes heat it
        if self.pools[pool].cold && self.resume_ip != 0 {
            self.pools[pool].heat += 1;
            if self.pools[pool].heat >= lower::HOT_LOOP { return (self.lowered_code(chunk, pool, false), false); }
        }
        let p = &self.pools[pool];
        match &p.code {
            // a body that suspends keeps the code its saved state names resume points in
            Some(code) if p.laid || p.calls < lower::TIER && self.resume_ip & crate::vm::frame::RESUME_RAW == 0 || chunk.is_generator || self.fi_of(chunk).is_some_and(|fi| self.is_async.get(fi).copied().unwrap_or(false)) => (code.clone(), false),
            Some(_) => (self.lowered_code(chunk, pool, true), false),
            None => self.first_code(chunk, pool),
        }
    }

    /* code grown for the head at `at` or none when the growth lays nothing new */
    pub(crate) fn grown_code(&mut self, chunk: &SSAChunk, pool: usize, at: usize, kind: lower::Grow, func: Val) -> Option<Rc<Code>> {
        let before = self.pools[pool].code.clone()?;
        let code = self.lowered_or_grown(chunk, pool, true, Some((at, kind, func)));
        (!Rc::ptr_eq(&code, &before)).then_some(code)
    }

    /* remakes the pool's code by its lowering then each logged growth for a mid-section restore */
    pub(crate) fn replayed(&mut self, chunk: &SSAChunk, pool: usize) -> Option<Rc<Code>> {
        let fi = (self.pools[pool].func != super::cache::NO_FN).then_some(self.pools[pool].func as usize);
        let key = self.code_key(chunk, fi);
        let log: Vec<[Val; 4]> = self.grown.chunks(4).filter(|e| e[0].as_int() == key).map(|e| [e[0], e[1], e[2], e[3]]).collect();
        // each lowering's entry with the functions it laid in the entries after it
        let mut steps: Vec<([Val; 4], Vec<usize>)> = Vec::new();
        for e in log {
            if e[2].as_int() != LAID_ENTRY { steps.push((e, Vec::new())); continue; }
            if let Some(s) = steps.last_mut() && let Ok(fi) = usize::try_from(e[1].as_int()) { s.1.push(fi); }
        }
        let ((base, base_laid), rest) = steps.split_first()?;
        if base[2].as_int() >= 0 { return None; }
        let saved = core::mem::take(&mut self.learned);
        let upto = |n: i64| saved[..usize::try_from(n).unwrap_or(0).saturating_mul(3).min(saved.len())].to_vec();
        self.replaying = true;
        self.learned = upto(base[1].as_int());
        if let Some(old) = self.pools[pool].code.take() { self.retired.push(old); }
        self.pools[pool].cold = false;
        self.lay_as_logged(base_laid.clone(), pool);
        // a module's code lowered past what it ran before lowers from the same point again
        self.lower_from = if base[3].is_int() { base[3].as_int() as usize } else { 0 };
        let mut code = self.lowered_or_grown(chunk, pool, base[2].as_int() == -1, None);
        self.lower_from = 0;
        for (g, laid) in rest {
            self.learned = upto(g[1].as_int());
            self.lay_as_logged(laid.clone(), pool);
            let Some(&kind) = lower::Grow::ALL.get((g[2].as_int() & 0xFF) as usize).filter(|_| g[2].as_int() >= 0) else { break };
            let at = (g[2].as_int() >> 8) as usize;
            // every growth but a whole one names a head of the code it grows
            if kind != lower::Grow::All && !code.ins.get(at).is_some_and(|i| lower::section_call(i.op) && at + lower::HEADER as usize <= code.ins.len()) { break; }
            // a binding's growth lays what the binding held then
            self.replay_global = (kind == lower::Grow::Global).then(|| code.block.as_deref().and_then(|b| b.sections.get(code.ins.get(at + 2)?.c as usize)).map(|s| (s.bound, s.global, g[3]))).flatten();
            if let Some(c) = self.grown_code(chunk, pool, at, kind, g[3]) { code = c; }
            self.replay_global = None;
        }
        (self.learned, self.replaying, self.replay_laid) = (saved, false, None);
        Some(code)
    }

    /* readies a replayed lowering to lay the functions `laid` names and no other */
    fn lay_as_logged(&mut self, laid: Vec<usize>, pool: usize) {
        for &fi in &laid { self.lower_laid(fi, pool); }
        self.replay_laid = Some(laid);
    }

    /* never-run `fi` gets its unlowered body so its first call runs as a loop frame */
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
        // its second call lowers it as a nested one would
        if self.pools[pool].cold {
            // a body lent before it ran makes this call its first
            if !self.pools[pool].ran { let p = &mut self.pools[pool]; (p.ran, p.heat) = (true, p.heat + 1); return; }
            self.first_code(chunk, pool);
            return;
        }
        if self.pools[pool].code.is_some() { return; }
        if chunk.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        if self.pools[pool].consts.is_none() { return; }
        let f = self.functions[fi];
        let code = self.first_identity(&lower::fuse_method_calls(&f.1, self.pools[pool].shape), &f.1, pool, self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi));
        let p = &mut self.pools[pool];
        (p.code, p.cold, p.ran, p.heat) = (Some(code), true, true, p.heat + 1);
    }

    /* the code a chunk without lowered code runs which stays unlowered until a loop heats */
    fn first_code(&mut self, chunk: &SSAChunk, pool: usize) -> (Rc<Code>, bool) {
        if cfg!(feature = "coverage") {
            let (kinds, module) = self.chunk_kinds(chunk);
            if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
            let code = self.first_identity(&chunk.instructions, chunk, pool, kinds, module);
            self.pools[pool].code = Some(code.clone());
            return (code, false);
        }
        // a first call or a cold resume runs unlowered until a loop heats
        if self.resume_ip & crate::vm::frame::RESUME_RAW != 0 { return (self.replayed(chunk, pool).unwrap_or_else(|| self.lowered_code(chunk, pool, true)), false); }
        let fi = (self.pools[pool].func != super::cache::NO_FN).then_some(self.pools[pool].func as usize);
        let p = &mut self.pools[pool];
        let cold = if self.resume_ip == 0 { !core::mem::replace(&mut p.ran, true) } else { p.heat < lower::HOT_LOOP };
        if cold {
            self.pools[pool].heat += 1;
            let (kinds, module) = self.chunk_kinds(chunk);
            return (self.first_identity(&lower::fuse_method_calls(chunk, self.pools[pool].shape), chunk, pool, kinds, module), true);
        }
        // a heated generator with calls lays at once since it never suspends inside a section
        let generator = chunk.is_generator && fi.is_some_and(|fi| !self.is_async.get(fi).copied().unwrap_or(false))
            && chunk.instructions.iter().any(|i| matches!(i.opcode, OpCode::Call | OpCode::CallMethodArgs));
        let laid = generator || self.pools[pool].calls >= lower::TIER || self.was_laid(chunk, fi);
        (self.lowered_code(chunk, pool, laid), false)
    }

    /* the chunk's lowered code made once and shared by every later frame */
    pub(crate) fn lowered_code(&mut self, chunk: &SSAChunk, pool: usize, laid: bool) -> Rc<Code> { self.lowered_or_grown(chunk, pool, laid, None) }

    /* `lowered_code` or with `grow` the pool's code grown as the head it names asks */
    fn lowered_or_grown(&mut self, chunk: &SSAChunk, pool: usize, laid: bool, grow: Option<(usize, lower::Grow, Val)>) -> Rc<Code> {
        let alone = core::mem::take(&mut self.pools[pool].whole);
        let p = &self.pools[pool];
        if grow.is_none() && let Some(code) = &p.code && !p.cold && (p.laid || !laid) { return code.clone(); }
        // sectionless code frames still run grows heads past its end else whole lowering is cheaper
        let (grow, whole) = if grow.is_none() && laid && p.active > 0 && !alone && p.code.as_ref().is_some_and(|c| c.block.is_none() && !c.unchanged() && !p.cold) { (Some((0, lower::Grow::All, Val::undef())), true) } else { (grow, false) };
        let first = self.pools[pool].cold;
        let before = self.pools[pool].code.take();
        let p = &mut self.pools[pool];
        let gone = core::mem::replace(&mut p.cold, false) && p.active == 0;
        if let Some(old) = &before && !gone { self.retired.push(old.clone()); }
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
            .any(|w| w[0].opcode == OpCode::Call && w[1].opcode == OpCode::GetIter && w[1].operand != crate::syntax::parser::ITER_VALUE && w[2].opcode == OpCode::ForIter);
        if laid && !light { self.lower_callees(&facts.kinds, facts.module, pool, 0); }
        if let Some((_, kind, func)) = grow && kind != lower::Grow::All { self.lower_callee(func, pool, 0); self.lower_closure(func, pool); }
        if laid {
            let noted: Vec<(usize, Val)> = self.learned.chunks(3).filter_map(|t| match self.heap.try_get(t[2]) { Some(&HeapObj::Func { fi, .. }) => Some((fi, t[2])), _ => None }).collect();
            for (fi, f) in noted {
                let p = self.call_facts[fi].pool;
                if self.pools[p].consts.is_none() && let Ok(consts) = super::cache::const_vals(&self.functions[fi].1, &mut self.heap) { self.pools[p].consts = Some(consts); }
                // a replay lays what the saved run had lowered by then closures included
                if self.replaying { self.lower_callee(f, pool, 0); self.lower_closure(f, pool); }
            }
        }
        if laid && !light && let Some(o) = before.clone().or_else(|| self.hint.clone()) { self.lower_found(&o, pool); }
        if steps && !light { self.lower_made(chunk); }
        let me = fi.filter(|&fi| self.calls_itself(fi, &facts.kinds));
        facts.recurs = me.is_some();
        if laid && fi.is_some() && self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        let old = before.clone().or_else(|| self.hint.clone());
        let key = self.code_key(chunk, fi);
        let noted: Vec<(u32, Val)> = self.learned.chunks(3).filter(|t| t[0].as_int() == key).map(|t| (t[1].as_int() as u32, t[2])).collect();
        let used = core::cell::RefCell::new(Vec::new());
        if laid && !first && facts.shape & lower::CALLS == 0 && let Some(code) = before.as_ref().filter(|c| c.block.is_none() && !c.unchanged()) {
            (self.pools[pool].code, self.pools[pool].laid) = (Some(code.clone()), true);
            return code.clone();
        }
        let gens = if steps && !light { self.gen_bodies(&facts.kinds, facts.module) } else { Vec::new() };
        if laid && !light { self.gen_consts(facts.module); }
        let lazy: core::cell::RefCell<Vec<(u64, usize, Rc<Code>)>> = core::cell::RefCell::new(Vec::new());
        let made: core::cell::OnceCell<Option<Rc<Code>>> = core::cell::OnceCell::new();
        let plain = || made.get_or_init(|| if !laid { None } else if let Some(old) = before.as_ref().and_then(|c| self.plain_of(c, chunk, || self.facts_with(chunk, facts.kinds.clone(), facts.module, facts.shape, fi))) { Some(old) }
            // laid fresh or from its first code it lays its own body for calls back
            else { fi.filter(|_| me.is_some() || old.as_deref().is_none_or(Code::unchanged)).and_then(|_| lower::lower(&src, chunk, &facts, &|_| None)).filter(|c| c.pure()).map(Rc::new) }).clone();
        let own = |f: usize| if fi != Some(f) { None } else if me.is_some() { plain() } else { old.clone().filter(|c| c.block.is_none() && c.pure()).or_else(plain) };
        let found = |w: lower::Want| if !laid { None } else {
            match w {
                // a generator a module binding holds for the loop that steps it
                lower::Want::Gen(m, g) => {
                    let value = self.scopes.get(m)?.at(g as u32);
                    if let Some(&(_, fi, ref body)) = gens.iter().chain(lazy.borrow().iter()).find(|b| b.0 == value.0) { return self.gen_section_of(value, fi, body.clone()); }
                    let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(value) else { return None };
                    let body = self.gen_body(fi)?;
                    lazy.borrow_mut().push((value.0, fi, body.clone()));
                    self.gen_section_of(value, fi, body)
                }
                // the generator a register call made for the loop over what it gave
                lower::Want::GenAt(at) => {
                    let value = noted.iter().find(|n| n.0 == at | lower::GEN)?.1;
                    let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(value) else { return None };
                    let body = self.gen_body(fi)?;
                    self.gen_section_of(value, fi, body)
                }
                // a generator another loop's head met for that loop
                lower::Want::GenFunc(value) => {
                    if let Some(&(_, fi, ref body)) = gens.iter().chain(lazy.borrow().iter()).find(|b| b.0 == value.0) { return self.gen_section_of(value, fi, body.clone()); }
                    let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(value) else { return None };
                    let body = self.gen_body(fi)?;
                    lazy.borrow_mut().push((value.0, fi, body.clone()));
                    self.gen_section_of(value, fi, body)
                }
                lower::Want::Global(m, g) => {
                    let value = match self.replay_global { Some((rm, rg, f)) if rm == m && rg == g => f, _ => self.scopes.get(m)?.at(g as u32) };
                    let mut callee = self.section_of(value, &own)?;
                    if callee.module != m { callee.bound = m as u16; }
                    // bindings often take closures in turn like a loop variable so all run the twin
                    callee.twin = callee.ctor().is_none() && (!callee.cells.is_empty() || self.remade(callee.fi));
                    Some(callee)
                }
                // a bound method a module binding holds
                lower::Want::Held(m, g) => match self.heap.try_get(self.scopes.get(m)?.at(g as u32)) {
                    Some(&HeapObj::Bound { func, .. }) if func.is_heap() => self.bound_method(func, &own),
                    _ => None,
                },
                // only a function taking `*args` or `**kwargs` is laid for a call packing its arguments
                lower::Want::Spread(m, g) => {
                    let value = self.scopes.get(m)?.at(g as u32);
                    let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(value) else { return None };
                    if self.call_facts[fi].arity.is_some() { return None; }
                    let mut callee = self.section_of(value, &own)?;
                    if callee.module != m { callee.bound = m as u16; }
                    Some(callee)
                }
                lower::Want::Method(at) => {
                    // a function the receiver held by the name is called with the arguments alone
                    if let Some(f) = noted.iter().find(|n| n.0 == at | lower::FIELD).map(|n| n.1) { return self.on_field(f, &own); }
                    let site = old.as_deref().and_then(|o| method_at(o, at));
                    let func = noted.iter().find(|n| n.0 == at).map(|n| n.1).or_else(|| site.and_then(site_func))?;
                    if self.heap.is_class(func) { return self.on_class(func, chunk.names.get(src.get(at as usize)?.operand as usize)?, &own); }
                    if !matches!(self.heap.try_get(func), Some(HeapObj::Func { .. })) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.set_site(site.filter(|s| site_func(*s).is_some_and(|f| f.0 == func.0)));
                    used.borrow_mut().push((key, at, func));
                    Some(callee)
                }
                lower::Want::Value(at) => {
                    let Some(func) = noted.iter().find(|n| n.0 == at).map(|n| n.1).or_else(|| old.as_deref().and_then(|o| callee_at(o, at))) else {
                        return noted.iter().find(|n| n.0 == at | lower::BOUND).and_then(|n| self.bound_method(n.1, &own));
                    };
                    let mut callee = self.section_of(func, &own)?;
                    callee.by_body = !callee.defaults.is_empty();
                    callee.twin = !callee.by_body && (!callee.cells.is_empty() || self.remade(callee.fi));
                    used.borrow_mut().push((key, at, func));
                    Some(callee)
                }
                // other functions a register call met noted first and the one its site found last
                lower::Want::Alt(nfi, at, j) => {
                    let mut seen: Vec<Val> = match nfi {
                        Some(nfi) => self.learned.chunks(3).filter(|t| t[0].as_int() == nfi as i64 && t[1].as_int() == at as i64).map(|t| t[2]).collect(),
                        None => noted.iter().filter(|n| n.0 == at).map(|n| n.1).collect(),
                    };
                    if nfi.is_none() && let Some(f) = old.as_deref().and_then(|o| callee_at(o, at)) && !seen.iter().any(|v| v.0 == f.0) { seen.push(f); }
                    let func = *seen.get(j as usize)?;
                    let mut callee = self.section_of(func, &own)?;
                    callee.twin = false;
                    used.borrow_mut().push((nfi.map_or(key, |f| f as i64), at, func));
                    Some(callee)
                }
                // the function a closure's cell holds which a call in its section's body makes
                lower::Want::Celled(cell) => {
                    let Some(&HeapObj::Cell(func)) = self.heap.try_get(cell) else { return None };
                    // one its `def` remakes is another function each call so the section's check refuses it
                    let Some(&HeapObj::Func { fi: cfi, .. }) = self.heap.try_get(func) else { return None };
                    if self.remade(cfi) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.twin = false;
                    Some(callee)
                }
                lower::Want::Func(func, method) => {
                    // a class runs its constructor and a bound method its function with the receiver first
                    if !method && self.heap.is_class(func) { return self.section_of(func, &own); }
                    if !method && let Some((_, f)) = self.heap.bound_of(func) && f.is_heap() { return self.bound_method(f, &own); }
                    if !matches!(self.heap.try_get(func), Some(HeapObj::Func { .. })) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.twin = false;
                    Some(callee)
                }
                // each other method a method call met noted under its enclosing function or this code
                lower::Want::MethodAlt(nfi, at, j) => {
                    let nkey = nfi.map_or(key, |f| f as i64);
                    let func = self.learned.chunks(3).filter(|t| t[0].as_int() == nkey && t[1].as_int() == at as i64).map(|t| t[2]).nth(j as usize)?;
                    // a function a receiver held by the name takes the arguments alone
                    if at & lower::FIELD != 0 { return self.on_field(func, &own); }
                    if at & lower::STATIC != 0 { return self.on_class_found(func, &own); }
                    if !matches!(self.heap.try_get(func), Some(HeapObj::Func { .. })) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.twin = false;
                    Some(callee)
                }
                // a method a call inside a laid function found and noted under that function
                lower::Want::Nested(nfi, at, seen, name) => {
                    let nkey = nfi as i64;
                    if let Some(f) = self.learned.chunks(3).find(|t| t[0].as_int() == nkey && t[1].as_int() == (at | lower::FIELD) as i64).map(|t| t[2]) { return self.on_field(f, &own); }
                    let func = self.learned.chunks(3).find(|t| t[0].as_int() == nkey && t[1].as_int() == at as i64).map(|t| t[2]).or_else(|| seen.and_then(site_func))?;
                    if self.heap.is_class(func) {
                        let body = &self.functions[nfi].1;
                        return self.on_class(func, body.names.get(name as usize)?, &own);
                    }
                    if !matches!(self.heap.try_get(func), Some(HeapObj::Func { .. })) { return None; }
                    let mut callee = self.section_of(func, &own)?;
                    callee.set_site(seen.filter(|s| site_func(*s).is_some_and(|f| f.0 == func.0)));
                    used.borrow_mut().push((nkey, at, func));
                    Some(callee)
                }
                // any function of a body the code makes called straight from where it was made
                lower::Want::Body(body) => self.body_section(body),
                // the body of a remade function so the section runs any function made from it
                lower::Want::BodyOf(func) => {
                    let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(func) else { return None };
                    if !self.remade(fi) { return None; }
                    self.body_section(self.functions[fi].1.id.get())
                }
                // a generator of a body the code made and called just before
                lower::Want::GenBody(body) => {
                    let fi = self.fi_of_id(body)?;
                    if self.call_facts[fi].closure { return None; }
                    let body = self.gen_body(fi)?;
                    self.gen_section_of(Val::undef(), fi, body)
                }
                lower::Want::Made(body) => self.made_section(body),
                lower::Want::MadeIn(fi, k) => self.functions.get(fi).and_then(|f| f.1.functions.get(k as usize)).filter(|f| super::scope::is_comp(&f.0)).and_then(|f| self.made_section(f.1.id.get())),
                lower::Want::NestedValue(nfi, at, seen) => {
                    let nkey = nfi as i64;
                    let (mut first, mut bound) = (None, None);
                    for t in self.learned.chunks(3).filter(|t| t[0].as_int() == nkey) {
                        let a = t[1].as_int() as u32;
                        if a == at { first = first.or(Some(t[2])); } else if a == at | lower::BOUND { bound = Some(t[2]); }
                    }
                    let func = first.or(seen).filter(|&f| matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })) || self.heap.is_class(f));
                    let Some(func) = func else { return bound.and_then(|f| self.bound_method(f, &own)) };
                    // a function noted as a bound method's lays as that bound method
                    if bound.is_some_and(|f| f.0 == func.0) { return self.bound_method(func, &own); }
                    let mut callee = self.section_of(func, &own)?;
                    callee.by_body = !callee.defaults.is_empty();
                    callee.twin = !callee.by_body && (!callee.cells.is_empty() || self.remade(callee.fi));
                    used.borrow_mut().push((nkey, at, func));
                    Some(callee)
                }
            }
        };
        // a replay lays only the functions the lowering it remakes laid
        let sections = |w: lower::Want| found(w).filter(|c| self.replay_laid.as_deref().is_none_or(|laid| laid.contains(&c.fi)));
        let mut fell = false;
        let mut code = match grow {
            None => match lower::lower(&src, chunk, &facts, &sections) {
                Some(code) => code,
                // code the lowering refuses runs on as the compiler wrote it
                None => return self.unlowered(chunk, pool, before, laid),
            },
            Some((at, kind, func)) => {
                let grown = before.as_deref().and_then(|old| lower::extend(old, &src, chunk, &facts, &sections, at, kind, func))
                    .or_else(|| { fell = whole; whole.then(|| lower::lower(&src, chunk, &facts, &sections)).flatten() });
                // a growth that lays nothing new leaves the code as it was
                let Some(code) = grown else { return self.ungrown(chunk, pool, before, laid) };
                code
            }
        };
        // only code keeping every value in registers lowers and the rest runs as compiled
        if !super::rows::unstacked(&code) {
            #[cfg(debug_assertions)]
            self.note_stack_work(&code);
            return if grow.is_none() || fell { self.unlowered(chunk, pool, before, laid) } else { self.ungrown(chunk, pool, before, laid) };
        }
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
                    // a module lowered past what it ran logs where so a restore lowers it alike
                    if code.block.is_some() || self.lower_from != 0 {
                        self.grown.extend_from_slice(&[Val::int(key), Val::int(read as i64), Val::int(-1), Val::int(self.lower_from as i64)]);
                        self.note_laid(key, &code);
                    }
                }
            }
            Some((at, kind, func)) if !self.replaying => {
                if !self.grown.chunks(4).any(|e| e[0].as_int() == key) {
                    let base = before.as_deref().and_then(|c| c.block.as_deref());
                    self.grown.extend_from_slice(&[Val::int(key), Val::int(base.map_or(0, |_| before.as_deref().map_or(0, |c| c.read.get()) as i64)), Val::int(if base.is_some() { -1 } else { -2 }), Val::undef()]);
                    if let Some(old) = before.as_deref() { self.note_laid(key, old); }
                }
                self.grown.extend_from_slice(&[Val::int(key), Val::int(read as i64), Val::int((at as i64) << 8 | kind as i64), func]);
                self.note_laid(key, &code);
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

    /* logs after its entry each function `code`'s sections came from for a replay */
    fn note_laid(&mut self, key: i64, code: &Code) {
        let Some(b) = code.block.as_deref() else { return };
        // a function laid in several sections is logged once
        let mut seen = vec![0u64; self.functions.len().div_ceil(64)];
        for s in b.sections.iter() {
            let Some(w) = seen.get_mut(s.fi / 64).filter(|w| **w & 1 << (s.fi % 64) == 0) else { continue };
            *w |= 1 << (s.fi % 64);
            self.grown.extend_from_slice(&[Val::int(key), Val::int(s.fi as i64), Val::int(LAID_ENTRY), Val::undef()]);
        }
    }

    /* gives function `fi` the body a replayed lowering lays as the saved run had it */
    fn lower_laid(&mut self, fi: usize, lowering: usize) {
        let Some(&body) = self.functions.get(fi) else { return };
        let pool = self.call_facts[fi].pool;
        let p = &self.pools[pool];
        if pool == lowering || p.code.as_ref().is_some_and(|c| !p.cold || c.plain.get().is_some()) || body.1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        if self.pools[pool].consts.is_some() && !self.lend(fi, pool, &body.1, false) { self.lowered_code(&body.1, pool, false); }
    }

    /* the code `pool` had before a growth laying nothing new or the compiler's if none */
    #[cold]
    #[inline(never)]
    fn ungrown(&mut self, chunk: &SSAChunk, pool: usize, before: Option<Rc<Code>>, laid: bool) -> Rc<Code> {
        if let Some(old) = &before && self.retired.last().is_some_and(|r| Rc::ptr_eq(r, old)) { self.retired.pop(); }
        self.pools[pool].code = before.clone();
        match before { Some(code) => code, None => self.unlowered(chunk, pool, None, laid) }
    }

    /* the compiler's code of `chunk` in `pool` once its lowering refused */
    #[cold]
    #[inline(never)]
    fn unlowered(&mut self, chunk: &SSAChunk, pool: usize, before: Option<Rc<Code>>, laid: bool) -> Rc<Code> {
        let code = match before.filter(|c| c.unchanged()) {
            Some(c) => c,
            None => {
                let (kinds, module) = self.chunk_kinds(chunk);
                self.first_identity(&lower::fuse_method_calls(chunk, self.pools[pool].shape), chunk, pool, kinds, module)
            }
        };
        (self.pools[pool].code, self.pools[pool].laid) = (Some(code.clone()), laid);
        code
    }

    /* what each slot of a chunk's frame is and the module it binds into */
    pub(crate) fn chunk_kinds(&mut self, chunk: &SSAChunk) -> (Rc<[Kind]>, usize) {
        let module = self.chunk_module_id(chunk);
        if let Some(fi) = self.fi_of(chunk) { return (self.fn_scope[fi].kinds.clone(), module); }
        // a class body keeps its namespace in slots and its members come from them
        if self.is_class_body(chunk) { return (vec![Kind::Local; chunk.names.len()].into(), module); }
        // only the names code reads or binds as variables take a module binding
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

    /* a call lays pure `code` itself or else its sectionless lowering made when first asked */
    pub(crate) fn plain_of(&self, code: &Rc<Code>, chunk: &SSAChunk, facts: impl FnOnce() -> Facts) -> Option<Rc<Code>> {
        if code.pure() { return Some(code.clone()); }
        // first code holds only the body lent to its callers
        if code.unchanged() { return code.plain.get().cloned().flatten(); }
        code.block.as_ref()?;
        code.plain.get_or_init(|| { let facts = facts(); lower::lower(&lower::fuse_method_calls(chunk, facts.shape), chunk, &facts, &|_| None).filter(|c| c.pure()).map(Rc::new) }).clone()
    }

    /* what the code of `chunk` in `pool` holds worked out once and kept */
    fn shape_of(&mut self, chunk: &SSAChunk, pool: usize) -> u8 {
        let p = &mut self.pools[pool];
        if p.shape == 0 { p.shape = lower::shape(&chunk.instructions); }
        p.shape
    }

    /* `lowering_facts` of function `fi`'s body whose slot kinds are known without the module */
    pub(crate) fn fn_facts(&self, fi: usize) -> Facts {
        let chunk = &self.functions[fi].1;
        let shape = match self.pools[self.call_facts[fi].pool].shape { 0 => lower::shape(&chunk.instructions), s => s };
        self.facts_with(chunk, self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi), shape, Some(fi))
    }

    /* what a chunk's lowering may assume about its frame's slots given `fi` as its function */
    pub(crate) fn lowering_facts(&mut self, chunk: &SSAChunk, pool: usize, fi: Option<usize>) -> Facts {
        // module code laid from the code it first ran takes the kinds that code read
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
        // a function's body is never a class body
        let regs = fi.is_some() || !self.is_class_body(chunk);
        let rebound = if self.any_rebound != 0 { self.bound_keys(module) } else { Vec::new() };
        Facts { regs, bound, bound_far, kinds, module, shape, once: fi.is_none(), from: if fi.is_none() { self.lower_from } else { 0 }, recurs: false, natives: self.natives, rebound }
    }

    /* each native module `m` rebinds with the slot of its binding */
    #[cold]
    #[inline(never)]
    pub(crate) fn bound_keys(&self, m: usize) -> Vec<(u16, u16)> {
        self.scopes.get(m).map_or_else(Vec::new, |s| s.rebound_names(|id| super::run::rebound_bit(&self.rebound, id)))
    }

    /* `grow_frame` for the code of `pool` cloned so no borrow of the vm stays out */
    #[cold]
    #[inline(never)]
    pub(crate) fn grow_pool(&mut self, chunk: &SSAChunk, pool: usize) {
        if let Some(code) = self.pools[pool].code.clone() { self.grow_frame(chunk, &code, pool); }
    }

    /* lays the frame-long window for `code` with constants none true and false past its names */
    #[cold]
    #[inline(never)]
    pub(crate) fn grow_frame(&mut self, chunk: &SSAChunk, code: &Code, pool: usize) {
        // debug builds settle the stack depth of the chunk as its window is first laid
        #[cfg(debug_assertions)]
        self.max_depth(chunk, pool);
        #[cfg(not(debug_assertions))]
        let _ = chunk;
        if code.rows != 0 { return self.lay_rows(code, pool); }
        let cbase = code.cbase as usize;
        let consts = self.pools[pool].consts.as_deref().unwrap_or(&[]).len();
        let (len, wlen) = (code.frame.max(cbase + consts + 3), self.pools[pool].wlen);
        // a window wider than its code narrows so levels copied from it lay no more
        let base = if wlen < len { self.place_window(pool, len) } else { if wlen > len { self.narrow_window(pool, len); } self.pools[pool].window };
        let p = &self.pools[pool];
        let at = base + cbase;
        self.regs[at..at + consts].copy_from_slice(p.consts.as_deref().unwrap_or(&[]));
        self.regs[at + consts..at + consts + 3].copy_from_slice(&[Val::none(), Val::bool(true), Val::bool(false)]);
        // the constants sections read lie past their registers
        if let Some(b) = &code.block {
            let mut from = 0;
            for &(at, n) in b.ranges.iter() { self.regs[base + at..base + at + n].copy_from_slice(&b.extra[from..from + n]); from += n; }
        }
        // unlowered code never marks the window so a call's fast entry never opens first code
        self.pools[pool].wcode = if code.unchanged() { 0 } else { code.layout as usize };
        self.note_window(pool);
        self.run_in(pool);
        let _ = base;
    }

    /* cuts the window of `pool` to `len` registers with nothing left alive past them */
    #[cold]
    #[inline(never)]
    fn narrow_window(&mut self, pool: usize, len: usize) {
        let p = &mut self.pools[pool];
        let (base, wide) = (p.window, p.wlen);
        p.wlen = len;
        self.regs[base + len..base + wide].fill(Val::undef());
    }

    /* whether the window of `pool` is laid out for `code` */
    #[inline(always)]
    pub(crate) fn laid(&self, code: &Code, pool: usize) -> bool {
        let p = &self.pools[pool];
        // code as the compiler wrote it reads its names from registers and nothing past them
        p.wcode == code.layout as usize || code.unchanged() && p.window != super::window::NO_WINDOW && p.wlen >= code.frame
    }

    /* the value register `r` holds with a name read the way `LoadName` reads it */
    #[inline(always)]
    pub(crate) fn reg(&self, chunk: &SSAChunk, r: u16) -> Result<Val, VmErr> {
        let v = self.regs[self.base + r as usize];
        if v.is_undef() { self.unbound(chunk, r) } else { Ok(v) }
    }

    /* the values of the `n` registers from `b` on */
    pub(crate) fn reg_row(&self, chunk: &SSAChunk, b: u16, n: usize) -> Result<Vec<Val>, VmErr> {
        (0..n as u16).map(|k| self.reg(chunk, b + k)).collect()
    }

    /* reading unbound slot `r` fails for a function local and else falls back to builtins */
    #[cold]
    #[inline(never)]
    pub(crate) fn unbound(&self, chunk: &SSAChunk, r: u16) -> Result<Val, VmErr> {
        if self.fi_of(chunk).is_some() { return Err(self.unbound_err(chunk, r as usize)); }
        let name = chunk.names.get(r as usize).map(|n| ssa_strip(n)).unwrap_or_default();
        if self.scopes[self.chunk_module_id(chunk)].closed { return Err(crate::lang::EngineError::Undefined(name.into()).into()); }
        self.builtin_binding(name).ok_or_else(|| crate::lang::EngineError::Undefined(name.into()).into())
    }

    /* the error unbound slot `s` raises telling a function's own variables from captured ones */
    #[cold]
    #[inline(never)]
    pub(crate) fn unbound_err(&self, chunk: &SSAChunk, s: usize) -> VmErr {
        let name = chunk.names.get(s).map(|n| ssa_strip(n)).unwrap_or_default();
        let Some(fi) = self.fi_of(chunk) else { return crate::lang::EngineError::Undefined(name.into()).into() };
        let free = self.fn_scope[fi].freevars.iter().any(|f| f.2 == name);
        crate::lang::EngineError::Unbound { name: Some(name.into()), free }.into()
    }

    /* `a = b op c` on numbers inline and anything else through the stack opcode */
    #[inline(always)]
    pub(crate) fn reg_binop(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &super::cache::OpcodeCache) -> Result<(), VmErr> {
        self.regs[self.base + ins.a as usize] = match numeric_binop(lower::stack_form(ins.op), self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) {
            Some(v) => v,
            None => {
                let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                match self.wide_fast(lower::stack_form(ins.op), b, c)? {
                    Some(v) => v,
                    None => {
                        let v = self.reg_value(ins, rip, chunk, sites)?;
                        // a job the op opened writes `a` once it ends
                        if v.is_undef() { return Ok(()); }
                        v
                    }
                }
            }
        };
        Ok(())
    }

    /* whether `b op c` holds with numbers compared inline */
    #[inline(always)]
    pub(crate) fn reg_test(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &super::cache::OpcodeCache) -> Result<bool, VmErr> {
        if let Some(v) = numeric_binop(lower::stack_form(ins.op), self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return Ok(v.as_bool()); }
        self.reg_test_slow(ins, rip, chunk, sites)
    }

    /* an unsettled comparison tries wide ints then the stack opcode kept out of the loop */
    #[inline(never)]
    fn reg_test_slow(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &super::cache::OpcodeCache) -> Result<bool, VmErr> {
        let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
        if let Some(v) = self.wide_fast(lower::stack_form(ins.op), b, c)? && v.is_bool() { return Ok(v.as_bool()); }
        let r = self.reg_value(ins, rip, chunk, sites)?;
        Ok(self.truthy(r))
    }

    /* runs a register form past its fast path or undef once its job writes `a` */
    #[inline(never)]
    pub(crate) fn reg_value(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &super::cache::OpcodeCache) -> Result<Val, VmErr> {
        // strings join or repeat and a build's slice cuts here never through the general op
        if matches!(ins.op, OpCode::AddR | OpCode::InPlaceAddR | OpCode::MulR) && matches!(self.heap.try_get(self.regs[self.base + ins.b as usize]), Some(HeapObj::Str(_)))
            && let Some(v) = self.joined(ins.op == OpCode::MulR, self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return Ok(v); }
        if ins.op == OpCode::GetItemR && let Some(v) = self.cut(self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return Ok(v); }
        if matches!(ins.op, OpCode::InR | OpCode::NotInR | OpCode::InPlaceAddR) && self.may_job(self.regs[self.base + ins.c as usize]) { return self.reg_job(ins, chunk); }
        // before a class or rebound length no op here hands a call to the loop
        if self.heap.users || self.redo.keep || ins.op == OpCode::LenR && self.native_rebound(ins.c) { return self.reg_armed(ins, rip, chunk, sites); }
        self.reg_op(ins, rip, chunk, sites)
    }

    #[inline(never)]
    fn reg_armed(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &super::cache::OpcodeCache) -> Result<Val, VmErr> {
        // a length whose native's name a module rebound hands its call to the loop
        let go = ins.op == OpCode::LenR && self.native_rebound(ins.c);
        let armed = self.redo_begin(go, rip, 0);
        let r = self.reg_op(ins, rip, chunk, sites);
        self.redo_done(armed);
        r
    }

    /* `in` or list `+=` over a lazy source giving undef once its job writes `a` */
    #[inline(never)]
    fn reg_job(&mut self, ins: Ins, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let op = lower::stack_form(ins.op);
        let (x, y) = (self.reg(chunk, ins.b)?, self.reg(chunk, ins.c)?);
        self.delivering(Dest::Reg(ins.a), |vm| if op == OpCode::InPlaceAdd { Ok(vm.arith_value(op, ins.x as u16, x, y, true)?.unwrap_or(Val::undef())) } else { vm.lazy_identity(op, x, y) })
    }

    /* the value a register form's op gives on its registers */
    #[inline(always)]
    fn reg_op(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &super::cache::OpcodeCache) -> Result<Val, VmErr> {
        let op = lower::stack_form(ins.op);
        let x = self.reg(chunk, ins.b)?;
        match op {
            // a refused operator asks the runtime's rule inside the region a call it makes reruns
            OpCode::Minus => return match self.minus_value(x) { Ok(v) => Ok(v), Err(e) => self.op_rule(e, rip, 0, chunk, sites) },
            OpCode::Not => return Ok(Val::bool(!self.truthy(x))),
            OpCode::Len => return self.len_value(ins.c, x, chunk),
            _ => {}
        }
        let y = self.reg(chunk, ins.c)?;
        let op = match (op, ins.x & lower::AUGMENTED != 0) {
            (OpCode::BitAnd, true) => OpCode::InPlaceBitAnd,
            (OpCode::BitOr, true) => OpCode::InPlaceBitOr,
            (OpCode::BitXor, true) => OpCode::InPlaceBitXor,
            (op, _) => op,
        };
        if op == OpCode::GetItem { return self.item_value(x, y); }
        match self.binary_value(op, ins.x as u16, x, y) { Ok(v) => Ok(v), Err(e) => self.op_rule(e, rip, 0, chunk, sites) }
    }

    /* the length of `o` by native `id` or the binding its module gave that name */
    fn len_value(&mut self, id: u16, o: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let Some(n) = (id != lower::ENGINE_LEN).then(|| self.natives.get(crate::lang::NativeId(id))).flatten() else {
            let n = self.builtin_len(o)?;
            return self.heap.int(n);
        };
        if self.native_rebound(id) && let Some(f) = self.native_binding(n, self.chunk_module_id(chunk)) {
            let v = self.call_with(f, None, &[o], &[], chunk)?;
            return self.got(v);
        }
        // one argument a heap op answers needs no call
        if let Some(fast) = n.fast1 && let Some(v) = fast(&mut self.heap, o) { self.pay_work()?; return Ok(v); }
        let ok = core::mem::take(&mut self.job_ok);
        let v = self.native_val(n, Val::undef(), None, &[o], &[], ok)?;
        self.got(v)
    }

    /* whether register `r` is true */
    #[inline(always)]
    pub(crate) fn reg_truthy(&mut self, chunk: &SSAChunk, r: u16) -> Result<bool, VmErr> {
        let v = self.regs[self.base + r as usize];
        if v.is_bool() { return Ok(self.heap.falsy.bool_truth(v.as_bool())); }
        if v.is_int() { return Ok(self.heap.falsy.int_truth(v.as_int())); }
        let v = self.reg(chunk, r)?;
        Ok(self.truthy(v))
    }

}

/* the int math register forms keep inline or none when the handler must answer */
pub(crate) fn int_add(a: i64, b: i64) -> Option<Val> { Val::int_checked(a + b) }
pub(crate) fn int_sub(a: i64, b: i64) -> Option<Val> { Val::int_checked(a - b) }
pub(crate) fn int_mul(a: i64, b: i64) -> Option<Val> { mul_exact(a, b).and_then(Val::int_checked) }
// the policy kernels inline so a loop's constant policy folds into its arm
#[inline(always)]
pub(crate) fn int_mod(a: i64, b: i64, p: &Policy) -> Option<Val> { p.int_rem(a, b).map(Val::int) }

/* `a * b` when exact with 32-bit operands skipping wasm's 128-bit overflow routine */
#[inline(always)]
pub(crate) fn mul_exact(a: i64, b: i64) -> Option<i64> {
    if a as i32 as i64 == a && b as i32 as i64 == b { Some(a * b) } else { a.checked_mul(b) }
}
#[inline(always)]
pub(crate) fn int_div(a: i64, b: i64, p: &Policy) -> Option<Val> { p.int_quotient(a, b) }
// only the least int over minus one leaves the inline range
#[inline(always)]
pub(crate) fn int_floordiv(a: i64, b: i64, p: &Policy) -> Option<Val> { p.int_floordiv(a, b).filter(|&q| q <= Val::INT_MAX).map(Val::int) }
#[inline(always)]
pub(crate) fn float_div(a: f64, b: f64, p: &Policy) -> Option<Val> { p.float_quotient(a, b) }

/* the int kernel of the arithmetic register form whose stack op is `op` */
#[inline(always)]
pub(crate) fn arith_int(op: OpCode, a: i64, b: i64, p: &Policy) -> Option<Val> {
    match op {
        OpCode::Add | OpCode::InPlaceAdd => int_add(a, b),
        OpCode::Sub | OpCode::InPlaceSub => int_sub(a, b),
        OpCode::Mul => int_mul(a, b),
        OpCode::Div => int_div(a, b, p),
        OpCode::Mod => int_mod(a, b, p),
        OpCode::FloorDiv => int_floordiv(a, b, p),
        _ => None,
    }
}

/* its float kernel with none where only the handler answers */
#[inline(always)]
pub(crate) fn arith_float(op: OpCode, a: f64, b: f64, p: &Policy) -> Option<Val> {
    match op {
        OpCode::Add | OpCode::InPlaceAdd => Some(Val::float(a + b)),
        OpCode::Sub | OpCode::InPlaceSub => Some(Val::float(a - b)),
        OpCode::Mul => Some(Val::float(a * b)),
        OpCode::Div => float_div(a, b, p),
        _ => None,
    }
}

/* whether `a` and `b` keep the order comparison `op` asks */
#[inline(always)]
pub(crate) fn ordered<T: PartialOrd>(op: OpCode, a: T, b: T) -> bool {
    match op {
        OpCode::Lt => a < b,
        OpCode::LtEq => a <= b,
        OpCode::Gt => a > b,
        OpCode::GtEq => a >= b,
        _ => false,
    }
}

/* a bitwise op on two inline ints or none when a shift overflows them */
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

/* `a ** b` for small ints or none for a negative exponent or an overflow */
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

/* float powers or none for zero to a negative power and complex or overflowing results */
fn float_pow(a: f64, b: f64) -> Option<Val> {
    if a == 0.0 && b < 0.0 || crate::value::math::complex_power(a, b) { return None; }
    crate::value::math::fpowf(a, b).map(Val::float)
}

/* `x op y` for two numbers as the register forms compute it or else none */
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
        // a division by the runtime's rules goes to the slow path that reads them
        OpCode::Div if !ints && g != 0.0 => Some(Val::float(f / g)),
        OpCode::Pow => if ints { int_pow(a, b) } else { float_pow(f, g) },
        OpCode::Eq => test(a == b, f == g),
        OpCode::NotEq => test(a != b, f != g),
        OpCode::Lt => test(a < b, f < g),
        OpCode::LtEq => test(a <= b, f <= g),
        OpCode::Gt => test(a > b, f > g),
        OpCode::GtEq => test(a >= b, f >= g),
        _ => None,
    }
}

