// The value model by the name the VM modules use.
pub(crate) use crate::value as types;

mod cache;
mod block;
mod level;
mod window;
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
mod act;
mod gc;
mod helpers;
mod init;
pub(crate) mod keys;
pub(crate) mod jobs;
pub(crate) mod natives;
mod redo;
mod nest;
mod object_model;
mod groups;
pub(crate) mod codecs;
pub(crate) mod memview;
mod hints;
mod timsort;
#[cfg(feature = "native")]
pub(crate) mod native;

use crate::parser::{SSAChunk, builtin_type};
use crate::util::hash::FxHashMap as HashMap;

pub use types::{Val, HeapObj, HeapPool, VmErr, Limits};
pub use methods::{ALL_METHODS, BuiltinMethodId, MethodDesc, MethodKind, lookup_method};
pub use globals::exc_parents;
pub use scope::{Binding, BodyKind, FnScope, Kind};

use types::*;
use cache::CachePool;
use alloc::{boxed::Box, string::{String, ToString}, vec::Vec};

pub(crate) use types::ExceptionFrame;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ParamKind { Normal, Star, DoubleStar, KwOnly }

/* Side-channel state passed between opcodes in one dispatch frame, grouped for auditability. */
pub(crate) struct Pending {
    /* Star/double-star spreads bump the argument count of the call whose first spread opened the frame. */
    pub pos_delta: i32,
    pub kw_delta: i32,
    // Saved enclosing spread deltas (BeginArgs).
    pub delta_save: alloc::vec::Vec<(i32, i32)>,
    /* The current call's ip, turned into a byte offset only for a traceback. */
    pub call_ip: Option<u32>,
    /* Set by `receive()` on empty queue, transitions the coro to `WaitingEvent`. */
    pub event_wait_request: bool,
    /* Set by `call_extern` on deferred native, transitions the coro to `WaitingHostCall`. */
    pub host_call_request: bool,
    /* Correlation id of the deferred call, read by `drive_root` into `WaitingHostCall(id)`. */
    pub host_call_id: u64,
    /* Lifted ExcInstance from `raise X(...)` so `except X as e` binds the real instance. */
    pub exc_val: Option<Val>,
    /* `(class, self)` for the next user-function call when it's invoked as a method, populated by method-dispatch paths and consumed by `run_body_with_frame`. */
    pub method_binding: Option<(Val, Val)>,
    /* Set at preempt, `drive_root` yields `Preempted`. */
    pub preempt_request: bool,
    /* Set by the raise that ends an `except*`, whose exception takes no context from the one handled. */
    pub no_context: bool,
}

impl Pending {
    const fn new() -> Self {
        Self {
            pos_delta: 0,
            kw_delta: 0,
            delta_save: alloc::vec::Vec::new(),
            call_ip: None,
            event_wait_request: false,
            host_call_request: false,
            host_call_id: 0,
            exc_val: None,
            method_binding: None,
            preempt_request: false,
            no_context: false,
        }
    }

    /* Forgets what the last suspension waited for. */
    pub fn clear_waits(&mut self) {
        self.event_wait_request = false;
        self.host_call_request = false;
    }
}

pub struct VM<'a> {
    pub(crate) stack: Vec<Val>,
    /* The registers, a window for each chunk that ran (`window`). */
    pub(crate) regs: Vec<Val>,
    /* Where the running frame starts in `regs`, the window of `pool_now`. */
    pub(crate) base: usize,
    pub(crate) pool_now: usize,
    pub(crate) heap: HeapPool,
    pub(crate) iter_stack: Vec<IterFrame>,
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
    pub(crate) budget: u64,
    /* Calls nested in Rust, through builtins, dunders and calls the loop cannot run in place, capped at `max_calls`. */
    pub(crate) depth: usize,
    pub(crate) max_calls: usize,
    /* The calls a plugin or the host makes into Python while a call of the VM waits on it, each of which runs a loop of its own. */
    pub(crate) hosted: u32,
    /* The instruction loops running, never more than one but for each call a plugin or the host makes into Python. */
    #[cfg(any(debug_assertions, feature = "coverage"))]
    pub(crate) loops: nest::Loops,
    /* How many calls run in place in instruction loops, and the pool of the innermost, its record before its window. */
    pub(crate) acts: usize,
    pub(crate) top_pool: usize,
    /* The instances the `__init__` calls running in place hand back, innermost last. */
    pub(crate) instances: Vec<Val>,
    /* The coroutine the next `exec` resumes, whose state its window may still hold. */
    pub(crate) resuming: Val,
    /* How many windows hold a generator's state. */
    pub(crate) owners: u32,
    /* Set by an instruction loop right before a call it can run in place, taken by that call. */
    pub(crate) inline_call: bool,
    /* Set by a call a loop let in, whose frame that loop runs next. */
    pub(crate) entered: bool,
    /* A generator frame a throw opened, which the loop enters as `entered` asks, raising where it carries on. */
    pub(crate) thrown: bool,
    /* The instance a constructor builds, which its `__init__` hands back if it runs as a frame of the loop. */
    pub(crate) pending_instance: Val,
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
    /* What calling each function needs to know of its body, found once. */
    pub(crate) call_facts: Vec<act::CallFacts>,
    // The bases of every frame that is not flat, innermost last.
    pub(crate) frame_bases: Vec<act::Bases>,
    /* Coroutines currently inside `resume_coroutine`, re-entry raises like Python's already-executing guard. Transient, never snapshotted. */
    pub(crate) executing_coros: Vec<u64>,
    /* Bumped by any class member change, voiding every site keyed on a class. */
    pub(crate) class_epoch: u32,
    /* Whether each function is made again and again, its `def` inside a function or a loop, 0 not found yet, 1 no, 2 yes. */
    pub(crate) fn_remade: Vec<core::cell::Cell<u8>>,
    /* Set once an instance may hide a method or property of its class with an attribute. */
    pub(crate) shadowed: bool,
    /* What the last load or store of an attribute ran, 1 a property's function, 2 a class's `__getattr__`, 0 neither. */
    pub(crate) ran: u8,
    /* Where a load running a property's getter lies and where its value goes (a register, the stack, or nowhere for a setter), for its function to open as a frame of the loop. */
    pub(crate) prop_at: Option<(usize, u16)>,
    /* Set once some class lists `__slots__`, so stores check the names a class allows. */
    pub(crate) slotted: bool,
    /* Set once some class defines `__hash__`, so keys ask it before a table probes. */
    pub(crate) user_hash: bool,
    /* Set once some class defines `__new__`, so a class call may hand the calls its construction makes to the loop. */
    pub(crate) user_new: bool,
    /* Set once some class defines `__setattr__` or `__delattr__`, so stores and deletes look for them. */
    pub(crate) attr_hooks: bool,
    /* The receiver a builtin dunder of its own type runs on, whose class dunders that run skips. */
    pub(crate) plain_recv: Val,
    /* Per table hash, one instance of each class of equal keys a user `__hash__` made, the last class number given. */
    pub(crate) key_reps: HashMap<u64, Vec<Val>>,
    pub(crate) key_classes: u32,
    /* The keys a job classed for the op running again now, each with the depth in frames of that op, which takes them classed. */
    pub(crate) keyed_now: Vec<(Val, usize)>,
    /* The op whose handler hands its calls to the loop, and what they gave. */
    pub(crate) redo: redo::Redo,
    /* The shapes instance dicts take, each the shape every next key leads to, made on the first attribute stored. */
    pub(crate) shapes: Vec<Vec<(Val, u32)>>,
    /* Set by a hot loop in unlowered code, its frame then carries on lowered. */
    pub(crate) tier_up: bool,
    // Set while a laid code is laid again for the other functions its calls met.
    pub(crate) relaying: bool,
    /* A head whose check turned down a function asks its code to grow, which the loop's handler does, holding the code, the head and what it asks for. */
    pub(crate) growing: Option<Box<(*const lower::Code, usize, lower::Grow, Val)>>,
    /* Whether the last ask for a growth was turned down for good, the pair asked for before or never laid. */
    pub(crate) refused: bool,
    /* A call the register loop made that failed, raised at that call. */
    pub(crate) reg_error: Option<VmErr>,
    /* The levels of recursions through sections kept now, which count toward the recursion limit as frames do. */
    pub(crate) turned: usize,
    /* Where the stack may stand, from `turn_low` on, `turn_span` long, while `turn_acts` frames run, without a recursion level checking its depth and the memory limit again. */
    pub(crate) turn_low: usize,
    pub(crate) turn_span: usize,
    pub(crate) turn_acts: usize,
    /* What calling each class needs, valid while the class epoch holds. */
    pub(crate) ctors: crate::util::hash::FxHashMap<u64, Ctor>,
    /* One bit per builtin whose name was rebound, its fused calls then check module bindings first. */
    pub(crate) builtins_rebound: u128,
    pub(crate) is_async: Vec<bool>,
    /* The functions, and by path the modules, whose code was laid with sections. */
    pub(crate) laid_fns: Vec<bool>,
    pub(crate) laid_mods: Vec<String>,
    /* The methods code laid as sections, each as the code's key, the call's compiler instruction and the method. */
    pub(crate) learned: Vec<Val>,
    /* How each sectioned code was made, so a restored run finds it made the same way, per code key its lowering then each growth, four values each (entries read, head and kind, function met). */
    pub(crate) grown: Vec<Val>,
    /* Set while a code is made again from `grown`, which notes nothing then. */
    pub(crate) replaying: bool,
    /* While a binding's growth is made again, the binding and the function it held then. */
    pub(crate) replay_global: Option<(usize, u16, Val)>,
    /* The code that ran unlowered, whose sites a lowering of its chunk reads. */
    pub(crate) hint: Option<alloc::rc::Rc<lower::Code>>,
    pub(crate) default_slots: Vec<Vec<(usize, Val)>>,
    /* Each chunk's code and caches, indexed through `pool_ids` or `fn_pool`. */
    pub(crate) pools: Vec<CachePool>,
    /* Code a later lowering replaced, kept for the calls that still run it. */
    pub(crate) retired: Vec<alloc::rc::Rc<lower::Code>>,
    pub(crate) pool_ids: HashMap<*const SSAChunk, usize>,
    pub(crate) fn_pool: Vec<usize>,
    pub(crate) with_stack: Vec<Val>,
    /* Marks of generator sections given back, kept to hold the next section's stacks without allocating, the box and its buffers alike. */
    #[allow(clippy::vec_box)]
    pub(crate) held_pool: Vec<Box<crate::value::Held>>,
    /* GC roots for operands popped off the stack but still read after a dunder call that can collect. */
    pub(crate) temp_roots: Vec<Val>,
    /* The code the next frame of a native program's slow step runs, its one instruction then a return. */
    #[cfg(feature = "native")]
    pub(crate) native_code: Option<alloc::rc::Rc<lower::Code>>,
    /* Weak flags for lists produced by iterator builtins (iter/map/filter/zip/enumerate/reversed), next() drains only these and plain lists raise TypeError. Weak so a swept slot can never alias a fresh list. */
    pub(crate) pending: Pending,
    /* Monotonic correlation id handed to each deferred host call, matched by `set_host_result_by_id`. */
    pub(crate) next_host_call_id: u64,
    /* The file of the code making the native call in progress, which package the host answers. */
    pub(crate) extern_caller: Option<alloc::sync::Arc<String>>,
    /* The chunks eval, exec and compile built, kept while the VM lives since their functions point into them. */
    pub(crate) compiled: globals::compile::CompiledSet,
    /* Where code that eval, exec and compile build resolves its imports. */
    pub resolver_hook: Option<crate::modules::ResolverHook>,
    /* The dict the last `globals()` call made for each module, which exec writes through to that module. */
    pub(crate) globals_dicts: Vec<(usize, Val)>,
    /* Scratch scopes whose functions exec defined in a dict, which their calls keep in sync with it. */
    pub(crate) linked: Vec<globals::compile::Linked>,
    /* The `builtins` module exec puts under `__builtins__`, made on first need. */
    pub(crate) builtins_module: Val,
    /* Sync helpers that suspended during the current resume, drained into the active Coroutine on yield-save. Lives at VM scope (not `Pending`) because it propagates across dispatch frames, not within one. */
    pub(crate) pending_sync_frames: Vec<types::ParkedFrame>,
    /* Overrides `exec`'s captured `exc_base`. Set by `resume_coroutine` to the level *before* restored exception frames so dispatch's handler search includes them, consumed once at exec entry. */
    pub(crate) pending_exec_exc_base: Option<usize>,
    /* Back-edges until the next preempt, 0 disables. */
    pub(crate) preempt_left: usize,
    pub(crate) preempt_every: usize,
    /* True while this `exec` frame can unwind. */
    pub(crate) frame_safe: bool,
    /* Generators a `for` runs as frames of a loop right now, which no preemption may park. */
    pub(crate) gen_frames: usize,
    /* The helpers the module body waits inside, opened as frames of the loop resuming it. */
    pub(crate) chain: globals::async_ops::Chain,
    /* A run of text exec or eval hands the loop that called it, and the floors of loops such runs set aside. */
    pub(crate) floor_req: bool,
    pub(crate) floors: Vec<globals::compile::Floored>,
    /* The operator dunder a frame of the loop just ran that returned NotImplemented, so the op's handler goes on past it, undef for none. */
    pub(crate) declined: Val,
    /* The left operand's dunder that declined before the reflected one in `declined` did, both of them skipped by the handler. */
    pub(crate) declined_also: Val,
    /* The exception pending as a `for`'s laid `__next__` of a user iterator last began, which its StopIteration leaves pending. */
    pub(crate) next_before: Option<Val>,
    /* Where a generator frame a stack `for` just opened resumes, taken when the loop switches to it. */
    pub(crate) gen_entry: usize,
    /* The builtins whose callbacks run as frames of a loop right now, innermost last, which no preemption may park. */
    pub(crate) jobs: Vec<jobs::Job>,
    /* Set right before a builtin call made where a frame may open in place, taken by a builtin that runs its callback so. */
    pub(crate) job_ok: bool,
    /* The string an `AppendR` made last and the register holding it, the only one that string grows in place in. */
    pub(crate) owned_str: Val,
    pub(crate) owned_at: usize,
    pub(crate) pending_exec_safe: bool,
    /* The one-shot raise at the park point, live within one resume, never snapshotted. */
    pub(crate) resume_raise: Option<VmErr>,
    // The generator a call resumes as a frame of the loop, with its job's takes and the exception pending as the call began.
    pub(crate) coro_frame: Option<(Val, crate::vm::jobs::Takes, Val)>,
    // How an awaited frame came back to its op, 1 yielded, 2 returned, 4 and 5 an async generator's yield and end, 6 an `async for`'s end.
    pub(crate) await_back: u8,
    // The waits of the bodies that delegate to a `Then` job's frame, innermost first, and whether a delegation may open one.
    pub(crate) conts: Vec<crate::vm::globals::async_ops::Then>,
    pub(crate) then_ok: bool,
    // Set while a driven step may leave a generator's next item to its driver.
    pub(crate) step_soft: bool,
    // What a `Then` job's frame gave, kept for `job_out`.
    pub(crate) then_out: Option<(u32, Result<crate::vm::globals::async_ops::Step, VmErr>)>,
    pub(crate) yielded: bool,
    /* Set by a `for` stepping a generator, which may then park on a wait with it. */
    pub(crate) for_steps: bool,
    /* Return value of the most recently exhausted iterator, read by `LoadYieldFrom` so `x = yield from it` evaluates to the subiterator's StopIteration value. */
    pub(crate) yield_from_value: Val,
    // The builtin iterator a `for` steps that holds no generator, which `lazy_over` then passes by.
    pub(crate) lazy_seen: Val,
    // The items jobs made for the next steps of the iterators they pair with, undef for a step that ends one, which those steps take first.
    pub(crate) pend: Vec<(Val, Val)>,
    // The items a job gathered from a generator for a native binding stepping it in Rust, past the one it keeps pending, last first.
    pub(crate) held: Vec<(Val, Vec<Val>)>,
    // The first of the functions of the loop a builtin's callback that steps a generator runs through, once made.
    pub(crate) calls_through: Option<usize>,
    pub(crate) resume_ip: usize,
    // The layout of the code a run suspended inside a section parked in, which `resume_ip` indexes, 0 for none.
    pub(crate) resume_layout: u32,
    // For a function a section makes, the register each slot of the section's function lies in.
    pub(crate) cell_map: Option<alloc::rc::Rc<[u16]>>,
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
    /* Where the error rose inside a text `exec` or `eval` ran, its source, path and byte, so the traceback starts there. */
    pub(crate) error_inner: Option<(alloc::sync::Arc<String>, alloc::sync::Arc<String>, u32)>,
    /* The name or attribute the last error missed, for the hint its traceback adds if it goes uncaught. */
    pub(crate) missed: Option<hints::Missed>,
    /* The names and slots of the locals of each body a name was missed in, measured once for every miss after. */
    pub(crate) local_names: HashMap<(usize, usize), hints::Locals>,
    /* spec -> Module Val, populated by `init_modules`, read by LoadModule / import_module(). */
    pub(crate) module_table: HashMap<String, Val>,
    /* `fi -> module spec`, scopes the free-load fallback to the fn's own module. */
    pub(crate) fn_module: Vec<Option<String>>,
    /* Function names parallel to `functions`, consumed by traceback render. Empty = lambda. */
    pub(crate) function_names: Vec<String>,
    /* Each function by its qualified name, `A.f`, which call errors and `__qualname__` give. */
    pub(crate) function_qualnames: Vec<String>,
    /* The qualified name of a class nested in another class or a function, by the class Val. */
    pub(crate) class_qualnames: HashMap<u64, String>,
    /* The frames an error unwound through, innermost first, built only as it unwinds and read by the traceback renderer. */
    pub(crate) call_stack: Vec<CallFrame>,
    /* `(depth, class, self)` of each running method, which zero-argument `super()` reads. */
    pub(crate) bindings: Vec<(usize, Val, Val)>,
    /* The module body while a `run()` is suspended to the host, None between runs. */
    pub(crate) root: Option<CoroutineHandle>,
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
    // A class with its own `__new__` or `__slots__`, which only the general path builds.
    pub special: bool,
}

impl<'a> VM<'a> {
    pub fn new(chunk: &'a SSAChunk) -> Self { Self::with_limits(chunk, Limits::sandbox()) }

    /* The generic frames this VM pushed, which `tests/nested.rs` holds at none. Debug and coverage builds only. */
    #[cfg(any(debug_assertions, feature = "coverage"))]
    #[doc(hidden)]
    pub fn old_path_counts(&self) -> [u64; 1] { [self.loops.generic] }

    /* Whether `chunk.instructions[ip]` ran in this VM, still answered after a run that raised. */
    #[cfg(feature = "coverage")]
    pub fn ran(&self, chunk: &SSAChunk, ip: usize) -> bool {
        self.executed.get(&(chunk as *const _)).and_then(|bits| bits.get(ip / 64)).is_some_and(|word| word & (1 << (ip % 64)) != 0)
    }

    pub fn with_limits(chunk: &'a SSAChunk, limits: Limits) -> Self {
        let mut vm = Self::fields(chunk, limits);
        vm.boot(chunk);
        vm
    }

    /* `with_limits` built in its box, the wasm boot then never copies the VM's bytes (fuel pays memory.copy per byte). */
    pub fn boxed(chunk: &'a SSAChunk, limits: Limits) -> Box<Self> {
        let mut vm = Box::write(Box::new_uninit(), Self::fields(chunk, limits));
        vm.boot(chunk);
        vm
    }

    /* The VM's fields before `boot`, inlined so `boxed` writes them straight into its box. */
    #[inline(always)]
    fn fields(chunk: &'a SSAChunk, limits: Limits) -> Self {
        Self {
            stack: Vec::with_capacity(256),
            regs: Vec::with_capacity(256),
            base: 0,
            pool_now: 0,
            iter_stack: Vec::with_capacity(16),
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
            budget: limits.ops,
            depth: 0,
            #[cfg(any(debug_assertions, feature = "coverage"))]
            loops: Default::default(),
            acts: 0,
            top_pool: 0,
            instances: Vec::new(),
            resuming: Val::undef(),
            owners: 0,
            inline_call: false,
            entered: false,
            thrown: false,
            pending_instance: Val::undef(),
            max_calls: MAX_NESTED,
            hosted: 0,
            with_stack: Vec::new(),
            held_pool: Vec::new(),
            temp_roots: Vec::new(),
            #[cfg(feature = "native")]
            native_code: None,
            pending: Pending::new(),
            next_host_call_id: 0,
            extern_caller: None,
            compiled: Default::default(),
            resolver_hook: None,
            globals_dicts: Vec::new(),
            linked: Vec::new(),
            builtins_module: Val::undef(),
            pending_sync_frames: Vec::new(),
            pending_exec_exc_base: None,
            preempt_left: 0,
            preempt_every: 0,
            frame_safe: false,
            gen_frames: 0,
            chain: Default::default(),
            floor_req: false,
            floors: Vec::new(),
            declined: Val::undef(),
            declined_also: Val::undef(),
            next_before: None,
            gen_entry: 0,
            jobs: Vec::new(),
            job_ok: false,
            owned_str: Val::undef(),
            owned_at: usize::MAX,
            pending_exec_safe: false,
            resume_raise: None,
            coro_frame: None,
            await_back: 0,
            conts: Vec::new(),
            then_ok: false,
            step_soft: false,
            then_out: None,
            yielded: false,
            for_steps: false,
            yield_from_value: Val::none(),
            lazy_seen: Val::undef(),
            pend: Vec::new(),
            held: Vec::new(),
            calls_through: None,
            resume_ip: 0,
            resume_layout: 0,
            cell_map: None,
            strict_input: true,
            output: Vec::new(),
            output_open: false,
            print_hook: None,
            send_hook: None,
            input_buffer: Vec::new(),
            event_queue: Vec::new(),
            mro_cache: HashMap::default(),
            exception_stack: Vec::new(),
            unwind_stack: Vec::new(),
            handling: Vec::new(),
            error_byte_pos: None,
            error_inner: None,
            missed: None,
            local_names: HashMap::default(),
            module_table: HashMap::default(),
            fn_module: Vec::new(),
            function_names: Vec::new(),
            function_qualnames: Vec::new(),
            class_qualnames: HashMap::default(),
            call_stack: Vec::new(),
            bindings: Vec::new(),
            root: None,
            #[cfg(feature = "coverage")]
            executed: HashMap::default(),
            functions: Vec::new(),
            fn_index: Vec::new(),
            function_parents: Vec::new(),
            body_to_fi: HashMap::default(),
            param_slots: Vec::new(),
            call_facts: Vec::new(),
            frame_bases: Vec::new(),
            executing_coros: Vec::new(),
            class_epoch: 0,
            fn_remade: Vec::new(),
            shadowed: false,
            ran: 0,
            prop_at: None,
            slotted: false,
            user_hash: false,
            user_new: false,
            attr_hooks: false,
            plain_recv: Val::undef(),
            key_reps: HashMap::default(),
            key_classes: 0,
            keyed_now: Vec::new(),
            redo: redo::Redo::new(),
            shapes: Vec::new(),
            tier_up: false,
            relaying: false,
            growing: None,
            refused: false,
            reg_error: None,
            turned: 0,
            turn_low: 0,
            turn_span: 0,
            turn_acts: 0,
            ctors: Default::default(),
            builtins_rebound: 0,
            is_async: Vec::new(),
            laid_fns: Vec::new(),
            laid_mods: Vec::new(),
            learned: Vec::new(),
            grown: Vec::new(),
            replaying: false,
            replay_global: None,
            hint: None,
            default_slots: Vec::new(),
            // Room for the pools of most programs, which so never grow it.
            pools: Vec::with_capacity(16),
            retired: Vec::new(),
            pool_ids: HashMap::default(),
            fn_pool: Vec::new(),
        }
    }

    /* What a fresh VM sets up on its chunk, wide ints, budget, function tables, `__name__` and `NotImplemented`. */
    fn boot(&mut self, chunk: &'a SSAChunk) {
        crate::frontend::install(&mut self.heap);
        self.heap.set_budget(self.budget);
        self.build_function_table(chunk, None, None);
        self.index_functions(0);
        if let Ok(main_name) = self.heap.alloc(HeapObj::Str("__main__".to_string())) {
            self.builtins.insert("__name__", main_name);
        }
        // `NotImplemented` singleton, dunders return it to delegate to the reflected operator.
        if let Ok(ni) = self.heap.alloc(HeapObj::NotImplemented) {
            self.builtins.insert("NotImplemented", ni);
        }
    }

    /* Derived per-function tables for functions[start..], the REPL re-invokes this to extend them for each adopted chunk. */
    pub(crate) fn index_functions(&mut self, start: usize) {
        let end = self.functions.len();
        // A chunk that defines no function has nothing to add.
        if start == end { return; }
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
        self.call_facts.truncate(start);
        self.call_facts.reserve(end - start);
        self.fn_remade.resize(end, core::cell::Cell::new(0));
        for fi in start..end {
            let (body, params) = (&self.functions[fi].1, &self.param_slots[fi]);
            let arity = params.iter().all(|&(k, s)| matches!(k, ParamKind::Normal) && s != usize::MAX).then_some(params.len());
            // A body whose positionals fill its first slots and that makes no cell opens as a frame of the loop calling it.
            let in_loop = arity.is_some() && params.iter().enumerate().all(|(k, &(_, s))| s == k) && self.fn_scope[fi].cellvars.is_empty() && !body.is_generator;
            let required = self.functions[fi].0.iter().filter(|p| !p.ends_with('=')).count();
            let closure = !self.fn_scope[fi].freevars.is_empty();
            // A body naming `super` keeps the binding its method call makes, which only the full path does.
            let plain = in_loop && !closure && arity == Some(required) && !body.names.iter().any(|n| n.starts_with("super") && crate::parser::ssa_strip(n) == "super");
            let (star, full) = if arity.is_some() { (act::NO_SLOTS, act::NO_SLOTS) } else {
                // `star_slot` wants undefaulted positionals in the first slots then `*args`, and `full_slots` plain, keyword-only and `*args` in order, neither making a cell or generator.
                let firsts = params.iter().enumerate().all(|(k, &(_, s))| s == k) && self.fn_scope[fi].cellvars.is_empty() && !body.is_generator;
                let p = params.iter().take_while(|(k, _)| matches!(k, ParamKind::Normal)).count();
                let star = firsts && params.len() == p + 1 && matches!(params[p].0, ParamKind::Star) && !self.functions[fi].0.iter().any(|q| q.ends_with('=')) && p < act::NO_SLOTS as usize;
                let full = firsts && params.iter().all(|&(k, _)| matches!(k, ParamKind::Normal | ParamKind::Star | ParamKind::KwOnly)) && params.len() < act::NO_SLOTS as usize;
                (if star { p as u8 } else { act::NO_SLOTS }, if full { params.len() as u8 } else { act::NO_SLOTS })
            };
            let opens = in_loop || !self.fn_scope[fi].cellvars.is_empty() && !body.is_generator;
            let required = required.min(u16::MAX as usize) as u16;
            self.call_facts.push(act::CallFacts { arity, required, closure, in_loop, pool: self.fn_pool[fi], names: body.names.len() as u32, plain, star, full, bits: if opens { act::OPENS } else { 0 } });
        }
    }

    /* A builtin name the program binds or deletes makes its fused calls run the binding. */
    pub(crate) fn note_builtin_binding(&mut self, bare: &str) {
        if let Some(id) = NativeFnId::from_name(bare) { self.rebind_builtin(id); }
    }

    /* The name of `id` holds a program value, its fused calls check bindings. */
    pub(crate) fn rebind_builtin(&mut self, id: NativeFnId) {
        self.builtins_rebound |= 1 << id as u32;
        // A rebound builtin calls its function, which the ops' arming takes, as it takes a dunder.
        self.heap.users = true;
    }

    /* Whether the builtin fused into `op` was rebound, so the call must run the binding. */
    #[inline]
    pub(crate) fn fused_rebound(&self, op: crate::parser::OpCode) -> bool {
        self.builtins_rebound != 0 && crate::parser::fused_native(op).is_some_and(|id| self.builtins_rebound & (1 << id as u32) != 0)
    }

    /* Gives `bare` a heap slot when it names a builtin, so a program pays only for the builtins it uses. */
    pub(crate) fn register_builtin(&mut self, bare: &str) {
        if self.builtins.contains_key(bare) { return; }
        // Type names stay Type objects even when a NativeFn shares them.
        let (name, obj) = if let Some(name) = builtin_type(bare) {
            // `IOError` and `EnvironmentError` are the `OSError` class under other names.
            if matches!(name, "IOError" | "EnvironmentError") {
                self.register_builtin("OSError");
                if let Some(&v) = self.builtins.get("OSError") { self.builtins.insert(name, v); }
                return;
            }
            (name, HeapObj::Type(name.to_string()))
        } else if let Some(id) = NativeFnId::from_name(bare) {
            (id.name(), HeapObj::NativeFn(id))
        } else if bare == "Ellipsis" {
            ("Ellipsis", HeapObj::Ellipsis)
        } else if bare == "__doc__" {
            // The docstring of the entry chunk is its `__doc__`, None without one.
            match crate::vm::opcodes::attr_lookup::docstring(self.chunk) {
                Some(d) => ("__doc__", HeapObj::Str(d)),
                None => { self.builtins.insert("__doc__", Val::none()); return; }
            }
        } else {
            return;
        };
        if let Ok(v) = self.heap.alloc(obj) { self.builtins.insert(name, v); }
    }

    /* The value of builtin `name`, made on first use. */
    pub(crate) fn builtin_value(&mut self, name: &'static str) -> Result<Val, VmErr> {
        self.register_builtin(name);
        match self.builtins.get(name) { Some(&v) => Ok(v), None => Err(VmErr::Heap) }
    }

    /* For the REPL, adopt `chunk` as the new entry module, state persists, only the new chunk executes. */
    pub fn adopt_entry_chunk(&mut self, chunk: &'a SSAChunk) {
        self.adopt_functions(chunk, None);
        self.chunk = chunk;
    }

    /* Adds the functions of `chunk` to the table, its names bound into module `spec`. */
    pub(crate) fn adopt_functions(&mut self, chunk: &'a SSAChunk, spec: Option<&str>) {
        let start = self.functions.len();
        self.build_function_table(chunk, None, spec);
        self.index_functions(start);
    }

    /* What the allocator holds for the program, garbage since the last collection included. */
    /* The index and scope of the function whose body is `body`, its names as this VM resolved them. */
    pub fn scope_of(&self, body: &SSAChunk) -> Option<(usize, &FnScope)> {
        let fi = *self.body_to_fi.get(&(body as *const SSAChunk))?;
        Some((fi, &self.fn_scope[fi]))
    }

    /* The parameters of function `fi`, each with its kind and the slot it binds. */
    pub fn params_of(&self, fi: usize) -> &[(ParamKind, usize)] { &self.param_slots[fi] }

    pub fn memory(&self) -> usize { self.heap.bytes() }

    /* The most the allocator held for the program at once, which a bench compares run to run. */
    pub fn memory_peak(&self) -> usize { self.heap.peak() }

    /* Fresh op budget for the next REPL input. */
    pub fn reset_budget(&mut self, ops: u64) {
        self.budget = ops;
        self.heap.set_budget(ops);
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
        (self.acts, self.top_pool) = (0, 0);
        self.instances.clear();
        self.jobs.clear();
        for p in &mut self.pools { (p.window, p.wlen, p.active, p.wcode) = (window::NO_WINDOW, 0, 0, 0); p.wins.clear(); p.w0 = window::Win { base: window::NO_WINDOW, len: 0, code: 0, owner: Val::undef() }; }
        self.owners = 0;
        (self.base, self.pool_now) = (0, 0);
        self.iter_stack.clear();
        self.exception_stack.clear();
        self.with_stack.clear();
        self.unwind_stack.clear();
        self.temp_roots.clear();
        self.call_stack.clear();
        self.bindings.clear();
        self.root = None;
        self.pending_sync_frames.clear();
        for id in core::mem::take(&mut self.executing_coros) {
            if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(Val(id)) { c.running = false; }
        }
        self.handling.clear();
        self.resume_raise = None;
        self.yielded = false;
        self.resume_ip = 0;
        self.depth = 0;
        self.pending = Pending::new();
    }
}
