mod prelude;
mod recv;
pub mod bytes;
pub mod bytearray;
pub mod dict;
pub mod list;
pub mod numeric;
pub mod object;
pub mod operator;
pub mod set;
pub mod slice;
pub mod string;

use prelude::{VM, Val, VmErr, HeapObj, Host, cold_type};
use crate::s;
use alloc::string::String;

pub type MethodFn = fn(&mut VM, Val, &[Val]) -> Result<(), VmErr>;

/* Builtin-method descriptor table + dispatcher. Bodies live in per-type files (string/bytes/list/dict/set) as `pub fn`, arity is checked uniformly by the dispatcher, so bodies never check it. A descriptor is name + fn ptr + arity range. */
pub struct MethodDesc {
    pub ty: &'static str, // receiver type ("str"/"bytes"/"list"/"dict"/"set"), drives lookup.
    pub name: &'static str,
    pub func: MethodFn,
    pub min_args: u8,
    pub max_args: u8, // 255 = unbounded (variadic).
    pub kind: MethodKind,
    pub post: Post,
    // Whether a call changes its receiver, what it holds or where it stands.
    pub mutating: bool,
    // Whether the result hangs on receiver and arguments alone.
    pub pure: bool,
    // The same body over plain values.
    #[cfg(feature = "pure")]
    pub plain: Option<crate::pure::PlainFn>,
}

/* What a call does to the result a method left, settled once for the table so a call only compares a tag. */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Post {
    None,
    // A bytes method a bytearray ran gives a bytearray where it gave bytes.
    ByteArray,
    // A frozendict or frozenset method that builds a new dict or set builds the frozen form.
    Freeze,
}

// Bytes become bytearrays and built dicts or sets freeze as post asks.
pub(crate) fn post_result<H: Host>(vm: &mut H, post: Post, top: Val) -> Result<Val, VmErr> {
    if post == Post::Freeze {
        if let Some(slot) = vm.heap_mut().try_get_mut(top) {
            match slot {
                HeapObj::Dict(rc) => *slot = HeapObj::FrozenDict(rc.clone()),
                HeapObj::Set(rc) => { let s = rc.borrow().clone(); *slot = HeapObj::FrozenSet(alloc::rc::Rc::new(s)); }
                _ => {}
            }
        }
        return Ok(top);
    }
    let items = match vm.heap().try_get(top) {
        Some(HeapObj::Bytes(b)) => { let b = b.clone(); return vm.heap_mut().alloc(HeapObj::ByteArray(b)); }
        Some(HeapObj::List(l)) => l.borrow().clone(),
        Some(HeapObj::Tuple(t)) => t.clone(),
        _ => return Ok(top),
    };
    let mut out = alloc::vec::Vec::with_capacity(items.len());
    for x in items {
        out.push(match vm.heap().try_get(x) { Some(HeapObj::Bytes(b)) => { let b = b.clone(); vm.heap_mut().alloc(HeapObj::ByteArray(b))? } _ => x });
        vm.root(*out.last().unwrap_or(&x));
    }
    vm.unroot(out.len());
    match vm.heap_mut().try_get_mut(top) {
        Some(HeapObj::List(l)) => *l.borrow_mut() = out,
        Some(HeapObj::Tuple(t)) => *t = out,
        _ => {}
    }
    Ok(top)
}

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() { return false; }
    let mut i = 0;
    while i < a.len() { if a[i] != b[i] { return false; } i += 1; }
    true
}

const fn post_of(ty: &str, name: &str) -> Post {
    // `take_bytes` and `maketrans` hand out bytes from a bytearray too.
    if same(ty, "bytearray") { return if same(name, "take_bytes") || same(name, "maketrans") { Post::None } else { Post::ByteArray }; }
    let builds = (same(ty, "frozendict") && same(name, "fromkeys")) || (same(ty, "frozenset")
        && (same(name, "union") || same(name, "intersection") || same(name, "difference") || same(name, "symmetric_difference")));
    if builds { Post::Freeze } else { Post::None }
}

const fn mutating_of(ty: &str, name: &str) -> bool {
    const LIST: &[&str] = &["__init__", "__setitem__", "append", "clear", "reverse", "extend", "insert", "remove", "pop", "sort"];
    const DICT: &[&str] = &["__init__", "__setitem__", "popitem", "update", "pop", "setdefault", "clear"];
    const SET: &[&str] = &["add", "remove", "discard", "pop", "clear", "update", "intersection_update", "difference_update", "symmetric_difference_update"];
    const BYTEARRAY: &[&str] = &["append", "extend", "pop", "insert", "remove", "clear", "reverse", "resize", "take_bytes"];
    // Stepping a generator, a coroutine or an awaitable moves where it stands.
    const STEPS: &[&str] = &["send", "throw", "close", "__next__"];
    const fn any(names: &[&str], name: &str) -> bool {
        let mut i = 0;
        while i < names.len() { if same(names[i], name) { return true; } i += 1; }
        false
    }
    if same(ty, "list") { return any(LIST, name); }
    if same(ty, "dict") { return any(DICT, name); }
    if same(ty, "set") { return any(SET, name); }
    if same(ty, "bytearray") { return any(BYTEARRAY, name); }
    if same(ty, "BaseException") { return same(name, "__init__") || same(name, "add_note"); }
    if same(ty, "iterator") { return same(name, "__next__"); }
    if same(ty, "async_generator") { return any(&["asend", "athrow", "aclose"], name); }
    if same(ty, "generator") || same(ty, "coroutine") || same(ty, "async_generator_asend") || same(ty, "async_generator_athrow")
        || same(ty, "anext_awaitable") || same(ty, "coroutine_wrapper") { return any(STEPS, name); }
    if same(ty, "memoryview") { return same(name, "release"); }
    if same(ty, "object") { return same(name, "__setattr__") || same(name, "__delattr__"); }
    if same(ty, "op") { return same(name, "__setitem__") || same(name, "__delitem__"); }
    false
}

impl MethodDesc {
    // Why call_pure leaves the method out.
    pub fn excluded(&self) -> Option<&'static str> { if self.pure { None } else { excluded_of(self.ty, self.name, self.mutating) } }
}

// The receiver it changes or the user code and frame it runs.
const fn excluded_of(ty: &str, name: &str, mutating: bool) -> Option<&'static str> {
    const FRAMES: &[&str] = &["generator", "coroutine", "async_generator", "async_generator_asend", "async_generator_athrow", "anext_awaitable", "coroutine_wrapper"];
    const fn any(names: &[&str], name: &str) -> bool {
        let mut i = 0;
        while i < names.len() { if same(names[i], name) { return true; } i += 1; }
        false
    }
    if mutating { return Some("changes its receiver"); }
    if any(FRAMES, ty) { return Some("steps the frame of a generator or a coroutine"); }
    if same(ty, "op") || same(ty, "object") || same(ty, "super") { return Some("runs the dunder of a value, which may be user code"); }
    if same(ty, "iterator") { return Some("an iterator lives in the VM that made it"); }
    if same(ty, "memoryview") { return Some("a view shares the buffer of the VM that holds it"); }
    if same(ty, "BaseException") || same(ty, "BaseExceptionGroup") { return Some("an exception carries its traceback and the frames that raised it"); }
    if same(name, "format") || same(name, "format_map") { return Some("a field may run the __format__, __str__ or __getitem__ of a user value"); }
    if same(name, "sort") { return Some("a key or an item may be user code"); }
    if same(name, "keys") || same(name, "values") || same(name, "items") { return Some("a view reads the dict it came from live"); }
    if same(ty, "str") && same(name, "translate") { return Some("a table may be a mapping with its own __getitem__"); }
    None
}

/* How a call reaches a method, plain through the table, or with the frame for user code. */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Plain,
    // Its arguments are iterables, so a user `__iter__` may stand in for them.
    Iterates,
    Sort,
    Format,
    // `index`, `count` and `remove`, on a list a user `__eq__` decides the match.
    Search,
    // `translate`, whose table may be a mapping with its own `__getitem__`.
    Mapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BuiltinMethodId(u16);

impl BuiltinMethodId {
    #[inline] pub fn name(self) -> &'static str { ALL_METHODS[self.0 as usize].name }
    #[inline] pub(crate) fn kind(self) -> MethodKind { ALL_METHODS[self.0 as usize].kind }
    #[inline] pub(crate) fn post(self) -> Post { ALL_METHODS[self.0 as usize].post }
    pub(crate) fn ty(self) -> &'static str { ALL_METHODS[self.0 as usize].ty }
    pub(crate) fn raw(self) -> u16 { self.0 }
    /* Bounds-checked decode for snapshot restore. */
    pub(crate) fn from_raw(i: u16) -> Option<Self> {
        ((i as usize) < ALL_METHODS.len()).then_some(Self(i))
    }
}

// Builds `ALL_METHODS` grouped by receiver type, `min..max` is the arity (max 255 = variadic).
macro_rules! methods {
    ( $( $ty:literal { $( $name:tt => $func:path, $min:literal .. $max:literal $($pure:ident)? $(=> $body:path)? );* $(;)? } )* ) => {
        &[ $( $( MethodDesc {
            ty: $ty, name: $name, func: methods!(@f $func, $($pure)?),
            min_args: $min, max_args: $max, kind: methods!(@k $name), post: post_of($ty, $name), mutating: mutating_of($ty, $name),
            pure: methods!(@b $($pure)? $($body)?),
            #[cfg(feature = "pure")] plain: methods!(@p $func, $($pure)? $($body)?),
        } ),* ),* ]
    };
    // A pure body is generic over its host.
    (@f $func:path,) => { $func };
    (@f $func:path, pure) => { |vm: &mut VM, recv: Val, pos: &[Val]| $func(vm, recv, pos) };
    (@b) => { false }; (@b pure) => { true }; (@b $body:path) => { true };
    (@p $func:path,) => { None };
    (@p $func:path, pure) => { Some(|h: &mut crate::pure::PlainHost, recv: Val, pos: &[Val]| $func(h, recv, pos)) };
    // In call_pure the dunder runs through the builtin it stands for.
    (@p $func:path, $body:path) => { Some(|h: &mut crate::pure::PlainHost, recv: Val, pos: &[Val]| $body(h, recv, pos)) };
    (@k "sort") => { MethodKind::Sort }; (@k "translate") => { MethodKind::Mapped }; (@k "format") => { MethodKind::Format }; (@k "format_map") => { MethodKind::Format };
    (@k "index") => { MethodKind::Search }; (@k "count") => { MethodKind::Search }; (@k "remove") => { MethodKind::Search };
    (@k "join") => { MethodKind::Iterates }; (@k "extend") => { MethodKind::Iterates }; (@k "update") => { MethodKind::Iterates };
    (@k "fromkeys") => { MethodKind::Iterates }; (@k "union") => { MethodKind::Iterates }; (@k "intersection") => { MethodKind::Iterates };
    (@k "difference") => { MethodKind::Iterates }; (@k "symmetric_difference") => { MethodKind::Iterates };
    (@k "intersection_update") => { MethodKind::Iterates }; (@k "difference_update") => { MethodKind::Iterates };
    (@k "symmetric_difference_update") => { MethodKind::Iterates }; (@k "issubset") => { MethodKind::Iterates };
    (@k "issuperset") => { MethodKind::Iterates }; (@k "isdisjoint") => { MethodKind::Iterates }; (@k $other:tt) => { MethodKind::Plain };
}

// Lookup keys on (ty, name) when compiled, so group however reads best.
const METHODS: &[MethodDesc] = methods! {
    "str" {
        "encode" => string::encode, 0..2 pure;
        "upper" => string::upper, 0..0 pure;
        "lower" => string::lower, 0..0 pure;
        "strip" => string::strip, 0..1 pure;
        "capitalize" => string::capitalize, 0..0 pure;
        "title" => string::title, 0..0 pure;
        "lstrip" => string::lstrip, 0..1 pure;
        "rstrip" => string::rstrip, 0..1 pure;
        "isdigit" => string::isdigit, 0..0 pure;
        "isalpha" => string::isalpha, 0..0 pure;
        "isalnum" => string::isalnum, 0..0 pure;
        "startswith" => string::startswith, 1..3 pure;
        "endswith" => string::endswith, 1..3 pure;
        "find" => string::find, 1..3 pure;
        "count" => string::count, 1..3 pure;
        "split" => string::split, 0..2 pure;
        "join" => string::join, 1..1 pure;
        "replace" => string::replace, 2..3 pure;
        "removeprefix" => string::removeprefix, 1..1 pure;
        "removesuffix" => string::removesuffix, 1..1 pure;
        "splitlines" => string::splitlines, 0..1 pure;
        "partition" => string::partition, 1..1 pure;
        "rpartition" => string::rpartition, 1..1 pure;
        "center" => string::center, 1..2 pure;
        "zfill" => string::zfill, 1..1 pure;
        "rsplit" => string::rsplit, 0..2 pure;
        "casefold" => string::casefold, 0..0 pure;
        "swapcase" => string::swapcase, 0..0 pure;
        "ljust" => string::ljust, 1..2 pure;
        "rjust" => string::rjust, 1..2 pure;
        "expandtabs" => string::expandtabs, 0..1 pure;
        "rfind" => string::rfind, 1..3 pure;
        "index" => string::index, 1..3 pure;
        "rindex" => string::rindex, 1..3 pure;
        "isspace" => string::isspace, 0..0 pure;
        "isupper" => string::isupper, 0..0 pure;
        "islower" => string::islower, 0..0 pure;
        "istitle" => string::istitle, 0..0 pure;
        "isascii" => string::isascii, 0..0 pure;
        "isdecimal" => string::isdecimal, 0..0 pure;
        "isnumeric" => string::isnumeric, 0..0 pure;
        "isprintable" => string::isprintable, 0..0 pure;
        "isidentifier" => string::isidentifier, 0..0 pure;
        "maketrans" => string::maketrans, 1..3 pure;
        "translate" => string::translate, 1..1;
        "format" => framed, 0..255;
    }
    "bytes" {
        "decode" => bytes::decode, 0..2 pure;
        "hex" => bytes::hex, 0..2 pure;
        "startswith" => bytes::startswith, 1..3 pure;
        "endswith" => bytes::endswith, 1..3 pure;
        "find" => bytes::find, 1..3 pure;
        "index" => bytes::index, 1..3 pure;
        "count" => bytes::count, 1..3 pure;
        "replace" => bytes::replace, 2..3 pure;
        "split" => bytes::split, 0..2 pure;
        "lower" => bytes::lower, 0..0 pure;
        "upper" => bytes::upper, 0..0 pure;
        "strip" => bytes::strip, 0..1 pure;
        "lstrip" => bytes::lstrip, 0..1 pure;
        "rstrip" => bytes::rstrip, 0..1 pure;
        "join" => bytes::join, 1..1 pure;
        "fromhex" => bytes::fromhex, 1..1 pure;
        "rfind" => bytes::rfind, 1..3 pure; "rindex" => bytes::rindex, 1..3 pure; "rsplit" => bytes::rsplit, 0..2 pure;
        "splitlines" => bytes::splitlines, 0..1 pure; "partition" => bytes::partition, 1..1 pure; "rpartition" => bytes::rpartition, 1..1 pure;
        "removeprefix" => bytes::removeprefix, 1..1 pure; "removesuffix" => bytes::removesuffix, 1..1 pure;
        "center" => bytes::center, 1..2 pure; "ljust" => bytes::ljust, 1..2 pure; "rjust" => bytes::rjust, 1..2 pure; "zfill" => bytes::zfill, 1..1 pure;
        "expandtabs" => bytes::expandtabs, 0..1 pure; "capitalize" => bytes::capitalize, 0..0 pure; "title" => bytes::title, 0..0 pure;
        "swapcase" => bytes::swapcase, 0..0 pure; "isalnum" => bytes::isalnum, 0..0 pure; "isalpha" => bytes::isalpha, 0..0 pure;
        "isascii" => bytes::isascii, 0..0 pure; "isdigit" => bytes::isdigit, 0..0 pure; "islower" => bytes::islower, 0..0 pure;
        "isspace" => bytes::isspace, 0..0 pure; "istitle" => bytes::istitle, 0..0 pure; "isupper" => bytes::isupper, 0..0 pure;
        "maketrans" => bytes::maketrans, 2..2 pure; "translate" => bytes::translate, 1..2 pure;
    }
    "list" {
        "__init__" => list::init, 0..1;
        "__getitem__" => object::container_getitem, 1..1 pure;
        "__setitem__" => object::container_setitem, 2..2;
        "__len__" => object::container_len, 0..0 pure;
        "index" => list::index, 1..3 pure;
        "count" => list::count, 1..1 pure;
        "copy" => list::copy, 0..0 pure;
        "append" => list::append, 1..1;
        "clear" => list::clear, 0..0;
        "reverse" => list::reverse, 0..0;
        "extend" => list::extend, 1..1;
        "insert" => list::insert, 2..2;
        "remove" => list::remove, 1..1;
        "pop" => list::pop, 0..1;
        "sort" => framed, 0..0;
    }
    "dict" {
        "__init__" => dict::update, 0..1;
        "__getitem__" => object::container_getitem, 1..1 pure;
        "__setitem__" => object::container_setitem, 2..2;
        "__len__" => object::container_len, 0..0 pure;
        "keys" => dict::keys, 0..0;
        "values" => dict::values, 0..0;
        "items" => dict::items, 0..0;
        "copy" => dict::copy, 0..0 pure;
        "popitem" => dict::popitem, 0..0;
        "get" => dict::get, 1..2 pure;
        "update" => dict::update, 0..1;
        "pop" => dict::pop, 1..2;
        "setdefault" => dict::setdefault, 1..2;
        "fromkeys" => dict::fromkeys, 1..2 pure;
    }
    // A frozendict reads like a dict, `copy` gives itself back and `fromkeys` makes a frozendict.
    "frozendict" {
        "keys" => dict::keys, 0..0; "values" => dict::values, 0..0; "items" => dict::items, 0..0;
        "get" => dict::get, 1..2 pure; "copy" => dict::frozen_copy, 0..0 pure; "fromkeys" => dict::fromkeys, 1..2 pure;
        "__getitem__" => object::container_getitem, 1..1 pure; "__len__" => object::container_len, 0..0 pure;
    }
    // A frozenset answers the set methods that change nothing, with frozenset results.
    "frozenset" {
        "union" => set::union, 0..255 pure; "intersection" => set::intersection, 0..255 pure; "difference" => set::difference, 0..255 pure;
        "symmetric_difference" => set::symmetric_difference, 1..1 pure; "issubset" => set::issubset, 1..1 pure;
        "issuperset" => set::issuperset, 1..1 pure; "isdisjoint" => set::isdisjoint, 1..1 pure; "copy" => set::frozen_copy, 0..0 pure;
    }
    "set" {
        "add" => set::add, 1..1;
        "remove" => set::remove, 1..1;
        "discard" => set::discard, 1..1;
        "pop" => set::pop, 0..0;
        "clear" => set::clear, 0..0;
        "update" => set::update, 0..255;
        "copy" => set::copy, 0..0 pure;
        "union" => set::union, 0..255 pure;
        "intersection" => set::intersection, 0..255 pure;
        "difference" => set::difference, 0..255 pure;
        "symmetric_difference" => set::symmetric_difference, 1..1 pure;
        "intersection_update" => set::intersection_update, 0..255;
        "difference_update" => set::difference_update, 0..255;
        "symmetric_difference_update" => set::symmetric_difference_update, 1..1;
        "issubset" => set::issubset, 1..1 pure;
        "issuperset" => set::issuperset, 1..1 pure;
        "isdisjoint" => set::isdisjoint, 1..1 pure;
    }
    "int" {
        "bit_length" => numeric::bit_length, 0..0 pure;
        "bit_count" => numeric::bit_count, 0..0 pure;
        "to_bytes" => numeric::to_bytes, 0..3 pure;
        "from_bytes" => numeric::from_bytes, 1..3 pure;
        "as_integer_ratio" => numeric::int_ratio, 0..0 pure;
        "is_integer" => numeric::int_is_integer, 0..0 pure;
    }
    "float" {
        "is_integer" => numeric::is_integer, 0..0 pure;
        "as_integer_ratio" => numeric::float_ratio, 0..0 pure;
        "hex" => numeric::float_hex, 0..0 pure;
        "fromhex" => numeric::float_fromhex, 1..1 pure;
        "from_number" => numeric::float_from_number, 1..1 pure;
    }
    // Appended groups, keep snapshot method ids stable.
    "dict" {
        "clear" => dict::clear, 0..0;
    }
    "slice" {
        "indices" => slice::indices, 1..1 pure;
    }
    "range" {
        "index" => object::range_index, 1..1 pure;
        "count" => object::range_count, 1..1 pure;
    }
    "BaseException" {
        "__init__" => object::exc_init, 0..255;
        "__str__" => object::exc_str, 0..0;
    }
    "iterator" {
        "__next__" => object::iter_next, 0..0;
        "__iter__" => object::iter_self, 0..0;
    }
    "dict_keys" {
        "isdisjoint" => dict::view_isdisjoint, 1..1 pure;
    }
    "dict_items" {
        "isdisjoint" => dict::view_isdisjoint, 1..1 pure;
    }
    "tuple" {
        "index" => list::index, 1..3 pure;
        "count" => list::count, 1..1 pure;
    }
    "str" {
        "format_map" => framed, 1..1;
    }
    "BaseException" {
        "add_note" => object::exc_add_note, 1..1;
    }
    "generator" {
        "send" => framed, 1..1;
        "throw" => framed, 1..3;
        "close" => framed, 0..0;
        "__next__" => framed, 0..0;
        "__iter__" => framed, 0..0;
    }
    "coroutine" {
        "send" => framed, 1..1;
        "throw" => framed, 1..3;
        "close" => framed, 0..0;
        "__await__" => framed, 0..0;
    }
    "async_generator" {
        "asend" => framed, 1..1;
        "athrow" => framed, 1..3;
        "aclose" => framed, 0..0;
        "__anext__" => framed, 0..0;
        "__aiter__" => framed, 0..0;
    }
    // The awaitables a coroutine and an async generator give, each stepped as a generator is.
    "async_generator_asend" {
        "send" => framed, 1..1; "throw" => framed, 1..3; "close" => framed, 0..0;
        "__next__" => framed, 0..0; "__iter__" => framed, 0..0; "__await__" => framed, 0..0;
    }
    "async_generator_athrow" {
        "send" => framed, 1..1; "throw" => framed, 1..3; "close" => framed, 0..0;
        "__next__" => framed, 0..0; "__iter__" => framed, 0..0; "__await__" => framed, 0..0;
    }
    "anext_awaitable" {
        "send" => framed, 1..1; "throw" => framed, 1..3; "close" => framed, 0..0;
        "__next__" => framed, 0..0; "__iter__" => framed, 0..0; "__await__" => framed, 0..0;
    }
    "coroutine_wrapper" {
        "send" => framed, 1..1; "throw" => framed, 1..3; "close" => framed, 0..0;
        "__next__" => framed, 0..0; "__iter__" => framed, 0..0; "__await__" => framed, 0..0;
    }
    // A bytearray answers the bytes methods, its results bytearrays again, and the methods that change it.
    "bytearray" {
        "decode" => bytes::decode, 0..2 pure; "hex" => bytes::hex, 0..2 pure; "startswith" => bytes::startswith, 1..3 pure;
        "endswith" => bytes::endswith, 1..3 pure; "find" => bytes::find, 1..3 pure; "index" => bytes::index, 1..3 pure;
        "count" => bytes::count, 1..3 pure; "replace" => bytes::replace, 2..3 pure; "split" => bytes::split, 0..2 pure;
        "lower" => bytes::lower, 0..0 pure; "upper" => bytes::upper, 0..0 pure; "strip" => bytes::strip, 0..1 pure;
        "lstrip" => bytes::lstrip, 0..1 pure; "rstrip" => bytes::rstrip, 0..1 pure; "join" => bytes::join, 1..1 pure;
        "fromhex" => bytes::fromhex, 1..1 pure; "append" => bytearray::append, 1..1; "extend" => bytearray::extend, 1..1;
        "pop" => bytearray::pop, 0..1; "insert" => bytearray::insert, 2..2; "remove" => bytearray::remove, 1..1;
        "clear" => bytearray::clear, 0..0; "reverse" => bytearray::reverse, 0..0; "copy" => bytearray::copy, 0..0 pure;
        "rfind" => bytes::rfind, 1..3 pure; "rindex" => bytes::rindex, 1..3 pure; "rsplit" => bytes::rsplit, 0..2 pure;
        "splitlines" => bytes::splitlines, 0..1 pure; "partition" => bytes::partition, 1..1 pure; "rpartition" => bytes::rpartition, 1..1 pure;
        "removeprefix" => bytes::removeprefix, 1..1 pure; "removesuffix" => bytes::removesuffix, 1..1 pure;
        "center" => bytes::center, 1..2 pure; "ljust" => bytes::ljust, 1..2 pure; "rjust" => bytes::rjust, 1..2 pure; "zfill" => bytes::zfill, 1..1 pure;
        "expandtabs" => bytes::expandtabs, 0..1 pure; "capitalize" => bytes::capitalize, 0..0 pure; "title" => bytes::title, 0..0 pure;
        "swapcase" => bytes::swapcase, 0..0 pure; "isalnum" => bytes::isalnum, 0..0 pure; "isalpha" => bytes::isalpha, 0..0 pure;
        "isascii" => bytes::isascii, 0..0 pure; "isdigit" => bytes::isdigit, 0..0 pure; "islower" => bytes::islower, 0..0 pure;
        "isspace" => bytes::isspace, 0..0 pure; "istitle" => bytes::istitle, 0..0 pure; "isupper" => bytes::isupper, 0..0 pure;
        "maketrans" => bytes::maketrans, 2..2 pure; "translate" => bytes::translate, 1..2 pure;
        "resize" => bytearray::resize, 1..1; "take_bytes" => bytearray::take_bytes, 0..1;
    }
    "memoryview" {
        "tobytes" => framed, 0..1; "tolist" => framed, 0..0; "hex" => framed, 0..2; "release" => framed, 0..0;
    }
    "BaseExceptionGroup" {
        "split" => framed, 1..1; "subgroup" => framed, 1..1; "derive" => framed, 1..1;
    }
    // What every class ends in, reached through `super()`, `object.name(...)` or an instance without its own.
    "super" {
        "__get__" => framed, 1..2;
    }
    "object" {
        "__init_subclass__" => framed, 0..0; "__init__" => framed, 0..255; "__repr__" => framed, 0..0;
        "__str__" => framed, 0..0; "__eq__" => framed, 1..1; "__ne__" => framed, 1..1; "__hash__" => framed, 0..0;
        "__format__" => framed, 1..1; "__setattr__" => framed, 2..2; "__delattr__" => framed, 1..1;
        "__getattribute__" => framed, 1..1;
        "__new__" => framed, 0..255;
    }
    // Dunders of builtin values, which `operator::applies` allows by the type of the receiver.
    "op" {
        "__add__" => framed, 1..1; "__radd__" => framed, 1..1; "__sub__" => framed, 1..1; "__rsub__" => framed, 1..1;
        "__mul__" => framed, 1..1; "__rmul__" => framed, 1..1; "__truediv__" => framed, 1..1; "__rtruediv__" => framed, 1..1;
        "__floordiv__" => framed, 1..1; "__rfloordiv__" => framed, 1..1; "__mod__" => framed, 1..1; "__rmod__" => framed, 1..1;
        "__rpow__" => framed, 1..1; "__and__" => framed, 1..1; "__rand__" => framed, 1..1; "__or__" => framed, 1..1;
        "__ror__" => framed, 1..1; "__xor__" => framed, 1..1; "__rxor__" => framed, 1..1; "__lshift__" => framed, 1..1;
        "__rlshift__" => framed, 1..1; "__rshift__" => framed, 1..1; "__rrshift__" => framed, 1..1; "__divmod__" => framed, 1..1;
        "__rdivmod__" => framed, 1..1; "__eq__" => framed, 1..1; "__ne__" => framed, 1..1; "__lt__" => framed, 1..1;
        "__le__" => framed, 1..1; "__gt__" => framed, 1..1; "__ge__" => framed, 1..1; "__contains__" => framed, 1..1 => object::op_contains;
        "__getitem__" => framed, 1..1 => object::op_getitem; "__format__" => framed, 1..1; "__setitem__" => framed, 2..2; "__delitem__" => framed, 1..1;
        "__pow__" => framed, 1..2;
        "__round__" => framed, 0..1;
        "__neg__" => framed, 0..0; "__pos__" => framed, 0..0; "__abs__" => framed, 0..0; "__invert__" => framed, 0..0;
        "__bool__" => framed, 0..0 => object::op_bool; "__int__" => framed, 0..0; "__float__" => framed, 0..0; "__index__" => framed, 0..0;
        "__hash__" => framed, 0..0; "__repr__" => framed, 0..0 => object::op_repr; "__str__" => framed, 0..0 => object::op_str; "__len__" => framed, 0..0 => object::container_len;
        "__iter__" => framed, 0..0; "__trunc__" => framed, 0..0; "__floor__" => framed, 0..0; "__ceil__" => framed, 0..0;
        "conjugate" => framed, 0..0;
    }
};
pub static ALL_METHODS: &[MethodDesc] = METHODS;

/* A method's lookup key, the type and the name mixed into one word. */
const fn method_key(ty: &str, name: &str) -> u32 {
    let mut h: u64 = 0;
    let mut k = 0;
    while k < 2 {
        let bytes = if k == 0 { ty.as_bytes() } else { name.as_bytes() };
        let mut i = 0;
        while i < bytes.len() {
            h = (h.rotate_left(5) ^ bytes[i] as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
            i += 1;
        }
        h = (h.rotate_left(5) ^ 0xff).wrapping_mul(0x517c_c1b7_2722_0a95);
        k += 1;
    }
    (h >> 32) as u32
}

/* Method keys in order beside their ids, sorted at compile time. */
static BY_KEY: [(u32, u16); METHODS.len()] = {
    assert!(METHODS.len() <= u16::MAX as usize);
    let mut keys = [(0u32, 0u16); METHODS.len()];
    let mut i = 0;
    while i < keys.len() {
        let key = (method_key(METHODS[i].ty, METHODS[i].name), i as u16);
        let mut j = i;
        while j > 0 && keys[j - 1].0 > key.0 { keys[j] = keys[j - 1]; j -= 1; }
        keys[j] = key;
        i += 1;
    }
    let mut k = 1;
    while k < keys.len() {
        assert!(keys[k - 1].0 < keys[k].0, "two builtin methods share a lookup key");
        k += 1;
    }
    keys
};

// Methods that run user code, `exec_bound_method` calls them with the frame this table cannot carry.
fn framed(_: &mut VM, _: Val, _: &[Val]) -> Result<(), VmErr> { Err(cold_type("method dispatched without a frame")) }

/* What an omitted parameter of a builtin method stands for. */
#[derive(Clone, Copy)]
enum Omitted { None, Int(i64), False }

/* The parameters by name of the builtin methods that take keywords, each with what it is when omitted. */
fn keyword_params(ty: &str, name: &str) -> Option<&'static [(&'static str, Omitted)]> {
    use Omitted::*;
    Some(match (ty, name) {
        ("str" | "bytes" | "bytearray", "split" | "rsplit") => &[("sep", None), ("maxsplit", Int(-1))],
        ("str" | "bytes" | "bytearray", "splitlines") => &[("keepends", False)],
        ("str", "encode") | ("bytes" | "bytearray", "decode") => &[("encoding", None), ("errors", None)],
        ("str" | "bytes" | "bytearray", "expandtabs") => &[("tabsize", Int(8))],
        ("bytes" | "bytearray" | "memoryview", "hex") => &[("sep", None), ("bytes_per_sep", Int(1))],
        // A leading `/` marks a parameter given by position only.
        ("bytes" | "bytearray", "translate") => &[("/table", None), ("delete", None)],
        ("int", "to_bytes") => &[("length", Int(1)), ("byteorder", None), ("signed", False)],
        ("int", "from_bytes") => &[("bytes", None), ("byteorder", None), ("signed", False)],
        _ => return Option::None,
    })
}

/* The positionals a call with keywords amounts to, each keyword in the slot of its name and gaps as omitted. */
fn keywords_as_positionals(vm: &VM, m: &MethodDesc, params: &[(&str, Omitted)], pos: &[Val], kw: &[Val]) -> Result<alloc::vec::Vec<Val>, VmErr> {
    let mut args: alloc::vec::Vec<Option<Val>> = pos.iter().map(|&v| Some(v)).collect();
    for pair in kw.chunks(2) {
        let name = vm.kw_name(pair[0]).unwrap_or("");
        let Some(i) = params.iter().position(|(n, _)| *n == name && !n.starts_with('/')) else {
            let hint = crate::util::suggest::did_you_mean(name, params.iter().map(|(n, _)| *n));
            return Err(VmErr::TypeMsg(s!(str m.name, "() got an unexpected keyword argument '", str name, "'", str &hint)));
        };
        if args.get(i).is_some_and(Option::is_some) { return Err(VmErr::TypeMsg(s!("argument for ", str m.ty, ".", str m.name, "() given by name ('", str name, "') and position"))); }
        if args.len() <= i { args.resize(i + 1, Option::None); }
        args[i] = Some(pair[1]);
    }
    Ok(args.into_iter().zip(params).map(|(a, (_, o))| a.unwrap_or(match o { Omitted::None => Val::none(), Omitted::Int(n) => Val::int(*n), Omitted::False => Val::bool(false) })).collect())
}

#[inline]
pub(in crate::vm) fn dispatch_method(vm: &mut VM, id: BuiltinMethodId, recv: Val, pos: &[Val], kw: &[Val]) -> Result<(), VmErr> {
    let m = &ALL_METHODS[id.0 as usize];
    if !kw.is_empty() && let Some(params) = keyword_params(m.ty, m.name) {
        let args = keywords_as_positionals(vm, m, params, pos, kw)?;
        return dispatch_method(vm, id, recv, &args, &[]);
    }
    if !kw.is_empty() {
        // Only `dict.update(**kwargs)` and `dict.__init__` reach the dispatcher with keywords (`list.sort` keywords are intercepted before dispatch), pack them into a dict and append as a positional, which `dict::update` already merges.
        if m.ty == "dict" && matches!(m.name, "update" | "__init__") 
            && let Some(kwd) = VM::pack_kw_dict(&mut vm.heap, kw)? {
                let mut p = alloc::vec::Vec::with_capacity(pos.len() + 1);
                p.extend_from_slice(pos);
                p.push(kwd);
                return (m.func)(vm, recv, &p);
            }

        return Err(VmErr::TypeMsg(s!(str m.ty, ".", str m.name, "() takes no keyword arguments")));
    }
    let n = pos.len();
    if n < m.min_args as usize || (m.max_args != 255 && n > m.max_args as usize) {
        return Err(arity_error(m, n));
    }
    (m.func)(vm, recv, pos)
}

/* The arity check `dispatch_method` applies, for a method the VM runs itself. */
pub(crate) fn method_arity(id: BuiltinMethodId, n: usize) -> Result<(), VmErr> {
    let m = &ALL_METHODS[id.0 as usize];
    if n < m.min_args as usize || (m.max_args != 255 && n > m.max_args as usize) {
        return Err(arity_error(m, n));
    }
    Ok(())
}

/* Every method of builtin type `ty`, in table order. */
pub(crate) fn methods_of(ty: &str) -> impl Iterator<Item = BuiltinMethodId> + '_ {
    ALL_METHODS.iter().enumerate().filter(move |(_, m)| m.ty == ty).map(|(i, _)| BuiltinMethodId(i as u16))
}

pub fn lookup_method(ty: &str, attr: &str) -> Option<BuiltinMethodId> {
    let k = BY_KEY.binary_search_by_key(&method_key(ty, attr), |e| e.0).ok()?;
    let id = BY_KEY[k].1;
    let m = &ALL_METHODS[id as usize];
    (m.ty == ty && m.name == attr).then_some(BuiltinMethodId(id))
}

// Methods with parameters beyond the positional ones, whose count errors read `name() takes at most N arguments (M given)`.
const TAKES: &[&str] = &["replace", "hex", "sort"];

/* The error a call of `m` with `got` positionals raises, worded the way each kind of method words it. */
#[cold]
fn arity_error(m: &MethodDesc, got: usize) -> VmErr {
    let (min, max, n) = (m.min_args as i64, m.max_args as i64, got as i64);
    let args = |k: i64| if k == 1 { "argument" } else { "arguments" };
    let bound = if n < min { ("at least ", min) } else { ("at most ", max) };
    let msg: String = if m.ty == "op" {
        // A dunder of a builtin value counts its arguments the way a slot wrapper does.
        let (word, k) = if min == max { ("", min) } else { bound };
        let name = if min == max { "" } else { m.name };
        let gap = if name.is_empty() { "" } else { " " };
        s!(str name, str gap, "expected ", str word, int k, " ", str args(k), ", got ", int n)
    } else if m.name.starts_with("__") && m.name != "__init__" && min == max && min != 1 {
        s!("expected ", int min, " arguments, got ", int n)
    } else if min == max && min <= 1 {
        let takes = if min == 0 { "() takes no arguments (" } else { "() takes exactly one argument (" };
        s!(str m.ty, ".", str m.name, str takes, int n, " given)")
    } else if TAKES.contains(&m.name) || keyword_params(m.ty, m.name).is_some() {
        // Too few names the first missing parameter when it has a name, `replace` takes its first two by position only.
        match keyword_params(m.ty, m.name).and_then(|p| p.get(got)).filter(|(p, _)| n < min && !p.starts_with('/')) {
            Some((param, _)) => s!(str m.name, "() missing required argument '", str param, "' (pos ", int n + 1, ")"),
            None if n < min => s!(str m.name, "() takes at least ", int min, " positional ", str args(min), " (", int n, " given)"),
            None => s!(str m.name, "() takes at most ", int max, " ", str args(max), " (", int n, " given)"),
        }
    } else {
        let name = if m.name == "__init__" { m.ty } else { m.name };
        let (word, k) = if min == max { ("", min) } else { bound };
        s!(str name, " expected ", str word, int k, " ", str args(k), ", got ", int n)
    };
    VmErr::TypeMsg(msg)
}
