use crate::lexer::lex;
use crate::modules::entry_dir;
use crate::parser::{Parser, Diagnostic, SSAChunk};
use crate::vm::{VM, Limits};
use crate::vm::types::{HeapObj, SchedulerStatus, VmErr};
use alloc::{boxed::Box, rc::Rc, string::{String, ToString}};
use core::ptr::NonNull;
use crate::s;

use super::{ModuleEntry, PausedRun, CHUNK_CACHE, now_ns_host, send_host, stream_print, with_runtime, with_slot, write_out, write_out_bytes};
use super::resolver::WasmHostResolver;
use crate::bridge::{self, BridgeState, VmGuard, safe_bytes, safe_str_owned};

/* Packed `u32` from `run_start` / `run_resume`, top 3 bits = kind, low 29 = out-buffer length. */
const STATUS_KIND_SHIFT: u32 = 29;
const STATUS_PAYLOAD_MASK: u32 = (1 << STATUS_KIND_SHIFT) - 1;
const STATUS_DONE: u32 = 0 << STATUS_KIND_SHIFT;
const STATUS_PENDING_TIMER: u32 = 1 << STATUS_KIND_SHIFT;
// Kind 2 was the render frame yield, retired so the other kinds keep their numbers.
const STATUS_PENDING_EVENT: u32 = 3 << STATUS_KIND_SHIFT;
const STATUS_ERROR: u32 = 4 << STATUS_KIND_SHIFT;
const STATUS_PENDING_HOST_CALL: u32 = 5 << STATUS_KIND_SHIFT;
// Uncaught `SystemExit`, clean termination, low 8 bits carry the POSIX exit code (not a buffer length).
const STATUS_EXIT: u32 = 6 << STATUS_KIND_SHIFT;
// Preempt tick, resumes with no host action.
const STATUS_PREEMPTED: u32 = 7 << STATUS_KIND_SHIFT;

/* The text lands in the out buffer, `out_len` reports its full length. */
fn err_status(msg: &str) -> u32 {
    let n = write_out(msg);
    STATUS_ERROR | (n as u32).min(STATUS_PAYLOAD_MASK)
}

/* Lex and parse with the host resolver, Err is rendered diagnostics. */
fn parse_source(src: &str) -> Result<SSAChunk, String> {
    let (tokens, lex_errs) = lex(src);
    let dir = with_slot(|s| s.entry_dir.clone());
    let resolver = Box::new(WasmHostResolver { dir });
    let mut p = Parser::with_resolver(src, tokens.into_iter(), resolver);
    for e in lex_errs {
        p.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() });
    }
    let (mut chunk, errs) = p.parse();
    if !errs.is_empty() { return Err(Diagnostic::render_all(&errs, src, source_name().as_deref())); }
    crate::vm::optimizer::constant_fold(&mut chunk);
    Ok(chunk)
}

/* The parsed program for `src`, shared by every slot booting the same source from the same dir. */
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
    let chunk = Rc::new(parse_source(src)?);
    with_runtime(|rt| {
        if rt.chunk_cache.len() >= CHUNK_CACHE {
            rt.chunk_cache.remove(0);
        }
        rt.chunk_cache.push((dir, chunk.clone()));
    });
    Ok(chunk)
}

/* The caps a fresh boot runs under, the host's `set_limits` or the sandbox profile. */
fn limits() -> Limits {
    with_slot(|s| s.limits).unwrap_or_else(Limits::sandbox)
}

/* The entry frame name for tracebacks, None until the host names the source. */
fn source_name() -> Option<String> {
    with_slot(|s| (!s.source_name.is_empty()).then(|| s.source_name.clone()))
}

/* Boots on `chunk`, the slot holds it until every VM borrowing it is gone. */
fn boot_vm(chunk: Rc<SSAChunk>, limits: Limits) -> VM<'static> {
    // SAFETY the slot's Rc outlives the VM, `Slot::clear_run` drops VMs before chunks.
    let chunk_static: &'static SSAChunk = unsafe { &*Rc::as_ptr(&chunk) };
    let (preempt, wall_clock) = with_slot(|s| {
        s.chunks.push(chunk);
        (s.preempt_every, s.wall_clock)
    });
    let mut vm = VM::with_limits(chunk_static, limits);
    vm.print_hook = Some(stream_print);
    vm.send_hook = Some(send_host);
    if wall_clock {
        vm.set_time_hook(now_ns_host);
    }
    vm.set_preempt_interval(preempt);
    vm
}

/* Drain host-supplied stdin bytes, invalid UTF-8 degrades to empty. */
fn take_input(vm: &mut VM) {
    let inp = with_slot(|s| core::mem::take(&mut s.input));
    let inp_text = core::str::from_utf8(&inp).unwrap_or("");
    if !inp_text.is_empty() {
        // One line per `input()` call, any trailing CR dropped.
        vm.input_buffer = inp_text.split('\n').map(|l| String::from(l.strip_suffix('\r').unwrap_or(l))).collect();
    }
}

/* A fresh boot hard-resets the selected slot, the bridge no longer points into it. */
fn reset_run() {
    with_slot(|s| s.clear_run());
    bridge::set_current_vm(None);
}

/* Copies a host `(ptr, len)` source, the host frees its buffer after the call. */
fn source_arg(ptr: *const u8, len: u32) -> Result<String, u32> {
    core::str::from_utf8(unsafe { safe_bytes(ptr, len) })
        .map(|s| s.to_string())
        .map_err(|e| err_status(&s!("input rejected: invalid utf-8 at byte ", int e.valid_up_to())))
}

/* Host-fed stdin for the next boot. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_input(ptr: *const u8, len: u32) {
    let bytes = unsafe { safe_bytes(ptr, len) }.to_vec();
    with_slot(|s| s.input = bytes);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn out_ptr() -> *const u8 {
    with_runtime(|rt| rt.out.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn out_len() -> u32 {
    with_runtime(|rt| rt.out.len() as u32)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_alloc(size: u32) -> *mut u8 {
    let v = alloc::vec![0u8; size as usize];
    Box::into_raw(v.into_boxed_slice()) as *mut u8
}

/* Frees a `wasm_alloc` buffer, the host passes the exact requested `size`, null or zero is a no-op. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_free(ptr: *mut u8, size: u32) {
    if ptr.is_null() || size == 0 { return; }
    unsafe {
        let slice = core::slice::from_raw_parts_mut(ptr, size as usize);
        let _ = Box::from_raw(slice as *mut [u8]);
    }
}

/* A fresh interpreter slot, the id `vm_select` takes. */
#[unsafe(no_mangle)]
pub extern "C" fn vm_create() -> u32 {
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

/* Points every per-run export at slot `id`, 0 ok, 1 when no such slot exists. */
#[unsafe(no_mangle)]
pub extern "C" fn vm_select(id: u32) -> i32 {
    with_runtime(|rt| select(rt, id as usize))
}

fn select(rt: &mut super::WasmRuntime, id: usize) -> i32 {
    rt.slot();
    if !rt.slots.get(id).is_some_and(Option::is_some) { return 1; }
    if id == rt.current { return 0; }
    // The bridge holds the selected slot's handles, park them and load the next slot's.
    let active = bridge::with_bridge(|b| core::mem::replace(b, BridgeState::new()));
    rt.slot().bridge = active;
    rt.current = id;
    let incoming = core::mem::replace(&mut rt.slot().bridge, BridgeState::new());
    bridge::with_bridge(|b| *b = incoming);
    0
}

/* Frees slot `id` and its runs, slot 0 stays, returns 1 when there is nothing to drop. */
#[unsafe(no_mangle)]
pub extern "C" fn vm_drop(id: u32) -> i32 {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn register_native_module(spec_ptr: *const u8, spec_len: u32, names_ptr: *const u8, names_len: u32, base_id: u32) {
    use alloc::vec::Vec;
    let spec = unsafe { safe_str_owned(spec_ptr, spec_len) };
    let names_str = core::str::from_utf8(unsafe { safe_bytes(names_ptr, names_len) }).unwrap_or("");
    let funcs: Vec<(String, u32)> = names_str.split('\n')
        .filter(|n| !n.is_empty())
        .enumerate()
        .map(|(i, name)| (name.to_string(), base_id + i as u32))
        .collect();
    with_runtime(|rt| rt.register(spec, ModuleEntry::Native(funcs)));
}

/* A spec or bare name the host cannot load, importing it fails at the import with `msg`. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn register_module_error(spec_ptr: *const u8, spec_len: u32, msg_ptr: *const u8, msg_len: u32) {
    let spec = unsafe { safe_str_owned(spec_ptr, spec_len) };
    let msg = unsafe { safe_str_owned(msg_ptr, msg_len) };
    with_runtime(|rt| rt.refuse(spec, msg));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn reset_modules() {
    with_runtime(|rt| {
        rt.registry.clear();
        rt.manifests.clear();
        rt.refusals.clear();
        rt.walk = None;
        rt.registry_changed();
        // Paused run references the now-stale module table, drop it for a clean reset.
        rt.slot().clear_run();
    });
    // The bridge current_vm may have pointed into the dropped paused VM, reset clears it too.
    bridge::reset();
}

/* The script the next run starts from, naming its traceback frame, its directory rooting the walk and the relative imports. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_entry(ptr: *const u8, len: u32) {
    let entry = unsafe { safe_str_owned(ptr, len) };
    with_slot(|s| {
        s.entry_dir = entry_dir(&entry);
        // A directory or no entry at all names no script, so its frame keeps the anonymous marker.
        s.source_name = if entry.ends_with('/') { String::new() } else { entry };
    });
}

/* Caps for the next `run_start` or `repl_eval`, memory in bytes, zero keeps the sandbox value. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_limits(memory: u64, ops: u64) {
    let sandbox = Limits::sandbox();
    let pick = |v: u64, fallback: usize| if v == 0 { fallback } else { usize::try_from(v).unwrap_or(usize::MAX) };
    // Half the address space at most, since garbage may take as much again before a collection.
    let limits = Limits { memory: pick(memory, sandbox.memory).min(usize::MAX / 2), ops: pick(ops, sandbox.ops) };
    with_slot(|s| s.limits = Some(limits));
}

/* Drive one segment of execution, on `Pending*` re-stash the VM into the recycled `PausedRun` box. */
fn step_vm(mut vm: VM<'static>, src: &str, prev_paused: Option<Box<PausedRun>>) -> u32 {
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
            let (kind, deadline) = match status {
                SchedulerStatus::PendingTimer(d) => (STATUS_PENDING_TIMER, d),
                SchedulerStatus::PendingEvent => (STATUS_PENDING_EVENT, 0),
                SchedulerStatus::PendingHostCall => (STATUS_PENDING_HOST_CALL, 0),
                SchedulerStatus::Preempted => (STATUS_PREEMPTED, 0),
            };
            let mut paused = match prev_paused {
                Some(mut b) => {
                    b.vm = Some(vm);
                    b.last_yield_deadline_ns = deadline;
                    b
                }
                None => Box::new(PausedRun {
                    vm: Some(vm),
                    last_yield_deadline_ns: deadline,
                }),
            };
            // Re-publish `current_vm` to the boxed VM so embedder calls between yields still allocate, its address is stable.
            let vm_ptr = paused.vm.as_mut().map(|v| NonNull::from(v).cast::<VM<'static>>());
            with_slot(|s| s.paused_run = Some(paused));
            bridge::set_current_vm(vm_ptr);
            kind
        }
        Err(e) => {
            // An uncaught `SystemExit` with an integer code is clean termination, not a crash.
            if let Some(code) = vm.system_exit_code() {
                park_repl_or_drop(vm);
                drop(prev_paused);
                return STATUS_EXIT | ((code as u32) & 0xFF);
            }
            let name = source_name();
            let mut traceback = vm.render_chain(src, name.as_deref());
            traceback.push_str(&e.render_traceback(
                src, vm.error_pos(), name.as_deref(),
                vm.call_stack_frames(), vm.function_names_ref(),
            ));
            // A failed input keeps its partial effects.
            park_repl_or_drop(vm);
            drop(prev_paused);
            err_status(&traceback)
        }
    }
}

/* The REPL keeps a finished VM for the next input, one-shot runs free it with its chunks. */
fn park_repl_or_drop(mut vm: VM<'static>) {
    let repl_mode = with_slot(|s| s.repl_mode);
    if repl_mode {
        vm.clear_error_state();
        bridge::set_current_vm(None);
        with_slot(|s| s.repl_vm = Some(Box::new(vm)));
    } else {
        drop(vm);
        with_slot(|s| if s.paused_run.is_none() { s.chunks.clear(); });
    }
}

/* REPL entry, first call boots the interpreter and later inputs adopt a new entry chunk on it. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn repl_eval(ptr: *const u8, len: u32) -> u32 {
    let src = match source_arg(ptr, len) {
        Ok(s) => s,
        Err(status) => return status,
    };
    let chunk = match parse_source(&src) {
        Ok(c) => Rc::new(c),
        Err(rendered) => return err_status(&rendered),
    };
    let existing = with_slot(|s| {
        s.repl_mode = true;
        s.paused_run = None;
        s.repl_vm.take()
    });
    let mut vm = match existing {
        Some(boxed) => {
            // SAFETY the REPL slot keeps every input's chunk while its interpreter lives.
            let chunk_static: &'static SSAChunk = unsafe { &*Rc::as_ptr(&chunk) };
            with_slot(|s| s.chunks.push(chunk));
            let mut vm = *boxed;
            vm.adopt_entry_chunk(chunk_static);
            vm.reset_budget(limits().ops);
            vm
        }
        None => boot_vm(chunk, limits()),
    };
    // Named native imports live only in the chunk's extern table, mirror them so later inputs resolve them.
    if let Err(e) = vm.bind_chunk_externs() {
        let name = source_name();
        let traceback = e.render_traceback(&src, vm.error_pos(), name.as_deref(), vm.call_stack_frames(), vm.function_names_ref());
        park_repl_or_drop(vm);
        return err_status(&traceback);
    }
    take_input(&mut vm);
    step_vm(vm, &src, None)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn run_start(ptr: *const u8, len: u32) -> u32 {
    // A fresh `run_start` hard-resets execution state.
    reset_run();
    let src = match source_arg(ptr, len) {
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn run_resume() -> u32 {
    let paused = match with_slot(|s| s.paused_run.take()) {
        Some(p) => p,
        None => return err_status("RuntimeError: run_resume called with no paused run"),
    };
    // Take VM out so `step_vm` owns it, recycle the empty Box for the next stash.
    let mut paused_box = paused;
    let vm = paused_box.vm.take().expect("paused_run with no VM is a runtime bug");
    // An error past a suspension still renders against the entry source.
    let src = vm.chunk.source.clone();
    step_vm(vm, &src, Some(paused_box))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn run_push_event(ptr: *const u8, len: u32) -> i32 {
    let bytes = unsafe { safe_bytes(ptr, len) };
    let s = match core::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return 1,
    };
    with_slot(|slot| {
        let Some(paused) = slot.paused_run.as_mut() else { return 1; };
        let Some(vm) = paused.vm.as_mut() else { return 1; };
        let val = match vm.heap.alloc(HeapObj::Str(s)) {
            Ok(v) => v,
            Err(_) => return 2,
        };
        vm.inject_event(val);
        0
    })
}

/* Shared `set_host_*` prologue, 1 means a stale handle and 3 means no paused run. */
fn with_paused_vm(handle: u32, f: impl FnOnce(&mut VM<'static>, crate::vm::types::Val) -> i32) -> i32 {
    let Some(val) = bridge::get_val(handle) else { return 1; };
    bridge::release_handles(&[handle]);
    with_slot(|slot| {
        let Some(paused) = slot.paused_run.as_mut() else { return 3; };
        let Some(vm) = paused.vm.as_mut() else { return 3; };
        f(vm, val)
    })
}

/* Wakes coro `id` with `handle`, 0 ok, 1 stale handle, 2 no waiter, 3 no paused run. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_host_result_by_id(id: u32, handle: u32) -> i32 {
    bridge::answered(id);
    with_paused_vm(handle, |vm, val| if vm.inject_host_result_by_id(id as u64, val) { 0 } else { 2 })
}

/* Raises an error into the `WaitingHostCall(id)` coro so its try/except can catch it, `msg_handle` is a str. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_host_error_by_id(id: u32, kind: u32, msg_handle: u32) -> i32 {
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

/* The most the last run held at once, as the allocator counted it for the limit, in bytes. */
#[unsafe(no_mangle)]
pub extern "C" fn memory_peak() -> u64 {
    with_slot(|s| s.memory_peak as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn last_yield_deadline_ns() -> u64 {
    with_slot(|s| s.paused_run.as_ref().map(|p| p.last_yield_deadline_ns).unwrap_or(0))
}

use crate::vm::snapshot;

/* Preempt every `n` loop back-edges, 0 disables. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_preempt_interval(n: u32) {
    with_slot(|s| s.preempt_every = n as usize);
}

/* Whether the next boot sleeps on the host's clock, 0 keeps the virtual clock for a run nobody granted time. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn set_wall_clock(on: u32) {
    with_slot(|s| s.wall_clock = on != 0);
}

/* Serialize the parked run into the out buffer, its length, -1 when none. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn save_state() -> i64 {
    let blob = with_slot(|s| {
        let vm = s.paused_run.as_ref().and_then(|p| p.vm.as_ref())?;
        let source = vm.chunk.source.clone();
        Some(snapshot::save(vm, &source))
    });
    match blob {
        Some(b) => write_out_bytes(b) as i64,
        None => -1,
    }
}

/* Boot from the blob's embedded source, overlay its saved state. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn restore_state(ptr: *const u8, len: u32) -> u32 {
    reset_run();
    let blob = unsafe { safe_bytes(ptr, len) };
    let source = match snapshot::source_of(blob) {
        Ok(s) => s.to_string(),
        Err(e) => return err_status(&e),
    };
    let limits = match snapshot::limits_of(blob) {
        Ok(l) => l,
        Err(e) => return err_status(&e),
    };
    let chunk = match entry_chunk(&source) {
        Ok(c) => c,
        Err(_) => return err_status("snapshot source no longer parses; was it saved by another compiler version?"),
    };
    let mut vm = boot_vm(chunk, limits);
    if let Err(e) = snapshot::restore(&mut vm, blob) {
        park_repl_or_drop(vm);
        return err_status(&e);
    }
    step_vm(vm, &source, None)
}

/* Parked or REPL module bindings as JSON. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn state_globals() -> u32 {
    let json = with_slot(|s| {
        s.paused_run
            .as_ref()
            .and_then(|p| p.vm.as_ref())
            .or(s.repl_vm.as_deref())
            .map(snapshot::inspect_globals)
    });
    write_out(json.as_deref().unwrap_or("{}")) as u32
}

/* Parked coroutines as JSON, `[]` when idle. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn state_stack() -> u32 {
    let json = with_slot(|s| {
        s.paused_run.as_ref().and_then(|p| p.vm.as_ref()).map(snapshot::inspect_stack)
    });
    write_out(json.as_deref().unwrap_or("[]")) as u32
}
