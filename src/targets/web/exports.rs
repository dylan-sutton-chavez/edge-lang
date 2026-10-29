use crate::deps::{HostResolver, entry_dir};
use crate::syntax::{chunk::SSAChunk, diag::Diagnostic};
use crate::value::Limits;
use crate::vm::VM;
use crate::value::{HeapObj, SchedulerStatus, VmErr};
use alloc::{boxed::Box, rc::Rc, string::{String, ToString}};
use core::ptr::NonNull;
use crate::s;

use super::{ModuleEntry, PausedRun, CHUNK_CACHE, with_runtime, with_slot, write_out, write_out_bytes};
use super::resolver::WebHost;
use crate::abi::bridge::{self, BridgeState, VmGuard};
use crate::r#unsafe::{pinned, web};

/* `run_start` and `run_resume` pack the kind in the top 3 bits above the out length */
const STATUS_KIND_SHIFT: u32 = 29;
const STATUS_PAYLOAD_MASK: u32 = (1 << STATUS_KIND_SHIFT) - 1;
const STATUS_DONE: u32 = 0 << STATUS_KIND_SHIFT;
// kinds 1 and 2 retired with sleep and frame yields so others keep their numbers
const STATUS_PENDING_EVENT: u32 = 3 << STATUS_KIND_SHIFT;
const STATUS_ERROR: u32 = 4 << STATUS_KIND_SHIFT;
const STATUS_PENDING_HOST_CALL: u32 = 5 << STATUS_KIND_SHIFT;
// an uncaught exit ends cleanly with the exit code in the low 8 bits
const STATUS_EXIT: u32 = 6 << STATUS_KIND_SHIFT;
// a preempt tick resumes with no host action
const STATUS_PREEMPTED: u32 = 7 << STATUS_KIND_SHIFT;

/* the error text lands in the out buffer and `out_len` reports its full length */
fn err_status(msg: &str) -> u32 {
    let n = write_out(msg);
    STATUS_ERROR | (n as u32).min(STATUS_PAYLOAD_MASK)
}

/* the program the front end makes of `src` importing from `dir` or its rendered errors */
fn parse_source(src: &str, dir: String) -> Result<Rc<SSAChunk>, String> {
    let resolver = Box::new(HostResolver::new(dir, WebHost));
    let mut chunk = web::entry().front().program(src, resolver).map_err(|errs| Diagnostic::render_all(&errs, src, source_name().as_deref()))?;
    if let Some(chunk) = Rc::get_mut(&mut chunk) { crate::code::fold::constant_fold(chunk, web::entry().front().policy); }
    Ok(chunk)
}

/* the parsed program for `src` that every slot booting it from the same dir shares */
fn entry_chunk(src: &str) -> Result<Rc<SSAChunk>, String> {
    let dir = with_slot(|s| s.entry_dir.clone());
    let hit = with_runtime(|rt| {
        let i = rt.chunk_cache.iter().position(|(d, c)| *d == dir && c.source.as_str() == src)?;
        let entry = rt.chunk_cache.remove(i);
        let chunk = entry.1.clone();
        rt.chunk_cache.push(entry);
        Some(chunk)
    });
    if let Some(chunk) = hit {
        return Ok(chunk);
    }
    let chunk = parse_source(src, dir.clone())?;
    with_runtime(|rt| {
        if rt.chunk_cache.len() >= CHUNK_CACHE {
            rt.chunk_cache.remove(0);
        }
        rt.chunk_cache.push((dir, chunk.clone()));
    });
    Ok(chunk)
}

/* the caps a fresh boot runs under from the host's `set_limits` or the sandbox profile */
fn limits() -> Limits {
    with_slot(|s| s.limits).unwrap_or_else(Limits::sandbox)
}

/* the entry frame name for tracebacks or `None` until the host names the source */
fn source_name() -> Option<String> {
    with_slot(|s| (!s.source_name.is_empty()).then(|| s.source_name.clone()))
}

/* boots a boxed VM on `chunk` so the exports can pass a pointer */
fn boot_vm(chunk: Rc<SSAChunk>, limits: Limits) -> Box<VM<'static>> {
    // the slot's `Rc` outlives the VM since `clear_run` drops VMs before chunks
    let chunk_static: &'static SSAChunk = pinned(&chunk);
    let preempt = with_slot(|s| {
        s.chunks.push(chunk);
        s.preempt_every
    });
    let mut vm = VM::boxed(web::entry().runtime(), chunk_static, limits);
    vm.print_hook = Some(web::print);
    vm.send_hook = Some(taken);
    vm.set_preempt_interval(preempt);
    vm
}

/* whether the host took a send where a refusal reads as not taken */
fn taken(group: &str, body: &str) -> bool {
    sent(group, body).unwrap_or(false)
}

/* whether the host took a send or the error it stashed refusing the send */
pub(crate) fn sent(group: &str, body: &str) -> Result<bool, VmErr> {
    match web::send(group, body) {
        0 => Ok(true),
        2 => refusal(),
        _ => Ok(false),
    }
}

/* the error a host stashed as it refused a send */
#[cold]
#[inline(never)]
fn refusal() -> Result<bool, VmErr> {
    bridge::take_error().map_or(Ok(false), |(kind, msg)| Err(bridge::error_from_kind(kind, msg)))
}

/* the package dir of the code sending now which a host asks inside a send */
pub(crate) fn send_package() -> u32 {
    let file = bridge::with_vm(|vm| vm.running_file()).flatten().filter(|file| !file.is_empty());
    // the entry chunk has no path so its code belongs to the entry dir
    let dir = file.map_or_else(|| with_slot(|s| s.entry_dir.clone()), |file| crate::deps::dir_of(&file));
    write_out(&crate::deps::served::package_of(&mut WebHost, &dir).unwrap_or_default()) as u32
}

/* drains host-fed stdin into the VM with invalid UTF-8 read as empty */
fn take_input(vm: &mut VM) {
    let inp = with_slot(|s| core::mem::take(&mut s.input));
    let inp_text = core::str::from_utf8(&inp).unwrap_or("");
    if !inp_text.is_empty() {
        let _paying = crate::value::meter::Paying::to(vm.heap.meter());
        // one line per `input()` call with any trailing carriage return dropped
        vm.input_buffer = inp_text.split('\n').map(|l| String::from(l.strip_suffix('\r').unwrap_or(l))).collect();
        vm.input_at = 0;
    }
}

/* a fresh boot hard-resets the selected slot and the bridge stops pointing into it */
fn reset_run() {
    with_slot(|s| s.clear_run());
    bridge::set_current_vm(None);
}

/* copies a source out of the buffer the host frees after the call */
fn source_arg(src: &[u8]) -> Result<String, u32> {
    core::str::from_utf8(src)
        .map(|s| s.to_string())
        .map_err(|e| err_status(&s!("input rejected: invalid utf-8 at byte ", int e.valid_up_to())))
}

/* host-fed stdin for the next boot */
pub(crate) fn set_input(input: &[u8]) {
    let bytes = input.to_vec();
    with_slot(|s| s.input = bytes);
}

pub(crate) fn out_ptr() -> *const u8 {
    with_runtime(|rt| rt.out.as_ptr())
}

pub(crate) fn out_len() -> u32 {
    with_runtime(|rt| rt.out.len() as u32)
}

/* a fresh interpreter slot as the id `vm_select` takes */
pub(crate) fn vm_create() -> u32 {
    with_runtime(|rt| {
        rt.slot();
        let fresh = Some(super::Slot::new());
        match rt.slots.iter().position(Option::is_none) {
            Some(i) => {
                rt.slots[i] = fresh;
                i as u32
            }
            None => {
                rt.slots.push(fresh);
                (rt.slots.len() - 1) as u32
            }
        }
    })
}

/* points per-run exports at slot `id` giving 0 or 1 when no such slot exists */
pub(crate) fn vm_select(id: u32) -> i32 {
    with_runtime(|rt| select(rt, id as usize))
}

fn select(rt: &mut super::WasmRuntime, id: usize) -> i32 {
    rt.slot();
    if !rt.slots.get(id).is_some_and(Option::is_some) { return 1; }
    if id == rt.current { return 0; }
    // the bridge holds the selected slot's handles so park them and load the next slot's
    let active = bridge::with_bridge(|b| core::mem::replace(b, BridgeState::new()));
    rt.slot().bridge = active;
    rt.current = id;
    let incoming = core::mem::replace(&mut rt.slot().bridge, BridgeState::new());
    bridge::with_bridge(|b| *b = incoming);
    0
}

/* frees slot `id` and its runs but slot 0 stays and 1 means nothing dropped */
pub(crate) fn vm_drop(id: u32) -> i32 {
    let id = id as usize;
    with_runtime(|rt| {
        if id == 0 || !rt.slots.get(id).is_some_and(Option::is_some) { return 1; }
        if id == rt.current {
            select(rt, 0);
        }
        rt.slots[id] = None;
        0
    })
}

pub(crate) fn register_native_module(spec: &str, names: &str, base_id: u32) {
    use alloc::vec::Vec;
    let spec = spec.to_string();
    let funcs: Vec<(String, u32)> = names.split('\n')
        .filter(|n| !n.is_empty())
        .enumerate()
        .map(|(i, name)| (name.to_string(), base_id + i as u32))
        .collect();
    with_runtime(|rt| rt.register(spec, ModuleEntry::Native(funcs)));
}

/* a spec or bare name the host cannot load so its import fails with `msg` */
pub(crate) fn register_module_error(spec: &str, msg: &str) {
    with_runtime(|rt| rt.refuse(spec.to_string(), msg.to_string()));
}

pub(crate) fn reset_modules() {
    with_runtime(|rt| {
        rt.deps.reset();
        rt.registry_changed();
        // a paused run references the now-stale module table so drop it for a clean reset
        rt.slot().clear_run();
    });
    // the bridge's `current_vm` may point into the dropped paused VM so reset clears it too
    bridge::reset();
}

/* the next run's script whose name labels tracebacks and dir roots the walk and imports */
pub(crate) fn set_entry(entry: &str) {
    with_slot(|s| {
        s.entry_dir = entry_dir(entry);
        // a directory or no entry names no script so its frame keeps the anonymous marker
        s.source_name = if entry.ends_with('/') { String::new() } else { entry.to_string() };
    });
}

// half the address space at most since garbage may take as much again before collecting
const MEMORY_CEILING: usize = usize::MAX / 2;

/* caps for the next run with memory in bytes and zero keeping the sandbox value */
pub(crate) fn set_limits(memory: u64, ops: u64) {
    let sandbox = Limits::sandbox();
    let pick = |v: u64, fallback: usize| if v == 0 { fallback } else { usize::try_from(v).unwrap_or(usize::MAX) };
    let limits = Limits { memory: pick(memory, sandbox.memory).min(MEMORY_CEILING), ops: if ops == 0 { sandbox.ops } else { ops } };
    with_slot(|s| s.limits = Some(limits));
}

/* drives one segment and on a yield re-stashes the VM in the recycled `PausedRun` box */
fn step_vm(mut vm: Box<VM<'static>>, src: &str, prev_paused: Option<Box<PausedRun>>) -> u32 {
    let result = {
        let _guard = VmGuard::new(&mut vm);
        vm.run()
    };
    with_slot(|s| s.memory_peak = vm.memory_peak());
    match result {
        Ok(_) => {
            park_repl_or_drop(vm);
            drop(prev_paused);
            STATUS_DONE
        }
        Err(VmErr::HostYield(status)) => {
            let kind = match status {
                SchedulerStatus::PendingEvent => STATUS_PENDING_EVENT,
                SchedulerStatus::PendingHostCall => STATUS_PENDING_HOST_CALL,
                SchedulerStatus::Preempted => STATUS_PREEMPTED,
            };
            let mut paused = match prev_paused {
                Some(mut b) => { b.vm = Some(vm); b }
                None => Box::new(PausedRun { vm: Some(vm) }),
            };
            // re-publish `current_vm` at the boxed VM whose stable address lets embedder calls between yields allocate
            let vm_ptr = paused.vm.as_deref_mut().map(|v| NonNull::from(v).cast::<VM<'static>>());
            with_slot(|s| s.paused_run = Some(paused));
            bridge::set_current_vm(vm_ptr);
            kind
        }
        Err(e) => {
            // an uncaught exit with an integer code ends cleanly rather than crashing
            if let Some(code) = vm.system_exit_code() {
                park_repl_or_drop(vm);
                drop(prev_paused);
                return STATUS_EXIT | ((code as u32) & 0xFF);
            }
            let name = source_name();
            let traceback = vm.report_error(&e, src, name.as_deref());
            // a failed input keeps its partial effects
            park_repl_or_drop(vm);
            drop(prev_paused);
            err_status(&traceback)
        }
    }
}

/* a finished VM stays for the next REPL input or else drops with its chunks */
fn park_repl_or_drop(mut vm: Box<VM<'static>>) {
    let repl_mode = with_slot(|s| s.repl_mode);
    if repl_mode {
        vm.clear_error_state();
        bridge::set_current_vm(None);
        with_slot(|s| s.repl_vm = Some(vm));
    } else {
        drop(vm);
        with_slot(|s| if s.paused_run.is_none() { s.chunks.clear(); });
    }
}

/* the first REPL input boots the interpreter and later ones adopt a new entry chunk */
pub(crate) fn repl_eval(src: &[u8]) -> u32 {
    let src = match source_arg(src) {
        Ok(s) => s,
        Err(status) => return status,
    };
    let chunk = match parse_source(&src, with_slot(|s| s.entry_dir.clone())) {
        Ok(c) => c,
        Err(rendered) => return err_status(&rendered),
    };
    let existing = with_slot(|s| {
        s.repl_mode = true;
        s.paused_run = None;
        s.repl_vm.take()
    });
    let mut vm = match existing {
        Some(mut vm) => {
            // the REPL slot keeps every input's chunk while its interpreter lives
            let chunk_static: &'static SSAChunk = pinned(&chunk);
            with_slot(|s| s.chunks.push(chunk));
            vm.adopt_entry_chunk(chunk_static);
            vm.reset_budget(limits().ops);
            vm
        }
        None => boot_vm(chunk, limits()),
    };
    // named native imports live only in the chunk's externs so mirror them for later inputs
    if let Err(e) = vm.bind_chunk_externs() {
        let name = source_name();
        let traceback = vm.report_error(&e, &src, name.as_deref());
        park_repl_or_drop(vm);
        return err_status(&traceback);
    }
    take_input(&mut vm);
    step_vm(vm, &src, None)
}

pub(crate) fn run_start(src: &[u8]) -> u32 {
    // a fresh `run_start` hard-resets execution state
    reset_run();
    let src = match source_arg(src) {
        Ok(s) => s,
        Err(status) => return status,
    };
    let chunk = match entry_chunk(&src) {
        Ok(c) => c,
        Err(rendered) => return err_status(&rendered),
    };
    let mut vm = boot_vm(chunk, limits());
    take_input(&mut vm);
    step_vm(vm, &src, None)
}

pub(crate) fn run_resume() -> u32 {
    let paused = match with_slot(|s| s.paused_run.take()) {
        Some(p) => p,
        None => return err_status(&s!(str web::entry().runtime().natives.names().error(crate::lang::ErrKind::Runtime), ": run_resume called with no paused run")),
    };
    // take the VM out for `step_vm` and recycle the empty box for the next stash
    let mut paused_box = paused;
    let vm = paused_box.vm.take().expect("paused_run with no VM is a runtime bug");
    // an error past a suspension still renders against the entry source
    let src = vm.chunk.source.clone();
    step_vm(vm, &src, Some(paused_box))
}

pub(crate) fn run_push_event(event: &[u8]) -> i32 {
    let Ok(s) = core::str::from_utf8(event) else { return 1 };
    with_slot(|slot| {
        let Some(paused) = slot.paused_run.as_mut() else { return 1; };
        let Some(vm) = paused.vm.as_mut() else { return 1; };
        if vm.push_event(s).is_ok() { 0 } else { 2 }
    })
}

/* the shared `set_host_*` prologue where 1 means a stale handle and 3 no paused run */
fn with_paused_vm(handle: u32, f: impl FnOnce(&mut VM<'static>, crate::value::Val) -> i32) -> i32 {
    let Some(val) = bridge::get_val(handle) else { return 1; };
    bridge::release_handles(&[handle]);
    with_slot(|slot| {
        let Some(paused) = slot.paused_run.as_mut() else { return 3; };
        let Some(vm) = paused.vm.as_mut() else { return 3; };
        let _paying = crate::value::meter::Paying::to(vm.heap.meter());
        f(vm, val)
    })
}

/* wakes coro `id` with `handle` and gives 2 when no waiter matches */
pub(crate) fn set_host_result_by_id(id: u32, handle: u32) -> i32 {
    bridge::answered(id);
    with_paused_vm(handle, |vm, val| if vm.inject_host_result_by_id(id as u64, val) { 0 } else { 2 })
}

/* raises an error with str `msg_handle` into the `WaitingHostCall(id)` coro so its try/except catches it */
pub(crate) fn set_host_error_by_id(id: u32, kind: u32, msg_handle: u32) -> i32 {
    bridge::answered(id);
    with_paused_vm(msg_handle, |vm, val| {
        let msg = match vm.heap.get(val) {
            HeapObj::Str(s) => s.clone(),
            _ => String::new(),
        };
        let e = bridge::error_from_kind(kind, msg);
        if vm.inject_host_error_by_id(id as u64, e) { 0 } else { 2 }
    })
}

/* the peak bytes the last run held as the allocator counted them for the limit */
pub(crate) fn memory_peak() -> u64 {
    with_slot(|s| s.memory_peak as u64)
}

use crate::vm::snapshot;


/* preempts every `n` loop back-edges and 0 disables it */
pub(crate) fn set_preempt_interval(n: u32) {
    with_slot(|s| s.preempt_every = n as usize);
}

/* the parked run's blob length in out or -1 for none and -2 on failure */
pub(crate) fn save_state() -> i64 {
    let blob = with_slot(|s| {
        // a directory entry names no script and keeps its directory
        let entry = if s.source_name.is_empty() { &s.entry_dir } else { &s.source_name };
        let vm = s.paused_run.as_mut().and_then(|p| p.vm.as_deref_mut())?;
        let source = vm.chunk.source.clone();
        Some(snapshot::save_swept(vm, &source, entry))
    });
    match blob {
        Some(Ok(b)) => write_out_bytes(b) as i64,
        Some(Err(e)) => { write_out(&e); -2 }
        None => -1,
    }
}

/* boots from the blob's embedded source and overlays its saved state */
pub(crate) fn restore_state(blob: &[u8]) -> u32 {
    // a refused blob leaves the host's run so the reset waits for every check
    let source = match snapshot::source_of(blob) {
        // a blob another runtime saved is refused before its source parses
        Ok(_) if !snapshot::saved_under(blob, web::entry().runtime()) => return err_status(snapshot::OTHER_RUNTIME),
        Ok(s) => s.to_string(),
        Err(e) => return err_status(&e),
    };
    // a restore runs under the lower of its blob's limits and the host's
    let host = limits();
    let limits = match snapshot::limits_of(blob) {
        Ok(l) => Limits { memory: l.memory.min(host.memory).min(MEMORY_CEILING), ops: l.ops.min(host.ops) },
        Err(e) => return err_status(&e),
    };
    let chunk = match entry_chunk(&source) {
        Ok(c) => c,
        Err(_) => return err_status("snapshot source no longer parses; was it saved by another compiler version?"),
    };
    reset_run();
    let mut vm = boot_vm(chunk, limits);
    if let Err(e) = snapshot::restore(&mut vm, blob) {
        park_repl_or_drop(vm);
        return err_status(&e);
    }
    step_vm(vm, &source, None)
}

/* parked or REPL module bindings as JSON */
pub(crate) fn state_globals() -> u32 {
    let json = with_slot(|s| {
        s.paused_run
            .as_mut()
            .and_then(|p| p.vm.as_deref_mut())
            .or(s.repl_vm.as_deref_mut())
            .map(snapshot::inspect_globals)
    });
    write_out(json.as_deref().unwrap_or("{}")) as u32
}

/* parked coroutines as JSON or `[]` when idle */
pub(crate) fn state_stack() -> u32 {
    let json = with_slot(|s| {
        s.paused_run.as_ref().and_then(|p| p.vm.as_deref()).map(snapshot::inspect_stack)
    });
    write_out(json.as_deref().unwrap_or("[]")) as u32
}
