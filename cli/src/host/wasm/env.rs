use super::{read, read_u32, rt, stage, unstage, write, write_u32, Exports, Native, Plugin, State};
use crate::host::{ErrorKind, Fault};
use anyhow::{anyhow, Result};
use edge::abi::WireValue;
use wasmtime::{AsContextMut, Caller, Linker};

/* the `env` imports edge.wasm declares bound to the store state */
pub fn link(linker: &mut Linker<State>) -> Result<()> {
    linker
        .func_wrap("env", "host_print", |mut caller: Caller<'_, State>, ptr: i32, len: i32| {
            let ex = exports(&caller);
            let text = String::from_utf8_lossy(&read(&mut caller, ex.memory, ptr, len)).into_owned();
            (caller.data_mut().hosting.print)(&text);
        })
        .map_err(|e| anyhow!("{e}"))?;
    // the walk already gave the runtime every manifest so nothing more answers here
    linker
        .func_wrap("env", "host_fetch_bytes", |mut caller: Caller<'_, State>, _spec_ptr: i32, _spec_len: i32, _hash_ptr: i32, out_len: i32| -> wasmtime::Result<i32> {
            let ex = exports(&caller);
            write_u32(&mut caller, ex.memory, out_len, 0);
            Ok(0)
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "host_send", |mut caller: Caller<'_, State>, group_ptr: i32, group_len: i32, body_ptr: i32, body_len: i32| -> i32 {
            let ex = exports(&caller);
            let group = String::from_utf8_lossy(&read(&mut caller, ex.memory, group_ptr, group_len)).into_owned();
            let body = String::from_utf8_lossy(&read(&mut caller, ex.memory, body_ptr, body_len)).into_owned();
            // only a file channel of a run with packages asks which of them sends
            let dir = if caller.data().hosting.asks_sender(&group) { sender(&mut caller, &ex) } else { None };
            // a refused send raises the fault the runtime finds stashed when it sees 2
            match caller.data_mut().hosting.deliver(group, body, dir) {
                Ok(true) => 0,
                Ok(false) => 1,
                Err(fault) => {
                    throw(&mut caller, &ex, &fault);
                    2
                }
            }
        })
        .map_err(|e| anyhow!("{e}"))?;
    linker
        .func_wrap("env", "host_call_native", |mut caller: Caller<'_, State>, id: i32, call_id: i32, argv_ptr: i32, argc: i32, out_ptr: i32| -> wasmtime::Result<i32> {
            call_native(&mut caller, id, call_id, argv_ptr, argc, out_ptr)
        })
        .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

fn exports(caller: &Caller<'_, State>) -> Exports {
    caller.data().exports.clone().expect("runtime exports bound before any host call")
}

/* dispatches one extern call with staged argv for plugins and decoded values for host modules */
fn call_native(caller: &mut Caller<'_, State>, id: i32, call_id: i32, argv_ptr: i32, argc: i32, out_ptr: i32) -> wasmtime::Result<i32> {
    let ex = exports(caller);
    let Some(native) = caller.data().natives.get(id as usize).cloned() else {
        throw(caller, &ex, &Fault::runtime(format!("native id {id} not registered")));
        return Ok(1);
    };
    match native {
        Native::Plugin(plugin) => {
            let argv = read(caller, ex.memory, argv_ptr, argc * 4);
            let len = (argc * 4).max(4);
            // an allocator trap fails the call like a method trap and never the interpreter
            let staged = plugin.alloc.call(&mut *caller, len).and_then(|a| Ok((a, plugin.alloc.call(&mut *caller, 4)?)));
            let (g_argv, g_out) = match staged {
                Ok(pair) => pair,
                Err(e) => {
                    throw(caller, &ex, &trapped(&e));
                    return Ok(1);
                }
            };
            write(caller, plugin.memory, g_argv, &argv);
            caller.data_mut().running.push(call_id as u32);
            let called = plugin.func.call(&mut *caller, (g_argv, argc, g_out));
            caller.data_mut().running.pop();
            let mut status = match called {
                Ok(status) => status,
                Err(e) => {
                    throw(caller, &ex, &trapped(&e));
                    return Ok(1);
                }
            };
            if status == 0 {
                let handle = read_u32(caller, plugin.memory, g_out);
                write_u32(caller, ex.memory, out_ptr, handle);
            }
            if let Some(free) = &plugin.free {
                let _ = free.call(&mut *caller, (g_argv, len));
                let _ = free.call(&mut *caller, (g_out, 4));
            }
            // a plugin waiting on a host call finishes in its resume export once that settles
            if status == 2 && plugin.resume.is_none() {
                throw(caller, &ex, &Fault::runtime("native module waits on a system call but exports no __edge_resume"));
                status = 1;
            }
            if status == 2 {
                caller.data_mut().waiting.insert(call_id as u32, plugin);
            }
            Ok(status)
        }
        Native::Host(hosted) => {
            let raw = read(caller, ex.memory, argv_ptr, argc.max(1) * 4);
            let handles: Vec<u32> = raw.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
            // the trailing slot holds the kwargs a host module call never takes
            let Some((&0, positional)) = handles.split_last() else {
                throw(caller, &ex, &Fault::new(ErrorKind::Type, format!("{} takes positional arguments only", hosted.name)));
                return Ok(1);
            };
            let mut args = Vec::with_capacity(positional.len());
            for &handle in positional {
                match rt::decode(caller, &ex, handle) {
                    Ok(value) => args.push(value),
                    Err(e) => {
                        throw(caller, &ex, &Fault::runtime(e));
                        return Ok(1);
                    }
                }
            }
            let answer = caller.data_mut().hosting.call(&hosted, call_id as u32, args);
            Ok(answered(caller, &ex, out_ptr, answer))
        }
    }
}

/* answers the runtime with a value now or a park until later or an error */
fn answered(caller: &mut Caller<'_, State>, ex: &Exports, out_ptr: i32, answer: Result<Option<WireValue>, Fault>) -> i32 {
    match answer.and_then(|value| value.map(|v| rt::encode(caller, ex, &v).map_err(Fault::runtime)).transpose()) {
        Ok(Some(handle)) => {
            write_u32(caller, ex.memory, out_ptr, handle);
            0
        }
        Ok(None) => 2,
        Err(fault) => {
            throw(caller, ex, &fault);
            1
        }
    }
}

/* hands a settled host call to its waiting plugin then delivers what the plugin answers */
pub(super) fn resume(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, id: u32, plugin: Box<Plugin>, settled: Result<WireValue, Fault>) -> wasmtime::Result<i32> {
    let Some(resume) = plugin.resume.clone() else { return Ok(2) };
    // a failed call reaches the plugin as a zero handle with its error stashed
    let answer = match settled {
        Ok(value) => rt::encode(cx, ex, &value).map_err(|e| wasmtime::format_err!("{e}"))? as i32,
        Err(fault) => {
            throw(cx, ex, &fault);
            0
        }
    };
    let out = match plugin.alloc.call(&mut *cx, 4) {
        Ok(out) => out,
        Err(e) => {
            throw(cx, ex, &trapped(&e));
            return fail_parked(cx, ex, id);
        }
    };
    cx.as_context_mut().data_mut().running.push(id);
    let called = resume.call(&mut *cx, (id as i32, answer, out));
    cx.as_context_mut().data_mut().running.pop();
    let status = called.unwrap_or_else(|e| {
        throw(cx, ex, &trapped(&e));
        1
    });
    let handle = read_u32(cx, plugin.memory, out);
    if let Some(free) = &plugin.free {
        let _ = free.call(&mut *cx, (out, 4));
    }
    match status {
        0 => ex.set_host_result_by_id.call(&mut *cx, (id as i32, handle as i32)),
        // still waiting and the host call it just made settles on the same id
        2 => {
            cx.as_context_mut().data_mut().waiting.insert(id, plugin);
            Ok(0)
        }
        _ => fail_parked(cx, ex, id),
    }
}

/* raises the error a plugin stashed in the call parked on `id` */
fn fail_parked(cx: &mut impl AsContextMut<Data = State>, ex: &Exports, id: u32) -> wasmtime::Result<i32> {
    let (kind, msg) = take_error(cx, ex)?;
    let handle = rt::encode(cx, ex, &WireValue::Bytes(msg.into_bytes())).map_err(|e| wasmtime::format_err!("{e}"))?;
    ex.set_host_error_by_id.call(&mut *cx, (id as i32, kind, handle as i32))
}

/* the kind and message of the error a plugin stashed */
fn take_error(cx: &mut impl AsContextMut<Data = State>, ex: &Exports) -> wasmtime::Result<(i32, String)> {
    let mut size = 256;
    loop {
        let kind = ex.wasm_alloc.call(&mut *cx, 4)?;
        let buf = ex.wasm_alloc.call(&mut *cx, size)?;
        let got = ex.host_edge_take_error.call(&mut *cx, (kind, buf, size))?;
        let taken = (read_u32(cx, ex.memory, kind) as i32, String::from_utf8_lossy(&read(cx, ex.memory, buf, got.max(0))).into_owned());
        let _ = ex.wasm_free.call(&mut *cx, (kind, 4));
        let _ = ex.wasm_free.call(&mut *cx, (buf, size));
        match got {
            n if n >= 0 => return Ok(taken),
            -1 => return Ok((ErrorKind::Runtime as i32, "native call failed".to_string())),
            n => size = -n,
        }
    }
}

/* what a plugin trap tells the program without the wasm backtrace */
fn trapped(e: &wasmtime::Error) -> Fault {
    Fault::runtime(match e.downcast_ref::<wasmtime::Trap>() {
        Some(trap) => format!("native module trapped: {trap}"),
        None => format!("native module trapped: {e}"),
    })
}

/* stashes the error the runtime raises once the call returns 1 */
pub(super) fn throw(caller: &mut impl AsContextMut<Data = State>, ex: &Exports, fault: &Fault) {
    let message = fault.message(caller.as_context_mut().data().hosting.descriptor);
    let text = message.as_bytes();
    if let Ok(ptr) = stage(caller, ex, text) {
        let _ = ex.host_edge_throw.call(&mut *caller, (fault.kind as i32, ptr, text.len() as i32));
        unstage(caller, ex, ptr, text.len());
    }
}

/* the package dir of the code sending now as the runtime names it */
fn sender(caller: &mut Caller<'_, State>, ex: &Exports) -> Option<String> {
    let asked = ex.send_package.as_ref()?.call(&mut *caller, ()).ok()?;
    let at = ex.out_ptr.call(&mut *caller, ()).ok()?;
    Some(String::from_utf8_lossy(&read(caller, ex.memory, at, asked)).into_owned())
}
