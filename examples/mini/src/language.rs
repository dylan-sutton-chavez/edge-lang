use alloc::string::String;
use alloc::vec::Vec;

use edge::lang::{
    Act, Body, CallFault, Callee, Cx, Descriptor, EngineError, ErrKind, Falsy, Fit, Front, Gen, IntDiv, IntMod, Intrinsic, KindId, Language,
    Lock, MapIter, Misfit, Mode, Names, Native, Natives, NegPow, NumEq, Op, Overflow, Pattern, Policy, StrIter, Use, ZeroDiv, spelled,
};
use edge::value::foreign::{BinOp, Built, Kind};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use edge::s;

use crate::{Mini, NAMES};

impl Language for Mini {
    const VERSION: u32 = 1;
    // the corpus' rules as mini's own choices
    const POLICY: Policy = Policy {
        int_overflow: Overflow::Promote,
        int_div: IntDiv::ToFloat,
        int_mod: IntMod::Truncated,
        zero_div: ZeroDiv::Error,
        neg_pow: NegPow::ToFloat,
        numeric_eq: NumEq::Unified,
        bool_is_int: true,
        falsy: Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::NAN).or(Falsy::EMPTY_TEXT),
        identity_first: true,
        neg_index: true,
        str_iter: StrIter::CodePoints,
        map_iter: MapIter::Keys,
        // off for now since reading keys puts a call on every attribute miss
        map_fields: false,
        builtins_scope: true,
        receiver_slot: true,
        // kept so the limit cases keep their numbers
        recursion_limit: 1000,
        max_int_digits: Some(4300),
    };
    const NATIVES: &'static Natives = &crate::NATIVES;
    const NAMES: &'static Names = &NAMES;
    const FRONT: Front = crate::FRONT;
    const DESCRIPTOR: &'static Descriptor = &crate::DESCRIPTOR;
    const WIDE: Option<&'static Kind> = Some(&crate::lang::WIDE);
    const KINDS: &'static [&'static Kind] = &[&crate::lang::TIME, &crate::lang::DURATION];
    const BUILT: Built = Built { ellipsis: Some(&crate::lang::DOTS), slice: Some(&crate::lang::SLICE) };
    type Ops = ();

    fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { lib::fmt::render::hook::<Mini>(cx, v, mode) }

    fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { lib::fmt::spec::hook::<Mini>(cx, v, spec) }

    fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { lib::types::match_exc::<Mini>(heap, exc, class) }

    fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { crate::words::arity(n, owner, m) }

    fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { message(cx, e) }
}

/* every engine error in the words the corpus reads */
fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String {
    use EngineError as E;
    match e {
        &E::Unsupported { op, a, b } => unsupported(cx, op, a, b),
        &E::Cannot { act, recv } => cannot(cx, act, recv),
        &E::BadIndex { recv, index } => match cx.kind(recv) {
            KindId::Str => s!("string indices must be integers, not '", str cx.type_name(index), "'"),
            // bytes name their index as one byte as the corpus reads them
            KindId::Bytes => s!("byte indices must be integers or slices, not ", str cx.type_name(index)),
            _ => s!(str cx.type_name(recv), " indices must be integers or slices, not ", str cx.type_name(index)),
        },
        E::BadSlice { step: true, .. } => String::from("slice step must be an integer"),
        E::BadSlice { .. } => s!("slice indices must be integers or ", str NAMES.nil, " or have an ", str NAMES.index_method, " method"),
        &E::OutOfRange { recv, store } => String::from(match (cx.kind(recv), store) {
            (KindId::List, true) => "list assignment index out of range",
            (KindId::List, false) => "list index out of range",
            (KindId::Tuple, _) => "tuple index out of range",
            (KindId::Str, _) => "string index out of range",
            (KindId::Range, _) => "range object index out of range",
            (KindId::ByteArray, _) => "bytearray index out of range",
            _ => "index out of range",
        }),
        E::TooWide(_) => s!("cannot fit '", str NAMES.kind(KindId::Int), "' into an index-sized integer"),
        &E::MissingKey(k) => cx.render(k, Mode::Repr).unwrap_or_default(),
        E::Changed(KindId::Set) => String::from("Set changed size during iteration"),
        E::Changed(_) => String::from("dictionary changed size during iteration"),
        E::ZeroStep { range: true } => s!(str NAMES.kind(KindId::Range), "() arg 3 must not be zero"),
        E::ZeroStep { range: false } => String::from("slice step cannot be zero"),
        E::Arity { callee, fit } => arity(cx, callee, fit),
        E::Call(f) => call_fault(cx, f),
        E::Undefined(n) => s!("name '", str n, "' is not defined"),
        E::Unbound { name: Some(n), free: false } => s!("cannot access local variable '", str n, "' where it is not associated with a value"),
        E::Unbound { name: Some(n), free: true } => s!("cannot access free variable '", str n, "' where it is not associated with a value in enclosing scope"),
        E::Unbound { name: None, .. } => String::from("cannot access a local variable that is not associated with a value"),
        E::NoAttribute { recv, name, store: true } => s!("'", str cx.type_name(*recv), "' object has no attribute '", str name, "' and no __dict__ for setting new attributes"),
        E::NoAttribute { recv, name, .. } => match cx.heap().try_get(*recv) {
            Some(HeapObj::Module(module, _)) => s!("module '", str module, "' has no attribute '", str name, "'"),
            _ if cx.heap().is_class(*recv) => s!("type object '", str cx.heap().class_name(*recv).unwrap_or(""), "' has no attribute '", str name, "'"),
            _ => s!("'", str cx.type_name(*recv), "' object has no attribute '", str name, "'"),
        },
        E::ReadOnly { recv, name, lock } => match lock {
            Lock::Type => match cx.heap().type_object_name(*recv) {
                Some(t) => s!("cannot set '", str name, "' attribute of immutable type '", str t, "'"),
                None => s!("cannot set '", str name, "' attribute of immutable type '", str cx.type_name(*recv), "'"),
            },
            Lock::Native => s!("'", str cx.type_name(*recv), "' object attribute '", str name, "' is read-only"),
            Lock::Fixed => String::from("readonly attribute"),
            Lock::Plain => s!("attribute '", str name, "' of '", str cx.type_name(*recv), "' objects is not writable"),
        },
        E::ZeroDiv => String::from("division by zero"),
        E::ZeroPow => String::from("zero to a negative power"),
        E::Overflow => String::from(NAMES.overflow),
        E::FloatRange => String::from("(34, 'Numerical result out of range')"),
        E::NegativeShift => String::from("negative shift count"),
        E::ByteRange => String::from("byte must be in range(0, 256)"),
        &E::Unpack { want, got, more: false, rest } => s!("not enough values to unpack (expected ", str if rest { "at least " } else { "" }, int want as i64, ", got ", int got.unwrap_or(0) as i64, ")"),
        &E::Unpack { want, got: Some(g), .. } => s!("too many values to unpack (expected ", int want as i64, ", got ", int g as i64, ")"),
        &E::Unpack { want, got: None, .. } => s!("too many values to unpack (expected ", int want as i64, ")"),
        &E::SliceSize { size, span } => s!("attempt to assign sequence of size ", int size as i64, " to extended slice of size ", int span as i64),
        &E::Uneven { arg, longer } => {
            let before = if arg == 1 { String::from("argument 1") } else { s!("arguments 1-", int i64::from(arg)) };
            s!(str cx.names().iters[edge::lang::iters::ZIP], "() argument ", int i64::from(arg) + 1, str if longer { " is longer than " } else { " is shorter than " }, str &before)
        }
        E::Generator(g) => generator(g),
        E::Pattern(p) => match p {
            &Pattern::OnePositional(subject) => s!(str cx.type_name(subject), "() accepts 1 positional sub-pattern"),
            Pattern::NoFields => s!("class pattern accepts no positional sub-patterns without ", str NAMES.fields.match_args),
            Pattern::TooMany => s!("class pattern got more positional sub-patterns than ", str NAMES.fields.match_args),
        },
        &E::Stop(v) | &E::Exit(v) | &E::Assert(v) => if v.is_undef() || v.is_none() { String::new() } else { cx.render(v, Mode::Str).unwrap_or_default() },
        E::Close => String::new(),
        E::Recursion => String::from("maximum recursion depth exceeded"),
        e => edge::lang::plain(cx, e),
    }
}

/* an operator's refusal in the corpus' forms with the operator as mini spells it */
fn unsupported(cx: &Cx<'_, '_>, op: Op, a: Val, b: Option<Val>) -> String {
    if matches!(op, Op::Bin(BinOp::Pow) | Op::InPlace(BinOp::Pow)) { return s!(str spelled(cx, Op::Bin(BinOp::Pow)), " requires numeric operands"); }
    let (sym, ta) = (spelled(cx, op), cx.type_name(a));
    let Some(b) = b else { return s!("bad operand type for unary ", str sym, ": '", str ta, "'") };
    let tb = cx.type_name(b);
    let seq = |k: KindId| matches!(k, KindId::Str | KindId::List | KindId::Tuple);
    let buffer = |k: KindId| matches!(k, KindId::Bytes | KindId::ByteArray);
    let (ka, kb) = (cx.kind(a), cx.kind(b));
    match op {
        Op::Bin(BinOp::Add) | Op::InPlace(BinOp::Add) if seq(ka) => s!("can only concatenate ", str ta, " (not \"", str tb, "\") to ", str ta),
        Op::Bin(BinOp::Add) | Op::InPlace(BinOp::Add) if buffer(ka) => s!("can't concat ", str tb, " to ", str ta),
        Op::Bin(BinOp::Mul) | Op::InPlace(BinOp::Mul) if seq(ka) || buffer(ka) => s!("can't multiply sequence by non-int of type '", str tb, "'"),
        Op::Bin(BinOp::Mul) | Op::InPlace(BinOp::Mul) if seq(kb) || buffer(kb) => s!("can't multiply sequence by non-int of type '", str ta, "'"),
        Op::Cmp(_) => s!("'", str sym, "' not supported between instances of '", str ta, "' and '", str tb, "'"),
        // a text holds only texts which names the item it was asked for
        Op::In => s!("'in <string>' requires string as left operand, not ", str ta),
        _ => s!("unsupported operand type(s) for ", str sym, ": '", str ta, "' and '", str tb, "'"),
    }
}

/* what a value cannot do in the corpus' forms */
fn cannot(cx: &Cx<'_, '_>, act: Act, recv: Option<Val>) -> String {
    let t = recv.map_or("", |v| cx.type_name(v));
    match act {
        Act::Iterate => s!("'", str t, "' object is not iterable"),
        Act::Next => s!("'", str t, "' object is not an iterator"),
        Act::Call => s!("'", str t, "' object is not callable"),
        Act::Subscript => s!("'", str t, "' object is not subscriptable"),
        Act::AssignItem if recv.is_none() => String::from("object does not support item assignment"),
        Act::AssignItem => s!("'", str t, "' object does not support item assignment"),
        Act::DeleteItem if recv.is_none() => String::from("object does not support item deletion"),
        Act::DeleteItem => s!("'", str t, "' object does not support item deletion"),
        Act::AssignSlice => s!("'", str t, "' object does not support slice assignment"),
        Act::Slice => String::from("object is not sliceable"),
        Act::Sequence => String::from("slice requires a sequence"),
        Act::Size => {
            // the length native as the running runtime names it
            let size = cx.natives().intrinsic(Intrinsic::Len).map_or("size", |n| n.name);
            s!("object of type '", str t, "' has no ", str size, "()")
        }
        Act::Hash { at, inner } => {
            let inner = NAMES.kind(inner);
            match at {
                Use::Plain => s!("unhashable type: '", str inner, "'"),
                Use::Key => s!("cannot use '", str t, "' as a dict key (unhashable type: '", str inner, "')"),
                Use::Element => s!("cannot use '", str t, "' as a set element (unhashable type: '", str inner, "')"),
            }
        }
        Act::Await => s!("'", str t, "' object can't be awaited"),
        Act::Map { spread: false } => s!("'", str t, "' object is not a mapping"),
        Act::Map { spread: true } => s!("Value after ** must be a mapping, not ", str t),
        Act::Contain => s!("argument of type '", str t, "' is not a container or iterable"),
        Act::Index => s!("'", str t, "' object cannot be interpreted as an integer"),
        Act::Raise { throw: false } => s!("exceptions must derive from ", str NAMES.error(ErrKind::Base)),
        Act::Raise { throw: true } => s!("exceptions must be classes or instances deriving from ", str NAMES.error(ErrKind::Base), ", not ", str t),
        Act::Bytes => String::from("a bytes-like object is required"),
        Act::Spec => String::from("format spec must be a string"),
        Act::Keyword => String::from("keywords must be strings"),
        Act::DeleteAttr => String::from("an attribute is deleted only off an instance or a class"),
        Act::DropArgs => s!(str NAMES.fields.args, " may not be deleted"),
        Act::Resume(Body::Generator) => String::from("not a generator"),
        Act::Resume(Body::Coroutine) => String::from("not a coroutine"),
        Act::Complex => String::from("complex numbers are not supported"),
    }
}

/* a call that misses its callee's signature in the corpus' forms */
fn arity(cx: &Cx<'_, '_>, callee: &Callee, fit: &Fit) -> String {
    let &Callee::Function { ref name, normal, defaults } = callee else { return native_arity(cx, callee, fit) };
    match fit {
        &Fit::Count(given) => {
            let noun = if normal == 1 && defaults == 0 { " positional argument but " } else { " positional arguments but " };
            let verb = if given == 1 { " was given" } else { " were given" };
            if defaults == 0 { s!(str name, "() takes ", int normal as i64, str noun, int given as i64, str verb) }
            else { s!(str name, "() takes from ", int (normal - defaults) as i64, " to ", int normal as i64, str noun, int given as i64, str verb) }
        }
        Fit::Missing { names, keyword, .. } => {
            let quoted: Vec<String> = names.iter().map(|n| s!("'", str n, "'")).collect();
            let list = match quoted.split_last() {
                Some((last, [])) => last.clone(),
                Some((last, rest)) => s!(str &rest.join(", "), str if rest.len() > 1 { ", and " } else { " and " }, str last),
                None => String::new(),
            };
            let kind = if *keyword { " required keyword-only argument" } else { " required positional argument" };
            s!(str name, "() missing ", int names.len() as i64, str kind, str if names.len() == 1 { ": " } else { "s: " }, str &list)
        }
        Fit::Unexpected { key, near: Some(n) } => s!(str name, "() got an unexpected keyword argument '", str key, "'. Did you mean '", str n, "'?"),
        Fit::Unexpected { key, near: None } => s!(str name, "() got an unexpected keyword argument '", str key, "'"),
        Fit::Twice(key) => s!(str name, "() got multiple values for argument '", str key, "'"),
        Fit::ByKeyword(key) => s!(str name, "() got some positional-only arguments passed as keyword arguments: '", str key, "'"),
    }
}

/* a native's misfit through the words mini's arity hook writes today */
fn native_arity(cx: &Cx<'_, '_>, callee: &Callee, fit: &Fit) -> String {
    let &Callee::Native { id, owner } = callee else { return String::new() };
    let Some(n) = cx.natives().get(id) else { return String::new() };
    let m = match fit {
        &Fit::Count(got) => Misfit::Count(got as usize),
        Fit::Unexpected { key, .. } | Fit::ByKeyword(key) => Misfit::Unexpected(key),
        Fit::Twice(key) => Misfit::Twice(key),
        &Fit::Missing { at, .. } => Misfit::Missing(at as usize),
    };
    match crate::words::arity(n, owner.map(|k| NAMES.kind(k)), m) {
        VmErr::TypeMsg(text) => text,
        e => e.message(&NAMES),
    }
}

/* a call its callee refuses in the corpus' forms */
fn call_fault(cx: &mut Cx<'_, '_>, f: &CallFault) -> String {
    match f {
        CallFault::NoArgs { name, keywords: true, .. } => s!(str name, "() takes no keyword arguments"),
        CallFault::NoArgs { name, called: true, .. } => s!(str name, "() takes no arguments"),
        CallFault::NoArgs { name, .. } => s!(str name, " takes no arguments"),
        CallFault::InitArgs => s!(str NAMES.kind(KindId::Instance), ".", str NAMES.constructor, "() takes exactly one argument (the instance to initialize)"),
        CallFault::ExcKeywords => String::from("exception class takes no keyword arguments"),
        CallFault::CtorArgs => String::from("class constructor takes no arguments"),
        &CallFault::NoReceiver { kind, name } => s!("unbound method ", str NAMES.kind(kind), ".", str name, "() needs an argument"),
        &CallFault::WrongReceiver { kind, name, got } => s!("descriptor '", str name, "' for '", str NAMES.kind(kind), "' objects doesn't apply to a '", str cx.type_name(got), "' object"),
        CallFault::TooMany { name, got } => s!("call '", str name, "': too many arguments (max 255, got ", int *got as i64, ")"),
        CallFault::Instantiate(name) => s!("cannot create '", str name, "' instances"),
        CallFault::Base(Some(name)) => s!("type '", str name, "' is not an acceptable base type"),
        CallFault::Base(None) => String::from("base class must be a class object"),
    }
}

/* the generator protocol in the corpus' forms */
fn generator(g: &Gen) -> String {
    let noun = |b: &Body| match b { Body::Generator => NAMES.kind(KindId::Coro), Body::Coroutine => NAMES.coroutine };
    match g {
        Gen::Running(b) => s!(str noun(b), " already executing"),
        Gen::FreshSend(b) => s!("can't send non-", str NAMES.nil, " value to a just-started ", str noun(b)),
        Gen::Reused => String::from("cannot reuse already awaited coroutine"),
        Gen::IgnoredClose(b) => s!(str noun(b), " ignored ", str NAMES.error(ErrKind::Close)),
        Gen::Wait { inside } => s!("receive() and host calls cannot wait inside", str inside),
        Gen::ClassSuspend => String::from("a class body cannot suspend, await outside it"),
        Gen::Sent => s!("'", str NAMES.kind(KindId::Iter), "' object has no attribute '", str NAMES.send, "'"),
    }
}

/* implements `Language` for `$lang` as mini's own over the natives `$natives` and the names `$names` */
#[doc(hidden)]
#[macro_export]
macro_rules! over {
    ($lang:ident, $natives:expr) => { $crate::over!($lang, $natives, &$crate::NAMES); };
    ($lang:ident, $natives:expr, $names:expr) => {
        impl ::edge::lang::Language for $lang {
            const VERSION: u32 = 1;
            const POLICY: ::edge::lang::Policy = <$crate::Mini as ::edge::lang::Language>::POLICY;
            const NATIVES: &'static ::edge::lang::Natives = $natives;
            const NAMES: &'static ::edge::lang::Names = $names;
            const FRONT: ::edge::lang::Front = $crate::FRONT;
            const DESCRIPTOR: &'static ::edge::lang::Descriptor = &$crate::DESCRIPTOR;
            const WIDE: Option<&'static ::edge::value::foreign::Kind> = <$crate::Mini as ::edge::lang::Language>::WIDE;
            const KINDS: &'static [&'static ::edge::value::foreign::Kind] = <$crate::Mini as ::edge::lang::Language>::KINDS;
            const BUILT: ::edge::value::foreign::Built = <$crate::Mini as ::edge::lang::Language>::BUILT;
            type Ops = ();

            fn show(cx: &mut ::edge::lang::Cx<'_, '_>, v: ::edge::value::Val, mode: ::edge::lang::Mode) -> Result<$crate::__String, ::edge::value::VmErr> {
                <$crate::Mini as ::edge::lang::Language>::show(cx, v, mode)
            }

            fn format(cx: &mut ::edge::lang::Cx<'_, '_>, v: ::edge::value::Val, spec: &str) -> Result<$crate::__String, ::edge::value::VmErr> {
                <$crate::Mini as ::edge::lang::Language>::format(cx, v, spec)
            }

            fn handles(heap: &::edge::value::HeapPool, exc: ::edge::value::Val, class: ::edge::value::Val) -> Result<bool, ::edge::value::VmErr> {
                <$crate::Mini as ::edge::lang::Language>::handles(heap, exc, class)
            }

            fn misfit(n: &::edge::lang::Native, owner: Option<&str>, m: ::edge::lang::Misfit<'_>) -> ::edge::value::VmErr {
                <$crate::Mini as ::edge::lang::Language>::misfit(n, owner, m)
            }

            fn message(cx: &mut ::edge::lang::Cx<'_, '_>, e: &::edge::lang::EngineError) -> $crate::__String {
                <$crate::Mini as ::edge::lang::Language>::message(cx, e)
            }
        }
    };
}
