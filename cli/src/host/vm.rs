use super::{read, resolver, rt, stage, unstage, wt, Calls, Exports, Fault, Fetched, Host, MemoryCap, Native, Opened, Project, Remote, Sink, State, Walk};
use anyhow::{anyhow, bail, Result};
use edge::abi::WireValue;
use edge::embed::Limits;
use crate::kit::Descriptor;
use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wasmtime::{ResourceLimiter, Store};

const KIND_SHIFT: u32 = 29;

/* what a run step left the interpreter waiting on as packed by the runtime exports */
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
    Error { id: u32, fault: Fault },
}

/* one runtime instance with its store and the slots its interpreters run in */
pub struct Instance {
    pub(super) host: Rc<Host>,
    pub(super) store: Store<State>,
    pub(super) ex: Exports,
    pub project: Project,
    // the slot the per-run exports act on right now
    selected: u32,
    // the entry dir and source already registered so slots booting them skip the walk
    prepared: Option<(String, String)>,
    // a trap leaves the instance memory untrustworthy so nothing runs in it again
    poisoned: bool,
    slots: usize,
    // the module being opened with what its package holds which gates every call it serves
    pub(super) opening: Option<(Arc<dyn super::HostModule>, super::Held)>,
}

impl Host {
    /* a runtime instance whose `deadline` in epoch ticks and `memory` in bytes bound plugins too */
    pub fn instance(self: &Rc<Self>, sink: Sink, project: Project, deadline: Option<u64>, memory: usize) -> Result<Rc<RefCell<Instance>>> {
        let state = State {
            exports: None,
            print: sink,
            natives: Vec::new(),
            registered: HashMap::new(),
            deferred: Vec::new(),
            running: Vec::new(),
            waiting: HashMap::new(),
            outbox: None,
            outbox_bytes: 0,
            outbox_cap: self.loaded.limits.memory,
            channel: None,
            channel_error: None,
            senders: None,
            unsent: None,
            limiter: MemoryCap { max: memory },
            slot: 0,
            events: None,
            opened: Vec::new(),
            descriptor: self.loaded.descriptor,
        };
        let mut store = Store::new(&self.loaded.engine, state);
        store.limiter(|s: &mut State| &mut s.limiter as &mut dyn ResourceLimiter);
        store.set_epoch_deadline(deadline.unwrap_or(1 << 40));
        store.epoch_deadline_trap();
        let instance = wt(self.linked.instantiate(&mut store))?;
        let ex = Exports::bind(&mut store, &instance)?;
        store.data_mut().exports = Some(ex.clone());
        wt(ex.reset_modules.call(&mut store, ()))?;
        Ok(Rc::new(RefCell::new(Instance { host: self.clone(), store, ex, project, selected: 0, prepared: None, poisoned: false, slots: 0, opening: None })))
    }

    /* one interpreter on an instance of its own as every one-shot run uses */
    pub fn vm(self: &Rc<Self>, sink: Sink, project: Project, deadline: Option<u64>, memory: Option<usize>) -> Result<Vm> {
        let limits = super::RunLimits::get().over(self.loaded.limits);
        // these limits bound an uncapped run and apply before a group sets its own
        let memory = memory.unwrap_or_else(|| super::linear_cap(limits.memory));
        let mut vm = Vm::on(self.instance(sink, project, deadline, memory)?, 0);
        vm.set_limits(&limits)?;
        Ok(vm)
    }
}

impl Instance {
    /* opens another interpreter slot in `inst` that frees when the `Vm` drops */
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

    /* lets `send()` reach the scheduler since only pool instances drain the outbox */
    pub fn accept_sends(&mut self) {
        self.store.data_mut().outbox.get_or_insert_with(Vec::new);
    }

    /* hands every line a run sends to `channel` the moment the program sends it */
    pub fn send_to(&mut self, channel: super::ChannelSink) {
        self.store.data_mut().channel = Some(channel);
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    /* any failed call is a trap and a trapped instance never runs again */
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

    /* stages `bytes` for one pointer and length call and frees them once it returns */
    fn with_bytes<R>(&mut self, bytes: &[u8], call: impl FnOnce(&mut Store<State>, &Exports, i32, i32) -> wasmtime::Result<R>) -> Result<wasmtime::Result<R>> {
        let staged = stage(&mut self.store, &self.ex, bytes);
        let ptr = self.checked(staged)?;
        let ex = self.ex.clone();
        let result = call(&mut self.store, &ex, ptr, bytes.len() as i32);
        unstage(&mut self.store, &self.ex, ptr, bytes.len());
        Ok(result)
    }

    /* what the last call left in the out buffer of the runtime */
    fn read_out(&mut self) -> Vec<u8> {
        let ptr = self.ex.out_ptr.call(&mut self.store, ()).unwrap_or(0);
        let len = self.ex.out_len.call(&mut self.store, ()).unwrap_or(0);
        read(&mut self.store, self.ex.memory, ptr, len)
    }

    /* whether the descriptor names this runtime as the one that saved the blob */
    fn saved_here(&mut self, blob: &[u8]) -> bool {
        let Some(describe) = self.ex.runtime_descriptor.clone() else { return true };
        let Ok(len) = describe.call(&mut self.store, ()) else { return true };
        let ptr = self.ex.out_ptr.call(&mut self.store, ()).unwrap_or(0);
        let text = read(&mut self.store, self.ex.memory, ptr, len);
        let Ok(d) = serde_json::from_slice::<serde_json::Value>(&text) else { return true };
        let (Some(at), Some(id)) = (d["snapshot"]["runtime_at"].as_u64(), d["snapshot"]["runtime"].as_str()) else { return true };
        let at = at as usize;
        blob.get(at..at + 8).and_then(|w| w.try_into().ok()).map(u64::from_le_bytes).is_some_and(|w| format!("{w:016x}") == id)
    }

    // names the entry the walk resolves from then registers its modules once and feeds stdin
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
            let raw = self.with_bytes(text.as_bytes(), |store, ex, ptr, len| ex.set_input.call(store, (ptr, len)))?;
            self.checked(raw)?;
        }
        Ok(None)
    }

    /* the script tracebacks name and the directory its relative imports join onto */
    pub(super) fn name_entry(&mut self, entry: &str) -> Result<()> {
        let raw = self.with_bytes(entry.as_bytes(), |store, ex, ptr, len| ex.set_entry.call(store, (ptr, len)))?;
        self.checked(raw)
    }

    /* drops every module and refusal the runtime holds */
    pub(super) fn reset_modules(&mut self) -> Result<()> {
        let raw = self.ex.reset_modules.call(&mut self.store, ());
        self.checked(raw)
    }

    fn status(&mut self, raw: wasmtime::Result<i32>) -> Result<Status> {
        let status = match raw {
            Ok(raw) => raw as u32,
            Err(e) => {
                self.poisoned = true;
                return Ok(Status::Error(trap_text(&e, self.host.descriptor())));
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

    pub(super) fn register_error(&mut self, spec: &str, msg: &str) -> Result<(), String> {
        self.register_pair(spec, msg.as_bytes(), |store, ex, (s, sl), (p, pl)| ex.register_module_error.call(store, (s, sl, p, pl)))
    }

    pub(super) fn register_native(&mut self, spec: &str, names: &[String], base: usize) -> Result<(), String> {
        let joined = names.join("\n");
        self.register_pair(spec, joined.as_bytes(), |store, ex, (s, sl), (p, pl)| ex.register_native_module.call(store, (s, sl, p, pl, base as i32)))
    }

    /* serves the calls `module` opened under `spec` with `names` in the order their index answers */
    pub fn serve(&mut self, spec: &str, module: &str, names: &[&str], calls: Box<dyn Calls>) -> Result<(), String> {
        // a spec this instance already serves keeps the calls its package opened first
        if let Some((base, names)) = self.store.data().registered.get(spec).cloned() {
            return self.register_native(spec, &names, base);
        }
        // the kit checks each call against the grant so no module can skip it
        let calls = match self.opening.clone() {
            Some((module, held)) => Box::new(super::access::Gate { module, held, names: names.iter().map(|name| name.to_string()).collect(), calls }),
            None => calls,
        };
        let state = self.store.data_mut();
        let calls: Opened = Arc::new(Mutex::new(calls));
        state.opened.push(calls.clone());
        let base = state.natives.len();
        for (index, name) in names.iter().enumerate() {
            state.natives.push(Native::Host { calls: calls.clone(), index, name: format!("{module}.{name}").into() });
        }
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        state.registered.insert(spec.to_string(), (base, names.clone()));
        self.register_native(spec, &names, base)
    }

    /* names the field the next walks carry with the holders it reserves after it */
    pub(super) fn walk_carry(&mut self, carried: &str) -> Result<(), String> {
        let Some(carry) = self.ex.walk_carry.clone() else { return Ok(()) };
        let ptr = stage(&mut self.store, &self.ex, carried.as_bytes()).map_err(|e| e.to_string())?;
        let result = carry.call(&mut self.store, (ptr, carried.len() as i32));
        unstage(&mut self.store, &self.ex, ptr, carried.len());
        self.checked(result).map_err(|e| e.to_string())
    }

    /* starts the engine resolution walk and returns its first step for this host as JSON */
    pub(super) fn walk_start(&mut self, src: &str, system: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[src.as_bytes(), system.as_bytes()], |store, ex, s| ex.walk_start.call(store, (s[0].0, s[0].1, s[1].0, s[1].1)))
    }

    /* answers a fetch as `kind` 0 bytes or 1 missing or 2 failed with why */
    pub(super) fn walk_fetched(&mut self, bytes: &[u8], kind: i32) -> Result<Vec<u8>, String> {
        self.walk_call(&[bytes], |store, ex, s| ex.walk_fetched.call(store, (s[0].0, s[0].1, kind)))
    }

    /* the plugin bytes the last step named which this host may never have seen */
    pub(super) fn walk_plugin_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.walk_call(&[], |store, ex, _| ex.walk_plugin_bytes.call(store, ()))
    }

    /* answers a plugin as `kind` 0 registered or 1 failed or 2 refused with why */
    pub(super) fn walk_plugin(&mut self, kind: i32, why: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[why.as_bytes()], |store, ex, s| ex.walk_plugin.call(store, (kind, s[0].0, s[0].1)))
    }

    /* hands back what serving the host modules failed at joined by NUL */
    pub(super) fn walk_served(&mut self, failures: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[failures.as_bytes()], |store, ex, s| ex.walk_served.call(store, (s[0].0, s[0].1)))
    }

    /* answers the undeclared names with the ones the registry has joined by NUL */
    pub(super) fn walk_known(&mut self, names: &str) -> Result<Vec<u8>, String> {
        self.walk_call(&[names.as_bytes()], |store, ex, s| ex.walk_known.call(store, (s[0].0, s[0].1)))
    }

    // stages buffers for one walk call and frees them before reading the step it left
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

    // stages a spec and payload for one registration call freeing both once it returns
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

/* one interpreter slot of an instance with the completions its deferred calls report */
pub struct Vm {
    inst: Rc<RefCell<Instance>>,
    slot: u32,
    events: Sender<Completion>,
    rx: Receiver<Completion>,
    // calls this interpreter parked on moved out of the shared store after every step
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
                // dropping the selected slot moves the runtime back to slot 0
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

    /* keeps what the step just parked on before another slot of the instance runs */
    fn stepped(&mut self, status: Result<Status>) -> Result<Status> {
        let calls = std::mem::take(&mut self.inst.borrow_mut().store.data_mut().deferred);
        self.parked.extend(calls);
        status
    }

    /* the instance with the slot of this interpreter selected */
    fn enter(&self) -> Result<RefMut<'_, Instance>> {
        let mut inst = self.inst.borrow_mut();
        inst.select(self.slot, &self.events)?;
        Ok(inst)
    }

    pub fn instance(&self) -> &Rc<RefCell<Instance>> {
        &self.inst
    }

    /* resolves imports and starts a fresh run with resolution failures read as compile errors */
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

    /* runs one more input on the persistent interpreter without running the history again */
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

    /* runs the next input as `entry` whose directory resolves its relative imports */
    pub fn set_entry(&mut self, entry: &str) {
        self.inst.borrow_mut().project.entry = entry.to_string();
    }

    /* wakes a parked `receive()` or queues the message until the run parks on one */
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

    /* hands queued messages to a run parked on an event and returns how many arrived */
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

    /* boots from the source the blob embeds whose imports must resolve again */
    pub fn restore_state(&mut self, blob: &[u8]) -> Result<Status> {
        // a blob another runtime saved is refused before its source is walked
        if !self.enter()?.saved_here(blob) { bail!("{}", edge::embed::OTHER_RUNTIME); }
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

    /* caps the next boot with the limits of a group or the sandbox profile */
    pub fn set_limits(&mut self, limits: &Limits) -> Result<()> {
        let mut inst = self.enter()?;
        // what a run sends between steps counts against what it may hold
        inst.store.data_mut().outbox_cap = limits.memory;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            ex.set_limits.call(&mut *store, (limits.memory as i64, limits.ops as i64))
        };
        inst.checked(raw)
    }

    /* drops every module registration so the next input starts in a fresh namespace */
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
        state.close_opened();
        drop(inst);
        self.parked.clear();
        Ok(())
    }

    /* moves the parked calls into the in-flight count whose answers arrive as completions */
    pub fn dispatch(&mut self) {
        self.inflight += std::mem::take(&mut self.parked).len();
    }

    pub fn inflight(&self) -> usize {
        self.inflight
    }

    /* blocks for the next completion then injects all that arrived and returns the count */
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<usize> {
        let first = match timeout {
            Some(t) => self.rx.recv_timeout(t).ok(),
            None => self.rx.recv().ok(),
        };
        let Some(completion) = first else { return Ok(0) };
        self.deliver(completion)?;
        Ok(1 + self.poll()?)
    }

    /* injects the completions already in without blocking */
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
            Completion::Error { id, fault } => (id, Err(fault)),
        };
        self.inflight = self.inflight.saturating_sub(1);
        let mut inst = self.enter()?;
        let raw = {
            let Instance { store, ex, .. } = &mut *inst;
            match (store.data_mut().waiting.remove(&id), value) {
                // a plugin waiting on this call finishes its work before the program sees the result
                (Some(plugin), settled) => super::env::resume(&mut *store, ex, id, plugin, settled),
                (None, Ok(value)) => {
                    let handle = rt::encode(&mut *store, ex, &value).map_err(|e| anyhow!(e))?;
                    ex.set_host_result_by_id.call(&mut *store, (id as i32, handle as i32))
                }
                (None, Err(fault)) => {
                    let text = fault.message(store.data().descriptor);
                    let handle = rt::encode(&mut *store, ex, &WireValue::Bytes(text.into_bytes())).map_err(|e| anyhow!(e))?;
                    ex.set_host_error_by_id.call(&mut *store, (id as i32, fault.kind as i32, handle as i32))
                }
            }
        };
        // a resumed plugin that waits again is already in flight on the same id
        let waits = std::mem::take(&mut inst.store.data_mut().deferred).len();
        let code = inst.checked(raw)?;
        drop(inst);
        self.inflight += waits;
        if code != 0 {
            bail!("host call {id} delivery returned {code}");
        }
        Ok(())
    }

    /* everything `send()` queued since the last take */
    pub fn take_sends(&mut self) -> Vec<(String, String)> {
        let mut inst = self.inst.borrow_mut();
        let state = inst.store.data_mut();
        state.outbox_bytes = 0;
        state.outbox.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /* why a channel of the run failed to take a line since the last ask */
    pub fn channel_error(&mut self) -> Option<String> {
        self.inst.borrow_mut().store.data_mut().channel_error.take()
    }

    /* begins the import walk of `src` which `walk_on` steps where its caller chooses */
    pub fn walk(&mut self, src: &str) -> Result<Walk, String> {
        let mut inst = self.enter().map_err(|e| e.to_string())?;
        let entry = inst.project.entry.clone();
        inst.name_entry(&entry).map_err(|e| e.to_string())?;
        Walk::start(&mut inst, src)
    }

    /* steps `walk` with what a fetch gave until it waits on another file or ends */
    pub fn walk_on(&mut self, walk: &mut Walk, fetched: Option<Fetched>) -> Result<Option<Remote>, String> {
        let mut inst = self.enter().map_err(|e| e.to_string())?;
        if let Some(fetched) = fetched {
            walk.answer(&mut inst, fetched)?;
        }
        let next = walk.advance(&mut inst)?;
        // a finished walk leaves the instance prepared so the start walks nothing again
        if next.is_none() {
            inst.prepared = Some(walk.key.clone());
        }
        Ok(next)
    }

    /* the group a send of this instance went to that no scheduler took */
    pub fn unsent(&self) -> Option<String> {
        self.inst.borrow().store.data().unsent.clone()
    }
}

/* the source a snapshot blob embeds read by the engine that wrote its header */
pub fn snapshot_source(blob: &[u8]) -> Result<String> {
    edge::embed::source_of(blob).map(String::from).map_err(|e| anyhow!(e))
}

/* a trap is a run error and an epoch deadline reads as the time limit */
fn trap_text(e: &wasmtime::Error, d: &Descriptor) -> String {
    if e.downcast_ref::<wasmtime::Trap>().is_some_and(|t| *t == wasmtime::Trap::Interrupt) {
        return time_limit(d);
    }
    format!("error: {e}")
}

/* the traceback of a run past its time limit raised as the runtime's runtime error */
pub fn time_limit(d: &Descriptor) -> String {
    format!("error: {}: run exceeded its time limit", d.runtime_error)
}

#[cfg(test)]
mod restore {
    use crate::host::{Host, Project, Status};
    use std::sync::{Arc, Mutex};

    // a blob the engine refuses leaves the run the instance keeps parked
    #[test]
    fn a_refused_blob_leaves_the_parked_run() {
        let kit = crate::kit::tests::bare();
        let host = Host::new(kit.load().unwrap()).unwrap();
        let printed = Arc::new(Mutex::new(String::new()));
        let into = printed.clone();
        let mut vm = host.vm(Box::new(move |text: &str| into.lock().unwrap().push_str(text)), Project::disk("", None), None, None).unwrap();
        assert!(matches!(vm.start("m = receive()\nprint('kept', m)\n", None).unwrap(), Status::PendingEvent));
        let blob = vm.save_state().unwrap().expect("a parked run saves");
        // the same blob sealed again over a source that no longer parses
        let len = u64::from_le_bytes(blob[24..32].try_into().unwrap()) as usize;
        let mut bad = blob[..blob.len() - 32].to_vec();
        bad[40..40 + len].fill(b'(');
        let seal = edge::util::sha256::sha256(&bad);
        bad.extend_from_slice(&seal);
        assert!(matches!(vm.restore_state(&bad).unwrap(), Status::Error(e) if e.contains("no longer parses")));
        vm.push_event("x");
        assert!(matches!(vm.resume().unwrap(), Status::Done));
        assert_eq!(printed.lock().unwrap().as_str(), "kept x\n");
    }
}
