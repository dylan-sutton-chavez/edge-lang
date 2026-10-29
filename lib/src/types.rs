use alloc::string::String;

use edge::lang::{Args, Cx, Intrinsic, KindId, Ret, Sig, TYPE};
use edge::value::foreign::Kind;
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::fail::{Fail, bare, fail};
use crate::{Kernel, Lang, Site};

/* the kind of the one value the ellipsis names typed as the runtime names it */
pub const fn dots<L: Lang>() -> Kind {
    Kind {
        name: "dots",
        type_name: L::NAMES.ellipsis_type,
        family: "dots",
        repr: |_| Ok(String::from(L::NAMES.ellipsis)),
        // the one value is equal to itself alone and hashes as one constant
        hash: |_, _| 0x2E2E_2E2E,
        eq: |_, _, _| true,
        ..Kind::PLAIN
    }
}

/* whether a value is of a type or a tuple of types */
pub const fn is_a<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), is_a_run::<S>).fast2(is_a_fast::<S::Lang>).intrinsic(Intrinsic::IsA) }

/* whether a class reaches a class or a tuple of classes */
pub const fn is_sub<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), is_sub_run::<S>).fast2(is_sub_fast::<S::Lang>) }

/* the class of a user instance and otherwise the type object its kind names */
pub const fn type_of<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), type_of_run).flags(TYPE) }

/* whether a value can be called */
pub const fn callable<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), callable_run).fast(callable_fast) }

/* the truth of a value where nothing given is false */
pub const fn truth<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), truth_run).fast(truth_fast).flags(TYPE) }

/* whether raised exception `exc` is of the class or tuple a handler names */
pub fn match_exc<L: Lang>(heap: &HeapPool, exc: Val, ty: Val) -> Result<bool, VmErr> {
    is_of::<L>(heap, exc, ty, || heap.type_name(exc)).map_err(|()| bare::<L>(Fail::HandlerArg))
}

/* whether value `of` is of the class or tuple `info` naming its type lazily */
#[inline(always)]
fn is_of<'h, L: Lang>(heap: &'h HeapPool, of: Val, info: Val, ty: impl Fn() -> &'h str) -> Result<bool, ()> {
    // the value's meta is read once for every class the walk meets
    let meta = heap.meta_of(of);
    let one = |t: Val| match heap.meta_of(t) {
        Some(m) if m.is_type_meta() => Ok(of_type::<L>(heap, meta, heap.meta_name(m).unwrap_or(""), &ty)),
        Some(m) if m.is_class_meta() => Ok(meta.is_some_and(|c| c.is_heap() && heap.is_subclass(c, t))),
        _ => Err(()),
    };
    match heap.try_get(info) {
        Some(HeapObj::Tuple(items)) => {
            // a long tuple counts the classes it reads as native work
            if items.len() > 64 { heap.spend(items.len()); }
            // a member that is no class fails once reached and a repeat answers as before
            let mut last = Val::undef();
            for &t in items {
                if t.raw() == last.raw() { continue; }
                if one(t)? { return Ok(true); }
                last = t;
            }
            Ok(false)
        }
        _ => one(info),
    }
}

/* whether a value of meta `meta` is of builtin type `name` */
#[inline(always)]
fn of_type<'h, L: Lang>(heap: &'h HeapPool, meta: Option<Val>, name: &str, ty: &impl Fn() -> &'h str) -> bool {
    let tree = L::TREE;
    if name == tree.root { return true; }
    match meta {
        // an instance matches by its type's name or through its class's bases
        Some(c) if c.is_heap() => match heap.type_named(c) {
            Some(n) => tree.derives(n, name),
            None => tree.base_of(heap, c).is_some_and(|b| tree.derives(b, name)),
        },
        // a type object also matches by its own name along the tree
        Some(m) if m.is_type_meta() && heap.meta_name(m).is_some_and(|n| tree.derives(n, name)) => true,
        _ => {
            let ty = ty();
            tree.derives(ty, name) || tree.counts_as(ty, name)
        }
    }
}

/* each class `info` names handed to `one` with its heap object until one matches */
fn each<'h>(heap: &'h HeapPool, info: Val, one: impl Fn(Val, Option<&'h HeapObj>) -> Result<bool, Fail>) -> Result<bool, Fail> {
    match heap.try_get(info) {
        Some(HeapObj::Tuple(items)) => {
            // a long tuple counts the classes it reads as native work
            if items.len() > 64 { heap.spend(items.len()); }
            // a member that is no class fails once the walk reaches it
            for &t in items { if one(t, heap.try_get(t))? { return Ok(true); } }
            Ok(false)
        }
        of => one(info, of),
    }
}

fn is_a_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let found = {
        let (cx, obj): (&Cx<'_, '_>, Val) = (cx, a.pos[0]);
        is_of::<S::Lang>(cx.heap(), obj, a.pos[1], move || cx.type_name(obj))
    };
    match found { Ok(b) => Ok(Val::bool(b).into()), Err(()) => Err(fail::<S>(cx, Fail::TypeArg)) }
}

/* `is_a` on the heap alone with its errors left to the full call */
fn is_a_fast<L: Lang>(heap: &mut HeapPool, obj: Val, info: Val) -> Option<Val> {
    let heap = &*heap;
    if matches!(heap.try_get(obj), Some(HeapObj::Coroutine(_))) && names_coro(heap, info) { return None; }
    is_of::<L>(heap, obj, info, || heap.type_name(obj)).ok().map(Val::bool)
}

/* whether `info` names the kind generators and coroutines share which only the engine splits */
fn names_coro(heap: &HeapPool, info: Val) -> bool {
    let coro = |t: Val| heap.type_kind(t) == Some(KindId::Coro);
    match heap.try_get(info) {
        Some(HeapObj::Tuple(items)) => { if items.len() > 64 { heap.spend(items.len()); } items.iter().any(|&t| coro(t)) }
        _ => coro(info),
    }
}

fn is_sub_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    match subclass::<S::Lang>(cx.heap(), a.pos[0], a.pos[1]) {
        Ok(b) => Ok(Val::bool(b).into()),
        Err(f) => Err(fail::<S>(cx, f)),
    }
}

/* `is_sub` with its errors left to the full call */
fn is_sub_fast<L: Lang>(heap: &mut HeapPool, sub: Val, info: Val) -> Option<Val> { subclass::<L>(heap, sub, info).ok().map(Val::bool) }

/* a builtin type reaches types along the tree and a user class along its bases */
fn subclass<L: Lang>(heap: &HeapPool, sub: Val, info: Val) -> Result<bool, Fail> {
    let tree = L::TREE;
    let own = match heap.type_object_name(sub) {
        Some(n) => Some(n),
        None if heap.is_class(sub) => None,
        None => return Err(Fail::ClassArg),
    };
    each(heap, info, |t, _| match heap.type_object_name(t) {
        // every class and type derives from the root
        Some(n) if n == tree.root => Ok(true),
        // a user class reaches a builtin type only through a base of the tree
        Some(n) => Ok(own.or_else(|| tree.base_of(heap, sub)).is_some_and(|b| tree.derives(b, n) || tree.counts_as(b, n))),
        None if heap.is_class(t) => Ok(own.is_none() && heap.is_subclass(sub, t)),
        None => Err(Fail::ClassesArg),
    })
}

fn type_of_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let o = a.pos[0];
    // an instance reports the class or type it was made of
    if let Some(cls) = cx.heap().class_of(o) { return Ok(cls.into()); }
    Ok(cx.type_of(o)?.into())
}

fn callable_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Val::bool(cx.callable(a.pos[0])).into()) }

/* whether a value off the heap is a foreign value whose kind calls */
fn callable_fast(heap: &mut HeapPool, x: Val) -> Option<Val> { (!x.is_heap()).then(|| Val::bool(heap.foreign_kind(x).is_some_and(|k| k.call.is_some()))) }

fn truth_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Val::bool(a.get(0).is_some_and(|v| cx.truthy(v))).into()) }

fn truth_fast(heap: &mut HeapPool, x: Val) -> Option<Val> { Some(Val::bool(heap.truthy(x))) }
