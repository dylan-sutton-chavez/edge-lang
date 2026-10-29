use crate::deps::{DepHost, Deps, HostResolver, NativeBinding, dir_of};
use alloc::{string::String, sync::Arc, vec::Vec};
use crate::s;

use super::{with_runtime, with_slot};
use crate::abi::ErrorKind;
use crate::abi::bridge::{error_from_kind, get_val, put_val, release_handles, take_error, with_bridge, with_vm};
use crate::r#unsafe::web;
use crate::value::{Val, VmErr};

/* the host dependency management reaches through the wasm imports */
#[derive(Clone, Copy)]
pub(super) struct WebHost;

impl DepHost for WebHost {
    fn deps<R>(&mut self, f: impl FnOnce(&mut Deps) -> R) -> R {
        with_runtime(|rt| f(&mut rt.deps))
    }

    fn fetch(&mut self, spec: &str, pin: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
        web::fetch(spec, pin.as_ref()).ok_or_else(|| s!("no bytes cached by host for '", str spec, "'"))
    }

    fn native(&mut self, name: String, id: u32, module: Arc<str>) -> NativeBinding {
        let closure = move |_: &mut crate::value::HeapPool, args: &[Val], kwargs: Option<Val>| -> Result<Val, VmErr> {
            // the id `call_extern` parks with on defer so the host routes the result back
            let call_id = with_vm(|vm| vm.next_host_call_id as u32).unwrap_or(0);
            call_host(id, call_id, &module, args, kwargs)
        };
        NativeBinding { name, func: Arc::new(closure) }
    }

    fn caller_dir(&mut self) -> String {
        let caller = with_vm(|vm| vm.extern_caller.clone()).flatten().unwrap_or_default();
        // the entry chunk has no path so its code resolves from the entry dir
        if caller.is_empty() { with_slot(|s| s.entry_dir.clone()) } else { dir_of(&caller) }
    }

    fn changed(&mut self) {
        with_runtime(|rt| rt.registry_changed());
    }
}

/* a plugin's system call `module.name` served as an import of that module beside the plugin */
pub(crate) fn system_call(call_id: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    let (module, function) = name.split_once('.').ok_or_else(|| VmErr::Of(crate::lang::ErrKind::Value, s!("a system call is named module.name, got '", str name, "'")))?;
    let caller = with_bridge(|b| b.calls.iter().rev().find(|(id, _)| *id == call_id).map(|(_, m)| m.clone()))
        .ok_or(VmErr::Runtime("a system call names the plugin call it belongs to, see edge_call_id"))?;
    let (spec, id) = HostResolver::new(dir_of(&caller), WebHost).system_fn(module, function)?;
    call_host(id, call_id, &spec, args, None)
}

/* one `host_call_native` remembered by its call id until it answers for the system calls inside */
fn call_host(id: u32, call_id: u32, module: &Arc<str>, args: &[Val], kwargs: Option<Val>) -> Result<Val, VmErr> {
    // positional args become handles the guest sees then the kwargs handle or 0 for none
    let mut argv: Vec<u32> = args.iter().map(|v| put_val(*v)).collect();
    argv.push(kwargs.map_or(0, put_val));
    let mut out_handle: u32 = 0;

    // a system module never makes system calls so only the others are remembered
    let remembered = !module.starts_with("system:");
    if remembered {
        with_bridge(|b| b.calls.push((call_id, module.clone())));
    }
    let status = web::call_native(id, call_id, &argv, &mut out_handle);
    // a waiting call stays remembered until answered since its resumed plugin still calls from it
    if remembered && status != 2 {
        with_bridge(|b| b.calls.pop());
    }

    // status 2 means deferred so the handler kept what it needs and the vm parks
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
    // the result is read before argv is released since a returned input points into it
    let result = get_val(out_handle).ok_or(VmErr::Runtime("native returned invalid handle"))?;
    argv.push(out_handle);
    release_handles(&argv);
    Ok(result)
}
