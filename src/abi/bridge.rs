use crate::abi::{classify_decode, classify_encode, DecodeBits, EncodeRequest, ErrorKind, ErrorStash, HandleTable, Op, PrimitiveBytes, TAG_INVALID};
use crate::lang::{ErrKind, Mode, Names};
use crate::value::{DictMap, HeapObj, Host, Val, VmErr};
use crate::vm::VM;
use crate::r#unsafe::{vm_mut, Single};
use alloc::{rc::Rc, string::{String, ToString}, vec::Vec};
use core::cell::RefCell;
use core::ptr::NonNull;
use crate::s;

/* all bridge state behind one accessor that lends the static once at a time */
pub(crate) struct BridgeState {
    pub handles: HandleTable,
    pub error_stash: ErrorStash,
    /* set by `VmGuard` or a paused run's stash and read only while its VM lives */
    pub current_vm: Option<NonNull<VM<'static>>>,
    /* each native call in flight or waiting by id so system calls find their package */
    pub calls: Vec<(u32, alloc::sync::Arc<str>)>,
}

impl BridgeState {
    pub(crate) const fn new() -> Self {
        BridgeState { handles: HandleTable::new(), error_stash: ErrorStash::new(), current_vm: None, calls: Vec::new() }
    }
}

// wasm runs one thread so one bridge serves the process and re-entry goes through `with_vm`
static BRIDGE: Single<BridgeState> = Single::new(BridgeState::new());

pub(crate) fn with_bridge<R>(f: impl FnOnce(&mut BridgeState) -> R) -> R { BRIDGE.with(f) }

pub fn put_val(v: Val) -> u32 { with_bridge(|b| b.handles.put(v.0)) }

/* marks the values live handles hold since a plugin keeps a value only by handle */
pub(crate) fn mark_handles(vm: *const u8, heap: &mut crate::value::HeapPool) {
    with_bridge(|b| if b.current_vm.is_some_and(|p| p.as_ptr() as *const u8 == vm) {
        for v in b.handles.live() { heap.mark(Val(v)); }
    });
}
pub fn get_val(h: u32) -> Option<Val> { with_bridge(|b| b.handles.get(h).map(Val)) }

// releases a batch of handles in one bridge borrow
pub fn release_handles(handles: &[u32]) {
    with_bridge(|b| for &h in handles { b.handles.release(h); });
}

/* drops handles stash and VM pointer together since stale handles go with a paused run */
pub(crate) fn reset() {
    with_bridge(|b| {
        b.handles.clear();
        b.error_stash.clear();
        b.current_vm = None;
        b.calls.clear();
    });
}

/* forgets a waiting call once its answer is delivered */
pub(crate) fn answered(call_id: u32) {
    with_bridge(|b| b.calls.retain(|(id, _)| *id != call_id));
}

pub(crate) fn set_current_vm(ptr: Option<NonNull<VM<'static>>>) {
    with_bridge(|b| b.current_vm = ptr);
}

/* publishes the live VM pointer so no panic or early return leaves a stale one */
pub struct VmGuard;

impl VmGuard {
    pub fn new(vm: &mut VM<'_>) -> Self {
        // the static lifetime is storage only and a deref happens in the guarded `run()` alone
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

/* the loan of a running machine a host call holds for its calls back */
pub(crate) struct Lend(Option<NonNull<VM<'static>>>);

impl Lend {
    /* publishes a loan of `vm` and gives the machine through it until the loan drops */
    pub(crate) fn of<'x>(vm: &mut VM<'_>) -> (Self, &'x mut VM<'static>) {
        let ptr: NonNull<VM<'static>> = NonNull::from(vm).cast();
        let before = with_bridge(|b| b.current_vm.replace(ptr));
        (Lend(before), vm_mut(ptr))
    }
}

impl Drop for Lend {
    fn drop(&mut self) {
        let before = self.0;
        with_bridge(|b| b.current_vm = before);
    }
}

pub(crate) fn with_vm<R>(f: impl FnOnce(&mut VM<'static>) -> R) -> Option<R> {
    // the bridge borrow drops before `f` since VM dispatch enters `with_bridge` again
    let vm = vm_mut(with_bridge(|b| b.current_vm)?);
    // a value the host builds between runs counts as memory of the program
    let _paying = crate::value::meter::Paying::to(vm.heap.meter());
    Some(f(vm))
}

/* `with_vm` that errors when called outside `run` */
pub(crate) fn in_vm(err: &'static str, f: impl FnOnce(&mut VM<'static>) -> Result<Val, VmErr>) -> Result<Val, VmErr> {
    with_vm(f).ok_or(VmErr::Runtime(err))?
}

/* `f` over the live VM and the value of `recv_h` or an error without either */
pub(crate) fn with_recv<R, F>(invalid_recv_msg: &'static str, recv_h: u32, f: F) -> Result<R, VmErr>
where F: FnOnce(&mut VM<'static>, Val) -> Result<R, VmErr>
{ let recv = get_val(recv_h).ok_or(VmErr::Runtime(invalid_recv_msg))?; with_vm(|vm| f(vm, recv)).ok_or(VmErr::Runtime("edge_op called outside run()"))? }

/* the kind that names the error's own class and Custom for any other class */
pub(crate) fn err_to_kind(e: &VmErr, names: &Names) -> ErrorKind { crate::lang::abi_kind(e, names) }

pub(crate) fn stash_error(e: VmErr) {
    // the facts of an engine error take the runtime's words before the host reads them
    let e = match e { VmErr::Engine(_) => with_vm(|vm| vm.word(e.clone())).unwrap_or(e), e => e };
    let names = with_vm(|vm| vm.natives.names()).unwrap_or(&Names::PLAIN);
    let kind = with_vm(|vm| vm.error_kind(&e)).unwrap_or_else(|| err_to_kind(&e, names));
    // a kind naming the class takes the bare message so no crossing writes it twice
    let msg = if kind == ErrorKind::Custom { e.render(names) } else { e.message(names) };
    with_bridge(|b| b.error_stash.set_typed(kind, msg));
}

/* a message alone for places without a `VmErr` like the wasm panic handler */
pub(crate) fn stash_raw_error(kind: u32, msg: String) {
    with_bridge(|b| b.error_stash.set(kind, msg));
}

pub fn take_error() -> Option<(u32, String)> {
    with_bridge(|b| b.error_stash.take())
}

/* rebuilds the `VmErr` `err_to_kind` read and matches every kind so none slips into `Raised` */
pub fn error_from_kind(kind: u32, msg: String) -> VmErr {
    match ErrorKind::from_u32(kind) {
        Some(ErrorKind::Type) => VmErr::TypeMsg(msg),
        Some(ErrorKind::Value) => VmErr::Of(ErrKind::Value, msg),
        Some(ErrorKind::Runtime) => VmErr::Of(ErrKind::Runtime, msg),
        Some(ErrorKind::Attribute) => VmErr::Attribute(msg),
        Some(ErrorKind::Index) => VmErr::Of(ErrKind::Index, msg),
        Some(ErrorKind::Key) => VmErr::Of(ErrKind::Key, msg),
        // an end a plugin passes back unread raises the class the runtime names for it
        Some(ErrorKind::Custom) if msg == abi::op::ITER_END => VmErr::Of(ErrKind::Stop, String::new()),
        // a custom kind carries its class in `msg` as written and passes through unchanged
        Some(ErrorKind::Custom) | None => VmErr::Raised(msg),
    }
}

// 0 leaves a handle and 1 stashes an error and 2 means a call waits
pub(crate) fn host_edge_op(op: u32, recv: u32, name: &str, argv: &[u32], out_handle: &mut u32) -> i32 {
    // a stale argument handle fails the op since dropping it shifts the arguments after it
    let args: Option<Vec<Val>> = argv.iter().map(|&h| get_val(h)).collect();
    let Some(args) = args else { stash_error(VmErr::TypeMsg("edge_op: invalid argument handle".into())); return 1; };

    // code a waiting plugin call runs gets its own loop as the rule's one exception
    with_vm(|vm| vm.hosted += 1);
    let result = host_op(op, recv, name, &args);
    with_vm(|vm| vm.hosted -= 1);

    match result {
        Ok(Some(v)) => { *out_handle = put_val(v); 0 }
        // the end reads in the ABI's own words so no runtime's class reaches a plugin
        Ok(None) => { stash_raw_error(ErrorKind::Custom as u32, String::from(abi::op::ITER_END)); 1 }
        // a system call still waits so the plugin returns 2 and the host resumes it
        Err(VmErr::HostCallDeferred) => 2,
        Err(e) => { stash_error(e); 1 }
    }
}

/* what `host_edge_op` asks of the VM with none once an iteration ends */
fn host_op(op: u32, recv: u32, name: &str, args: &[Val]) -> Result<Option<Val>, VmErr> {
    let val = match Op::from_u32(op) {
        Some(Op::Call) => dispatch_call(recv, name, args),
        Some(Op::GetAttr) => dispatch_get_attr(recv, name),
        Some(Op::SetAttr) => dispatch_set_attr(recv, name, args),
        Some(Op::GetItem) => dispatch_get_item(recv, args),
        Some(Op::SetItem) => dispatch_set_item(recv, args),
        Some(Op::Len) => dispatch_len(recv),
        Some(Op::Iter) => dispatch_iter(recv),
        Some(Op::IterNext) => return dispatch_iter_next(recv),
        Some(Op::NewDict) => in_vm("edge_op new_dict called outside run()", |vm| vm.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(DictMap::new()))))),
        Some(Op::NewList) => in_vm("edge_op new_list called outside run()", |vm| vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(Vec::new()))))),
        Some(Op::TypeOf) => dispatch_type_of(recv),
        Some(Op::NewTuple) => in_vm("edge_op new_tuple called outside run()", |vm| vm.heap.alloc(HeapObj::Tuple(args.to_vec()))),
        Some(Op::NewSet) => in_vm("edge_op new_set called outside run()", |vm| vm.set_from_items(args.to_vec())),
        Some(Op::NewFrozenSet) => Err(VmErr::Type("frozenset is not supported")),
        // a system call names its plugin call in `recv` as the id `edge_call_id` gave it
        Some(Op::Sys) => crate::targets::web::system_call(recv, name, args),
        None => Err(VmErr::Raised(s!("edge_op: unsupported op ", int op as i64))),
    };
    val.map(Some)
}

fn dispatch_call(recv_h: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    // no receiver and the text name ask for the text the runtime writes
    if recv_h == 0 && name == abi::op::TEXT { return text_of(args); }
    with_recv("edge_op call: invalid receiver handle", recv_h, |vm, recv| {
        // the self name calls `recv` itself so any callable a plugin holds dispatches normally
        if name == abi::op::CALL_SELF {
            let chunk = vm.chunk;
            return vm.call_with(recv, None, args, &[], chunk)?.ok_or(VmErr::Runtime("edge_op call of itself: callable left no result"));
        }
        // the runtime's native method of the receiver's kind
        let Some((n, owner)) = vm.native_method(recv, name) else {
            return Err(vm.fail(crate::lang::EngineError::NoAttribute { recv, name: String::from(name), store: false }));
        };
        vm.native_val(n, recv, Some(owner), args, &[], false)?.ok_or(VmErr::Runtime("edge_op call: method left no result"))
    })
}

/* the one value of `args` as text the way the runtime's render hook writes it */
fn text_of(args: &[Val]) -> Result<Val, VmErr> {
    let &[v] = args else { return Err(VmErr::TypeMsg(s!("str expects exactly 1 value, got ", int args.len() as i64))); };
    in_vm("edge_op str called outside run()", |vm| {
        // a runtime with no render hook writes only ints and strs and refuses the rest
        let text = vm.render(v, Mode::Str)?;
        vm.heap.alloc(HeapObj::Str(text))
    })
}

/* the op that reads like `recv.name` */
fn dispatch_get_attr(recv_h: u32, name: &str) -> Result<Val, VmErr> {
    with_recv("edge_op get_attr: invalid receiver handle", recv_h, |vm, recv| {
        vm.attr_value(recv, name)
    })
}

/* the op that writes like `recv.name = value` */
fn dispatch_set_attr(recv_h: u32, name: &str, args: &[Val]) -> Result<Val, VmErr> {
    let &[value] = args else {
        return Err(VmErr::TypeMsg(s!("set_attr expects exactly 1 value, got ", int args.len() as i64)));
    };
    with_recv("edge_op set_attr: invalid receiver handle", recv_h, |vm, recv| {
        vm.store_attr(recv, name, value)?;
        Ok(Val::none())
    })
}

/* builtin indexing only as a plugin call has no frame for an instance's own indexing */
fn dispatch_get_item(recv_h: u32, args: &[Val]) -> Result<Val, VmErr> {
    if args.len() != 1 {
        return Err(VmErr::TypeMsg(s!("get_item expects 1 index, got ", int args.len() as i64)));
    }
    let idx = args[0];
    with_recv("edge_op get_item: invalid receiver handle", recv_h, |vm, recv| {
        vm.get_item_builtin(recv, idx)
    })
}

/* item assignment on builtins alone for the reason `dispatch_get_item` gives */
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

/* runs the VM's `len` builtin so its caps and charges apply */
fn dispatch_len(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op len: invalid receiver handle", recv_h, |vm, recv| {
        let n = vm.builtin_len(recv)?;
        vm.int_to_val(Some(n))
    })
}

/* flattens any iterable into a list the guest reads by index and length */
fn dispatch_iter(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op iter: invalid receiver handle", recv_h, |vm, recv| {
        let items = vm.extract_iter(recv)?;
        vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))
    })
}

/* the next item of an iterator list or builtin iterator and none at the end */
fn dispatch_iter_next(recv_h: u32) -> Result<Option<Val>, VmErr> {
    with_recv("edge_op iter_next: invalid receiver handle", recv_h, |vm, recv| {
        if matches!(vm.heap.try_get(recv), Some(HeapObj::Iter(..))) {
            vm.iter_step(recv)
        } else if let Some(HeapObj::List(rc)) = vm.heap.try_get(recv) {
            // a cursor beside the handle walks the list so no step shifts what is left
            let at = with_bridge(|b| b.handles.advance(recv_h)).unwrap_or(u32::MAX);
            Ok(rc.borrow().get(at as usize).copied())
        } else {
            Err(VmErr::TypeMsg(s!("iter_next expects a List iterator (produced by Op::Iter), got '", str &vm.type_named(recv), "'")))
        }
    })
}

fn dispatch_type_of(recv_h: u32) -> Result<Val, VmErr> {
    with_recv("edge_op type_of: invalid receiver handle", recv_h, |vm, recv| {
        let name = vm.type_named(recv);
        vm.heap.alloc(HeapObj::Str(name))
    })
}

// turns a tag and bytes into a value handle or 0 for an invalid pair
pub(crate) fn host_edge_encode(tag: u32, bytes: &[u8]) -> u32 {
    // allocates in the live VM and gives 0 outside a run or out of memory
    fn alloc_and_put(make: impl FnOnce() -> HeapObj) -> u32 {
        // the payload is copied inside so the program's meter pays for it between runs too
        match with_vm(|vm| vm.heap.alloc(make()).ok()).flatten() {
            Some(val) => put_val(val),
            None => 0,
        }
    }
    match classify_encode(tag, bytes) {
        EncodeRequest::Direct(bits) => put_val(Val(bits)),
        EncodeRequest::AllocStr(s) => alloc_and_put(|| HeapObj::Str(s.to_string())),
        EncodeRequest::AllocBytes(b) => alloc_and_put(|| HeapObj::Bytes(b.to_vec())),
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

/* a wire tree as a heap value keyed by text alone and split like `classify_encode` */
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

/* a foreign value as a wire int unless past i128 or of a homebound kind */
fn foreign_int(vm: &crate::vm::VM, v: Val) -> Option<i128> {
    vm.heap.foreign_kind(v).filter(|k| k.transfer == crate::value::foreign::Transfer::Copy)?;
    vm.heap.foreign_i128(v)
}

/* a heap value as a wire tree or none for cycles depth and local values */
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
            // a shared reference is fine but meeting a value again mid walk is a cycle
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
                HeapObj::Dict(rc) => {
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

// bootstrap decoder writing the tag to `out_tag` and the bytes into `dst`
pub(crate) fn host_edge_decode(h: u32, out_tag: &mut u32, dst: &mut [u8]) -> i32 {
    let v = match get_val(h) {
        Some(v) => v,
        None => { *out_tag = TAG_INVALID; return 0; }
    };
    let mut copy_into = |tag: u32, bytes: &[u8]| -> i32 {
        *out_tag = tag;
        let Some(room) = dst.get_mut(..bytes.len()) else { return -(bytes.len() as i32) };
        room.copy_from_slice(bytes);
        bytes.len() as i32
    };

    match classify_decode(v.0) {
        DecodeBits::Primitive { tag, bytes } => match bytes {
            PrimitiveBytes::None => copy_into(tag, &[]),
            PrimitiveBytes::Bool(b) => copy_into(tag, &[b]),
            PrimitiveBytes::Eight(a) => copy_into(tag, &a),
            PrimitiveBytes::Sixteen(a) => copy_into(tag, &a),
        },
        DecodeBits::Heap => {
            // text bytes ints and composites decode while other values go through `edge_op`
            enum Decoded { Str(alloc::string::String), Bytes(Vec<u8>), Int(i128), Wire(u32, Vec<u8>), Other }
            let decoded = with_vm(|vm| if v.is_foreign() { foreign_int(vm, v).map_or(Decoded::Other, Decoded::Int) } else { match vm.heap.get(v) {
                HeapObj::Str(s) => Decoded::Str(s.clone()),
                HeapObj::Bytes(b) => Decoded::Bytes(b.clone()),
                HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) => {
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
                Decoded::Other => copy_into(TAG_INVALID, &[]),
            }
        }
        DecodeBits::Invalid => copy_into(TAG_INVALID, &[]),
    }
}

// decrements the refcount of a handle and does nothing for an invalid one
pub(crate) fn host_edge_release(h: u32) {
    with_bridge(|b| b.handles.release(h));
}

// stashes a guest error for the host over any pending one
pub(crate) fn host_edge_throw(kind: u32, msg: &str) {
    with_bridge(|b| b.error_stash.set(kind, msg.to_string()));
}

// drains the most recent error
pub(crate) fn host_edge_take_error(out_kind: &mut u32, dst: &mut [u8]) -> i32 {
    // a peek first lets a caller with too small a buffer retry
    let (kind, len) = match with_bridge(|b| b.error_stash.peek().map(|(k, m)| (k, m.len()))) {
        Some(p) => p,
        None => return -1,
    };
    if len > dst.len() { return -(len as i32); }
    // the buffer fits so the error drains and a lost peek race never panics
    let Some((_, msg)) = with_bridge(|b| b.error_stash.take()) else { return -1; };
    let bytes = msg.as_bytes();
    *out_kind = kind;
    dst[..bytes.len()].copy_from_slice(bytes);
    bytes.len() as i32
}
