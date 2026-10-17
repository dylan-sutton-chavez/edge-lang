use alloc::{string::{String, ToString}, vec::Vec};

use crate::parser::{OpCode, SSAChunk, ssa_strip, ImportKind};

use super::VM;
use super::types::*;

/* A module's top-level bindings as its attrs, in store order. */
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
    // Names the code bound another way, through exec or a global statement, follow in binding order.
    for (name, v) in bound.iter() {
        if !name.starts_with('#') && name != "__name__" && seen.insert(name.to_string()) { attrs.push((name.to_string(), v)); }
    }
    attrs
}

impl<'a> VM<'a> {

    /* Flatten nested defs into one table (DFS), also build parent/body-pointer maps so `exec_call` can tell lexical-parent calls (late-bind) from foreign closures (captures stick). */
    pub(crate) fn build_function_table(&mut self, chunk: &'a SSAChunk, parent_fi: Option<usize>, module_spec: Option<&str>) {
        let module = self.module_scope(module_spec);
        self.chunk_module.insert(chunk as *const _, module);
        let mut indices = Vec::with_capacity(chunk.functions.len());
        for desc in chunk.functions.iter() {
            let global = self.functions.len() as u32;
            self.functions.push(desc);
            self.function_parents.push(parent_fi);
            self.fn_definer.push(chunk as *const _);
            let pool = self.pool_of(&desc.1);
            self.fn_pool.push(pool);
            self.fn_module.push(module_spec.map(String::from));
            self.body_to_fi.insert(&desc.1 as *const _, global as usize);
            // Bare function name (SSA suffix stripped) for tracebacks.
            let name = chunk.names.get(desc.3 as usize).map(|n| ssa_strip(n).to_string()).unwrap_or_default();
            self.function_qualnames.push(if desc.1.qualname.is_empty() { name.clone() } else { desc.1.qualname.clone() });
            self.function_names.push(name);
            indices.push(global);
            self.build_function_table(&desc.1, Some(global as usize), module_spec);
        }
        self.fn_index.push((chunk as *const _, indices));

        for sname in &chunk.names { self.register_builtin(crate::parser::SsaName::parse_or_bare(sname).0); }
        for class_body in chunk.classes.iter() {
            self.class_chunks.insert(class_body as *const _);
            self.build_function_table(class_body, parent_fi, module_spec);
        }
        // Recurse into code-module imports, each fn carries its spec so namespaces stay separate.
        for entry in chunk.imports.iter() {
            if let ImportKind::Code(sub) = &entry.kind {
                self.build_function_table(sub, None, Some(&entry.spec));
            }
        }
    }

    /* Hands `val` to the module body waiting in `receive()` and readies it, queues `val` otherwise. Shared by `push_event` and `run_push_event`. */
    pub fn inject_event(&mut self, val: Val) {
        if self.root.as_ref().is_some_and(|h| matches!(h.state, CoroState::WaitingEvent)) { self.deliver_host_result(val); } else { self.event_queue.push(val); }
    }

    /* Hands `val` to the module body waiting on host call `id` and readies it, false when nothing waits on `id`. */
    pub fn inject_host_result_by_id(&mut self, id: u64, val: Val) -> bool {
        let waits = self.root.as_ref().is_some_and(|h| matches!(h.state, CoroState::WaitingHostCall(w) if w == id));
        if waits { self.deliver_host_result(val); }
        waits
    }

    /* Writes `val` where the module body waits, the stack top of its innermost helper, and readies it. */
    fn deliver_host_result(&mut self, val: Val) {
        let Some(root) = self.root.as_mut() else { return };
        root.state = CoroState::Ready;
        if let HeapObj::Coroutine(c) = self.heap.get_mut(root.coro) {
            match c.wait_slot() { Some(top) => *top = val, None => c.stack.push(val) }
        }
    }

    /* Raises `e` where the module body waits on host call `id` on its next resume, false when nothing waits on `id`. */
    pub fn inject_host_error_by_id(&mut self, id: u64, e: VmErr) -> bool {
        match self.root.as_mut() {
            Some(root) if matches!(root.state, CoroState::WaitingHostCall(w) if w == id) => { root.state = CoroState::Raising(e, None); true }
            _ => false,
        }
    }

    /* Yield `Preempted` every `n` back-edges, 0 disables. */
    pub fn set_preempt_interval(&mut self, n: usize) {
        self.preempt_every = n;
        self.preempt_left = n;
    }

    /* Charges what runs next to this program, refusing a memory limit no `Metered` allocator counts. */
    pub(crate) fn paying(&self) -> Result<crate::value::meter::Paying, VmErr> {
        if self.heap.unmetered() { return Err(VmErr::Runtime("a memory limit needs compiler::value::meter::Metered as the global allocator")); }
        Ok(crate::value::meter::Paying::to(self.heap.meter()))
    }

    /* Push a string event onto the event queue, consumed by the next `receive()` call. Mirrors what `run_push_event` does for WASM hosts. */
    pub fn push_event(&mut self, message: &str) -> Result<(), VmErr> {
        let _paying = crate::value::meter::Paying::to(self.heap.meter());
        let val = self.heap.alloc(HeapObj::Str(message.into()))?;
        self.inject_event(val);
        Ok(())
    }

    pub fn run(&mut self) -> Result<Val, VmErr> {
        let _paying = self.paying()?;
        self.error_byte_pos = None;
        // A run that suspended to the host keeps its module body, so this call resumes it.
        if self.root.is_none() {
            // Imports run before user code, the walk giving their topological order.
            let mut in_progress: crate::util::hash::FxHashSet<String> = crate::util::hash::FxHashSet::default();
            self.init_modules(self.chunk, &mut in_progress)?;
            // The module body runs as a coroutine, so a top-level statement can wait on the host.
            let slots = self.fill_builtins(&self.chunk.names);
            let coro = self.heap.alloc(HeapObj::Coroutine(Coro::fresh(slots, BodyRef::Module)))?;
            self.root = Some(CoroutineHandle { coro, state: CoroState::Ready });
        }
        self.drive_root()
    }

    /* Init each unique import once, code modules run their top-level, native ones just bind. `in_progress` catches cycles cleanly. */
    pub(crate) fn init_modules(&mut self, chunk: &SSAChunk, in_progress: &mut crate::util::hash::FxHashSet<String>) -> Result<(), VmErr> {
        for entry in &chunk.imports {
            if self.module_table.contains_key(&entry.spec) { continue; }
            if !in_progress.insert(entry.spec.clone()) {
                return Err(VmErr::Runtime("circular import"));
            }
            match &entry.kind {
                ImportKind::Native { funcs, classes, consts } => {
                    let mut attrs: Vec<(String, Val)> = Vec::with_capacity(funcs.len() + classes.len() + consts.len());
                    for b in funcs {
                        let val = self.heap.alloc(HeapObj::Extern(b.clone()))?;
                        attrs.push((b.name.clone(), val));
                    }
                    // Materialise each constant by invoking its export once and binding the result as an attr.
                    for c in consts {
                        let func = c.func.clone();
                        let val = func(&mut self.heap, &[], None)?;
                        attrs.push((c.name.clone(), val));
                    }
                    // Synthesise a HeapObj::Class per native class, each method becomes an Extern Val on the class.
                    for c in classes {
                        let mut methods: Vec<(String, Val)> = Vec::with_capacity(c.methods.len());
                        for m in &c.methods {
                            let mval = self.heap.alloc(HeapObj::Extern(m.clone()))?;
                            methods.push((m.name.clone(), mval));
                        }
                        let cls_val = self.heap.alloc(HeapObj::Class(c.name.clone(), Vec::new(), alloc::rc::Rc::new(core::cell::RefCell::new(methods))))?;
                        attrs.push((c.name.clone(), cls_val));
                    }
                    let val = self.heap.alloc(HeapObj::Module(entry.spec.clone(), attrs))?;
                    self.module_table.insert(entry.spec.clone(), val);
                }
                ImportKind::Code(sub_chunk) => {
                    self.init_modules(sub_chunk, in_progress)?;
                    let mut sub_slots = self.fill_builtins(&sub_chunk.names);
                    // `__name__` is the module spec, so a main guard skips on import.
                    let spec_val = self.heap.alloc(HeapObj::Str(entry.spec.clone()))?;
                    let module = self.chunk_module_id(sub_chunk);
                    self.scopes[module].set("__name__", spec_val);
                    let doc = match crate::vm::opcodes::attr_lookup::docstring(sub_chunk) { Some(d) => self.heap.alloc(HeapObj::Str(d))?, None => Val::none() };
                    self.scopes[module].set("__doc__", doc);
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
