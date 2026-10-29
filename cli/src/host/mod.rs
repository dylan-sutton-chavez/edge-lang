mod access;
#[cfg(feature = "web")]
pub mod browser;
pub mod config;
pub mod driver;
mod env;
mod module;
mod plugins;
mod resolver;
mod rt;
pub mod scan;
mod send;
mod vm;

pub use access::{Access, Closed, Held, Open, Package, Packages, Scopes, Untrusted};
pub use edge::abi::{ErrorKind, WireValue};
pub use module::{Calls, Reply};
pub use resolver::{fetch_cached, Fetched, Project, Remote, Walk};
pub use send::{ChannelSink, Senders, FILE};
pub use vm::{time_limit, Completion, Instance, Status, Vm};

use anyhow::{anyhow, Result};
use edge::value::Limits;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use wasmtime::{AsContextMut, Engine, Instance as Wasm, InstancePre, Linker, Memory, Module, ResourceLimiter, Store, TypedFunc};

use crate::kit::{Descriptor, Runtime};

// the epoch ticker period that untrusted deadlines count in
pub const TICK_NS: u64 = 100_000_000;

// a build pulls dozens of files so one reset among them should not end it
const ATTEMPTS: usize = 3;

// how long a connection and a whole fetch may take before either fails
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// the most one download may hold since the largest module is well under a megabyte
pub const MAX_FETCH_BYTES: u64 = 64 << 20;

/* where a composition's packages come from and what its caches and variables are named */
#[derive(Clone, Debug, Default)]
pub struct Names {
    // the binary's name which its cache dirs and variables follow
    pub name: String,
    // the origin package urls start with which untrusted runs alone may fetch from
    pub origin: Option<String>,
    // the registry the package commands ask
    pub registry: Option<String>,
}

static NAMES: OnceLock<Names> = OnceLock::new();

/* the names of the composition this process runs as its kit set them first */
pub fn names() -> &'static Names {
    NAMES.get_or_init(|| {
        let name = std::env::current_exe().ok().and_then(|exe| exe.file_stem().map(|stem| stem.to_string_lossy().into_owned()));
        Names { name: name.unwrap_or_default(), origin: None, registry: None }
    })
}

/* fixes the names every later call of this process reads where the first kit wins */
pub fn name(names: Names) {
    let _ = NAMES.set(names);
}

/* an engine lock message with the command this binary writes the lock with */
pub fn lock_help(msg: &str) -> String {
    match msg.ends_with(&format!(" in {}", edge::deps::lock::FILE)) {
        true => format!("{msg}\nhelp: run {} lock", names().name),
        false => msg.to_string(),
    }
}

/* a variable of the composition such as `EDGE_CDN_BASE` named after its binary */
fn variable(suffix: &str) -> Option<String> {
    let name = names().name.to_ascii_uppercase().replace('-', "_");
    std::env::var(format!("{name}_{suffix}")).ok()
}

/* the registry endpoint for `path` which tests and staging move with `<NAME>_SITE_BASE` */
pub fn site(path: &str) -> Result<String, String> {
    let base = variable("SITE_BASE").or_else(|| names().registry.clone()).ok_or_else(|| format!("{} names no registry to ask", names().name))?;
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/* tests and staging serve the composition's origin from `<NAME>_CDN_BASE` which production never sets */
pub fn cdn(url: &str) -> String {
    match &names().origin {
        Some(origin) => rebase(origin, url, variable("CDN_BASE").as_deref(), variable("SITE_BASE").is_some()),
        None => url.to_string(),
    }
}

/* whether an untrusted run may fetch `url` which only the composition's origin serves */
pub fn admitted(url: &str) -> bool {
    names().origin.as_ref().is_some_and(|origin| url.strip_prefix(origin.as_str()).is_some_and(|rest| rest.starts_with('/')))
}

// real registry packages live only on the real CDN and move only with the registry
fn rebase(origin: &str, url: &str, base: Option<&str>, registry_moved: bool) -> String {
    match (url.strip_prefix(origin), base) {
        (Some(path), Some(base)) if registry_moved || !path.starts_with("/pkg/") => format!("{}{path}", base.trim_end_matches('/')),
        _ => url.to_string(),
    }
}

/* retries hiccups with a growing pause and returns any answer about the resource at once */
pub fn get(source: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let call = || ureq::get(source).config().timeout_connect(Some(CONNECT_TIMEOUT)).timeout_global(Some(FETCH_TIMEOUT)).build().call();
    let mut pause = std::time::Duration::from_millis(200);

    for _ in 1..ATTEMPTS {
        match call() {
            Err(e) if again(&e) => std::thread::sleep(pause),
            done => return done,
        }
        pause *= 3;
    }

    call()
}

/* the whole body of a download refused past the one cap every fetch keeps */
pub fn body(resp: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, ureq::Error> {
    resp.body_mut().with_config().limit(MAX_FETCH_BYTES).read_to_vec()
}

/* a reset or an overloaded server may answer next time while a 404 already answered */
fn again(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(code) => *code == 429 || *code >= 500,
        // a server silent for the whole budget gets no second one
        ureq::Error::Timeout(ureq::Timeout::Global) => false,
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed | ureq::Error::Protocol(_) => true,
        _ => false,
    }
}

/* the per-user cache of the composition holding the modules runs fetched */
pub fn cache_root() -> Result<PathBuf, String> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(x).join(&names().name));
    }
    let home = std::env::var("HOME").map_err(|_| "cannot locate a cache dir (no HOME)".to_string())?;
    Ok(PathBuf::from(home).join(".cache").join(&names().name))
}

/* downloads stay out of the wiped cache as a browser never refetches like a module */
pub fn data_root() -> Result<PathBuf, String> {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(x).join(&names().name));
    }
    let home = std::env::var("HOME").map_err(|_| "cannot locate a data dir (no HOME)".to_string())?;
    Ok(PathBuf::from(home).join(".local").join("share").join(&names().name))
}

// wall-clock nanoseconds that every actor deadline is minted against
pub fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

pub fn wt<T>(r: wasmtime::Result<T>) -> Result<T> {
    r.map_err(|e| anyhow!("{e}"))
}

pub type Sink = Box<dyn FnMut(&str) + Send>;

/* whether a bind address is the loopback interface a request must then name */
pub fn loopback_bind(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/* whether a request's Host names the loopback address on `port` where no browser omits one */
pub fn hosted_here(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return true };
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| host.eq_ignore_ascii_case(&format!("{name}:{port}")) || (port == 80 && host.eq_ignore_ascii_case(name)))
}

/* whether a walk enters `dir` first by its real path so a link loop ends */
pub fn first_visit(seen: &mut std::collections::HashSet<PathBuf>, dir: &std::path::Path) -> bool {
    std::fs::canonicalize(dir).is_ok_and(|real| seen.insert(real))
}

/* whether `path` is names joined by slashes below a directory and nothing above it */
pub use edge::deps::rules::plain as plain_path;

// each registered module spec with the native table slice it occupies
pub type Registered = HashMap<String, (usize, Vec<String>)>;

// the universal dispatch export taking op receiver name argv argc and the out slot
pub type EdgeOp = TypedFunc<(i32, i32, i32, i32, i32, i32, i32), i32>;

/* a module the host serves under a name no import may shadow */
pub trait HostModule: Send + Sync {
    fn name(&self) -> &str;

    /* whether a grant may give the module `scope` after its name and a colon */
    fn scope(&self, scope: &str) -> bool;

    /* the scope `call` reaches with `args` which the kit checks first or none */
    fn reaches(&self, call: &str, args: &[WireValue]) -> Result<Option<String>, Fault>;

    /* whether `scope` from a grant covers the scope a call reaches */
    fn covers(&self, scope: &str, reached: &str) -> bool {
        scope == reached
    }

    /* what a refusal names as asked for a reached scope no held one covers */
    fn asked(&self, _held: &[String], reached: &str) -> String {
        reached.to_string()
    }

    /* registers the calls of the module in `inst` under `spec` for `pkg` holding `held` */
    fn open(&self, inst: &mut Instance, spec: &str, pkg: &str, held: Held) -> Result<(), String>;

    /* whether what a package holds lets it write `path` through a file channel */
    fn writes(&self, _held: &Held, _path: &str) -> bool {
        false
    }
}

/* a failed host call as an abi error kind and the text the program reads */
#[derive(Clone, Debug)]
pub struct Fault {
    pub kind: ErrorKind,
    // the class a custom kind raises which the runtime's descriptor names
    pub class: Option<Class>,
    pub text: String,
}

/* the roles of the classes a refused or failed host call raises */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class { Denied, Failed }

impl Fault {
    pub fn new(kind: ErrorKind, text: impl Into<String>) -> Fault {
        Fault { kind, class: None, text: text.into() }
    }

    /* the kind every failure without one of its own takes */
    pub fn runtime(text: impl Into<String>) -> Fault {
        Fault::new(ErrorKind::Runtime, text)
    }

    /* a call whose arguments the module refuses before it reaches anything */
    pub fn invalid(text: impl Into<String>) -> Fault {
        Fault::new(ErrorKind::Value, text)
    }

    /* a call no grant of the package covers */
    pub fn denied(text: impl Into<String>) -> Fault {
        Fault { class: Some(Class::Denied), ..Fault::new(ErrorKind::Custom, text) }
    }

    /* a call that reached out and met a failure */
    pub fn failed(text: impl Into<String>) -> Fault {
        Fault { class: Some(Class::Failed), ..Fault::new(ErrorKind::Custom, text) }
    }

    /* the text the program reads with the class the runtime names in front */
    pub fn message(&self, d: &Descriptor) -> String {
        match self.class {
            Some(Class::Denied) => format!("{}: {}", d.denied_error, self.text),
            Some(Class::Failed) => format!("{}: {}", d.failed_error, self.text),
            None => self.text.clone(),
        }
    }
}

/* the runtime loaded into wasmtime with what every scheduler thread shares */
pub struct Loaded {
    pub engine: Engine,
    module: Module,
    pub descriptor: &'static Descriptor,
    // the host modules the composition wired which every run of this runtime may import
    modules: Vec<Arc<dyn HostModule>>,
    // what trusted runs hold of them and the holder names each manifest field reserves
    access: Arc<dyn Access>,
    holders: Vec<(&'static str, &'static str)>,
    // what a run holds where its command line names no limit
    pub limits: Limits,
}

impl Loaded {
    /* maps a precompiled runtime or compiles a raw one once through the native cache */
    pub fn new(runtime: &Runtime, modules: Vec<Arc<dyn HostModule>>, access: Arc<dyn Access>, holders: Vec<(&'static str, &'static str)>, limits: Limits) -> Result<Arc<Loaded>> {
        let engine = wt(Engine::new(&config::base()))?;
        let module = match runtime.native() {
            // SAFETY whoever built the runtime vouched these bytes are this config's precompile
            Some(native) => wt(unsafe { Module::deserialize(&engine, native) })?,
            None => crate::wasm_cache::load(&engine, runtime.module()).map_err(|e| anyhow!("compiling the runtime: {e}"))?,
        };
        Ok(Arc::new(Loaded { engine, module, descriptor: runtime.descriptor(), modules, access, holders, limits }))
    }

    pub fn modules(&self) -> &[Arc<dyn HostModule>] {
        &self.modules
    }

    /* the access of a run where an untrusted run with none reaches nothing */
    pub fn access(&self, project: &Project) -> Arc<dyn Access> {
        match (&project.access, project.untrusted) {
            (Some(access), _) => access.clone(),
            (None, true) => Arc::new(Closed),
            (None, false) => self.access.clone(),
        }
    }

    /* the field a walk carries for `access` then the holders it reserves one per line */
    fn carried(&self, access: &dyn Access) -> String {
        let Some(field) = access.field() else { return String::new() };
        let holders = self.holders.iter().filter(|(_, by)| *by == field).map(|(holder, _)| *holder);
        std::iter::once(field).chain(holders).collect::<Vec<_>>().join("\n")
    }

    /* the module names the walk keeps any import from shadowing joined by newlines */
    fn reserved(&self) -> String {
        self.modules.iter().map(|module| module.name()).collect::<Vec<_>>().join("\n")
    }

    /* advances the engine epoch every tick so untrusted deadlines fire */
    pub fn start_ticker(self: &Arc<Self>) {
        let loaded = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_nanos(TICK_NS));
                loaded.engine.increment_epoch();
            }
        });
    }
}

/* per-thread linkers over the loaded runtime that mint the instances */
pub struct Host {
    pub loaded: Arc<Loaded>,
    // the runtime with its imports linked once per thread
    linked: InstancePre<State>,
    guest: Linker<State>,
    // third party plugins by the sha256 of their bytes compiled once per thread
    plugins: RefCell<HashMap<[u8; 32], InstancePre<State>>>,
}

impl Host {
    pub fn new(loaded: Arc<Loaded>) -> Result<Rc<Host>> {
        let mut linker = Linker::new(&loaded.engine);
        env::link(&mut linker)?;
        let linked = wt(linker.instantiate_pre(&loaded.module))?;
        let mut guest = Linker::new(&loaded.engine);
        plugins::link(&mut guest)?;
        Ok(Rc::new(Host { loaded, linked, guest, plugins: RefCell::new(HashMap::new()) }))
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        self.loaded.descriptor
    }

    /* a third party plugin linked for instantiation which Cranelift compiles on first sight */
    pub fn third_party(&self, bytes: &[u8]) -> Result<InstancePre<State>, String> {
        let key = edge::util::sha256::sha256(bytes);
        if let Some(pre) = self.plugins.borrow().get(&key) {
            return Ok(pre.clone());
        }
        let module = crate::wasm_cache::load(&self.loaded.engine, bytes)?;
        let pre = self.plugin_pre(&module)?;
        self.plugins.borrow_mut().insert(key, pre.clone());
        Ok(pre)
    }

    /* a plugin module linked against the guest imports */
    pub fn plugin_pre(&self, module: &Module) -> Result<InstancePre<State>, String> {
        wt(self.guest.instantiate_pre(module)).map_err(|e| e.to_string())
    }
}

/* one export of a plugin instance with the ABI exports its calls go through */
#[derive(Clone)]
pub struct Plugin {
    pub func: TypedFunc<(i32, i32, i32), i32>,
    pub alloc: TypedFunc<i32, i32>,
    pub free: Option<TypedFunc<(i32, i32), ()>>,
    pub memory: Memory,
    // where a host call the plugin waited on settles
    pub resume: Option<TypedFunc<(i32, i32, i32), i32>>,
}

// the calls one package opened of a host module shared by each of its natives
pub type Opened = Arc<Mutex<Box<dyn Calls>>>;

/* one entry of the native table the runtime dispatches `host_call_native` through */
#[derive(Clone)]
pub enum Native {
    Plugin(Box<Plugin>),
    // a host module call by its index among the names it was served under
    Host { calls: Opened, index: usize, name: Arc<str> },
}

/* what `--memory` in MB and `--ops` asked for with `None` for the composition's value */
#[derive(clap::Args, Clone, Copy, Default)]
pub struct RunLimits {
    /// Memory a run may hold, in MB
    #[arg(long, value_name = "MB")]
    pub memory: Option<usize>,
    /// Operations a run may spend
    #[arg(long)]
    pub ops: Option<u64>,
}

static RUN_LIMITS: OnceLock<RunLimits> = OnceLock::new();

impl RunLimits {
    /* sets them once from the command line for every run this process starts */
    pub fn set(self) {
        let _ = RUN_LIMITS.set(self);
    }

    pub fn get() -> RunLimits {
        RUN_LIMITS.get().copied().unwrap_or_default()
    }

    /* the limits a run holds where each flag given replaces the one `base` sets */
    pub fn over(self, base: Limits) -> Limits {
        Limits { memory: self.memory.map_or(base.memory, |mb| mb.saturating_mul(1 << 20)), ops: self.ops.unwrap_or(base.ops) }
    }

    /* the limit flags of `command` with help naming the defaults `limits` gives */
    pub fn shown(command: clap::Command, limits: Limits) -> clap::Command {
        command
            .mut_arg("memory", |arg| arg.help(format!("Memory a run may hold, in MB [default: {}]", limits.memory >> 20)))
            .mut_arg("ops", |arg| arg.help(format!("Operations a run may spend [default: {}]", limits.ops)))
    }
}

/* room past the limit for garbage plus a doubling container and the engine itself */
pub fn linear_cap(memory: usize) -> usize {
    memory.saturating_mul(3).saturating_add(64 << 20)
}

// refuses linear memory growth past the cap so an untrusted run cannot outgrow its slot
pub struct MemoryCap {
    pub max: usize,
}

impl ResourceLimiter for MemoryCap {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= self.max)
    }

    fn table_growing(&mut self, _current: usize, _desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

/* everything the host functions reach through the store shared by the slots of the instance */
pub struct State {
    pub exports: Option<Exports>,
    pub print: Sink,
    pub natives: Vec<Native>,
    pub registered: Registered,
    // ids of host calls parked since the last dispatch whose answers arrive as completions
    pub deferred: Vec<u32>,
    // the plugin calls running now innermost last which `edge_call_id` answers with
    pub running: Vec<u32>,
    // the plugin waiting on each call id resumed once the host call it made settles
    pub waiting: HashMap<u32, Box<Plugin>>,
    // messages `send()` handed over or `None` outside an actor pool where no scheduler drains them
    pub outbox: Option<Vec<(String, String)>>,
    // the bytes the outbox holds and the most a run may leave there between steps
    pub outbox_bytes: usize,
    pub outbox_cap: usize,
    // where a run's channels take each line the moment the program sends it
    pub channel: Option<ChannelSink>,
    // the first line a channel could not take which ends the run after its step
    pub channel_error: Option<String>,
    // the packages the last walk met which decide who writes a file channel
    pub senders: Option<Senders>,
    // the group the first send went to that no outbox took
    pub unsent: Option<String>,
    pub limiter: MemoryCap,
    // the selected slot and the channel its completions and events reach it through
    pub slot: u32,
    pub events: Option<Sender<Completion>>,
    // the host modules this instance opened which close with it
    pub opened: Vec<Opened>,
    // the runtime that names the classes a failed host call raises
    pub descriptor: &'static Descriptor,
}

impl State {
    /* aborts what every opened host module left running */
    pub(crate) fn close_opened(&mut self) {
        for calls in self.opened.drain(..) {
            calls.lock().unwrap_or_else(|e| e.into_inner()).close();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.close_opened();
    }
}

/* the runtime exports the host drives bound once per instance */
#[derive(Clone)]
pub struct Exports {
    pub memory: Memory,
    pub out_ptr: TypedFunc<(), i32>,
    pub out_len: TypedFunc<(), i32>,
    pub wasm_alloc: TypedFunc<i32, i32>,
    pub wasm_free: TypedFunc<(i32, i32), ()>,
    pub register_native_module: TypedFunc<(i32, i32, i32, i32, i32), ()>,
    pub register_module_error: TypedFunc<(i32, i32, i32, i32), ()>,
    pub reset_modules: TypedFunc<(), ()>,
    pub set_entry: TypedFunc<(i32, i32), ()>,
    pub set_input: TypedFunc<(i32, i32), ()>,
    pub repl_eval: TypedFunc<(i32, i32), i32>,
    pub run_start: TypedFunc<(i32, i32), i32>,
    pub run_resume: TypedFunc<(), i32>,
    pub run_push_event: TypedFunc<(i32, i32), i32>,
    pub set_host_result_by_id: TypedFunc<(i32, i32), i32>,
    pub set_host_error_by_id: TypedFunc<(i32, i32, i32), i32>,
    pub set_preempt_interval: TypedFunc<i32, ()>,
    pub save_state: TypedFunc<(), i64>,
    pub restore_state: TypedFunc<(i32, i32), i32>,
    pub host_edge_op: EdgeOp,
    pub host_edge_encode: TypedFunc<(i32, i32, i32), i32>,
    pub host_edge_decode: TypedFunc<(i32, i32, i32, i32), i32>,
    pub host_edge_release: TypedFunc<i32, ()>,
    pub host_edge_throw: TypedFunc<(i32, i32, i32), ()>,
    pub host_edge_take_error: TypedFunc<(i32, i32, i32), i32>,
    pub set_limits: TypedFunc<(i64, i64), ()>,
    pub vm_create: TypedFunc<(), i32>,
    pub vm_select: TypedFunc<i32, i32>,
    pub vm_drop: TypedFunc<i32, i32>,
    // the engine resolution walk where each call answers one step and leaves the next
    pub walk_start: TypedFunc<(i32, i32, i32, i32), i32>,
    pub walk_fetched: TypedFunc<(i32, i32, i32), i32>,
    pub walk_plugin_bytes: TypedFunc<(), i32>,
    pub walk_plugin: TypedFunc<(i32, i32, i32), i32>,
    pub walk_served: TypedFunc<(i32, i32), i32>,
    pub walk_known: TypedFunc<(i32, i32), i32>,
    // a runtime built before walks carried a field lacks it and carries its own default
    pub walk_carry: Option<TypedFunc<(i32, i32), ()>>,
    // a runtime older than it lacks it and every send then reads as the root's
    pub send_package: Option<TypedFunc<(), i32>>,
    // the runtime's descriptor which a runtime older than it lacks
    pub runtime_descriptor: Option<TypedFunc<(), i32>>,
}

impl Exports {
    fn bind(store: &mut Store<State>, instance: &Wasm) -> Result<Exports> {
        let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| anyhow!("edge.wasm exports no memory"))?;
        macro_rules! f {
            ($name:literal) => {
                wt(instance.get_typed_func(&mut *store, $name))?
            };
        }
        Ok(Exports {
            memory,
            out_ptr: f!("out_ptr"),
            out_len: f!("out_len"),
            wasm_alloc: f!("wasm_alloc"),
            wasm_free: f!("wasm_free"),
            register_native_module: f!("register_native_module"),
            register_module_error: f!("register_module_error"),
            reset_modules: f!("reset_modules"),
            set_entry: f!("set_entry"),
            set_input: f!("set_input"),
            repl_eval: f!("repl_eval"),
            run_start: f!("run_start"),
            run_resume: f!("run_resume"),
            run_push_event: f!("run_push_event"),
            set_host_result_by_id: f!("set_host_result_by_id"),
            set_host_error_by_id: f!("set_host_error_by_id"),
            set_preempt_interval: f!("set_preempt_interval"),
            save_state: f!("save_state"),
            restore_state: f!("restore_state"),
            host_edge_op: f!("host_edge_op"),
            host_edge_encode: f!("host_edge_encode"),
            host_edge_decode: f!("host_edge_decode"),
            host_edge_release: f!("host_edge_release"),
            host_edge_throw: f!("host_edge_throw"),
            host_edge_take_error: f!("host_edge_take_error"),
            set_limits: f!("set_limits"),
            vm_create: f!("vm_create"),
            vm_select: f!("vm_select"),
            vm_drop: f!("vm_drop"),
            walk_start: f!("walk_start"),
            walk_fetched: f!("walk_fetched"),
            walk_plugin_bytes: f!("walk_plugin_bytes"),
            walk_plugin: f!("walk_plugin"),
            walk_served: f!("walk_served"),
            walk_known: f!("walk_known"),
            walk_carry: instance.get_typed_func(&mut *store, "walk_carry").ok(),
            send_package: instance.get_typed_func(&mut *store, "send_package").ok(),
            runtime_descriptor: instance.get_typed_func(&mut *store, "runtime_descriptor").ok(),
        })
    }
}

pub(crate) fn read(cx: &mut impl AsContextMut<Data = State>, mem: Memory, ptr: i32, len: i32) -> Vec<u8> {
    let mut buf = vec![0u8; len.max(0) as usize];
    if mem.read(&mut *cx, ptr as u32 as usize, &mut buf).is_err() {
        buf.clear();
    }
    buf
}

pub(crate) fn write(cx: &mut impl AsContextMut<Data = State>, mem: Memory, ptr: i32, bytes: &[u8]) {
    let _ = mem.write(&mut *cx, ptr as u32 as usize, bytes);
}

pub(crate) fn read_u32(cx: &mut impl AsContextMut<Data = State>, mem: Memory, ptr: i32) -> u32 {
    let bytes = read(cx, mem, ptr, 4);
    bytes.try_into().map(u32::from_le_bytes).unwrap_or(0)
}

pub(crate) fn write_u32(cx: &mut impl AsContextMut<Data = State>, mem: Memory, ptr: i32, value: u32) {
    write(cx, mem, ptr, &value.to_le_bytes());
}

/* copies bytes into a fresh runtime allocation that `unstage` frees with the same length */
pub(crate) fn stage(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, bytes: &[u8]) -> wasmtime::Result<i32> {
    let ptr = ex.wasm_alloc.call(&mut *cx, bytes.len().max(1) as i32)?;
    write(cx, ex.memory, ptr, bytes);
    Ok(ptr)
}

pub(crate) fn unstage(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, ptr: i32, len: usize) {
    let _ = ex.wasm_free.call(&mut *cx, (ptr, len.max(1) as i32));
}

#[cfg(test)]
mod tests {
    use super::*;

    // a package follows its registry while everything built comes from the staged CDN
    #[test]
    fn a_staged_run_reads_its_cdn_but_the_real_registrys_packages() {
        let (origin, staged) = ("https://cdn.example", "http://127.0.0.1:8788");
        for path in ["/edge.wasm", "/js/src/index.js", "/cli/install.sh"] {
            assert_eq!(rebase(origin, &format!("{origin}{path}"), Some(staged), false), format!("{staged}{path}"));
        }
        let pkg = format!("{origin}/pkg/test/0.1.0/app.edge");
        assert_eq!(rebase(origin, &pkg, Some(staged), false), pkg);
        assert_eq!(rebase(origin, &pkg, Some(staged), true), format!("{staged}/pkg/test/0.1.0/app.edge"));
    }

    // a refused or failed call raises the class its runtime names for that role
    #[test]
    fn a_host_fault_reads_its_class_from_the_runtime() {
        let named = Descriptor { denied_error: "Refused", failed_error: "Broken", ..mini::DESCRIPTOR };
        assert_eq!(Fault::denied("no grant").message(&named), "Refused: no grant");
        assert_eq!(Fault::failed("gone").message(&named), "Broken: gone");
        assert_eq!(Fault::invalid("bad").message(&named), "bad");
        assert_eq!(Fault::denied("no grant").message(&mini::DESCRIPTOR), "PermissionError: no grant");
    }
}
