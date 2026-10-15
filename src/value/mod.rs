use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};
use core::cell::{Cell, RefCell};
use crate::util::hash::FxHashMap as HashMap;

pub mod frames;
pub mod eq;
pub mod err;
pub mod math;
pub mod scheduler;
pub mod meter;
pub mod pool;

pub use frames::*;
pub use eq::*;
pub use err::*;
pub use math::*;
pub use scheduler::*;

/* Per-execution caps for the op budget and the bytes a program holds, every execution is metered. */
#[derive(Clone, Copy)]
pub struct Limits { pub ops: u64, pub memory: usize }

impl Limits {
    pub fn sandbox() -> Self { Self { ops: 100_000_000, memory: 256 << 20 } }
}

// How many coroutines run at once, and how many frames a traceback keeps.
pub const MAX_CALLS: usize = 192;

// How deep loops nest in Rust, so the deepest builtin callback fits the wasm stack.
pub const MAX_NESTED: usize = 128;

// What a value takes in a container, 8 bytes by NaN-boxing on every architecture.
pub const VAL_BYTES: usize = 8;
/* Native work a step lets pile up before the loop leaves its fast path to charge it. */
const WORK_SLICE: usize = 1 << 16;

/* Host-provided callable, resolved at compile time and dispatched by `CallExtern`. `Arc<dyn Fn>` lets loaders capture stateful handles, `pure` enables memoization. Third arg is the kwargs slot, `None` for plain positional calls, `Some(dict_val)` when the caller used `name=value` syntax. */
pub type ExternCallable =
    alloc::sync::Arc<dyn Fn(&mut HeapPool, &[Val], Option<Val>) -> Result<Val, VmErr> + Send + Sync>;

#[derive(Clone)]
pub struct ExternFn {
    pub name: String,
    pub func: ExternCallable,
    pub pure: bool,
}

impl core::fmt::Debug for ExternFn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExternFn").field("name", &self.name).field("pure", &self.pure).finish()
    }
}

/* NaN-boxed 8-byte value (48-bit int, float, bool, None, undef, heap idx), layout in `abi::nan_box`. */
use crate::abi::nan_box::{
    QNAN, SIGN, TAG_UNDEF, TAG_NONE, TAG_TRUE, TAG_FALSE, TAG_INT, TAG_HEAP,
    INT_PAYLOAD_MASK,
};

#[derive(Clone, Copy, Debug)]
pub struct Val(pub(crate) u64);

impl PartialEq for Val {
    #[inline] fn eq(&self, o: &Self) -> bool {
        if self.0 == o.0 { return true; }
        // Mirror Hash, numeric immediates unify by value so True==1==1.0 share a dict/set key.
        let num = |v: &Val| -> Option<f64> {
            if v.is_int() { Some(v.as_int() as f64) }
            else if v.is_bool() { Some(v.as_bool() as i64 as f64) }
            else if v.is_float() { Some(v.as_float()) }
            else { None }
        };
        match (num(self), num(o)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }
}
impl Eq for Val {}

impl Val {
    /* The one quiet NaN every NaN becomes, its tag bit clear so `is_float()` holds. */
    const NAN_BASE: u64 = 0x7FF8_0000_0000_0000;
    const EXP_MASK: u64 = 0x7FF0_0000_0000_0000;
    #[inline(always)] pub fn float(f: f64) -> Self {
        let bits = f.to_bits();
        if (bits & Self::EXP_MASK) == Self::EXP_MASK { Self::special_float(bits) } else { Self(bits) }
    }
    /* An infinity keeps its bits, a NaN keeps only its sign. */
    #[cold]
    fn special_float(bits: u64) -> Self {
        if bits & 0x000F_FFFF_FFFF_FFFF == 0 { return Self(bits); }
        Self((bits & SIGN) | Self::NAN_BASE)
    }
    pub const INT_MAX: i64 = 0x0000_7FFF_FFFF_FFFF;
    pub const INT_MIN: i64 = -0x0000_8000_0000_0000;
    #[inline(always)] pub fn int(i: i64) -> Self {
        Self(TAG_INT | (i as u64 & INT_PAYLOAD_MASK))
    }
    #[inline(always)] pub fn int_checked(i: i64) -> Option<Self> {
        if !(Self::INT_MIN..=Self::INT_MAX).contains(&i) { None } else { Some(Self::int(i)) }
    }
    #[inline(always)] pub fn none() -> Self { Self(TAG_NONE) }
    #[inline(always)] pub fn bool(b: bool) -> Self { Self(if b { TAG_TRUE } else { TAG_FALSE }) }
    #[inline(always)] pub fn heap(idx: u32) -> Self { Self(TAG_HEAP | ((idx as u64) << 4)) }
    /* Unbound-local sentinel, lets slots stay Vec<Val> and LoadName check via one u64 compare. */
    #[inline(always)] pub fn undef() -> Self { Self(TAG_UNDEF) }

    #[inline(always)] pub fn is_float(&self) -> bool { (self.0 & QNAN) != QNAN }
    #[inline(always)] pub fn is_int(&self) -> bool { (self.0 & (QNAN | SIGN)) == TAG_INT }
    #[inline(always)] pub fn is_none(&self) -> bool { self.0 == TAG_NONE }
    #[inline(always)] pub fn is_true(&self) -> bool { self.0 == TAG_TRUE }
    #[inline(always)] pub fn is_false(&self) -> bool { self.0 == TAG_FALSE }
    #[inline(always)] pub fn is_bool(&self) -> bool { self.0 == TAG_TRUE || self.0 == TAG_FALSE }
    #[inline(always)] pub fn is_undef(&self) -> bool { self.0 == TAG_UNDEF }
    #[inline(always)] pub fn is_heap(&self) -> bool {
        (self.0 & QNAN) == QNAN && (self.0 & SIGN) == 0 && (self.0 & 0xF) >= 4
    }

    #[inline(always)] pub fn as_float(&self) -> f64 { f64::from_bits(self.0) }
    /* Wire-format accessors (FFI / WASM loader / SDK). */
    #[inline(always)] pub fn raw(&self) -> u64 { self.0 }
    /// # Safety
    ///
    /// `u` must come from `Val::raw()` on a live heap slot in the same VM.
    #[inline(always)] pub unsafe fn from_raw(u: u64) -> Self { Self(u) }
    #[inline(always)] pub fn as_int(&self) -> i64 {
        let raw = (self.0 & INT_PAYLOAD_MASK) as i64;
        (raw << 16) >> 16
    }
    #[inline(always)] pub fn as_bool(&self) -> bool { self.0 == TAG_TRUE }
    #[inline(always)] pub fn as_heap(&self) -> u32 { ((self.0 >> 4) & 0x0FFF_FFFF) as u32 }
}


/* Heap-allocated value variants in HeapPool's arena, indexed via Val::heap. */
#[derive(Clone, Debug)]
pub enum HeapObj {
    Str(String),
    Bytes(Vec<u8>),
    List(Rc<RefCell<Vec<Val>>>),
    Dict(Rc<RefCell<DictMap>>),
    Set(Rc<RefCell<ValSet>>),
    /* Immutable, hashable counterpart of Set, built via `frozenset(iter)`. */
    FrozenSet(Rc<ValSet>),
    Tuple(Vec<Val>),
    // (fi, defaults, captures, attrs), attrs share the Class member shape.
    Func(usize, Vec<Val>, Vec<(usize, Val)>, Rc<RefCell<Vec<(String, Val)>>>),
    Range(i64, i64, i64),
    Slice(Val, Val, Val),
    // True `...` singleton, distinct from any string.
    Ellipsis,
    Type(String),
    // `NotImplemented` singleton, dunder return sentinel that triggers the reflected operator fallback.
    NotImplemented,
    /* Wide-int slow path (i128), `int_to_val` canonicalises so 48-bit values stay inline. */
    LongInt(Wide),
    /* Exception instance, type name + ctor args (exposed via `.args`) + its chain, undef until it has one. */
    ExcInstance(String, Vec<Val>, Val),
    BoundMethod(Val, BuiltinMethodId),
    NativeFn(NativeFnId),
    // `bases` lists direct parents in declared order, the VM walks a cached C3 linearization on miss. Members are mutable so a decorator or `cls.attr = ...` can add or replace class attributes.
    Class(String, Vec<Val>, Rc<RefCell<Vec<(String, Val)>>>),
    Instance(Val, Rc<RefCell<DictMap>>),
    // `(recv, func, class)`, `class` is where `func` was found so the called frame knows what `super()` should skip past.
    BoundUserMethod(Val, Val, Val),
    // `super()` proxy, attribute access walks the bases of `cls` (skipping `cls` itself), methods bind to `recv`.
    Super(Val, Val),
    // `(getter, setter)`, `setter == none()` for getter-only properties, written via `@property` / `@x.setter`.
    Property(Val, Val),
    // Intermediate produced by `prop.setter`, a callable that takes a function and returns a new `Property` with the setter attached.
    PropertySetter(Val),
    // `staticmethod(func)` wraps a function so attribute lookup returns it unbound.
    StaticMethod(Val),
    // `classmethod(func)`, attribute lookup binds the class.
    ClassMethod(Val),
    // Trailing `Vec<SyncFrame>` stacks suspended sync sub-calls (innermost-last). Resume walks inside-out, each return lands on next frame's Call site. `BodyRef` discriminates user-fn coros from the implicit module-body coro. Final `Vec<ExceptionFrame>` carries try/except across yields.
    Coroutine(Box<Coro>),
    /* Produced by `import m`, attr access via LoadAttr, calls fuse through CallMethod. */
    Module(String, Vec<(String, Val)>),
    /* A native binding lifted to a first-class callable. */
    Extern(ExternFn),
    // `list[int]`, the origin type or alias and its args tuple.
    GenericAlias(Val, Val),
    // `type X = v`, the name and a zero-argument function that evaluates `v` on `X.__value__`.
    TypeAlias(String, Val),
    // `int | str`, the member types as one tuple.
    Union(Val),
    // `T` of a `class Box[T]` type parameter list.
    TypeVar(String),
    // A builtin iterator such as `iter(xs)` or `map(f, xs)`, its frame shared by every name for it.
    Iter(Rc<RefCell<IterFrame>>, &'static str),
    // A variable a closure shares with the frame binding it, undef while unbound.
    Cell(Val),
    // `d.keys()`, `d.values()` or `d.items()`, reading the dict live.
    DictView(Val, View),
    // What `compile()` made, compiled again where `eval` or `exec` runs it.
    Code(Rc<CodeObj>),
}

/* How compiled source runs, an expression for `eval`, statements for `exec`, one statement echoing an expression for `single`. */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeMode { Exec, Eval, Single }

/* The source of a code object, the file name its errors cite and how it runs. */
#[derive(Clone, Debug)]
pub struct CodeObj {
    pub source: String,
    pub filename: String,
    pub mode: CodeMode,
}

/* The part of a dict a view shows. */
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum View { Keys, Values, Items }

/* A generator or coroutine body, where it resumes and the state it resumes with. */
#[derive(Clone, Debug)]
pub struct Coro {
    pub ip: usize,
    pub slots: Vec<Val>,
    pub stack: Vec<Val>,
    pub body: BodyRef,
    pub iters: Vec<IterFrame>,
    pub syncs: Vec<SyncFrame>,
    pub excs: Vec<ExceptionFrame>,
}

impl Coro {
    /* A body not yet started, its frame bound. */
    pub fn fresh(slots: Vec<Val>, body: BodyRef) -> Box<Self> {
        Box::new(Self { ip: 0, slots, stack: Vec::new(), body, iters: Vec::new(), syncs: Vec::new(), excs: Vec::new() })
    }
}

/* An i128 at eight-byte alignment, so it does not widen every heap object. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(Rust, packed(8))]
pub struct Wide(i128);

impl Wide {
    #[inline]
    pub fn get(self) -> i128 { self.0 }
}

impl From<i128> for Wide {
    #[inline]
    fn from(i: i128) -> Self { Self(i) }
}

/* Type names a builtin iterator can carry, the index is how a snapshot stores one. */
pub const ITER_KINDS: [&str; 20] = [
    "list_iterator", "range_iterator", "tuple_iterator", "str_ascii_iterator", "str_iterator", "dict_keyiterator", "set_iterator",
    "bytes_iterator", "callable_iterator", "map", "filter", "zip", "enumerate", "list_reverseiterator", "reversed",
    "dict_valueiterator", "dict_itemiterator", "dict_reversekeyiterator", "dict_reversevalueiterator", "dict_reverseitemiterator",
];

pub use crate::vm::methods::BuiltinMethodId;

// One entry per builtin with its name and arity, `n`, `(lo, hi)`, `(lo, var)` or `var`.
macro_rules! builtins {
    ( $( $variant:ident => $name:literal, $arity:tt );* $(;)? ) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        pub enum NativeFnId { $( $variant ),* }

        impl NativeFnId {
            // Python-visible name.
            pub fn name(self) -> &'static str { match self { $( NativeFnId::$variant => $name ),* } }
            // Inverse of `name`, used by snapshot restore.
            pub fn from_name(n: &str) -> Option<Self> { match n { $( $name => Some(NativeFnId::$variant), )* _ => None } }
            // Positional arity as `lo..=hi`, a table indexed by the variant.
            const ARITY: &'static [(u16, u16)] = &[ $( builtins!(@a $arity) ),* ];
            pub fn arity(self) -> (u16, u16) { Self::ARITY[self as usize] }
            pub fn takes(self, n: u16) -> bool { let (lo, hi) = self.arity(); (lo..=hi).contains(&n) }
            // Every builtin, in table order.
            pub const ALL: &'static [NativeFnId] = &[ $( NativeFnId::$variant ),* ];
        }
    };
    (@a var) => { (0, u16::MAX) };
    (@a ($lo:literal, var)) => { ($lo, u16::MAX) };
    (@a ($lo:literal, $hi:literal)) => { ($lo, $hi) };
    (@a $n:literal) => { ($n, $n) };
}

builtins! {
    Print => "print", var;
    Len => "len", 1; Abs => "abs", 1; Str => "str", (0, 3); Int => "int", (0, 2); Float => "float", (0, 1);
    Bool => "bool", (0, 1); Type => "type", 1; Chr => "chr", 1; Ord => "ord", 1;
    Range => "range", (1, 3); Round => "round", (1, 2); Min => "min", (1, var); Max => "max", (1, var); Sum => "sum", (1, 2);
    Sorted => "sorted", 1; Enumerate => "enumerate", (1, 2); Zip => "zip", var;
    List => "list", (0, 1); Tuple => "tuple", (0, 1); Dict => "dict", (0, 1); Set => "set", (0, 1);
    IsInstance => "isinstance", 2; IsSubclass => "issubclass", 2; Input => "input", 0;
    All => "all", 1; Any => "any", 1;
    Bin => "bin", 1; Oct => "oct", 1; Hex => "hex", 1; Divmod => "divmod", 2; Pow => "pow", (2, 3);
    Repr => "repr", 1; Reversed => "reversed", 1; Callable => "callable", 1;
    Format => "format", (1, 2); GetAttr => "getattr", (2, 3); HasAttr => "hasattr", 2; SetAttr => "setattr", 3; DelAttr => "delattr", 2;
    Next => "next", (1, 2); Run => "run", var; Sleep => "sleep", 1;
    Receive => "receive", 0; Map => "map", (2, var); Filter => "filter", 2; Iter => "iter", (1, 2);
    Bytes => "bytes", (0, 3); ImportModule => "import_module", 1; Slice => "slice", (1, 3); Vars => "vars", 1;
    Gather => "gather", var; WithTimeout => "with_timeout", 2; Cancel => "cancel", 1;
    BytesFromHex => "bytes_fromhex", 1; IntFromBytes => "int_from_bytes", 2; IntToBytes => "int_to_bytes", 3; FrozenSet => "frozenset", (0, 1);
    Globals => "globals", 0;
    Super => "super", 0;
    Property => "property", var;
    StaticMethod => "staticmethod", 1;
    ClassMethod => "classmethod", 1;
    SendMsg => "send", 2;
    Eval => "eval", (1, 3);
    Exec => "exec", (1, 3);
    Compile => "compile", (3, 6);
    Id => "id", 1;
    Hash => "hash", 1;
}

/* Insertion-ordered set, each item beside its hash and removed ones undef tombstones, so no table layout decides its order. */
#[derive(Clone, Debug, Default)]
pub struct ValSet {
    entries: Vec<(u64, Val)>,
    index: hashbrown::HashTable<usize>,
    live: usize,
}

impl ValSet {
    pub fn new() -> Self { Self::default() }
    pub fn with_capacity(cap: usize) -> Self {
        let index = if cap > SMALL_DICT { hashbrown::HashTable::with_capacity(cap) } else { hashbrown::HashTable::new() };
        Self { entries: Vec::with_capacity(cap), index, live: 0 }
    }
    /* Content-deduped set of `items`. */
    pub fn from_vals(items: &[Val], heap: &HeapPool) -> Self {
        let mut s = Self::with_capacity(items.len());
        for &v in items { s.insert(v, heap); }
        s
    }
    pub fn len(&self) -> usize { self.live }
    pub fn is_empty(&self) -> bool { self.live == 0 }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.live = 0;
    }
    pub fn iter(&self) -> impl Iterator<Item = &Val> + '_ { self.entries.iter().filter(|e| !e.1.is_undef()).map(|(_, v)| v) }

    /* Whether the index holds every live item, a small set scanning its entries instead. */
    #[inline]
    fn indexed(&self) -> bool { self.entries.len() > SMALL_DICT }

    /* The live item under hash `h` that `eq` accepts. */
    #[inline]
    pub(crate) fn slot(&self, h: u64, mut eq: impl FnMut(Val) -> bool) -> Option<usize> {
        let e = &self.entries;
        if !self.indexed() { return e.iter().position(|x| x.0 == h && !x.1.is_undef() && eq(x.1)); }
        self.index.find(h, |&i| e[i].0 == h && eq(e[i].1)).copied()
    }

    fn reindex(&mut self) {
        if !self.indexed() { self.index = hashbrown::HashTable::new(); return; }
        self.index.clear();
        let e = &self.entries;
        for i in 0..e.len() {
            if !e[i].1.is_undef() { self.index.insert_unique(e[i].0, i, |&j| e[j].0); }
        }
    }

    pub fn contains(&self, v: Val, heap: &HeapPool) -> bool {
        self.slot(hash_val_with_heap(v, heap), |k| eq_member(k, v, heap)).is_some()
    }
    /* Returns true when newly inserted (content-equal value not already present). */
    pub fn insert(&mut self, v: Val, heap: &HeapPool) -> bool {
        let h = hash_val_with_heap(v, heap);
        if self.slot(h, |k| eq_member(k, v, heap)).is_some() { return false; }
        let i = self.entries.len();
        self.entries.push((h, v));
        self.live += 1;
        // Growing past the scan size builds the index over everything stored.
        if self.entries.len() == SMALL_DICT + 1 { self.reindex(); }
        else if self.indexed() { let e = &self.entries; self.index.insert_unique(h, i, |&j| e[j].0); }
        true
    }
    /* Drops `v`, compacting once tombstones outnumber live items. */
    pub fn remove(&mut self, v: Val, heap: &HeapPool) -> bool {
        let h = hash_val_with_heap(v, heap);
        let Some(i) = self.slot(h, |k| eq_member(k, v, heap)) else { return false };
        if self.indexed() && let Ok(entry) = self.index.find_entry(h, |&j| j == i) { entry.remove(); }
        self.entries[i] = (0, Val::undef());
        self.live -= 1;
        if self.entries.len() > 32 && self.entries.len() > 2 * self.live {
            self.entries.retain(|e| !e.1.is_undef());
            self.reindex();
        }
        true
    }
    pub(crate) fn iter_hashed(&self) -> impl Iterator<Item = (u64, Val)> + '_ { self.entries.iter().filter(|e| !e.1.is_undef()).copied() }
}

/* Insertion-ordered dict, each entry keeps its key hash beside it, a HashTable<usize> index gives O(1) get, removed entries become undef tombstones. */
#[derive(Clone, Debug)]
pub struct DictMap {
    entries: Vec<(Val, Val, u64)>,
    index: hashbrown::HashTable<usize>,
    live: usize,
}

/* Entries a dict scans before keeping an index, most instances stay under it. */
const SMALL_DICT: usize = 8;

impl DictMap {
    pub fn new() -> Self { Self::with_capacity(0) }

    pub fn with_capacity(cap: usize) -> Self {
        let index = if cap > SMALL_DICT { hashbrown::HashTable::with_capacity(cap) } else { hashbrown::HashTable::new() };
        Self { entries: Vec::with_capacity(cap), index, live: 0 }
    }

    /* Pairs restored before the heap lives, `rebuild_index` hashes them once it does. */
    pub(crate) fn from_unhashed(pairs: Vec<(Val, Val)>) -> Self {
        let live = pairs.len();
        Self { entries: pairs.into_iter().map(|(k, v)| (k, v, 0)).collect(), index: hashbrown::HashTable::new(), live }
    }

    /* Hashes every entry and indexes them, after a restore. */
    pub(crate) fn rebuild_index(&mut self, heap: &HeapPool) {
        for e in self.entries.iter_mut() { e.2 = hash_val_with_heap(e.0, heap); }
        self.reindex();
    }

    fn reindex(&mut self) {
        if !self.indexed() { self.index = hashbrown::HashTable::new(); return; }
        self.index.clear();
        let e = &self.entries;
        for i in 0..e.len() {
            if !e[i].0.is_undef() { self.index.insert_unique(e[i].2, i, |&j| e[j].2); }
        }
    }

    /* Whether the index holds every live entry, a small dict scanning its entries instead. */
    #[inline]
    fn indexed(&self) -> bool { self.entries.len() > SMALL_DICT }

    /* The live entry under hash `h` that `eq` accepts. */
    #[inline]
    fn slot(&self, h: u64, eq: impl Fn(Val) -> bool) -> Option<usize> {
        let e = &self.entries;
        if !self.indexed() { return e.iter().position(|x| x.2 == h && !x.0.is_undef() && eq(x.0)); }
        self.index.find(h, |&i| e[i].2 == h && eq(e[i].0)).copied()
    }

    #[inline]
    fn find(&self, key: Val, heap: &HeapPool) -> Option<usize> {
        self.slot(hash_val_with_heap(key, heap), |k| eq_member(k, key, heap))
    }

    pub fn get(&self, key: &Val, heap: &HeapPool) -> Option<&Val> {
        self.find(*key, heap).map(|i| &self.entries[i].1)
    }

    pub fn contains_key(&self, key: &Val, heap: &HeapPool) -> bool {
        self.find(*key, heap).is_some()
    }

    pub fn insert(&mut self, key: Val, value: Val, heap: &HeapPool) {
        let h = hash_val_with_heap(key, heap);
        match self.slot(h, |k| eq_member(k, key, heap)) {
            Some(i) => self.entries[i].1 = value,
            None => self.push_hashed(key, value, h),
        }
    }

    pub fn remove(&mut self, key: &Val, heap: &HeapPool) -> Option<Val> {
        let i = self.slot(hash_val_with_heap(*key, heap), |k| eq_member(k, *key, heap))?;
        Some(self.remove_at(i))
    }

    pub(crate) fn key_at(&self, i: usize) -> Val { self.entries.get(i).map_or(Val::undef(), |e| e.0) }
    pub(crate) fn hash_at(&self, i: usize) -> u64 { self.entries.get(i).map_or(0, |e| e.2) }
    /* Entries ever stored, removed ones included until a compaction drops them. */
    pub(crate) fn entry_count(&self) -> usize { self.entries.len() }
    /* The entry holding the string key `name` under its hash `h`. */
    pub(crate) fn position_str(&self, name: &str, h: u64, heap: &HeapPool) -> Option<usize> {
        self.slot(h, |k| matches!(heap.try_get(k), Some(HeapObj::Str(s)) if s == name))
    }
    pub(crate) fn value_at(&self, i: usize) -> Val { self.entries[i].1 }
    pub(crate) fn set_value_at(&mut self, i: usize, v: Val) { self.entries[i].1 = v; }

    /* Appends `key` under hash `h`, the caller checked it is absent. */
    pub(crate) fn push_hashed(&mut self, key: Val, value: Val, h: u64) {
        let i = self.entries.len();
        self.entries.push((key, value, h));
        self.live += 1;
        // Growing past the scan size builds the index over everything stored.
        if self.entries.len() == SMALL_DICT + 1 { self.reindex(); }
        else if self.indexed() { let e = &self.entries; self.index.insert_unique(h, i, |&j| e[j].2); }
    }

    /* Drops entry `i` and returns its value, compacting once tombstones outnumber live entries. */
    pub(crate) fn remove_at(&mut self, i: usize) -> Val {
        if self.indexed() && let Ok(entry) = self.index.find_entry(self.entries[i].2, |&j| j == i) { entry.remove(); }
        self.tombstone(i)
    }

    /* Marks entry `i`, already out of the index, as removed and returns its value. */
    fn tombstone(&mut self, i: usize) -> Val {
        let val = self.entries[i].1;
        self.entries[i] = (Val::undef(), Val::undef(), 0);
        self.live -= 1;
        if self.entries.len() > 32 && self.entries.len() > 2 * self.live {
            self.entries.retain(|e| !e.0.is_undef());
            self.reindex();
        }
        val
    }

    pub fn len(&self) -> usize { self.live }
    pub fn is_empty(&self) -> bool { self.live == 0 }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.live = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = (Val, Val)> + '_ {
        self.entries.iter().filter(|e| !e.0.is_undef()).map(|&(k, v, _)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = Val> + '_ {
        self.iter().map(|(k, _)| k)
    }

    /* Last live entry. */
    pub fn last(&self) -> Option<(Val, Val)> {
        self.entries.iter().rev().find(|e| !e.0.is_undef()).map(|&(k, v, _)| (k, v))
    }

    pub fn from_pairs(pairs: Vec<(Val, Val)>, heap: &HeapPool) -> Self {
        let mut dm = Self::with_capacity(pairs.len());
        for (k, v) in pairs { dm.insert(k, v, heap); }
        dm
    }
}

impl Default for DictMap {
    fn default() -> Self { Self::new() }
}

/* Insert-or-replace in a named-member list, shared by Class members and Func attrs. */
pub(crate) fn set_member(members: &Rc<RefCell<Vec<(String, Val)>>>, name: &str, value: Val) {
    let mut m = members.borrow_mut();
    match m.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value,
        None => m.push((String::from(name), value)),
    }
}

/* Visits every reachable `Val` once, single source of truth for GC traversal. */
pub(crate) fn for_each_val(obj: &HeapObj, mut f: impl FnMut(Val)) {
    match obj {
        HeapObj::Tuple(items) => for &v in items { f(v); },
        HeapObj::Slice(a, b, c) => { f(*a); f(*b); f(*c); }
        HeapObj::List(rc) => for &v in rc.borrow().iter() { f(v); },
        HeapObj::Dict(rc) => for (k, v) in rc.borrow().iter() { f(k); f(v); },
        HeapObj::Set(rc) => for &v in rc.borrow().iter() { f(v); },
        HeapObj::FrozenSet(rc) => for &v in rc.iter() { f(v); },
        HeapObj::BoundMethod(recv, _) => f(*recv),
        HeapObj::Class(_, bases, methods) => {
            for &v in bases { f(v); }
            for (_, v) in methods.borrow().iter() { f(*v); }
        }
        HeapObj::BoundUserMethod(r, fu, cls) => { f(*r); f(*fu); f(*cls); }
        HeapObj::Super(cls, recv) => { f(*cls); f(*recv); }
        HeapObj::Property(g, s) => { f(*g); f(*s); }
        HeapObj::PropertySetter(p) => f(*p),
        HeapObj::StaticMethod(func) => f(*func),
        HeapObj::ClassMethod(func) => f(*func),
        HeapObj::Instance(cls, attrs) => {
            f(*cls);
            for (k, v) in attrs.borrow().iter() { f(k); f(v); }
        }
        HeapObj::Coroutine(c) => {
            for &v in &c.slots { f(v); }
            for &v in &c.stack { f(v); }
            for fr in &c.iters { fr.for_each_val(&mut f); }
            for sf in &c.syncs { sf.for_each_val(&mut f); }
        }
        HeapObj::Func(_, defaults, captures, attrs) => {
            for (_, v) in attrs.borrow().iter() { f(*v); }
            for &v in defaults { f(v); }
            for &(_, v) in captures { f(v); }
        }
        HeapObj::Module(_, attrs) => for (_, v) in attrs { f(*v); },
        HeapObj::ExcInstance(_, args, chain) => { for &v in args { f(v); } f(*chain); }
        HeapObj::GenericAlias(origin, args) => { f(*origin); f(*args); }
        HeapObj::TypeAlias(_, value) => f(*value),
        HeapObj::Union(args) => f(*args),
        // Variants without Val payloads, terminal, nothing to trace.
        HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::LongInt(_)
        | HeapObj::Type(_) | HeapObj::NativeFn(_) | HeapObj::Range(..)
        | HeapObj::Extern(_) | HeapObj::Ellipsis | HeapObj::NotImplemented | HeapObj::TypeVar(_) | HeapObj::Code(_) => {}
        HeapObj::Iter(frame, _) => frame.borrow().for_each_val(&mut f),
        HeapObj::Cell(v) | HeapObj::DictView(v, _) => f(*v),
    }
}

fn slot_str_hash(slots: &[HeapSlot], i: u32) -> u64 {
    match &slots[i as usize].obj { Some(HeapObj::Str(t)) => eq::hash_key(t), _ => 0 }
}

/* Removes `key` unless another slot owns its entry, one hash on the common path. */
fn unintern<K: Eq + core::hash::Hash + Clone>(map: &mut HashMap<K, u32>, key: &K, idx: u32) {
    if let Some(owner) = map.remove(key) && owner != idx { map.insert(key.clone(), owner); }
}

/* Arena allocator with mark-sweep GC, names, constants and single characters interned. */
struct HeapSlot {
    obj: Option<HeapObj>,
    marked: bool,
    /* ASCII-only Str, 0 unknown, 1 yes, 2 no, classified on first use. */
    ascii: u8,
    /* Whether `strings` holds this slot, a name, constant or one-character string. */
    interned: bool,
    /* Whether `classes` holds this slot, a dict or list a user class made. */
    classed: bool,
}

impl HeapSlot {
    fn new(obj: Option<HeapObj>) -> Self { Self { obj, marked: false, ascii: 0, interned: false, classed: false } }
}

/* The user class of a dict or list made by a class deriving from it, and the attributes it got. */
pub type ClassedEntry = (Val, Rc<RefCell<DictMap>>);

pub struct HeapPool {
    slots: Vec<HeapSlot>,
    free_list: Vec<u32>,
    live: usize,
    pub gc_threshold: usize,
    alloc_count: Cell<usize>,
    // What the allocator handed out while this program ran, garbage since the last sweep included.
    meter: &'static meter::Meter,
    // Native work since the last step, scans and copies the op budget charges at the next one.
    work: Cell<usize>,
    limit: usize,
    /* Interns short strings by content, each entry the slot holding one. */
    strings: hashbrown::HashTable<u32>,
    /* Interns short bytes literals so equal `b"..."` share a Val (Hash uses raw bits). */
    bytes_intern: HashMap<Vec<u8>, u32>,
    /* Interns LongInt by value so equal i128s share a Val and stay hash/eq consistent. */
    longints: HashMap<i128, u32>,
    /* Interns Type objects by name so `type(x) is set` and `type(None) is type(None)` hold. */
    types: HashMap<String, u32>,
    // Cached Ellipsis slot index so `... is ...` is True (singleton parity).
    ellipsis_idx: Option<u32>,
    // Same singleton invariant as `ellipsis_idx`, but for `NotImplemented`.
    notimpl_idx: Option<u32>,
    /* Interns bound methods per receiver and method so repeated `obj.m()` share one slot. */
    bound_methods: HashMap<(u64, BuiltinMethodId), u32>,
    bound_user_methods: HashMap<(u64, u64, u64), u32>,
    /* Vals visited by the last mark, sizes the next collection. */
    marked_vals: usize,
    alloc_limit: usize,
    /* Reused across mark() calls, cleared not freed, so GC never re-allocates under pressure. */
    mark_worklist: Vec<u32>,
    /* The class and attributes of each classed slot, which stays a real dict or list for the builtins. */
    classes: HashMap<u32, ClassedEntry>,
}

// A program gone leaves the shared instance total.
impl Drop for HeapPool {
    fn drop(&mut self) { self.meter.retire(); }
}

impl HeapPool {
    /* `i` as the narrowest int, inline when it fits 48 bits, else a LongInt. */
    #[inline]
    pub fn int(&mut self, i: i128) -> Result<Val, VmErr> {
        match i64::try_from(i).ok().and_then(Val::int_checked) {
            Some(v) => Ok(v),
            None => self.alloc(HeapObj::LongInt(i.into())),
        }
    }

    pub fn new(limit: usize) -> Self {
        Self {
            slots: Vec::new(),
            free_list: Vec::new(),
            live: 0,
            gc_threshold: 512,
            alloc_count: Cell::new(0),
            meter: crate::util::leak(Box::new(meter::Meter::new(limit))),
            work: Cell::new(0),
            limit,
            strings: hashbrown::HashTable::new(),
            bytes_intern: HashMap::default(),
            longints: HashMap::default(),
            types: HashMap::default(),
            ellipsis_idx: None,
            notimpl_idx: None,
            bound_methods: HashMap::default(),
            bound_user_methods: HashMap::default(),
            marked_vals: 0,
            alloc_limit: 4096,
            mark_worklist: Vec::with_capacity(64),
            classes: HashMap::default(),
        }
    }

    /* Existing interned/singleton slot for `obj`, longer strings only through `intern_str`. */
    #[inline]
    fn intern_lookup(&self, obj: &HeapObj) -> Option<u32> {
        match obj {
            // Every one-character and empty string shares one slot.
            HeapObj::Str(s) if s.len() <= 1 => self.string_slot(s),
            HeapObj::Bytes(b) if b.len() <= 128 => self.bytes_intern.get(b).copied(),
            HeapObj::LongInt(i) => self.longints.get(&i.get()).copied(),
            HeapObj::Type(name) => self.types.get(name).copied(),
            HeapObj::Ellipsis => self.ellipsis_idx,
            HeapObj::NotImplemented => self.notimpl_idx,
            HeapObj::BoundMethod(recv, id) => self.bound_methods.get(&(recv.0, *id)).copied(),
            HeapObj::BoundUserMethod(r, f, c) => self.bound_user_methods.get(&(r.0, f.0, c.0)).copied(),
            _ => None,
        }
    }

    /* The slot interning text `s`, each candidate compared by its own text. */
    #[inline]
    fn string_slot(&self, s: &str) -> Option<u32> {
        let slots = &self.slots;
        self.strings.find(eq::hash_key(s), |&i| matches!(&slots[i as usize].obj, Some(HeapObj::Str(t)) if t == s)).copied()
    }

    /* Register `idx` in the intern and singleton tables its object belongs to. */
    fn intern_insert(&mut self, idx: u32) {
        match self.slots[idx as usize].obj.as_ref() {
            Some(HeapObj::Str(s)) if s.len() <= 1 => self.intern_text(idx, eq::hash_key(s)),
            Some(HeapObj::Bytes(b)) if b.len() <= 128 => { self.bytes_intern.insert(b.clone(), idx); }
            Some(HeapObj::LongInt(i)) => { self.longints.insert(i.get(), idx); }
            Some(HeapObj::Type(name)) => { self.types.insert(name.clone(), idx); }
            Some(HeapObj::Ellipsis) => { self.ellipsis_idx = Some(idx); }
            Some(HeapObj::NotImplemented) => { self.notimpl_idx = Some(idx); }
            Some(HeapObj::BoundMethod(recv, id)) => { self.bound_methods.insert((recv.0, *id), idx); }
            Some(HeapObj::BoundUserMethod(r, f, c)) => { self.bound_user_methods.insert((r.0, f.0, c.0), idx); }
            _ => {}
        }
    }

    /* Drop `idx` from the tables `intern_insert` filled, long strings and bytes never entered one. */
    fn intern_remove(&mut self, idx: u32) {
        match self.slots[idx as usize].obj.as_ref() {
            Some(HeapObj::Str(s)) if self.slots[idx as usize].interned => {
                if let Ok(e) = self.strings.find_entry(eq::hash_key(s), |&i| i == idx) { e.remove(); }
            }
            Some(HeapObj::Bytes(b)) if b.len() <= 128 => unintern(&mut self.bytes_intern, b, idx),
            Some(HeapObj::LongInt(i)) => unintern(&mut self.longints, &i.get(), idx),
            Some(HeapObj::Type(name)) => unintern(&mut self.types, name, idx),
            Some(HeapObj::Ellipsis) if self.ellipsis_idx == Some(idx) => { self.ellipsis_idx = None; }
            Some(HeapObj::NotImplemented) if self.notimpl_idx == Some(idx) => { self.notimpl_idx = None; }
            Some(HeapObj::BoundMethod(recv, id)) => unintern(&mut self.bound_methods, &(recv.0, *id), idx),
            Some(HeapObj::BoundUserMethod(r, f, c)) => unintern(&mut self.bound_user_methods, &(r.0, f.0, c.0), idx),
            _ => {}
        }
    }

    /* Take a free slot or grow the arena. */
    fn place(&mut self, obj: HeapObj) -> u32 {
        if let Some(i) = self.free_list.pop() {
            self.slots[i as usize] = HeapSlot::new(Some(obj));
            i
        } else {
            let i = self.slots.len() as u32;
            self.slots.push(HeapSlot::new(Some(obj)));
            i
        }
    }

    pub fn alloc(&mut self, obj: HeapObj) -> Result<Val, VmErr> { self.admit(obj, true) }

    /* The shared Val for a name or constant `s`, runtime strings never enter. */
    pub fn intern_str(&mut self, s: &str) -> Result<Val, VmErr> {
        if s.len() > 128 { return self.alloc(HeapObj::Str(s.into())); }
        if let Some(i) = self.string_slot(s) { return Ok(Val::heap(i)); }
        let v = self.alloc(HeapObj::Str(s.into()))?;
        // A one-character string entered on allocation.
        if !self.slots[v.as_heap() as usize].interned { self.intern_text(v.as_heap(), eq::hash_key(s)); }
        Ok(v)
    }

    /* Indexes slot `idx`, a string under hash `h`, in the intern table. */
    fn intern_text(&mut self, idx: u32, h: u64) {
        self.slots[idx as usize].interned = true;
        let slots = &self.slots;
        self.strings.insert_unique(h, idx, |&i| slot_str_hash(slots, i));
    }

    /* Reserved for constructing the exception that reports the limit itself, skips the soft limit but not the hard slot cap. */
    pub fn alloc_emergency(&mut self, obj: HeapObj) -> Result<Val, VmErr> { self.admit(obj, false) }

    /* An interned twin, or a fresh slot within the hard cap and, when `soft`, twice the memory limit. */
    #[inline(always)]
    fn admit(&mut self, obj: HeapObj, soft: bool) -> Result<Val, VmErr> {
        if let Some(idx) = self.intern_lookup(&obj) { return Ok(Val::heap(idx)); }
        if soft && self.meter.held() > self.limit.saturating_mul(2) { return Err(cold_heap()); }
        if self.slots.len() >= (1 << 28) { return Err(VmErr::Heap); }
        let idx = self.place(obj);
        self.intern_insert(idx);
        self.live += 1;
        self.alloc_count.set(self.alloc_count.get().saturating_add(1));
        Ok(Val::heap(idx))
    }

    /* What a result may still take, the limit or what twice the limit leaves, garbage included. */
    pub fn room(&self) -> usize {
        self.limit.min(self.limit.saturating_mul(2).saturating_sub(self.meter.held())).min(self.meter.instance_room())
    }

    /* Refuses a result of `extra` bytes before it is built, so it never reaches the allocator. */
    #[inline]
    pub fn reserve(&self, extra: usize) -> Result<(), VmErr> {
        if extra > self.room() { Err(cold_heap()) } else { Ok(()) }
    }

    /* Whether the program holds more than its limit or took the instance past its budget, garbage included. */
    #[inline]
    pub fn over(&self) -> bool { self.meter.over() }

    /* What the allocator holds for the program, garbage since the last sweep included. */
    pub fn bytes(&self) -> usize { self.meter.held() }

    /* The most the allocator held for the program at once. */
    pub fn peak(&self) -> usize { self.meter.peak() }

    /* The meter this program charges, which its runs make the payer. */
    pub fn meter(&self) -> &'static meter::Meter { self.meter }

    /* Whether a memory limit is set that no metering allocator counts. */
    pub(crate) fn unmetered(&self) -> bool { self.limit != usize::MAX && !meter::installed() }

    pub fn mark(&mut self, v: Val) {
        if !v.is_heap() { return; }
        /* Split borrow, closure needs &mut mark_worklist while we read slots. */
        let HeapPool { slots, mark_worklist, marked_vals, classes, .. } = self;
        mark_worklist.push(v.as_heap());
        while let Some(idx) = mark_worklist.pop() {
            let idx = idx as usize;
            if slots[idx].marked { continue; }
            slots[idx].marked = true;
            *marked_vals += 1;
            if slots[idx].classed && let Some((cls, attrs)) = classes.get(&(idx as u32)) {
                for v in core::iter::once(*cls).chain(attrs.borrow().iter().flat_map(|(k, v)| [k, v])) {
                    if v.is_heap() { mark_worklist.push(v.as_heap()); }
                }
            }
            if let Some(obj) = &slots[idx].obj {
                for_each_val(obj, |val| { *marked_vals += 1; if val.is_heap() { mark_worklist.push(val.as_heap()); } });
            }
        }
    }

    pub fn sweep(&mut self) {
        for idx in 0..self.slots.len() {
            let slot = &mut self.slots[idx];
            if slot.obj.is_none() { continue; }
            if slot.marked { slot.marked = false; continue; }
            if slot.classed { slot.classed = false; self.classes.remove(&(idx as u32)); }
            self.intern_remove(idx as u32);
            self.slots[idx].obj = None;
            self.free_list.push(idx as u32);
            self.live -= 1;
        }

        // Both triggers scale with the volume the last mark walked.
        self.gc_threshold = (self.live * 2).max(512).max(self.marked_vals / 4);
        self.alloc_limit = (self.marked_vals / 4).max(4096);
        self.marked_vals = 0;
        self.alloc_count.set(0);

        // Cap free list at 512K slots, sort to prefer low indices and reduce fragmentation.
        if self.free_list.len() > 524_288 {
            self.free_list.sort_unstable();
            self.free_list.truncate(524_288);
        }
    }

    /* One entry per slot, None when free. */
    pub(crate) fn snapshot_objs(&self) -> impl Iterator<Item = Option<&HeapObj>> {
        self.slots.iter().map(|s| s.obj.as_ref())
    }

    /* Replace the pool, rebuild free and intern tables. */
    pub(crate) fn restore_objs(&mut self, objs: Vec<Option<HeapObj>>) {
        self.slots = objs.into_iter().map(HeapSlot::new).collect();
        self.classes.clear();
        self.free_list.clear();
        self.strings.clear();
        self.bytes_intern.clear();
        self.longints.clear();
        self.types.clear();
        self.ellipsis_idx = None;
        self.notimpl_idx = None;
        self.bound_methods.clear();
        self.bound_user_methods.clear();
        self.live = 0;
        for idx in 0..self.slots.len() {
            if self.slots[idx].obj.is_none() { self.free_list.push(idx as u32); continue; }
            self.live += 1;
            self.intern_insert(idx as u32);
        }
        self.gc_threshold = (self.live * 2).max(512);
        self.alloc_limit = 4096;
        self.marked_vals = 0;
        self.alloc_count.set(0);
    }

    /* Swap a live slot's object during restore. */
    pub(crate) fn replace_obj(&mut self, idx: u32, obj: HeapObj) {
        self.slots[idx as usize] = HeapSlot::new(Some(obj));
    }

    /* Whether the next step leaves the fast path, for a collection or for a slice of native work to charge. */
    pub fn needs_gc(&self) -> bool {
        let alloc_limit = (self.live / 4).max(self.alloc_limit);
        self.live >= self.gc_threshold || self.alloc_count.get() >= alloc_limit || self.work.get() > WORK_SLICE || self.over()
    }

    /* Counts `n` units of native work, the next step charges them to the op budget. */
    #[inline]
    pub fn spend(&self, n: usize) { self.work.set(self.work.get().saturating_add(n)); }

    /* Whether native work waits to be charged. */
    #[inline]
    pub fn has_work(&self) -> bool { self.work.get() != 0 }

    /* The native work counted since the last step, which starts again from zero. */
    #[inline]
    pub fn take_work(&self) -> usize {
        let w = self.work.get();
        if w != 0 { self.work.set(0); }
        w
    }

    /* ASCII-only Str, scanned once per slot. */
    #[inline]
    pub fn str_is_ascii(&mut self, v: Val) -> bool {
        if !v.is_heap() { return false; }
        let slot = &mut self.slots[v.as_heap() as usize];
        if slot.ascii == 0 {
            slot.ascii = match &slot.obj { Some(HeapObj::Str(s)) if s.is_ascii() => 1, _ => 2 };
        }
        slot.ascii == 1
    }

    pub fn usage(&self) -> usize { self.live }

    /* Object-count budget, reused as a byte cap for single oversized allocations. */
    pub fn limit(&self) -> usize { self.limit }

    #[inline(always)] pub fn get(&self, v: Val) -> &HeapObj {
        self.slots[v.as_heap() as usize].obj
            .as_ref()
            .expect("garbage collector invariant violated: live Val references a freed heap slot")
    }
    #[inline(always)] pub fn get_mut(&mut self, v: Val) -> &mut HeapObj {
        self.slots[v.as_heap() as usize].obj
            .as_mut()
            .expect("garbage collector invariant violated: live Val references a freed heap slot (mut)")
    }

    /* Panic-free access for type checks, None when `v` is not a live heap object. */
    #[inline] pub fn try_get(&self, v: Val) -> Option<&HeapObj> {
        if !v.is_heap() { return None; }
        self.slots.get(v.as_heap() as usize).and_then(|s| s.obj.as_ref())
    }
    /* The object at `v` unless a user class made it, so a fast path never skips that class. */
    #[inline] pub fn plain(&self, v: Val) -> Option<&HeapObj> {
        if !v.is_heap() { return None; }
        self.slots.get(v.as_heap() as usize).filter(|s| !s.classed).and_then(|s| s.obj.as_ref())
    }
    /* The class and attributes of `v`, a dict or list some user class made. */
    #[inline] pub fn classed(&self, v: Val) -> Option<&ClassedEntry> {
        if !v.is_heap() || !self.slots.get(v.as_heap() as usize).is_some_and(|s| s.classed) { return None; }
        self.classes.get(&v.as_heap())
    }
    /* Marks `v`, a fresh dict or list, as made by user class `cls` with `attrs`. */
    pub(crate) fn set_class(&mut self, v: Val, cls: Val, attrs: Rc<RefCell<DictMap>>) {
        self.slots[v.as_heap() as usize].classed = true;
        self.classes.insert(v.as_heap(), (cls, attrs));
    }
    /* Every classed slot with its class and attributes, for snapshots. */
    pub(crate) fn classed_slots(&self) -> impl Iterator<Item = (&u32, &ClassedEntry)> { self.classes.iter() }
    #[inline] pub fn try_get_mut(&mut self, v: Val) -> Option<&mut HeapObj> {
        if !v.is_heap() { return None; }
        self.slots.get_mut(v.as_heap() as usize).and_then(|s| s.obj.as_mut())
    }


    /* Identity probe for the `NotImplemented` singleton, consumed by the dunder dispatch protocol. */
    #[inline(always)]
    pub fn is_not_implemented(&self, v: Val) -> bool {
        v.is_heap()
            && matches!(self.slots[v.as_heap() as usize].obj.as_ref(), Some(HeapObj::NotImplemented))
    }

    /* `child` is `ancestor` or has it in its transitive bases. Identity on heap idx, classes are interned per-MakeClass and never mutated, so direct equality suffices. */
    pub fn is_subclass(&self, child: Val, ancestor: Val) -> bool {
        if child.0 == ancestor.0 { return true; }
        if !child.is_heap() { return false; }
        let HeapObj::Class(_, bases, _) = self.get(child) else { return false; };
        bases.iter().any(|&b| self.is_subclass(b, ancestor))
    }
}

/* Widens int/bool/LongInt to i128 for the slow path, None on non-integer operands. */
#[inline]
pub fn as_i128(v: Val, heap: &HeapPool) -> Option<i128> {
    if v.is_int() { Some(v.as_int() as i128) }
    else if v.is_bool() { Some(v.as_bool() as i128) }
    else if v.is_heap() {
        match heap.get(v) {
            HeapObj::LongInt(i) => Some(i.get()),
            _ => None,
        }
    }
    else { None }
}

/* Wide-int payload only, skips inline ints and bools unlike as_i128. */
#[inline]
pub fn as_long_int(v: Val, heap: &HeapPool) -> Option<i128> {
    if v.is_heap() && let HeapObj::LongInt(i) = heap.get(v) { Some(i.get()) } else { None }
}
