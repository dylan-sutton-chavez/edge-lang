use super::resolver::{self, Side};
use super::{Calls, ChannelSink, Completion, Engine, ErrorKind, Fault, Hosted, Hosting, Project, RunLimits, Serve, Sink, Status, Waits, Wiring};
use anyhow::{anyhow, bail, Result};
use edge::abi::{error_from_kind, from_wire, to_wire};
use edge::code::{constant_fold, Diagnostic, SSAChunk};
use edge::deps::walk::{self, Carry};
use edge::deps::{dir_of, entry_dir, DepHost, Deps, HostResolver, ModuleEntry, NativeBinding};
use edge::embed::{Limits, SchedulerStatus, VM};
use edge::lang::{Front, Runtime};
use edge::value::meter::Paying;
use edge::value::{HeapPool, Val, VmErr};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

// half the address space at most since garbage may take as much again before collecting
const MEMORY_CEILING: usize = usize::MAX / 2;

std::thread_local! {
    // the host side of the interpreter stepping on this thread which its hooks and bindings reach
    static NOW: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

/* what the hooks and bindings of a run reach while it steps */
struct Shared {
    hosting: Hosting,
    // the host module calls by the id the runtime registered each under
    natives: Vec<Hosted>,
    host: Host,
}

/* the modules the walk registered and the dir the entry's code resolves from */
#[derive(Clone)]
struct Host {
    deps: Arc<Mutex<Deps>>,
    dir: Arc<Mutex<String>>,
}

impl Host {
    fn dir(&self) -> String {
        self.dir.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl DepHost for Host {
    fn deps<R>(&mut self, f: impl FnOnce(&mut Deps) -> R) -> R {
        f(&mut self.deps.lock().unwrap_or_else(PoisonError::into_inner))
    }

    // the walk already handed over every file so nothing more answers here
    fn fetch(&mut self, spec: &str, _pin: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
        Err(format!("no bytes cached by host for '{spec}'"))
    }

    fn native(&mut self, name: String, id: u32, _module: Arc<str>) -> NativeBinding {
        NativeBinding { name, func: Arc::new(move |heap: &mut HeapPool, args: &[Val], kwargs: Option<Val>| call(id as usize, heap, args, kwargs)) }
    }

    // the entry chunk has no path so its code resolves from the entry dir
    fn caller_dir(&mut self) -> String {
        match edge::embed::calling().and_then(|now| now.file).filter(|file| !file.is_empty()) {
            Some(file) => dir_of(&file),
            None => self.dir(),
        }
    }
}

/* the abi error a fault raises in the program with the class its runtime names */
fn raised(fault: &Fault, hosting: &Hosting) -> VmErr {
    error_from_kind(fault.kind as u32, fault.message(hosting.descriptor))
}

/* one host module call with its answer now or a park until a completion brings it */
fn call(id: usize, heap: &mut HeapPool, args: &[Val], kwargs: Option<Val>) -> Result<Val, VmErr> {
    NOW.with_borrow_mut(|now| {
        let Some(shared) = now.as_mut() else { return Err(VmErr::Runtime("a host module call ran outside its interpreter")) };
        let Some(hosted) = shared.natives.get(id).cloned() else { return Err(error_from_kind(ErrorKind::Runtime as u32, format!("native id {id} not registered"))) };
        if kwargs.is_some() {
            return Err(error_from_kind(ErrorKind::Type as u32, format!("{} takes positional arguments only", hosted.name)));
        }
        let args = args.iter().map(|&v| to_wire(heap, v)).collect::<Option<Vec<_>>>();
        let Some(args) = args else { return Err(error_from_kind(ErrorKind::Runtime as u32, "value is not a transit value".to_string())) };
        let id = edge::embed::calling().map_or(0, |now| now.id) as u32;
        match shared.hosting.call(&hosted, id, args) {
            Ok(Some(value)) => from_wire(heap, &value),
            Ok(None) => Err(VmErr::HostCallDeferred),
            Err(fault) => Err(raised(&fault, &shared.hosting)),
        }
    })
}

fn print(text: &str) {
    NOW.with_borrow_mut(|now| {
        if let Some(shared) = now.as_mut() {
            (shared.hosting.print)(text);
        }
    });
}

/* whether the host took a send where a refusal raises its fault in the program */
fn sent(group: &str, body: &str) -> bool {
    NOW.with_borrow_mut(|now| {
        let Some(Shared { hosting, host, .. }) = now.as_mut() else { return false };
        // only a file channel of a run with packages asks which of them sends
        let dir = hosting.asks_sender(group).then(|| {
            let dir = host.caller_dir();
            edge::deps::served::package_of(host, &dir).unwrap_or_default()
        });
        hosting.deliver(group.to_string(), body.to_string(), dir).unwrap_or_else(|fault| {
            edge::embed::refuse(raised(&fault, hosting));
            false
        })
    })
}

/* one interpreter of a runtime built into this binary with no wasm between */
pub struct Native {
    rt: &'static Runtime,
    wiring: Arc<Wiring>,
    project: Project,
    host: Host,
    // back here between steps and with the thread while one runs
    shared: Option<Shared>,
    carry: Carry,
    limits: Limits,
    preempt: usize,
    input: Option<String>,
    // the script tracebacks name or none for a directory or no entry
    name: Option<String>,
    // the entry dir and source already registered so a run of them skips the walk
    prepared: Option<(String, String)>,
    // the source the interpreter reports against and whether it keeps inputs as a REPL does
    source: String,
    repl: bool,
    paused: bool,
    // declared before the chunks it borrows so it drops first
    vm: Option<Box<VM<'static>>>,
    chunks: Vec<Rc<SSAChunk>>,
    waits: Waits,
}

impl Native {
    /* an interpreter of `rt` serving what `wiring` holds under the limits a run's flags give */
    pub fn new(rt: &'static Runtime, wiring: Arc<Wiring>, print: Sink, project: Project) -> Result<Native> {
        let host = Host { deps: Arc::new(Mutex::new(Deps::new())), dir: Arc::new(Mutex::new(String::new())) };
        let mut hosting = Hosting::new(print, wiring.descriptor, wiring.limits.memory);
        // the one interpreter of this run takes every completion its calls report
        let (events, waits) = Waits::new();
        hosting.events = Some(events);
        let limits = RunLimits::get().over(wiring.limits);
        let shared = Some(Shared { hosting, natives: Vec::new(), host: host.clone() });
        let mut native = Native { rt, wiring, project, host, shared, carry: Carry::default(), limits, preempt: 0, input: None, name: None, prepared: None, source: String::new(), repl: false, paused: false, vm: None, chunks: Vec::new(), waits };
        native.set_limits(&limits)?;
        Ok(native)
    }

    fn shared(&mut self) -> &mut Shared {
        self.shared.as_mut().expect("the host side is back after every step")
    }

    fn front(&self) -> &'static Front {
        self.rt.front.as_ref().unwrap_or(&Front::NONE)
    }

    // names the entry the walk resolves from then registers its modules once and keeps stdin
    fn prepare(&mut self, src: &str, input: Option<&str>) -> Result<Option<Status>> {
        let entry = self.project.entry.clone();
        self.name_entry(&entry)?;
        let key = (entry, src.to_string());
        if self.prepared.as_ref() != Some(&key) {
            if let Err(e) = resolver::prefetch(self, src) {
                return Ok(Some(Status::Error(e)));
            }
            self.prepared = Some(key);
        }
        if let Some(text) = input {
            self.input = Some(text.to_string());
        }
        Ok(None)
    }

    /* the program `src` makes with its imports from the entry dir or its rendered errors */
    fn compile(&self, src: &str) -> Result<Rc<SSAChunk>, String> {
        let resolver = Box::new(HostResolver::new(self.host.dir(), self.host.clone()));
        let mut chunk = self.front().program(src, resolver).map_err(|errs| Diagnostic::render_all(&errs, src, self.name.as_deref()))?;
        if let Some(chunk) = Rc::get_mut(&mut chunk) {
            constant_fold(chunk, self.front().policy);
        }
        Ok(chunk)
    }

    /* `chunk` kept until the interpreters that borrow it are gone */
    fn pin(&mut self, chunk: Rc<SSAChunk>) -> &'static SSAChunk {
        // SAFETY the Rc stays in `chunks` which `clear` empties only after dropping the vm
        let pinned = unsafe { &*Rc::as_ptr(&chunk) };
        self.chunks.push(chunk);
        pinned
    }

    fn boot(&mut self, chunk: Rc<SSAChunk>, limits: Limits) -> Box<VM<'static>> {
        let chunk = self.pin(chunk);
        let mut vm = VM::boxed(self.rt, chunk, limits);
        vm.print_hook = Some(print);
        vm.send_hook = Some(sent);
        vm.set_preempt_interval(self.preempt);
        vm
    }

    /* hands stdin to the boot about to run one line per `input()` call */
    fn take_input(&mut self, vm: &mut VM<'static>) {
        if let Some(text) = self.input.take().filter(|text| !text.is_empty()) {
            let _paying = Paying::to(vm.heap().meter());
            vm.set_input(text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_string()).collect());
        }
    }

    /* drops the interpreter then the chunks it borrowed */
    fn clear(&mut self) {
        self.vm = None;
        self.chunks.clear();
        (self.repl, self.paused) = (false, false);
    }

    /* a finished interpreter stays for the next REPL input or else drops with its chunks */
    fn settle(&mut self) {
        self.paused = false;
        match (self.repl, self.vm.as_mut()) {
            (true, Some(vm)) => vm.clear_error_state(),
            _ => self.clear(),
        }
    }

    /* runs `vm` to its next stop with its hooks reaching this run's host side */
    fn step(&mut self, mut vm: Box<VM<'static>>) -> Result<Status> {
        NOW.set(self.shared.take());
        let ran = vm.run();
        self.shared = NOW.take();
        self.vm = Some(vm);
        let status = match ran {
            Ok(_) => {
                self.settle();
                Status::Done
            }
            Err(VmErr::HostYield(wait)) => {
                self.paused = true;
                match wait {
                    SchedulerStatus::PendingEvent => Status::PendingEvent,
                    SchedulerStatus::PendingHostCall => Status::PendingHostCall,
                    SchedulerStatus::Preempted => Status::Preempted,
                }
            }
            Err(e) => {
                let vm = self.vm.as_ref().expect("the stepped vm");
                // an uncaught exit with an integer code ends cleanly rather than crashing
                let status = match vm.system_exit_code() {
                    Some(code) => Status::Exit(code as u8),
                    None => Status::Error(vm.report_error(&e, &self.source, self.name.as_deref())),
                };
                // a failed input keeps its partial effects
                self.settle();
                status
            }
        };
        let calls = std::mem::take(&mut self.shared().hosting.deferred);
        self.waits.parked.extend(calls);
        Ok(status)
    }
}

/* hands a completion to the run `vm` parked on it */
fn deliver(vm: Option<&mut VM<'static>>, hosting: &Hosting, completion: Completion) -> Result<usize> {
    let Some(vm) = vm else { bail!("host call delivery found no paused run") };
    let _paying = Paying::to(vm.heap().meter());
    let (id, delivered) = match completion {
        Completion::Value { id, value } => {
            let value = from_wire(vm.heap_mut(), &value).map_err(|e| anyhow!("{}", vm.error_text(&e)))?;
            (id, vm.inject_host_result_by_id(id as u64, value))
        }
        Completion::Error { id, fault } => (id, vm.inject_host_error_by_id(id as u64, raised(&fault, hosting))),
    };
    if !delivered {
        bail!("host call {id} has no run waiting on it");
    }
    Ok(0)
}

impl Engine for Native {
    fn start(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        if let Some(status) = self.prepare(src, input)? {
            return Ok(status);
        }
        self.clear();
        let chunk = match self.compile(src) {
            Ok(chunk) => chunk,
            Err(rendered) => return Ok(Status::Error(rendered)),
        };
        self.source = src.to_string();
        let mut vm = self.boot(chunk, self.limits);
        self.take_input(&mut vm);
        self.step(vm)
    }

    fn repl_eval(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        if let Some(status) = self.prepare(src, input)? {
            return Ok(status);
        }
        let chunk = match self.compile(src) {
            Ok(chunk) => chunk,
            Err(rendered) => return Ok(Status::Error(rendered)),
        };
        // a run parked past its input is dropped as the next input starts
        let kept = self.vm.take().filter(|_| self.repl && !self.paused);
        (self.repl, self.paused) = (true, false);
        let mut vm = match kept {
            Some(mut vm) => {
                vm.adopt_entry_chunk(self.pin(chunk));
                vm.reset_budget(self.limits.ops);
                vm
            }
            None => self.boot(chunk, self.limits),
        };
        self.source = src.to_string();
        // named native imports live only in the chunk's externs so mirror them for later inputs
        if let Err(e) = vm.bind_chunk_externs() {
            let traceback = vm.report_error(&e, src, self.name.as_deref());
            self.vm = Some(vm);
            self.settle();
            return Ok(Status::Error(traceback));
        }
        self.take_input(&mut vm);
        self.step(vm)
    }

    fn resume(&mut self) -> Result<Status> {
        match self.vm.take().filter(|_| self.paused) {
            Some(vm) => self.step(vm),
            None => Ok(Status::Error(format!("{}: run_resume called with no paused run", self.wiring.descriptor.runtime_error))),
        }
    }

    fn set_entry(&mut self, entry: &str) {
        self.project.entry = entry.to_string();
    }

    fn save_state(&mut self) -> Result<Option<Vec<u8>>> {
        // a directory entry names no script and keeps its directory
        let entry = self.name.clone().unwrap_or_else(|| self.host.dir());
        let Some(vm) = self.vm.as_deref_mut().filter(|_| self.paused) else { return Ok(None) };
        edge::embed::save_swept(vm, &self.source, &entry).map(Some).map_err(|e| anyhow!("{e}"))
    }

    fn restore_state(&mut self, blob: &[u8]) -> Result<Status> {
        // a blob another runtime saved is refused before its source is walked
        if !edge::embed::saved_under(blob, self.rt) {
            bail!("{}", edge::embed::OTHER_RUNTIME);
        }
        let source = super::snapshot_source(blob)?;
        if let Some(status) = self.prepare(&source, None)? {
            return Ok(status);
        }
        // a restore runs under the lower of its blob's limits and the host's and a refused blob leaves the run
        let limits = match edge::embed::limits_of(blob) {
            Ok(l) => Limits { memory: l.memory.min(self.limits.memory).min(MEMORY_CEILING), ops: l.ops.min(self.limits.ops) },
            Err(e) => return Ok(Status::Error(e)),
        };
        let Ok(chunk) = self.compile(&source) else { return Ok(Status::Error("snapshot source no longer parses; was it saved by another compiler version?".to_string())) };
        self.clear();
        self.source = source;
        let mut vm = self.boot(chunk, limits);
        if let Err(e) = edge::embed::restore(&mut vm, blob) {
            self.clear();
            return Ok(Status::Error(e));
        }
        self.step(vm)
    }

    fn set_limits(&mut self, limits: &Limits) -> Result<()> {
        // what a run sends between steps counts against what it may hold
        self.shared().hosting.outbox_cap = limits.memory;
        let sandbox = Limits::sandbox();
        let memory = if limits.memory == 0 { sandbox.memory } else { limits.memory.min(MEMORY_CEILING) };
        self.limits = Limits { memory, ops: if limits.ops == 0 { sandbox.ops } else { limits.ops } };
        Ok(())
    }

    fn set_preempt_interval(&mut self, n: usize) -> Result<()> {
        self.preempt = n;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.reset_modules()?;
        self.prepared = None;
        let shared = self.shared();
        shared.natives.clear();
        shared.hosting.forget();
        self.waits.parked.clear();
        Ok(())
    }

    fn push_event(&mut self, message: &str) -> bool {
        let vm = self.vm.as_deref_mut().filter(|_| self.paused);
        self.waits.push(message, |message| vm.is_some_and(|vm| vm.push_event(message).is_ok()))
    }

    fn dispatch(&mut self) {
        self.waits.dispatch();
    }

    fn inflight(&self) -> usize {
        self.waits.inflight
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Result<usize> {
        let mut vm = self.vm.as_deref_mut().filter(|_| self.paused);
        let hosting = &self.shared.as_ref().expect("the host side is back after every step").hosting;
        self.waits.wait(timeout, |completion| deliver(vm.as_deref_mut(), hosting, completion))
    }

    fn drain_buffered(&mut self) -> usize {
        let mut vm = self.vm.as_deref_mut().filter(|_| self.paused);
        self.waits.drain(|message| vm.as_deref_mut().is_some_and(|vm| vm.push_event(message).is_ok()))
    }

    // nothing traps in process so an interpreter stays fit to run
    fn poisoned(&self) -> bool {
        false
    }

    fn send_to(&mut self, channel: ChannelSink) {
        self.shared().hosting.channel = Some(channel);
    }

    fn channel_error(&mut self) -> Option<String> {
        self.shared().hosting.channel_error.take()
    }
}

impl Serve for Native {
    fn project(&self) -> &Project {
        &self.project
    }

    fn serve(&mut self, spec: &str, module: &str, names: &[&str], calls: Box<dyn Calls>) -> Result<(), String> {
        let shared = self.shared();
        let (base, names, hosted) = shared.hosting.open(spec, module, names, calls, shared.natives.len());
        shared.natives.extend(hosted);
        let names = names.into_iter().enumerate().map(|(at, name)| (name, (base + at) as u32)).collect();
        self.host.deps(|deps| deps.register(spec.to_string(), ModuleEntry::Native(names)));
        Ok(())
    }
}

impl Side for Native {
    fn wiring(&self) -> Arc<Wiring> {
        self.wiring.clone()
    }

    fn hosting(&mut self) -> &mut Hosting {
        &mut self.shared().hosting
    }

    fn plugin(&mut self, _spec: &str, _bytes: &[u8]) -> Result<(), String> {
        Err(String::from("is a wasm plugin and a runtime run natively loads none"))
    }

    fn name_entry(&mut self, entry: &str) -> Result<()> {
        *self.host.dir.lock().unwrap_or_else(PoisonError::into_inner) = entry_dir(entry);
        // a directory or no entry names no script so its frame keeps the anonymous marker
        self.name = (!entry.is_empty() && !entry.ends_with('/')).then(|| entry.to_string());
        Ok(())
    }

    fn reset_modules(&mut self) -> Result<()> {
        self.host.deps(Deps::reset);
        self.clear();
        Ok(())
    }

    fn register_error(&mut self, spec: &str, msg: &str) -> Result<(), String> {
        self.host.deps(|deps| deps.refuse(spec.to_string(), msg.to_string()));
        Ok(())
    }

    fn walk_carry(&mut self, carried: &str) -> Result<(), String> {
        self.carry = walk::carry(carried.as_bytes());
        Ok(())
    }

    fn walk_start(&mut self, src: &str, system: &str) -> Result<Vec<u8>, String> {
        let dir = self.host.dir();
        Ok(walk::start(self.front(), &mut self.host, src, &dir, system.as_bytes(), self.carry.clone()).into_bytes())
    }

    fn walk_fetched(&mut self, bytes: &[u8], kind: i32) -> Result<Vec<u8>, String> {
        Ok(walk::fetched(&mut self.host, bytes, kind as u32).into_bytes())
    }

    fn walk_plugin_bytes(&mut self) -> Result<Vec<u8>, String> {
        Ok(walk::plugin_bytes(&mut self.host))
    }

    fn walk_plugin(&mut self, kind: i32, why: &str) -> Result<Vec<u8>, String> {
        Ok(walk::loaded(&mut self.host, kind as u32, why.into()).into_bytes())
    }

    fn walk_served(&mut self, failures: &str) -> Result<Vec<u8>, String> {
        Ok(walk::served(&mut self.host, failures.as_bytes()).into_bytes())
    }

    fn walk_known(&mut self, names: &str) -> Result<Vec<u8>, String> {
        Ok(walk::known(&mut self.host, names.as_bytes()).into_bytes())
    }
}
