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

use prelude::{VM, Val, VmErr, cold_type};
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
    ( $( $ty:literal { $( $name:tt => $func:path, $min:literal .. $max:literal );* $(;)? } )* ) => {
        &[ $( $( MethodDesc {
            ty: $ty, name: $name, func: $func,
            min_args: $min, max_args: $max, kind: methods!(@k $name), post: post_of($ty, $name),
        } ),* ),* ]
    };
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
        "encode" => string::encode, 0..2;
        "upper" => string::upper, 0..0;
        "lower" => string::lower, 0..0;
        "strip" => string::strip, 0..1;
        "capitalize" => string::capitalize, 0..0;
        "title" => string::title, 0..0;
        "lstrip" => string::lstrip, 0..1;
        "rstrip" => string::rstrip, 0..1;
        "isdigit" => string::isdigit, 0..0;
        "isalpha" => string::isalpha, 0..0;
        "isalnum" => string::isalnum, 0..0;
        "startswith" => string::startswith, 1..3;
        "endswith" => string::endswith, 1..3;
        "find" => string::find, 1..3;
        "count" => string::count, 1..3;
        "split" => string::split, 0..2;
        "join" => string::join, 1..1;
        "replace" => string::replace, 2..3;
        "removeprefix" => string::removeprefix, 1..1;
        "removesuffix" => string::removesuffix, 1..1;
        "splitlines" => string::splitlines, 0..1;
        "partition" => string::partition, 1..1;
        "rpartition" => string::rpartition, 1..1;
        "center" => string::center, 1..2;
        "zfill" => string::zfill, 1..1;
        "rsplit" => string::rsplit, 0..2;
        "casefold" => string::casefold, 0..0;
        "swapcase" => string::swapcase, 0..0;
        "ljust" => string::ljust, 1..2;
        "rjust" => string::rjust, 1..2;
        "expandtabs" => string::expandtabs, 0..1;
        "rfind" => string::rfind, 1..3;
        "index" => string::index, 1..3;
        "rindex" => string::rindex, 1..3;
        "isspace" => string::isspace, 0..0;
        "isupper" => string::isupper, 0..0;
        "islower" => string::islower, 0..0;
        "istitle" => string::istitle, 0..0;
        "isascii" => string::isascii, 0..0;
        "isdecimal" => string::isdecimal, 0..0;
        "isnumeric" => string::isnumeric, 0..0;
        "isprintable" => string::isprintable, 0..0;
        "isidentifier" => string::isidentifier, 0..0;
        "maketrans" => string::maketrans, 1..3;
        "translate" => string::translate, 1..1;
        "format" => framed, 0..255;
    }
    "bytes" {
        "decode" => bytes::decode, 0..2;
        "hex" => bytes::hex, 0..2;
        "startswith" => bytes::startswith, 1..3;
        "endswith" => bytes::endswith, 1..3;
        "find" => bytes::find, 1..3;
        "index" => bytes::index, 1..3;
        "count" => bytes::count, 1..3;
        "replace" => bytes::replace, 2..3;
        "split" => bytes::split, 0..2;
        "lower" => bytes::lower, 0..0;
        "upper" => bytes::upper, 0..0;
        "strip" => bytes::strip, 0..1;
        "lstrip" => bytes::lstrip, 0..1;
        "rstrip" => bytes::rstrip, 0..1;
        "join" => bytes::join, 1..1;
        "fromhex" => bytes::fromhex, 1..1;
        "rfind" => bytes::rfind, 1..3; "rindex" => bytes::rindex, 1..3; "rsplit" => bytes::rsplit, 0..2;
        "splitlines" => bytes::splitlines, 0..1; "partition" => bytes::partition, 1..1; "rpartition" => bytes::rpartition, 1..1;
        "removeprefix" => bytes::removeprefix, 1..1; "removesuffix" => bytes::removesuffix, 1..1;
        "center" => bytes::center, 1..2; "ljust" => bytes::ljust, 1..2; "rjust" => bytes::rjust, 1..2; "zfill" => bytes::zfill, 1..1;
        "expandtabs" => bytes::expandtabs, 0..1; "capitalize" => bytes::capitalize, 0..0; "title" => bytes::title, 0..0;
        "swapcase" => bytes::swapcase, 0..0; "isalnum" => bytes::isalnum, 0..0; "isalpha" => bytes::isalpha, 0..0;
        "isascii" => bytes::isascii, 0..0; "isdigit" => bytes::isdigit, 0..0; "islower" => bytes::islower, 0..0;
        "isspace" => bytes::isspace, 0..0; "istitle" => bytes::istitle, 0..0; "isupper" => bytes::isupper, 0..0;
        "maketrans" => bytes::maketrans, 2..2; "translate" => bytes::translate, 1..2;
    }
    "list" {
        "__init__" => list::init, 0..1;
        "__getitem__" => object::container_getitem, 1..1;
        "__setitem__" => object::container_setitem, 2..2;
        "__len__" => object::container_len, 0..0;
        "index" => list::index, 1..3;
        "count" => list::count, 1..1;
        "copy" => list::copy, 0..0;
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
        "__getitem__" => object::container_getitem, 1..1;
        "__setitem__" => object::container_setitem, 2..2;
        "__len__" => object::container_len, 0..0;
        "keys" => dict::keys, 0..0;
        "values" => dict::values, 0..0;
        "items" => dict::items, 0..0;
        "copy" => dict::copy, 0..0;
        "popitem" => dict::popitem, 0..0;
        "get" => dict::get, 1..2;
        "update" => dict::update, 0..1;
        "pop" => dict::pop, 1..2;
        "setdefault" => dict::setdefault, 1..2;
        "fromkeys" => dict::fromkeys, 1..2;
    }
    // A frozendict reads like a dict, `copy` gives itself back and `fromkeys` makes a frozendict.
    "frozendict" {
        "keys" => dict::keys, 0..0; "values" => dict::values, 0..0; "items" => dict::items, 0..0;
        "get" => dict::get, 1..2; "copy" => dict::frozen_copy, 0..0; "fromkeys" => dict::fromkeys, 1..2;
        "__getitem__" => object::container_getitem, 1..1; "__len__" => object::container_len, 0..0;
    }
    // A frozenset answers the set methods that change nothing, with frozenset results.
    "frozenset" {
        "union" => set::union, 0..255; "intersection" => set::intersection, 0..255; "difference" => set::difference, 0..255;
        "symmetric_difference" => set::symmetric_difference, 1..1; "issubset" => set::issubset, 1..1;
        "issuperset" => set::issuperset, 1..1; "isdisjoint" => set::isdisjoint, 1..1; "copy" => set::frozen_copy, 0..0;
    }
    "set" {
        "add" => set::add, 1..1;
        "remove" => set::remove, 1..1;
        "discard" => set::discard, 1..1;
        "pop" => set::pop, 0..0;
        "clear" => set::clear, 0..0;
        "update" => set::update, 0..255;
        "copy" => set::copy, 0..0;
        "union" => set::union, 0..255;
        "intersection" => set::intersection, 0..255;
        "difference" => set::difference, 0..255;
        "symmetric_difference" => set::symmetric_difference, 1..1;
        "intersection_update" => set::intersection_update, 0..255;
        "difference_update" => set::difference_update, 0..255;
        "symmetric_difference_update" => set::symmetric_difference_update, 1..1;
        "issubset" => set::issubset, 1..1;
        "issuperset" => set::issuperset, 1..1;
        "isdisjoint" => set::isdisjoint, 1..1;
    }
    "int" {
        "bit_length" => numeric::bit_length, 0..0;
        "bit_count" => numeric::bit_count, 0..0;
        "to_bytes" => numeric::to_bytes, 0..3;
        "from_bytes" => numeric::from_bytes, 1..3;
        "as_integer_ratio" => numeric::int_ratio, 0..0;
        "is_integer" => numeric::int_is_integer, 0..0;
    }
    "float" {
        "is_integer" => numeric::is_integer, 0..0;
        "as_integer_ratio" => numeric::float_ratio, 0..0;
        "hex" => numeric::float_hex, 0..0;
        "fromhex" => numeric::float_fromhex, 1..1;
        "from_number" => numeric::float_from_number, 1..1;
    }
    // Appended groups, keep snapshot method ids stable.
    "dict" {
        "clear" => dict::clear, 0..0;
    }
    "slice" {
        "indices" => slice::indices, 1..1;
    }
    "range" {
        "index" => object::range_index, 1..1;
        "count" => object::range_count, 1..1;
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
        "isdisjoint" => dict::view_isdisjoint, 1..1;
    }
    "dict_items" {
        "isdisjoint" => dict::view_isdisjoint, 1..1;
    }
    "tuple" {
        "index" => list::index, 1..3;
        "count" => list::count, 1..1;
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
    // A bytearray answers the bytes methods, its results bytearrays again, and the methods that change it.
    "bytearray" {
        "decode" => bytes::decode, 0..2; "hex" => bytes::hex, 0..2; "startswith" => bytes::startswith, 1..3;
        "endswith" => bytes::endswith, 1..3; "find" => bytes::find, 1..3; "index" => bytes::index, 1..3;
        "count" => bytes::count, 1..3; "replace" => bytes::replace, 2..3; "split" => bytes::split, 0..2;
        "lower" => bytes::lower, 0..0; "upper" => bytes::upper, 0..0; "strip" => bytes::strip, 0..1;
        "lstrip" => bytes::lstrip, 0..1; "rstrip" => bytes::rstrip, 0..1; "join" => bytes::join, 1..1;
        "fromhex" => bytes::fromhex, 1..1; "append" => bytearray::append, 1..1; "extend" => bytearray::extend, 1..1;
        "pop" => bytearray::pop, 0..1; "insert" => bytearray::insert, 2..2; "remove" => bytearray::remove, 1..1;
        "clear" => bytearray::clear, 0..0; "reverse" => bytearray::reverse, 0..0; "copy" => bytearray::copy, 0..0;
        "rfind" => bytes::rfind, 1..3; "rindex" => bytes::rindex, 1..3; "rsplit" => bytes::rsplit, 0..2;
        "splitlines" => bytes::splitlines, 0..1; "partition" => bytes::partition, 1..1; "rpartition" => bytes::rpartition, 1..1;
        "removeprefix" => bytes::removeprefix, 1..1; "removesuffix" => bytes::removesuffix, 1..1;
        "center" => bytes::center, 1..2; "ljust" => bytes::ljust, 1..2; "rjust" => bytes::rjust, 1..2; "zfill" => bytes::zfill, 1..1;
        "expandtabs" => bytes::expandtabs, 0..1; "capitalize" => bytes::capitalize, 0..0; "title" => bytes::title, 0..0;
        "swapcase" => bytes::swapcase, 0..0; "isalnum" => bytes::isalnum, 0..0; "isalpha" => bytes::isalpha, 0..0;
        "isascii" => bytes::isascii, 0..0; "isdigit" => bytes::isdigit, 0..0; "islower" => bytes::islower, 0..0;
        "isspace" => bytes::isspace, 0..0; "istitle" => bytes::istitle, 0..0; "isupper" => bytes::isupper, 0..0;
        "maketrans" => bytes::maketrans, 2..2; "translate" => bytes::translate, 1..2;
        "resize" => bytearray::resize, 1..1; "take_bytes" => bytearray::take_bytes, 0..1;
    }
    "memoryview" {
        "tobytes" => framed, 0..1; "tolist" => framed, 0..0; "hex" => framed, 0..2; "release" => framed, 0..0;
    }
    "BaseExceptionGroup" {
        "split" => framed, 1..1; "subgroup" => framed, 1..1; "derive" => framed, 1..1;
    }
    // What every class ends in, reached through `super()`, `object.name(...)` or an instance without its own.
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
        "__le__" => framed, 1..1; "__gt__" => framed, 1..1; "__ge__" => framed, 1..1; "__contains__" => framed, 1..1;
        "__getitem__" => framed, 1..1; "__format__" => framed, 1..1; "__setitem__" => framed, 2..2; "__delitem__" => framed, 1..1;
        "__pow__" => framed, 1..2;
        "__round__" => framed, 0..1;
        "__neg__" => framed, 0..0; "__pos__" => framed, 0..0; "__abs__" => framed, 0..0; "__invert__" => framed, 0..0;
        "__bool__" => framed, 0..0; "__int__" => framed, 0..0; "__float__" => framed, 0..0; "__index__" => framed, 0..0;
        "__hash__" => framed, 0..0; "__repr__" => framed, 0..0; "__str__" => framed, 0..0; "__len__" => framed, 0..0;
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
