use crate::bridge::BridgeState;
use crate::vm::{Limits, VM};
use crate::modules::Manifest;
use crate::parser::SSAChunk;
use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};

// Wires parser and VM to the host via the handle ABI, the wire contract lives in `crate::abi`.
mod exports;
mod package;
mod resolver;
mod walk;

pub(crate) use resolver::system_call;

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    pub(super) fn host_print(ptr: *const u8, len: usize);

    /* CallExtern dispatch for register_native_module, `call_id` correlates a deferred result back to its coro. */
    pub(super) fn host_call_native(id: u32, call_id: u32, argv_ptr: *const u32, argc: u32, out: *mut u32) -> i32;

    /* Host-cached bytes for `spec`. Non-null `hash_ptr` is a 32-byte expected sha-256. */
    pub(super) fn host_fetch_bytes(spec_ptr: *const u8, spec_len: u32, hash_ptr: *const u8, out_len: *mut u32) -> *mut u8;

    /* Wall clock in nanoseconds, without it the VM falls back to a deterministic virtual clock for tests. */
    pub(super) fn host_now_ns() -> u64;

    /* Hands `send(group, body)` to the host scheduler, non-zero means this host has none. */
    pub(super) fn host_send(group_ptr: *const u8, group_len: u32, body_ptr: *const u8, body_len: u32) -> i32;
}

pub(super) fn stream_print(s: &str) {
    unsafe { host_print(s.as_ptr(), s.len()); }
}

/* `set_time_hook` wants a `fn() -> u64`, so the unsafe import is wrapped like `stream_print`. */
pub(super) fn now_ns_host() -> u64 {
    unsafe { host_now_ns() }
}

pub(super) fn send_host(group: &str, body: &str) -> bool {
    unsafe { host_send(group.as_ptr(), group.len() as u32, body.as_ptr(), body.len() as u32) == 0 }
}

/* dlmalloc keeps alloc and free O(1), the pool keeps freed small blocks, the meter tags each block with its payer. */
#[global_allocator]
static A: crate::value::meter::Metered<crate::value::pool::Pooled<dlmalloc::GlobalDlmalloc>> = crate::value::meter::Metered(crate::value::pool::Pooled::new(dlmalloc::GlobalDlmalloc));

/* Best-effort panic-to-stash so the host gets a typed message, re-entry during the format still traps. */
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let msg = alloc::format!("internal panic: {}", info.message());
    crate::bridge::stash_raw_error(crate::abi::ErrorKind::Runtime as u32, msg);
    core::arch::wasm32::unreachable()
}

// Parsed entry programs kept for slots that boot the same source.
const CHUNK_CACHE: usize = 8;

#[derive(Clone)]
pub(super) enum ModuleEntry {
    Code(String),
    Native(Vec<(String, u32)>),
}

/* VM suspended on `VmErr::HostYield`, kept across `run_start` -> `run_resume` for cooperative resume. */
pub(super) struct PausedRun {
    /* Option so `step_vm` can `take()` for re-entry and stash back without a dummy VM. */
    pub vm: Option<Box<VM<'static>>>,
    /* Earliest wake-up deadline (ns) from the last yield, zero for `PendingEvent`. */
    pub last_yield_deadline_ns: u64,
}

/* One interpreter the host drives, the exports act on the selected slot. */
pub(super) struct Slot {
    /* Owned across `run_start` / `run_resume`, mutually exclusive with the bridge's `current_vm`. */
    pub paused_run: Option<Box<PausedRun>>,
    /* REPL, the interpreter kept alive between `repl_eval` inputs. */
    pub repl_vm: Option<Box<VM<'static>>>,
    pub repl_mode: bool,
    /* Host-fed stdin for the next boot, one `input()` call per line. */
    pub input: Vec<u8>,
    /* Back-edges between preempt yields, 0 disables. */
    pub preempt_every: usize,
    /* Caps for the next boot, the sandbox profile until the host sets its own. */
    pub limits: Option<Limits>,
    /* Whether the next boot sleeps on the host's clock, off keeps the virtual one. */
    pub wall_clock: bool,
    /* The memory peak of the last run, read through `memory_peak` once the VM is gone. */
    pub memory_peak: usize,
    /* The script `set_entry` named, naming the entry frame in tracebacks, empty renders the anonymous marker. */
    pub source_name: String,
    /* Directory of the entry `set_entry` named, rooting the walk and the quoted imports of the source. */
    pub entry_dir: String,
    /* Chunks the VMs above borrow, declared after them so a dropped slot frees the VMs first. */
    pub chunks: Vec<Rc<SSAChunk>>,
    /* Handles, stash and live VM pointer parked here while another slot is selected. */
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
            wall_clock: true,
            memory_peak: 0,
            source_name: String::new(),
            entry_dir: String::new(),
            chunks: Vec::new(),
            bridge: BridgeState::new(),
        }
    }

    /* Drops the run and REPL interpreters and the last memory peak, then the chunks they borrowed. */
    pub fn clear_run(&mut self) {
        self.paused_run = None;
        self.repl_vm = None;
        self.repl_mode = false;
        self.memory_peak = 0;
        self.chunks.clear();
    }
}

/* Mutable WASM-host state behind `with_runtime`, handles and the stash live in `crate::bridge`. */
pub(super) struct WasmRuntime {
    /* Last result text or snapshot, read through `out_ptr` / `out_len` before the next call. */
    pub out: Vec<u8>,
    pub registry: Vec<(String, ModuleEntry)>,
    pub manifests: Vec<(String, Manifest)>,
    /* Specs the host refuses, importing one fails at the import with its message. */
    pub refusals: Vec<(String, String)>,
    /* Entry chunks with the entry dir they were parsed under, most recent last. */
    pub chunk_cache: Vec<(String, Rc<SSAChunk>)>,
    pub slots: Vec<Option<Slot>>,
    pub current: usize,
    /* The resolution walk the host is driving, and the plugin bytes its last step named. */
    pub walk: Option<crate::modules::walk::Walk>,
    pub walk_plugin: Vec<u8>,
    /* The package dir each source dir belongs to, found once per dir. */
    pub packages: Vec<(String, Option<String>)>,
}

impl WasmRuntime {
    const fn new() -> Self {
        Self {
            out: Vec::new(),
            registry: Vec::new(),
            manifests: Vec::new(),
            refusals: Vec::new(),
            chunk_cache: Vec::new(),
            slots: Vec::new(),
            current: 0,
            walk: None,
            walk_plugin: Vec::new(),
            packages: Vec::new(),
        }
    }

    /* The selected slot, slot 0 exists from the first call so a single-VM host never selects. */
    pub fn slot(&mut self) -> &mut Slot {
        if self.slots.is_empty() {
            self.slots.push(Some(Slot::new()));
        }
        let current = self.current;
        self.slots[current].get_or_insert_with(Slot::new)
    }

    /* Any registration can change what a source compiles to, so parsed chunks go stale. */
    pub fn registry_changed(&mut self) {
        self.chunk_cache.clear();
    }

    /* Registers `spec`, replacing what an earlier registration of it left. */
    pub fn register(&mut self, spec: String, entry: ModuleEntry) {
        self.registry.retain(|(s, _)| *s != spec);
        self.registry.push((spec, entry));
        self.registry_changed();
    }

    /* Makes an import of `spec` fail with `msg`. */
    pub fn refuse(&mut self, spec: String, msg: String) {
        self.refusals.retain(|(s, _)| *s != spec);
        self.refusals.push((spec, msg));
        self.registry_changed();
    }
}

static mut RUNTIME: WasmRuntime = WasmRuntime::new();

// SAFETY single-threaded WASM, re-entrant callers route through `with_vm` to drop the borrow first.
pub(super) fn with_runtime<R>(f: impl FnOnce(&mut WasmRuntime) -> R) -> R {
    unsafe { f(&mut *core::ptr::addr_of_mut!(RUNTIME)) }
}

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
