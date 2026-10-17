use super::{read, resolver, rt, stage, unstage, wt, Exports, Host, MemoryCap, Project, Sink, State};
use anyhow::{anyhow, bail, Result};
use compiler::abi::WireValue;
use compiler::vm::Limits;
use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wasmtime::{ResourceLimiter, Store};

const KIND_SHIFT: u32 = 29;

/* What a run step left the interpreter waiting on, the packed status of the compiler exports. */
pub enum Status {
    Done,
    PendingEvent,
    PendingHostCall,
    Error(String),
    Exit(u8),
    Preempted,
}

pub enum Completion {
    Value { id: u32, value: WireValue },
    Error { id: u32, msg: String },
}

/* One compiler instance, its store and the slots its interpreters run in. */
pub struct Instance {
    pub(super) host: Rc<Host>,
    pub(super) store: Store<State>,
    pub(super) ex: Exports,
    pub project: Project,
    // The slot the per-run exports act on right now.
    selected: u32,
    // Entry dir and source whose modules are registered, slots booting them skip the walk.
    prepared: Option<(String, String)>,
    // A trap leaves the instance memory untrustworthy, nothing runs in it again.
    poisoned: bool,
    slots: usize,
}

impl Host {
    /* A fresh compiler instance, `deadline` in epoch ticks and `memory` in bytes bound every memory of its store, plugins included. */
    pub fn instance(self: &Rc<Self>, sink: Sink, project: Project, deadline: Option<u64>, memory: usize) -> Result<Rc<RefCell<Instance>>> {
        let state = State {
            exports: None,
            print: Arc::new(Mutex::new(sink)),
            natives: Vec::new(),
            registered: HashMap::new(),
            deferred: Vec::new(),
            running: Vec::new(),
            waiting: HashMap::new(),
            outbox: None,
            limiter: MemoryCap { max: memory },
            slot: 0,
            events: None,
            run: super::system::run_id(),
        };
        let mut store = Store::new(&self.runtime.engine, state);
        store.limiter(|s: &mut State| &mut s.limiter as &mut dyn ResourceLimiter);
        store.set_epoch_deadline(deadline.unwrap_or(1 << 40));
        store.epoch_deadline_trap();
        let instance = wt(self.compiler.instantiate(&mut store))?;
        let ex = Exports::bind(&mut store, &instance)?;
        store.data_mut().exports = Some(ex.clone());
        wt(ex.reset_modules.call(&mut store, ()))?;
        Ok(Rc::new(RefCell::new(Instance { host: self.clone(), store, ex, project, selected: 0, prepared: None, poisoned: false, slots: 0 })))
    }

    /* One interpreter on an instance of its own, the shape of every one-shot run. */
    pub fn vm(self: &Rc<Self>, sink: Sink, project: Project, deadline: Option<u64>, memory: Option<usize>) -> Result<Vm> {
        let asked = super::RunLimits::get().engine();
        // Without a cap of its own the run grows only as far as the limit of the command line allows.
        let memory = memory.unwrap_or_else(|| super::linear_cap(asked.unwrap_or_else(Limits::sandbox).memory));
        let mut vm = Vm::on(self.instance(sink, project, deadline, memory)?, 0);
        // What the command line asked for, before a group sets its own.
        if let Some(limits) = asked {
            vm.set_limits(&limits)?;
        }
        Ok(vm)
    }
}

impl Instance {
    /* Opens another interpreter slot in `inst`, the slot frees when the Vm drops. */
    pub fn slot(inst: &Rc<RefCell<Instance>>) -> Result<Vm> {
        let id = {
            let mut i = inst.borrow_mut();
            let raw = {
                let Instance { store, ex, .. } = &mut *i;
                ex.vm_create.call(&mut *store, ())
            };
            let id = i.checked(raw)?;
            i.slots += 1;
            id as u32
        };
        Ok(Vm::on(inst.clone(), id))
    }

    pub fn slots(&self) -> usize {
        self.slots
    }

    /* Lets send() reach the scheduler, only pool instances drain the outbox. */
    pub fn accept_sends(&mut self) {
        self.store.data_mut().outbox.get_or_insert_with(Vec::new);
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    /* Any failed call is a trap, and a trapped instance never runs again. */
    fn checked<R>(&mut self, r: wasmtime::Result<R>) -> Result<R> {
        r.map_err(|e| {
            self.poisoned = true;
            anyhow!("{e}")
        })
    }

    fn select(&mut self, slot: u32, events: &Sender<Completion>) -> Result<()> {
        if self.poisoned {
            bail!("the interpreter instance trapped");
        }
        if self.selected != slot {
            let raw = self.ex.vm_select.call(&mut self.store, slot as i32);
            if self.checked(raw)? != 0 {
                bail!("interpreter slot {slot} is gone");
            }
            self.selected = slot;
            self.store.data_mut().events = None;
        }
        let state = self.store.data_mut();
        state.slot = slot;
        if state.events.is_none() {
            state.events = Some(events.clone());
        }
        Ok(())
    }

    /* Stages `bytes` for one `(ptr, len)` call and frees them once it returns. */
    fn with_bytes<R>(&mut self, bytes: &[u8], call: impl FnOnce(&mut Store<State>, &Exports, i32, i32) -> wasmtime::Result<R>) -> Result<wasmtime::Result<R>> {
        let staged = stage(&mut self.store, &self.ex, bytes);
        let ptr = self.checked(staged)?;
        let ex = self.ex.clone();
        let result = call(&mut self.store, &ex, ptr, bytes.len() as i32);
        unstage(&mut self.store, &self.ex, ptr, bytes.len());
        Ok(result)
    }

    /* The compiler's out buffer, what the last call left there. */
    fn read_out(&mut self) -> Vec<u8> {
        let ptr = self.ex.out_ptr.call(&mut self.store, ()).unwrap_or(0);
        let len = self.ex.out_len.call(&mut self.store, ()).unwrap_or(0);
        read(&mut self.store, self.ex.memory, ptr, len)
    }

    // Names the entry the walk and parse resolve from, registers its modules once, feeds stdin.
    fn prepare(&mut self, src: &str, input: Option<&str>) -> Result<Option<Status>> {
        let entry = self.project.entry.clone();
        let raw = self.with_bytes(entry.as_bytes(), |store, ex, ptr, len| ex.set_entry.call(store, (ptr, len)))?;
        self.checked(raw)?;
        let key = (entry, src.to_string());
        if self.prepared.as_ref() != Some(&key) {
            if let Err(e) = resolver::prefetch(self, src) {
                return Ok(Some(Status::Error(e)));
            }
            // The SyntaxWarnings of a newly compiled program go to stderr before it runs, as Python prints them.
            let raw = self.with_bytes(src.as_bytes(), |store, ex, ptr, len| ex.compile_warnings.call(store, (ptr, len)))?;
            if self.checked(raw)? > 0 { eprint!("{}", String::from_utf8_lossy(&self.read_out())); }
            self.prepared = Some(key);
        }
        if let Some(text) = input {
            let raw = self.with_bytes(text.as_bytes(), |store, ex, ptr, len| ex.set_input.call(store, (ptr, len)))?;
            self.checked(raw)?;
        }
        Ok(None)
    }

    fn status(&mut self, raw: wasmtime::Result<i32>) -> Result<Status> {
        let status = match raw {
            Ok(raw) => raw as u32,
            Err(e) => {
                self.poisoned = true;
                return Ok(Status::Error(trap_text(&e)));
            }
        };
        Ok(match status >> KIND_SHIFT {
            0 => Status::Done,
            3 => Status::PendingEvent,
            4 => Status::Error(String::from_utf8_lossy(&self.read_out()).into_owned()),
            5 => Status::PendingHostCall,
            6 => Status::Exit((status & 0xFF) as u8),
            7 => Status::Preempted,
            kind => bail!("unknown run status kind {kind}"),
        })
    }

    /* The run this instance reports its system calls under. */
    pub(super) fn run(&self) -> u64 {
        self.store.data().run
    }

    pub(super) fn register_error(&mut self, spec: &str, msg: &str) -> Result<(), String> {
        self.register_pair(spec, msg.as_bytes(), |store, ex, (s, sl), (p, pl)| ex.register_module_error.call(store, (s, sl, p, pl)))
    }

    pub(super) fn register_native(&mut self, spec: &str, names: &[String], base: usize) -> Result<(), String> {
        let joined = names.join("\n");
        self.register_pair(spec, joined.as_bytes(), |store, ex, (s, sl), (p, pl)| ex.register_native_module.call(store, (s, sl, p, pl, base as i32)))
    }

    /* Serves `module` to `pkg` under `spec`, opened with the scopes that package holds. */
    pub(super) fn register_system(&mut self, spec: &str, pkg: &str, module: &str, held: &[String]) -> Result<(), String> {
        if let Some((base, names)) = self.store.data().registered.get(spec).cloned() {
            return self.register_native(spec, &names, base);
        }
        let state = self.store.data_mut();
        let names = super::system::open(state.run, spec, pkg, module, held);
        let base = state.natives.len();
        for name in &names {
            state.natives.push(super::Native::System { module: module.to_string(), name: name.clone(), spec: spec.to_string() });
        }
        state.registered.insert(spec.to_string(), (base, names.clone()));
        self.register_native(spec, &names, base)
    }

    /* Starts the engine's resolution walk, returning the first step it asks this host, as JSON. */
    pub(super) fn walk_start(&mut self, src: &str, system: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[src.as_bytes(), system.as_bytes()], |store, ex, s| ex.walk_start.call(store, (s[0].0, s[0].1, s[1].0, s[1].1)))
    }

    /* Answers a fetch, `kind` 0 with the bytes, 1 missing, 2 failed with why. */
    pub(super) fn walk_fetched(&mut self, bytes: &[u8], kind: i32) -> Result<Vec<u8>, String> {
        self.walk_call(&[bytes], |store, ex, s| ex.walk_fetched.call(store, (s[0].0, s[0].1, kind)))
    }

    /* The bytes of the plugin the last step named, which may never have passed through this host. */
    pub(super) fn walk_plugin_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.walk_call(&[], |store, ex, _| ex.walk_plugin_bytes.call(store, ()))
    }

    /* Answers a plugin, `kind` 0 registered, 1 failed, 2 refused, each with why. */
    pub(super) fn walk_plugin(&mut self, kind: i32, why: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[why.as_bytes()], |store, ex, s| ex.walk_plugin.call(store, (kind, s[0].0, s[0].1)))
    }

    /* Hands back what serving the system modules failed at, the failures joined by NUL. */
    pub(super) fn walk_served(&mut self, failures: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[failures.as_bytes()], |store, ex, s| ex.walk_served.call(store, (s[0].0, s[0].1)))
    }

    /* Answers the undeclared names with the ones the registry has, joined by NUL. */
    pub(super) fn walk_known(&mut self, names: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[names.as_bytes()], |store, ex, s| ex.walk_known.call(store, (s[0].0, s[0].1)))
    }

    // Stages the buffers for one walk call, frees them after, and reads the step it left.
    fn walk_call(&mut self, bufs: &[&[u8]], call: impl FnOnce(&mut Store<State>, &Exports, &[(i32, i32)]) -> wasmtime::Result<i32>) -> Result<Vec<u8>, String> {
        let mut staged = Vec::with_capacity(bufs.len());
        for bytes in bufs {
            let ptr = stage(&mut self.store, &self.ex, bytes).map_err(|e| e.to_string())?;
            staged.push((ptr, bytes.len() as i32));
        }
        let ex = self.ex.clone();
        let result = call(&mut self.store, &ex, &staged);
        for (ptr, len) in &staged {
            unstage(&mut self.store, &self.ex, *ptr, *len as usize);
        }
        self.checked(result).map_err(|e| e.to_string())?;
        Ok(self.read_out())
    }

    // Stages a spec and a payload for one registration call, both freed once it returns.
    fn register_pair(&mut self, spec: &str, payload: &[u8], call: impl FnOnce(&mut Store<State>, &Exports, (i32, i32), (i32, i32)) -> wasmtime::Result<()>) -> Result<(), String> {
        let s = stage(&mut self.store, &self.ex, spec.as_bytes()).map_err(|e| e.to_string())?;
        let p = stage(&mut self.store, &self.ex, payload).map_err(|e| e.to_string())?;
        let ex = self.ex.clone();
        let r = call(&mut self.store, &ex, (s, spec.len() as i32), (p, payload.len() as i32));
        unstage(&mut self.store, &self.ex, s, spec.len());
        unstage(&mut self.store, &self.ex, p, payload.len());
        r.map_err(|e| e.to_string())
    }
}

/* One interpreter, a slot of an instance plus the completions its deferred calls report to. */
pub struct Vm {
    inst: Rc<RefCell<Instance>>,
    slot: u32,
    events: Sender<Completion>,
    rx: Receiver<Completion>,
    // Calls this interpreter parked on, moved out of the shared store after every step.
    parked: Vec<u32>,
    inflight: usize,
    buffered: VecDeque<String>,
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.slot == 0 {
            return;
        }
        if let Ok(mut inst) = self.inst.try_borrow_mut() {
            inst.slots -= 1;
            if !inst.poisoned {
                let Instance { store, ex, selected, .. } = &mut *inst;
                let _ = ex.vm_drop.call(&mut *store, self.slot as i32);
                // Dropping the selected slot moves the compiler back to slot 0.
                if *selected == self.slot {
                    *selected = 0;
                    store.data_mut().events = None;
                }
            }
        }
    }
}

impl Vm {
    fn on(inst: Rc<RefCell<Instance>>, slot: u32) -> Vm {
        let (events, rx) = channel();
        Vm { inst, slot, events, rx, parked: Vec::new(), inflight: 0, buffered: VecDeque::new() }
    }

    /* Keeps what the step just parked on, before another slot of the instance runs. */
    fn stepped(&mut self, status: Result<Status>) -> Result<Status> {
        let calls = std::mem::take(&mut self.inst.borrow_mut().store.data_mut().deferred);
        self.parked.extend(calls);
        status
    }

    /* The instance with this interpreter's slot selected. */
    fn enter(&self) -> Result<RefMut<'_, Instance>> {
        let mut inst = self.inst.borrow_mut();
        inst.select(self.slot, &self.events)?;
        Ok(inst)
    }

    pub fn instance(&self) -> &Rc<RefCell<Instance>> {
        &self.inst
    }

    /* Resolves imports and starts a fresh run, a resolution failure reads like a compile error. */
    pub fn start(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        let status = {
            let mut inst = self.enter()?;
            if let Some(status) = inst.prepare(src, input)? {
                return Ok(status);
            }
            let raw = inst.with_bytes(src.as_bytes(), |store, ex, ptr, len| ex.run_start.call(store, (ptr, len)))?;
            inst.status(raw)
        };
        self.stepped(status)
    }

    /* Runs one more input on the persistent interpreter, history never re-executes. */
    pub fn repl_eval(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        let status = {
            let mut inst = self.enter()?;
            if let Some(status) = inst.prepare(src, input)? {
                return Ok(status);
            }
            let raw = inst.with_bytes(src.as_bytes(), |store, ex, ptr, len| ex.repl_eval.call(store, (ptr, len)))?;
            inst.status(raw)
        };
        self.stepped(status)
    }

    pub fn resume(&mut self) -> Result<Status> {
        let status = {
            let mut inst = self.enter()?;
            let raw = {
                let Instance { store, ex, .. } = &mut *inst;
                ex.run_resume.call(&mut *store, ())
            };
            inst.status(raw)
        };
        self.stepped(status)
    }

    /* Runs the next input as `entry`, whose directory resolves its relative imports. */
    pub fn set_entry(&mut self, entry: &str) {
        self.inst.borrow_mut().project.entry = entry.to_string();
    }

    /* Wakes a parked receive(), or queues the message until the run parks on one. */
    pub fn push_event(&mut self, message: &str) -> bool {
        if self.inject(message) {
            return true;
        }
        self.buffered.push_back(message.to_string());
        false
    }

    fn inject(&mut self, message: &str) -> bool {
        let Ok(mut inst) = self.enter() else { return false };
        let Ok(raw) = inst.with_bytes(message.as_bytes(), |store, ex, ptr, len| ex.run_push_event.call(store, (ptr, len))) else { return false };
        matches!(inst.checked(raw), Ok(0))
    }

    /* Hands queued messages to a run parked on an event, the count delivered. */
    pub fn drain_buffered(&mut self) -> usize {
        let mut delivered = 0;
        while let Some(message) = self.buffered.front().cloned() {
            if !self.inject(&message) {
                break;
            }
            self.buffered.pop_front();
            delivered += 1;
        }
        delivered
    }

    pub fn save_state(&mut self) -> Result<Option<Vec<u8>>> {
        let mut inst = self.enter()?;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            ex.save_state.call(&mut *store, ())
        };
        match inst.checked(raw)? {
            -2 => bail!("{}", String::from_utf8_lossy(&inst.read_out())),
            n if n < 0 => Ok(None),
            _ => Ok(Some(inst.read_out())),
        }
    }

    /* Boots from the blob's embedded source, its imports must resolve again. */
    pub fn restore_state(&mut self, blob: &[u8]) -> Result<Status> {
        let source = snapshot_source(blob)?;
        let status = {
            let mut inst = self.enter()?;
            if let Some(status) = inst.prepare(&source, None)? {
                return Ok(status);
            }
            let raw = inst.with_bytes(blob, |store, ex, ptr, len| ex.restore_state.call(store, (ptr, len)))?;
            inst.status(raw)
        };
        self.stepped(status)
    }

    pub fn set_preempt_interval(&mut self, n: usize) -> Result<()> {
        let mut inst = self.enter()?;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            ex.set_preempt_interval.call(&mut *store, n as i32)
        };
        inst.checked(raw)
    }

    /* Caps the next boot, a group's limits or the sandbox profile. */
    pub fn set_limits(&mut self, limits: &Limits) -> Result<()> {
        let mut inst = self.enter()?;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            ex.set_limits.call(&mut *store, (limits.memory as i64, limits.ops as i64))
        };
        inst.checked(raw)
    }

    /* Drops every module registration, the next input starts in a fresh namespace. */
    pub fn reset(&mut self) -> Result<()> {
        let mut inst = self.enter()?;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            ex.reset_modules.call(&mut *store, ())
        };
        inst.checked(raw)?;
        inst.prepared = None;
        let state = inst.store.data_mut();
        state.natives.clear();
        state.registered.clear();
        state.deferred.clear();
        state.waiting.clear();
        drop(inst);
        self.parked.clear();
        Ok(())
    }

    /* Moves the parked calls into the in-flight count, each answer arrives as a completion. */
    pub fn dispatch(&mut self) {
        self.inflight += std::mem::take(&mut self.parked).len();
    }

    pub fn inflight(&self) -> usize {
        self.inflight
    }

    /* Blocks for the next completion, then injects every one that arrived, the count delivered. */
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<usize> {
        let first = match timeout {
            Some(t) => self.rx.recv_timeout(t).ok(),
            None => self.rx.recv().ok(),
        };
        let Some(completion) = first else { return Ok(0) };
        self.deliver(completion)?;
        Ok(1 + self.poll()?)
    }

    /* Injects the completions already in, never blocks. */
    pub fn poll(&mut self) -> Result<usize> {
        let mut delivered = 0;
        while let Ok(completion) = self.rx.try_recv() {
            self.deliver(completion)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    fn deliver(&mut self, completion: Completion) -> Result<()> {
        let (id, value) = match completion {
            Completion::Value { id, value } => (id, Ok(value)),
            Completion::Error { id, msg } => (id, Err(msg)),
        };
        self.inflight = self.inflight.saturating_sub(1);
        let mut inst = self.enter()?;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            match (store.data_mut().waiting.remove(&id), value) {
                // A plugin waiting on this call finishes its own work before the program sees the result.
                (Some(plugin), settled) => super::env::resume(&mut *store, ex, id, plugin, settled),
                (None, Ok(value)) => {
                    let handle = rt::encode(&mut *store, ex, &value).map_err(|e| anyhow!(e))?;
                    ex.set_host_result_by_id.call(&mut *store, (id as i32, handle as i32))
                }
                (None, Err(msg)) => {
                    let (kind, msg) = super::env::kind_of(&msg);
                    let handle = rt::encode(&mut *store, ex, &WireValue::Bytes(msg.as_bytes().to_vec())).map_err(|e| anyhow!(e))?;
                    ex.set_host_error_by_id.call(&mut *store, (id as i32, kind, handle as i32))
                }
            }
        };
        // A resumed plugin that waits again is already in flight on the same id.
        let waits = std::mem::take(&mut inst.store.data_mut().deferred).len();
        let code = inst.checked(raw)?;
        drop(inst);
        self.inflight += waits;
        if code != 0 {
            bail!("host call {id} delivery returned {code}");
        }
        Ok(())
    }

    /* Everything send() queued since the last take. */
    pub fn take_sends(&mut self) -> Vec<(String, String)> {
        self.inst.borrow_mut().store.data_mut().outbox.as_mut().map(std::mem::take).unwrap_or_default()
    }
}

/* The source a snapshot blob embeds, the header layout of vm/snapshot.rs. */
pub fn snapshot_source(blob: &[u8]) -> Result<String> {
    let bad = || anyhow!("not an edge-python snapshot");
    if blob.len() < 24 || u32::from_le_bytes(blob[0..4].try_into().map_err(|_| bad())?) != 0x4E53_5045 {
        return Err(bad());
    }
    let len = u64::from_le_bytes(blob[16..24].try_into().map_err(|_| bad())?) as usize;
    let end = 24usize.checked_add(len).filter(|e| *e <= blob.len()).ok_or_else(bad)?;
    Ok(String::from_utf8_lossy(&blob[24..end]).into_owned())
}

/* A trap is a run error, an epoch deadline reads as the time limit. */
fn trap_text(e: &wasmtime::Error) -> String {
    if e.downcast_ref::<wasmtime::Trap>().is_some_and(|t| *t == wasmtime::Trap::Interrupt) {
        return "error: RuntimeError: run exceeded its time limit".to_string();
    }
    format!("error: {e}")
}
