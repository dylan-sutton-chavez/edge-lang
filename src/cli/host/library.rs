use super::resolver::{self, Side};
use super::{engine::{saved_by, status_of}, pooled::Pooled, snapshot_source, Calls, ChannelSink, Completion, Engine, ErrorKind, Fault, Fetched, Hosted, Hosting, Project, Remote, Serve, Sink, Status, Waits, Walk, Wiring};
use crate::cli::kit::Descriptor;
use anyhow::{anyhow, bail, Result};
use crate::abi::{WireValue, TAG_INVALID};
use crate::embed::Limits;
use std::cell::{RefCell, RefMut};
use crate::r#unsafe::library::{self as ffi, Cx, Fns, HostTable};
use std::ffi::c_void;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

// the version of the native contract this host speaks which a library reports before any context
const NATIVE_ABI: u32 = 1;

/* a native runtime library loaded once per process with the names it gives itself */
pub struct Library {
    // kept open for as long as any context lives so its code stays mapped
    _lib: libloading::Library,
    f: Fns,
    pub descriptor: &'static Descriptor,
    // where it was loaded from so a bundle carries the same library
    pub path: std::path::PathBuf,
    // the descriptor json whose snapshot identity a restore checks a blob against
    described: Vec<u8>,
}

impl Library {
    /* opens the library at `path` and reads its descriptor through a throwaway context */
    pub fn open(path: &Path) -> Result<Arc<Library>> {
        let (lib, version) = ffi::open(path)?;
        if version != NATIVE_ABI {
            bail!("{} speaks native contract {version} and this host speaks {NATIVE_ABI}", path.display());
        }
        let f = Fns::bind(&lib)?;
        let mut quiet = Quiet;
        let table = HostTable { user: (&mut quiet as *mut Quiet).cast(), print: quiet_print, call_native: quiet_call, fetch_bytes: no_fetch, send: quiet_send };
        let cx = f.context_new(&table);
        if cx.is_null() {
            bail!("the native runtime made no context");
        }
        f.runtime_descriptor(cx);
        let json = f.out(cx);
        f.context_free(cx);
        let descriptor = super::wasm::descriptor(&json)?;
        Ok(Arc::new(Library { _lib: lib, f, descriptor, path: path.to_path_buf(), described: json }))
    }
}

// a descriptor read reaches no host so its table answers nothing
struct Quiet;
extern "C" fn quiet_print(_: *mut c_void, _: *const u8, _: usize) {}
extern "C" fn quiet_call(_: *mut c_void, _: u32, _: u32, _: *const u32, _: u32, _: *mut u32) -> i32 { 1 }
extern "C" fn quiet_send(_: *mut c_void, _: *const u8, _: u32, _: *const u8, _: u32) -> i32 { 1 }

// the walk already gave the runtime every manifest so nothing more answers here
extern "C" fn no_fetch(_: *mut c_void, _: *const u8, _: u32, _: *const u8, out_len: *mut u32) -> *mut u8 {
    ffi::put(out_len, 0);
    std::ptr::null_mut()
}

/* the host side of one context which its callbacks reach through the user pointer */
struct State {
    f: Fns,
    cx: Cx,
    hosting: Hosting,
    natives: Vec<Hosted>,
}

impl State {
    fn decode(&self, handle: u32) -> Result<WireValue, String> {
        let mut tag = 0u32;
        let mut buf = vec![0u8; 256];
        let mut n = self.f.host_interpreter_decode(self.cx, handle, &mut tag, buf.as_mut_ptr(), buf.len() as u32);
        if n < 0 {
            buf.resize((-n) as usize, 0);
            n = self.f.host_interpreter_decode(self.cx, handle, &mut tag, buf.as_mut_ptr(), buf.len() as u32);
        }
        if tag == TAG_INVALID {
            return Err("value is not a transit value".to_string());
        }
        buf.truncate(n.max(0) as usize);
        WireValue::decode_body(tag, &buf).ok_or_else(|| "malformed wire value".to_string())
    }

    fn encode(&self, value: &WireValue) -> Result<u32, String> {
        let mut body = Vec::new();
        value.encode_body(&mut body);
        match self.f.host_interpreter_encode(self.cx, value.tag(), body.as_ptr(), body.len() as u32) {
            0 => Err("encode failed".to_string()),
            h => Ok(h),
        }
    }

    /* stashes the error the runtime raises once the call returns 1 */
    fn throw(&self, fault: &Fault) {
        let message = fault.message(self.hosting.descriptor);
        self.f.host_interpreter_throw(self.cx, fault.kind as u32, message.as_ptr(), message.len() as u32);
    }
}

// the engine calls back only during an export this host made on the context the user pointer names
fn state<'a>(user: *mut c_void) -> &'a mut State {
    ffi::user(user)
}

fn text(ptr: *const u8, len: usize) -> String {
    ffi::text(ptr, len)
}

extern "C" fn host_print(user: *mut c_void, ptr: *const u8, len: usize) {
    let state = state(user);
    (state.hosting.print)(&text(ptr, len));
}

extern "C" fn host_send(user: *mut c_void, group_ptr: *const u8, group_len: u32, body_ptr: *const u8, body_len: u32) -> i32 {
    let state = state(user);
    let group = text(group_ptr, group_len as usize);
    let body = text(body_ptr, body_len as usize);
    // only a file channel of a run with packages asks which of them sends
    let dir = if state.hosting.asks_sender(&group) {
        let len = state.f.send_package(state.cx);
        Some(String::from_utf8_lossy(&state.f.out(state.cx)[..len as usize]).into_owned())
    } else {
        None
    };
    match state.hosting.deliver(group, body, dir) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(fault) => {
            state.throw(&fault);
            2
        }
    }
}

/* dispatches one extern call with decoded values to the host module it names */
extern "C" fn host_call(user: *mut c_void, id: u32, call_id: u32, argv: *const u32, argc: u32, out_handle: *mut u32) -> i32 {
    let state = state(user);
    let Some(hosted) = state.natives.get(id as usize).cloned() else {
        state.throw(&Fault::runtime(format!("native id {id} not registered")));
        return 1;
    };
    let handles = ffi::handles(argv, argc);
    // the trailing slot holds the kwargs a host module call never takes
    let Some((&0, positional)) = handles.split_last() else {
        state.throw(&Fault::new(ErrorKind::Type, format!("{} takes positional arguments only", hosted.name)));
        return 1;
    };
    let mut args = Vec::with_capacity(positional.len());
    for &handle in positional {
        match state.decode(handle) {
            Ok(value) => args.push(value),
            Err(e) => {
                state.throw(&Fault::runtime(e));
                return 1;
            }
        }
    }
    let answer = state.hosting.call(&hosted, call_id, args);
    match answer.and_then(|value| value.map(|v| state.encode(&v).map_err(Fault::runtime)).transpose()) {
        Ok(Some(handle)) => {
            ffi::put(out_handle, handle);
            0
        }
        Ok(None) => 2,
        Err(fault) => {
            state.throw(&fault);
            1
        }
    }
}

/* one context of a native runtime with the slots its interpreters run in */
pub struct Instance {
    lib: Arc<Library>,
    wiring: Arc<Wiring>,
    // boxed so the pointer the engine calls back with stays put
    state: Box<State>,
    pub project: Project,
    selected: u32,
    prepared: Option<(String, String)>,
    // a panic leaves the context's state unknown so nothing runs in it again
    poisoned: bool,
    slots: usize,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.lib.f.context_free(self.state.cx);
    }
}

impl Instance {
    /* a fresh context of `lib` printing through `sink` over the files of `project` */
    pub fn fresh(lib: Arc<Library>, wiring: Arc<Wiring>, sink: Sink, project: Project) -> Result<Instance> {
        let hosting = Hosting::new(sink, wiring.descriptor, wiring.limits.memory);
        let mut state = Box::new(State { f: lib.f, cx: std::ptr::null(), hosting, natives: Vec::new() });
        let table = HostTable { user: (&mut *state as *mut State).cast(), print: host_print, call_native: host_call, fetch_bytes: no_fetch, send: host_send };
        let cx = lib.f.context_new(&table);
        if cx.is_null() {
            bail!("the native runtime made no context");
        }
        state.cx = cx;
        let mut inst = Instance { lib, wiring, state, project, selected: 0, prepared: None, poisoned: false, slots: 0 };
        inst.lib.f.reset_modules(cx);
        inst.checked()?;
        Ok(inst)
    }

    fn cx(&self) -> Cx {
        self.state.cx
    }

    pub fn slots(&self) -> usize {
        self.slots
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    /* lets `send()` reach the scheduler since only pool instances drain the outbox */
    pub fn accept_sends(&mut self) {
        self.state.hosting.outbox.get_or_insert_with(Vec::new);
    }

    fn f(&self) -> Fns {
        self.lib.f
    }

    /* the panic a call just raised ends the context and reads as its error */
    fn checked(&mut self) -> Result<()> {
        let n = self.f().context_panic(self.cx(), std::ptr::null_mut(), 0);
        if n <= 0 {
            return Ok(());
        }
        let mut buf = vec![0u8; n as usize];
        self.f().context_panic(self.cx(), buf.as_mut_ptr(), n as u32);
        self.poisoned = true;
        bail!("{}", String::from_utf8_lossy(&buf))
    }

    fn read_out(&self) -> Vec<u8> {
        self.lib.f.out(self.cx())
    }

    fn select(&mut self, slot: u32, events: &Sender<Completion>) -> Result<()> {
        if self.poisoned {
            bail!("the native runtime panicked and runs nothing again");
        }
        if self.selected != slot {
            let code = self.f().vm_select(self.cx(), slot);
            self.checked()?;
            if code != 0 {
                bail!("interpreter slot {slot} is gone");
            }
            self.selected = slot;
            self.state.hosting.events = None;
        }
        if self.state.hosting.events.is_none() {
            self.state.hosting.events = Some(events.clone());
        }
        Ok(())
    }

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
            self.f().set_input(self.cx(), text.as_ptr(), text.len() as u32);
            self.checked()?;
        }
        Ok(None)
    }

    fn status(&mut self, raw: u32) -> Result<Status> {
        if let Err(e) = self.checked() {
            return Ok(Status::Error(format!("error: {e}")));
        }
        status_of(raw, || self.read_out())
    }

    fn walked(&mut self) -> Result<Vec<u8>, String> {
        self.checked().map_err(|e| e.to_string())?;
        Ok(self.read_out())
    }
}

impl Serve for Instance {
    fn project(&self) -> &Project {
        &self.project
    }

    fn serve(&mut self, spec: &str, module: &str, names: &[&str], calls: Box<dyn Calls>) -> Result<(), String> {
        let state = &mut *self.state;
        let (base, names, hosted) = state.hosting.open(spec, module, names, calls, state.natives.len());
        state.natives.extend(hosted);
        let joined = names.join("\n");
        self.lib.f.register_native_module(self.state.cx, spec.as_ptr(), spec.len() as u32, joined.as_ptr(), joined.len() as u32, base as u32);
        self.checked().map_err(|e| e.to_string())
    }
}

impl Side for Instance {
    fn wiring(&self) -> Arc<Wiring> {
        self.wiring.clone()
    }

    fn hosting(&mut self) -> &mut Hosting {
        &mut self.state.hosting
    }

    fn plugin(&mut self, _spec: &str, _bytes: &[u8]) -> Result<(), String> {
        Err("is a .wasm plugin, which a native runtime does not load, run the project on a wasm runtime".to_string())
    }

    fn name_entry(&mut self, entry: &str) -> Result<()> {
        self.f().set_entry(self.cx(), entry.as_ptr(), entry.len() as u32);
        self.checked()
    }

    fn reset_modules(&mut self) -> Result<()> {
        self.f().reset_modules(self.cx());
        self.checked()
    }

    fn register_error(&mut self, spec: &str, msg: &str) -> Result<(), String> {
        self.f().register_module_error(self.cx(), spec.as_ptr(), spec.len() as u32, msg.as_ptr(), msg.len() as u32);
        self.checked().map_err(|e| e.to_string())
    }

    fn walk_carry(&mut self, carried: &str) -> Result<(), String> {
        self.f().walk_carry(self.cx(), carried.as_ptr(), carried.len() as u32);
        self.checked().map_err(|e| e.to_string())
    }

    fn walk_start(&mut self, src: &str, system: &str) -> Result<Vec<u8>, String> {
        self.f().walk_start(self.cx(), src.as_ptr(), src.len() as u32, system.as_ptr(), system.len() as u32);
        self.walked()
    }

    fn walk_fetched(&mut self, bytes: &[u8], kind: i32) -> Result<Vec<u8>, String> {
        self.f().walk_fetched(self.cx(), bytes.as_ptr(), bytes.len() as u32, kind as u32);
        self.walked()
    }

    fn walk_plugin_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.f().walk_plugin_bytes(self.cx());
        self.walked()
    }

    fn walk_plugin(&mut self, kind: i32, why: &str) -> Result<Vec<u8>, String> {
        self.f().walk_plugin(self.cx(), kind as u32, why.as_ptr(), why.len() as u32);
        self.walked()
    }

    fn walk_served(&mut self, failures: &str) -> Result<Vec<u8>, String> {
        self.f().walk_served(self.cx(), failures.as_ptr(), failures.len() as u32);
        self.walked()
    }
}

/* one interpreter slot of a native context with the completions its deferred calls report */
pub struct Vm {
    inst: Rc<RefCell<Instance>>,
    slot: u32,
    events: Sender<Completion>,
    waits: Waits,
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.slot == 0 {
            return;
        }
        if let Ok(mut inst) = self.inst.try_borrow_mut() {
            inst.slots -= 1;
            if !inst.poisoned {
                inst.f().vm_drop(inst.cx(), self.slot);
                if inst.selected == self.slot {
                    inst.selected = 0;
                    inst.state.hosting.events = None;
                }
            }
        }
    }
}

impl Vm {
    /* one interpreter on a context of its own as every one-shot run uses */
    pub fn new(lib: Arc<Library>, wiring: Arc<Wiring>, sink: Sink, project: Project) -> Result<Vm> {
        let limits = crate::cli::host::RunLimits::get().over(wiring.limits);
        let inst = Rc::new(RefCell::new(Instance::fresh(lib, wiring, sink, project)?));
        let mut vm = Vm::on(inst, 0);
        vm.set_limits(&limits)?;
        Ok(vm)
    }

    /* opens another interpreter slot in `inst` that frees when the `Vm` drops */
    pub fn slot(inst: &Rc<RefCell<Instance>>) -> Result<Vm> {
        let id = {
            let mut i = inst.borrow_mut();
            let id = i.f().vm_create(i.cx());
            i.checked()?;
            i.slots += 1;
            id
        };
        Ok(Vm::on(inst.clone(), id))
    }

    fn on(inst: Rc<RefCell<Instance>>, slot: u32) -> Vm {
        let (events, waits) = Waits::new();
        Vm { inst, slot, events, waits }
    }

    fn stepped(&mut self, status: Result<Status>) -> Result<Status> {
        let calls = std::mem::take(&mut self.inst.borrow_mut().state.hosting.deferred);
        self.waits.parked.extend(calls);
        status
    }

    fn enter(&self) -> Result<RefMut<'_, Instance>> {
        enter(&self.inst, self.slot, &self.events)
    }

    pub fn instance(&self) -> &Rc<RefCell<Instance>> {
        &self.inst
    }

    /* prepares `src` then makes the run `call` makes keeping what the step parked on */
    fn run(&mut self, src: &str, input: Option<&str>, call: impl FnOnce(&Instance) -> u32) -> Result<Status> {
        let status = {
            let mut inst = self.enter()?;
            if let Some(status) = inst.prepare(src, input)? {
                return Ok(status);
            }
            let raw = call(&inst);
            inst.status(raw)
        };
        self.stepped(status)
    }
}

impl Pooled for Vm {
    fn take_sends(&mut self) -> Vec<(String, String)> {
        self.inst.borrow_mut().state.hosting.take_sends()
    }

    fn unsent(&self) -> Option<String> {
        self.inst.borrow().state.hosting.unsent.clone()
    }

    fn poll(&mut self) -> Result<usize> {
        self.waits.poll(|completion| deliver(&self.inst, self.slot, &self.events, completion))
    }

    fn walk(&mut self, src: &str) -> Result<Walk, String> {
        let mut inst = self.enter().map_err(|e| e.to_string())?;
        let entry = inst.project.entry.clone();
        inst.name_entry(&entry).map_err(|e| e.to_string())?;
        Walk::start(&mut *inst, src)
    }

    fn walk_on(&mut self, walk: &mut Walk, fetched: Option<Fetched>) -> Result<Option<Remote>, String> {
        let mut inst = self.enter().map_err(|e| e.to_string())?;
        if let Some(fetched) = fetched {
            walk.answer(&mut *inst, fetched)?;
        }
        let next = walk.advance(&mut *inst)?;
        if next.is_none() {
            inst.prepared = Some(walk.key.clone());
        }
        Ok(next)
    }
}

impl Engine for Vm {
    fn start(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        self.run(src, input, |inst| inst.f().run_start(inst.cx(), src.as_ptr(), src.len() as u32))
    }

    fn repl_eval(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        self.run(src, input, |inst| inst.f().repl_eval(inst.cx(), src.as_ptr(), src.len() as u32))
    }

    fn resume(&mut self) -> Result<Status> {
        let status = {
            let mut inst = self.enter()?;
            let raw = inst.f().run_resume(inst.cx());
            inst.status(raw)
        };
        self.stepped(status)
    }

    fn set_entry(&mut self, entry: &str) {
        self.inst.borrow_mut().project.entry = entry.to_string();
    }

    fn push_event(&mut self, message: &str) -> bool {
        self.waits.push(message, |message| inject(&self.inst, self.slot, &self.events, message))
    }

    fn drain_buffered(&mut self) -> usize {
        self.waits.drain(|message| inject(&self.inst, self.slot, &self.events, message))
    }

    fn save_state(&mut self) -> Result<Option<Vec<u8>>> {
        let mut inst = self.enter()?;
        let n = inst.f().save_state(inst.cx());
        inst.checked()?;
        match n {
            -2 => bail!("{}", String::from_utf8_lossy(&inst.read_out())),
            n if n < 0 => Ok(None),
            _ => Ok(Some(inst.read_out())),
        }
    }

    fn restore_state(&mut self, blob: &[u8]) -> Result<Status> {
        // a blob another runtime saved is refused before its source is walked as on wasm
        if !saved_by(&self.enter()?.lib.described, blob) { bail!("{}", crate::embed::OTHER_RUNTIME); }
        let source = snapshot_source(blob)?;
        self.run(&source, None, |inst| inst.f().restore_state(inst.cx(), blob.as_ptr(), blob.len() as u32))
    }

    fn set_preempt_interval(&mut self, n: usize) -> Result<()> {
        let mut inst = self.enter()?;
        inst.f().set_preempt_interval(inst.cx(), n as u32);
        inst.checked()
    }

    fn set_limits(&mut self, limits: &Limits) -> Result<()> {
        let mut inst = self.enter()?;
        inst.state.hosting.outbox_cap = limits.memory;
        inst.f().set_limits(inst.cx(), limits.memory as u64, limits.ops);
        inst.checked()
    }

    fn reset(&mut self) -> Result<()> {
        let mut inst = self.enter()?;
        inst.f().reset_modules(inst.cx());
        inst.checked()?;
        inst.prepared = None;
        inst.state.natives.clear();
        inst.state.hosting.forget();
        drop(inst);
        self.waits.parked.clear();
        Ok(())
    }

    fn dispatch(&mut self) {
        self.waits.dispatch();
    }

    fn inflight(&self) -> usize {
        self.waits.inflight
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Result<usize> {
        self.waits.wait(timeout, |completion| deliver(&self.inst, self.slot, &self.events, completion))
    }

    fn poisoned(&self) -> bool {
        self.inst.borrow().poisoned
    }

    fn send_to(&mut self, channel: ChannelSink) {
        self.inst.borrow_mut().state.hosting.channel = Some(channel);
    }

    fn channel_error(&mut self) -> Option<String> {
        self.inst.borrow_mut().state.hosting.channel_error.take()
    }
}

fn enter<'a>(inst: &'a Rc<RefCell<Instance>>, slot: u32, events: &Sender<Completion>) -> Result<RefMut<'a, Instance>> {
    let mut inst = inst.borrow_mut();
    inst.select(slot, events)?;
    Ok(inst)
}

fn inject(inst: &Rc<RefCell<Instance>>, slot: u32, events: &Sender<Completion>, message: &str) -> bool {
    let Ok(mut inst) = enter(inst, slot, events) else { return false };
    let code = inst.f().run_push_event(inst.cx(), message.as_ptr(), message.len() as u32);
    inst.checked().is_ok() && code == 0
}

/* hands a completion to the run of `slot` */
fn deliver(inst: &Rc<RefCell<Instance>>, slot: u32, events: &Sender<Completion>, completion: Completion) -> Result<usize> {
    let mut inst = enter(inst, slot, events)?;
    let code = match completion {
        Completion::Value { id, value } => {
            let handle = inst.state.encode(&value).map_err(|e| anyhow!(e))?;
            inst.f().set_host_result_by_id(inst.cx(), id, handle)
        }
        Completion::Error { id, fault } => {
            let text = fault.message(inst.state.hosting.descriptor);
            let handle = inst.state.encode(&WireValue::Bytes(text.into_bytes())).map_err(|e| anyhow!(e))?;
            inst.f().set_host_error_by_id(inst.cx(), id, fault.kind as u32, handle)
        }
    };
    inst.checked()?;
    if code != 0 {
        bail!("host call delivery returned {code}");
    }
    Ok(std::mem::take(&mut inst.state.hosting.deferred).len())
}
