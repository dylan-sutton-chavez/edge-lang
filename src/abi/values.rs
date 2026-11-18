use crate::abi::{classify_decode, DecodeBits as DB, ErrorKind, PrimitiveBytes as PB, WireValue, MAX_WIRE_DEPTH};
use crate::lang::ErrKind;
use crate::value::{DictMap, HeapObj, HeapPool, Val, VmErr};
use alloc::{rc::Rc, string::{String, ToString}, vec::Vec};
use core::cell::RefCell;

/* rebuilds the `VmErr` `abi_kind` read and matches every kind so none slips into `Raised` */
pub fn error_from_kind(kind: u32, msg: String) -> VmErr {
    match ErrorKind::from_u32(kind) {
        Some(ErrorKind::Type) => VmErr::TypeMsg(msg),
        Some(ErrorKind::Value) => VmErr::Of(ErrKind::Value, msg),
        Some(ErrorKind::Runtime) => VmErr::Of(ErrKind::Runtime, msg),
        Some(ErrorKind::Attribute) => VmErr::Attribute(msg),
        Some(ErrorKind::Index) => VmErr::Of(ErrKind::Index, msg),
        Some(ErrorKind::Key) => VmErr::Of(ErrKind::Key, msg),
        // an end a plugin passes back unread raises the class the runtime names for it
        Some(ErrorKind::Custom) if msg == crate::abi::op::ITER_END => VmErr::Of(ErrKind::Stop, String::new()),
        // a custom kind carries its class in `msg` as written and passes through unchanged
        Some(ErrorKind::Custom) | None => VmErr::Raised(msg),
    }
}

/* a wire tree as a heap value keyed by text alone and split like `classify_encode` */
pub fn from_wire(heap: &mut HeapPool, w: &WireValue) -> Result<Val, VmErr> {
    Ok(match w {
        WireValue::None => Val::none(),
        WireValue::Bool(b) => Val::bool(*b),
        WireValue::Int(i) => heap.int(*i)?,
        WireValue::Float(f) => Val::float(*f),
        WireValue::Bytes(b) => {
            let s = core::str::from_utf8(b).map_err(|_| VmErr::Value("invalid UTF-8 in wire str"))?;
            heap.alloc(HeapObj::Str(s.to_string()))?
        }
        WireValue::Raw(b) => heap.alloc(HeapObj::Bytes(b.clone()))?,
        WireValue::List(items) => {
            let vals = items.iter().map(|it| from_wire(heap, it)).collect::<Result<Vec<_>, _>>()?;
            heap.alloc(HeapObj::List(Rc::new(RefCell::new(vals))))?
        }
        WireValue::Dict(pairs) => {
            let mut map = DictMap::new();
            for (k, v) in pairs {
                let WireValue::Bytes(kb) = k else { return Err(VmErr::Type("wire dict keys must be str")); };
                let ks = core::str::from_utf8(kb).map_err(|_| VmErr::Value("invalid UTF-8 in wire str"))?;
                let key = heap.alloc(HeapObj::Str(ks.to_string()))?;
                let val = from_wire(heap, v)?;
                map.insert(key, val, heap);
            }
            heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(map))))?
        }
    })
}

/* a foreign value as a wire int unless past i128 or of a homebound kind */
pub(crate) fn foreign_int(heap: &HeapPool, v: Val) -> Option<i128> {
    heap.foreign_kind(v).filter(|k| k.transfer == crate::value::foreign::Transfer::Copy)?;
    heap.foreign_i128(v)
}

/* a heap value as a wire tree or none for cycles depth and local values */
fn val_to_wire(heap: &HeapPool, v: Val, depth: u32, seen: &mut Vec<u64>) -> Option<WireValue> {
    if depth > MAX_WIRE_DEPTH { return None; }
    match classify_decode(v.0) {
        // `classify_decode` gives a float eight bytes and an int sixteen
        DB::Primitive { bytes, .. } => Some(match bytes {
            PB::None => WireValue::None,
            PB::Bool(b) => WireValue::Bool(b != 0),
            PB::Eight(a) => WireValue::Float(f64::from_le_bytes(a)),
            PB::Sixteen(a) => WireValue::Int(i128::from_le_bytes(a)),
        }),
        DB::Heap => {
            // a shared reference is fine but meeting a value again mid walk is a cycle
            let guard = |heap: &HeapPool, seen: &mut Vec<u64>, items: &[Val]| -> Option<WireValue> {
                seen.push(v.0);
                let items = items.iter().map(|it| val_to_wire(heap, *it, depth + 1, seen)).collect::<Option<Vec<_>>>()?;
                seen.pop();
                Some(WireValue::List(items))
            };
            if seen.contains(&v.0) { return None; }
            if v.is_foreign() { return foreign_int(heap, v).map(WireValue::Int); }
            match heap.get(v) {
                HeapObj::Str(s) => Some(WireValue::Bytes(s.as_bytes().to_vec())),
                HeapObj::Bytes(b) => Some(WireValue::Raw(b.clone())),
                HeapObj::List(rc) => guard(heap, seen, &rc.borrow()),
                HeapObj::Tuple(t) => guard(heap, seen, t),
                HeapObj::Dict(rc) => {
                    seen.push(v.0);
                    let mut pairs = Vec::new();
                    for (k, val) in rc.borrow().iter() {
                        let key = match val_to_wire(heap, k, depth + 1, seen)? {
                            key @ WireValue::Bytes(_) => key,
                            _ => return None,
                        };
                        pairs.push((key, val_to_wire(heap, val, depth + 1, seen)?));
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

/* `v` as the wire tree a host reads from the top of it down */
pub fn to_wire(heap: &HeapPool, v: Val) -> Option<WireValue> {
    val_to_wire(heap, v, 0, &mut Vec::new())
}
