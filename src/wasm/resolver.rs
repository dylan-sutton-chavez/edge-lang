use crate::modules::{NativeBinding, Resolved, Resolver, partition_bindings, parse_manifest, walk_up_dirs, dir_of, join_relative, system_spec};
use crate::util::hash::FxHashSet;
use alloc::{boxed::Box, string::{String, ToString}, vec::Vec};
use crate::s;

use super::{ModuleEntry, host_fetch_bytes, with_runtime, with_slot};
use super::exports::wasm_free;
use crate::abi::ErrorKind;
use crate::bridge::{error_from_kind, get_val, put_val, release_handles, take_error, with_bridge, with_vm};
use crate::vm::types::{Val, VmErr};
use alloc::sync::Arc;

// Cap on edge.json `extends` chain, bounds attacker-crafted loops, 32 dwarfs real workspace depth.
const MAX_PACKAGES_HOPS: u32 = 32;

pub(super) struct WasmHostResolver {
    pub(super) dir: String,
    // Resolves for code compiled while the program runs, which finds only the modules the host already holds.
    pub(super) late: bool,
}

impl Resolver for WasmHostResolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        if !spec.contains('/') {
            let dir = self.dir.clone();
            return self.resolve_bare(spec, &dir);
        }
        let canonical = if spec.contains("://") || spec.starts_with('/') {
            spec.to_string()
        } else if spec.starts_with("./") || spec.starts_with("../") {
            join_relative(&self.dir, spec)
        } else {
            // A dotted import anchors at the nearest manifest dir.
            let root = self.manifest_root(spec)?;
            join_relative(&root, spec)
        };
        self.resolve_canonical(&canonical)
    }

    fn fetch_bytes(&mut self,spec: &str,expected_hash: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
        let mut len: u32 = 0;
        let hash_ptr = expected_hash.as_ref().map(|h| h.as_ptr()).unwrap_or(core::ptr::null());
        let ptr = unsafe {
            host_fetch_bytes(spec.as_ptr(), spec.len() as u32, hash_ptr, &mut len as *mut u32)
        };
        if ptr.is_null() {
            return Err(s!("no bytes cached by host for '", str spec, "'"));
        }
        // Host allocates via `wasm_alloc` (abi.md), copy into a guest Vec, then `wasm_free`. `Vec::from_raw_parts` would UB by freeing Box-laid memory through Vec's layout.
        let len = len as usize;
        let bytes: Vec<u8> = unsafe { core::slice::from_raw_parts(ptr, len) }.to_vec();
        unsafe { wasm_free(ptr, len as u32) };
        Ok(bytes)
    }

    fn child(&self, spec: &str) -> Box<dyn Resolver> {
        Box::new(WasmHostResolver { dir: dir_of(spec).to_string(), late: self.late })
    }
}

impl WasmHostResolver {
    fn resolve_bare(&mut self, name: &str, start_dir: &str) -> Result<Resolved, String> {
        let mut visited: FxHashSet<String> = FxHashSet::default();
        let mut search_dir = start_dir.to_string();
        let mut hops: u32 = 0;
        // The nearest manifest names the package the import comes from.
        let mut package: Option<String> = None;
        loop {
            if hops > MAX_PACKAGES_HOPS {
                return Err(s!(
                    "edge.json walk-up exceeded ",
                    int MAX_PACKAGES_HOPS as i64,
                    " hops resolving '", str name, "'"));
            }
            hops += 1;

            let mut hit: Option<(String, Option<String>, Option<String>)> = None;
            for dir in walk_up_dirs(&search_dir) {
                let m_spec = s!(str &dir, "edge.json");
                if let Some((target, ext)) = self.lookup_in_manifest(&m_spec, name)? {
                    hit = Some((dir, target, ext));
                    break;
                }
            }
            let Some((dir, target, ext)) = hit else {
                return self.resolve_system(name, package.as_deref());
            };
            package.get_or_insert_with(|| dir.clone());
            if let Some(target) = target {
                let canonical = join_relative(&dir, &target);
                return self.resolve_canonical(&canonical);
            }
            let m_spec = s!(str &dir, "edge.json");
            if let Some(ext) = ext {
                if !visited.insert(m_spec) {
                    return Err(s!("circular extends chain in edge.json"));
                }
                let mut next = join_relative(&dir, &ext);
                if !next.ends_with('/') { next.push('/'); }
                search_dir = next;
                continue;
            }
            return self.resolve_system(name, package.as_deref());
        }
    }

    /* A name no manifest declares may be a system module, which the host serves or refuses per package. */
    fn resolve_system(&self, name: &str, package: Option<&str>) -> Result<Resolved, String> {
        let Some(dir) = package else { return Err(self.missing(name)) };
        let spec = system_spec(name, dir);
        let served = refusal(&spec).is_some() || with_runtime(|rt| rt.registry.iter().any(|(s, _)| *s == spec));
        if !served {
            return Err(self.missing(name));
        }
        self.resolve_canonical(&spec)
    }

    /* A module the host does not hold, which code compiled at run time can never load. */
    fn missing(&self, name: &str) -> String {
        if self.late { s!("module '", str name, "' is not one the program imports, and code that eval or exec compiles imports only those") } else { undeclared(name) }
    }

    /* Nearest ancestor dir holding an edge.json, probed live like the bare-name walk-up. */
    fn manifest_root(&mut self, spec: &str) -> Result<String, String> {
        let start = self.dir.clone();
        for dir in walk_up_dirs(&start) {
            let m_spec = s!(str &dir, "edge.json");
            let cached = with_runtime(|rt| rt.manifests.iter().any(|(s, _)| s == &m_spec));
            if cached || self.fetch_bytes(&m_spec, None).is_ok() {
                return Ok(dir);
            }
        }
        Err(s!("no edge.json above '", str &self.dir, "' to resolve '", str spec, "'"))
    }

    #[allow(clippy::type_complexity)]
    fn lookup_in_manifest(&mut self, m_spec: &str, name: &str) -> Result<Option<(Option<String>, Option<String>)>, String> {
        if let Some(hit) = with_runtime(|rt| {
            rt.manifests.iter()
                .find(|(s, _)| s == m_spec)
                .map(|(_, m)| (m.imports.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()), m.extends.clone()))
        }) {
            return Ok(Some(hit));
        }
        // Walk-up fetch, manifests aren't pinned by URL fragment, so no hash.
        let bytes = match self.fetch_bytes(m_spec, None) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let parsed = parse_manifest(&bytes).map_err(|e| s!("edge.json at '", str m_spec, "': ", str &e))?;
        let target = parsed.imports.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let ext = parsed.extends.clone();
        with_runtime(|rt| rt.manifests.push((m_spec.to_string(), parsed)));
        Ok(Some((target, ext)))
    }

    fn resolve_canonical(&self, spec: &str) -> Result<Resolved, String> {
        if let Some(msg) = refusal(spec) {
            return Err(msg);
        }
        let entry = with_runtime(|rt| rt.registry.iter().find(|(s, _)| s == spec).map(|(_, e)| e.clone()))
            .ok_or_else(|| if self.late { self.missing(spec) } else { s!("module '", str spec, "' not registered (host did not pre-fetch / register before run())") })?;
        match entry {
            ModuleEntry::Code(src) => Ok(Resolved::Code {
                src,
                canonical: spec.to_string(),
            }),
            ModuleEntry::Native(funcs) => {
                let module: Arc<str> = Arc::from(spec);
                let all: Vec<NativeBinding> = funcs.iter().map(|(n, id)| make_native_binding(n.clone(), *id, module.clone())).collect();
                let (bindings, classes, consts) = partition_bindings(all);
                Ok(Resolved::Native { bindings, classes, consts, canonical: spec.to_string() })
            }
        }
    }
}

/* Why the host cannot load `spec`, as it registered through `register_module_error`. */
fn refusal(spec: &str) -> Option<String> {
    with_runtime(|rt| rt.refusals.iter().find(|(s, _)| s == spec).map(|(_, m)| m.clone()))
}

/* An undeclared bare name, the host's own wording wins when it registered one. */
fn undeclared(name: &str) -> String {
    refusal(name).unwrap_or_else(|| s!("module '", str name, "' is not provided by this host and no edge.json declares it"))
}

/* A plugin's system call `module.name`, served as a `.py` beside the plugin would import it. */
pub(crate) fn system_call(call_id: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    let (module, function) = name.split_once('.').ok_or_else(|| VmErr::Raised(s!("ValueError: a system call is named module.name, got '", str name, "'")))?;
    let caller = with_bridge(|b| b.calls.iter().rev().find(|(id, _)| *id == call_id).map(|(_, m)| m.clone()))
        .ok_or(VmErr::Runtime("a system call names the plugin call it belongs to, see edge_call_id"))?;
    let (spec, id) = WasmHostResolver { dir: dir_of(&caller), late: false }.system_fn(module, function)?;
    call_host(id, call_id, &spec, args, None)
}

impl WasmHostResolver {
    /* Where `module.function` is served to the package holding `self.dir`, never an import sharing its name. */
    fn system_fn(&mut self, module: &str, function: &str) -> Result<(Arc<str>, u32), VmErr> {
        let start = self.dir.clone();
        let mut package = None;
        for dir in walk_up_dirs(&start) {
            if self.lookup_in_manifest(&s!(str &dir, "edge.json"), module).map_err(VmErr::Raised)?.is_some() {
                package = Some(dir);
                break;
            }
        }
        let denied = |msg: String| VmErr::Raised(s!("PermissionError: ", str &msg));
        let spec = system_spec(module, &package.ok_or_else(|| denied(undeclared(module)))?);
        if let Some(msg) = refusal(&spec) {
            return Err(denied(msg));
        }
        let funcs = with_runtime(|rt| rt.registry.iter().find(|(s, _)| *s == spec).and_then(|(_, e)| match e {
            ModuleEntry::Native(funcs) => Some(funcs.clone()),
            ModuleEntry::Code(_) => None,
        }));
        let funcs = funcs.ok_or_else(|| denied(undeclared(module)))?;
        let id = funcs.iter().find(|(n, _)| n == function).map(|(_, id)| *id);
        id.map(|id| (Arc::from(spec.as_str()), id)).ok_or_else(|| VmErr::Attribute(s!("module '", str module, "' has no call '", str function, "'")))
    }
}

/* Builds a NativeBinding that marshals handles around `host_call_native`. Lives here so the bridge stays host-import-free. */
fn make_native_binding(name: String, id: u32, module: Arc<str>) -> NativeBinding {
    let closure = move |_: &mut crate::vm::types::HeapPool, args: &[Val], kwargs: Option<Val>| -> Result<Val, VmErr> {
        if module.starts_with("system:") { answers_caller(&module)?; }
        // call_id is what call_extern will park with on defer, lets the host route the result back.
        let call_id = with_vm(|vm| vm.next_host_call_id as u32).unwrap_or(0);
        call_host(id, call_id, &module, args, kwargs)
    };
    NativeBinding { name, func: Arc::new(closure) }
}

/* A system module answers only code of the package that imported it, whoever else holds the object. */
fn answers_caller(spec: &str) -> Result<(), VmErr> {
    let caller = with_vm(|vm| vm.extern_caller.clone()).flatten().unwrap_or_default();
    let name = spec.strip_prefix("system:").and_then(|s| s.split_once('@')).map_or("", |(n, _)| n);
    // The entry chunk has no path, its imports resolve from the entry dir and so does its package.
    let dir = if caller.is_empty() { with_slot(|s| s.entry_dir.clone()) } else { dir_of(&caller) };
    if package_of(&dir).is_some_and(|package| system_spec(name, &package) == spec) { return Ok(()); }
    Err(VmErr::Raised(s!("PermissionError: this ", str name, " module was imported by another package and answers only it, import ", str name, " here instead")))
}

/* The dir of the nearest edge.json at or above `dir`, the package the code in it belongs to. */
fn package_of(dir: &str) -> Option<String> {
    if let Some(found) = with_runtime(|rt| rt.packages.iter().find(|(d, _)| d == dir).map(|(_, p)| p.clone())) { return found; }
    let mut resolver = WasmHostResolver { dir: dir.to_string(), late: false };
    let found = walk_up_dirs(dir).find(|up| matches!(resolver.lookup_in_manifest(&s!(str up, "edge.json"), ""), Ok(Some(_))));
    with_runtime(|rt| rt.packages.push((dir.to_string(), found.clone())));
    found
}

/* One host_call_native, remembered by its call id until it answers, for the system calls inside. */
fn call_host(id: u32, call_id: u32, module: &Arc<str>, args: &[Val], kwargs: Option<Val>) -> Result<Val, VmErr> {
    /* 1. Register positional args as handles the guest will see, append the kwargs handle (0 means no kwargs). */
    let mut argv: Vec<u32> = args.iter().map(|v| put_val(*v)).collect();
    argv.push(kwargs.map_or(0, put_val));
    let mut out_handle: u32 = 0;

    // 2. A system module never makes system calls, so only the others are remembered.
    let remembered = !module.starts_with("system:");
    if remembered {
        with_bridge(|b| b.calls.push((call_id, module.clone())));
    }
    let status = unsafe {
        super::host_call_native(
            id, call_id,
            argv.as_ptr(), argv.len() as u32,
            &mut out_handle as *mut u32,
        )
    };
    // A waiting call stays remembered until answered, since its resumed plugin still calls from it.
    if remembered && status != 2 {
        with_bridge(|b| b.calls.pop());
    }

    /* 3. Read result BEFORE releasing argv, a returned input would point into slots we're about to free. */
    // Status 2 = DEFERRED, handler has captured what it needs, release argv and park the VM.
    if status == 2 {
        release_handles(&argv);
        return Err(VmErr::HostCallDeferred);
    }
    if status != 0 {
        release_handles(&argv);
        let (kind, msg) = take_error()
            .unwrap_or((ErrorKind::Runtime as u32, String::from("native call failed")));
        return Err(error_from_kind(kind, msg));
    }
    let result = get_val(out_handle).ok_or(VmErr::Runtime("native returned invalid handle"))?;
    argv.push(out_handle);
    release_handles(&argv);
    Ok(result)
}

