use super::{read, read_u32, stage, unstage, write, write_u32, wt, Exports, Instance, Native, Plugin, State};
use anyhow::{anyhow, Result};
use edge::abi::EDGE_ABI_VERSION;
use wasmtime::{Caller, ExternType, InstancePre, Linker, Memory, TypedFunc};

/* the seven `env` imports a plugin declares each bridging guest memory to the runtime exports */
pub fn link(linker: &mut Linker<State>) -> Result<()> {
    linker
        .func_wrap("env", "edge_op", |mut caller: Caller<'_, State>, op: i32, recv: i32, name_ptr: i32, name_len: i32, argv_ptr: i32, argc: i32, out: i32| -> wasmtime::Result<i32> {
            let ex = exports(&caller);
            let guest = guest_memory(&mut caller)?;
            let name = read(&mut caller, guest, name_ptr, name_len);
            let argv = read(&mut caller, guest, argv_ptr, argc * 4);
            let c_name = stage(&mut caller, &ex, &name)?;
            let argv_len = (argc * 4).max(4);
            let c_argv = ex.wasm_alloc.call(&mut caller, argv_len)?;
            write(&mut caller, ex.memory, c_argv, &argv);
            let c_out = ex.wasm_alloc.call(&mut caller, 4)?;
            let ret = ex.host_edge_op.call(&mut caller, (op, recv, c_name, name_len, c_argv, argc, c_out))?;
            if ret == 0 && out != 0 {
                let handle = read_u32(&mut caller, ex.memory, c_out);
                write_u32(&mut caller, guest, out, handle);
            }
            unstage(&mut caller, &ex, c_name, name.len());
            let _ = ex.wasm_free.call(&mut caller, (c_argv, argv_len));
            let _ = ex.wasm_free.call(&mut caller, (c_out, 4));
            Ok(ret)
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "edge_encode", |mut caller: Caller<'_, State>, tag: i32, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let ex = exports(&caller);
            let guest = guest_memory(&mut caller)?;
            let bytes = read(&mut caller, guest, ptr, len);
            let c = stage(&mut caller, &ex, &bytes)?;
            let handle = ex.host_edge_encode.call(&mut caller, (tag, c, len))?;
            unstage(&mut caller, &ex, c, bytes.len());
            Ok(handle)
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "edge_decode", |mut caller: Caller<'_, State>, handle: i32, out_tag: i32, dst: i32, dst_max: i32| -> wasmtime::Result<i32> {
            let ex = exports(&caller);
            let guest = guest_memory(&mut caller)?;
            let buf_len = dst_max.max(1);
            let c_tag = ex.wasm_alloc.call(&mut caller, 4)?;
            let c_buf = ex.wasm_alloc.call(&mut caller, buf_len)?;
            let ret = ex.host_edge_decode.call(&mut caller, (handle, c_tag, c_buf, dst_max))?;
            let tag = read_u32(&mut caller, ex.memory, c_tag);
            write_u32(&mut caller, guest, out_tag, tag);
            if ret > 0 {
                let bytes = read(&mut caller, ex.memory, c_buf, ret);
                write(&mut caller, guest, dst, &bytes);
            }
            let _ = ex.wasm_free.call(&mut caller, (c_tag, 4));
            let _ = ex.wasm_free.call(&mut caller, (c_buf, buf_len));
            Ok(ret)
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "edge_release", |mut caller: Caller<'_, State>, handle: i32| -> wasmtime::Result<()> {
            let ex = exports(&caller);
            ex.host_edge_release.call(&mut caller, handle)
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "edge_call_id", |caller: Caller<'_, State>| -> i32 { caller.data().running.last().copied().unwrap_or(0) as i32 })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "edge_throw", |mut caller: Caller<'_, State>, kind: i32, ptr: i32, len: i32| -> wasmtime::Result<()> {
            let ex = exports(&caller);
            let guest = guest_memory(&mut caller)?;
            let bytes = read(&mut caller, guest, ptr, len);
            let c = stage(&mut caller, &ex, &bytes)?;
            ex.host_edge_throw.call(&mut caller, (kind, c, len))?;
            unstage(&mut caller, &ex, c, bytes.len());
            Ok(())
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "edge_take_error", |mut caller: Caller<'_, State>, out_kind: i32, dst: i32, dst_max: i32| -> wasmtime::Result<i32> {
            let ex = exports(&caller);
            let guest = guest_memory(&mut caller)?;
            let buf_len = dst_max.max(1);
            let c_kind = ex.wasm_alloc.call(&mut caller, 4)?;
            let c_buf = ex.wasm_alloc.call(&mut caller, buf_len)?;
            let ret = ex.host_edge_take_error.call(&mut caller, (c_kind, c_buf, dst_max))?;
            if ret >= 0 {
                let kind = read_u32(&mut caller, ex.memory, c_kind);
                write_u32(&mut caller, guest, out_kind, kind);
                if ret > 0 {
                    let bytes = read(&mut caller, ex.memory, c_buf, ret);
                    write(&mut caller, guest, dst, &bytes);
                }
            }
            let _ = ex.wasm_free.call(&mut caller, (c_kind, 4));
            let _ = ex.wasm_free.call(&mut caller, (c_buf, buf_len));
            Ok(ret)
        })
        .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

fn exports(caller: &Caller<'_, State>) -> Exports {
    caller.data().exports.clone().expect("runtime exports bound before any plugin call")
}

fn guest_memory(caller: &mut Caller<'_, State>) -> wasmtime::Result<Memory> {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::format_err!("plugin exports no memory"))
}

/* instantiates a third party plugin once per instance or says why it does not load */
pub fn register_bytes(inst: &mut Instance, spec: &str, bytes: &[u8]) -> Result<(), String> {
    if let Some((base, names)) = inst.store.data().hosting.registered.get(spec).cloned() {
        return inst.register_native(spec, &names, base);
    }
    let host = inst.host.clone();
    let pre = host.third_party(bytes).map_err(|e| headline(&e).to_string())?;
    register_pre(inst, spec, &pre)
}

/* the first line of a wasmtime error cut before the dangling byte dump after it */
fn headline(e: &str) -> &str {
    let line = e.lines().next().unwrap_or(e);
    line.rsplit_once(" - ").filter(|(_, tail)| tail.ends_with('[')).map_or(line, |(head, _)| head)
}

/* instantiates a linked plugin in the instance store and registers its exports under `spec` */
pub fn register_pre(inst: &mut Instance, spec: &str, pre: &InstancePre<State>) -> Result<(), String> {
    let instance = wt(pre.instantiate(&mut inst.store)).map_err(|e| format!("it fails to instantiate, {e}"))?;
    let memory = instance.get_memory(&mut inst.store, "memory").ok_or_else(|| String::from("it exports no memory"))?;
    let version: TypedFunc<(), i32> = wt(instance.get_typed_func(&mut inst.store, "__edge_abi_version")).map_err(|e| e.to_string())?;
    let got = wt(version.call(&mut inst.store, ())).map_err(|e| e.to_string())?;
    if got != EDGE_ABI_VERSION as i32 {
        return Err(format!("it speaks ABI v{got}, this cli expects v{EDGE_ABI_VERSION}"));
    }
    let alloc: TypedFunc<i32, i32> = wt(instance.get_typed_func(&mut inst.store, "__edge_alloc")).map_err(|e| e.to_string())?;
    let free: Option<TypedFunc<(i32, i32), ()>> = instance.get_typed_func(&mut inst.store, "__edge_free").ok();
    let resume: Option<TypedFunc<(i32, i32, i32), i32>> = instance.get_typed_func(&mut inst.store, "__edge_resume").ok();
    let base = inst.store.data().natives.len();
    let mut names = Vec::new();
    for export in pre.module().exports() {
        let n = export.name();
        if !matches!(export.ty(), ExternType::Func(_)) || n == "memory" {
            continue;
        }
        // convention exports carry the module surface while other `__` names are ABI internals
        if n.starts_with("__") && !(n.starts_with("__fn_") || n.starts_with("__class_") || n.starts_with("__const_")) {
            continue;
        }
        let Ok(func) = instance.get_typed_func::<(i32, i32, i32), i32>(&mut inst.store, n) else { continue };
        names.push(n.to_string());
        inst.store.data_mut().natives.push(Native::Plugin(Box::new(Plugin { func, alloc: alloc.clone(), free: free.clone(), memory, resume: resume.clone() })));
    }
    inst.store.data_mut().hosting.registered.insert(spec.to_string(), (base, names.clone()));
    inst.register_native(spec, &names, base)
}
