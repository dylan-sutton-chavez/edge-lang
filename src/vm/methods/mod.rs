mod prelude;
mod recv;
pub mod bytes;
pub mod dict;
pub mod list;
pub mod numeric;
pub mod object;
pub mod set;
pub mod slice;
pub mod string;

use prelude::{VM, Val, VmErr, cold_type};
use crate::s;
use alloc::string::String;

pub type MethodFn = fn(&mut VM, Val, &[Val]) -> Result<(), VmErr>;

/* Builtin-method descriptor table + dispatcher. Bodies live in per-type files (string/bytes/list/dict/set) as `pub fn`, arity is checked uniformly by the dispatcher and `mutating` calls are marked impure there, so bodies do neither. A descriptor is name + fn ptr + mutating flag + arity range. */
pub struct MethodDesc {
    pub ty: &'static str, // receiver type ("str"/"bytes"/"list"/"dict"/"set"), drives lookup.
    pub name: &'static str,
    pub func: MethodFn,
    pub mutating: bool,
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

// Builds `ALL_METHODS` grouped by receiver type. `ro`/`rw` = read-only/mutating, `min..max` is the arity (max 255 = variadic).
macro_rules! methods {
    ( $( $ty:literal { $( $name:tt => $func:path, $m:ident, $min:literal .. $max:literal );* $(;)? } )* ) => {
        &[ $( $( MethodDesc {
            ty: $ty, name: $name, func: $func,
            mutating: methods!(@m $m), min_args: $min, max_args: $max, kind: methods!(@k $name),
        } ),* ),* ]
    };
    (@m ro) => { false };
    (@m rw) => { true };
    (@k "sort") => { MethodKind::Sort }; (@k "format") => { MethodKind::Format };
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
        "encode" => string::encode, ro, 0..1;
        "upper" => string::upper, ro, 0..0;
        "lower" => string::lower, ro, 0..0;
        "strip" => string::strip, ro, 0..1;
        "capitalize" => string::capitalize, ro, 0..0;
        "title" => string::title, ro, 0..0;
        "lstrip" => string::lstrip, ro, 0..1;
        "rstrip" => string::rstrip, ro, 0..1;
        "isdigit" => string::isdigit, ro, 0..0;
        "isalpha" => string::isalpha, ro, 0..0;
        "isalnum" => string::isalnum, ro, 0..0;
        "startswith" => string::startswith, ro, 1..3;
        "endswith" => string::endswith, ro, 1..3;
        "find" => string::find, ro, 1..3;
        "count" => string::count, ro, 1..3;
        "split" => string::split, ro, 0..2;
        "join" => string::join, ro, 1..1;
        "replace" => string::replace, ro, 2..3;
        "removeprefix" => string::removeprefix, ro, 1..1;
        "removesuffix" => string::removesuffix, ro, 1..1;
        "splitlines" => string::splitlines, ro, 0..0;
        "partition" => string::partition, ro, 1..1;
        "rpartition" => string::rpartition, ro, 1..1;
        "center" => string::center, ro, 1..2;
        "zfill" => string::zfill, ro, 1..1;
        "rsplit" => string::rsplit, ro, 0..2;
        "casefold" => string::casefold, ro, 0..0;
        "swapcase" => string::swapcase, ro, 0..0;
        "ljust" => string::ljust, ro, 1..2;
        "rjust" => string::rjust, ro, 1..2;
        "expandtabs" => string::expandtabs, ro, 0..1;
        "rfind" => string::rfind, ro, 1..3;
        "index" => string::index, ro, 1..3;
        "rindex" => string::rindex, ro, 1..3;
        "isspace" => string::isspace, ro, 0..0;
        "isupper" => string::isupper, ro, 0..0;
        "islower" => string::islower, ro, 0..0;
        "istitle" => string::istitle, ro, 0..0;
        "format" => framed, ro, 0..255;
    }
    "bytes" {
        "decode" => bytes::decode, ro, 0..2;
        "hex" => bytes::hex, ro, 0..0;
        "startswith" => bytes::startswith, ro, 1..1;
        "endswith" => bytes::endswith, ro, 1..1;
        "find" => bytes::find, ro, 1..3;
        "index" => bytes::index, ro, 1..3;
        "count" => bytes::count, ro, 1..3;
        "replace" => bytes::replace, ro, 2..3;
        "split" => bytes::split, ro, 0..2;
        "lower" => bytes::lower, ro, 0..0;
        "upper" => bytes::upper, ro, 0..0;
        "strip" => bytes::strip, ro, 0..1;
        "lstrip" => bytes::lstrip, ro, 0..1;
        "rstrip" => bytes::rstrip, ro, 0..1;
        "join" => bytes::join, ro, 1..1;
        "fromhex" => bytes::fromhex, ro, 1..1;
    }
    "list" {
        "__init__" => list::init, rw, 0..1;
        "__getitem__" => object::container_getitem, ro, 1..1;
        "__setitem__" => object::container_setitem, rw, 2..2;
        "__len__" => object::container_len, ro, 0..0;
        "index" => list::index, ro, 1..3;
        "count" => list::count, ro, 1..1;
        "copy" => list::copy, ro, 0..0;
        "append" => list::append, rw, 1..1;
        "clear" => list::clear, rw, 0..0;
        "reverse" => list::reverse, rw, 0..0;
        "extend" => list::extend, rw, 1..1;
        "insert" => list::insert, rw, 2..2;
        "remove" => list::remove, rw, 1..1;
        "pop" => list::pop, rw, 0..1;
        "sort" => framed, rw, 0..0;
    }
    "dict" {
        "__init__" => dict::update, rw, 0..1;
        "__getitem__" => object::container_getitem, ro, 1..1;
        "__setitem__" => object::container_setitem, rw, 2..2;
        "__len__" => object::container_len, ro, 0..0;
        "keys" => dict::keys, ro, 0..0;
        "values" => dict::values, ro, 0..0;
        "items" => dict::items, ro, 0..0;
        "copy" => dict::copy, ro, 0..0;
        "popitem" => dict::popitem, rw, 0..0;
        "get" => dict::get, ro, 1..2;
        "update" => dict::update, rw, 1..1;
        "pop" => dict::pop, rw, 1..2;
        "setdefault" => dict::setdefault, rw, 1..2;
        "fromkeys" => dict::fromkeys, ro, 1..2;
    }
    "set" {
        "add" => set::add, rw, 1..1;
        "remove" => set::remove, rw, 1..1;
        "discard" => set::discard, rw, 1..1;
        "pop" => set::pop, rw, 0..0;
        "clear" => set::clear, rw, 0..0;
        "update" => set::update, rw, 0..255;
        "copy" => set::copy, ro, 0..0;
        "union" => set::union, ro, 0..255;
        "intersection" => set::intersection, ro, 0..255;
        "difference" => set::difference, ro, 0..255;
        "symmetric_difference" => set::symmetric_difference, ro, 1..1;
        "intersection_update" => set::intersection_update, rw, 0..255;
        "difference_update" => set::difference_update, rw, 0..255;
        "symmetric_difference_update" => set::symmetric_difference_update, rw, 1..1;
        "issubset" => set::issubset, ro, 1..1;
        "issuperset" => set::issuperset, ro, 1..1;
        "isdisjoint" => set::isdisjoint, ro, 1..1;
    }
    "int" {
        "bit_length" => numeric::bit_length, ro, 0..0;
        "bit_count" => numeric::bit_count, ro, 0..0;
        "to_bytes" => numeric::to_bytes, ro, 0..2;
        "from_bytes" => numeric::from_bytes, ro, 1..2;
    }
    "float" {
        "is_integer" => numeric::is_integer, ro, 0..0;
    }
    // Appended groups, keep snapshot method ids stable.
    "dict" {
        "clear" => dict::clear, rw, 0..0;
    }
    "slice" {
        "indices" => slice::indices, ro, 1..1;
    }
    "BaseException" {
        "__init__" => object::exc_init, rw, 0..255;
        "__str__" => object::exc_str, ro, 0..0;
    }
    "iterator" {
        "__next__" => object::iter_next, rw, 0..0;
        "__iter__" => object::iter_self, ro, 0..0;
    }
    "dict_keys" {
        "isdisjoint" => dict::view_isdisjoint, ro, 1..1;
    }
    "dict_items" {
        "isdisjoint" => dict::view_isdisjoint, ro, 1..1;
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
                let result = (m.func)(vm, recv, &p);
                if result.is_ok() { vm.mark_impure(); }
                return result;
            }

        return Err(cold_type("builtin method takes no keyword arguments"));
    }
    let n = pos.len();
    if n < m.min_args as usize || (m.max_args != 255 && n > m.max_args as usize) {
        return Err(arity_error(m.name, m.min_args, m.max_args, n));
    }
    let result = (m.func)(vm, recv, pos);
    if m.mutating && result.is_ok() {
        vm.mark_impure();
    }
    result
}

/* The arity check and impurity mark `dispatch_method` applies, for a method the VM runs itself. */
pub(crate) fn method_frame(vm: &mut VM, id: BuiltinMethodId, n: usize) -> Result<(), VmErr> {
    let m = &ALL_METHODS[id.0 as usize];
    if n < m.min_args as usize || (m.max_args != 255 && n > m.max_args as usize) {
        return Err(arity_error(m.name, m.min_args, m.max_args, n));
    }
    if m.mutating { vm.mark_impure(); }
    Ok(())
}

pub fn lookup_method(ty: &str, attr: &str) -> Option<BuiltinMethodId> {
    let k = BY_KEY.binary_search_by_key(&method_key(ty, attr), |e| e.0).ok()?;
    let id = BY_KEY[k].1;
    let m = &ALL_METHODS[id as usize];
    (m.ty == ty && m.name == attr).then_some(BuiltinMethodId(id))
}

#[cold]
fn arity_error(name: &str, min: u8, max: u8, got: usize) -> VmErr {
    let msg: String = if min == max {
        s!(str name, "() takes ", int min as i64, " arg(s), got ", int got as i64)
    } else if max == 255 {
        s!(str name, "() takes at least ", int min as i64, ", got ", int got as i64)
    } else {
        s!(str name, "() takes ", int min as i64, "..", int max as i64, " args, got ", int got as i64)
    };
    VmErr::TypeMsg(msg)
}
