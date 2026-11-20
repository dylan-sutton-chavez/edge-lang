use alloc::{string::String, vec::Vec};

use crate::engine::r#unsafe::plugin as ffi;

mod macros;

// the macros a plugin writes its exports with under their short names
pub use crate::{__plugin_class as plugin_class, __plugin_fn as plugin_fn, __plugin_module as plugin_module, __plugin_resume as plugin_resume};

// a cell for static plugin state that is `Sync` because wasm has no threads
pub use crate::engine::r#unsafe::plugin::PluginCell;

// the running call and the key to keep state under while a system call waits
pub fn call_id() -> u32 { ffi::call_id() }

use crate::engine::abi::{op, tag, ErrorKind};

// the pieces the macros expand to which lib.rs hands them through its private door
pub(crate) mod expand {
    use super::{Error, FromValue, Handle, IntoValue, Result};
    use alloc::string::{String, ToString};

    pub use crate::engine::r#unsafe::plugin::export;
    pub use alloc::collections::BTreeMap;

    // a zero handle means the system call failed and the host holds its error
    pub fn answer(h: u32) -> Result<Handle> {
        if h == 0 { Err(super::last_error()) } else { Ok(Handle::from_raw(h)) }
    }

    // a constructed instance lives under the id its runtime object keeps
    pub fn construct(this: &Handle, id: i64) -> Result<()> {
        this.set_attr("__rust_id", &id.into_handle()?)
    }

    // the state of the instance whose runtime object a method was called on
    pub fn instance<T>(state: &'static mut BTreeMap<i64, T>, this: &Handle) -> Result<&'static mut T> {
        let id = i64::from_handle(this.get_attr("__rust_id")?.raw())?;
        state.get_mut(&id).ok_or_else(|| Error::Runtime(String::from("instance state missing")))
    }

    // 2 while a system call waits and otherwise 1 with the error stashed
    pub(crate) fn fail(e: Error) -> i32 {
        if matches!(e, Error::Waiting) { return 2; }
        stash_error(e);
        1
    }

    // hands the host the error a plugin call ends with
    pub(crate) fn stash_error(e: Error) {
        let msg = e.message().to_string();
        super::ffi::throw(e.code(), &msg);
    }

    /* the order every export's params keep which a misplaced `Args` or `Kwargs` breaks at compile time */
    pub const fn check(kinds: &[super::Kind]) {
        let mut i = 0;
        let mut rest = false;
        while i < kinds.len() {
            match kinds[i] {
                super::Kind::Fixed => assert!(!rest, "a plugin export takes `Args` after the fixed params and before any `Kwargs`"),
                super::Kind::Rest => { assert!(!rest, "a plugin export takes one `Args` at most"); rest = true; }
                super::Kind::Keywords => assert!(i + 1 == kinds.len(), "a plugin export takes `Kwargs` as its final param"),
            }
            i += 1;
        }
    }

    /* the handles of one call read param by param once their count fits the export */
    pub struct Cursor<'a> { slots: &'a [u32], at: usize, fixed: usize }

    impl<'a> Cursor<'a> {
        // the host always appends a kwargs slot after the fixed positionals and an optional `Args`
        pub fn open(label: &str, kinds: &[super::Kind], slots: &'a [u32]) -> Result<Self> {
            let fixed = kinds.iter().filter(|k| matches!(k, super::Kind::Fixed)).count();
            let rest = kinds.iter().any(|k| matches!(k, super::Kind::Rest));
            let got = slots.len().saturating_sub(1);
            if rest && slots.len() < fixed + 1 {
                return Err(Error::Type(alloc::format!("{label} expects at least {fixed} positional args, got {got}")));
            }
            if !rest && slots.len() != fixed + 1 {
                return Err(Error::Type(alloc::format!("{label} expects {fixed} positional args, got {got}")));
            }
            Ok(Self { slots, at: 0, fixed })
        }

        pub(super) fn next(&mut self) -> u32 { let h = self.slots[self.at]; self.at += 1; h }
        pub(super) fn rest(&self) -> &'a [u32] { &self.slots[self.fixed..self.slots.len() - 1] }
        pub(super) fn last(&self) -> u32 { self.slots[self.slots.len() - 1] }
    }
}

/* how a param of an export takes its handles */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum Kind { Fixed, Rest, Keywords }

/* a type a plugin export takes as a param */
pub trait Param: Sized {
    const KIND: Kind;
    fn take(at: &mut expand::Cursor<'_>) -> Result<Self>;
}

impl<T: FromValue> Param for T {
    const KIND: Kind = Kind::Fixed;
    fn take(at: &mut expand::Cursor<'_>) -> Result<Self> { T::from_handle(at.next()) }
}

impl Param for Args {
    const KIND: Kind = Kind::Rest;
    fn take(at: &mut expand::Cursor<'_>) -> Result<Self> { Ok(Args(at.rest().iter().map(|&h| Handle::borrow(h)).collect())) }
}

impl Param for Kwargs {
    const KIND: Kind = Kind::Keywords;
    // handle 0 means no kwargs and any other borrows the dict
    fn take(at: &mut expand::Cursor<'_>) -> Result<Self> {
        let h = at.last();
        Ok(Kwargs((h != 0).then(|| Handle::borrow(h))))
    }
}

/* what a plugin export returns as a value or as a result */
pub trait Ret { fn settle(self) -> Result<Handle>; }

impl<T: IntoValue> Ret for T {
    fn settle(self) -> Result<Handle> { self.into_handle() }
}

impl<T: IntoValue> Ret for Result<T> {
    fn settle(self) -> Result<Handle> { self?.into_handle() }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    Type(String),
    Value(String),
    Runtime(String),
    Attribute(String),
    Index(String),
    Key(String),
    // a raise whose text names its class as written
    #[non_exhaustive]
    Custom { kind: u32, message: String },
    // a system call still waits and `plugin_resume!` runs once it settles
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
    /* an error the program raises as the class its text names */
    pub fn custom(message: impl Into<String>) -> Self {
        Self::Custom { kind: crate::engine::abi::error_kind::CUSTOM, message: message.into() }
    }

    /* the kind a host raises the error as with every code it does not know read as custom */
    pub fn kind(&self) -> ErrorKind {
        ErrorKind::from_u32(self.code()).unwrap_or(ErrorKind::Custom)
    }

    // the raw code the host is thrown with
    pub(crate) fn code(&self) -> u32 {
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
    pub(crate) fn from_kind(kind: u32, message: String) -> Self {
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

// takes the error the host stashed after an `interpreter_op` returned 1
pub(crate) fn last_error() -> Error {
    let mut kind: u32 = 2;
    let mut buf = alloc::vec![0u8; 256];
    loop {
        let r = ffi::take_error(&mut kind, &mut buf);
        if r >= 0 {
            buf.truncate(r as usize);
            let msg = String::from_utf8_lossy(&buf).into_owned();
            return Error::from_kind(kind, msg);
        }
        if r == -1 { return Error::Runtime(String::from("native error: no message")); }
        // a negative result is the size needed so the buffer grows and retries
        buf.resize((-r) as usize, 0);
    }
}

// an opaque host reference released on drop only when `owned` and borrowed from argv otherwise
pub struct Handle { raw: u32, owned: bool }

impl Handle {
    // the host owns an argv handle for the whole call so drop leaves it
    pub fn borrow(raw: u32) -> Self { Self { raw, owned: false } }
    // owned so `Drop` releases the refcount
    pub(crate) fn from_raw(raw: u32) -> Self { Self { raw, owned: true } }
    // gives up the handle without `Drop` when it is written to `*out`
    pub(crate) fn into_raw(self) -> u32 {
        let r = self.raw;
        core::mem::forget(self);
        r
    }
    pub fn raw(&self) -> u32 { self.raw }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.owned && self.raw != 0 { ffi::release(self.raw) }
    }
}

impl FromValue for Handle {
    fn from_handle(h: u32) -> Result<Self> { Ok(Handle::borrow(h)) }
    fn from_owned(h: Handle) -> Result<Self> { Ok(h) }
}
impl IntoValue for Handle {
    fn into_handle(self) -> Result<Handle> { Ok(self) }
}

// the trailing kwargs slot which the host sends as handle 0 when none were passed
pub struct Kwargs(Option<Handle>);

impl Kwargs {
    // a kwarg as `T` or `Ok(None)` when absent where `Handle` borrows what no other type decodes
    pub fn get<T: FromValue>(&self, name: &str) -> Result<Option<T>> {
        match self.get_handle(name)? {
            None => Ok(None),
            Some(h) => T::from_owned(h).map(Some),
        }
    }

    // borrows a kwarg as a raw handle
    fn get_handle(&self, name: &str) -> Result<Option<Handle>> {
        let Some(dict) = self.0.as_ref() else { return Ok(None); };
        let key = encode(Value::Bytes(name.as_bytes().to_vec()))?;
        // indexing names no method so it reads the same in every runtime
        match dict.get_item(&key) {
            Ok(val) => Ok((!is_nil(val.raw())).then_some(val)),
            Err(Error::Key(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// a zero-length buffer peeks at the tag without copying the value
fn is_nil(h: u32) -> bool {
    let mut tag: u32 = 0;
    let r = ffi::decode(h, &mut tag, &mut []);
    r >= 0 && tag == tag::NONE
}

// fails on composites that do not transit which go through `Handle::call`
pub(crate) fn decode(h: u32) -> Result<Value> {
    let mut tag: u32 = 0;
    let mut buf = alloc::vec![0u8; 256];
    loop {
        let r = ffi::decode(h, &mut tag, &mut buf);
        if r >= 0 {
            if tag == u32::MAX {
                return Err(Error::Type(String::from("value is not a transit value (use Handle::call for composites)")));
            }
            buf.truncate(r as usize);
            return Value::decode_body(tag, &buf)
                .ok_or_else(|| Error::Type(String::from("malformed wire value")));
        }
        // a negative result is the size needed
        buf.resize((-r) as usize, 0);
    }
}

pub(crate) fn encode(v: Value) -> Result<Handle> {
    let mut body = Vec::new();
    v.encode_body(&mut body);
    let raw = ffi::encode(v.tag(), &body);
    if raw == 0 { Err(Error::Runtime(String::from("encode failed"))) }
    else { Ok(Handle::from_raw(raw)) }
}

// the typed transit value shared with the host while sets and instances go through `Handle::call`
use crate::engine::abi::WireValue as Value;

pub trait FromValue: Sized {
    fn from_handle(h: u32) -> Result<Self>;
    // an owned handle read then released unless the value keeps it as a handle does
    fn from_owned(h: Handle) -> Result<Self> { Self::from_handle(h.raw()) }
}

pub trait IntoValue {
    fn into_handle(self) -> Result<Handle>;
}

/* the type error of a value that decoded as another kind than `expected` words */
fn mismatch<T>(expected: &str, v: Value) -> Result<T> { Err(Error::Type(alloc::format!("{}, got {:?}", expected, v))) }

impl FromValue for () { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::None => Ok(()), v => mismatch("expected nil", v) } } }
impl IntoValue for () { fn into_handle(self) -> Result<Handle> { encode(Value::None) } }

impl FromValue for bool { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::Bool(b) => Ok(b), v => mismatch("expected bool", v) } } }
impl IntoValue for bool { fn into_handle(self) -> Result<Handle> { encode(Value::Bool(self)) } }

// a whole float stands for its int since a language of doubles has no int
impl FromValue for i64 { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::Int(i) => i64::try_from(i).map_err(|_| Error::Value(alloc::format!("int {} out of range for i64 (use i128)", i))), Value::Float(f) if f as i64 as f64 == f => Ok(f as i64), v => mismatch("expected int", v) } } }
impl IntoValue for i64 { fn into_handle(self) -> Result<Handle> { encode(Value::Int(self as i128)) } }

impl FromValue for i128 { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::Int(i) => Ok(i), Value::Float(f) if f as i128 as f64 == f => Ok(f as i128), v => mismatch("expected int", v) } } }
impl IntoValue for i128 { fn into_handle(self) -> Result<Handle> { encode(Value::Int(self)) } }

impl FromValue for f64 { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::Float(f) => Ok(f), Value::Int(i) => Ok(i as f64), v => mismatch("expected float", v) } } }
impl IntoValue for f64 { fn into_handle(self) -> Result<Handle> { encode(Value::Float(self)) } }

impl FromValue for String { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::Bytes(b) => String::from_utf8(b).map_err(|e| Error::Value(alloc::format!("invalid utf-8: {}", e))), v => mismatch("expected str", v) } } }
impl IntoValue for String { fn into_handle(self) -> Result<Handle> { encode(Value::Bytes(self.into_bytes())) } }
impl IntoValue for &str { fn into_handle(self) -> Result<Handle> { encode(Value::Bytes(self.as_bytes().to_vec())) } }
impl IntoValue for alloc::borrow::Cow<'_, str> { fn into_handle(self) -> Result<Handle> { encode(Value::Bytes(self.into_owned().into_bytes())) } }

// raw bytes that transit without ever being decoded as UTF-8
#[expect(clippy::exhaustive_structs, reason = "a newtype a user builds around its one value")]
pub struct Bytes(pub Vec<u8>);

impl core::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] { &self.0 }
}

impl FromValue for Bytes { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::Raw(b) => Ok(Bytes(b)), v => mismatch("expected bytes", v) } } }
impl IntoValue for Bytes { fn into_handle(self) -> Result<Handle> { encode(Value::Raw(self.0)) } }

impl FromValue for Value { fn from_handle(h: u32) -> Result<Self> { decode(h) } }
impl IntoValue for Value { fn into_handle(self) -> Result<Handle> { encode(self) } }

/* a list crosses whole in one `interpreter_decode` or `interpreter_encode` instead of an op per item */
impl FromValue for Vec<Value> { fn from_handle(h: u32) -> Result<Self> { match decode(h)? { Value::List(items) => Ok(items), v => mismatch("expected a sequence", v) } } }
impl IntoValue for Vec<Value> { fn into_handle(self) -> Result<Handle> { encode(Value::List(self)) } }

impl FromValue for Vec<f64> { fn from_handle(h: u32) -> Result<Self> { Vec::<Value>::from_handle(h)?.into_iter().map(|v| match v { Value::Float(f) => Ok(f), Value::Int(i) => Ok(i as f64), other => mismatch("expected a sequence of numbers", other) }).collect() } }
impl IntoValue for Vec<f64> { fn into_handle(self) -> Result<Handle> { encode(Value::List(self.into_iter().map(Value::Float).collect())) } }

// the trailing variadic params of a `plugin_fn!` as borrowed handles
pub struct Args(Vec<Handle>);

impl Args {
    // the handles in the order the call passed them
    pub fn iter(&self) -> core::slice::Iter<'_, Handle> { self.0.iter() }
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize { self.0.len() }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    // `None` past the last arg and an error when it does not decode as `T`
    pub fn get<T: FromValue>(&self, i: usize) -> Option<Result<T>> {
        self.0.get(i).map(|h| T::from_handle(h.raw()))
    }
}

impl<T: FromValue> FromValue for Option<T> { fn from_handle(h: u32) -> Result<Self> { if is_nil(h) { Ok(None) } else { T::from_handle(h).map(Some) } } }
impl<T: IntoValue> IntoValue for Option<T> { fn into_handle(self) -> Result<Handle> { match self { None => encode(Value::None), Some(v) => v.into_handle() } } }

impl Handle {
    // an empty name or argv passes a null pointer
    fn raw_op(opcode: u32, recv: u32, name: &str, argv: &[u32]) -> Result<Handle> {
        let mut out: u32 = 0;
        if ffi::op(opcode, recv, name, argv, &mut out) != 0 { return Err(last_error()); }
        Ok(Handle::from_raw(out))
    }

    // `recv.<name>(args)` with the argv handles still owned by the caller
    pub fn call(&self, name: &str, args: &[u32]) -> Result<Handle> { Self::raw_op(op::CALL, self.raw, name, args) }

    // the system call `module.name` which waits with `Error::Waiting` until `plugin_resume!` gets the answer
    pub fn sys(name: &str, args: &[u32]) -> Result<Handle> {
        let mut out: u32 = 0;
        match ffi::op(op::SYS, call_id(), name, args, &mut out) {
            0 => Ok(Handle::from_raw(out)),
            2 => Err(Error::Waiting),
            _ => Err(last_error()),
        }
    }

    // `recv.<name>` reading an attribute or binding a builtin method
    pub fn get_attr(&self, name: &str) -> Result<Handle> { Self::raw_op(op::GET_ATTR, self.raw, name, &[]) }

    // `recv[key]` with an int key indexing a list and a str key reading a dict
    pub fn get_item(&self, key: &Handle) -> Result<Handle> { Self::raw_op(op::GET_ITEM, self.raw, "", &[key.raw]) }

    // `recv[key] = value` whose None result is released at once
    pub fn set_item(&self, key: &Handle, value: &Handle) -> Result<()> { Self::raw_op(op::SET_ITEM, self.raw, "", &[key.raw, value.raw]).map(drop) }

    // the handle is owned before decoding so `Drop` releases it on any early return
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> Result<i64> { i64::from_handle(Self::raw_op(op::LEN, self.raw, "", &[])?.raw()) }

    // `recv.<name> = value` whose None result is released at once
    pub fn set_attr(&self, name: &str, value: &Handle) -> Result<()> { Self::raw_op(op::SET_ATTR, self.raw, name, &[value.raw]).map(drop) }

    // a fresh empty dict owned by the guest
    pub fn new_dict() -> Result<Handle> { Self::raw_op(op::NEW_DICT, 0, "", &[]) }

    // a fresh empty list owned by the guest
    pub fn new_list() -> Result<Handle> { Self::raw_op(op::NEW_LIST, 0, "", &[]) }

    // a tuple of the item handles built in one host call
    pub fn new_tuple(items: &[u32]) -> Result<Handle> { Self::raw_op(op::NEW_TUPLE, 0, "", items) }

    // an unhashable item raises the runtime's type error
    pub fn new_set(items: &[u32]) -> Result<Handle> { Self::raw_op(op::NEW_SET, 0, "", items) }

    // an unhashable item raises the runtime's type error
    pub fn new_frozenset(items: &[u32]) -> Result<Handle> { Self::raw_op(op::NEW_FROZENSET, 0, "", items) }

    // a fresh str naming the runtime type of `recv`
    pub fn type_of(&self) -> Result<Handle> { Self::raw_op(op::TYPE_OF, self.raw, "", &[]) }

    // the value as the runtime writes it as text
    pub fn text(&self) -> Result<String> { String::from_handle(Self::raw_op(op::CALL, 0, op::TEXT, &[self.raw])?.raw()) }

    // the receiver materialised as a list iterator
    pub fn iter(&self) -> Result<Handle> { Self::raw_op(op::ITER, self.raw, "", &[]) }

    // `Ok(None)` once the iterator ends and any other error passes through
    pub fn iter_next(&self) -> Result<Option<Handle>> {
        match Self::raw_op(op::ITER_NEXT, self.raw, "", &[]) {
            Ok(h) => Ok(Some(h)),
            Err(e) if ended(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// the end of an iteration as the ABI words it and no runtime's class
fn ended(e: &Error) -> bool {
    e.code() == crate::engine::abi::error_kind::CUSTOM && e.message() == op::ITER_END
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_abi_end_ends_an_iteration() {
        assert!(ended(&Error::from_kind(crate::engine::abi::error_kind::CUSTOM, String::from(op::ITER_END))));
        // a class a runtime names for the end is an error like any other
        assert!(!ended(&Error::from_kind(crate::engine::abi::error_kind::CUSTOM, String::from("Stop"))));
        assert!(!ended(&Error::Value(String::from(op::ITER_END))));
    }

    #[test]
    fn an_error_names_its_kind_as_the_abi_types_it_and_throws_its_own_code() {
        assert_eq!(Error::Value(String::from("v")).kind(), ErrorKind::Value);
        assert_eq!(Error::Waiting.kind(), ErrorKind::Runtime);
        assert_eq!(Error::custom("Mine").kind(), ErrorKind::Custom);
        // a code no host knows reads as custom yet goes to the host as written
        let other = Error::from_kind(42, String::from("Mine"));
        assert_eq!((other.kind(), other.code()), (ErrorKind::Custom, 42));
    }
}
