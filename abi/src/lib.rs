#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/* bumped on any breaking change and a host refuses any `__edge_abi_version` it does not know */
pub const EDGE_ABI_VERSION: u32 = 1;

/* the NaN-boxing layout that packs a `Val` into 64 bits */
pub mod nan_box {
    pub const QNAN: u64 = 0x7FFC_0000_0000_0000;
    pub const SIGN: u64 = 0x8000_0000_0000_0000;
    pub const TAG_UNDEF: u64 = QNAN;
    pub const TAG_NONE: u64 = QNAN | 1;
    pub const TAG_TRUE: u64 = QNAN | 2;
    pub const TAG_FALSE: u64 = QNAN | 3;
    pub const TAG_INT: u64 = QNAN | SIGN;
    pub const TAG_HEAP: u64 = QNAN | 4;
    /* a foreign handle with 16 generation and 32 index bits under the first free prefix */
    pub const TAG_FOREIGN: u64 = QNAN | 1 << 48;
    /* a 48-bit signed payload with its sign at bit 47 */
    pub const INT_PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
}

/* op codes of the `edge_op` dispatch primitive */
pub mod op {
    pub const CALL: u32 = 0;
    pub const GET_ATTR: u32 = 1;
    pub const SET_ATTR: u32 = 2;
    pub const GET_ITEM: u32 = 3;
    pub const SET_ITEM: u32 = 4;
    pub const LEN: u32 = 5;
    pub const ITER: u32 = 6;
    pub const ITER_NEXT: u32 = 7;
    pub const NEW_DICT: u32 = 8;
    pub const NEW_LIST: u32 = 9;
    pub const TYPE_OF: u32 = 10;
    // builds composites from the argv items in one call
    pub const NEW_TUPLE: u32 = 11;
    pub const NEW_SET: u32 = 12;
    pub const NEW_FROZENSET: u32 = 13;
    // a system call `module.name` served as the package of the plugin would import it
    pub const SYS: u32 = 14;
    // the name a call gives to call its receiver itself
    pub const CALL_SELF: &str = "__call__";
    // the name a receiverless call gives to ask for its one value as text
    pub const TEXT: &str = "str";
    // the custom error an ended `ITER_NEXT` gives whose spaces no class name holds
    pub const ITER_END: &str = "end of iteration";
}

/* the tags a value carries through `edge_encode` and `edge_decode` */
pub mod tag {
    pub const NONE: u32 = 0;
    pub const BOOL: u32 = 1;
    pub const INT: u32 = 2;
    pub const FLOAT: u32 = 3;
    /* text as UTF-8 bytes that encode to a str and decode back to its bytes */
    pub const BYTES: u32 = 4;
    // opaque bytes kept as a bytes value without UTF-8 validation
    pub const RAW: u32 = 5;
    // a u32le count then that many nodes for a list or a tuple
    pub const LIST: u32 = 6;
    // a u32le count then that many key and value node pairs with str keys only
    pub const DICT: u32 = 7;
}

/* the `edge_decode` answer for an invalid handle or a non-primitive left to `edge_op` */
pub const TAG_INVALID: u32 = u32::MAX;

/* error kinds of `edge_take_error` and `edge_throw` */
pub mod error_kind {
    pub const TYPE: u32 = 0;
    pub const VALUE: u32 = 1;
    pub const RUNTIME: u32 = 2;
    pub const ATTRIBUTE: u32 = 3;
    pub const INDEX: u32 = 4;
    pub const KEY: u32 = 5;
    pub const CUSTOM: u32 = 6;
}

// the nesting cap of `decode_body` past which a depth bomb returns `None`
pub const MAX_WIRE_DEPTH: u32 = 32;

// a value that crosses whole while sets and instances go through `edge_op`
#[derive(Debug, Clone, PartialEq)]
pub enum WireValue {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    // text the host materialises as a `str`
    Bytes(Vec<u8>),
    // opaque bytes the host materialises as a `bytes`
    Raw(Vec<u8>),
    // a list or tuple that a JS host sees as an array
    List(Vec<WireValue>),
    // str keys only since a dict with other keys does not cross whole
    Dict(Vec<(WireValue, WireValue)>),
}

impl WireValue {
    pub fn tag(&self) -> u32 {
        match self {
            Self::None => tag::NONE,
            Self::Bool(_) => tag::BOOL,
            Self::Int(_) => tag::INT,
            Self::Float(_) => tag::FLOAT,
            Self::Bytes(_) => tag::BYTES,
            Self::Raw(_) => tag::RAW,
            Self::List(_) => tag::LIST,
            Self::Dict(_) => tag::DICT,
        }
    }

    // the payload without its frame as the top-level call passes the tag apart
    pub fn encode_body(&self, out: &mut Vec<u8>) {
        match self {
            Self::None => {}
            Self::Bool(b) => out.push(*b as u8),
            Self::Int(i) => out.extend_from_slice(&i.to_le_bytes()),
            Self::Float(f) => out.extend_from_slice(&f.to_le_bytes()),
            Self::Bytes(b) | Self::Raw(b) => out.extend_from_slice(b),
            Self::List(items) => {
                out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for item in items { item.encode_node(out); }
            }
            Self::Dict(pairs) => {
                out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
                for (k, v) in pairs {
                    k.encode_node(out);
                    v.encode_node(out);
                }
            }
        }
    }

    // a nested node framed as a u32le tag and a u32le length before the payload
    pub fn encode_node(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tag().to_le_bytes());
        let len_at = out.len();
        out.extend_from_slice(&0u32.to_le_bytes());
        self.encode_body(out);
        let len = (out.len() - len_at - 4) as u32;
        out[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
    }

    // `None` on bad sizes or trailing bytes or a depth bomb
    pub fn decode_body(tag: u32, bytes: &[u8]) -> Option<WireValue> {
        Self::parse_body(tag, bytes, 0)
    }

    fn parse_body(t: u32, bytes: &[u8], depth: u32) -> Option<WireValue> {
        if depth > MAX_WIRE_DEPTH { return None; }
        match t {
            tag::NONE => bytes.is_empty().then_some(Self::None),
            tag::BOOL => (bytes.len() == 1).then(|| Self::Bool(bytes[0] != 0)),
            tag::INT => {
                let arr: [u8; 16] = bytes.try_into().ok()?;
                Some(Self::Int(i128::from_le_bytes(arr)))
            }
            tag::FLOAT => {
                let arr: [u8; 8] = bytes.try_into().ok()?;
                Some(Self::Float(f64::from_le_bytes(arr)))
            }
            tag::BYTES => Some(Self::Bytes(bytes.to_vec())),
            tag::RAW => Some(Self::Raw(bytes.to_vec())),
            tag::LIST | tag::DICT => {
                let mut pos = 0usize;
                let count = Self::read_u32(bytes, &mut pos)? as usize;
                // each node frame takes at least 8 bytes so a count bomb fails before allocating
                if count > (bytes.len() - pos) / 8 { return None; }
                let val = if t == tag::LIST {
                    let mut items = Vec::with_capacity(count);
                    for _ in 0..count { items.push(Self::parse_node(bytes, &mut pos, depth + 1)?); }
                    Self::List(items)
                } else {
                    let mut pairs = Vec::with_capacity(count);
                    for _ in 0..count {
                        let k = Self::parse_node(bytes, &mut pos, depth + 1)?;
                        let v = Self::parse_node(bytes, &mut pos, depth + 1)?;
                        pairs.push((k, v));
                    }
                    Self::Dict(pairs)
                };
                (pos == bytes.len()).then_some(val)
            }
            _ => None,
        }
    }

    fn parse_node(bytes: &[u8], pos: &mut usize, depth: u32) -> Option<WireValue> {
        let t = Self::read_u32(bytes, pos)?;
        let len = Self::read_u32(bytes, pos)? as usize;
        let payload = bytes.get(*pos..*pos + len)?;
        *pos += len;
        Self::parse_body(t, payload, depth)
    }

    fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
        let b = bytes.get(*pos..*pos + 4)?;
        *pos += 4;
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }
}

