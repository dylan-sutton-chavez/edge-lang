use alloc::{string::{String, ToString}, vec::Vec};

use crate::code::OpCode;
use crate::syntax::chunk::{SSAChunk, ImportKind};
use crate::syntax::ssa::ssa_strip;

use super::VM;
use crate::value::*;

/* the events pushed to a run read through a cursor so receives take constant time */
#[derive(Default)]
pub(crate) struct Events {
    queue: Vec<Val>,
    at: usize,
}

impl Events {
    pub(crate) fn push(&mut self, v: Val) { self.queue.push(v); }

    /* the oldest event no receive took yet */
    pub(crate) fn take(&mut self) -> Option<Val> {
        // a queue read to its end clears so any other one has an event left
        if self.queue.is_empty() { return None; }
        // a taken slot holds no value so the collector can mark them all
        let v = core::mem::replace(self.queue.get_mut(self.at)?, Val::undef());
        self.at += 1;
        // a queue read to its end lets go and a long read prefix drops once
        if self.at == self.queue.len() { self.queue.clear(); self.at = 0; }
        else if self.at >= 64 && self.at * 2 >= self.queue.len() { self.queue.drain(..self.at); self.at = 0; }
        Some(v)
    }

    /* the events still to take which a snapshot keeps */
    pub(crate) fn unread(&self) -> &[Val] { &self.queue[self.at..] }

    /* every slot the collector marks where a taken one holds no value */
    pub(crate) fn slots(&self) -> &[Val] { &self.queue }

    pub(crate) fn of(queue: Vec<Val>) -> Events { Events { queue, at: 0 } }
}

/* a module's top-level bindings as its attrs in store order */
pub(crate) fn collect_module_attrs(chunk: &SSAChunk, bound: &super::scope::Globals) -> Vec<(String, Val)> {
    let mut attrs: Vec<(String, Val)> = Vec::new();
    let mut seen: crate::util::hash::FxHashSet<String> = crate::util::hash::FxHashSet::default();
    for ins in &chunk.instructions {
        if !matches!(ins.opcode, OpCode::StoreName) { continue; }
        let Some(name) = chunk.names.get(ins.operand as usize) else { continue; };
        let bare = ssa_strip(name).to_string();
        if !seen.insert(bare.clone()) { continue; }
        if let Some(v) = bound.get(&bare) { attrs.push((bare, v)); }
    }
    // names bound another way through a global statement follow in binding order
    for (name, v) in bound.iter() {
        if !name.starts_with('#') && name != bound.runtime_names().module_name && seen.insert(name.to_string()) { attrs.push((name.to_string(), v)); }
    }
    attrs
}

impl<'a> VM<'a> {

    /* flattens nested defs depth first with parent maps telling late-bound parent calls from closures */
    pub(crate) fn build_function_table(&mut self, chunk: &'a SSAChunk, parent_fi: Option<usize>, module_spec: Option<&str>) {
        let module = self.module_scope(module_spec);
        let at = self.body_at(chunk);
        self.bodies[at].module = module as u32;
        let mut indices = Vec::with_capacity(chunk.functions.len());
        for desc in chunk.functions.iter() {
            let global = self.functions.len() as u32;
            self.functions.push(desc);
            self.function_parents.push(parent_fi);
            self.fn_definer.push(chunk);
            let pool = self.pool_of(&desc.1);
            self.pools[pool].func = global;
            self.fn_pool.push(pool);
            self.fn_module.push(module_spec.map(String::from));
            let body = self.body_at(&desc.1);
            self.bodies[body].fi = global;
            // bare function name with its SSA suffix stripped for tracebacks
            let name = chunk.names.get(desc.3 as usize).map(|n| ssa_strip(n).to_string()).unwrap_or_default();
            self.function_qualnames.push(if desc.1.qualname.is_empty() { name.clone() } else { desc.1.qualname.clone() });
            self.function_names.push(name);
            indices.push(global);
            self.build_function_table(&desc.1, Some(global as usize), module_spec);
        }
        // a chunk met twice keeps the functions its first meeting gave
        if self.bodies[at].fns.is_empty() { self.bodies[at].fns = indices; }

        for sname in &chunk.names { self.register_builtin(crate::syntax::ssa::SsaName::parse_or_bare(sname).0); }
        for class_body in chunk.classes.iter() {
            let body = self.body_at(class_body);
            self.bodies[body].class = true;
            self.build_function_table(class_body, parent_fi, module_spec);
        }
        // recurse into code-module imports and each fn carries its spec so namespaces stay separate
        for entry in chunk.imports.iter() {
            if let ImportKind::Code(sub) = &entry.kind {
                self.build_function_table(sub, None, Some(&entry.spec));
            }
        }
    }

    /* the oldest event the host pushed that no receive took yet */
    pub(crate) fn take_event(&mut self) -> Option<Val> { self.event_queue.take() }

    /* hands `val` to a module body waiting on events or queues it */
    pub fn inject_event(&mut self, val: Val) {
        if self.root.as_ref().is_some_and(|h| matches!(h.state, CoroState::WaitingEvent)) { self.deliver_host_result(val); } else { self.event_queue.push(val); }
    }

    /* readies the module body awaiting host call `id` with `val` giving false without one */
    pub fn inject_host_result_by_id(&mut self, id: u64, val: Val) -> bool {
        let waits = self.root.as_ref().is_some_and(|h| matches!(h.state, CoroState::WaitingHostCall(w) if w == id));
        if waits { self.deliver_host_result(val); }
        waits
    }

    /* writes `val` where the module body waits in its innermost helper and readies it */
    fn deliver_host_result(&mut self, val: Val) {
        let Some(root) = self.root.as_mut() else { return };
        root.state = CoroState::Ready;
        if let HeapObj::Coroutine(c) = self.heap.get_mut(root.coro) && let Some(top) = c.wait_slot() { *top = val; }
    }

    /* raises `e` on resuming the module body awaiting host call `id` giving false without one */
    pub fn inject_host_error_by_id(&mut self, id: u64, e: VmErr) -> bool {
        match self.root.as_mut() {
            Some(root) if matches!(root.state, CoroState::WaitingHostCall(w) if w == id) => { root.state = CoroState::Raising(e, None); true }
            _ => false,
        }
    }

    /* yields `Preempted` every `n` back-edges and 0 disables it */
    pub fn set_preempt_interval(&mut self, n: usize) {
        self.preempt_every = n;
        self.preempt_left = n;
    }

    /* charges what runs next to this program and refuses a limit nothing meters */
    pub(crate) fn paying(&self) -> Result<crate::value::meter::Paying, VmErr> {
        if self.heap.unmetered() { return Err(VmErr::Runtime("a memory limit needs the metering allocator that edge::allocator!() installs")); }
        // a chunk names its natives by their places in the table it was compiled over
        if let Some(n) = self.chunk.natives() && !core::ptr::eq(n, self.natives) && let Some(e) = self.other_runtime(n) { return Err(e); }
        Ok(crate::value::meter::Paying::to(self.heap.meter()))
    }

    /* the refusal naming both runtimes of a chunk compiled for another */
    #[cold]
    #[inline(never)]
    fn other_runtime(&self, n: &crate::lang::Natives) -> Option<VmErr> {
        if n.runtime.is_empty() || n.runtime == self.rt.name { return None; }
        let on = if self.rt.name.is_empty() { "the empty runtime" } else { self.rt.name };
        Some(VmErr::Of(ErrKind::Runtime, crate::s!("a chunk compiled for ", str n.runtime, " cannot run on ", str on)))
    }

    /* queues a text event for the next wait as the wasm export does */
    pub fn push_event(&mut self, message: &str) -> Result<(), VmErr> {
        let _paying = crate::value::meter::Paying::to(self.heap.meter());
        let val = self.heap.alloc(HeapObj::Str(message.into()))?;
        self.inject_event(val);
        Ok(())
    }

    pub fn run(&mut self) -> Result<Val, VmErr> {
        let _paying = self.paying()?;
        self.error_byte_pos = None;
        // a run suspended to the host keeps its module body so this call resumes it
        if self.root.is_none() {
            self.turned = 0;
            // imports run before user code in the topological order the walk gives
            let mut in_progress: crate::util::hash::FxHashSet<String> = crate::util::hash::FxHashSet::default();
            self.init_modules(self.chunk, &mut in_progress)?;
            // the module body runs as a coroutine so top-level statements can wait on the host
            let slots = self.fill_builtins(&self.chunk.names);
            let coro = self.heap.alloc(HeapObj::Coroutine(Coro::fresh(slots, BodyRef::Module)))?;
            self.root = Some(CoroutineHandle { coro, state: CoroState::Ready });
        }
        let ran = self.drive_root();
        // a pause hands the host every generator's state where a snapshot finds it
        self.evict_all();
        ran.map_err(|e| self.word(e))
    }

    /* binds the native module `entry` names with its functions and constants and classes made here */
    fn bind_native(&mut self, entry: &crate::syntax::chunk::ImportEntry) -> Result<(), VmErr> {
        let ImportKind::Native { funcs, classes, consts } = &entry.kind else { return Ok(()) };
        let mut attrs: Vec<(String, Val)> = Vec::with_capacity(funcs.len() + classes.len() + consts.len());
        for b in funcs {
            let val = self.heap.host_value(b)?;
            attrs.push((b.name.clone(), val));
        }
        // materialise each constant by invoking its export once and binding the result as an attr
        for c in consts {
            let func = c.func.clone();
            let val = func(&mut self.heap, &[], None)?;
            attrs.push((c.name.clone(), val));
        }
        for c in classes {
            let mut methods: Vec<(alloc::boxed::Box<str>, Val)> = Vec::with_capacity(c.methods.len());
            for m in &c.methods {
                let mval = self.heap.host_value(m)?;
                methods.push((m.name.as_str().into(), mval));
            }
            let cls_val = self.heap.new_class(c.name.clone(), Vec::new(), methods)?;
            self.heap.users = true;
            attrs.push((c.name.clone(), cls_val));
        }
        let val = self.heap.alloc(HeapObj::Module(entry.spec.clone(), attrs))?;
        self.module_table.insert(entry.spec.clone(), val);
        Ok(())
    }

    /* the top level slots of module `spec` whose name keeps an entry guard from running */
    pub(crate) fn module_slots(&mut self, spec: &str, sub: &SSAChunk) -> Result<Vec<Val>, VmErr> {
        let slots = self.fill_builtins(&sub.names);
        let spec_val = self.heap.alloc(HeapObj::Str(spec.into()))?;
        let (module, names) = (self.chunk_module_id(sub), self.natives.names());
        self.scopes[module].set(names.module_name, spec_val);
        let doc = match crate::vm::slow::attr::docstring(sub) { Some(d) => self.heap.alloc(HeapObj::Str(d))?, None => Val::none() };
        self.scopes[module].set(names.doc, doc);
        Ok(slots)
    }

    /* inits each import once running code modules and binding native ones with `in_progress` catching cycles */
    pub(crate) fn init_modules(&mut self, chunk: &SSAChunk, in_progress: &mut crate::util::hash::FxHashSet<String>) -> Result<(), VmErr> {
        for entry in &chunk.imports {
            if self.module_table.contains_key(&entry.spec) { continue; }
            if !in_progress.insert(entry.spec.clone()) {
                return Err(VmErr::Runtime("circular import"));
            }
            match &entry.kind {
                ImportKind::Native { .. } => self.bind_native(entry)?,
                ImportKind::Code(sub_chunk) => {
                    self.init_modules(sub_chunk, in_progress)?;
                    let mut sub_slots = self.module_slots(&entry.spec, sub_chunk)?;
                    let module = self.chunk_module_id(sub_chunk);
                    self.exec(sub_chunk, &mut sub_slots)?;
                    let attrs = collect_module_attrs(sub_chunk, &self.scopes[module]);
                    let val = self.heap.alloc(HeapObj::Module(entry.spec.clone(), attrs))?;
                    self.module_table.insert(entry.spec.clone(), val);
                }
            }
            in_progress.remove(&entry.spec);
        }
        Ok(())
    }
}
