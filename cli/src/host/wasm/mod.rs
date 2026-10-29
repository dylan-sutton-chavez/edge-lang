mod cache;
pub mod config;
mod env;
mod plugins;
mod rt;
mod vm;

pub use vm::{time_limit, Instance, Vm};

use super::{Hosted, Hosting, Wiring};
use crate::kit::Descriptor;
use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use wasmtime::{AsContextMut, Engine, Instance as Wasm, InstancePre, Linker, Memory, Module, ResourceLimiter, Store, TypedFunc};

// the epoch ticker period that untrusted deadlines count in
pub const TICK_NS: u64 = 100_000_000;

pub fn wt<T>(r: wasmtime::Result<T>) -> Result<T> {
    r.map_err(|e| anyhow!("{e}"))
}

// the universal dispatch export taking op receiver name argv argc and the out slot
pub type EdgeOp = TypedFunc<(i32, i32, i32, i32, i32, i32, i32), i32>;

/* the runtime loaded into wasmtime with what every scheduler thread shares */
pub struct Loaded {
    pub engine: Engine,
    module: Module,
    pub wiring: Arc<Wiring>,
}

impl Loaded {
    /* maps a precompiled runtime or compiles a raw one once through the native cache */
    pub fn new(wasm: &[u8], native: Option<&'static [u8]>, wiring: Arc<Wiring>) -> Result<Arc<Loaded>> {
        let engine = wt(Engine::new(&config::base()))?;
        let module = match native {
            // SAFETY whoever built the runtime vouched these bytes are this config's precompile
            Some(native) => wt(unsafe { Module::deserialize(&engine, native) })?,
            None => cache::load(&engine, wasm).map_err(|e| anyhow!("compiling the runtime: {e}"))?,
        };
        Ok(Arc::new(Loaded { engine, module, wiring }))
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
        self.loaded.wiring.descriptor
    }

    /* a third party plugin linked for instantiation which Cranelift compiles on first sight */
    pub fn third_party(&self, bytes: &[u8]) -> Result<InstancePre<State>, String> {
        let key = edge::util::sha256::sha256(bytes);
        if let Some(pre) = self.plugins.borrow().get(&key) {
            return Ok(pre.clone());
        }
        let module = cache::load(&self.loaded.engine, bytes)?;
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

/* one entry of the native table the runtime dispatches `host_call_native` through */
#[derive(Clone)]
pub enum Native {
    Plugin(Box<Plugin>),
    Host(Hosted),
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
    // what the host functions of either engine reach of a run
    pub hosting: Hosting,
    pub natives: Vec<Native>,
    // the plugin calls running now innermost last which `edge_call_id` answers with
    pub running: Vec<u32>,
    // the plugin waiting on each call id resumed once the host call it made settles
    pub waiting: HashMap<u32, Box<Plugin>>,
    pub limiter: MemoryCap,
    // the selected slot whose completions and events reach it through the hosting's channel
    pub slot: u32,
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

