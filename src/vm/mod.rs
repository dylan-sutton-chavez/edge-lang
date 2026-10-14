/* The value model moved to `crate::value`, kept here so existing embedder paths keep resolving. */
#[doc(hidden)]
pub use crate::value as types;
/* The optimizer moved to `crate::optimizer`, kept here so existing embedder paths keep resolving. */
#[doc(hidden)]
pub use crate::optimizer;

mod cache;
mod lower;
mod registers;
pub(crate) mod scope;
mod sites;
mod value_ops;
mod format_spec;
pub(crate) mod globals;
pub(crate) mod opcodes;
pub(crate) mod methods;
pub mod snapshot;

mod chain;
mod dispatch;
mod gc;
mod helpers;
mod init;
mod keys;

use crate::parser::{SSAChunk, builtin_type};
use crate::util::hash::FxHashMap as HashMap;

pub use types::{Val, HeapObj, HeapPool, VmErr, Limits};

use types::*;
use cache::{CachePool, Templates};
use alloc::{string::{String, ToString}, vec::Vec};

pub(crate) use types::ExceptionFrame;

#[derive(Clone, Copy)]
pub(crate) enum ParamKind { Normal, Star, DoubleStar, KwOnly }

/* Side-channel state passed between opcodes in one dispatch frame, grouped for auditability. */
pub(crate) struct Pending {
    /* Star/double-star spreads bump the argument count of the call whose first spread opened the frame. */
    pub pos_delta: i32,
    pub kw_delta: i32,
    // Saved enclosing spread deltas (BeginArgs).
    pub delta_save: alloc::vec::Vec<(i32, i32)>,
    /* The current call's ip, turned into a byte offset only for a traceback. */
    pub call_ip: Option<u32>,
    /* Wakeup deadline set by `sleep()` and consumed by the scheduler. */
    pub sleep_until_ns: Option<u64>,
    /* Set by `receive()` on empty queue, transitions the coro to `WaitingEvent`. */
    pub event_wait_request: bool,
    /* Set by `call_extern` on deferred native, transitions the coro to `WaitingHostCall`. */
    pub host_call_request: bool,
    /* Correlation id of the deferred call, read by `scheduler_step` into `WaitingHostCall(id)`. */
    pub host_call_id: u64,
    /* Set by `call_run` / `call_gather` / `call_with_timeout` when they yield, transitions the outer to `WaitingForChildren`. */
    pub waiting_for_children: Option<(Vec<Val>, types::WaitKind)>,
    /* Lifted ExcInstance from `raise X(...)` so `except X as e` binds the real instance. */
    pub exc_val: Option<Val>,
    /* `(class, self)` for the next user-function call when it's invoked as a method, populated by method-dispatch paths and consumed by `run_body_with_frame`. */
    pub method_binding: Option<(Val, Val)>,
    /* Set at preempt, `top_loop` yields `Preempted`. */
    pub preempt_request: bool,
}

impl Pending {
    const fn new() -> Self {
        Self {
            pos_delta: 0,
            kw_delta: 0,
            delta_save: alloc::vec::Vec::new(),
            call_ip: None,
            sleep_until_ns: None,
            event_wait_request: false,
            host_call_request: false,
            host_call_id: 0,
            waiting_for_children: None,
            exc_val: None,
            method_binding: None,
            preempt_request: false,
        }
    }
}

/* `bare_name -> [(version, slot), ...]` for one chunk's `chunk.names`. */
pub(crate) type NameVersionIndex = crate::util::hash::FxHashMap<String, Vec<(i64, usize)>>;

pub struct VM<'a> {
    pub(crate) stack: Vec<Val>,
    /* The registers of every running frame, each frame a window from its base to the next. */
    pub(crate) regs: Vec<Val>,
    /* Where the running frame starts in `regs`. */
    pub(crate) base: usize,
    pub(crate) heap: HeapPool,
    pub(crate) iter_stack: Vec<IterFrame>,
    pub(crate) yields: Vec<Val>,
    pub(crate) chunk: &'a SSAChunk,
    /* Each module's bindings, the entry module first, by the index each name keeps. */
    pub(crate) scopes: Vec<scope::Globals>,
    /* Module spec -> its index in `scopes`. */
    pub(crate) scope_ids: HashMap<String, usize>,
    /* Chunk -> the module whose bindings its code reads. */
    pub(crate) chunk_module: HashMap<*const SSAChunk, usize>,
    /* How each function body's names resolve, indexed by fi. */
    pub(crate) fn_scope: Vec<scope::FnScope>,
    /* The chunk each function is defined in, whose frame its closure cells come from. */
    pub(crate) fn_definer: Vec<*const SSAChunk>,
    /* Class bodies, whose names stay a namespace in slots. */
    pub(crate) class_chunks: crate::util::hash::FxHashSet<*const SSAChunk>,
    /* The cells around each running class body, which its methods close over. */
    pub(crate) class_cells: Vec<Vec<(String, Val)>>,
    /* The builtins under the globals, keyed by their static names and never rebound. */
    pub(crate) builtins: HashMap<&'static str, Val>,
    pub(crate) templates: Templates,
    pub(crate) budget: usize,
    pub(crate) depth: usize,
    pub(crate) max_calls: usize,
    pub(crate) observed_impure: Vec<bool>,
    // C3 method-resolution order per class, keyed by the class Val's heap bits. Computed once at MakeClass (the class graph is a static DAG). A reused slot is overwritten by its new class, so stale entries are never read (lookup checks HeapObj::Class first). Not a GC root because MRO members stay reachable via the class's own `bases`.
    pub(crate) mro_cache: HashMap<u64, alloc::rc::Rc<Vec<Val>>>,
    pub(crate) exception_stack: Vec<ExceptionFrame>,
    /* Active finally/with cleanup reasons (innermost last), EndFinally pops one per body. */
    pub(crate) unwind_stack: Vec<types::Unwind>,
    /* Exceptions being handled with where each was raised, innermost last, a bare `raise` re-raises the last. */
    pub(crate) handling: Vec<(Val, Option<u32>)>,
    pub(crate) functions: Vec<&'a (Vec<String>, SSAChunk, u16, u16)>,
    // (chunk_ptr, global fn ids), linear scan over a tiny list avoids HashMap monomorphization.
    pub(crate) fn_index: Vec<(*const SSAChunk, Vec<u32>)>,
    // function_parents maps to the lexical enclosing fi (None at module level), body_to_fi maps chunk->fi.
    pub(crate) function_parents: Vec<Option<usize>>,
    pub(crate) body_to_fi: HashMap<*const SSAChunk, usize>,
    pub(crate) param_slots: Vec<Vec<(ParamKind, usize)>>,
    /* A function's positional count when every parameter is plain. */
    pub(crate) simple_arity: Vec<Option<usize>>,
    pub(crate) slot_templates: Vec<Vec<Val>>,
    /* Deduped template values, templates are static after init, so the GC marks this flat list instead of every per-function template. */
    pub(crate) template_roots: Vec<Val>,
    /* Whether `fi` may memoize, a call that shows its result can change turns it off. */
    pub(crate) memo_ok: Vec<bool>,
    /* Coroutines currently inside `resume_coroutine`, re-entry raises like Python's already-executing guard. Transient, never snapshotted. */
    pub(crate) executing_coros: Vec<u64>,
    /* Bumped by any class member change, voiding every site keyed on a class. */
    pub(crate) class_epoch: u32,
    /* Set by a hot loop in unlowered code, its frame then carries on lowered. */
    pub(crate) tier_up: bool,
    /* A call the register loop made that failed, raised at that call. */
    pub(crate) reg_error: Option<VmErr>,
    /* What calling each class needs, valid while the class epoch holds. */
    pub(crate) ctors: crate::util::hash::FxHashMap<u64, Ctor>,
    /* True once a builtin name is rebound, fused calls then check module bindings first. */
    pub(crate) builtins_rebound: bool,
    pub(crate) is_async: Vec<bool>,
    pub(crate) default_slots: Vec<Vec<(usize, Val)>>,
    /* Each chunk's code and caches, indexed through `pool_ids` or `fn_pool`. */
    pub(crate) pools: Vec<CachePool>,
    pub(crate) pool_ids: HashMap<*const SSAChunk, usize>,
    pub(crate) fn_pool: Vec<usize>,
    /* Per-chunk `bare -> [(version, slot)]` index, telling a name bound once from rebound. */
    pub(crate) chunk_name_versions: HashMap<*const SSAChunk, NameVersionIndex>,
    pub(crate) with_stack: Vec<Val>,
    /* GC roots for operands popped off the stack but still read after a dunder call that can collect. */
    pub(crate) temp_roots: Vec<Val>,
    /* Weak flags for lists produced by iterator builtins (iter/map/filter/zip/enumerate/reversed), next() drains only these and plain lists raise TypeError. Weak so a swept slot can never alias a fresh list. */
    pub(crate) pending: Pending,
    /* Monotonic correlation id handed to each deferred host call, matched by `set_host_result_by_id`. */
    pub(crate) next_host_call_id: u64,
    /* Sync helpers that suspended during the current resume, drained into the active Coroutine on yield-save. Lives at VM scope (not `Pending`) because it propagates across dispatch frames, not within one. */
    pub(crate) pending_sync_frames: Vec<types::SyncFrame>,
    /* Overrides `exec`'s captured `exc_base`. Set by `resume_coroutine` to the level *before* restored exception frames so dispatch's handler search includes them, consumed once at exec entry. */
    pub(crate) pending_exec_exc_base: Option<usize>,
    /* Back-edges until the next preempt, 0 disables. */
    pub(crate) preempt_left: usize,
    pub(crate) preempt_every: usize,
    /* True while this `exec` frame can unwind. */
    pub(crate) frame_safe: bool,
    pub(crate) pending_exec_safe: bool,
    /* Cancellation drive flag and the one-shot park-point raise, both live within one resume, never snapshotted. */
    pub(crate) cancelling: bool,
    pub(crate) resume_raise: Option<VmErr>,
    pub(crate) yielded: bool,
    /* Return value of the most recently exhausted iterator, read by `LoadYieldFrom` so `x = yield from it` evaluates to the subiterator's StopIteration value. */
    pub(crate) yield_from_value: Val,
    pub(crate) resume_ip: usize,
    pub output: Vec<String>,
    /* True when the last `output` entry is an unterminated line (print(end="") left it open). */
    pub(crate) output_open: bool,
    pub print_hook: Option<fn(&str)>,
    /* Host scheduler handoff for `send()`, unset or false raises the missing scheduler error. */
    pub send_hook: Option<fn(&str, &str) -> bool>,
    pub input_buffer: Vec<String>,
    pub event_queue: Vec<Val>,
    pub strict_input: bool,
    /* Byte offset of the deepest propagating error in the last run(). */
    pub(crate) error_byte_pos: Option<u32>,
    /* spec -> Module Val, populated by `init_modules`, read by LoadModule / import_module(). */
    pub(crate) module_table: HashMap<String, Val>,
    /* `fi -> module spec`, scopes the free-load fallback to the fn's own module. */
    pub(crate) fn_module: Vec<Option<String>>,
    /* Function names parallel to `functions`, consumed by traceback render. Empty = lambda. */
    pub(crate) function_names: Vec<String>,
    /* The frames an error unwound through, innermost first, built only as it unwinds and read by the traceback renderer. */
    pub(crate) call_stack: Vec<CallFrame>,
    /* `(depth, class, self)` of each running method, which zero-argument `super()` reads. */
    pub(crate) bindings: Vec<(usize, Val, Val)>,
    /* Cooperative scheduler for `run` / `gather` / `with_timeout`, one handle per coroutine. Single-driver model where only `top_loop` drives this, async builtins yield instead of recursing. */
    pub(crate) scheduler: Vec<CoroutineHandle>,
    /* Count of scheduler entries in `WaitingForChildren`, gates the sweep so the common (no-nested-run) tick is one comparison. */
    pub(crate) waiting_for_children_count: usize,
    /* Host-installed wall-clock (ns). */
    pub(crate) time_hook: Option<fn() -> u64>,
    /* Fallback monotonic counter when `time_hook` is None, reset each `run()`. */
    pub(crate) virtual_clock_ns: u64,
    /* Each instruction a chunk executed, one bit per ip, read through `ran`. */
    #[cfg(feature = "coverage")]
    pub(crate) executed: HashMap<*const SSAChunk, Vec<u64>>,
}

/* What calling a class needs, its `__init__`, exception kind and attribute count. */
#[derive(Clone, Copy)]
pub(crate) struct Ctor {
    pub epoch: u32,
    pub init: Option<(Val, Val)>,
    pub exception: bool,
    pub attrs: usize,
    // The builtin `dict` or `list` the class derives from, which then builds its instances.
    pub native: Option<Val>,
}

impl<'a> VM<'a> {
    pub fn new(chunk: &'a SSAChunk) -> Self { Self::with_limits(chunk, Limits::sandbox()) }

    /* Whether `chunk.instructions[ip]` ran in this VM, still answered after a run that raised. */
    #[cfg(feature = "coverage")]
    pub fn ran(&self, chunk: &SSAChunk, ip: usize) -> bool {
        self.executed.get(&(chunk as *const _)).and_then(|bits| bits.get(ip / 64)).is_some_and(|word| word & (1 << (ip % 64)) != 0)
    }

    pub fn with_limits(chunk: &'a SSAChunk, limits: Limits) -> Self {
        let mut vm = Self {
            stack: Vec::with_capacity(256),
            regs: Vec::with_capacity(256),
            base: 0,
            iter_stack: Vec::with_capacity(16),
            yields: Vec::new(),
            chunk,
            heap: HeapPool::new(limits.memory),
            scopes: alloc::vec![scope::Globals::default()],
            scope_ids: HashMap::default(),
            chunk_module: HashMap::default(),
            fn_scope: Vec::new(),
            fn_definer: Vec::new(),
            class_chunks: Default::default(),
            class_cells: Vec::new(),
            builtins: HashMap::default(),
            templates: Templates::new(),
            budget: limits.ops,
            depth: 0,
            max_calls: MAX_CALLS,
            with_stack: Vec::new(),
            temp_roots: Vec::new(),
            pending: Pending::new(),
            next_host_call_id: 0,
            pending_sync_frames: Vec::new(),
            pending_exec_exc_base: None,
            preempt_left: 0,
            preempt_every: 0,
            frame_safe: false,
            pending_exec_safe: false,
            cancelling: false,
            resume_raise: None,
            yielded: false,
            yield_from_value: Val::none(),
            resume_ip: 0,
            strict_input: true,
            output: Vec::new(),
            output_open: false,
            print_hook: None,
            send_hook: None,
            input_buffer: Vec::new(),
            event_queue: Vec::new(),
            observed_impure: Vec::new(),
            mro_cache: HashMap::default(),
            exception_stack: Vec::new(),
            unwind_stack: Vec::new(),
            handling: Vec::new(),
            error_byte_pos: None,
            module_table: HashMap::default(),
            fn_module: Vec::new(),
            function_names: Vec::new(),
            call_stack: Vec::new(),
            bindings: Vec::new(),
            scheduler: Vec::new(),
            waiting_for_children_count: 0,
            time_hook: None,
            virtual_clock_ns: 0,
            #[cfg(feature = "coverage")]
            executed: HashMap::default(),
            functions: Vec::new(),
            fn_index: Vec::new(),
            function_parents: Vec::new(),
            body_to_fi: HashMap::default(),
            param_slots: Vec::new(),
            simple_arity: Vec::new(),
            slot_templates: Vec::new(),
            template_roots: Vec::new(),
            memo_ok: Vec::new(),
            executing_coros: Vec::new(),
            class_epoch: 0,
            tier_up: false,
            reg_error: None,
            ctors: Default::default(),
            builtins_rebound: false,
            is_async: Vec::new(),
            default_slots: Vec::new(),
            pools: Vec::new(),
            pool_ids: HashMap::default(),
            fn_pool: Vec::new(),
            chunk_name_versions: HashMap::default(),
        };
        vm.build_function_table(chunk, None, None);
        vm.index_functions(0);
        // Entry chunk's `__name__` is "__main__", inserted before slot_templates is built.
        if let Ok(main_name) = vm.heap.alloc(HeapObj::Str("__main__".to_string())) {
            vm.builtins.insert("__name__", main_name);
        }
        // `NotImplemented` singleton, dunders return it to delegate to the reflected operator.
        if let Ok(ni) = vm.heap.alloc(HeapObj::NotImplemented) {
            vm.builtins.insert("NotImplemented", ni);
        }
        // Slot templates built after all globals are registered.
        vm.index_templates(0);
        vm
    }

    /* Derived per-function tables for functions[start..], the REPL re-invokes this to extend them for each adopted chunk. */
    pub(crate) fn index_functions(&mut self, start: usize) {
        let end = self.functions.len();
        let new: Vec<Vec<(ParamKind, usize)>> = (start..end).map(|fi| {
            let (params, body, _, _) = self.functions[fi];
            params.iter().map(|p| {
                // `~` prefix marks kw-only parameters (after a lone `*`).
                let kind = if p.starts_with("**") {
                    ParamKind::DoubleStar
                } else if p.starts_with('*') {
                    ParamKind::Star
                } else if p.starts_with('~') {
                    ParamKind::KwOnly
                } else {
                    ParamKind::Normal
                };
                // A parameter binds the first version of its bare name.
                let bare = crate::parser::types::param_base_name(p);
                let slot = body.names.iter().position(|n| n.strip_suffix("_0") == Some(bare)).unwrap_or(usize::MAX);
                (kind, slot)
            }).collect()
        }).collect();
        self.param_slots.truncate(start);
        self.param_slots.extend(new);
        let new: Vec<Option<usize>> = (start..end).map(|fi| {
            let params = &self.param_slots[fi];
            params.iter().all(|&(k, s)| matches!(k, ParamKind::Normal) && s != usize::MAX).then_some(params.len())
        }).collect();
        self.simple_arity.truncate(start);
        self.simple_arity.extend(new);

        // Default-slot table of (slot, placeholder) entries the call path overwrites.
        let new: Vec<Vec<(usize, Val)>> = (start..end).map(|fi| {
            let (params, _, n_defaults, _) = self.functions[fi];
            if *n_defaults == 0 { return Vec::new(); }
            // Defaults map to `=`-marked params in source order, not the trailing N.
            params.iter().zip(self.param_slots[fi].iter())
                .filter(|(p, _)| p.ends_with('='))
                .map(|(_, &(_, slot))| (slot, Val::none()))
                .collect()
        }).collect();
        self.default_slots.truncate(start);
        self.default_slots.extend(new);
        self.analyze_scopes(start);
    }

    /* Templates read `globals`, so they build after builtin registration, rebuilding the deduped roots is cheap. */
    pub(crate) fn index_templates(&mut self, start: usize) {
        // Every slot starts unbound, so a local read before its first store fails.
        let new: Vec<Vec<Val>> = (start..self.functions.len()).map(|fi| alloc::vec![Val::undef(); self.functions[fi].1.names.len()]).collect();
        self.slot_templates.truncate(start);
        self.slot_templates.extend(new);
        // Cells change behind a cached result, and an import reads other globals.
        let new: Vec<bool> = (start..self.functions.len()).map(|fi| {
            let scope = &self.fn_scope[fi];
            self.functions[fi].1.is_pure && scope.freevars.is_empty()
                && (self.fn_module[fi].is_none() || scope.reads.iter().all(|bare| self.function_names.get(fi).is_some_and(|n| n == bare)))
        }).collect();
        self.memo_ok.truncate(start);
        self.memo_ok.extend(new);
        let mut seen: crate::util::hash::FxHashSet<u64> = crate::util::hash::FxHashSet::default();
        self.template_roots = self.slot_templates.iter().flatten()
            .filter(|v| !v.is_undef() && seen.insert(v.0))
            .copied()
            .collect();
    }

    /* A builtin name the program binds or deletes voids every result memoized under the old binding. */
    pub(crate) fn note_builtin_binding(&mut self, bare: &str) {
        if NativeFnId::from_name(bare).is_some() { self.rebind_builtin(); }
    }

    /* A builtin's name holds a program value, fused calls check bindings, memos clear. */
    pub(crate) fn rebind_builtin(&mut self) {
        self.builtins_rebound = true;
        self.templates.clear();
    }

    /* Gives `bare` a heap slot when it names a builtin, so a program pays only for the builtins it uses. */
    pub(crate) fn register_builtin(&mut self, bare: &str) {
        if self.builtins.contains_key(bare) { return; }
        // Type names stay Type objects even when a NativeFn shares them.
        let (name, obj) = if let Some(name) = builtin_type(bare) {
            // `IOError` is the `OSError` class under a second name.
            if name == "IOError" {
                self.register_builtin("OSError");
                if let Some(&v) = self.builtins.get("OSError") { self.builtins.insert(name, v); }
                return;
            }
            (name, HeapObj::Type(name.to_string()))
        } else if let Some(id) = NativeFnId::from_name(bare) {
            (id.name(), HeapObj::NativeFn(id))
        } else {
            return;
        };
        if let Ok(v) = self.heap.alloc(obj) { self.builtins.insert(name, v); }
    }

    /* For the REPL, adopt `chunk` as the new entry module, state persists, only the new chunk executes. */
    pub fn adopt_entry_chunk(&mut self, chunk: &'a SSAChunk) {
        let start = self.functions.len();
        self.build_function_table(chunk, None, None);
        self.index_functions(start);
        self.index_templates(start);
        // A later input can rebind any name a cached result read.
        self.templates.clear();
        self.chunk = chunk;
    }

    /* What the allocator holds for the program, garbage since the last collection included. */
    pub fn memory(&self) -> usize { self.heap.bytes() }

    /* The most the allocator held for the program at once, which a bench compares run to run. */
    pub fn memory_peak(&self) -> usize { self.heap.peak() }

    /* Fresh op budget for the next REPL input. */
    pub fn reset_budget(&mut self, ops: usize) {
        self.budget = ops;
    }

    /* Mirror compile-time extern bindings by name, runs pre-exec so user rebinds win. */
    pub fn bind_chunk_externs(&mut self) -> Result<(), VmErr> {
        let chunk = self.chunk;
        for (name, &idx) in chunk.extern_index.iter() {
            if let Some(b) = chunk.extern_table.get(idx as usize) {
                let v = self.heap.alloc(HeapObj::Extern(b.clone()))?;
                self.scopes[0].set(name, v);
            }
        }
        Ok(())
    }

    /* Discard transient execution state so a parked REPL interpreter can run its next input. The heap, globals, and module bindings persist. */
    pub fn clear_error_state(&mut self) {
        self.stack.clear();
        self.regs.clear();
        self.base = 0;
        self.iter_stack.clear();
        self.exception_stack.clear();
        self.with_stack.clear();
        self.unwind_stack.clear();
        self.temp_roots.clear();
        self.call_stack.clear();
        self.bindings.clear();
        self.scheduler.clear();
        self.pending_sync_frames.clear();
        self.executing_coros.clear();
        self.handling.clear();
        self.cancelling = false;
        self.resume_raise = None;
        self.yielded = false;
        self.resume_ip = 0;
        self.depth = 0;
        self.pending = Pending::new();
    }
}
