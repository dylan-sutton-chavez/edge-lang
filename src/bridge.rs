#![allow(clippy::missing_safety_doc)] // safety contract in docs/03-reference/06-abi.mdx, per-fn sections would duplicate it

use crate::abi::{classify_decode, classify_encode, DecodeBits, EncodeRequest, ErrorKind, ErrorStash, HandleTable, Op, PrimitiveBytes, TAG_INVALID};
use crate::vm::types::{DictMap, HeapObj, Val, VmErr};
use crate::vm::methods::lookup_method;
use crate::vm::VM;
use alloc::{rc::Rc, string::{String, ToString}, vec::Vec};
use core::cell::RefCell;
use core::ptr::NonNull;
use crate::s;

/* All bridge state behind one accessor so `with_bridge` is the sole unsafe point. */
pub(crate) struct BridgeState {
    pub handles: HandleTable,
    pub error_stash: ErrorStash,
    /* Set and cleared by `VmGuard` or the paused-run stashes, deref only while its VM lives. */
    pub current_vm: Option<NonNull<VM<'static>>>,
    /* Each native call in flight or waiting, by id, so a system call finds its package. */
    pub calls: Vec<(u32, alloc::sync::Arc<str>)>,
}

impl BridgeState {
    pub(crate) const fn new() -> Self {
        BridgeState { handles: HandleTable::new(), error_stash: ErrorStash::new(), current_vm: None, calls: Vec::new() }
    }
}

// WASM is single-threaded, one process-wide bridge, re-entry routes through `with_vm`.
static mut BRIDGE: BridgeState = BridgeState::new();

pub(crate) fn with_bridge<R>(f: impl FnOnce(&mut BridgeState) -> R) -> R {
    unsafe { f(&mut *core::ptr::addr_of_mut!(BRIDGE)) }
}

pub fn put_val(v: Val) -> u32 { with_bridge(|b| b.handles.put(v.0)) }

/* Marks the values live handles hold, a plugin keeps a value only by handle. */
pub(crate) fn mark_handles(vm: *const u8, heap: &mut crate::vm::types::HeapPool) {
    with_bridge(|b| if b.current_vm.is_some_and(|p| p.as_ptr() as *const u8 == vm) {
        for v in b.handles.live() { heap.mark(Val(v)); }
    });
}
pub fn get_val(h: u32) -> Option<Val> { with_bridge(|b| b.handles.get(h).map(Val)) }

// Release a batch of handles in one bridge borrow.
pub fn release_handles(handles: &[u32]) {
    with_bridge(|b| for &h in handles { b.handles.release(h); });
}

/* Drops handles, stash and VM pointer together, a paused run holding stale handles must go with them. */
pub(crate) fn reset() {
    with_bridge(|b| {
        b.handles.clear();
        b.error_stash.clear();
        b.current_vm = None;
        b.calls.clear();
    });
}

/* Forgets a waiting call once its answer is delivered. */
pub(crate) fn answered(call_id: u32) {
    with_bridge(|b| b.calls.retain(|(id, _)| *id != call_id));
}

pub(crate) fn set_current_vm(ptr: Option<NonNull<VM<'static>>>) {
    with_bridge(|b| b.current_vm = ptr);
}

/* RAII publisher for the live VM pointer, a panic or early return never leaves a stale one. */
pub struct VmGuard;

impl VmGuard {
    pub fn new(vm: &mut VM<'_>) -> Self {
        // 'static is storage-only, deref only inside the `run()` frame holding the guard.
        let ptr: NonNull<VM<'static>> = NonNull::from(vm).cast();
        with_bridge(|b| b.current_vm = Some(ptr));
        Self
    }
}

impl Drop for VmGuard {
    fn drop(&mut self) {
        with_bridge(|b| b.current_vm = None);
    }
}

pub(crate) fn with_vm<R>(f: impl FnOnce(&mut VM<'static>) -> R) -> Option<R> {
    // Drop the bridge borrow before `f`, VM dispatch re-enters `with_bridge`.
    let ptr = with_bridge(|b| b.current_vm)?;
    let vm = unsafe { &mut *ptr.as_ptr() };
    // A value the host builds between runs counts as memory of the program.
    let _paying = crate::value::meter::Paying::to(vm.heap.meter());
    Some(f(vm))
}

/* Builds a `&[u8]` from an FFI `(ptr, len)`, empty on null or zero length, `from_raw_parts` would UB on either. */
pub(crate) unsafe fn safe_bytes<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if ptr.is_null() || len == 0 { return &[]; }
    unsafe { core::slice::from_raw_parts(ptr, len as usize) }
}

/* Same for `&[u32]` argv arrays. */
pub(crate) unsafe fn safe_handles<'a>(ptr: *const u32, len: u32) -> &'a [u32] {
    if ptr.is_null() || len == 0 { return &[]; }
    unsafe { core::slice::from_raw_parts(ptr, len as usize) }
}

/* Owned UTF-8 string from an FFI `(ptr, len)`, empty on null or invalid UTF-8. */
pub(crate) unsafe fn safe_str_owned(ptr: *const u8, len: u32) -> String {
    core::str::from_utf8(unsafe { safe_bytes(ptr, len) }).unwrap_or("").to_string()
}

/* `with_vm` that errors when called outside run(). */
pub(crate) fn in_vm(err: &'static str, f: impl FnOnce(&mut VM<'static>) -> Result<Val, VmErr>) -> Result<Val, VmErr> {
    with_vm(f).ok_or(VmErr::Runtime(err))?
}

/* `dispatch_*` prologue, resolve `recv_h` and run `f` against the live VM. Fails on stale handle or call outside `run()`. */
pub(crate) fn with_recv<F>(invalid_recv_msg: &'static str, recv_h: u32, f: F) -> Result<Val, VmErr>
where F: FnOnce(&mut VM<'static>, Val) -> Result<Val, VmErr>
{ let recv = get_val(recv_h).ok_or(VmErr::Runtime(invalid_recv_msg))?; with_vm(|vm| f(vm, recv)).ok_or(VmErr::Runtime("edge_op called outside run()"))? }

/* VmErr classifier for the ABI boundary. */
pub(crate) fn err_to_kind(e: &VmErr) -> ErrorKind {
    match e {
        VmErr::Type(_) | VmErr::TypeMsg(_) => ErrorKind::Type,
        VmErr::Value(_) => ErrorKind::Value,
        VmErr::Runtime(_) => ErrorKind::Runtime,
        VmErr::Attribute(_) | VmErr::Name(_) => ErrorKind::Attribute,
        VmErr::Raised(s) => {
            if s.starts_with("ValueError") { ErrorKind::Value }
            else if s.starts_with("IndexError") { ErrorKind::Index }
            else if s.starts_with("KeyError") { ErrorKind::Key }
            // Any other class crosses as a custom kind, its name kept in the message.
            else { ErrorKind::Custom }
        }
        _ => ErrorKind::Runtime,
    }
}

pub(crate) fn stash_error(e: VmErr) {
    let kind = err_to_kind(&e);
    let msg = e.render();
    with_bridge(|b| b.error_stash.set_typed(kind, msg));
}

/* Message-only stash for contexts without a `VmErr`, e.g. the WASM panic handler. */
pub(crate) fn stash_raw_error(kind: u32, msg: String) {
    with_bridge(|b| b.error_stash.set(kind, msg));
}

pub fn take_error() -> Option<(u32, String)> {
    with_bridge(|b| b.error_stash.take())
}

/* Inverse of `err_to_kind`, rebuilds a `VmErr` from (kind, msg). Exhaustive over `ErrorKind` so new variants can't slip into `Raised`. */
pub fn error_from_kind(kind: u32, msg: String) -> VmErr {
    match ErrorKind::from_u32(kind) {
        Some(ErrorKind::Type) => VmErr::TypeMsg(msg),
        Some(ErrorKind::Value) => VmErr::Raised(s!("ValueError: ", str &msg)),
        Some(ErrorKind::Runtime) => VmErr::Raised(s!("RuntimeError: ", str &msg)),
        Some(ErrorKind::Attribute) => VmErr::Attribute(msg),
        Some(ErrorKind::Index) => VmErr::Raised(s!("IndexError: ", str &msg)),
        Some(ErrorKind::Key) => VmErr::Raised(s!("KeyError: ", str &msg)),
        // Custom kinds carry the user-defined class name in `msg` (`<ClassName>: <text>`), pass through unchanged.
        Some(ErrorKind::Custom) | None => VmErr::Raised(msg),
    }
}

// Universal dispatch, 0 + handle in `*out_handle`, 1 + stashed error, 2 while a system call waits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_edge_op(op: u32, recv: u32, name_ptr: *const u8, name_len: u32,argv_ptr: *const u32, argc: u32, out_handle: *mut u32) -> i32 {
    let name = unsafe { safe_str_owned(name_ptr, name_len) };
    // A stale argument handle fails the op, dropping it would shift the arguments after it.
    let args: Option<Vec<Val>> = unsafe { safe_handles(argv_ptr, argc) }.iter().map(|&h| get_val(h)).collect();
    let Some(args) = args else { stash_error(VmErr::TypeMsg("edge_op: invalid argument handle".into())); return 1; };

    // The plugin's call waits on what it asks, so Python it runs takes a loop of its own, the one door the rule leaves.
    with_vm(|vm| vm.hosted += 1);
    let result = host_op(op, recv, &name, &args);
    with_vm(|vm| vm.hosted -= 1);

    match result {
        Ok(v) => { unsafe { *out_handle = put_val(v); } 0 }
        // A system call still waits, the plugin returns 2 and the host resumes it later.
        Err(VmErr::HostCallDeferred) => 2,
        Err(e) => { stash_error(e); 1 }
    }
}

/* What `host_edge_op` asks of the VM. */
fn host_op(op: u32, recv: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    match Op::from_u32(op) {
        Some(Op::Call) => dispatch_call(recv, name, args),
        Some(Op::GetAttr) => dispatch_get_attr(recv, name),
        Some(Op::SetAttr) => dispatch_set_attr(recv, name, args),
        Some(Op::GetItem) => dispatch_get_item(recv, args),
        Some(Op::SetItem) => dispatch_set_item(recv, args),
        Some(Op::Len) => dispatch_len(recv),
        Some(Op::Iter) => dispatch_iter(recv),
        Some(Op::IterNext) => dispatch_iter_next(recv),
        Some(Op::NewDict) => in_vm("edge_op new_dict called outside run()", |vm| vm.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(DictMap::new()))))),
        Some(Op::NewList) => in_vm("edge_op new_list called outside run()", |vm| vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(Vec::new()))))),
        Some(Op::TypeOf) => dispatch_type_of(recv),
        Some(Op::NewTuple) => in_vm("edge_op new_tuple called outside run()", |vm| vm.tuple_from_items(args.to_vec())),
        Some(Op::NewSet) => in_vm("edge_op new_set called outside run()", |vm| vm.set_from_items(args.to_vec())),
        Some(Op::NewFrozenSet) => in_vm("edge_op new_frozenset called outside run()", |vm| vm.frozenset_from_items(args.to_vec())),
        // A system call names its plugin call in `recv`, the id `edge_call_id` gave it.
        Some(Op::Sys) => crate::wasm::system_call(recv, name, args),
        None => Err(VmErr::Raised(s!("edge_op: unsupported op ", int op as i64))),
    }
}

fn dispatch_call(recv_h: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    with_recv("edge_op call: invalid receiver handle", recv_h, |vm, recv| {
        // `__call__` invokes `recv` itself, so any callable a plugin holds takes the normal dispatch path.
        if name == "__call__" {
            let chunk = vm.chunk;
            return take_result(vm, "edge_op call(__call__): callable left no result", |vm| vm.call_with(recv, None, args, &[], chunk));
        }
        let ty = vm.type_name(recv);
        let mid = lookup_method(ty, name).ok_or_else(|| VmErr::Attribute(s!("'", str ty, "' object has no method '", str name, "'")))?;
        // The path a script call takes, so `str.format` or `list.sort` gets the frame its user code runs in.
        let chunk = vm.chunk;
        take_result(vm, "edge_op call: method left no result", |vm| vm.exec_bound_method(recv, mid, args, &[], chunk))
    })
}

/* Runs `f` frameless, a hook has no frame, and pops the one value it leaves. */
fn take_result(vm: &mut VM<'static>, what: &'static str, f: impl FnOnce(&mut VM<'static>) -> Result<(), VmErr>) -> Result<Val, VmErr> {
    let before = vm.stack.len();
    f(vm)?;
    if vm.stack.len() != before + 1 { return Err(VmErr::Runtime(what)); }
    vm.stack.pop().ok_or(VmErr::Runtime(what))
}

/* GetAttr reads like `recv.name`. */
fn dispatch_get_attr(recv_h: u32, name: &str) -> Result<Val, VmErr> {
    with_recv("edge_op get_attr: invalid receiver handle", recv_h, |vm, recv| {
        let chunk = vm.chunk;
        take_result(vm, "edge_op get_attr: lookup left no result", |vm| vm.load_attr(recv, name, chunk))
    })
}

/* SetAttr writes like `recv.name = value`. */
fn dispatch_set_attr(recv_h: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    let &[value] = args else {
        return Err(VmErr::TypeMsg(s!("set_attr expects exactly 1 value, got ", int args.len() as i64)));
    };
    with_recv("edge_op set_attr: invalid receiver handle", recv_h, |vm, recv| {
        let chunk = vm.chunk;
        vm.store_attr(recv, name, value, chunk)?;
        Ok(Val::none())
    })
}

/* GetItem, built-in indexing only, FFI has no bytecode frame to drive instance `__getitem__` dispatch. */
fn dispatch_get_item(recv_h: u32, args: &[Val]) -> Result<Val, VmErr> {
    if args.len() != 1 {
        return Err(VmErr::TypeMsg(s!("get_item expects 1 index, got ", int args.len() as i64)));
    }
    let idx = args[0];
    with_recv("edge_op get_item: invalid receiver handle", recv_h, |vm, recv| {
        take_result(vm, "edge_op get_item: get_item left no result", |vm| vm.get_item_builtin(recv, idx))
    })
}

/* SetItem, built-in item-assignment only, same rationale as `dispatch_get_item`. */
fn dispatch_set_item(recv_h: u32, args: &[Val]) -> Result<Val, VmErr> {
    if args.len() != 2 {
        return Err(VmErr::TypeMsg(s!("set_item expects (index, value), got ", int args.len() as i64, " args")));
    }
    let idx = args[0];
    let value = args[1];
    with_recv("edge_op set_item: invalid receiver handle", recv_h, |vm, recv| {
        vm.store_item_builtin(recv, idx, value)?;
        Ok(Val::none())
    })
}

/* Len runs the VM `len` builtin, so its caps and charges apply. */
fn dispatch_len(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op len: invalid receiver handle", recv_h, |vm, recv| {
        let n = vm.builtin_len(recv)?;
        vm.int_to_val(Some(n))
    })
}

/* Iter, flatten any iterable into a List for guest GetItem/Len access. */
fn dispatch_iter(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op iter: invalid receiver handle", recv_h, |vm, recv| {
        let items = vm.extract_iter(recv)?;
        vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))
    })
}

/* IterNext pops the head of the list Op::Iter made or steps a builtin iterator, StopIteration once spent. */
fn dispatch_iter_next(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op iter_next: invalid receiver handle", recv_h, |vm, recv| {
        if matches!(vm.heap.try_get(recv), Some(HeapObj::Iter(..))) {
            vm.iter_step(recv)?.ok_or_else(|| VmErr::Raised(s!("StopIteration")))
        } else if let Some(HeapObj::List(rc)) = vm.heap.try_get(recv) {
            let mut v = rc.borrow_mut();
            if v.is_empty() { return Err(VmErr::Raised(s!("StopIteration"))); }
            Ok(v.remove(0))
        } else {
            Err(VmErr::TypeMsg(s!("iter_next expects a List iterator (produced by Op::Iter), got '", str vm.type_name(recv), "'")))
        }
    })
}

fn dispatch_type_of(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op type_of: invalid receiver handle", recv_h, |vm, recv| {
        let name = vm.type_name(recv).to_string();
        vm.heap.alloc(HeapObj::Str(name))
    })
}

// Bootstrap encoder, classifies (tag, bytes) into a Val handle, returns 0 on Invalid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_edge_encode(tag: u32, ptr: *const u8, len: u32) -> u32 {
    // Alloc in the live VM, 0 when outside run() or OOM.
    fn alloc_and_put(obj: HeapObj) -> u32 {
        match with_vm(|vm| vm.heap.alloc(obj).ok()).flatten() {
            Some(val) => put_val(val),
            None => 0,
        }
    }
    let bytes = unsafe { safe_bytes(ptr, len) };
    match classify_encode(tag, bytes) {
        EncodeRequest::Direct(bits) => put_val(Val(bits)),
        EncodeRequest::AllocStr(s) => alloc_and_put(HeapObj::Str(s.to_string())),
        EncodeRequest::AllocBytes(b) => alloc_and_put(HeapObj::Bytes(b.to_vec())),
        EncodeRequest::AllocInt(i) => match with_vm(|vm| vm.heap.int(i).ok()).flatten() { Some(val) => put_val(val), None => 0 },
        EncodeRequest::Composite(w) => {
            match with_vm(|vm| wire_to_val(vm, &w).ok()).flatten() {
                Some(val) => put_val(val),
                None => 0,
            }
        }
        EncodeRequest::Invalid => 0,
    }
}

/* Wire tree to heap value. Str keys only for dicts, mirrors `classify_encode` inline-int split. */
fn wire_to_val(vm: &mut crate::vm::VM, w: &crate::abi::WireValue) -> Result<Val, VmErr> {
    use crate::abi::WireValue;
    Ok(match w {
        WireValue::None => Val::none(),
        WireValue::Bool(b) => Val::bool(*b),
        WireValue::Int(i) => vm.heap.int(*i)?,
        WireValue::Float(f) => Val::float(*f),
        WireValue::Bytes(b) => {
            let s = core::str::from_utf8(b).map_err(|_| VmErr::Value("invalid UTF-8 in wire str"))?;
            vm.heap.alloc(HeapObj::Str(s.to_string()))?
        }
        WireValue::Raw(b) => vm.heap.alloc(HeapObj::Bytes(b.clone()))?,
        WireValue::List(items) => {
            let vals = items.iter().map(|it| wire_to_val(vm, it)).collect::<Result<Vec<_>, _>>()?;
            vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(vals))))?
        }
        WireValue::Dict(pairs) => {
            let mut map = DictMap::new();
            for (k, v) in pairs {
                let WireValue::Bytes(kb) = k else { return Err(VmErr::Type("wire dict keys must be str")); };
                let ks = core::str::from_utf8(kb).map_err(|_| VmErr::Value("invalid UTF-8 in wire str"))?;
                let key = vm.heap.alloc(HeapObj::Str(ks.to_string()))?;
                let val = wire_to_val(vm, v)?;
                map.insert(key, val, &vm.heap);
            }
            vm.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(map))))?
        }
    })
}

/* A FOREIGN value as a wire int, None past i128 or for a kind that never crosses. */
fn foreign_int(vm: &crate::vm::VM, v: Val) -> Option<i128> {
    vm.heap.foreign_kind(v).filter(|k| k.transfer == crate::value::foreign::Transfer::Copy)?;
    vm.heap.foreign_i128(v)
}

/* Heap value to wire tree. `None` on cycles, depth past the cap, or non-transit members. */
fn val_to_wire(vm: &crate::vm::VM, v: Val, depth: u32, seen: &mut Vec<u64>) -> Option<crate::abi::WireValue> {
    use crate::abi::{DecodeBits as DB, PrimitiveBytes as PB, WireValue, MAX_WIRE_DEPTH};
    if depth > MAX_WIRE_DEPTH { return None; }
    match classify_decode(v.0) {
        DB::Primitive { tag, bytes } => {
            let buf: Vec<u8> = match bytes {
                PB::None => Vec::new(),
                PB::Bool(b) => alloc::vec![b],
                PB::Eight(a) => a.to_vec(),
                PB::Sixteen(a) => a.to_vec(),
            };
            WireValue::decode_body(tag, &buf)
        }
        DB::Heap => {
            // Shared references are fine, re-entering a value mid-walk is a cycle.
            let guard = |vm: &crate::vm::VM, seen: &mut Vec<u64>, items: &[Val]| -> Option<Vec<crate::abi::WireValue>> {
                items.iter().map(|it| val_to_wire(vm, *it, depth + 1, seen)).collect()
            };
            if seen.contains(&v.0) { return None; }
            if v.is_foreign() { return foreign_int(vm, v).map(WireValue::Int); }
            match vm.heap.get(v) {
                HeapObj::Str(s) => Some(WireValue::Bytes(s.as_bytes().to_vec())),
                HeapObj::Bytes(b) => Some(WireValue::Raw(b.clone())),
                HeapObj::List(rc) => {
                    seen.push(v.0);
                    let items = guard(vm, seen, &rc.borrow())?;
                    seen.pop();
                    Some(WireValue::List(items))
                }
                HeapObj::Tuple(t) => {
                    seen.push(v.0);
                    let items = guard(vm, seen, t)?;
                    seen.pop();
                    Some(WireValue::List(items))
                }
                HeapObj::Dict(rc) | HeapObj::FrozenDict(rc) => {
                    seen.push(v.0);
                    let mut pairs = Vec::new();
                    for (k, val) in rc.borrow().iter() {
                        let key = match val_to_wire(vm, k, depth + 1, seen)? {
                            key @ WireValue::Bytes(_) => key,
                            _ => return None,
                        };
                        pairs.push((key, val_to_wire(vm, val, depth + 1, seen)?));
                    }
                    seen.pop();
                    Some(WireValue::Dict(pairs))
                }
                _ => None,
            }
        }
        DB::Invalid => None,
    }
}

// Bootstrap decoder, writes tag to `*out_tag`, bytes to `dst[..dst_max]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_edge_decode(h: u32, out_tag: *mut u32, dst: *mut u8, dst_max: u32) -> i32 {
    let copy_into = |tag: u32, bytes: &[u8]| -> i32 {
        unsafe { *out_tag = tag; }
        if bytes.len() > dst_max as usize { return -(bytes.len() as i32); }
        if !bytes.is_empty() {
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            }
        }
        bytes.len() as i32
    };

    let v = match get_val(h) {
        Some(v) => v,
        None => { unsafe { *out_tag = TAG_INVALID; } return 0; }
    };

    match classify_decode(v.0) {
        DecodeBits::Primitive { tag, bytes } => match bytes {
            PrimitiveBytes::None => copy_into(tag, &[]),
            PrimitiveBytes::Bool(b) => copy_into(tag, &[b]),
            PrimitiveBytes::Eight(a) => copy_into(tag, &a),
            PrimitiveBytes::Sixteen(a) => copy_into(tag, &a),
        },
        DecodeBits::Heap => {
            // Str, Bytes, ints and TLV composites decode, sets, instances and other non-transit values go through `edge_op`.
            enum Decoded { Str(alloc::string::String), Bytes(Vec<u8>), Int(i128), Wire(u32, Vec<u8>), Other }
            let decoded = with_vm(|vm| if v.is_foreign() { foreign_int(vm, v).map_or(Decoded::Other, Decoded::Int) } else { match vm.heap.get(v) {
                HeapObj::Str(s) => Decoded::Str(s.clone()),
                HeapObj::Bytes(b) => Decoded::Bytes(b.clone()),
                HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::FrozenDict(_) => {
                    match val_to_wire(vm, v, 0, &mut Vec::new()) {
                        Some(w) => {
                            let mut buf = Vec::new();
                            w.encode_body(&mut buf);
                            Decoded::Wire(w.tag(), buf)
                        }
                        None => Decoded::Other,
                    }
                }
                _ => Decoded::Other,
            } }).unwrap_or(Decoded::Other);
            match decoded {
                Decoded::Str(s) => copy_into(crate::abi::Tag::Bytes as u32, s.as_bytes()),
                Decoded::Bytes(b) => copy_into(crate::abi::Tag::Raw as u32, &b),
                Decoded::Int(i) => copy_into(crate::abi::Tag::Int as u32, &i.to_le_bytes()),
                Decoded::Wire(tag, buf) => copy_into(tag, &buf),
                Decoded::Other => { unsafe { *out_tag = TAG_INVALID; } 0 }
            }
        }
        DecodeBits::Invalid => { unsafe { *out_tag = TAG_INVALID; } 0 }
    }
}

// Decrement refcount on a handle. No-op for invalid handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_edge_release(h: u32) {
    with_bridge(|b| b.handles.release(h));
}

// Stash a guest error for the host. Overwrites any pending error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_edge_throw(kind: u32, msg_ptr: *const u8, msg_len: u32) {
    let msg = unsafe { safe_str_owned(msg_ptr, msg_len) };
    with_bridge(|b| b.error_stash.set(kind, msg));
}

// Drain the most recent error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_edge_take_error(out_kind: *mut u32, dst: *mut u8, dst_max: u32) -> i32 {
    // Peek first so buffer-too-small callers can retry.
    let (kind, len) = match with_bridge(|b| b.error_stash.peek().map(|(k, m)| (k, m.len()))) {
        Some(p) => p,
        None => return -1,
    };
    if len > dst_max as usize { return -(len as i32); }
    // Buffer fits, drain and copy, a None from `take()` is a lost peek race, never a panic.
    let Some((_, msg)) = with_bridge(|b| b.error_stash.take()) else { return -1; };
    let bytes = msg.as_bytes();
    unsafe {
        *out_kind = kind;
        if !bytes.is_empty() {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
    }
    bytes.len() as i32
}
