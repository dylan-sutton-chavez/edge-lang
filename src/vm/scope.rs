use alloc::{string::String, vec, vec::Vec};

use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::syntax::ssa::ssa_strip;
use crate::value::hash_key;
use crate::util::hash::FxHashMap;

use super::VM;
use crate::value::*;

/* bumped when a heap value's binding is replaced so laid sections recheck bindings only then */
pub(crate) static REBOUND: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[inline(always)]
fn rebinds(old: Val, new: Val) {
    if old.is_heap() && old.0 != new.0 { REBOUND.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
}

// a binding whose name no native of the runtime has
const NO_NATIVE: u16 = u16::MAX;

/* a module's bindings where each name keeps one index for the whole run */
#[derive(Clone)]
pub(crate) struct Globals {
    /* each name's index found by comparing against `names` */
    index: hashbrown::HashTable<u32>,
    names: Vec<String>,
    vals: Vec<Val>,
    /* each name's native or NO_NATIVE and the builtin a read found while unbound */
    builtin: Vec<(u16, core::cell::Cell<Val>)>,
    /* set for text run under its own builtins whose unbound names find no builtin */
    pub(crate) closed: bool,
    /* the runtime's natives which tell the builtin a name stands for */
    natives: &'static crate::lang::Natives,
}

impl Default for Globals {
    fn default() -> Self { Self::of(&crate::lang::Natives::EMPTY) }
}

impl Globals {
    /* the bindings of a module whose builtins are those of `natives` */
    pub(crate) fn of(natives: &'static crate::lang::Natives) -> Self {
        Self { index: Default::default(), names: Vec::new(), vals: Vec::new(), builtin: Vec::new(), closed: false, natives }
    }

    /* what the runtime of these bindings calls the engine's conventions */
    pub(crate) fn runtime_names(&self) -> &'static crate::lang::Names { self.natives.names() }

    fn find(&self, name: &str) -> Option<u32> {
        self.index.find(hash_key(name), |&i| self.names[i as usize] == name).copied()
    }

    /* the index of `name` which stays unbound until something binds it */
    pub fn id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.find(name) { return i; }
        let i = self.vals.len() as u32;
        self.names.push(name.into());
        self.vals.push(Val::undef());
        self.builtin.push((self.natives.function_id(name).map_or(NO_NATIVE, |id| id.0), core::cell::Cell::new(Val::undef())));
        let names = &self.names;
        self.index.insert_unique(hash_key(name), i, |&j| hash_key(&names[j as usize]));
        i
    }

    pub fn get(&self, name: &str) -> Option<Val> {
        self.find(name).map(|i| self.vals[i as usize]).filter(|v| !v.is_undef())
    }

    /* each native `holds` takes whose name the module binds with that binding's slot */
    pub(crate) fn rebound_names(&self, holds: impl Fn(u16) -> bool) -> Vec<(u16, u16)> {
        self.builtin.iter().zip(&self.vals).enumerate().take(usize::from(u16::MAX))
            .filter(|(_, (b, v))| b.0 != NO_NATIVE && !v.is_undef() && holds(b.0)).map(|(g, (b, _))| (b.0, g as u16)).collect()
    }

    pub fn set(&mut self, name: &str, v: Val) {
        let i = self.id(name);
        rebinds(self.vals[i as usize], v);
        self.vals[i as usize] = v;
    }

    #[inline(always)]
    pub fn at(&self, i: u32) -> Val { self.vals.get(i as usize).copied().unwrap_or(Val::undef()) }

    #[inline(always)]
    pub fn set_at(&mut self, i: u32, v: Val) { if let Some(s) = self.vals.get_mut(i as usize) { rebinds(*s, v); *s = v; } }

    pub fn name(&self, i: u32) -> &str { self.names.get(i as usize).map_or("", |n| n.as_str()) }

    /* the builtin last read under index `i` while unbound and undef before the first read */
    #[inline]
    pub fn fallback(&self, i: u32) -> Val { self.builtin.get(i as usize).map_or(Val::undef(), |b| b.1.get()) }

    /* the binding at `i` else the builtin it last stood for and undef for neither */
    #[inline]
    pub fn read(&self, i: u32) -> Val { match self.at(i) { v if v.is_undef() => self.fallback(i), v => v } }

    /* the native binding `i` names whose calls a store there redirects */
    #[inline]
    pub fn names_native(&self, i: u32) -> Option<u16> { self.builtin.get(i as usize).map(|b| b.0).filter(|&id| id != NO_NATIVE) }

    /* every name with its value in first-named order and undef for unbound ones */
    pub fn entries(&self) -> impl Iterator<Item = (&str, Val)> + '_ { self.names.iter().map(String::as_str).zip(self.vals.iter().copied()) }

    /* bound names with their values */
    pub fn iter(&self) -> impl Iterator<Item = (&str, Val)> + '_ { self.entries().filter(|(_, v)| !v.is_undef()) }

    pub fn from_entries(entries: Vec<(String, Val)>, natives: &'static crate::lang::Natives) -> Self {
        let mut g = Self::of(natives);
        for (name, v) in entries { g.set(&name, v); }
        g
    }
}

/* what a frame slot holds fixed for the chunk before it runs */
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Local,
    // a cell a nested function shares with reads and writes going through it
    Cell,
    // a module binding by its index with the builtins under it
    Global(u32),
}

/* how a function body's names resolve as computed once from the code */
#[derive(Default, Clone)]
pub struct FnScope {
    pub kinds: alloc::rc::Rc<[Kind]>,
    /* slots that start as a fresh cell holding what the call bound there */
    pub cellvars: Vec<usize>,
    /* the cells a definition takes by its slot and the definer's slot and name */
    pub freevars: Vec<(usize, Option<usize>, String)>,
}

/* what a module or class or function body name is as resolved before running */
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Binding {
    // a slot of the body's frame or a temporary or attribute name no variable takes
    Local,
    // a cell a nested function shares or in a class body the enclosing function's cell
    Cell,
    // a module binding by its index with the builtins under it
    Global(u32),
    // a name a class body binds as a class member its methods do not see
    Member,
    // a plain local of the defining function read by value while the class is made
    Lent,
}

/* which body a scope belongs to with a function's index in the VM's table */
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BodyKind { Module, Class, Function(usize) }

fn canon_of(chunk: &SSAChunk, i: usize) -> usize {
    chunk.canonical.get(i).map_or(i, |&c| c as usize)
}

/* the slot each bare name's versions share preferring a versioned name */
fn slots_by_name(chunk: &SSAChunk) -> FxHashMap<&str, usize> {
    let mut map = FxHashMap::default();
    for versioned in [true, false] {
        for (i, n) in chunk.names.iter().enumerate() {
            if crate::syntax::ssa::SsaName::parse(n).is_some() == versioned { map.entry(ssa_strip(n)).or_insert(canon_of(chunk, i)); }
        }
    }
    map
}

/* sorted bare names a body binds by store or delete or parameter except nonlocals */
fn binds<'c>(params: &'c [String], body: &'c SSAChunk, bare: &[&'c str]) -> Vec<&'c str> {
    let mut set: Vec<&str> = params.iter().map(|p| crate::syntax::ssa::param_base_name(p)).collect();
    for ins in &body.instructions {
        if (matches!(ins.opcode, OpCode::StoreName | OpCode::Phi) || ins.opcode == OpCode::Del && !declared_global(body, ins.operand)) && let Some(&n) = bare.get(ins.operand as usize) { set.push(n); }
    }
    if !is_comp(params) { walrus_names(body, &mut set); }
    set.sort_unstable();
    set.dedup();
    set.retain(|n| !body.nonlocals.iter().any(|x| x == n));
    set
}

/* whether `body` declares name `k` `global` so its deletes unbind it in the module */
fn declared_global(body: &SSAChunk, k: u16) -> bool { body.instructions.iter().any(|i| i.opcode == OpCode::Global && i.operand == k) }

pub(crate) fn is_comp(params: &[String]) -> bool { matches!(params, [p] if p == crate::syntax::ssa::COMP_ARG) }

/* the walrus targets of the comprehensions in `body` which the function holding them binds */
fn walrus_names<'c>(body: &'c SSAChunk, out: &mut Vec<&'c str>) {
    for (params, inner, _, _) in &body.functions {
        if is_comp(params) {
            out.extend(inner.nonlocals.iter().map(String::as_str));
            walrus_names(inner, out);
        }
    }
}

fn add<'c>(set: &mut Vec<&'c str>, n: &'c str) { if !set.contains(&n) { set.push(n); } }

impl<'a> VM<'a> {
    /* resolves free names of functions from `start` on to an enclosing cell or a global */
    pub(crate) fn analyze_scopes(&mut self, start: usize) {
        let end = self.functions.len();
        let functions = self.functions.clone();
        // only analyzed bodies and enclosing ones are stripped so adopted code pays only for itself
        let mut need = vec![false; end];
        for fi in start..end {
            let mut at = Some(fi);
            while let Some(f) = at.filter(|&f| !need[f]) { need[f] = true; at = self.function_parents[f]; }
        }
        let bare: Vec<Vec<&str>> = functions.iter().zip(&need).map(|((_, body, _, _), &n)| if n { body.names.iter().map(|n| ssa_strip(n)).collect() } else { Vec::new() }).collect();
        let locals: Vec<Vec<&str>> = functions.iter().zip(&bare).zip(&need).map(|(((params, body, _, _), b), &n)| if n { binds(params, body, b) } else { Vec::new() }).collect();
        let owns = |f: usize, n: &str| locals[f].binary_search(&n).is_ok();
        let mut cells: Vec<Vec<&str>> = vec![Vec::new(); end];
        let mut frees: Vec<Vec<&str>> = vec![Vec::new(); end];
        let mut reads: Vec<Vec<&str>> = vec![Vec::new(); end];
        for fi in start..end {
            let body = &functions[fi].1;
            for ins in &body.instructions {
                // a name declared `global` reads and writes the module binding wherever it appears
                if matches!(ins.opcode, OpCode::LoadGlobal | OpCode::StoreGlobal) {
                    if let Some(n) = body.names.get(ins.operand as usize) && !reads[fi].contains(&n.as_str()) { reads[fi].push(n); }
                    continue;
                }
                if !matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi) { continue; }
                let Some(&n) = bare[fi].get(ins.operand as usize) else { continue };
                if n.starts_with('#') || owns(fi, n) { continue; }
                // the nearest enclosing function binding it owns the cell and those between pass it on
                let mut between = Vec::new();
                let mut anc = self.function_parents[fi];
                let owner = loop {
                    match anc {
                        Some(a) if owns(a, n) => break Some(a),
                        Some(a) => { between.push(a); anc = self.function_parents[a]; }
                        None => break None,
                    }
                };
                match owner {
                    Some(a) => {
                        add(&mut frees[fi], n);
                        for b in between { add(&mut frees[b], n); }
                        add(&mut cells[a], n);
                    }
                    None => if !reads[fi].contains(&n) { reads[fi].push(n); },
                }
            }
        }
        let mut scopes = Vec::with_capacity(end - start);
        for fi in start..end {
            let body = &functions[fi].1;
            let module = self.fn_module_id(fi);
            // the names a `global` statement declares gathered once
            let globals: Vec<u16> = body.instructions.iter().filter(|i| i.opcode == OpCode::Global).map(|i| i.operand).collect();
            let kinds: Vec<Kind> = body.names.iter().zip(&bare[fi]).enumerate().map(|(k, (name, &bare))| {
                if bare.starts_with('#') { Kind::Local }
                else if globals.contains(&(k as u16)) { Kind::Global(self.scopes[module].id(bare)) }
                else if reads[fi].contains(&name.as_str()) { Kind::Global(self.scopes[module].id(name)) }
                else if cells[fi].contains(&bare) || frees[fi].contains(&bare) { Kind::Cell }
                else if owns(fi, bare) { Kind::Local }
                else if reads[fi].contains(&bare) { Kind::Global(self.scopes[module].id(bare)) }
                else { Kind::Local }
            }).collect();
            let cellvars: Vec<usize> = (0..body.names.len())
                .filter(|&i| canon_of(body, i) == i && cells[fi].contains(&bare[fi][i]))
                .collect();
            let definer: &SSAChunk = self.fn_definer[fi];
            let definer_slots = self.fi_of(definer).is_some().then(|| slots_by_name(definer));
            // every slot naming a free variable takes its cell including a declaration's bare name
            let mut freevars: Vec<(usize, Option<usize>, String)> = (0..body.names.len())
                .filter(|&i| canon_of(body, i) == i && frees[fi].contains(&bare[fi][i]))
                .map(|i| {
                    let n = bare[fi][i];
                    (i, definer_slots.as_ref().and_then(|m| m.get(n).copied()), String::from(n))
                })
                .collect();
            freevars.sort_unstable_by_key(|f| f.0);
            scopes.push(FnScope { kinds: kinds.into(), cellvars, freevars });
        }
        self.fn_scope.truncate(start);
        self.fn_scope.extend(scopes);
    }

    /* the bindings of every module class and function body under `root` keyed by body id */
    pub(crate) fn body_bindings(&mut self, root: &'a SSAChunk) -> FxHashMap<u32, (BodyKind, Vec<Binding>)> {
        // each class body under the body that defines it
        fn walk<'c>(c: &'c SSAChunk, modules: &mut Vec<&'c SSAChunk>, definers: &mut Vec<(&'c SSAChunk, &'c SSAChunk)>) {
            for k in &c.classes { definers.push((k, c)); walk(k, modules, definers); }
            for f in &c.functions { walk(&f.1, modules, definers); }
            for e in &c.imports { if let crate::syntax::chunk::ImportKind::Code(sub) = &e.kind { modules.push(sub); walk(sub, modules, definers); } }
        }
        let (mut modules, mut definers) = (vec![root], Vec::new());
        walk(root, &mut modules, &mut definers);
        let mut out = FxHashMap::default();
        for fi in 0..self.functions.len() {
            let kinds = self.fn_scope[fi].kinds.iter().map(|k| match *k { Kind::Local => Binding::Local, Kind::Cell => Binding::Cell, Kind::Global(g) => Binding::Global(g) }).collect();
            out.insert(self.functions[fi].1.id.get(), (BodyKind::Function(fi), kinds));
        }
        for m in modules {
            let (kinds, _) = self.chunk_kinds(m);
            out.insert(m.id.get(), (BodyKind::Module, kinds.iter().map(|k| match *k { Kind::Global(g) => Binding::Global(g), _ => Binding::Local }).collect()));
        }
        let definer_of = |c: &SSAChunk| definers.iter().find(|d| core::ptr::eq(d.0, c)).map(|d| d.1);
        for &(body, definer) in &definers {
            // a class body reads the cells of the function around it through the classes between
            let mut around = definer;
            while self.fi_of(around).is_none() && let Some(d) = definer_of(around) { around = d; }
            let fi = self.fi_of(around);
            let kind_in = |vm: &Self, bare: &str| -> Option<Kind> {
                let fi = fi?;
                let all: Vec<Kind> = around.names.iter().enumerate().filter(|&(_, n)| ssa_strip(n) == bare).filter_map(|(i, _)| vm.fn_scope[fi].kinds.get(i).copied()).collect();
                // the cells around the class come before the function's plain locals
                [Kind::Cell, Kind::Local].into_iter().find(|k| all.contains(k))
            };
            let direct = core::ptr::eq(around, definer);
            let module = self.chunk_module_id(body);
            let mut var = vec![false; body.names.len()];
            let mut member = vec![false; body.names.len()];
            let mut global = vec![false; body.names.len()];
            for ins in &body.instructions {
                let Some(i) = Some(ins.operand as usize).filter(|&i| i < body.names.len()) else { continue };
                match ins.opcode {
                    OpCode::StoreName | OpCode::Phi => { var[i] = true; member[i] = true; }
                    OpCode::LoadName | OpCode::Del => var[i] = true,
                    OpCode::LoadGlobal | OpCode::StoreGlobal | OpCode::Global => { var[i] = true; global[i] = true; }
                    _ => {}
                }
            }
            let kinds = body.names.iter().enumerate().map(|(i, name)| {
                let bare = ssa_strip(name);
                if !var[i] || bare.starts_with('#') { return Binding::Local; }
                if global[i] { return Binding::Global(self.scopes[module].id(bare)); }
                if member[i] { return Binding::Member; }
                // a plain local is lent only to a class its function defines itself
                match kind_in(self, bare) {
                    Some(Kind::Cell) => Binding::Cell,
                    Some(Kind::Local) if direct => Binding::Lent,
                    _ => Binding::Global(self.scopes[module].id(bare)),
                }
            }).collect();
            out.insert(body.id.get(), (BodyKind::Class, kinds));
        }
        out
    }

    /* the module table a function reads its globals from */
    pub(crate) fn fn_module_id(&self, fi: usize) -> usize {
        self.fn_module.get(fi).and_then(|m| m.as_ref()).and_then(|spec| self.scope_ids.get(spec).copied()).unwrap_or(0)
    }

    /* the module table a chunk's code binds into */
    pub(crate) fn chunk_module_id(&self, chunk: &SSAChunk) -> usize {
        self.body_of(chunk).map_or(0, |b| b.module as usize)
    }

    /* the file of the code running now which names the package a send comes from */
    #[cfg(any(feature = "std", all(target_arch = "wasm32", feature = "runtime")))]
    pub(crate) fn running_file(&self) -> Option<alloc::sync::Arc<String>> {
        let pool = self.pools.get(self.pool_now)?;
        if pool.func != super::cache::NO_FN { return self.functions.get(pool.func as usize).map(|f| f.1.path.clone()); }
        // a module or class body is this pool's chunk named by the module it binds
        let body = self.bodies.iter().find(|b| b.pool as usize == self.pool_now)?;
        match body.module as usize {
            0 => Some(self.chunk.path.clone()),
            module => self.scope_ids.iter().find_map(|(spec, &at)| (at == module).then(|| alloc::sync::Arc::new(spec.clone()))),
        }
    }

    /* the table index for module `spec` made empty on first use */
    pub(crate) fn module_scope(&mut self, spec: Option<&str>) -> usize {
        let Some(spec) = spec else { return 0 };
        if let Some(&i) = self.scope_ids.get(spec) { return i; }
        self.scopes.push(Globals::of(self.natives));
        let i = self.scopes.len() - 1;
        self.scope_ids.insert(spec.into(), i);
        i
    }

    /* a module binding by index or its builtin once unbound */
    #[inline]
    pub(crate) fn global_at(&self, module: usize, g: u32) -> Result<Val, VmErr> {
        let v = self.scopes[module].at(g);
        if v.is_undef() { self.unbound_global(module, g) } else { Ok(v) }
    }

    #[cold]
    #[inline(never)]
    fn unbound_global(&self, module: usize, g: u32) -> Result<Val, VmErr> {
        let scope = &self.scopes[module];
        let name = scope.name(g);
        if scope.closed { return Err(crate::lang::EngineError::Undefined(name.into()).into()); }
        let v = self.builtins.get(name).copied().ok_or_else(|| VmErr::from(crate::lang::EngineError::Undefined(name.into())))?;
        // a builtin keeps its value for the whole run so the slot remembers it
        if let Some(b) = scope.builtin.get(g as usize) { b.1.set(v); }
        Ok(v)
    }

    /* the local names of `chunk` and their slots alike for every frame of it */
    pub(crate) fn local_slots(&self, chunk: &SSAChunk) -> Vec<(String, usize)> {
        let kinds = self.fi_of(chunk).map(|fi| &self.fn_scope[fi].kinds);
        // slot order is the order the names were first bound with parameters first
        let mut slots: Vec<(&str, usize)> = slots_by_name(chunk).into_iter().collect();
        slots.sort_unstable_by_key(|&(_, slot)| slot);
        slots.into_iter()
            .filter(|&(n, slot)| !n.starts_with('#') && !kinds.is_some_and(|k| matches!(k.get(slot), Some(Kind::Global(_)))))
            .map(|(n, slot)| (String::from(n), slot))
            .collect()
    }

    /* the value a cell holds or `v` itself when it is not one */
    #[inline]
    pub(crate) fn deref(&self, v: Val) -> Val {
        match self.heap.try_get(v) { Some(&HeapObj::Cell(inner)) => inner, _ => v }
    }

    /* rebinds the cell in `cell` or reports that no cell is there */
    #[inline]
    pub(crate) fn set_cell(&mut self, cell: Val, v: Val) -> bool {
        match self.heap.try_get_mut(cell) {
            Some(HeapObj::Cell(inner)) => { *inner = v; true }
            _ => false,
        }
    }
}

impl<'a> VM<'a> {

    /* stores `v` into binding `g` of module `m` noting a native's name once */
    #[inline(never)]
    pub(crate) fn store_noted(&mut self, m: usize, g: u32, v: Val) {
        let scope = &mut self.scopes[m];
        scope.set_at(g, v);
        if let Some(id) = scope.names_native(g) && !self.native_rebound(id) { self.rebind_native(id); }
    }

    /* a true builtin's pre-registered value and None for user names so deleted bindings stay deleted */
    pub(crate) fn builtin_binding(&self, bare: &str) -> Option<Val> {
        if self.natives.function(bare).is_none() && self.natives.type_named(bare).is_none() {
            return None;
        }
        self.global(bare)
    }

    /* seed slots with `undef()` so LoadName can detect unbound names via a u64 compare */
    pub(crate) fn fill_builtins(&self, names: &[String]) -> Vec<Val> {
        let mut slots = vec![Val::undef(); names.len()];
        for (i, name) in names.iter().enumerate() {
            if let Some(v) = self.global_slot(name) {
                slots[i] = v;
            }
        }
        slots
    }

    /* an entry module binding by name or else the builtin under it */
    pub(crate) fn global(&self, bare: &str) -> Option<Val> {
        self.scopes[0].get(bare).or_else(|| self.builtins.get(bare).copied())
    }

    /* the global a slot starts from where a builtin also answers to its version-0 name */
    pub(crate) fn global_slot(&self, name: &str) -> Option<Val> {
        self.global(name).or_else(|| name.strip_suffix("_0").and_then(|bare| self.builtins.get(bare).copied()))
    }
}
