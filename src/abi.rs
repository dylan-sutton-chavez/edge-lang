use alloc::{string::String, vec::Vec};

pub use wasm_abi::{nan_box, WireValue, EDGE_ABI_VERSION, MAX_WIRE_DEPTH, TAG_INVALID};

/* Sealed op codes, tags and error kinds, each with a `from_u32` reverse map, spec in docs/03-reference/06-abi.mdx. */
macro_rules! abi_enum {
    ($name:ident { $($variant:ident = $value:path),+ $(,)? }) => {
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u32)]
        pub enum $name { $($variant = $value),+ }
        impl $name {
            pub fn from_u32(v: u32) -> Option<Self> {
                match v { $($value => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

/* Op codes (sealed), values mirror `wasm_abi::op::*`. */
abi_enum!(Op {
    Call = wasm_abi::op::CALL,
    GetAttr = wasm_abi::op::GET_ATTR,
    SetAttr = wasm_abi::op::SET_ATTR,
    GetItem = wasm_abi::op::GET_ITEM,
    SetItem = wasm_abi::op::SET_ITEM,
    Len = wasm_abi::op::LEN,
    Iter = wasm_abi::op::ITER,
    IterNext = wasm_abi::op::ITER_NEXT,
    NewDict = wasm_abi::op::NEW_DICT,
    NewList = wasm_abi::op::NEW_LIST,
    TypeOf = wasm_abi::op::TYPE_OF,
    NewTuple = wasm_abi::op::NEW_TUPLE,
    NewSet = wasm_abi::op::NEW_SET,
    NewFrozenSet = wasm_abi::op::NEW_FROZENSET,
    Sys = wasm_abi::op::SYS,
});

/* Tags (sealed), values mirror `wasm_abi::tag::*`. */
abi_enum!(Tag {
    None = wasm_abi::tag::NONE,
    Bool = wasm_abi::tag::BOOL,
    Int = wasm_abi::tag::INT,
    Float = wasm_abi::tag::FLOAT,
    // UTF-8 bytes, encoder builds a str, decoder returns its bytes.
    Bytes = wasm_abi::tag::BYTES,
    // Opaque bytes, no UTF-8 validation, maps to Python `bytes`.
    Raw = wasm_abi::tag::RAW,
    // TLV composites, payloads defined in `wasm_abi::WireValue`.
    List = wasm_abi::tag::LIST,
    Dict = wasm_abi::tag::DICT,
});

/* Error kinds (sealed), values mirror `wasm_abi::error_kind::*`. */
abi_enum!(ErrorKind {
    Type = wasm_abi::error_kind::TYPE,
    Value = wasm_abi::error_kind::VALUE,
    Runtime = wasm_abi::error_kind::RUNTIME,
    Attribute = wasm_abi::error_kind::ATTRIBUTE,
    Index = wasm_abi::error_kind::INDEX,
    Key = wasm_abi::error_kind::KEY,
    Custom = wasm_abi::error_kind::CUSTOM,
});

/* Handle table */

// Handle slot, rc=0 = free. Exposed handle = index+1 (0 reserved as invalid).
struct HandleSlot {
    // Raw Val bits, opaque to the ABI, encode/decode go through classify_*.
    val: u64,
    rc: u32,
}

// Refcounted handle -> Val-bits map, cleared between script runs.
pub struct HandleTable {
    slots: Vec<HandleSlot>,
    free_list: Vec<u32>,
}

impl Default for HandleTable {
    fn default() -> Self { Self::new() }
}

impl HandleTable {
    pub const fn new() -> Self {
        Self { slots: Vec::new(), free_list: Vec::new() }
    }

    // Reset to empty state. Called by the host between runs.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free_list.clear();
    }

    // Register a value. Returns a fresh handle (rc=1).
    pub fn put(&mut self, val: u64) -> u32 {
        if let Some(idx) = self.free_list.pop() {
            self.slots[idx as usize] = HandleSlot { val, rc: 1 };
            idx + 1
        } else {
            self.slots.push(HandleSlot { val, rc: 1 });
            self.slots.len() as u32
        }
    }

    // Every value a handle still holds.
    pub fn live(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots.iter().filter(|s| s.rc > 0).map(|s| s.val)
    }

    // Look up a value by handle, or `None` if invalid / freed.
    pub fn get(&self, h: u32) -> Option<u64> {
        if h == 0 { return None; }
        self.slots.get((h - 1) as usize)
            .filter(|s| s.rc > 0)
            .map(|s| s.val)
    }

    // Decrements rc, frees slot at 0. Safe against double-release.
    pub fn release(&mut self, h: u32) {
        if h == 0 { return; }
        let idx = (h - 1) as usize;
        if let Some(slot) = self.slots.get_mut(idx)
            && slot.rc > 0
        {
            slot.rc -= 1;
            if slot.rc == 0 { self.free_list.push(idx as u32); }
        }
    }
}

/* Error stash */

// Single-slot error stash, populated by dispatch failures / edge_throw, drained by edge_take_error.
#[derive(Default)]
pub struct ErrorStash(Option<(u32, String)>);

impl ErrorStash {
    pub const fn new() -> Self { Self(None) }
    pub fn clear(&mut self) { self.0 = None; }

    // Replace any pending error with `(kind, msg)`.
    pub fn set(&mut self, kind: u32, msg: String) {
        self.0 = Some((kind, msg));
    }

    // Stash a typed error.
    pub fn set_typed(&mut self, kind: ErrorKind, msg: String) {
        self.0 = Some((kind as u32, msg));
    }

    // Take the error if present.
    pub fn take(&mut self) -> Option<(u32, String)> { self.0.take() }

    // Peeks without consuming, lets edge_take_error retry on buffer-too-small.
    pub fn peek(&self) -> Option<(u32, &str)> {
        self.0.as_ref().map(|(k, m)| (*k, m.as_str()))
    }
}

/* Primitive codec helpers */

// edge_encode outcome, Direct (Val bits), AllocStr / AllocBytes / AllocInt (host alloc), Composite (host builds recursively), or Invalid.
pub enum EncodeRequest<'a> {
    Direct(u64),
    AllocStr(&'a str),
    AllocBytes(&'a [u8]),
    AllocInt(i128),
    Composite(WireValue),
    Invalid,
}

// Val bits for an inline int, `None` when the value needs a box.
pub fn inline_int_bits(i: i128) -> Option<u64> {
    i64::try_from(i).ok().and_then(crate::value::Val::int_checked).map(|v| v.0)
}

// Maps (tag, bytes) to EncodeRequest using the sealed `nan_box` layout.
pub fn classify_encode(tag: u32, bytes: &[u8]) -> EncodeRequest<'_> {
    use nan_box::*;

    match Tag::from_u32(tag) {
        Some(Tag::None) => EncodeRequest::Direct(TAG_NONE),
        Some(Tag::Bool) => {
            let b = !bytes.is_empty() && bytes[0] != 0;
            EncodeRequest::Direct(if b { TAG_TRUE } else { TAG_FALSE })
        }
        Some(Tag::Int) => {
            // Wire format is 16 bytes (i128) covering Edge Python's full int range.
            if bytes.len() != 16 { return EncodeRequest::Invalid; }
            let mut buf = [0u8; 16];
            buf.copy_from_slice(bytes);
            let i = i128::from_le_bytes(buf);
            // Fits in 48-bit inline range -> emit as Val::int directly, else box it.
            match inline_int_bits(i) {
                Some(bits) => EncodeRequest::Direct(bits),
                None => EncodeRequest::AllocInt(i),
            }
        }
        Some(Tag::Float) => {
            if bytes.len() != 8 { return EncodeRequest::Invalid; }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(bytes);
            // Val::float canonicalizes NaNs that would collide with the tag space.
            EncodeRequest::Direct(crate::value::Val::float(f64::from_le_bytes(buf)).0)
        }
        Some(Tag::Bytes) => match core::str::from_utf8(bytes) {
            Ok(s) => EncodeRequest::AllocStr(s),
            Err(_) => EncodeRequest::Invalid,
        },
        // Raw bytes bypass UTF-8 validation and become a Python `bytes`.
        Some(Tag::Raw) => EncodeRequest::AllocBytes(bytes),
        Some(Tag::List) | Some(Tag::Dict) => match WireValue::decode_body(tag, bytes) {
            Some(w) => EncodeRequest::Composite(w),
            None => EncodeRequest::Invalid,
        },
        None => EncodeRequest::Invalid,
    }
}

// edge_decode outcome, Primitive (ready bytes), Heap (host materializes), or Invalid.
pub enum DecodeBits {
    Primitive { tag: u32, bytes: PrimitiveBytes },
    Heap,
    Invalid,
}

pub enum PrimitiveBytes {
    None,
    Bool(u8),
    Eight([u8; 8]),
    Sixteen([u8; 16]),
}

// Classifies Val bits, Heap routes the host to HeapPool.
pub fn classify_decode(val_bits: u64) -> DecodeBits {
    let v = crate::value::Val(val_bits);
    // Ints widen to the 16-byte wire form.
    let (tag, bytes) = if v.is_float() { (Tag::Float, PrimitiveBytes::Eight(v.as_float().to_le_bytes())) }
        else if v.is_int() { (Tag::Int, PrimitiveBytes::Sixteen((v.as_int() as i128).to_le_bytes())) }
        else if v.is_none() { (Tag::None, PrimitiveBytes::None) }
        else if v.is_bool() { (Tag::Bool, PrimitiveBytes::Bool(v.as_bool() as u8)) }
        else if v.is_heap() || v.is_foreign() { return DecodeBits::Heap; }
        else { return DecodeBits::Invalid; };
    DecodeBits::Primitive { tag: tag as u32, bytes }
}
