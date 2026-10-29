use alloc::rc::Rc;
use alloc::vec::Vec;
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::syntax::ssa::ssa_strip;
use super::{VM, lower};
use super::lower::Code;
use crate::code::Ins;
use super::scope::Kind;
use crate::value::*;

/* the first op from compiler instruction `at` that `is` takes searching its lowered range first */
fn lowered_at(code: &Code, at: u32, is: impl Fn(crate::code::OpCode) -> bool) -> Option<usize> {
    let (from, to) = (code.lowered(at as usize), code.lowered(at as usize + 1));
    let found = |j: &usize| is(code.ins[*j].op) && code.orig(*j) == at;
    (from..to.min(code.ins.len())).find(found).or_else(|| (0..code.ins.len()).find(found))
}

/* whether a call runs inside a loop of `src` so laying it pays each pass */
fn calls_in_loop(src: &[crate::syntax::chunk::Instruction]) -> bool {
    let call = |i: &crate::syntax::chunk::Instruction| matches!(i.opcode, OpCode::Call | OpCode::CallMethod | OpCode::CallSpread);
    src.iter().enumerate().any(|(k, i)| i.opcode == OpCode::Jump && (i.operand as usize) < k && src[i.operand as usize..k].iter().any(call))
}

/* the callee the site of the call at compiler instruction `at` of `code` keeps */
pub(crate) fn callee_at(code: &Code, at: u32) -> Option<Val> {
    let j = lowered_at(code, at, |op| matches!(op, crate::code::OpCode::CallR | crate::code::OpCode::Call | crate::code::OpCode::CallSpreadR))?;
    match code.cache.site(j) { super::sites::Site::Call { callee, .. } => Some(callee), _ => None }
}

/* the method call site at compiler instruction `at` of `code` when it found a method */
pub(crate) fn method_at(code: &Code, at: u32) -> Option<super::sites::Site> {
    let j = lowered_at(code, at, |op| matches!(op, crate::code::OpCode::CallMethodR | crate::code::OpCode::CallMethod))?;
    lower::method_of(code, j)
}

/* the method a method site found */
pub(crate) fn site_func(site: super::sites::Site) -> Option<Val> {
    match site { super::sites::Site::Method { func, .. } => Some(Val::heap(func)), _ => None }
}

impl<'a> VM<'a> {

    /* the function a call of method name on the class runs with the arguments alone */
    pub(crate) fn on_class(&self, class: Val, name: &str, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let (v, _) = self.lookup_class_member(class, crate::syntax::ssa::ssa_strip(name))?;
        self.on_class_found(v, own)
    }

    /* on_class for what the class holds by the name */
    pub(crate) fn on_class_found(&self, v: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        if !matches!(self.heap.try_get(v), Some(HeapObj::Func { .. })) { return None; }
        let mut callee = self.section_of(v, own)?;
        if callee.ctor().is_some() { return None; }
        callee.on_class = 1;
        Some(callee)
    }

    /* an instance's function `func` that a method call of its name runs with arguments alone */
    pub(crate) fn on_field(&self, func: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        if !matches!(self.heap.try_get(func), Some(HeapObj::Func { .. })) { return None; }
        let mut callee = self.section_of(func, own)?;
        if callee.ctor().is_some() { return None; }
        callee.on_class = 4;
        Some(callee)
    }

    /* `func`'s section for bound methods a register call calls with the receiver as first argument */
    pub(crate) fn bound_method(&self, func: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        if !matches!(self.heap.try_get(func), Some(HeapObj::Func { .. })) { return None; }
        let mut callee = self.section_of(func, own)?;
        callee.on_class = 7;
        Some(callee)
    }

    /* whether a nested or looped `def` remakes `fi` so a call site meets many copies */
    pub(crate) fn remade(&self, fi: usize) -> bool {
        match self.fn_remade[fi].get() { 0 => {}, r => return r == 2 }
        if self.function_parents[fi].is_some() { self.fn_remade[fi].set(2); return true; }
        let parent = self.fn_definer[fi];
        let ins = &parent.instructions;
        let backs: Vec<(usize, usize)> = ins.iter().enumerate().filter(|(j, i)| i.opcode == OpCode::Jump && (i.operand as usize) <= *j).map(|(j, i)| (i.operand as usize, j)).collect();
        for (m, i) in ins.iter().enumerate().filter(|(_, i)| matches!(i.opcode, OpCode::MakeFunction | OpCode::MakeCoroutine)) {
            let Some(g) = parent.functions.get(i.operand as usize).and_then(|d| self.fi_of(&d.1)) else { continue };
            if self.fn_remade[g].get() == 0 { self.fn_remade[g].set(1 + backs.iter().any(|&(t, j)| t <= m && m < j) as u8); }
        }
        if self.fn_remade[fi].get() == 0 { self.fn_remade[fi].set(1); }
        self.fn_remade[fi].get() == 2
    }

    /* what names `chunk`'s code across snapshots as function `fi` or below zero its module's path */
    pub(crate) fn code_key(&self, chunk: &SSAChunk, fi: Option<usize>) -> i64 { Self::code_key_of(chunk, fi) }

    /* `code_key` as a function of the chunk and its function alone */
    pub(crate) fn code_key_of(chunk: &SSAChunk, fi: Option<usize>) -> i64 {
        match fi {
            Some(fi) => fi as i64,
            None => -1 - (crate::value::hash_key(chunk.path.as_str()) as i64 & 0x3FFF_FFFF_FFFF),
        }
    }

    /* whether `chunk` as function `fi` or a module had code laid with sections */
    pub(crate) fn was_laid(&self, chunk: &SSAChunk, fi: Option<usize>) -> bool {
        match fi {
            Some(fi) => self.laid_fns.get(fi).copied().unwrap_or(false),
            None => self.laid_mods.iter().any(|p| p.as_str() == chunk.path.as_str()),
        }
    }

    /* `value` as a call's section when its lowered body calls only such functions */
    pub(crate) fn section_of(&self, value: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        if cfg!(feature = "coverage") { return None; }
        if self.heap.is_class(value) { return self.ctor_section(value, own); }
        let Some(HeapObj::Func { fi, defaults, cells: captures }) = self.heap.try_get(value) else { return None };
        self.section_of_fn(value, *fi, defaults.to_vec(), captures.to_vec(), own)
    }

    /* the section every function made from `body` without defaults runs with cells filled from each */
    pub(crate) fn body_section(&self, body: u32) -> Option<lower::Callee<'_>> {
        let fi = self.fi_of_id(body)?;
        if self.functions[fi].2 != 0 || self.call_facts[fi].arity.is_none() { return None; }
        // a closure's cells fill from each function the body makes as it is called
        let cells: Vec<(usize, Val)> = if self.call_facts[fi].closure { self.fn_scope[fi].freevars.iter().map(|f| (f.0, Val::undef())).collect() } else { Vec::new() };
        let mut callee = self.section_of_fn(Val::undef(), fi, Vec::new(), cells, &|_| None)?;
        callee.by_body = true;
        Some(callee)
    }

    /* the section every comprehension made from `body` runs with cells filled from each one */
    pub(crate) fn made_section(&self, body: u32) -> Option<lower::Callee<'_>> {
        let fi = self.fi_of_id(body)?;
        if self.functions[fi].2 != 0 || self.call_facts[fi].arity != Some(1) { return None; }
        let cells: Vec<(usize, Val)> = self.fn_scope[fi].freevars.iter().map(|f| (f.0, Val::undef())).collect();
        let mut callee = self.section_of_fn(Val::undef(), fi, Vec::new(), cells, &|_| None)?;
        callee.by_body = true;
        Some(callee)
    }

    /* `section_of` for `fi` with `defaults` and `cells` and `value` undef for any of its body */
    fn section_of_fn(&self, value: Val, fi: usize, defaults: Vec<Val>, cells: Vec<(usize, Val)>, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let f = &self.call_facts[fi];
        use super::ParamKind::{KwOnly, Normal};
        let params = &self.param_slots[fi];
        let positional = params.iter().take_while(|p| matches!(p.0, Normal)).count();
        let marks = &self.functions[fi].0;
        let keyed = f.arity.is_none() && positional < params.len() && params[positional..].iter().all(|p| matches!(p.0, KwOnly))
            && marks.iter().skip_while(|m| !m.ends_with('=')).all(|m| m.ends_with('='));
        if f.arity.is_none() && !keyed { return self.star_section(value, fi, own); }
        let required = f.required as usize;
        let arity = f.arity.unwrap_or(params.len());
        // laid bodies fill their first slots with positionals and exclude coroutines whose call runs nothing
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
        Some(lower::Callee { value, fi, cells, defaults, required, rare: None, body, arity, names, consts, module, params: &self.functions[fi].0, posonly: self.functions[fi].1.posonly as usize, literals: &self.functions[fi].1.constants, by_body: false, twin: false, star: None, dstar: None, cellvars: self.fn_scope[fi].cellvars.clone(), positional, on_class: 0, generator: false, exc: false, bound: u16::MAX })
    }

    /* generator functions the module bindings of `kinds` hold with bodies lowered without sections */
    pub(crate) fn gen_bodies(&mut self, kinds: &[Kind], module: usize) -> Vec<(u64, usize, Rc<Code>)> {
        let mut out: Vec<(u64, usize, Rc<Code>)> = Vec::new();
        for k in kinds {
            let Kind::Global(g) = *k else { continue };
            let Some(v) = self.scopes.get(module).map(|s| s.at(g)) else { continue };
            let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(v) else { continue };
            if !self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) || out.iter().any(|o| o.0 == v.0) { continue; }
            let (body, pool) = (self.functions[fi], self.call_facts[fi].pool);
            if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[pool].consts = Some(consts); }
            if self.pools[pool].consts.is_none() { continue; }
            let code = match &self.pools[pool].code {
                Some(code) if !code.unchanged() && code.block.is_none() => code.clone(),
                // lowered once and kept as an empty pool's code like one only sections run
                _ => {
                    let facts = self.lowering_facts(&body.1, pool, Some(fi));
                    // a lowering keeping a value over rows leaves the body unlowered
                    let Some(code) = lower::lower(&lower::fuse_method_calls(&body.1, facts.shape), &body.1, &facts, &|_| None).filter(super::rows::unstacked) else { continue };
                    let code = Rc::new(code);
                    if self.pools[pool].code.is_none() { self.pools[pool].code = Some(code.clone()); }
                    code
                }
            };
            out.push((v.0, fi, code));
        }
        out
    }

    /* the constants of module `m`'s generator functions made once so a lowering may lay any */
    pub(crate) fn gen_consts(&mut self, m: usize) {
        let Some(scope) = self.scopes.get(m) else { return };
        let fns: Vec<usize> = scope.iter().filter_map(|(_, v)| match self.heap.try_get(v) { Some(&HeapObj::Func { fi, .. }) if self.functions[fi].1.is_generator => Some(fi), _ => None }).collect();
        for fi in fns {
            let pool = self.call_facts[fi].pool;
            if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&self.functions[fi].1, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        }
    }

    /* the body of generator `fi` lowered without sections from its pool or made now */
    pub(crate) fn gen_body(&self, fi: usize) -> Option<Rc<Code>> {
        if !self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return None; }
        let (body, pool) = (self.functions[fi], self.call_facts[fi].pool);
        self.pools[pool].consts.as_ref()?;
        match &self.pools[pool].code {
            Some(code) if !code.unchanged() && code.block.is_none() => Some(code.clone()),
            _ => {
                let facts = self.fn_facts(fi);
                lower::lower(&lower::fuse_method_calls(&body.1, facts.shape), &body.1, &facts, &|_| None).filter(super::rows::unstacked).map(Rc::new)
            }
        }
    }

    /* a `for`'s generator as its section taking positionals then `*args` with no captures or cells */
    pub(crate) fn gen_section_of(&self, value: Val, fi: usize, body: Rc<Code>) -> Option<lower::Callee<'_>> {
        use super::ParamKind::*;
        if cfg!(feature = "coverage") { return None; }
        // one a body makes with `value` undefined takes neither defaults nor captures
        let (defaults, captures) = match self.heap.try_get(value) { Some(HeapObj::Func { defaults: d, cells: c, .. }) => (d.to_vec(), c.len()), _ => (Vec::new(), 0) };
        let params = &self.param_slots[fi];
        let p = params.iter().take_while(|(k, _)| matches!(k, Normal)).count();
        let star = params.get(p).filter(|(k, _)| matches!(k, Star)).map(|_| p);
        let arity = p + star.is_some() as usize;
        if params.len() != arity || !params.iter().enumerate().all(|(k, &(_, s))| s == k) || captures != 0 || !self.fn_scope[fi].cellvars.is_empty()
            || star.is_some() && !defaults.is_empty() || defaults.len() > p { return None; }
        let consts = self.pools[self.call_facts[fi].pool].consts.as_deref()?.to_vec();
        Some(lower::Callee { value, fi, cells: Vec::new(), required: p - defaults.len(), defaults, rare: None, body, arity, names: self.functions[fi].1.names.len(), consts,
            module: self.fn_module_id(fi), params: &self.functions[fi].0, posonly: self.functions[fi].1.posonly as usize, literals: &self.functions[fi].1.constants, by_body: false, twin: false, star, dstar: None, cellvars: Vec::new(), positional: arity, on_class: 0, generator: true, exc: false, bound: u16::MAX })
    }

    /* a plain function `fi` with `*args` or `**kwargs` laid as a section */
    fn star_section(&self, value: Val, fi: usize, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        use super::ParamKind::*;
        let Some(HeapObj::Func { defaults, cells: captures, .. }) = self.heap.try_get(value) else { return None };
        let (cells, defaults) = (captures.to_vec(), defaults.to_vec());
        // positionals then the tuple and keyword-only parameters then the dict all in slot order
        let params = &self.param_slots[fi];
        let p = params.iter().take_while(|(k, _)| matches!(k, Normal)).count();
        let star = params.get(p).filter(|(k, _)| matches!(k, Star)).map(|_| p);
        let q = p + star.is_some() as usize + params[p + star.is_some() as usize..].iter().take_while(|(k, _)| matches!(k, KwOnly)).count();
        let dstar = params.get(q).filter(|(k, _)| matches!(k, KwRest)).map(|_| q);
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
        Some(lower::Callee { value, fi, cells, defaults, required, rare: None, body: body_code, arity, names: body.names.len(), consts,
            module: self.fn_module_id(fi), params: &self.functions[fi].0, posonly: self.functions[fi].1.posonly as usize, literals: &self.functions[fi].1.constants, by_body: false, twin: false, star, dstar, cellvars: Vec::new(), positional: p, on_class: 0, generator: false, exc: false, bound: u16::MAX })
    }

    /* a plain class's init laid as the section its calls jump to */
    fn ctor_section(&self, class: Val, own: &dyn Fn(usize) -> Option<Rc<Code>>) -> Option<lower::Callee<'_>> {
        let (init, _) = self.lookup_class_member(class, self.natives.names().constructor)?;
        if !matches!(self.heap.try_get(init), Some(HeapObj::Func { .. })) { return None; }
        let mut callee = self.section_of(init, own)?;
        // an exception's instance takes every argument as its `args` so each call passes them all
        callee.exc = self.exc_base(class).is_some();
        if callee.exc && (!callee.defaults.is_empty() || callee.positional != callee.arity) { return None; }
        let body = callee.body.clone();
        let none = (callee.body.cbase as usize + callee.consts.len()) as u16;
        let writes_self = |i: &Ins| matches!(super::block::role(i), Some((true, _, _))) && i.a == 0 || i.op == OpCode::StoreTopR && i.a == 0
            || matches!(i.op, OpCode::UnpackR | OpCode::OpR) && super::block::rows(i).is_some_and(|(d, _)| d.iter().any(|d| d.contains(&0)));
        if callee.arity == 0 || body.ins.iter().any(|i| writes_self(i) || i.op == OpCode::ReturnR && i.b != none || i.op == OpCode::ReturnValue) { return None; }
        let attrs = self.ctors.get(&class.0).map(|c| c.attrs).filter(|&a| a > 0).unwrap_or_else(|| {
            let mut named: Vec<u16> = body.ins.iter().filter(|i| i.op == OpCode::SetAttrR && i.a == 0).map(|i| i.c).collect();
            named.sort_unstable();
            named.dedup();
            named.len()
        });
        callee.value = class;
        callee.set_ctor(Some((self.layout_epoch, attrs, init)));
        Some(callee)
    }

    /* lowers without sections each codeless function `kinds` names and their callees a few levels deep */
    pub(crate) fn lower_callees(&mut self, kinds: &[Kind], module: usize, lowering: usize, depth: usize) {
        if depth > 3 { return; }
        for k in kinds {
            let Kind::Global(g) = *k else { continue };
            let Some(value) = self.scopes.get(module).map(|s| s.at(g)) else { continue };
            self.lower_callee(value, lowering, depth);
        }
        // the module's bindings a native call of their names calls are callees too
        if self.any_rebound == 0 { return; }
        for (_, g) in self.bound_keys(module) {
            if let Some(value) = self.scopes.get(module).map(|s| s.at(u32::from(g))) { self.lower_callee(value, lowering, depth); }
        }
    }

    /* lowers as `lower_callees` does the functions and methods the calls of code found */
    pub(crate) fn lower_found(&mut self, code: &Code, lowering: usize) {
        for j in 0..code.ins.len() {
            let call = matches!(code.ins[j].op, OpCode::CallR | OpCode::CallMethodR);
            let value = match code.cache.site(j) {
                super::sites::Site::Call { callee, .. } if call => callee,
                super::sites::Site::Method { func, .. } if call => Val::heap(func),
                _ => continue,
            };
            self.lower_callee(value, lowering, 0);
        }
    }

    /* `lower_callees` for the function or class `value` */
    pub(crate) fn lower_callee(&mut self, value: Val, lowering: usize, depth: usize) {
        // a class call runs its init lowered as a function is
        let value = match self.heap.try_get(value) {
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() => match self.lookup_class_member(value, self.natives.names().constructor) { Some((init, _)) => init, None => return },
            _ => value,
        };
        let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(value) else { return };
        let f = self.call_facts[fi];
        // first code holding a body lent to its callers is lowered for them already
        let p = &self.pools[f.pool];
        if f.closure || f.pool == lowering || p.code.as_ref().is_some_and(|c| !p.cold || c.plain.get().is_some()) || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        let body = self.functions[fi];
        if self.pools[f.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[f.pool].consts = Some(consts); }
        if self.pools[f.pool].consts.is_none() { return; }
        if !self.lend(fi, f.pool, &body.1, false) { self.lowered_code(&body.1, f.pool, false); }
        let kinds = self.fn_scope[fi].kinds.clone();
        self.lower_callees(&kinds, self.fn_module_id(fi), lowering, depth + 1);
    }

    /* lowers closure `value` which `lower_callee` skips so a section of its body may be laid */
    pub(crate) fn lower_closure(&mut self, value: Val, lowering: usize) {
        let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(value) else { return };
        let f = self.call_facts[fi];
        if !f.closure || f.pool == lowering || self.pools[f.pool].code.is_some() || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return; }
        let body = self.functions[fi];
        if self.pools[f.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[f.pool].consts = Some(consts); }
        if self.pools[f.pool].consts.is_some() && !self.lend(fi, f.pool, &body.1, false) { self.lowered_code(&body.1, f.pool, false); }
    }

    /* lowers like `lower_callees` the never-run bodies `chunk` makes functions of so calls may lay them */
    pub(crate) fn lower_made(&mut self, chunk: &SSAChunk) {
        for ins in chunk.instructions.iter().filter(|i| i.opcode == OpCode::MakeFunction) {
            let Some(desc) = chunk.functions.get(ins.operand as usize) else { continue };
            let Some(fi) = self.fi_of(&desc.1) else { continue };
            let f = self.call_facts[fi];
            if f.closure || !f.in_loop || self.pools[f.pool].code.as_ref().is_some_and(|c| !c.unchanged() || c.plain.get().is_some()) || desc.2 != 0 { continue; }
            let body = self.functions[fi];
            if self.pools[f.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[f.pool].consts = Some(consts); }
            if self.pools[f.pool].consts.is_none() || self.lend(fi, f.pool, &body.1, true) { continue; }
            // a body run only unlowered lowers now and frames running it keep the old code
            if let Some(old) = self.pools[f.pool].code.take() { self.retired.push(old); }
            self.lowered_code(&body.1, f.pool, false);
        }
    }

    /* lowers body `chunk` of `fi` for callers to lay beside the first code it heats */
    #[cold]
    #[inline(never)]
    pub(crate) fn lend(&mut self, fi: usize, pool: usize, chunk: &SSAChunk, made: bool) -> bool {

        if self.pools[pool].shape == 0 { self.pools[pool].shape = lower::shape(&chunk.instructions); }
        // a body without a calling loop gains nothing from heating so it runs the lowering
        if cfg!(feature = "coverage") || self.pools[pool].shape & (lower::LOOPS | lower::CALLS) != lower::LOOPS | lower::CALLS
            || made && (chunk.is_generator || self.is_async.get(fi).copied().unwrap_or(false) || !calls_in_loop(&chunk.instructions)) { return false; }
        // one that never ran takes its first code now and its first call still counts
        if self.pools[pool].code.is_none() {
            self.cold_code(fi);
            let p = &mut self.pools[pool];
            if p.cold { (p.ran, p.heat) = (false, p.heat.saturating_sub(1)); }
        }
        let Some(code) = self.pools[pool].code.clone() else { return false };
        let facts = self.fn_facts(fi);
        let _ = code.plain.set(lower::lower(&lower::fuse_method_calls(chunk, facts.shape), chunk, &facts, &|_| None).filter(|c| c.pure()).map(Rc::new));
        true
    }

    /* whether `fi`'s body reads the module binding holding it as a call of itself does */
    pub(crate) fn calls_itself(&self, fi: usize, kinds: &[Kind]) -> bool {
        let Some(scope) = self.scopes.get(self.fn_module_id(fi)) else { return false };
        kinds.iter().any(|k| matches!(*k, Kind::Global(g) if matches!(self.heap.try_get(scope.at(g)), Some(&HeapObj::Func { fi: f, .. }) if f == fi)))
            || self.calls_own_method(fi)
    }

    /* whether method `fi` reads its own name off an object as `self.depth()` does */
    pub(crate) fn calls_own_method(&self, fi: usize) -> bool {
        if !self.is_class_body(self.fn_definer[fi]) { return false; }
        let (Some(name), chunk) = (self.function_names.get(fi), &self.functions[fi].1) else { return false };
        chunk.instructions.iter().any(|i| i.opcode == OpCode::LoadAttr && chunk.names.get(i.operand as usize).is_some_and(|n| ssa_strip(n) == name))
    }
}
