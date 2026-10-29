mod cache;
mod block;
mod level;
mod window;
pub(crate) mod lower;
mod depth;
mod rows;
mod registers;
mod resolve;
pub(crate) mod scope;
mod sites;
pub mod snapshot;

mod frame;
mod run;
mod exc;
pub(crate) mod coro;
pub(crate) mod calls;
pub(crate) mod sections;
pub(crate) mod slow;
mod limits;
mod act;
mod gc;
mod embed;
mod init;
pub(crate) mod jobs;
mod steps;
mod redo;
mod nest;
mod hints;
mod vet;

use crate::syntax::chunk::SSAChunk;
use crate::lang::same_text;
use crate::util::hash::FxHashMap as HashMap;

pub use crate::value::{Val, HeapObj, HeapPool, VmErr, Limits};
pub use scope::{Binding, BodyKind, FnScope, Kind};

use crate::value::*;
use cache::CachePool;
use alloc::{boxed::Box, string::{String, ToString}, vec::Vec};

pub(crate) use crate::value::ExceptionFrame;
pub(crate) use frame::{driven, engine_drive};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ParamKind { Normal, Star, KwRest, KwOnly }

/* side-channel state passed between opcodes in one dispatch frame and grouped for auditability */
pub(crate) struct Pending {
    // the next spread call's packed operands as a mask with its maps in order
    pub spread: (u8, u32),
    // saved enclosing spread deltas from `BeginArgs`
    pub delta_save: alloc::vec::Vec<(i32, i32)>,
    /* the current call's ip turned into a byte offset only for a traceback */
    pub call_ip: Option<u32>,
    /* set by a native waiting on an empty event queue to park the body */
    pub event_wait_request: bool,
    /* set by `call_extern` on a deferred native so the coro moves to `WaitingHostCall` */
    pub host_call_request: bool,
    /* the deferred call's correlation id which `drive_root` reads into `WaitingHostCall(id)` */
    pub host_call_id: u64,
    /* the ExcInstance lifted from `raise X(args)` so `except X as e` binds the real one */
    pub exc_val: Option<Val>,
    /* set at preempt so `drive_root` yields `Preempted` */
    pub preempt_request: bool,
}

impl Pending {
    const fn new() -> Self {
        Self {
            spread: (0, 0),
            delta_save: alloc::vec::Vec::new(),
            call_ip: None,
            event_wait_request: false,
            host_call_request: false,
            host_call_id: 0,
            exc_val: None,
            preempt_request: false,
        }
    }

    /* forgets what the last suspension waited for */
    pub fn clear_waits(&mut self) {
        self.event_wait_request = false;
        self.host_call_request = false;
    }
}

pub struct VM<'a> {
    /* the runtime the program runs under and its natives read on hot paths */
    pub(crate) rt: &'static crate::lang::Runtime,
    pub(crate) natives: &'static crate::lang::Natives,
    // whether a nested run enters the runtime's own loop rather than the engine's
    pub(crate) own_loop: bool,
    /* the fold a native's walk hands the next gather it makes */
    pub(crate) next_fold: core::cell::Cell<Option<Box<steps::Folding>>>,
    /* the registers and links recursion levels through sections keep with the innermost last */
    pub(crate) levels: Vec<Val>,
    /* the registers holding a window for each chunk that ran which `window` lays out */
    pub(crate) regs: Vec<Val>,
    /* where the running frame starts in `regs` at the window of `pool_now` */
    pub(crate) base: usize,
    pub(crate) pool_now: usize,
    pub(crate) heap: HeapPool,
    pub(crate) iter_stack: Vec<IterFrame>,
    pub(crate) chunk: &'a SSAChunk,
    /* each module's bindings with the entry module first by the index each name keeps */
    pub(crate) scopes: Vec<scope::Globals>,
    /* each module spec to its index in `scopes` */
    pub(crate) scope_ids: HashMap<String, usize>,
    /* every chunk the machine registered by the place its id names */
    pub(crate) bodies: Vec<Body>,
    /* how each function body's names resolve indexed by `fi` */
    pub(crate) fn_scope: Vec<scope::FnScope>,
    /* the chunk each function is defined in whose frame its closure cells come from */
    pub(crate) fn_definer: Vec<&'a SSAChunk>,
    /* the cells around each running class body which its methods close over */
    pub(crate) class_cells: Vec<Vec<(String, Val)>>,
    /* the builtins under the globals keyed by their static names and never rebound */
    pub(crate) builtins: HashMap<&'static str, Val>,
    pub(crate) budget: u64,
    /* nesting in Rust from natives and op methods the loop cannot run in place */
    pub(crate) depth: usize,
    pub(crate) max_calls: usize,
    /* nested host or plugin calls into user code each running its own loop */
    pub(crate) hosted: u32,
    /* the instruction loops running which only host or plugin calls into user code add */
    #[cfg(any(debug_assertions, feature = "coverage"))]
    pub(crate) loops: nest::Loops,
    // lowerings refused for an op over rows and those ops the finish line counts
    #[cfg(debug_assertions)]
    pub(crate) stack_lowered: u32,
    #[cfg(debug_assertions)]
    pub(crate) stack_ops: [u32; 256],
    /* how many calls run in place and the innermost's pool whose record precedes its window */
    pub(crate) acts: usize,
    pub(crate) top_pool: usize,
    /* the instances init calls running in place hand back innermost last */
    pub(crate) instances: Vec<Val>,
    /* the coroutine the next `exec` resumes whose state its window may still hold */
    pub(crate) resuming: Val,
    /* how many windows hold a generator's state */
    pub(crate) owners: u32,
    /* set just before a call the loop can run in place which that call takes */
    pub(crate) inline_call: bool,
    /* set by a call a loop let in whose frame that loop runs next */
    pub(crate) entered: bool,
    /* a throw opened a generator frame the loop enters like `entered` raising where it resumes */
    pub(crate) thrown: bool,
    /* the instance a constructor builds which its init hands back as a loop frame */
    pub(crate) pending_instance: Val,
    /* the depth first order of each class by its heap bits made at creation */
    pub(crate) mro_cache: HashMap<u64, alloc::rc::Rc<Vec<Val>>>,
    pub(crate) exception_stack: Vec<ExceptionFrame>,
    /* active `finally` and `with` cleanup reasons innermost last and `EndFinally` pops one per body */
    pub(crate) unwind_stack: Vec<crate::value::Unwind>,
    /* exceptions being handled and where each rose innermost last for a bare `raise` to re-raise */
    pub(crate) handling: Vec<(Val, Option<u32>)>,
    pub(crate) functions: Vec<&'a (Vec<String>, SSAChunk, u16, u16)>,
    // the lexical enclosing fi of each function and None at module level
    pub(crate) function_parents: Vec<Option<usize>>,
    pub(crate) param_slots: Vec<Vec<(ParamKind, usize)>>,
    /* what calling each function needs to know of its body found once */
    pub(crate) call_facts: Vec<act::CallFacts>,
    // the bases of every frame that is not flat innermost last
    pub(crate) frame_bases: Vec<act::Bases>,
    /* coroutines inside `resume_coroutine` whose reentry raises and which snapshots never keep */
    pub(crate) executing_coros: Vec<u64>,
    /* a change to any layout a meta gives bumps it voiding every guarded site */
    pub(crate) layout_epoch: u32,
    /* the last class whose exception base was asked with the epoch and that base */
    pub(crate) exc_memo: core::cell::Cell<(u64, u32, Val)>,
    /* whether nested or looped `def`s remake each function as 0 unknown 1 no 2 yes */
    pub(crate) fn_remade: Vec<core::cell::Cell<u8>>,
    /* set once an instance may hide a method of its class with an attribute */
    pub(crate) shadowed: bool,
    /* the op whose handler hands its calls to the loop and what they gave */
    pub(crate) redo: redo::Redo,
    /* instance dict shapes made at the first attribute store each mapping keys to next shapes */
    pub(crate) shapes: Vec<Vec<(Val, u32)>>,
    /* set by a hot loop in unlowered code whose frame then carries on lowered */
    pub(crate) tier_up: bool,
    // set while a laid code is laid again for the other functions its calls met
    pub(crate) relaying: bool,
    // the compiler instruction a module's floor turns to lowered code at
    pub(crate) lower_from: usize,
    /* a head whose check refused a function asks the loop's handler to grow its code */
    pub(crate) growing: Option<Box<(crate::r#unsafe::CodePtr, usize, lower::Grow, Val)>>,
    /* whether the last growth ask failed for good on a repeated or never laid pair */
    pub(crate) refused: bool,
    /* a call the register loop made that failed and raises at that call */
    pub(crate) reg_error: Option<VmErr>,
    /* the levels recursions through sections keep now counting toward the recursion limit like frames */
    pub(crate) turned: usize,
    // the frames and levels that may run before a new one passes the recursion limit
    pub(crate) deepest: usize,
    /* where the levels may stand while `turn_acts` frames run with no depth or memory check */
    pub(crate) turn_low: usize,
    pub(crate) turn_span: usize,
    pub(crate) turn_acts: usize,
    /* what calling each class needs while the layout epoch holds */
    pub(crate) ctors: crate::util::hash::FxHashMap<u64, Ctor>,
    /* a bit per native whose name a module binds so its calls check that binding */
    pub(crate) rebound: Vec<u64>,
    // whether any bit of `rebound` is set and past it what `run::ops_bound` last found
    pub(crate) any_rebound: u8,
    // a call rule handed between a site and its slow path both ways
    pub(crate) rule_hand: Option<sites::Site>,
    pub(crate) is_async: Vec<bool>,
    /* the functions and by path the modules whose code was laid with sections */
    pub(crate) laid_fns: Vec<bool>,
    pub(crate) laid_mods: Vec<String>,
    /* methods laid as sections in triples of code key then call's compiler instruction then method */
    pub(crate) learned: Vec<Val>,
    /* each sectioned code's lowering then growths four values each so a restore remakes it alike */
    pub(crate) grown: Vec<Val>,
    /* set while a code is remade from `grown` so nothing is noted then */
    pub(crate) replaying: bool,
    /* the binding whose growth is being remade and the function it held then */
    pub(crate) replay_global: Option<(usize, u16, Val)>,
    /* the functions the lowering being remade laid and none outside a replay */
    pub(crate) replay_laid: Option<Vec<usize>>,
    /* the code that ran unlowered whose sites a lowering of its chunk reads */
    pub(crate) hint: Option<alloc::rc::Rc<lower::Code>>,
    pub(crate) default_slots: Vec<Vec<(usize, Val)>>,
    /* each chunk's code and caches indexed through `bodies` or `fn_pool` */
    pub(crate) pools: Vec<CachePool>,
    /* code a later lowering replaced kept for the calls that still run it */
    pub(crate) retired: Vec<alloc::rc::Rc<lower::Code>>,
    pub(crate) fn_pool: Vec<usize>,
    /* generator section marks given back so the next section's stacks reuse their box and buffers */
    #[allow(clippy::vec_box)]
    pub(crate) held_pool: Vec<Box<crate::value::Held>>,
    /* the GC roots for popped operands still read after an op method that can collect */
    pub(crate) temp_roots: Vec<Val>,
    /* weak marks on lists iterator builtins made which next drains while plain lists refuse */
    pub(crate) pending: Pending,
    /* monotonic correlation id handed to each deferred host call and matched by `set_host_result_by_id` */
    pub(crate) next_host_call_id: u64,
    /* the caller file of the current native call which the host resolves packages from */
    pub(crate) extern_caller: Option<alloc::sync::Arc<String>>,
    /* chunks compiled at run time kept while the vm lives since functions point into them */
    pub(crate) compiled: snapshot::CompiledSet,
    /* sync helpers suspended this resume which yield-save drains and which outlive one dispatch frame */
    pub(crate) pending_sync_frames: Vec<crate::value::ParkedFrame>,
    /* `resume_coroutine` overrides `exec`'s `exc_base` once to below restored exception frames so handler search sees them */
    pub(crate) pending_exec_exc_base: Option<usize>,
    /* back-edges until the next preempt where 0 disables it */
    pub(crate) preempt_left: usize,
    pub(crate) preempt_every: usize,
    // back-edges a due preempt waited for every frame to leave the sections it runs in
    pub(crate) preempt_waited: u32,
    /* true while this `exec` frame can unwind */
    pub(crate) frame_safe: bool,
    /* generators a `for` runs as loop frames now which no preemption may park */
    pub(crate) gen_frames: usize,
    /* the helpers the module body waits inside opened as frames of the loop resuming it */
    pub(crate) chain: coro::Chain,
    /* a class body handed to its loop and the floors such bodies set aside */
    pub(crate) floor_req: bool,
    pub(crate) floors: Vec<frame::Floored>,
    /* where a generator frame a stack `for` just opened resumes taken at the loop's switch */
    pub(crate) gen_entry: usize,
    /* builtins whose callbacks run as loop frames now innermost last which no preemption may park */
    pub(crate) jobs: Vec<jobs::Job>,
    /* lets the builtin called next run its callback as a frame in place once */
    pub(crate) job_ok: bool,
    // where the value of a job the running op opens goes
    pub(crate) job_to: act::Dest,
    // the value a register op takes running again past the frame it opened
    pub(crate) retake: Val,
    // the line a native's plain output writes kept so each write reuses it
    pub(crate) out_line: String,
    /* the last string an `AppendR` made and the one register where it grows in place */
    pub(crate) owned_str: Val,
    pub(crate) owned_at: usize,
    // the running frame's next free row or zero for lowered code holding none
    pub(crate) top: u32,
    // the top a floor resumes from and then the one it parked with
    pub(crate) park_top: u32,
    pub(crate) pending_exec_safe: bool,
    /* the one-shot raise at the park point live within one resume and never snapshotted */
    pub(crate) resume_raise: Option<VmErr>,
    // a generator resumed in place with its job's takes and the call's prior pending exception
    pub(crate) coro_frame: Option<(Val, crate::vm::jobs::Takes, Val)>,
    // how an awaited frame came back to its op as 1 yielded or 2 returned
    pub(crate) await_back: u8,
    // body waits delegating to a `Then` job's frame innermost first and whether one may open
    pub(crate) conts: Vec<crate::vm::coro::Then>,
    pub(crate) then_ok: bool,
    // set while a driven step may leave a generator's next item to its driver
    pub(crate) step_soft: bool,
    // what a `Then` job's frame gave kept for `job_out`
    pub(crate) then_out: Option<(u32, Result<crate::vm::coro::Step, VmErr>)>,
    pub(crate) yielded: bool,
    /* set by a `for` stepping a generator that may park with it on a wait */
    pub(crate) for_steps: bool,
    /* the last spent iterator's return value which `LoadYieldFrom` gives a delegation */
    pub(crate) yield_from_value: Val,
    // the builtin iterator a `for` steps holding no generator which `lazy_over` then passes by
    pub(crate) lazy_seen: Val,
    // items jobs made that paired iterators' next steps take first with undef ending the iterator
    pub(crate) pend: Vec<(Val, Val)>,
    // items gathered for a Rust native stepping a generator beyond the pending one last first
    pub(crate) held: Vec<(Val, Vec<Val>)>,
    // one past the first loop function generator callbacks run through so it takes one word
    pub(crate) calls_through: Option<core::num::NonZeroUsize>,
    pub(crate) resume_ip: usize,
    // the layout `resume_ip` indexes for a run suspended inside a section or 0 for none
    pub(crate) resume_layout: u32,
    // the register of each slot of a section's function for functions that section makes
    pub(crate) cell_map: Option<alloc::rc::Rc<[u16]>>,
    pub output: Vec<String>,
    /* true when the last `output` entry is a line left open as by `print(end="")` */
    pub(crate) output_open: bool,
    pub print_hook: Option<fn(&str)>,
    /* the host scheduler handoff for `send()` where unset or false raises the missing scheduler error */
    pub send_hook: Option<fn(&str, &str) -> bool>,
    pub input_buffer: Vec<String>,
    // the next line of `input_buffer` a read takes so reads never shift the rest
    pub(crate) input_at: usize,
    // the events pushed and not yet received read through a cursor
    pub(crate) event_queue: init::Events,
    pub strict_input: bool,
    /* byte offset of the deepest propagating error in the last `run()` */
    pub(crate) error_byte_pos: Option<u32>,
    /* where an error rose inside an imported module so its traceback starts there */
    pub(crate) error_inner: Option<(alloc::sync::Arc<String>, alloc::sync::Arc<String>, u32)>,
    /* the name or attribute the last error missed for its traceback's hint if uncaught */
    pub(crate) missed: Option<hints::Missed>,
    /* locals of each body a name was missed in measured once for every later miss */
    pub(crate) local_names: HashMap<(usize, usize), hints::Locals>,
    /* spec to module value filled by `init_modules` and read by LoadModule */
    pub(crate) module_table: HashMap<String, Val>,
    /* each function's module spec which scopes the free-load fallback to its own module */
    pub(crate) fn_module: Vec<Option<String>>,
    /* function names parallel to `functions` that tracebacks render where empty means a lambda */
    pub(crate) function_names: Vec<String>,
    /* each function by its qualified name such as `A.f` which errors and reprs give */
    pub(crate) function_qualnames: Vec<String>,
    /* qualified names of classes nested in a class or function keyed by the class value */
    pub(crate) class_qualnames: HashMap<u64, String>,
    /* frames an error unwound through innermost first built only while unwinding for the traceback renderer */
    pub(crate) call_stack: Vec<CallFrame>,
    /* the module body while a `run()` is suspended to the host and `None` between runs */
    pub(crate) root: Option<CoroutineHandle>,
    /* each instruction a chunk executed as one bit per ip read through `ran` */
    #[cfg(feature = "coverage")]
    pub(crate) executed: HashMap<*const SSAChunk, Vec<u64>>,
}

/* a chunk the machine registered with what its code reads of it */
pub(crate) struct Body {
    // the chunk itself compared with no read to tell a stale id from its own
    pub chunk: *const SSAChunk,
    // its function or `NO_BODY` for a module or class body
    pub fi: u32,
    // the module whose bindings its code reads
    pub module: u32,
    pub class: bool,
    // its pool or `NO_BODY` before it first runs
    pub pool: u32,
    // the global ids of the functions it defines in their order
    pub fns: Vec<u32>,
}

// a field of `Body` naming nothing
pub(crate) const NO_BODY: u32 = u32::MAX;

/* the function whose body `chunk` is among `bodies` */
#[inline]
pub(crate) fn fi_in(bodies: &[Body], chunk: &SSAChunk) -> Option<usize> {
    bodies.get(chunk.id.get().wrapping_sub(1) as usize).filter(|b| core::ptr::eq(b.chunk, chunk) && b.fi != NO_BODY).map(|b| b.fi as usize)
}

impl<'a> VM<'a> {
    /* the place of `chunk` among the registered bodies found by its id alone */
    #[inline]
    pub(crate) fn body_of(&self, chunk: &SSAChunk) -> Option<&Body> {
        self.bodies.get(chunk.id.get().wrapping_sub(1) as usize).filter(|b| core::ptr::eq(b.chunk, chunk))
    }

    /* the pool of `chunk` when it has one */
    #[inline]
    pub(crate) fn pool_at(&self, chunk: &SSAChunk) -> Option<usize> { self.body_of(chunk).map(|b| b.pool).filter(|&p| p != NO_BODY).map(|p| p as usize) }

    /* the place of `chunk` among the bodies registered as one now if it was not */
    pub(crate) fn body_at(&mut self, chunk: &SSAChunk) -> usize {
        let at = chunk.id.get().wrapping_sub(1) as usize;
        if self.bodies.get(at).is_some_and(|b| core::ptr::eq(b.chunk, chunk)) { return at; }
        self.bodies.push(Body { chunk, fi: NO_BODY, module: 0, class: false, pool: NO_BODY, fns: Vec::new() });
        chunk.id.set(self.bodies.len() as u32);
        self.bodies.len() - 1
    }

    /* the function whose body `chunk` is */
    #[inline]
    pub(crate) fn fi_of(&self, chunk: &SSAChunk) -> Option<usize> { fi_in(&self.bodies, chunk) }

    /* the function whose body has the id `id` */
    #[inline]
    pub(crate) fn fi_of_id(&self, id: u32) -> Option<usize> { self.bodies.get(id.wrapping_sub(1) as usize).filter(|b| b.fi != NO_BODY).map(|b| b.fi as usize) }

    /* whether `chunk` is a class body whose names stay a namespace in slots */
    #[inline]
    pub(crate) fn is_class_body(&self, chunk: &SSAChunk) -> bool { self.body_of(chunk).is_some_and(|b| b.class) }
}

/* what calling a class needs from its init and exception kind to its attribute count */
#[derive(Clone, Copy)]
pub(crate) struct Ctor {
    pub epoch: u32,
    pub init: Option<Val>,
    pub exception: bool,
    pub attrs: usize,
}

impl<'a> VM<'a> {
    pub fn new(chunk: &'a SSAChunk) -> Self { Self::with_limits(chunk, Limits::sandbox()) }

    /* the generic frames this VM pushed which `tests/nested.rs` holds at none */
    #[cfg(any(debug_assertions, feature = "coverage"))]
    #[doc(hidden)]
    pub fn old_path_counts(&self) -> [u64; 1] { [self.loops.generic] }

    /* whether `chunk.instructions[ip]` ran in this VM answered even after a run that raised */
    #[cfg(feature = "coverage")]
    pub fn ran(&self, chunk: &SSAChunk, ip: usize) -> bool {
        self.executed.get(&(chunk as *const _)).and_then(|bits| bits.get(ip / 64)).is_some_and(|word| word & (1 << (ip % 64)) != 0)
    }

    pub fn with_limits(chunk: &'a SSAChunk, limits: Limits) -> Self {
        let mut vm = Self::fields(chunk, limits, &crate::lang::Runtime::EMPTY);
        vm.boot(chunk);
        vm
    }

    /* a VM whose program runs under runtime `rt` */
    pub fn with_runtime(rt: &'static crate::lang::Runtime, chunk: &'a SSAChunk, limits: Limits) -> Self {
        let mut vm = Self::fields(chunk, limits, rt);
        vm.boot(chunk);
        vm
    }

    /* `with_runtime` built in its box so wasm boot copies no vm bytes fuel would charge */
    pub fn boxed(rt: &'static crate::lang::Runtime, chunk: &'a SSAChunk, limits: Limits) -> Box<Self> {
        let mut vm = Box::write(Box::new_uninit(), Self::fields(chunk, limits, rt));
        vm.boot(chunk);
        vm
    }

    /* the VM's fields before `boot` inlined so `boxed` writes them straight into its box */
    #[inline(always)]
    fn fields(chunk: &'a SSAChunk, limits: Limits, rt: &'static crate::lang::Runtime) -> Self {
        let natives = rt.natives;
        Self {
            rt,
            natives,
            own_loop: !rt.engine_loop,
            next_fold: core::cell::Cell::new(None),
            levels: Vec::new(),
            regs: Vec::with_capacity(256),
            base: 0,
            pool_now: 0,
            iter_stack: Vec::with_capacity(16),
            chunk,
            heap: HeapPool::new(limits.memory),
            scopes: alloc::vec![scope::Globals::of(natives)],
            scope_ids: HashMap::default(),
            bodies: Vec::new(),
            fn_scope: Vec::new(),
            fn_definer: Vec::new(),
            class_cells: Vec::new(),
            builtins: HashMap::default(),
            budget: limits.ops,
            depth: 0,
            #[cfg(any(debug_assertions, feature = "coverage"))]
            loops: Default::default(),
            #[cfg(debug_assertions)]
            stack_lowered: 0,
            #[cfg(debug_assertions)]
            stack_ops: [0; 256],
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
            held_pool: Vec::new(),
            temp_roots: Vec::new(),
            pending: Pending::new(),
            next_host_call_id: 0,
            extern_caller: None,
            compiled: Default::default(),
            pending_sync_frames: Vec::new(),
            pending_exec_exc_base: None,
            preempt_left: 0,
            preempt_every: 0,
            preempt_waited: 0,
            frame_safe: false,
            gen_frames: 0,
            chain: Default::default(),
            floor_req: false,
            floors: Vec::new(),
            gen_entry: 0,
            jobs: Vec::new(),
            job_ok: false,
            job_to: act::Dest::Top,
            retake: Val::undef(),
            out_line: String::new(),
            owned_str: Val::undef(),
            owned_at: usize::MAX,
            top: 0,
            park_top: 0,
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
            input_at: 0,
            event_queue: init::Events::default(),
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
            root: None,
            #[cfg(feature = "coverage")]
            executed: HashMap::default(),
            functions: Vec::new(),
            function_parents: Vec::new(),
            param_slots: Vec::new(),
            call_facts: Vec::new(),
            frame_bases: Vec::new(),
            executing_coros: Vec::new(),
            layout_epoch: 0,
            exc_memo: core::cell::Cell::new((u64::MAX, 0, Val::undef())),
            fn_remade: Vec::new(),
            shadowed: false,
            redo: redo::Redo::new(),
            shapes: Vec::new(),
            tier_up: false,
            relaying: false,
            lower_from: 0,
            growing: None,
            refused: false,
            reg_error: None,
            turned: 0,
            // the module and the frame about to open count toward the limit too
            deepest: (natives.policy.recursion_limit as usize).saturating_sub(2),
            turn_low: 0,
            turn_span: 0,
            turn_acts: 0,
            ctors: Default::default(),
            rebound: Vec::new(),
            any_rebound: 0,
            rule_hand: None,
            is_async: Vec::new(),
            laid_fns: Vec::new(),
            laid_mods: Vec::new(),
            learned: Vec::new(),
            grown: Vec::new(),
            replaying: false,
            replay_global: None,
            replay_laid: None,
            hint: None,
            default_slots: Vec::new(),
            // room for the pools of most programs so they never grow it
            pools: Vec::with_capacity(16),
            retired: Vec::new(),
            fn_pool: Vec::new(),
        }
    }

    /* what a fresh vm sets up from wide ints to the entry name */
    fn boot(&mut self, chunk: &'a SSAChunk) {
        let names = self.natives.names();
        // wide ints register first so no constant boxes one before them
        if let Some(kind) = self.rt.wide { self.heap.set_wide_int(kind); }
        (self.heap.names, self.heap.natives) = (names, self.natives);
        (self.heap.policy, self.heap.falsy) = (&self.natives.policy, self.natives.policy.falsy);
        #[cfg(all(feature = "std", not(target_arch = "wasm32")))]
        crate::value::err::shown::set(names);
        self.heap.set_budget(self.budget);
        self.build_function_table(chunk, None, None);
        self.index_functions(0);
        if let Ok(main_name) = self.heap.alloc(HeapObj::Str(names.entry.to_string())) {
            self.builtins.insert(names.module_name, main_name);
        }
    }

    /* derives per-function tables from `start` on which the REPL reruns for each adopted chunk */
    pub(crate) fn index_functions(&mut self, start: usize) {
        let end = self.functions.len();
        // a chunk that defines no function has nothing to add
        if start == end { return; }
        let new: Vec<Vec<(ParamKind, usize)>> = (start..end).map(|fi| {
            let (params, body, _, _) = self.functions[fi];
            params.iter().map(|p| {
                // a `~` prefix marks keyword-only parameters which follow a lone `*`
                let kind = if p.starts_with("**") {
                    ParamKind::KwRest
                } else if p.starts_with('*') {
                    ParamKind::Star
                } else if p.starts_with('~') {
                    ParamKind::KwOnly
                } else {
                    ParamKind::Normal
                };
                // a parameter binds the first version of its bare name
                let bare = crate::syntax::ssa::param_base_name(p);
                let slot = body.names.iter().position(|n| n.strip_suffix("_0") == Some(bare)).unwrap_or(usize::MAX);
                (kind, slot)
            }).collect()
        }).collect();
        self.param_slots.truncate(start);
        self.param_slots.extend(new);
        // the default-slot table of slot and placeholder pairs the call path overwrites
        let new: Vec<Vec<(usize, Val)>> = (start..end).map(|fi| {
            let (params, _, n_defaults, _) = self.functions[fi];
            if *n_defaults == 0 { return Vec::new(); }
            // defaults map to `=`-marked params in source order rather than the last `n_defaults` params
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
            // a body with leading positional slots and no cells opens in the loop calling it
            let in_loop = arity.is_some() && params.iter().enumerate().all(|(k, &(_, s))| s == k) && self.fn_scope[fi].cellvars.is_empty() && !body.is_generator;
            let required = self.functions[fi].0.iter().filter(|p| !p.ends_with('=')).count();
            let closure = !self.fn_scope[fi].freevars.is_empty();
            let plain = in_loop && !closure && arity == Some(required);
            let (star, full) = if arity.is_some() { (act::NO_SLOTS, act::NO_SLOTS) } else {
                // `star_slot` wants undefaulted positionals then `*args` and `full_slots` plain or keyword-only or `*args` in order
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

    /* a native's name the program binds or deletes makes its calls check the binding */
    pub(crate) fn note_builtin_binding(&mut self, bare: &str) {
        if let Some(id) = self.natives.function_id(bare) { self.rebind_native(id.0); }
    }

    /* the name of native `id` holds a program value so its calls check bindings */
    #[cold]
    #[inline(never)]
    pub(crate) fn rebind_native(&mut self, id: u16) {
        let (w, b) = (id as usize / 64, id % 64);
        if self.rebound.len() <= w { self.rebound.resize(w + 1, 0); }
        self.rebound[w] |= 1 << b;
        // the runtime opcodes ask again whether a name they fuse is bound
        self.any_rebound = run::OPS_STALE;
    }

    /* whether native `id`'s name was bound so its call looks at the binding */
    #[inline]
    pub(crate) fn native_rebound(&self, id: u16) -> bool {
        self.any_rebound != 0 && self.rebound.get(id as usize / 64).is_some_and(|w| w & 1 << (id % 64) != 0)
    }

    /* gives `bare` a heap slot if it names a builtin so unused builtins cost nothing */
    pub(crate) fn register_builtin(&mut self, bare: &str) {
        let names = self.natives.names();
        // most program names miss every builtin and convention on their outline alone
        if !self.natives.may_name(bare) { return; }
        if self.builtins.contains_key(bare) { return; }
        // type names stay type objects even when a native shares them
        let (name, made) = if let Some(name) = self.natives.type_named(bare) {
            (name, self.heap.type_object_static(name))
        } else if let Some(n) = self.natives.function(bare) {
            (n.name, if n.is_type() { self.heap.type_object_static(n.name) } else { self.heap.native_value(n.id) })
        } else if same_text(bare, names.ellipsis) {
            (names.ellipsis, self.heap.ellipsis())
        } else if same_text(bare, names.doc) {
            // the docstring of the entry chunk and nil without one
            match crate::vm::slow::attr::docstring(self.chunk) {
                Some(d) => (names.doc, self.heap.alloc(HeapObj::Str(d))),
                None => { self.builtins.insert(names.doc, Val::none()); return; }
            }
        } else if same_text(bare, crate::lang::ASSERT_CLASS) {
            // the runtime's names give the class whether or not its types list it
            (crate::lang::ASSERT_CLASS, self.heap.type_object_static(names.error(crate::lang::ErrKind::Assert)))
        } else {
            return;
        };
        if let Ok(v) = made { self.builtins.insert(name, v); }
    }

    /* the value of builtin `name` made on first use */
    pub(crate) fn builtin_value(&mut self, name: &'static str) -> Result<Val, VmErr> {
        self.register_builtin(name);
        match self.builtins.get(name) { Some(&v) => Ok(v), None => Err(VmErr::Heap) }
    }

    /* adopts `chunk` as the REPL's new entry module keeping state and running only that chunk */
    pub fn adopt_entry_chunk(&mut self, chunk: &'a SSAChunk) {
        self.adopt_functions(chunk, None);
        self.chunk = chunk;
    }

    /* adds the functions of `chunk` to the table with its names bound into module `spec` */
    pub(crate) fn adopt_functions(&mut self, chunk: &'a SSAChunk, spec: Option<&str>) {
        let start = self.functions.len();
        self.build_function_table(chunk, None, spec);
        self.index_functions(start);
    }

    /* the index and scope of `body`'s function with its names as this VM resolved them */
    pub fn scope_of(&self, body: &SSAChunk) -> Option<(usize, &FnScope)> {
        let fi = self.fi_of(body)?;
        Some((fi, &self.fn_scope[fi]))
    }

    /* the parameters of function `fi` each with its kind and the slot it binds */
    pub fn params_of(&self, fi: usize) -> &[(ParamKind, usize)] { &self.param_slots[fi] }

    /* what the allocator holds for the program including garbage since the last collection */
    pub fn memory(&self) -> usize { self.heap.bytes() }

    /* the most the allocator held for the program at once which benches compare across runs */
    pub fn memory_peak(&self) -> usize { self.heap.peak() }

    /* a fresh op budget for the next REPL input */
    pub fn reset_budget(&mut self, ops: u64) {
        self.budget = ops;
        self.heap.set_budget(ops);
    }

    /* mirrors compile-time extern bindings by name before exec so user rebinds win */
    pub fn bind_chunk_externs(&mut self) -> Result<(), VmErr> {
        let chunk = self.chunk;
        for (name, &idx) in chunk.extern_index.iter() {
            if let Some(b) = chunk.extern_table.get(idx as usize) {
                let v = self.heap.host_value(b)?;
                self.scopes[0].set(name, v);
            }
        }
        Ok(())
    }

    /* discards transient execution state for a parked REPL's next input keeping heap and all bindings */
    pub fn clear_error_state(&mut self) {
        self.levels.clear();
        self.regs.clear();
        (self.acts, self.top_pool) = (0, 0);
        self.instances.clear();
        self.jobs.clear();
        for p in &mut self.pools { (p.window, p.wlen, p.active, p.wcode) = (window::NO_WINDOW, 0, 0, 0); p.wins.clear(); p.w0 = window::Win { base: window::NO_WINDOW, len: 0, code: 0, owner: Val::undef() }; }
        self.owners = 0;
        (self.base, self.pool_now) = (0, 0);
        self.iter_stack.clear();
        self.exception_stack.clear();
        self.unwind_stack.clear();
        self.temp_roots.clear();
        self.call_stack.clear();
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
