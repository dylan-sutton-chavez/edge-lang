/* `python.sentinel`, the FOREIGN kind of `sentinel(name)`, an object equal only to itself that reads as its name. */
use alloc::{rc::Rc, string::String};
use core::any::Any;

use crate::value::foreign::{Kind, KindToken, Snapshot, Transfer};
use crate::value::{HeapPool, Val, VmErr};

/* A sentinel, the name it was made with and the text its repr shows. */
#[derive(Debug)]
pub struct Sentinel { pub name: String, pub repr: String }

pub static KIND: Kind = Kind {
    name: "python.sentinel",
    version: 1,
    type_name: "sentinel",
    family: "python.sentinel",
    immutable: false,
    repr: |p| Ok(of(p).repr.clone()),
    truthy: |_| true,
    hash: |p| crate::value::eq::hash_str(&of(p).name),
    // Only the sentinel itself is equal to it.
    eq: |heap, p, other| heap.foreign.token_of(&KIND).and_then(|t| heap.foreign_payload::<Sentinel>(t, other)).is_some_and(|q| core::ptr::eq(of(p), q)),
    cmp: None,
    binop: None,
    unop: None,
    to_f64: None,
    to_i128: None,
    from_i128: None,
    trace: None,
    snapshot: Snapshot::Serialize {
        save: |p, out| {
            let s = of(p);
            for part in [&s.name, &s.repr] { out.extend_from_slice(&(part.len() as u32).to_le_bytes()); out.extend_from_slice(part.as_bytes()); }
        },
        load: |b| {
            let mut at = 0;
            let mut part = || -> Option<String> {
                let n = u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
                let s = core::str::from_utf8(b.get(at + 4..at + 4 + n)?).ok()?.into();
                at += 4 + n;
                Some(s)
            };
            let (name, repr) = (part()?, part()?);
            Some(Rc::new(Sentinel { name, repr }) as Rc<dyn Any>)
        },
    },
    transfer: Transfer::Deny,
};

fn of(p: &dyn Any) -> &Sentinel { p.downcast_ref::<Sentinel>().expect("python.sentinel payload") }

fn token(heap: &mut HeapPool) -> KindToken {
    match heap.foreign.token_of(&KIND) { Some(t) => t, None => heap.register_kind(&KIND) }
}

/* A new sentinel named `name`, shown as `repr` or as its name. */
pub fn make(heap: &mut HeapPool, name: String, repr: Option<String>) -> Result<Val, VmErr> {
    let t = token(heap);
    let repr = repr.unwrap_or_else(|| name.clone());
    heap.foreign_box(t, Rc::new(Sentinel { name, repr }))
}

/* The sentinel `v` holds, None for any other value. */
pub fn get(heap: &HeapPool, v: Val) -> Option<&Sentinel> {
    heap.foreign.token_of(&KIND).and_then(|t| heap.foreign_payload::<Sentinel>(t, v))
}
