mod prelude;
mod recv;
pub mod bytes;
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BuiltinMethodId(u8);

impl BuiltinMethodId {
    #[inline] pub fn name(self) -> &'static str { ALL_METHODS[self.0 as usize].name }
    #[inline] pub(crate) fn kind(self) -> MethodKind { ALL_METHODS[self.0 as usize].kind }
    pub(crate) fn ty(self) -> &'static str { ALL_METHODS[self.0 as usize].ty }
    pub(crate) fn raw(self) -> u8 { self.0 }
    /* Bounds-checked decode for snapshot restore. */
    pub(crate) fn from_raw(i: u8) -> Option<Self> {
        ((i as usize) < ALL_METHODS.len()).then_some(Self(i))
    }
}

// Builds `ALL_METHODS` grouped by receiver type, `min..max` is the arity (max 255 = variadic).
macro_rules! methods {
    ( $( $ty:literal { $( $name:tt => $func:path, $min:literal .. $max:literal );* $(;)? } )* ) => {
        &[ $( $( MethodDesc {
            ty: $ty, name: $name, func: $func,
            min_args: $min, max_args: $max, kind: methods!(@k $name),
        } ),* ),* ]
    };
    (@k "sort") => { MethodKind::Sort }; (@k "format") => { MethodKind::Format }; (@k "format_map") => { MethodKind::Format };
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
        "encode" => string::encode, 0..1;
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
        "format" => framed, 0..255;
    }
    "bytes" {
        "decode" => bytes::decode, 0..2;
        "hex" => bytes::hex, 0..0;
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
        "to_bytes" => numeric::to_bytes, 0..2;
        "from_bytes" => numeric::from_bytes, 1..2;
    }
    "float" {
        "is_integer" => numeric::is_integer, 0..0;
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
        "__getitem__" => framed, 1..1; "__format__" => framed, 1..1;
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
static BY_KEY: [(u32, u8); METHODS.len()] = {
    assert!(METHODS.len() <= 256);
    let mut keys = [(0u32, 0u8); METHODS.len()];
    let mut i = 0;
    while i < keys.len() {
        let key = (method_key(METHODS[i].ty, METHODS[i].name), i as u8);
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

#[inline]
pub(in crate::vm) fn dispatch_method(vm: &mut VM, id: BuiltinMethodId, recv: Val, pos: &[Val], kw: &[Val]) -> Result<(), VmErr> {
    let m = &ALL_METHODS[id.0 as usize];
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

pub fn lookup_method(ty: &str, attr: &str) -> Option<BuiltinMethodId> {
    let k = BY_KEY.binary_search_by_key(&method_key(ty, attr), |e| e.0).ok()?;
    let id = BY_KEY[k].1;
    let m = &ALL_METHODS[id as usize];
    (m.ty == ty && m.name == attr).then_some(BuiltinMethodId(id))
}

// Search and split methods of text, whose count errors read `name() takes at most N arguments (M given)`.
const PARSED: &[&str] = &["startswith", "endswith", "find", "rfind", "index", "rindex", "count", "split", "rsplit", "expandtabs", "decode"];

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
    } else if matches!(m.ty, "str" | "bytes") && PARSED.contains(&m.name) {
        s!(str m.name, "() takes ", str bound.0, int bound.1, " ", str args(bound.1), " (", int n, " given)")
    } else {
        let name = if m.name == "__init__" { m.ty } else { m.name };
        let (word, k) = if min == max { ("", min) } else { bound };
        s!(str name, " expected ", str word, int k, " ", str args(k), ", got ", int n)
    };
    VmErr::TypeMsg(msg)
}
