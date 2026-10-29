use alloc::string::String;

use edge::lang::{Args, Cx, ErrKind, Ret, Sig};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

/* a field of a value by name where a default answers only a missing field */
pub const fn get<S: Site>() -> Kernel { Kernel::new(Sig::range(2, 3), get_run::<S>).fast2(get_fast) }

/* whether a value has a field by name */
pub const fn has<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), has_run::<S>).fast2(has_fast) }

/* stores a field of a value by name */
pub const fn set<S: Site>() -> Kernel { Kernel::new(Sig::exact(3), set_run::<S>) }

/* removes a field of a value by name */
pub const fn delete<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), delete_run::<S>) }

// the longest field name copied aside on the stack and not the heap
const SHORT: usize = 64;

/* runs `f` on field name `v` copied so the text borrows nothing of `cx` */
fn named<S: Site, R>(cx: &mut Cx<'_, '_>, v: Val, f: impl FnOnce(&mut Cx<'_, '_>, &str) -> Result<R, VmErr>) -> Result<R, VmErr> {
    let mut buf = [0u8; SHORT];
    let short = match cx.text(v) {
        Some(s) => buf.get_mut(..s.len()).map(|b| { b.copy_from_slice(s.as_bytes()); s.len() }),
        None => return Err(fail::<S>(cx, Fail::AttrName(v))),
    };
    if let Some(name) = short.and_then(|n| core::str::from_utf8(&buf[..n]).ok()) { return f(cx, name); }
    // a longer name goes to the heap
    let long = cx.text(v).map(String::from).unwrap_or_default();
    f(cx, &long)
}

fn get_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (o, default) = (a.pos[0], a.get(2));
    named::<S, _>(cx, a.pos[1], |cx, name| match (cx.get_attr(o, name), default) {
        (Err(e), Some(d)) if e.is_kind(ErrKind::Attribute) => Ok(d.into()),
        (found, _) => Ok(found?.into()),
    })
}

fn has_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let o = a.pos[0];
    named::<S, _>(cx, a.pos[1], |cx, name| match cx.get_attr(o, name) {
        Ok(_) => Ok(Val::bool(true).into()),
        Err(e) if e.is_kind(ErrKind::Attribute) => Ok(Val::bool(false).into()),
        Err(e) => Err(e),
    })
}

/* `get` of a plain user instance through its fields and its line of classes */
fn get_fast(heap: &mut HeapPool, o: Val, name: Val) -> Option<Val> {
    match read(heap, o, name)? {
        Read::Field(v) => Some(v),
        // a function of the class binds to the instance read off
        Read::Member(m) if matches!(heap.try_get(m), Some(HeapObj::Func { .. })) || heap.host_of(m).is_some() => heap.bind(o, m).ok(),
        Read::Member(m) => Some(m),
        // the missing field raises through the full call
        Read::Missing => None,
    }
}

/* `has` of a plain user instance answered from its fields and its line of classes */
fn has_fast(heap: &mut HeapPool, o: Val, name: Val) -> Option<Val> { Some(Val::bool(!matches!(read(heap, o, name)?, Read::Missing))) }

/* where a read of a field off a plain user instance lands */
enum Read { Field(Val), Member(Val), Missing }

/* the read of `name` off user instance `o` with none wherever the engine decides */
fn read(heap: &HeapPool, o: Val, name: Val) -> Option<Read> {
    let (Some((cls, attrs)), Some(HeapObj::Str(text))) = (heap.instance(o), heap.try_get(name)) else { return None };
    let own = {
        let attrs = attrs.borrow();
        // a name is interned so the key that spells it is mostly this very value
        let own = attrs.iter().find(|&(k, _)| k.raw() == name.raw());
        own.or_else(|| attrs.iter().find(|&(k, _)| matches!(heap.try_get(k), Some(HeapObj::Str(s)) if s == text))).map(|(_, v)| v)
    };
    let mut c = cls;
    loop {
        // a builtin base or several bases leave the read to the engine
        if !heap.is_class(c) { return None; }
        if own.is_none() && let Some(m) = heap.own_member(c, text) { return Some(Read::Member(m)); }
        match *heap.bases(c) {
            [] => break,
            [b] => c = b,
            _ => return None,
        }
    }
    if let Some(v) = own { return Some(Read::Field(v)); }
    // the constructor every class ends in answers its name as the runtime spells it
    (text != heap.names().constructor).then_some(Read::Missing)
}

fn set_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (o, v) = (a.pos[0], a.pos[2]);
    named::<S, _>(cx, a.pos[1], |cx, name| cx.set_attr(o, name, v))?;
    Ok(Val::none().into())
}

fn delete_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let o = a.pos[0];
    named::<S, _>(cx, a.pos[1], |cx, name| cx.del_attr(o, name))?;
    Ok(Val::none().into())
}
