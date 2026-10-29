use alloc::{string::String, vec::Vec};

pub use abi::{nan_box, WireValue, EDGE_ABI_VERSION, MAX_WIRE_DEPTH, TAG_INVALID};

/* a sealed ABI enum with a `from_u32` reverse map as `docs/04-reference/06-abi.mdx` specifies */
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

abi_enum!(Op {
    Call = abi::op::CALL,
    GetAttr = abi::op::GET_ATTR,
    SetAttr = abi::op::SET_ATTR,
    GetItem = abi::op::GET_ITEM,
    SetItem = abi::op::SET_ITEM,
    Len = abi::op::LEN,
    Iter = abi::op::ITER,
    IterNext = abi::op::ITER_NEXT,
    NewDict = abi::op::NEW_DICT,
    NewList = abi::op::NEW_LIST,
    TypeOf = abi::op::TYPE_OF,
    NewTuple = abi::op::NEW_TUPLE,
    NewSet = abi::op::NEW_SET,
    NewFrozenSet = abi::op::NEW_FROZENSET,
    Sys = abi::op::SYS,
});

abi_enum!(Tag {
    None = abi::tag::NONE,
    Bool = abi::tag::BOOL,
    Int = abi::tag::INT,
    Float = abi::tag::FLOAT,
    // text as UTF-8 bytes that encode to a str and decode back to its bytes
    Bytes = abi::tag::BYTES,
    // opaque bytes kept as a bytes value without UTF-8 validation
    Raw = abi::tag::RAW,
    // composites in TLV form whose payloads `abi::WireValue` defines
    List = abi::tag::LIST,
    Dict = abi::tag::DICT,
});

abi_enum!(ErrorKind {
    Type = abi::error_kind::TYPE,
    Value = abi::error_kind::VALUE,
    Runtime = abi::error_kind::RUNTIME,
    Attribute = abi::error_kind::ATTRIBUTE,
    Index = abi::error_kind::INDEX,
    Key = abi::error_kind::KEY,
    Custom = abi::error_kind::CUSTOM,
});

// free at rc 0 and handed out as index plus one so 0 stays invalid
struct HandleSlot {
    // raw `Val` bits that only the classify functions interpret
    val: u64,
    rc: u32,
    // how far `IterNext` walked the list the handle names
    at: u32,
}

// refcounted handles to `Val` bits cleared between script runs
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

    pub fn clear(&mut self) {
        self.slots.clear();
        self.free_list.clear();
    }

    // a fresh handle at rc 1 reusing a freed slot first
    pub fn put(&mut self, val: u64) -> u32 {
        if let Some(idx) = self.free_list.pop() {
            self.slots[idx as usize] = HandleSlot { val, rc: 1, at: 0 };
            idx + 1
        } else {
            self.slots.push(HandleSlot { val, rc: 1, at: 0 });
            self.slots.len() as u32
        }
    }

    // every value a handle still holds
    pub fn live(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots.iter().filter(|s| s.rc > 0).map(|s| s.val)
    }

    // the value of a live handle and `None` for an invalid or freed one
    pub fn get(&self, h: u32) -> Option<u64> {
        if h == 0 { return None; }
        self.slots.get((h - 1) as usize)
            .filter(|s| s.rc > 0)
            .map(|s| s.val)
    }

    // where `IterNext` stands in the list live handle `h` names before it moves one on
    pub fn advance(&mut self, h: u32) -> Option<u32> {
        let slot = self.slots.get_mut(h.checked_sub(1)? as usize).filter(|s| s.rc > 0)?;
        slot.at = slot.at.saturating_add(1);
        Some(slot.at - 1)
    }

    // frees the slot at rc 0 and ignores a handle released twice
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

// one pending error that dispatch failures and `edge_throw` set and `edge_take_error` drains
#[derive(Default)]
pub struct ErrorStash(Option<(u32, String)>);

impl ErrorStash {
    pub const fn new() -> Self { Self(None) }
    pub fn clear(&mut self) { self.0 = None; }

    pub fn set(&mut self, kind: u32, msg: String) {
        self.0 = Some((kind, msg));
    }

    pub fn set_typed(&mut self, kind: ErrorKind, msg: String) {
        self.0 = Some((kind as u32, msg));
    }

    pub fn take(&mut self) -> Option<(u32, String)> { self.0.take() }

    // leaves the error in place so `edge_take_error` can retry with a bigger buffer
    pub fn peek(&self) -> Option<(u32, &str)> {
        self.0.as_ref().map(|(k, m)| (*k, m.as_str()))
    }
}

// the outcome of `edge_encode` where the host allocates the `Alloc` cases and builds a `Composite`
pub enum EncodeRequest<'a> {
    Direct(u64),
    AllocStr(&'a str),
    AllocBytes(&'a [u8]),
    AllocInt(i128),
    Composite(WireValue),
    Invalid,
}

// the `Val` bits of an inline int or `None` when the value needs a box
pub fn inline_int_bits(i: i128) -> Option<u64> {
    i64::try_from(i).ok().and_then(crate::value::Val::int_checked).map(|v| v.0)
}

// maps a tag and its bytes to a request on the sealed `nan_box` layout
pub fn classify_encode(tag: u32, bytes: &[u8]) -> EncodeRequest<'_> {
    use nan_box::*;

    match Tag::from_u32(tag) {
        Some(Tag::None) => EncodeRequest::Direct(TAG_NONE),
        Some(Tag::Bool) => {
            let b = !bytes.is_empty() && bytes[0] != 0;
            EncodeRequest::Direct(if b { TAG_TRUE } else { TAG_FALSE })
        }
        Some(Tag::Int) => {
            // the wire int is an i128 of 16 bytes covering the whole int range
            if bytes.len() != 16 { return EncodeRequest::Invalid; }
            let mut buf = [0u8; 16];
            buf.copy_from_slice(bytes);
            let i = i128::from_le_bytes(buf);
            // ints past the 48-bit inline range get boxed
            match inline_int_bits(i) {
                Some(bits) => EncodeRequest::Direct(bits),
                None => EncodeRequest::AllocInt(i),
            }
        }
        Some(Tag::Float) => {
            if bytes.len() != 8 { return EncodeRequest::Invalid; }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(bytes);
            // `Val::float` canonicalizes NaNs that would collide with the tag space
            EncodeRequest::Direct(crate::value::Val::float(f64::from_le_bytes(buf)).0)
        }
        Some(Tag::Bytes) => match core::str::from_utf8(bytes) {
            Ok(s) => EncodeRequest::AllocStr(s),
            Err(_) => EncodeRequest::Invalid,
        },
        Some(Tag::Raw) => EncodeRequest::AllocBytes(bytes),
        Some(Tag::List) | Some(Tag::Dict) => match WireValue::decode_body(tag, bytes) {
            Some(w) => EncodeRequest::Composite(w),
            None => EncodeRequest::Invalid,
        },
        None => EncodeRequest::Invalid,
    }
}

// the outcome of `edge_decode` where the host materializes a `Heap` value itself
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

// a `Heap` answer sends the host to `HeapPool` for the value
pub fn classify_decode(val_bits: u64) -> DecodeBits {
    let v = crate::value::Val(val_bits);
    let (tag, bytes) = if v.is_float() { (Tag::Float, PrimitiveBytes::Eight(v.as_float().to_le_bytes())) }
        else if v.is_int() { (Tag::Int, PrimitiveBytes::Sixteen((v.as_int() as i128).to_le_bytes())) }
        else if v.is_none() { (Tag::None, PrimitiveBytes::None) }
        else if v.is_bool() { (Tag::Bool, PrimitiveBytes::Bool(v.as_bool() as u8)) }
        else if v.is_heap() || v.is_foreign() { return DecodeBits::Heap; }
        else { return DecodeBits::Invalid; };
    DecodeBits::Primitive { tag: tag as u32, bytes }
}
