use crate::engine::abi::bridge::BridgeState;
use crate::engine::r#unsafe::Single;
use crate::engine::value::Limits;
use crate::engine::vm::VM;
use crate::engine::deps::Deps;
use crate::engine::syntax::chunk::SSAChunk;
use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};

// wires parser and VM to the host via the handle ABI the `abi` module defines
pub(crate) mod entry;
pub(crate) mod exports;
mod resolver;
pub(crate) mod walk;

pub use entry::Entry;
pub(crate) use resolver::system_call;
pub(super) use crate::engine::deps::ModuleEntry;

/* every block's meter tag names its payer atop a small block pool over O(1) dlmalloc */
#[cfg(target_arch = "wasm32")]
crate::allocator!(crate::engine::value::pool::Pooled<dlmalloc::GlobalDlmalloc> = crate::engine::value::pool::Pooled::new(dlmalloc::GlobalDlmalloc));

// a native runtime meters what it allocates over the system allocator
#[cfg(not(target_arch = "wasm32"))]
crate::allocator!();

#[cfg(all(not(target_arch = "wasm32"), not(feature = "std")))]
compile_error!("a native runtime builds with the std feature, which catches its panics and gives it the system allocator");

// the static data a layout build shifts so a count that follows an address shows
#[cfg(feature = "unstable-layout")]
#[used]
static PAD: [u8; 64] = [0x5a; 64];

/* stashes a panic so the host gets a typed message though re-entry while formatting traps */
#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let msg = alloc::format!("internal panic: {}", info.message());
    crate::engine::abi::bridge::stash_raw_error(crate::engine::abi::ErrorKind::Runtime as u32, msg);
    core::arch::wasm32::unreachable()
}

// parsed entry programs kept for slots that boot the same source
const CHUNK_CACHE: usize = 8;

/* a VM suspended on `HostYield` kept from `run_start` to `run_resume` for cooperative resume */
pub(super) struct PausedRun {
    /* an `Option` so `step_vm` can `take()` for re-entry and stash back without a dummy VM */
    pub vm: Option<Box<VM<'static>>>,
}

/* one interpreter the host drives with the exports acting on the selected slot */
pub(super) struct Slot {
    /* owned across `run_start` and `run_resume` and mutually exclusive with the bridge's `current_vm` */
    pub paused_run: Option<Box<PausedRun>>,
    /* the REPL interpreter kept alive between `repl_eval` inputs */
    pub repl_vm: Option<Box<VM<'static>>>,
    pub repl_mode: bool,
    /* host-fed stdin for the next boot with one `input()` call per line */
    pub input: Vec<u8>,
    /* back-edges between preempt yields where 0 disables them */
    pub preempt_every: usize,
    /* the next boot's caps which stay the sandbox profile until the host sets its own */
    pub limits: Option<Limits>,
    /* the last run's memory peak that `memory_peak` reads even after the VM is gone */
    pub memory_peak: usize,
    /* the script `set_entry` named for the traceback entry frame or empty for the anonymous marker */
    pub source_name: String,
    /* the dir of the `set_entry` entry rooting the walk and the source's quoted imports */
    pub entry_dir: String,
    /* declared after the VMs that borrow them so a dropped slot frees those VMs first */
    pub chunks: Vec<Rc<SSAChunk>>,
    /* the handles stash and live VM pointer parked here while another slot is selected */
    pub bridge: BridgeState,
}

impl Slot {
    fn new() -> Self {
        Slot {
            paused_run: None,
            repl_vm: None,
            repl_mode: false,
            input: Vec::new(),
            preempt_every: 0,
            limits: None,
            memory_peak: 0,
            source_name: String::new(),
            entry_dir: String::new(),
            chunks: Vec::new(),
            bridge: BridgeState::new(),
        }
    }

    /* drops the run and REPL interpreters and memory peak then the chunks they borrowed */
    pub(super) fn clear_run(&mut self) {
        self.paused_run = None;
        self.repl_vm = None;
        self.repl_mode = false;
        self.memory_peak = 0;
        self.chunks.clear();
    }
}

/* host state behind `with_runtime` while the handles and the stash live in the abi bridge */
pub(super) struct WasmRuntime {
    /* the last result text or snapshot that `out_ptr` and `out_len` read before the next call */
    pub out: Vec<u8>,
    /* the modules and refusals the host registered and what resolving them read */
    pub deps: Deps,
    /* the manifest field each walk carries once the host names one */
    pub carry: Option<crate::engine::deps::walk::Carry>,
    /* entry chunks with the entry dir they were parsed under kept most recent last */
    pub chunk_cache: Vec<(String, Rc<SSAChunk>)>,
    pub slots: Vec<Option<Slot>>,
    pub current: usize,
}

impl WasmRuntime {
    const fn new() -> Self {
        Self {
            out: Vec::new(),
            deps: Deps::new(),
            carry: None,
            chunk_cache: Vec::new(),
            slots: Vec::new(),
            current: 0,
        }
    }

    /* the selected slot with slot 0 made on first use so single-VM hosts never select */
    pub(super) fn slot(&mut self) -> &mut Slot {
        if self.slots.is_empty() {
            self.slots.push(Some(Slot::new()));
        }
        let current = self.current;
        self.slots[current].get_or_insert_with(Slot::new)
    }

    /* any registration can change what a source compiles to so parsed chunks go stale */
    pub(super) fn registry_changed(&mut self) {
        self.chunk_cache.clear();
    }
}

/* the interpreter state of one wasm instance or of one native context */
pub(crate) struct State {
    runtime: Single<WasmRuntime>,
    bridge: Single<BridgeState>,
}

impl State {
    pub(crate) const fn new() -> Self {
        State { runtime: Single::new(WasmRuntime::new()), bridge: Single::new(BridgeState::new()) }
    }
}

// a wasm instance is one context so its state is the module's own
#[cfg(target_arch = "wasm32")]
static STATE: State = State::new();

#[cfg(target_arch = "wasm32")]
#[inline(always)]
pub(crate) fn state() -> &'static State { &STATE }

/* the state of the context the running native export entered */
#[cfg(not(target_arch = "wasm32"))]
#[inline(always)]
pub(crate) fn state() -> &'static State { &crate::engine::r#unsafe::web::native::current().state }

// re-entrant callers route through `with_vm` to drop the loan first
pub(super) fn with_runtime<R>(f: impl FnOnce(&mut WasmRuntime) -> R) -> R { state().runtime.with(f) }

/* the handles and the stash of the running context which the abi bridge lends */
pub(crate) fn with_bridge<R>(f: impl FnOnce(&mut BridgeState) -> R) -> R { state().bridge.with(f) }

pub(super) fn with_slot<R>(f: impl FnOnce(&mut Slot) -> R) -> R {
    with_runtime(|rt| f(rt.slot()))
}

pub(super) fn write_out(s: &str) -> usize {
    write_out_bytes(s.as_bytes().to_vec())
}

pub(super) fn write_out_bytes(b: Vec<u8>) -> usize {
    with_runtime(|rt| {
        rt.out = b;
        rt.out.len()
    })
}
