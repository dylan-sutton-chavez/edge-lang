#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// Wraps a free fn as a plugin ABI export. A minimal plugin is one such fn.
///
/// ```ignore
/// use wasm_pdk::*;
///
/// #[plugin_fn]
/// fn slugify(s: String) -> String {
///     s.to_lowercase().replace(' ', "-")
/// }
/// ```
pub use wasm_pdk_macros::plugin_fn;
pub use wasm_pdk_macros::{plugin_const, plugin_class, plugin_methods, plugin_ctor, plugin_resume};

/// Curated import surface that hides `__internals` / `__edge_alloc` from glob users.
pub mod prelude {
    pub use crate::{plugin_fn, plugin_const, plugin_class, plugin_methods, plugin_ctor, plugin_resume, call_id, Handle, Value, Bytes, Args, Error, Result, FromValue, IntoValue, Kwargs, PluginCell};
}

/* Plugin bootstrap */

// Hidden re-export so `module!` resolves lol_alloc without the author wiring it.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub use lol_alloc as __lol_alloc;

/// Emits the wasm32 boilerplate (allocator + panic handler) every plugin needs.
#[macro_export]
macro_rules! module {
    () => {
        // A plugin runs on one thread, so the free list needs no lock and reuses what it frees.
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static __WASM_PDK_ALLOC: $crate::__lol_alloc::AssumeSingleThreaded<$crate::__lol_alloc::FreeListAllocator> =
            unsafe { $crate::__lol_alloc::AssumeSingleThreaded::new($crate::__lol_alloc::FreeListAllocator::new()) };

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __wasm_pdk_panic(_: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}

// Hidden re-export so `module_fixed_pool!` resolves linked_list_allocator without the author wiring it.
#[doc(hidden)]
pub use linked_list_allocator as __lla;

/// Like `module!` on a fixed 4 MiB pool, never calls `memory.grow`, emits nothing off wasm32.
#[macro_export]
macro_rules! module_fixed_pool {
    () => { $crate::module_fixed_pool!(4 * 1024 * 1024); };
    ($pool:expr) => {
        #[cfg(target_arch = "wasm32")]
        mod __wasm_pdk_allocator {
            use core::alloc::{GlobalAlloc, Layout};
            use core::cell::UnsafeCell;
            use core::ptr::NonNull;
            use core::sync::atomic::{AtomicBool, Ordering};
            use $crate::__lla::Heap;

            const POOL_SIZE: usize = $pool;

            #[repr(align(16))]
            struct Pool(UnsafeCell<[u8; POOL_SIZE]>);
            unsafe impl Sync for Pool {}

            static POOL: Pool = Pool(UnsafeCell::new([0; POOL_SIZE]));

            struct HeapCell(UnsafeCell<Heap>);
            unsafe impl Sync for HeapCell {}

            static HEAP: HeapCell = HeapCell(UnsafeCell::new(Heap::empty()));
            static INIT: AtomicBool = AtomicBool::new(false);

            pub struct FreeListAlloc;

            unsafe impl GlobalAlloc for FreeListAlloc {
                unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                    if !INIT.load(Ordering::Relaxed) {
                        unsafe { (*HEAP.0.get()).init(POOL.0.get() as *mut u8, POOL_SIZE); }
                        INIT.store(true, Ordering::Relaxed);
                    }
                    unsafe {
                        (*HEAP.0.get())
                            .allocate_first_fit(layout)
                            .map_or(core::ptr::null_mut(), |p| p.as_ptr())
                    }
                }

                unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                    unsafe { (*HEAP.0.get()).deallocate(NonNull::new_unchecked(ptr), layout); }
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static __WASM_PDK_ALLOC: __wasm_pdk_allocator::FreeListAlloc = __wasm_pdk_allocator::FreeListAlloc;

        #[cfg(target_arch = "wasm32")]
        #[panic_handler]
        fn __wasm_pdk_panic(_: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}

use alloc::{string::String, vec::Vec};

/* Wire imports */

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    pub fn edge_op(op: u32, recv: u32, name_ptr: *const u8, name_len: u32, argv_ptr: *const u32, argc: u32, out: *mut u32) -> i32;
    pub fn edge_encode(tag: u32, ptr: *const u8, len: u32) -> u32;
    pub fn edge_decode(h: u32, out_tag: *mut u32, dst: *mut u8, dst_max: u32) -> i32;
    pub fn edge_release(h: u32);
    pub fn edge_take_error(out_kind: *mut u32, dst: *mut u8, dst_max: u32) -> i32;
    pub fn edge_throw(kind: u32, msg_ptr: *const u8, msg_len: u32);
    pub fn edge_call_id() -> u32;
}

/// The call running in this plugin, the key to keep state under while a system call waits.
pub fn call_id() -> u32 { unsafe { edge_call_id() } }

/* ABI version handshake */

// Host refuses to instantiate plugins whose ABI version it does not understand.
pub use wasm_abi::EDGE_ABI_VERSION;

#[unsafe(no_mangle)]
pub extern "C" fn __edge_abi_version() -> u32 { EDGE_ABI_VERSION }

/* Op codes & tags */

pub use wasm_abi::{op, tag};

/* Internals, macro contract surface, not user API */

/// Hidden module so `use wasm_pdk::*;` cannot leak the macro contract symbols.
#[doc(hidden)]
pub mod __internals {
    use super::Error;
    use alloc::string::ToString;

    /// A failed call's status, 2 while a system call waits, else 1 with its error stashed.
    pub fn fail(e: Error) -> i32 {
        if matches!(e, Error::Waiting) { return 2; }
        stash_error(e);
        1
    }

    /// Invoked by `#[plugin_fn]` expansion when a user fn returns `Err(_)`.
    pub fn stash_error(e: Error) {
        let kind = e.kind();
        let msg = e.message().to_string();
        unsafe { super::edge_throw(kind, msg.as_ptr(), msg.len() as u32); }
    }

    /// Host-side argv stager, paired with `__edge_free`.
    #[unsafe(no_mangle)]
    pub extern "C" fn __edge_alloc(size: u32) -> *mut u8 {
        let v = alloc::vec![0u8; size as usize];
        alloc::boxed::Box::into_raw(v.into_boxed_slice()) as *mut u8
    }

    /// Frees an `__edge_alloc` buffer. Sizes must match.
    #[unsafe(no_mangle)]
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub extern "C" fn __edge_free(ptr: *mut u8, size: u32) {
        if ptr.is_null() || size == 0 { return; }
        unsafe {
            let slice = core::slice::from_raw_parts_mut(ptr, size as usize);
            let _ = alloc::boxed::Box::from_raw(slice as *mut [u8]);
        }
    }
}

/* Errors */

#[derive(Debug, Clone)]
pub enum Error {
    Type(String),
    Value(String),
    Runtime(String),
    Attribute(String),
    Index(String),
    Key(String),
    Custom { kind: u32, message: String },
    /// A system call is still waiting, the host resumes the plugin through `#[plugin_resume]` once it settles.
    Waiting,
}

impl Error {
    pub fn message(&self) -> &str {
        match self {
            Self::Type(s) | Self::Value(s) | Self::Runtime(s)
            | Self::Attribute(s) | Self::Index(s) | Self::Key(s) => s,
            Self::Custom { message, .. } => message,
            Self::Waiting => "waiting on a system call",
        }
    }
    pub fn kind(&self) -> u32 {
        match self {
            Self::Type(_) => 0,
            Self::Value(_) => 1,
            Self::Runtime(_) => 2,
            Self::Attribute(_) => 3,
            Self::Index(_) => 4,
            Self::Key(_) => 5,
            Self::Custom { kind, .. } => *kind,
            Self::Waiting => 2,
        }
    }
    pub fn from_kind(kind: u32, message: String) -> Self {
        match kind {
            0 => Self::Type(message),
            1 => Self::Value(message),
            2 => Self::Runtime(message),
            3 => Self::Attribute(message),
            4 => Self::Index(message),
            5 => Self::Key(message),
            _ => Self::Custom { kind, message },
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// Drain the host's stashed error after a 1-returning `edge_op`. Never returns `None`.
pub fn last_error() -> Error {
    let mut kind: u32 = 2;
    let mut buf = alloc::vec![0u8; 256];
    loop {
        let r = unsafe {
            edge_take_error(&mut kind as *mut u32, buf.as_mut_ptr(), buf.len() as u32)
        };
        if r >= 0 {
            buf.truncate(r as usize);
            let msg = String::from_utf8_lossy(&buf).into_owned();
            return Error::from_kind(kind, msg);
        }
        if r == -1 { return Error::Runtime(String::from("native error: no message")); }
        // Negative = -needed, so resize and retry.
        buf.resize((-r) as usize, 0);
    }
}

/* Handle (RAII) */

/// Opaque host-managed reference. The `owned` flag distinguishes guest-released from argv-borrowed.
pub struct Handle { raw: u32, owned: bool }

impl Handle {
    /// Wrap an argv handle without releasing on drop (host owns it for the call).
    pub fn borrow(raw: u32) -> Self { Self { raw, owned: false } }
    /// Take ownership of a raw handle. `Drop` will release the refcount.
    pub fn from_raw(raw: u32) -> Self { Self { raw, owned: true } }
    /// Surrender the handle without running `Drop` (used when writing to `*out`).
    pub fn into_raw(self) -> u32 {
        let r = self.raw;
        core::mem::forget(self);
        r
    }
    pub fn raw(&self) -> u32 { self.raw }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.owned && self.raw != 0 { unsafe { edge_release(self.raw) } }
    }
}

impl FromValue for Handle {
    fn from_handle(h: u32) -> Result<Self> { Ok(Handle::borrow(h)) }
}
impl IntoValue for Handle {
    fn into_handle(self) -> Result<Handle> { Ok(self) }
}

/* Kwargs */

/// Trailing kwargs slot. `None` when no kwargs were passed (host sends handle 0), `Some(dict)` otherwise.
pub struct Kwargs(Option<Handle>);

impl Kwargs {
    /// Decodes `name` from kwargs as primitive `T`, `Ok(None)` when absent, `get_handle` covers the rest.
    pub fn get<T: FromValue>(&self, name: &str) -> Result<Option<T>> {
        match self.get_handle(name)? {
            None => Ok(None),
            Some(h) => T::from_handle(h.into_raw()).map(Some),
        }
    }

    /// Borrows the value for `name` as a raw `Handle`, `Ok(None)` when absent, for anything `get` cannot decode.
    pub fn get_handle(&self, name: &str) -> Result<Option<Handle>> {
        let Some(dict) = self.0.as_ref() else { return Ok(None); };
        let key = encode(Value::Bytes(name.as_bytes().to_vec()))?;
        let val = dict.call("get", &[key.raw()])?;
        let ty = val.type_of()?;
        let ty_str = String::from_handle(ty.raw())?;
        if ty_str == "NoneType" { Ok(None) } else { Ok(Some(val)) }
    }
}

impl FromValue for Kwargs {
    /// Macro-generated decode for the trailing kwargs slot, handle 0 = no kwargs, else borrow the dict.
    fn from_handle(h: u32) -> Result<Self> {
        if h == 0 { Ok(Kwargs(None)) } else { Ok(Kwargs(Some(Handle::borrow(h)))) }
    }
}

/* Bootstrap codec */

/// Decode a handle into a typed `Value`. Errors on non-transit composites (use `Handle::call`).
pub fn decode(h: u32) -> Result<Value> {
    let mut tag: u32 = 0;
    let mut buf = alloc::vec![0u8; 256];
    loop {
        let r = unsafe {
            edge_decode(h, &mut tag as *mut u32, buf.as_mut_ptr(), buf.len() as u32)
        };
        if r >= 0 {
            if tag == u32::MAX {
                return Err(Error::Type(alloc::string::String::from("value is not a transit value (use Handle::call for composites)")));
            }
            buf.truncate(r as usize);
            return Value::decode_body(tag, &buf)
                .ok_or_else(|| Error::Type(alloc::string::String::from("malformed wire value")));
        }
        // Negative = -needed.
        buf.resize((-r) as usize, 0);
    }
}

/// Encode a transit value into a handle.
pub fn encode(v: Value) -> Result<Handle> {
    let mut body = Vec::new();
    v.encode_body(&mut body);
    let raw = unsafe { edge_encode(v.tag(), body.as_ptr(), body.len() as u32) };
    if raw == 0 { Err(Error::Runtime(alloc::string::String::from("encode failed"))) }
    else { Ok(Handle::from_raw(raw)) }
}

/// Typed transit value, shared with the host as `wasm_abi::WireValue`. Sets and instances go through `Handle::call`.
pub use wasm_abi::WireValue as Value;

/* FromValue / IntoValue */

pub trait FromValue: Sized {
    fn from_handle(h: u32) -> Result<Self>;
}

pub trait IntoValue {
    fn into_handle(self) -> Result<Handle>;
}

impl FromValue for () {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::None => Ok(()),
            v => Err(Error::Type(alloc::format!("expected None, got {:?}", v))),
        }
    }
}
impl IntoValue for () {
    fn into_handle(self) -> Result<Handle> { encode(Value::None) }
}

impl FromValue for bool {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::Bool(b) => Ok(b),
            v => Err(Error::Type(alloc::format!("expected bool, got {:?}", v))),
        }
    }
}
impl IntoValue for bool {
    fn into_handle(self) -> Result<Handle> { encode(Value::Bool(self)) }
}

impl FromValue for i64 {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::Int(i) => i64::try_from(i).map_err(|_| Error::Value(
                alloc::format!("int {} out of range for i64 (use i128)", i)
            )),
            v => Err(Error::Type(alloc::format!("expected int, got {:?}", v))),
        }
    }
}
impl IntoValue for i64 {
    fn into_handle(self) -> Result<Handle> { encode(Value::Int(self as i128)) }
}

impl FromValue for i128 {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::Int(i) => Ok(i),
            v => Err(Error::Type(alloc::format!("expected int, got {:?}", v))),
        }
    }
}
impl IntoValue for i128 {
    fn into_handle(self) -> Result<Handle> { encode(Value::Int(self)) }
}

impl FromValue for f64 {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::Float(f) => Ok(f),
            Value::Int(i) => Ok(i as f64),
            v => Err(Error::Type(alloc::format!("expected float, got {:?}", v))),
        }
    }
}
impl IntoValue for f64 {
    fn into_handle(self) -> Result<Handle> { encode(Value::Float(self)) }
}

impl FromValue for String {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::Bytes(b) => String::from_utf8(b).map_err(|e| Error::Value(alloc::format!("invalid utf-8: {}", e))),
            v => Err(Error::Type(alloc::format!("expected str, got {:?}", v))),
        }
    }
}
impl IntoValue for String {
    fn into_handle(self) -> Result<Handle> { encode(Value::Bytes(self.into_bytes())) }
}
impl IntoValue for &str {
    fn into_handle(self) -> Result<Handle> {
        encode(Value::Bytes(self.as_bytes().to_vec()))
    }
}
impl IntoValue for alloc::borrow::Cow<'_, str> {
    fn into_handle(self) -> Result<Handle> {
        encode(Value::Bytes(self.into_owned().into_bytes()))
    }
}

/// Python `bytes`, transits raw and is never decoded as UTF-8. Deref reads it as a slice.
pub struct Bytes(pub Vec<u8>);

impl core::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] { &self.0 }
}

impl FromValue for Bytes {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::Raw(b) => Ok(Bytes(b)),
            v => Err(Error::Type(alloc::format!("expected bytes, got {:?}", v))),
        }
    }
}
impl IntoValue for Bytes {
    fn into_handle(self) -> Result<Handle> { encode(Value::Raw(self.0)) }
}

impl FromValue for Value {
    fn from_handle(h: u32) -> Result<Self> { decode(h) }
}
impl IntoValue for Value {
    fn into_handle(self) -> Result<Handle> { encode(self) }
}

/* List transit, the whole sequence crosses in one edge_decode/edge_encode instead of per-item ops. */

impl FromValue for Vec<Value> {
    fn from_handle(h: u32) -> Result<Self> {
        match decode(h)? {
            Value::List(items) => Ok(items),
            v => Err(Error::Type(alloc::format!("expected a sequence, got {:?}", v))),
        }
    }
}
impl IntoValue for Vec<Value> {
    fn into_handle(self) -> Result<Handle> { encode(Value::List(self)) }
}

impl FromValue for Vec<f64> {
    fn from_handle(h: u32) -> Result<Self> {
        Vec::<Value>::from_handle(h)?.into_iter().map(|v| match v {
            Value::Float(f) => Ok(f),
            Value::Int(i) => Ok(i as f64),
            other => Err(Error::Type(alloc::format!("expected a sequence of numbers, got {:?}", other))),
        }).collect()
    }
}
impl IntoValue for Vec<f64> {
    fn into_handle(self) -> Result<Handle> {
        encode(Value::List(self.into_iter().map(Value::Float).collect()))
    }
}

/// Trailing variadic params of a `#[plugin_fn]`, captured as borrowed handles.
pub struct Args(pub Vec<Handle>);

impl Args {
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize { self.0.len() }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    /// Decode the i-th arg as `T`, `None` when out of range.
    pub fn get<T: FromValue>(&self, i: usize) -> Option<Result<T>> {
        self.0.get(i).map(|h| T::from_handle(h.raw()))
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_handle(h: u32) -> Result<Self> {
        // Peek the tag with a zero-length buffer, consuming only on non-None.
        let mut tag: u32 = 0;
        let r = unsafe { edge_decode(h, &mut tag as *mut u32, core::ptr::null_mut(), 0) };
        if r >= 0 && tag == 0 { return Ok(None); }
        T::from_handle(h).map(Some)
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_handle(self) -> Result<Handle> {
        match self {
            None => encode(Value::None),
            Some(v) => v.into_handle(),
        }
    }
}

/* Universal dispatch via Handle */

impl Handle {
    // Shared edge_op wrapper. Empty name/argv pass null.
    fn raw_op(opcode: u32, recv: u32, name: &str, argv: &[u32]) -> Result<u32> {
        let name_ptr = if name.is_empty() { core::ptr::null() } else { name.as_ptr() };
        let argv_ptr = if argv.is_empty() { core::ptr::null() } else { argv.as_ptr() };
        let mut out: u32 = 0;
        let r = unsafe {
            edge_op(opcode, recv, name_ptr, name.len() as u32, argv_ptr, argv.len() as u32, &mut out as *mut u32)
        };
        if r != 0 { return Err(last_error()); }
        Ok(out)
    }

    /// Invoke `recv.<name>(args)`. Argv handles stay owned by the caller.
    pub fn call(&self, name: &str, args: &[u32]) -> Result<Handle> {
        Self::raw_op(op::CALL, self.raw, name, args).map(Handle::from_raw)
    }

    /// The system call `module.name`, `Error::Waiting` while it waits and the answer comes to `#[plugin_resume]`.
    pub fn sys(name: &str, args: &[u32]) -> Result<Handle> {
        let argv_ptr = if args.is_empty() { core::ptr::null() } else { args.as_ptr() };
        let mut out: u32 = 0;
        match unsafe { edge_op(op::SYS, call_id(), name.as_ptr(), name.len() as u32, argv_ptr, args.len() as u32, &mut out as *mut u32) } {
            0 => Ok(Handle::from_raw(out)),
            2 => Err(Error::Waiting),
            _ => Err(last_error()),
        }
    }

    /// `recv.<name>`, read attribute or bind builtin method.
    pub fn get_attr(&self, name: &str) -> Result<Handle> {
        Self::raw_op(op::GET_ATTR, self.raw, name, &[]).map(Handle::from_raw)
    }

    /// `recv[key]`, the key is a handle, an int for list indexing or a str for dict lookup.
    pub fn get_item(&self, key: &Handle) -> Result<Handle> {
        Self::raw_op(op::GET_ITEM, self.raw, "", &[key.raw]).map(Handle::from_raw)
    }

    /// `recv[key] = value`. Key and value are passed as handles. Returns None which is released immediately.
    pub fn set_item(&self, key: &Handle, value: &Handle) -> Result<()> {
        let out = Self::raw_op(op::SET_ITEM, self.raw, "", &[key.raw, value.raw])?;
        let _ = Handle::from_raw(out);
        Ok(())
    }

    /// `len(recv)`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> Result<i64> {
        // Wrap before decoding so Drop releases on any early return.
        let h = Handle::from_raw(Self::raw_op(op::LEN, self.raw, "", &[])?);
        i64::from_handle(h.raw())
    }

    /// `recv.<name> = value`. SET_ATTR returns None which we release immediately.
    pub fn set_attr(&self, name: &str, value: &Handle) -> Result<()> {
        let out = Self::raw_op(op::SET_ATTR, self.raw, name, &[value.raw])?;
        let _ = Handle::from_raw(out);
        Ok(())
    }

    /// Returns a fresh empty dict handle owned by the guest.
    pub fn new_dict() -> Result<Handle> {
        Self::raw_op(op::NEW_DICT, 0, "", &[]).map(Handle::from_raw)
    }

    /// Returns a fresh empty list handle owned by the guest.
    pub fn new_list() -> Result<Handle> {
        Self::raw_op(op::NEW_LIST, 0, "", &[]).map(Handle::from_raw)
    }

    /// Construct a tuple from item handles in one host call.
    pub fn new_tuple(items: &[u32]) -> Result<Handle> {
        Self::raw_op(op::NEW_TUPLE, 0, "", items).map(Handle::from_raw)
    }

    /// Construct a set from item handles. Unhashable items raise `TypeError`.
    pub fn new_set(items: &[u32]) -> Result<Handle> {
        Self::raw_op(op::NEW_SET, 0, "", items).map(Handle::from_raw)
    }

    /// Construct a frozenset from item handles. Unhashable items raise `TypeError`.
    pub fn new_frozenset(items: &[u32]) -> Result<Handle> {
        Self::raw_op(op::NEW_FROZENSET, 0, "", items).map(Handle::from_raw)
    }

    /// `type(recv).__name__`. Returns a fresh str handle naming the runtime type.
    pub fn type_of(&self) -> Result<Handle> {
        Self::raw_op(op::TYPE_OF, self.raw, "", &[]).map(Handle::from_raw)
    }

    /// `iter(recv)`. Materialises the receiver as a List iterator handle.
    pub fn iter(&self) -> Result<Handle> {
        Self::raw_op(op::ITER, self.raw, "", &[]).map(Handle::from_raw)
    }

    /// `next(recv)`. Returns `Ok(None)` at end-of-iteration, propagates other errors.
    pub fn iter_next(&self) -> Result<Option<Handle>> {
        match Self::raw_op(op::ITER_NEXT, self.raw, "", &[]) {
            Ok(out) => Ok(Some(Handle::from_raw(out))),
            // Host renders the sentinel as "Exception: StopIteration", so match anywhere.
            Err(e) if e.message().contains("StopIteration") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/* Plugin-class state helpers */

/// Single-threaded interior-mutable cell for static plugin state. Sync because WASM has no threads.
pub struct PluginCell<T>(core::cell::UnsafeCell<Option<T>>);

unsafe impl<T> Sync for PluginCell<T> {}

impl<T> PluginCell<T> {
    /// Const constructor for static initialization.
    pub const fn new() -> Self { Self(core::cell::UnsafeCell::new(None)) }

    /// Unsafe getter. The caller must ensure no overlapping &mut borrows across reentrant edge_op calls.
    #[allow(clippy::mut_from_ref)]
    pub fn get_or_init<F: FnOnce() -> T>(&self, init: F) -> &mut T {
        unsafe {
            let ptr = self.0.get();
            if (*ptr).is_none() { *ptr = Some(init()); }
            (*ptr).as_mut().unwrap()
        }
    }
}

impl<T> Default for PluginCell<T> {
    fn default() -> Self { Self::new() }
}
