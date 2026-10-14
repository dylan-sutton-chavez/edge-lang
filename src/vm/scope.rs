use alloc::{string::String, vec, vec::Vec};

use crate::parser::{OpCode, SSAChunk, ssa_strip};
use crate::value::hash_key;
use crate::util::hash::FxHashMap;

use super::VM;
use super::types::*;

/* A module's bindings, each name keeping one index for the whole run. */
#[derive(Default, Clone)]
pub(crate) struct Globals {
    /* Each name's index, found by comparing against `names`. */
    index: hashbrown::HashTable<u32>,
    names: Vec<String>,
    vals: Vec<Val>,
    /* The builtin each name belongs to, and the builtin value a read found while the name was unbound. */
    builtin: Vec<(Option<crate::value::NativeFnId>, core::cell::Cell<Val>)>,
    /* Set for text run under a `__builtins__` of its own, whose unbound names find no builtin. */
    pub(crate) closed: bool,
}

impl Globals {
    fn find(&self, name: &str) -> Option<u32> {
        self.index.find(hash_key(name), |&i| self.names[i as usize] == name).copied()
    }

    /* The index of `name`, unbound until something binds it. */
    pub fn id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.find(name) { return i; }
        let i = self.vals.len() as u32;
        self.names.push(name.into());
        self.vals.push(Val::undef());
        self.builtin.push((crate::value::NativeFnId::from_name(name), core::cell::Cell::new(Val::undef())));
        let names = &self.names;
        self.index.insert_unique(hash_key(name), i, |&j| hash_key(&names[j as usize]));
        i
    }

    pub fn get(&self, name: &str) -> Option<Val> {
        self.find(name).map(|i| self.vals[i as usize]).filter(|v| !v.is_undef())
    }

    pub fn set(&mut self, name: &str, v: Val) {
        let i = self.id(name);
        self.vals[i as usize] = v;
    }

    #[inline(always)]
    pub fn at(&self, i: u32) -> Val { self.vals.get(i as usize).copied().unwrap_or(Val::undef()) }

    #[inline(always)]
    pub fn set_at(&mut self, i: u32, v: Val) { if let Some(s) = self.vals.get_mut(i as usize) { *s = v; } }

    pub fn name(&self, i: u32) -> &str { self.names.get(i as usize).map_or("", |n| n.as_str()) }

    /* The builtin last read under index `i` while it was unbound, undef before the first read. */
    #[inline]
    pub fn fallback(&self, i: u32) -> Val { self.builtin.get(i as usize).map_or(Val::undef(), |b| b.1.get()) }

    #[inline]
    pub fn names_builtin(&self, i: u32) -> Option<crate::value::NativeFnId> { self.builtin.get(i as usize).and_then(|b| b.0) }

    /* Every name with its value, unbound ones undef, in first-named order. */
    pub fn entries(&self) -> impl Iterator<Item = (&str, Val)> + '_ { self.names.iter().map(String::as_str).zip(self.vals.iter().copied()) }

    /* Bound names with their values. */
    pub fn iter(&self) -> impl Iterator<Item = (&str, Val)> + '_ { self.entries().filter(|(_, v)| !v.is_undef()) }

    /* Unbinds every name, each keeping its index for the code that reads it. */
    pub fn unbind_all(&mut self) { self.vals.fill(Val::undef()); }

    pub fn from_entries(entries: Vec<(String, Val)>) -> Self {
        let mut g = Self::default();
        for (name, v) in entries { g.set(&name, v); }
        g
    }
}

/* What a frame slot holds, fixed for the chunk before it runs. */
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Kind {
    Local,
    // A cell a nested function shares, read and written through it.
    Cell,
    // A module binding by its index, the builtins under it.
    Global(u32),
}

/* How a function body's names resolve, computed once from the code. */
#[derive(Default, Clone)]
pub(crate) struct FnScope {
    pub kinds: alloc::rc::Rc<[Kind]>,
    /* Slots that start as a fresh cell holding what the call bound there. */
    pub cellvars: Vec<usize>,
    /* The cells a definition takes, by its slot, the definer's slot and name. */
    pub freevars: Vec<(usize, Option<usize>, String)>,
    /* Globals the body reads. */
    pub reads: Vec<String>,
    /* Whether it declares a global or nonlocal name, which makes every call impure. */
    pub declares: bool,
}

fn canon_of(chunk: &SSAChunk, i: usize) -> usize {
    chunk.alias_groups.get(i).and_then(|g| g.first().copied()).map_or(i, |c| c as usize)
}

/* The slot each bare name's versions share, preferring a versioned name. */
fn slots_by_name(chunk: &SSAChunk) -> FxHashMap<&str, usize> {
    let mut map = FxHashMap::default();
    for versioned in [true, false] {
        for (i, n) in chunk.names.iter().enumerate() {
            if crate::parser::SsaName::parse(n).is_some() == versioned { map.entry(ssa_strip(n)).or_insert(canon_of(chunk, i)); }
        }
    }
    map
}

/* Bare names a body binds by store, delete or parameter, sorted, nonlocals aside. */
fn binds<'c>(params: &'c [String], body: &'c SSAChunk, bare: &[&'c str]) -> Vec<&'c str> {
    let mut set: Vec<&str> = params.iter().map(|p| crate::parser::types::param_base_name(p)).collect();
    for ins in &body.instructions {
        if matches!(ins.opcode, OpCode::StoreName | OpCode::Phi | OpCode::Del) && let Some(&n) = bare.get(ins.operand as usize) { set.push(n); }
    }
    if !is_comp(params) { walrus_names(body, &mut set); }
    set.sort_unstable();
    set.dedup();
    set.retain(|n| !body.nonlocals.iter().any(|x| x == n));
    set
}

pub(crate) fn is_comp(params: &[String]) -> bool { matches!(params, [p] if p == crate::parser::COMP_ARG) }

/* The walrus targets of the comprehensions in `body`, which the function holding them binds. */
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
    /* Resolves each free name of functions[start..] to an enclosing cell or a global. */
    pub(crate) fn analyze_scopes(&mut self, start: usize) {
        let end = self.functions.len();
        let functions = self.functions.clone();
        // Each body's names without their versions, stripped once.
        let bare: Vec<Vec<&str>> = functions.iter().map(|(_, body, _, _)| body.names.iter().map(|n| ssa_strip(n)).collect()).collect();
        let locals: Vec<Vec<&str>> = functions.iter().zip(&bare).map(|((params, body, _, _), b)| binds(params, body, b)).collect();
        let owns = |f: usize, n: &str| locals[f].binary_search(&n).is_ok();
        let mut cells: Vec<Vec<&str>> = vec![Vec::new(); end];
        let mut frees: Vec<Vec<&str>> = vec![Vec::new(); end];
        let mut reads: Vec<Vec<&str>> = vec![Vec::new(); end];
        for fi in start..end {
            let body = &functions[fi].1;
            for ins in &body.instructions {
                // A name declared `global` reads and writes the module binding wherever it appears.
                if matches!(ins.opcode, OpCode::LoadGlobal | OpCode::StoreGlobal) {
                    if let Some(n) = body.names.get(ins.operand as usize) && !reads[fi].contains(&n.as_str()) { reads[fi].push(n); }
                    continue;
                }
                if !matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi) { continue; }
                let Some(&n) = bare[fi].get(ins.operand as usize) else { continue };
                if n.starts_with('#') || owns(fi, n) { continue; }
                // The nearest enclosing function binding it owns the cell, those between pass it on.
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
            let kinds: Vec<Kind> = body.names.iter().zip(&bare[fi]).map(|(name, &bare)| {
                if bare.starts_with('#') { Kind::Local }
                else if reads[fi].contains(&name.as_str()) { Kind::Global(self.scopes[module].id(name)) }
                else if cells[fi].contains(&bare) || frees[fi].contains(&bare) { Kind::Cell }
                else if owns(fi, bare) { Kind::Local }
                else if reads[fi].contains(&bare) { Kind::Global(self.scopes[module].id(bare)) }
                else { Kind::Local }
            }).collect();
            let cellvars: Vec<usize> = (0..body.names.len())
                .filter(|&i| canon_of(body, i) == i && cells[fi].contains(&bare[fi][i]))
                .collect();
            let definer = self.fn_definer[fi];
            // SAFETY each definer is a chunk borrowed for the VM's lifetime.
            let definer: &SSAChunk = unsafe { &*definer };
            let definer_slots = self.body_to_fi.contains_key(&(definer as *const SSAChunk)).then(|| slots_by_name(definer));
            // Every slot naming a free variable takes its cell, a declaration's bare name included.
            let mut freevars: Vec<(usize, Option<usize>, String)> = (0..body.names.len())
                .filter(|&i| canon_of(body, i) == i && frees[fi].contains(&bare[fi][i]))
                .map(|i| {
                    let n = bare[fi][i];
                    (i, definer_slots.as_ref().and_then(|m| m.get(n).copied()), String::from(n))
                })
                .collect();
            freevars.sort_unstable_by_key(|f| f.0);
            let declares = body.instructions.iter().any(|i| matches!(i.opcode, OpCode::Global | OpCode::Nonlocal));
            scopes.push(FnScope { kinds: kinds.into(), cellvars, freevars, reads: reads[fi].iter().map(|&n| String::from(n)).collect(), declares });
        }
        self.fn_scope.truncate(start);
        self.fn_scope.extend(scopes);
    }

    /* The module table a function reads its globals from. */
    pub(crate) fn fn_module_id(&self, fi: usize) -> usize {
        self.fn_module.get(fi).and_then(|m| m.as_ref()).and_then(|spec| self.scope_ids.get(spec).copied()).unwrap_or(0)
    }

    /* The module table a chunk's code binds into. */
    pub(crate) fn chunk_module_id(&self, chunk: &SSAChunk) -> usize {
        self.chunk_module.get(&(chunk as *const SSAChunk)).copied().unwrap_or(0)
    }

    /* The table index for module `spec`, made empty on first use. */
    pub(crate) fn module_scope(&mut self, spec: Option<&str>) -> usize {
        let Some(spec) = spec else { return 0 };
        if let Some(&i) = self.scope_ids.get(spec) { return i; }
        self.scopes.push(Globals::default());
        let i = self.scopes.len() - 1;
        self.scope_ids.insert(spec.into(), i);
        i
    }

    /* A module binding by index, its builtin once unbound. */
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
        if scope.closed { return Err(VmErr::Name(name.into())); }
        let v = self.builtins.get(name).copied().ok_or_else(|| VmErr::Name(name.into()))?;
        // A builtin keeps its value for the whole run, so the slot remembers it.
        if let Some(b) = scope.builtin.get(g as usize) { b.1.set(v); }
        Ok(v)
    }

    /* The bound locals of the frame running `chunk`, cells read through, which eval and exec get a copy of. */
    pub(crate) fn frame_locals(&self, chunk: &SSAChunk) -> Vec<(String, Val)> {
        let kinds = self.body_to_fi.get(&(chunk as *const SSAChunk)).map(|&fi| &self.fn_scope[fi].kinds);
        slots_by_name(chunk).into_iter()
            .filter(|&(n, slot)| !n.starts_with('#') && !kinds.is_some_and(|k| matches!(k.get(slot), Some(Kind::Global(_)))))
            .filter_map(|(n, slot)| {
                let v = self.deref(self.regs.get(self.base + slot).copied()?);
                (!v.is_undef()).then(|| (String::from(n), v))
            })
            .collect()
    }

    /* The value a cell holds, or `v` itself when it is not one. */
    #[inline]
    pub(crate) fn deref(&self, v: Val) -> Val {
        match self.heap.try_get(v) { Some(&HeapObj::Cell(inner)) => inner, _ => v }
    }

    /* Rebinds the cell in `cell`, or reports that no cell is there. */
    #[inline]
    pub(crate) fn set_cell(&mut self, cell: Val, v: Val) -> bool {
        match self.heap.try_get_mut(cell) {
            Some(HeapObj::Cell(inner)) => { *inner = v; true }
            _ => false,
        }
    }
}
