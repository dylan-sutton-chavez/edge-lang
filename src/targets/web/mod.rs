use crate::abi::bridge::BridgeState;
use crate::r#unsafe::Single;
use crate::value::Limits;
use crate::vm::VM;
use crate::deps::Deps;
use crate::syntax::chunk::SSAChunk;
use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};

// wires parser and VM to the host via the handle ABI the `abi` module defines
pub(crate) mod entry;
pub(crate) mod exports;
pub(crate) mod package;
#[cfg(feature = "perms")]
pub(crate) mod perms;
mod resolver;
pub(crate) mod walk;

pub use entry::Entry;
pub(crate) use resolver::system_call;
pub(super) use crate::deps::ModuleEntry;

/* every block's meter tag names its payer atop a small block pool over O(1) dlmalloc */
crate::allocator!(crate::value::pool::Pooled<dlmalloc::GlobalDlmalloc> = crate::value::pool::Pooled::new(dlmalloc::GlobalDlmalloc));

// the static data a layout build shifts so a count that follows an address shows
#[cfg(feature = "layout")]
#[used]
static PAD: [u8; 64] = [0x5a; 64];

/* stashes a panic so the host gets a typed message though re-entry while formatting traps */
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let msg = alloc::format!("internal panic: {}", info.message());
    crate::abi::bridge::stash_raw_error(crate::abi::ErrorKind::Runtime as u32, msg);
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
    pub fn clear_run(&mut self) {
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
    pub carry: Option<crate::deps::walk::Carry>,
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
    pub fn slot(&mut self) -> &mut Slot {
        if self.slots.is_empty() {
            self.slots.push(Some(Slot::new()));
        }
        let current = self.current;
        self.slots[current].get_or_insert_with(Slot::new)
    }

    /* any registration can change what a source compiles to so parsed chunks go stale */
    pub fn registry_changed(&mut self) {
        self.chunk_cache.clear();
    }

    /* registers `spec` over whatever an earlier registration of it left */
    pub fn register(&mut self, spec: String, entry: ModuleEntry) {
        self.deps.register(spec, entry);
        self.registry_changed();
    }

    /* makes an import of `spec` fail with `msg` */
    pub fn refuse(&mut self, spec: String, msg: String) {
        self.deps.refuse(spec, msg);
        self.registry_changed();
    }
}

static RUNTIME: Single<WasmRuntime> = Single::new(WasmRuntime::new());

// re-entrant callers route through `with_vm` to drop the loan first
pub(super) fn with_runtime<R>(f: impl FnOnce(&mut WasmRuntime) -> R) -> R { RUNTIME.with(f) }

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
