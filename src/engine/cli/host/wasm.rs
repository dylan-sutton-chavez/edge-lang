mod cache;
mod env;
mod plugins;
mod rt;
pub(crate) mod vm;

pub(crate) use vm::Instance;

use super::{Hosted, Hosting, Wiring};
use crate::engine::cli::kit::Descriptor;
use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use wasmtime::{AsContextMut, Caller, Config, Engine, Instance as Wasm, InstancePre, Linker, Memory, Module, ResourceLimiter, Store, TypedFunc};

// the epoch ticker period that untrusted deadlines count in
pub(crate) const TICK_NS: u64 = 100_000_000;

pub(super) fn wt<T>(r: wasmtime::Result<T>) -> Result<T> {
    r.map_err(|e| anyhow!("{e}"))
}

/* the settings the cached native code and every engine of the runtime must agree on */
fn config() -> Config {
    let mut cfg = Config::new();
    cfg.epoch_interruption(true);
    cfg.consume_fuel(false);
    // a full 4 GiB reservation lets Cranelift drop bounds checks on loads and stores
    cfg.memory_reservation(4 << 30);
    cfg.memory_guard_size(32 << 20);
    cfg.memory_init_cow(true);
    // `MAX_NESTED` engine loops take up to about 620 KiB of native stack on x86
    cfg.max_wasm_stack(1 << 20);
    cfg
}

/* the runtime exports a host or plugin import reaches through the store */
fn exports(caller: &Caller<'_, State>) -> Exports {
    caller.data().exports.clone().expect("runtime exports bound before any host or plugin call")
}

// the universal dispatch export taking op receiver name argv argc and the out slot
pub(super) type InterpreterOp = TypedFunc<(i32, i32, i32, i32, i32, i32, i32), i32>;

/* the runtime loaded into wasmtime with what every scheduler thread shares */
pub(crate) struct Loaded {
    pub engine: Engine,
    module: Module,
    pub wiring: Arc<Wiring>,
}

impl Loaded {
    /* compiles the runtime once through the native cache and maps it from there after */
    pub(crate) fn new(wasm: &[u8], wiring: Arc<Wiring>) -> Result<Arc<Loaded>> {
        let engine = wt(Engine::new(&config()))?;
        let module = cache::load(&engine, wasm).map_err(|e| anyhow!("compiling the runtime: {e}"))?;
        Ok(Arc::new(Loaded { engine, module, wiring }))
    }

    /* advances the engine epoch every tick so untrusted deadlines fire */
    pub(crate) fn start_ticker(self: &Arc<Self>) {
        let loaded = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_nanos(TICK_NS));
                loaded.engine.increment_epoch();
            }
        });
    }
}

/* the names a runtime module gives itself through `runtime_descriptor` kept for the whole process */
pub(crate) fn describe(wasm: &[u8]) -> Result<&'static Descriptor> {
    let engine = wt(Engine::new(&config()))?;
    let module = cache::load(&engine, wasm).map_err(|e| anyhow!("compiling the runtime: {e}"))?;
    let mut linker: Linker<()> = Linker::new(&engine);
    // naming itself reaches no host so each import answers nothing
    wt(linker.func_wrap("env", "host_print", |_: i32, _: i32| {}))?;
    wt(linker.func_wrap("env", "host_fetch_bytes", |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 }))?;
    wt(linker.func_wrap("env", "host_send", |_: i32, _: i32, _: i32, _: i32| -> i32 { 1 }))?;
    wt(linker.func_wrap("env", "host_call_native", |_: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 1 }))?;
    let mut store = Store::new(&engine, ());
    // the engine counts epochs for untrusted runs and naming itself needs no deadline
    store.set_epoch_deadline(u64::MAX);
    let instance = wt(linker.instantiate(&mut store, &module))?;
    // a module built before the export speaks the first level
    let level = match instance.get_typed_func::<(), i32>(&mut store, "interpreter_runtime_abi") {
        Ok(f) => wt(f.call(&mut store, ()))?,
        Err(_) => 1,
    };
    if !(1..=crate::engine::abi::RUNTIME_ABI as i32).contains(&level) {
        anyhow::bail!("the runtime speaks host contract v{level} and this cli serves v1 to v{}", crate::engine::abi::RUNTIME_ABI);
    }
    let not_one = || anyhow!("the module is no interpreter runtime, it exports no runtime_descriptor");
    let describe: TypedFunc<(), i32> = instance.get_typed_func(&mut store, "runtime_descriptor").map_err(|_| not_one())?;
    let out_ptr: TypedFunc<(), i32> = instance.get_typed_func(&mut store, "out_ptr").map_err(|_| not_one())?;
    let memory = instance.get_memory(&mut store, "memory").ok_or_else(not_one)?;
    let len = wt(describe.call(&mut store, ()))?;
    let ptr = wt(out_ptr.call(&mut store, ()))?;
    let mut json = vec![0u8; len.max(0) as usize];
    memory.read(&store, ptr as u32 as usize, &mut json).map_err(|e| anyhow!("reading the runtime's descriptor: {e}"))?;
    descriptor(&json)
}

/* a descriptor read from the JSON every runtime writes of itself */
pub(super) fn descriptor(json: &[u8]) -> Result<&'static Descriptor> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| anyhow!("the runtime describes itself in no JSON: {e}"))?;
    let text = |key: &str| -> Result<&'static str> {
        let s = v[key].as_str().ok_or_else(|| anyhow!("the runtime's descriptor has no {key}"))?;
        Ok(Box::leak(s.to_owned().into_boxed_str()))
    };
    let entries: Vec<&'static str> = v["entries"].as_array().map(|a| a.iter().filter_map(|e| e.as_str()).map(|e| &*Box::leak(e.to_owned().into_boxed_str())).collect()).unwrap_or_default();
    if entries.is_empty() {
        anyhow::bail!("the runtime's descriptor names no entry file");
    }
    let status = |at: usize, or: u8| v["statuses"][at].as_u64().and_then(|n| u8::try_from(n).ok()).unwrap_or(or);
    let described = Descriptor {
        language: text("language")?,
        extension: text("extension")?,
        entries: Box::leak(entries.into_boxed_slice()),
        test_suffix: text("test_suffix")?,
        scaffold: text("scaffold")?,
        example_fence: text("example_fence")?,
        manifest_fence: text("manifest_fence")?,
        type_error: text("type_error")?,
        value_error: text("value_error")?,
        runtime_error: text("runtime_error")?,
        denied_error: text("denied_error")?,
        failed_error: text("failed_error")?,
        statuses: (status(0, 1), status(1, 1)),
        diagnosed: None,
    };
    Ok(Box::leak(Box::new(described)))
}

/* per-thread linkers over the loaded runtime that mint the instances */
pub(crate) struct Host {
    pub loaded: Arc<Loaded>,
    // the runtime with its imports linked once per thread
    linked: InstancePre<State>,
    guest: Linker<State>,
    // third party plugins by the sha256 of their bytes compiled once per thread
    plugins: RefCell<HashMap<[u8; 32], InstancePre<State>>>,
}

impl Host {
    pub(crate) fn new(loaded: Arc<Loaded>) -> Result<Rc<Host>> {
        let mut linker = Linker::new(&loaded.engine);
        env::link(&mut linker)?;
        let linked = wt(linker.instantiate_pre(&loaded.module))?;
        let mut guest = Linker::new(&loaded.engine);
        plugins::link(&mut guest)?;
        Ok(Rc::new(Host { loaded, linked, guest, plugins: RefCell::new(HashMap::new()) }))
    }

    pub(crate) fn descriptor(&self) -> &'static Descriptor {
        self.loaded.wiring.descriptor
    }

    /* a third party plugin linked for instantiation which Cranelift compiles on first sight */
    pub(crate) fn third_party(&self, bytes: &[u8]) -> Result<InstancePre<State>, String> {
        let key = crate::engine::util::sha256::sha256(bytes);
        if let Some(pre) = self.plugins.borrow().get(&key) {
            return Ok(pre.clone());
        }
        let module = cache::load(&self.loaded.engine, bytes)?;
        // the module linked against the guest imports
        let pre = wt(self.guest.instantiate_pre(&module)).map_err(|e| e.to_string())?;
        self.plugins.borrow_mut().insert(key, pre.clone());
        Ok(pre)
    }
}

/* one export of a plugin instance with the ABI exports its calls go through */
#[derive(Clone)]
pub(crate) struct Plugin {
    pub func: TypedFunc<(i32, i32, i32), i32>,
    pub alloc: TypedFunc<i32, i32>,
    pub free: Option<TypedFunc<(i32, i32), ()>>,
    pub memory: Memory,
    // where a host call the plugin waited on settles
    pub resume: Option<TypedFunc<(i32, i32, i32), i32>>,
}

/* one entry of the native table the runtime dispatches `host_call_native` through */
#[derive(Clone)]
pub(crate) enum Native {
    Plugin(Box<Plugin>),
    Host(Hosted),
}

/* room past the limit for garbage plus a doubling container and the engine itself */
pub(crate) fn linear_cap(memory: usize) -> usize {
    memory.saturating_mul(3).saturating_add(64 << 20)
}

// refuses linear memory growth past the cap so an untrusted run cannot outgrow its slot
pub(crate) struct MemoryCap {
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
pub(crate) struct State {
    pub exports: Option<Exports>,
    // what the host functions of either engine reach of a run
    pub hosting: Hosting,
    pub natives: Vec<Native>,
    // the plugin calls running now innermost last which `interpreter_call_id` answers with
    pub running: Vec<u32>,
    // the plugin waiting on each call id resumed once the host call it made settles
    pub waiting: HashMap<u32, Box<Plugin>>,
    pub limiter: MemoryCap,
    // the selected slot whose completions and events reach it through the hosting's channel
    pub slot: u32,
}

/* the runtime exports the host drives bound once per instance */
#[derive(Clone)]
pub(crate) struct Exports {
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
    pub host_interpreter_op: InterpreterOp,
    pub host_interpreter_encode: TypedFunc<(i32, i32, i32), i32>,
    pub host_interpreter_decode: TypedFunc<(i32, i32, i32, i32), i32>,
    pub host_interpreter_release: TypedFunc<i32, ()>,
    pub host_interpreter_throw: TypedFunc<(i32, i32, i32), ()>,
    pub host_interpreter_take_error: TypedFunc<(i32, i32, i32), i32>,
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
    // a runtime built before walks carried a field lacks it and carries its own default
    pub walk_carry: Option<TypedFunc<(i32, i32), ()>>,
    // a runtime older than it lacks it and every send then reads as the root's
    pub send_package: Option<TypedFunc<(), i32>>,
    // the runtime's descriptor which a runtime older than it lacks
    pub runtime_descriptor: Option<TypedFunc<(), i32>>,
}

impl Exports {
    fn bind(store: &mut Store<State>, instance: &Wasm) -> Result<Exports> {
        let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| anyhow!("the runtime exports no memory"))?;
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
            host_interpreter_op: f!("host_interpreter_op"),
            host_interpreter_encode: f!("host_interpreter_encode"),
            host_interpreter_decode: f!("host_interpreter_decode"),
            host_interpreter_release: f!("host_interpreter_release"),
            host_interpreter_throw: f!("host_interpreter_throw"),
            host_interpreter_take_error: f!("host_interpreter_take_error"),
            set_limits: f!("set_limits"),
            vm_create: f!("vm_create"),
            vm_select: f!("vm_select"),
            vm_drop: f!("vm_drop"),
            walk_start: f!("walk_start"),
            walk_fetched: f!("walk_fetched"),
            walk_plugin_bytes: f!("walk_plugin_bytes"),
            walk_plugin: f!("walk_plugin"),
            walk_served: f!("walk_served"),
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
    use super::describe;

    // a module whose only export says it speaks the host contract at `level`
    fn speaking(level: u8) -> Vec<u8> {
        let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x1b, 0x01, 0x17];
        m.extend_from_slice(b"interpreter_runtime_abi");
        m.extend_from_slice(&[0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, level, 0x0b]);
        m
    }

    #[test]
    fn a_runtime_of_a_newer_host_contract_is_refused_by_its_level() {
        let Err(e) = describe(&speaking(crate::engine::abi::RUNTIME_ABI as u8 + 1)) else { panic!("a newer level is refused") };
        assert!(e.to_string().contains("speaks host contract v2 and this cli serves v1 to v1"), "{e}");
    }

    #[test]
    fn a_runtime_of_a_known_level_passes_the_level_check() {
        let Err(e) = describe(&speaking(1)) else { panic!("a module with no descriptor is refused") };
        assert!(e.to_string().contains("exports no runtime_descriptor"), "{e}");
    }
}
