use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};
use core::cell::{Cell, RefCell};
use crate::util::hash::FxHashMap as HashMap;
use crate::util::probe::{Interns, Places};

pub mod frames;
pub mod eq;
pub mod err;
pub mod math;
pub(crate) mod wide;
pub mod scheduler;
pub mod meter;
pub mod pool;
pub mod foreign;
pub mod host;
pub mod object;
pub(crate) mod callable;
pub use host::Host;
pub use foreign::{BinOp, UnOp};
pub use object::Head;

pub use frames::*;
pub use eq::*;
pub use err::*;
pub use math::*;
pub use scheduler::*;

/* the caps of one run on its op budget and the bytes it holds */
#[derive(Clone, Copy)]
pub struct Limits { pub ops: u64, pub memory: usize }

impl Limits {
    pub fn sandbox() -> Self { Self { ops: 100_000_000, memory: 256 << 20 } }
}

// how many coroutines run at once and how many frames a traceback keeps
pub const MAX_CALLS: usize = 192;

// how deep native loops nest so the deepest builtin callback fits the wasm stack
pub const MAX_NESTED: usize = 128;

// what a value takes in a container which nan boxing makes 8 bytes everywhere
pub const VAL_BYTES: usize = 8;
/* native work that piles up before the loop leaves its fast path to charge it */
const WORK_SLICE: usize = 1 << 16;
// bytes a run holds before its first collection by size which small runs never reach
const GC_BYTES_FLOOR: usize = 8 << 20;
// the live slots the first collection waits for
const GC_FIRST: usize = 512;
// two under 512 so the slots a loop takes before collecting fit in the table
const GC_SLOTS_FLOOR: usize = 510;
// slots past 512 the table takes there so what a loop makes before collecting fits
const GC_SLACK: usize = 8;

/* a host callable bound at compile time taking positionals and a keyword map or none */
pub type ExternCallable =
    alloc::sync::Arc<dyn Fn(&mut HeapPool, &[Val], Option<Val>) -> Result<Val, VmErr> + Send + Sync>;

#[derive(Clone)]
pub struct ExternFn {
    pub name: String,
    pub func: ExternCallable,
}

impl core::fmt::Debug for ExternFn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExternFn").field("name", &self.name).finish()
    }
}

/* a value boxed into a nan's 8 bytes as the abi module lays out */
use crate::abi::nan_box::{
    QNAN, SIGN, TAG_UNDEF, TAG_NONE, TAG_TRUE, TAG_FALSE, TAG_INT, TAG_HEAP, TAG_FOREIGN,
    INT_PAYLOAD_MASK,
};

#[derive(Clone, Copy, Debug)]
pub struct Val(pub(crate) u64);

impl PartialEq for Val {
    #[inline] fn eq(&self, o: &Self) -> bool {
        if self.0 == o.0 { return true; }
        // inline numbers and bools compare by value so true and 1 and 1.0 are equal
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
    /* the one quiet nan every nan becomes with its tag bit clear so `is_float()` holds */
    const NAN_BASE: u64 = 0x7FF8_0000_0000_0000;
    const EXP_MASK: u64 = 0x7FF0_0000_0000_0000;
    #[inline(always)] pub fn float(f: f64) -> Self {
        let bits = f.to_bits();
        if (bits & Self::EXP_MASK) == Self::EXP_MASK { Self::special_float(bits) } else { Self(bits) }
    }
    /* an infinity keeps its bits and a nan keeps only its sign */
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
    /* a native by its place as a bound value's function with no slot */
    #[inline(always)] pub fn native_ref(id: crate::lang::NativeId) -> Self { Self(QNAN | 7 | (id.0 as u64) << 8) }
    /* the place of the native a bound value's function names in place */
    #[inline(always)] pub fn as_native_ref(&self) -> Option<crate::lang::NativeId> { (self.0 & 0xFFFF_0000_0000_00FF == QNAN | 7).then_some(crate::lang::NativeId((self.0 >> 8) as u16)) }
    /* the mark of an unbound local which a load checks with one compare */
    #[inline(always)] pub fn undef() -> Self { Self(TAG_UNDEF) }

    #[inline(always)] pub fn is_float(&self) -> bool { (self.0 & QNAN) != QNAN }
    #[inline(always)] pub fn is_int(&self) -> bool { (self.0 & (QNAN | SIGN)) == TAG_INT }
    #[inline(always)] pub fn is_none(&self) -> bool { self.0 == TAG_NONE }
    #[inline(always)] pub fn is_true(&self) -> bool { self.0 == TAG_TRUE }
    #[inline(always)] pub fn is_false(&self) -> bool { self.0 == TAG_FALSE }
    #[inline(always)] pub fn is_bool(&self) -> bool { self.0 == TAG_TRUE || self.0 == TAG_FALSE }
    #[inline(always)] pub fn is_undef(&self) -> bool { self.0 == TAG_UNDEF }
    /* the exact prefix and low nibble so no other tag passes for a heap index */
    #[inline(always)] pub fn is_heap(&self) -> bool { (self.0 & 0xFFFF_0000_0000_000F) == TAG_HEAP }
    #[inline(always)] pub fn foreign(generation: u16, index: u32) -> Self { Self(TAG_FOREIGN | (generation as u64) << 32 | index as u64) }
    #[inline(always)] pub fn is_foreign(&self) -> bool { (self.0 & 0xFFFF_0000_0000_0000) == TAG_FOREIGN }
    /* the generation and index of a FOREIGN value */
    #[inline] pub fn foreign_parts(&self) -> Option<(u16, u32)> { self.is_foreign().then_some(((self.0 >> 32) as u16, self.0 as u32)) }

    #[inline(always)] pub fn as_float(&self) -> f64 { f64::from_bits(self.0) }
    /* the bits of the value as the wire and the hosts carry them */
    #[inline(always)] pub fn raw(&self) -> u64 { self.0 }
    /* the `Val` whose bits `raw` gave which heap reads check like any other */
    #[inline(always)] pub fn from_raw(u: u64) -> Self { Self(u) }
    #[inline(always)] pub fn as_int(&self) -> i64 {
        let raw = (self.0 & INT_PAYLOAD_MASK) as i64;
        (raw << 16) >> 16
    }
    // the unsigned payload a code address keeps whole past bit 47
    #[inline(always)] pub fn word(&self) -> usize { (self.0 & INT_PAYLOAD_MASK) as usize }
    #[inline(always)] pub fn as_bool(&self) -> bool { self.0 == TAG_TRUE }
    #[inline(always)] pub fn as_heap(&self) -> u32 { ((self.0 >> 4) & 0x0FFF_FFFF) as u32 }
}


/* the objects of the heap's arena which a heap value names by index */
#[derive(Clone, Debug)]
// wasm keeps a byte tag since a niche there slows every match on the kind
#[cfg_attr(target_arch = "wasm32", repr(u8))]
pub enum HeapObj {
    Str(String),
    Bytes(Vec<u8>),
    List(Rc<RefCell<Vec<Val>>>),
    Dict(Rc<RefCell<DictMap>>),
    Set(Rc<RefCell<ValSet>>),
    Tuple(Vec<Val>),
    // a function of the program with its defaults and the cells it closes over
    Func { fi: usize, defaults: Box<[Val]>, cells: Box<[(usize, Val)]> },
    Range(i64, i64, i64),
    /* a range with a bound past i64 holding its start stop and step as ints */
    BigRange(Val, Val, Val),
    // a native of the runtime's table or a binding a host gave by its place
    Native(crate::lang::NativeId),
    // a class a type or an instance whose meta decides its fields methods and operators
    Object { meta: Val, slots: Rc<RefCell<DictMap>> },
    // a callable with its receiver fixed as its first argument
    Bound { recv: Val, func: Val },
    // a generator or coroutine with its state and the helper calls it parked inside
    Coroutine(Box<Coro>),
    /* a module an import made whose attributes loads read */
    Module(String, Vec<(String, Val)>),
    // a builtin iterator whose frame every name for it shares
    Iter(Rc<RefCell<IterFrame>>, &'static str),
    // a variable a closure shares with the frame binding it and undef while unbound
    Cell(Val),
    // a mutable sequence of bytes changed in place through the heap
    ByteArray(Vec<u8>),
}

/* the bit of a coroutine's layout marking the code its evicted slots were laid for */
pub const SAVED: u32 = 1 << 31;

/* a generator or coroutine body with where it resumes and the state it resumes with */
#[derive(Clone, Debug)]
pub struct Coro {
    pub ip: usize,
    pub slots: Vec<Val>,
    // the recursion levels it kept through sections as it parked
    pub levels: Vec<Val>,
    pub body: BodyRef,
    pub iters: Vec<IterFrame>,
    pub syncs: Vec<ParkedFrame>,
    pub excs: Vec<ExceptionFrame>,
    // whether it runs now so a resume from inside itself is found in constant time
    pub running: bool,
    // the section code it resumes in by layout or past `SAVED` its saved slots' code
    pub layout: u32,
    // the item a builtin's step took ahead of time for its next step or undef
    pub pending: Val,
    // the top a floor parked over rows at or one for a generator over rows
    pub top: u16,
}

impl Coro {
    /* a body not yet started with its frame bound */
    pub fn fresh(slots: Vec<Val>, body: BodyRef) -> Box<Self> {
        Box::new(Self { ip: 0, slots, levels: Vec::new(), body, iters: Vec::new(), syncs: Vec::new(), excs: Vec::new(), running: false, layout: 0, pending: Val::undef(), top: 0 })
    }

    /* whether it resumes only in the section code its layout names */
    #[inline(always)]
    pub fn pinned(&self) -> bool { (self.layout as i32) > 0 }

    /* the row a wait left for what wakes it in its innermost helper or itself */
    pub fn wait_slot(&mut self) -> Option<&mut Val> {
        match self.syncs.last_mut() {
            Some(f) => f.top.checked_sub(1).and_then(|t| f.slots.get_mut(t as usize)),
            None => self.top.checked_sub(1).and_then(|t| self.slots.get_mut(t as usize)),
        }
    }
}

/* an insertion ordered set keeping each item beside its hash and removed ones as undef */
#[derive(Clone, Debug, Default)]
pub struct ValSet {
    entries: Vec<(u64, Val)>,
    index: Places,
    live: usize,
    // every entry before it was removed so walks and pops from the front skip them
    head: usize,
}

/* the bytes a set item takes with its hash and its share of the places */
pub const SET_TABLE: usize = 26;

/* the bytes a dict entry takes with its hash and its share of the places */
pub const MAP_TABLE: usize = 34;

impl ValSet {
    pub fn new() -> Self { Self::default() }
    pub fn with_capacity(cap: usize) -> Self {
        let index = if cap > SMALL_DICT { Places::with_capacity(cap) } else { Places::default() };
        Self { entries: Vec::with_capacity(cap), index, live: 0, head: 0 }
    }
    /* a set of `items` with equal ones kept once */
    #[inline(never)]
    pub fn from_vals(items: &[Val], heap: &HeapPool) -> Self {
        let mut s = Self::with_capacity(items.len());
        for &v in items { s.insert(v, heap); }
        s
    }

    /* a set of `items` refused past the memory left and stopped at the budget */
    pub fn build(items: &[Val], heap: &HeapPool) -> Result<Self, VmErr> {
        // a short run holds too little to refuse or to spend on
        if items.len() <= 64 { return Ok(Self::from_vals(items, heap)); }
        let mut s = Self::new();
        s.put_all(items, heap)?;
        Ok(s)
    }

    /* puts `items` in at a size the limit allows or grown within the memory left */
    pub fn put_all(&mut self, items: &[Val], heap: &HeapPool) -> Result<(), VmErr> {
        // a short run grows too little to refuse
        if items.len() <= 64 {
            for &v in items { self.insert(v, heap); heap.within_budget()?; }
            return Ok(());
        }
        let need = items.len().saturating_mul(SET_TABLE);
        // the limit every host shares decides the sizing so each host pays the same probes
        let sized = self.entries.is_empty() && need <= heap.limit();
        if sized { heap.reserve(need)?; *self = Self::with_capacity(items.len()); }
        for &v in items {
            let new = self.insert(v, heap);
            heap.within_budget()?;
            if !sized && new && self.live >= 64 && self.live.is_power_of_two() { heap.reserve(self.live.saturating_mul(2 * SET_TABLE))?; }
        }
        Ok(())
    }
    pub fn len(&self) -> usize { self.live }
    pub fn is_empty(&self) -> bool { self.live == 0 }
    pub fn clear(&mut self) {
        // places hold items only past the scan size so a short set skips them
        if self.indexed() { self.index.clear(); }
        self.entries.clear();
        (self.live, self.head) = (0, 0);
    }
    pub fn iter(&self) -> impl Iterator<Item = &Val> + '_ { self.entries[self.head..].iter().filter(|e| !e.1.is_undef()).map(|(_, v)| v) }

    /* whether the index holds every live item where a small set scans its entries */
    #[inline]
    fn indexed(&self) -> bool { self.entries.len() > SMALL_DICT }

    /* the live item under hash `h` that `eq` accepts where long probes pay */
    #[inline]
    pub(crate) fn slot(&self, h: u64, heap: &HeapPool, mut eq: impl FnMut(Val) -> bool) -> Option<usize> {
        let e = &self.entries;
        if !self.indexed() { return e.iter().position(|x| x.0 == h && !x.1.is_undef() && eq(x.1)); }
        self.index.find(h, |i| e[i].0 == h && eq(e[i].1), |n| heap.spend(n))
    }

    fn reindex(&mut self) {
        if !self.indexed() { self.index = Places::default(); return; }
        // room for the next item too so growth builds the places once a doubling
        self.index.reset(self.entries.len() + 1);
        for (i, x) in self.entries.iter().enumerate() {
            if !x.1.is_undef() { self.index.push(x.0, i); }
        }
    }

    pub fn contains(&self, v: Val, heap: &HeapPool) -> bool {
        self.slot(hash_val_with_heap(v, heap), heap, |k| eq_member(k, v, heap)).is_some()
    }
    /* whether `v` went in since no equal item was there */
    pub fn insert(&mut self, v: Val, heap: &HeapPool) -> bool {
        let h = hash_val_with_heap(v, heap);
        let (i, e) = (self.entries.len(), &self.entries);
        // a miss ends at the place the item goes
        let place = if i > SMALL_DICT {
            match self.index.place(h, |j| e[j].0 == h && eq_member(e[j].1, v, heap), |n| heap.spend(n)) {
                Ok(_) => return false,
                Err(at) => at,
            }
        } else {
            if e.iter().any(|x| x.0 == h && !x.1.is_undef() && eq_member(x.1, v, heap)) { return false; }
            0
        };
        self.entries.push((h, v));
        self.live += 1;
        // growing past the scan size builds the index over everything stored
        if i == SMALL_DICT || (i > SMALL_DICT && i >= self.index.room()) { self.reindex(); }
        else if i > SMALL_DICT { self.index.put(place, i); }
        true
    }
    /* drops `v` and compacts once removed items outnumber live ones */
    pub fn remove(&mut self, v: Val, heap: &HeapPool) -> bool {
        let h = hash_val_with_heap(v, heap);
        let e = &self.entries;
        let found = if e.len() > SMALL_DICT {
            self.index.take(h, |j| e[j].0 == h && eq_member(e[j].1, v, heap), |n| heap.spend(n))
        } else {
            e.iter().position(|x| x.0 == h && !x.1.is_undef() && eq_member(x.1, v, heap))
        };
        let Some(i) = found else { return false };
        self.entries[i] = (0, Val::undef());
        self.live -= 1;
        if self.entries.len() > 32 && self.entries.len() > 2 * self.live {
            self.entries.retain(|e| !e.1.is_undef());
            self.head = 0;
            if roomy(&self.index, self.live) { self.index = Places::default(); self.entries.shrink_to(2 * self.live); }
            self.reindex();
        }
        true
    }
    pub(crate) fn iter_hashed(&self) -> impl Iterator<Item = (u64, Val)> + '_ { self.entries[self.head..].iter().filter(|e| !e.1.is_undef()).copied() }
    /* takes out the first live item and starts the next search past it */
    pub fn pop_first(&mut self, heap: &HeapPool) -> Option<Val> {
        self.head += self.entries[self.head..].iter().take_while(|e| e.1.is_undef()).count();
        let v = self.entries.get(self.head)?.1;
        self.remove(v, heap);
        Some(v)
    }
    /* a copy of the set with room for `extra` more items so growth moves nothing */
    pub(crate) fn copy_with_room(&self, extra: usize) -> Self {
        let mut entries = Vec::with_capacity(self.entries.len().saturating_add(extra));
        entries.extend_from_slice(&self.entries);
        Self { entries, index: self.index.clone(), live: self.live, head: self.head }
    }
    /* the item entry `i` that `slot` found holds */
    pub(crate) fn item_at(&self, i: usize) -> Val { self.entries[i].1 }
}

/* whether places that once grew large hold four times what `live` entries need */
#[inline]
fn roomy(index: &Places, live: usize) -> bool { index.room() > 1024 && index.room() > 4 * (live + 1) }

/* an insertion ordered dict whose places find each key and whose removed entries stay undef */
#[derive(Clone, Debug)]
pub struct DictMap {
    entries: Vec<(Val, Val, u64)>,
    index: Places,
    live: usize,
    // the shape naming its keys in order while it only appended and `NO_SHAPE` after
    shape: u32,
    // the length that builds the places again lowered for each entry cut off the end
    grow_at: usize,
}

/* a dict whose keys no shape names */
pub(crate) const NO_SHAPE: u32 = 0;

/* entries a dict scans before keeping an index which most instances stay under */
const SMALL_DICT: usize = 8;

impl DictMap {
    pub fn new() -> Self { Self::with_capacity(0) }

    pub fn with_capacity(cap: usize) -> Self {
        if cap <= SMALL_DICT { return Self { entries: Vec::with_capacity(cap), index: Places::default(), live: 0, shape: NO_SHAPE, grow_at: 0 }; }
        let index = Places::with_capacity(cap);
        Self { entries: Vec::with_capacity(cap), grow_at: index.room(), index, live: 0, shape: NO_SHAPE }
    }

    /* pairs restored before the heap lives which `rebuild_index` hashes once it does */
    pub(crate) fn from_unhashed(pairs: Vec<(Val, Val)>) -> Self {
        let live = pairs.len();
        Self { entries: pairs.into_iter().map(|(k, v)| (k, v, 0)).collect(), index: Places::default(), live, shape: NO_SHAPE, grow_at: 0 }
    }

    /* hashes and indexes every entry after a restore */
    pub(crate) fn rebuild_index(&mut self, heap: &HeapPool) {
        for e in self.entries.iter_mut() { e.2 = hash_val_with_heap(e.0, heap); }
        self.reindex();
    }

    fn reindex(&mut self) {
        // places cut entries filled start over once they hold four times what is live
        if self.grow_at < self.index.room() && roomy(&self.index, self.live) { self.index = Places::default(); }
        if !self.indexed() { (self.index, self.grow_at) = (Places::default(), 0); return; }
        // room for the next entry too so growth builds the places once a doubling
        self.index.reset(self.entries.len() + 1);
        for (i, x) in self.entries.iter().enumerate() {
            if !x.0.is_undef() { self.index.push(x.2, i); }
        }
        self.grow_at = self.index.room();
    }

    /* whether the index holds every live entry where a small dict scans its entries */
    #[inline]
    fn indexed(&self) -> bool { self.entries.len() > SMALL_DICT }

    /* the live entry under hash `h` that `eq` accepts where long probes pay */
    #[inline]
    fn slot(&self, h: u64, heap: &HeapPool, eq: impl Fn(Val) -> bool) -> Option<usize> {
        let e = &self.entries;
        if !self.indexed() { return e.iter().position(|x| x.2 == h && !x.0.is_undef() && eq(x.0)); }
        self.index.find(h, |i| e[i].2 == h && eq(e[i].0), |n| heap.spend(n))
    }

    #[inline]
    fn find(&self, key: Val, heap: &HeapPool) -> Option<usize> {
        self.slot(hash_val_with_heap(key, heap), heap, |k| eq_member(k, key, heap))
    }

    pub fn get(&self, key: &Val, heap: &HeapPool) -> Option<&Val> {
        self.find(*key, heap).map(|i| &self.entries[i].1)
    }

    pub fn contains_key(&self, key: &Val, heap: &HeapPool) -> bool {
        self.find(*key, heap).is_some()
    }

    pub fn insert(&mut self, key: Val, value: Val, heap: &HeapPool) {
        let h = hash_val_with_heap(key, heap);
        let (i, e) = (self.entries.len(), &self.entries);
        if i <= SMALL_DICT {
            return match e.iter().position(|x| x.2 == h && !x.0.is_undef() && eq_member(x.0, key, heap)) {
                Some(j) => self.entries[j].1 = value,
                None => self.push_hashed(key, value, h),
            };
        }
        // a miss ends at the place the entry goes
        match self.index.place(h, |j| e[j].2 == h && eq_member(e[j].0, key, heap), |n| heap.spend(n)) {
            Ok(j) => self.entries[j].1 = value,
            Err(at) => {
                self.shape = NO_SHAPE;
                self.entries.push((key, value, h));
                self.live += 1;
                if i >= self.grow_at { self.reindex(); } else { self.index.put(at, i); }
            }
        }
    }

    pub fn remove(&mut self, key: &Val, heap: &HeapPool) -> Option<Val> {
        let h = hash_val_with_heap(*key, heap);
        let e = &self.entries;
        let i = if e.len() > SMALL_DICT {
            self.index.take(h, |j| e[j].2 == h && eq_member(e[j].0, *key, heap), |n| heap.spend(n))?
        } else {
            e.iter().position(|x| x.2 == h && !x.0.is_undef() && eq_member(x.0, *key, heap))?
        };
        Some(self.tombstone(i))
    }

    pub(crate) fn key_at(&self, i: usize) -> Val { self.entries.get(i).map_or(Val::undef(), |e| e.0) }
    pub(crate) fn hash_at(&self, i: usize) -> u64 { self.entries.get(i).map_or(0, |e| e.2) }
    /* entries ever stored with removed ones counted until a compaction drops them */
    pub(crate) fn entry_count(&self) -> usize { self.entries.len() }
    /* the entry holding the text key `name` under its hash `h` */
    pub(crate) fn position_str(&self, name: &str, h: u64, heap: &HeapPool) -> Option<usize> {
        self.slot(h, heap, |k| matches!(heap.try_get(k), Some(HeapObj::Str(s)) if s == name))
    }
    pub(crate) fn value_at(&self, i: usize) -> Val { self.entries[i].1 }
    pub(crate) fn set_value_at(&mut self, i: usize, v: Val) { self.entries[i].1 = v; }

    pub(crate) fn shape(&self) -> u32 { self.shape }
    pub(crate) fn set_shape(&mut self, shape: u32) { self.shape = shape; }

    /* appends the absent `key` under hash `h` with the keys then named by `shape` */
    pub(crate) fn push_shaped(&mut self, key: Val, value: Val, h: u64, shape: u32) {
        self.push_hashed(key, value, h);
        self.shape = shape;
    }

    /* appends `key` the caller found absent under hash `h` */
    pub(crate) fn push_hashed(&mut self, key: Val, value: Val, h: u64) {
        self.shape = NO_SHAPE;
        let i = self.entries.len();
        self.entries.push((key, value, h));
        self.live += 1;
        // growing past the scan size builds the index over everything stored
        if i == SMALL_DICT || (i > SMALL_DICT && i >= self.grow_at) { self.reindex(); }
        else if i > SMALL_DICT { self.index.push(h, i); }
    }

    /* marks entry `i` already out of the index as removed and gives its value */
    fn tombstone(&mut self, i: usize) -> Val {
        self.shape = NO_SHAPE;
        let val = self.entries[i].1;
        self.entries[i] = (Val::undef(), Val::undef(), 0);
        self.live -= 1;
        if self.entries.len() > 32 && self.entries.len() > 2 * self.live {
            self.entries.retain(|e| !e.0.is_undef());
            if roomy(&self.index, self.live) { self.index = Places::default(); self.entries.shrink_to(2 * self.live); }
            self.reindex();
        }
        val
    }

    pub fn len(&self) -> usize { self.live }
    pub fn is_empty(&self) -> bool { self.live == 0 }

    pub fn clear(&mut self) {
        self.shape = NO_SHAPE;
        // places hold entries only past the scan size so a short dict skips them
        if self.indexed() { self.index.clear(); }
        self.entries.clear();
        self.live = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = (Val, Val)> + '_ {
        self.entries.iter().filter(|e| !e.0.is_undef()).map(|&(k, v, _)| (k, v))
    }

    pub fn keys(&self) -> impl Iterator<Item = Val> + '_ {
        self.iter().map(|(k, _)| k)
    }

    /* the last live entry */
    pub fn last(&self) -> Option<(Val, Val)> {
        self.entries.iter().rev().find(|e| !e.0.is_undef()).map(|&(k, v, _)| (k, v))
    }

    /* takes out the last live entry cutting the removed ones behind it off the end */
    pub fn pop_last(&mut self, heap: &HeapPool) -> Option<(Val, Val)> {
        let k = self.entries.iter().rev().take_while(|e| e.0.is_undef()).count();
        self.entries.truncate(self.entries.len() - k);
        self.grow_at = self.grow_at.saturating_sub(k);
        let key = self.entries.last()?.0;
        let v = self.remove(&key, heap)?;
        // the entry just removed went last so it leaves the end too
        if self.entries.last().is_some_and(|e| e.0.is_undef()) { self.entries.pop(); self.grow_at = self.grow_at.saturating_sub(1); }
        Some((key, v))
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

/* visits each value `obj` holds once as the one walk the collector traces */
pub(crate) fn for_each_val(obj: &HeapObj, mut f: impl FnMut(Val)) {
    match obj {
        HeapObj::Tuple(items) => for &v in items { f(v); },
        HeapObj::BigRange(a, b, c) => { f(*a); f(*b); f(*c); }
        HeapObj::List(rc) => for &v in rc.borrow().iter() { f(v); },
        HeapObj::Dict(rc) => for (k, v) in rc.borrow().iter() { f(k); f(v); },
        HeapObj::Set(rc) => for &v in rc.borrow().iter() { f(v); },
        HeapObj::Bound { recv, func } => { f(*recv); f(*func); }
        HeapObj::Object { meta, slots } => {
            f(*meta);
            for (k, v) in slots.borrow().iter() { f(k); f(v); }
        }
        HeapObj::Coroutine(c) => {
            for &v in &c.slots { f(v); }
            for &v in &c.levels { f(v); }
            for fr in &c.iters { fr.for_each_val(&mut f); }
            for sf in &c.syncs { sf.for_each_val(&mut f); }
            f(c.pending);
        }
        HeapObj::Func { defaults, cells, .. } => {
            for &v in defaults.iter() { f(v); }
            for &(_, v) in cells.iter() { f(v); }
        }
        HeapObj::Module(_, attrs) => for (_, v) in attrs { f(*v); },
        // kinds holding no value give nothing to trace
        HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_)
        | HeapObj::Range(..) | HeapObj::Native(_) => {}
        HeapObj::Iter(frame, _) => frame.borrow().for_each_val(&mut f),
        HeapObj::Cell(v) => f(*v),
    }
}

/* removes `key` unless another slot owns its entry with one hash on the common path */
fn unintern<K: Eq + core::hash::Hash + Clone>(map: &mut HashMap<K, u32>, key: &K, idx: u32) {
    if let Some(owner) = map.remove(key) && owner != idx { map.insert(key.clone(), owner); }
}

/* the hash of callable `func` bound to `recv` */
#[inline(always)]
fn bound_hash(recv: u64, func: u64) -> u64 {
    use core::hash::{BuildHasher, Hasher};
    let mut h = crate::util::hash::FxBuildHasher::new().build_hasher();
    h.write_u64(recv);
    h.write_u64(func);
    h.finish()
}

/* the hash the intern table of short bytes probes with folded as text keys fold */
#[inline]
fn bytes_hash(b: &[u8]) -> u64 {
    use core::hash::Hasher;
    let mut h = crate::util::hash::FxHasher::default();
    h.write_u8(2);
    h.write(b);
    let h = h.finish();
    (h ^ h >> 32).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/* the `ascii` mark of a class whose instances give the ops nothing to run */
const PLAIN: u8 = 3;

/* past this many bytes a maker marks its text since a reader's scan costs more */
pub const SHORT_TEXT: usize = 64;

/* one slot of the arena holding an object and the marks kept beside it */
struct HeapSlot {
    obj: Option<HeapObj>,
    marked: bool,
    /* a text's ascii mark 1 for yes 2 for no and 0 while unread */
    ascii: u8,
    /* whether `strings` holds this slot as a name or constant or one character text */
    interned: bool,
    // what a type object stands for each fact once asked with its known bit
    facts: Cell<u8>,
    // a text's characters past ascii once counted or one past an instance's args tuple
    chars: u32,
}

impl HeapSlot {
    fn new(obj: Option<HeapObj>) -> Self { Self { obj, marked: false, ascii: 0, interned: false, facts: Cell::new(0), chars: 0 } }
}
// the count lies in the padding past the flags so a slot keeps its size
const _: () = assert!(core::mem::size_of::<HeapSlot>() <= core::mem::size_of::<Option<HeapObj>>() + 8);

// type object facts each asked once with the kind plus one in the low bits
const KIND_KNOWN: u8 = 0x80;
const EXC_KNOWN: u8 = 0x40;
const EXCEPTION: u8 = 0x20;

pub struct HeapPool {
    slots: Vec<HeapSlot>,
    // whether the program made a user class before which no value runs a special method
    pub(crate) users: bool,
    free_list: Vec<u32>,
    live: usize,
    // slots counted live for pacing that no object holds until the next sweep
    paced: usize,
    pub gc_threshold: usize,
    alloc_count: Cell<usize>,
    // what the allocator handed this program with garbage since the last sweep counted
    meter: &'static meter::Meter,
    // the scans and copies since the last step which the next step charges
    work: Cell<usize>,
    // whether slots or allocations or native work call for a collection or a charge
    due: Cell<bool>,
    // the op budget left at the last charge so natives refuse work they cannot pay
    budget: Cell<u64>,
    limit: usize,
    // the slots interning names constants and one character texts by their text
    strings: Interns,
    // the slots interning short bytes so equal ones share one value
    bytes_intern: Interns,
    // the type objects by name so every load of a name shares one
    pub(crate) types: Interns,
    // one past the slot of the error type a raise made and no load named
    unlisted: u32,
    // the error kind a raise made last and one past the slot of its type
    err_cache: (u32, u32),
    // the name bases and members of each class and type by its meta's place
    pub(crate) heads: Vec<object::Head>,
    // the slots every type object and instance with no fields yet share
    no_slots: Option<Rc<RefCell<DictMap>>>,
    // the arguments of instances and the fields of functions made on their first use
    pub(crate) objects: Option<Box<object::Tables>>,
    // the one value of the runtime's ellipsis kind once made so every use shares it
    pub(crate) dots: Option<Val>,
    // callables bound to a receiver interned so repeated reads share one slot
    bound: Interns,
    // bound natives the running sweep freed whose entries it drops at its end
    bound_freed: usize,
    /* the values the last mark visited which size the next collection */
    marked_vals: usize,
    alloc_limit: usize,
    /* the bytes past which a collection runs at four times what the last sweep kept */
    gc_bytes: usize,
    /* the mark's work list kept between marks so a collection allocates nothing under pressure */
    mark_worklist: Vec<u32>,
    /* the entries FOREIGN values name which are marked and swept with the slots */
    pub(crate) foreign: foreign::ForeignTable,
    // the spent frame drained walks share in place of their own
    spent: Option<Rc<RefCell<IterFrame>>>,
    // the slot plus one and the character and byte where a text's last walk ended
    text_at: Cell<[usize; 3]>,
    // what the runtime calls the engine's kinds which a type name reads and its natives
    pub(crate) names: &'static crate::lang::Names,
    pub(crate) natives: &'static crate::lang::Natives,
    // the runtime's rules outside the loop and the values it reads as false
    pub(crate) policy: &'static crate::lang::Policy,
    pub(crate) falsy: crate::lang::Falsy,
}

// a program gone leaves the shared instance total
impl Drop for HeapPool {
    fn drop(&mut self) { self.meter.retire(); }
}

impl HeapPool {
    /* `i` inline when it fits 48 bits and boxed by the wide int kind otherwise */
    #[inline]
    pub fn int(&mut self, i: i128) -> Result<Val, VmErr> {
        match i64::try_from(i).ok().and_then(Val::int_checked) { Some(v) => Ok(v), None => self.box_int(i) }
    }

    /* boxes an int past the inline range apart so `int` stays small enough to inline */
    #[inline(never)]
    fn box_int(&mut self, i: i128) -> Result<Val, VmErr> {
        match self.wide_box(i) {
            Some(v) => Ok(v),
            None => match self.foreign.wide_int().and_then(|k| k.from_i128) { Some(f) => f(self, i), None => Err(cold_int_range()) },
        }
    }

    /* counts an allocation toward the next collection */
    #[inline(always)]
    pub(crate) fn note_alloc(&mut self) {
        let n = self.alloc_count.get().saturating_add(1);
        self.alloc_count.set(n);
        // a count or a slot only grows here so it can only make `due` true
        if n >= self.alloc_limit && n >= self.live / 4 || self.live >= self.gc_threshold { self.fall_due(); }
    }

    /* counts a slot an op no longer builds so collections keep their pace */
    #[inline]
    pub(crate) fn pace(&mut self) {
        self.live += 1;
        self.paced += 1;
        self.note_alloc();
    }

    /* a collection falls due but at half the first threshold the table takes its slack */
    #[cold]
    #[inline(never)]
    fn fall_due(&mut self) {
        if self.gc_threshold != GC_FIRST / 2 || self.live < GC_FIRST / 2 { return self.due.set(true); }
        self.gc_threshold = GC_FIRST;
        // a table full at 256 takes its coming doubling and the slack in one growth
        if self.free_list.is_empty() && self.slots.len() == GC_FIRST / 2 && self.slots.capacity() == GC_FIRST / 2 { self.slots.reserve_exact(GC_FIRST / 2 + GC_SLACK); }
    }

    /* a table off the powers of two its next threshold outgrows goes back on them */
    #[cold]
    #[inline(never)]
    fn realign(&mut self) {
        let base = 1 << self.slots.capacity().ilog2();
        // a table its next threshold fits keeps its slack and the values in it
        if self.gc_threshold <= base { return; }
        if self.slots.get(base..).is_some_and(|top| top.iter().any(|s| s.obj.is_some())) { return self.slots.reserve_exact(2 * base - self.slots.len()); }
        self.free_list.retain(|&i| (i as usize) < base);
        self.slots.truncate(base);
        self.slots.shrink_to(base);
    }

    /* sets `due` from the slots and allocations and native work after any of them changes */
    #[inline]
    fn refresh(&self) {
        let alloc_limit = (self.live / 4).max(self.alloc_limit);
        self.due.set(self.live >= self.gc_threshold || self.alloc_count.get() >= alloc_limit || self.work.get() > WORK_SLICE);
    }

    pub fn new(limit: usize) -> Self {
        Self {
            slots: Vec::new(),
            users: false,
            free_list: Vec::new(),
            live: 0,
            paced: 0,
            gc_threshold: GC_FIRST / 2,
            alloc_count: Cell::new(0),
            due: Cell::new(false),
            meter: crate::util::leak(Box::new(meter::Meter::new(limit))),
            work: Cell::new(0),
            budget: Cell::new(u64::MAX),
            limit,
            strings: Interns::default(),
            bytes_intern: Interns::default(),
            types: Interns::default(),
            unlisted: 0,
            err_cache: (0, 0),
            heads: Vec::new(),
            no_slots: None,
            objects: None,
            dots: None,
            bound: Interns::default(),
            bound_freed: 0,
            marked_vals: 0,
            alloc_limit: 4096,
            gc_bytes: GC_BYTES_FLOOR,
            mark_worklist: Vec::with_capacity(64),
            foreign: foreign::ForeignTable::default(),
            spent: None,
            text_at: Cell::new([0; 3]),
            names: &crate::lang::Names::PLAIN,
            natives: &crate::lang::Natives::EMPTY,
            policy: &crate::lang::Policy::ENGINE,
            falsy: crate::lang::Policy::ENGINE.falsy,
        }
    }

    /* the slot already sharing `obj` where longer texts share only through `intern_str` */
    #[inline]
    fn intern_lookup(&self, obj: &HeapObj) -> Option<u32> {
        match obj {
            // every empty and one character text shares one slot
            HeapObj::Str(s) if s.len() <= 1 => self.string_slot(s),
            HeapObj::Bytes(b) if b.len() <= 128 => {
                let slots = &self.slots;
                self.bytes_intern.find(bytes_hash(b), |i| matches!(&slots[i as usize].obj, Some(HeapObj::Bytes(t)) if t == b), |n| self.spend(n))
            }
            &HeapObj::Bound { recv: r, func: f } => {
                let slots = &self.slots;
                self.bound.find(bound_hash(r.0, f.0), |i| matches!(slots[i as usize].obj, Some(HeapObj::Bound { recv, func }) if recv.0 == r.0 && func.0 == f.0), |n| self.spend(n))
            }
            // a native method read off its type shares one value and a free one none
            &HeapObj::Native(id) if self.natives.owner(id).is_some() => self.tables()?.natives.get(&id.0).copied(),
            _ => None,
        }
    }

    /* the slot interning text `s` where a long probe run pays the budget */
    #[inline]
    pub(crate) fn string_slot(&self, s: &str) -> Option<u32> {
        let slots = &self.slots;
        self.strings.find(eq::hash_key(s), |i| matches!(&slots[i as usize].obj, Some(HeapObj::Str(t)) if t == s), |n| self.spend(n))
    }

    /* enters `idx` in the intern and singleton tables its object belongs to */
    fn intern_insert(&mut self, idx: u32) {
        match self.slots[idx as usize].obj.as_ref() {
            Some(HeapObj::Str(s)) if s.len() <= 1 => self.intern_text(idx, eq::hash_key(s)),
            Some(HeapObj::Bytes(b)) if b.len() <= 128 => { let h = bytes_hash(b); self.enter(|p| &mut p.bytes_intern, h, idx); }
            // one entry per bound value so no lookup checks for a twin
            Some(&HeapObj::Bound { recv: r, func: f }) => self.enter(|p| &mut p.bound, bound_hash(r.0, f.0), idx),
            Some(&HeapObj::Native(id)) if self.natives.owner(id).is_some() => { self.tables_mut().natives.insert(id.0, idx); }
            _ => {}
        }
    }

    /* drops `idx` from the tables `intern_insert` filled which long texts and bytes never enter */
    #[inline(always)]
    fn intern_remove(&mut self, idx: u32) {
        match self.slots[idx as usize].obj.as_ref() {
            Some(HeapObj::Str(s)) if self.slots[idx as usize].interned => { self.strings.remove(eq::hash_key(s), idx); }
            Some(HeapObj::Bytes(b)) if b.len() <= 128 => { self.bytes_intern.remove(bytes_hash(b), idx); }
            Some(&HeapObj::Object { meta, .. }) if !meta.is_heap() => self.drop_head(meta, idx),
            Some(HeapObj::Func { .. }) if self.slots[idx as usize].chars != 0 => self.drop_fn_fields(idx),
            // the sweep drops these entries together once it ends
            Some(HeapObj::Bound { .. }) => self.bound_freed += 1,
            Some(&HeapObj::Native(id)) => if let Some(t) = self.objects.as_deref_mut() { unintern(&mut t.natives, &id.0, idx) },
            _ => {}
        }
    }

    /* takes a free slot or grows the arena */
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

    /* the one value of name or constant `s` with texts past 128 bytes never shared */
    pub fn intern_str(&mut self, s: &str) -> Result<Val, VmErr> {
        if s.len() > 128 { return self.alloc(HeapObj::Str(s.into())); }
        if let Some(i) = self.string_slot(s) { return Ok(Val::heap(i)); }
        let v = self.alloc(HeapObj::Str(s.into()))?;
        // a one character text entered the table as it was made
        if !self.slots[v.as_heap() as usize].interned { self.intern_text(v.as_heap(), eq::hash_key(s)); }
        Ok(v)
    }

    /* enters slot `idx` holding a text under hash `h` in the intern table */
    fn intern_text(&mut self, idx: u32, h: u64) {
        self.slots[idx as usize].interned = true;
        self.enter(|p| &mut p.strings, h, idx);
    }

    /* puts slot `idx` under hash `h` in the intern table `table` picks charging long probes */
    #[inline]
    pub(crate) fn enter(&mut self, table: impl FnOnce(&mut Self) -> &mut Interns, h: u64, idx: u32) {
        let mut owed = 0;
        table(self).insert(h, idx, |n| owed = n);
        if owed != 0 { self.spend(owed); }
    }

    /* `alloc` past the soft limit for the error that reports the limit itself */
    pub fn alloc_emergency(&mut self, obj: HeapObj) -> Result<Val, VmErr> { self.admit(obj, false) }

    /* a shared twin or new slot within the cap and twice the limit for `soft` */
    #[inline(always)]
    fn admit(&mut self, obj: HeapObj, soft: bool) -> Result<Val, VmErr> {
        if let Some(idx) = self.intern_lookup(&obj) { return Ok(Val::heap(idx)); }
        if soft && self.meter.held() > self.limit.saturating_mul(2) { return Err(cold_heap()); }
        if self.slots.len() >= (1 << 28) { return Err(VmErr::Heap); }
        let idx = self.place(obj);
        self.intern_insert(idx);
        self.live += 1;
        self.note_alloc();
        Ok(Val::heap(idx))
    }

    /* what a result may still take as the limit or what twice the limit leaves */
    pub fn room(&self) -> usize {
        self.limit.min(self.limit.saturating_mul(2).saturating_sub(self.meter.held())).min(self.meter.instance_room())
    }

    /* refuses a result of `extra` bytes past the room left before it is built */
    #[inline]
    pub fn reserve(&self, extra: usize) -> Result<(), VmErr> {
        if extra > self.room() { Err(cold_heap()) } else { Ok(()) }
    }

    /* whether the program holds more than its limit or took the instance past its budget */
    #[inline]
    pub fn over(&self) -> bool { self.meter.over() }

    /* what the allocator holds for the program with garbage since the last sweep */
    pub fn bytes(&self) -> usize { self.meter.held() }

    /* the most the allocator held for the program at once */
    pub fn peak(&self) -> usize { self.meter.peak() }

    /* the meter this program charges which its runs make the payer */
    pub fn meter(&self) -> &'static meter::Meter { self.meter }

    /* whether a memory limit is set that no metering allocator counts */
    pub(crate) fn unmetered(&self) -> bool { self.limit != usize::MAX && !meter::installed() }

    pub fn mark(&mut self, v: Val) {
        // a root holding no object or one already marked costs only the look
        if !v.is_foreign() && (!v.is_heap() || self.slots.get(v.as_heap() as usize).is_none_or(|s| s.marked)) { return; }
        let mut foreign = Vec::new();
        if v.is_foreign() { foreign.push(v); }
        if v.is_heap() { self.mark_worklist.push(v.as_heap()); }
        // slots and FOREIGN entries reach each other so both sides drain until neither grows
        loop {
            // the pool splits so the closure fills the work list while the slots are read
            let HeapPool { slots, mark_worklist, marked_vals, objects, heads, .. } = self;
            while let Some(idx) = mark_worklist.pop() {
                let idx = idx as usize;
                if slots[idx].marked { continue; }
                slots[idx].marked = true;
                *marked_vals += 1;
                let mut visit = |val: Val| { if val.is_heap() { mark_worklist.push(val.as_heap()); } else if val.is_foreign() { foreign.push(val); } };
                if let Some(obj) = &slots[idx].obj {
                    for_each_val(obj, |val| { *marked_vals += 1; visit(val); });
                    // a class keeps its bases in its head and an instance its args tuple
                    if let HeapObj::Object { meta, .. } = obj {
                        if meta.is_class_meta() && let Some(h) = heads.get((meta.0 >> 8 & 0xFFFF_FFFF) as usize) { for &b in h.bases.iter().chain(h.members.iter().map(|(_, v)| v)) { visit(b); } }
                        if slots[idx].chars != 0 && meta.is_heap() { visit(Val::heap(slots[idx].chars - 1)); }
                    }
                    // a function keeps the fields a program gave it in the side table
                    if let HeapObj::Func { .. } = obj && slots[idx].chars != 0 && let Some(f) = objects.as_deref().and_then(|t| t.fn_fields.get(slots[idx].chars as usize - 1)) { for &(_, b) in f.iter() { visit(b); } }
                }
            }
            let Some(f) = foreign.pop() else { break };
            let mut kept = Vec::new();
            self.mark_foreign(f, &mut kept);
            for c in kept { if c.is_heap() { self.mark_worklist.push(c.as_heap()); } else if c.is_foreign() { foreign.push(c); } }
        }
    }

    pub fn sweep(&mut self) {
        let first = self.free_list.capacity() == 0;
        for idx in 0..self.slots.len() {
            let slot = &mut self.slots[idx];
            if slot.obj.is_none() { continue; }
            if slot.marked { slot.marked = false; continue; }
            self.intern_remove(idx as u32);
            self.slots[idx].obj = None;
            self.free_list.push(idx as u32);
            self.live -= 1;
        }

        self.live -= core::mem::take(&mut self.paced);
        if self.bound_freed != 0 { self.forget_bound(); }
        self.live -= self.foreign.sweep();
        // a first collection just past a power of two keeps no doubled free list
        if first && self.free_list.capacity() > self.free_list.len() * 3 / 2 { self.free_list.shrink_to_fit(); }
        // both triggers scale with the volume the last mark walked
        self.gc_threshold = (self.live * 2).max(GC_SLOTS_FLOOR).max(self.marked_vals / 4);
        let cap = self.slots.capacity();
        if cap > 4 && !cap.is_power_of_two() { self.realign(); }
        self.alloc_limit = (self.marked_vals / 4).max(4096);
        self.gc_bytes = self.meter.held().saturating_mul(4).max(GC_BYTES_FLOOR);
        self.marked_vals = 0;
        self.alloc_count.set(0);
        self.refresh();
        // a freed slot may hold another text so no walk goes on from it
        self.text_at.set([0; 3]);
    }

    /* drops the entries of the bound natives a sweep freed with one pass or none */
    fn forget_bound(&mut self) {
        // each live bound native holds one entry so freeing as many frees them all
        if self.bound_freed == self.bound.len() { self.bound.clear(); } else {
            let slots = &self.slots;
            self.bound.retain(|i| slots[i as usize].obj.is_some());
        }
        self.bound_freed = 0;
    }

    /* the free slots in the order the next allocations take them from the end */
    pub(crate) fn free_order(&self) -> &[u32] { &self.free_list }

    /* what times the next collection by slots allocations bytes and paced slots */
    pub(crate) fn schedule(&self) -> [usize; 5] { [self.gc_threshold, self.alloc_limit, self.alloc_count.get(), self.gc_bytes, self.paced] }

    /* times the next collection as `schedule` gave it within the floors a sweep keeps */
    pub(crate) fn set_schedule(&mut self, [threshold, limit, count, bytes, paced]: [usize; 5]) {
        (self.gc_threshold, self.alloc_limit, self.gc_bytes) = (threshold.max(GC_FIRST / 2), limit.max(4096), bytes.max(GC_BYTES_FLOOR));
        // paced slots count live again as the run had them
        self.paced = paced.min(1 << 28);
        self.live += self.paced;
        self.alloc_count.set(count);
        self.refresh();
    }

    /* restores a saved free order unless it repeats a slot or names a live one */
    pub(crate) fn restore_free(&mut self, order: Vec<u32>) -> bool {
        let mut seen = alloc::vec![false; self.slots.len()];
        for &i in &order {
            match self.slots.get(i as usize) {
                Some(s) if s.obj.is_none() && !core::mem::replace(&mut seen[i as usize], true) => {}
                _ => return false,
            }
        }
        self.free_list = order;
        true
    }

    /* one entry for each slot and none for a free one */
    pub(crate) fn snapshot_objs(&self) -> impl Iterator<Item = Option<&HeapObj>> {
        self.slots.iter().map(|s| s.obj.as_ref())
    }

    /* replaces the pool and rebuilds the free list and intern tables */
    pub(crate) fn restore_objs(&mut self, objs: Vec<Option<HeapObj>>) {
        self.slots = objs.into_iter().map(HeapSlot::new).collect();
        self.users = self.slots.iter().any(|s| matches!(s.obj, Some(HeapObj::Object { meta, .. }) if meta.is_class_meta()));
        self.free_list.clear();
        self.strings.clear();
        self.bytes_intern.clear();
        self.types.clear();
        (self.unlisted, self.err_cache) = (0, (0, 0));
        if let Some(t) = self.objects.as_deref_mut() { t.fn_fields.clear(); t.free_fn_fields.clear(); t.natives.clear(); }
        self.bound.clear();
        self.bound_freed = 0;
        self.text_at.set([0; 3]);
        (self.live, self.paced) = (0, 0);
        for idx in 0..self.slots.len() {
            if self.slots[idx].obj.is_none() { self.free_list.push(idx as u32); continue; }
            self.live += 1;
            self.intern_insert(idx as u32);
            // every type object a blob holds is found by its name again
            if self.is_type(Val::heap(idx as u32)) { self.list_type(idx as u32); }
        }
        // foreign values of inline stores count live as the run counted them
        self.live += self.foreign.recount();
        self.gc_threshold = (self.live * 2).max(GC_SLOTS_FLOOR);
        self.alloc_limit = 4096;
        self.gc_bytes = self.meter.held().saturating_mul(4).max(GC_BYTES_FLOOR);
        self.marked_vals = 0;
        self.alloc_count.set(0);
        self.refresh();
    }

    /* whether the next step leaves the fast path to collect or to charge native work */
    pub fn needs_gc(&self) -> bool {
        self.due.get() || self.meter.held() > self.gc_bytes || self.over()
    }

    /* counts `n` units of native work which the next step charges to the op budget */
    #[inline]
    pub fn spend(&self, n: usize) {
        let w = self.work.get().saturating_add(n);
        self.work.set(w);
        if w > WORK_SLICE { self.due.set(true); }
    }

    /* records the op budget left which `afford` holds native work to */
    #[inline]
    pub fn set_budget(&self, ops: u64) { self.budget.set(ops); }

    /* refuses native work past the op budget before it starts */
    pub fn afford(&self, work: usize) -> Result<(), VmErr> { if work as u64 > self.budget.get() { Err(cold_budget()) } else { Ok(()) } }

    /* refuses more of a long native walk once its counted work passes the budget */
    #[inline]
    pub fn within_budget(&self) -> Result<(), VmErr> { if self.due.get() && self.work.get() as u64 > self.budget.get() { Err(cold_budget()) } else { Ok(()) } }

    /* whether native work waits to be charged */
    #[inline]
    pub fn has_work(&self) -> bool { self.work.get() != 0 }

    /* the native work counted since the last step which then starts again from zero */
    #[inline]
    pub fn take_work(&self) -> usize {
        let w = self.work.get();
        if w != 0 { self.work.set(0); self.refresh(); }
        w
    }

    /* character `i` of ascii text `v` shared or none for the slow path to answer */
    #[inline]
    pub fn str_char(&mut self, v: Val, i: i64) -> Option<Val> {
        if !self.str_is_ascii(v) { return None; }
        let Some(HeapObj::Str(s)) = self.try_get(v) else { return None };
        let n = s.len() as i64;
        let j = if i < 0 && self.policy.neg_index { i + n } else { i };
        if !(0..n).contains(&j) { return None; }
        let c = s.as_bytes()[j as usize];
        self.string_slot(crate::r#unsafe::whole(&[c])).map(Val::heap)
    }

    /* whether `v` is an unshared text made at run time that may grow in place */
    #[inline]
    pub fn growable_str(&self, v: Val) -> bool {
        v.is_heap() && self.slots.get(v.as_heap() as usize).is_some_and(|s| !s.interned && matches!(&s.obj, Some(HeapObj::Str(t)) if t.len() > 1))
    }

    /* appends `t` to the text `s` its owner grows in place or gives false */
    pub fn append_str(&mut self, s: Val, t: Val) -> bool {
        if !self.growable_str(s) || !t.is_heap() || s.0 == t.0 { return false; }
        let (si, ti) = (s.as_heap() as usize, t.as_heap() as usize);
        let (lo, hi) = self.slots.split_at_mut(si.max(ti));
        let (dst, src) = if si < ti { (&mut lo[si], &hi[0]) } else { (&mut hi[0], &lo[ti]) };
        let (Some(HeapObj::Str(d)), Some(HeapObj::Str(x))) = (&mut dst.obj, &src.obj) else { return false };
        let work = if x.len() > 64 { x.len() } else { 0 };
        // the ascii mark holds only while both halves are ascii and a kept count goes
        match dst.ascii { 1 if !x.is_ascii() => dst.ascii = 0, 2 => dst.chars = 0, _ => {} }
        d.push_str(x);
        if work != 0 { self.spend(work); }
        true
    }

    /* whether two plain texts hold the same text and none for any other pair */
    #[inline]
    pub fn str_eq(&self, a: Val, b: Val) -> Option<bool> {
        match (self.try_get(a)?, self.try_get(b)?) {
            (HeapObj::Str(x), HeapObj::Str(y)) => Some(a.0 == b.0 || eq::eq_text(x.as_bytes(), y.as_bytes(), self)),
            _ => None,
        }
    }

    /* whether `v` is an all ascii text scanned once per slot */
    #[inline]
    pub fn str_is_ascii(&mut self, v: Val) -> bool {
        if !v.is_heap() { return false; }
        let slot = &mut self.slots[v.as_heap() as usize];
        if slot.ascii == 0 {
            slot.ascii = match &slot.obj { Some(HeapObj::Str(s)) if s.is_ascii() => 1, _ => 2 };
        }
        slot.ascii == 1
    }

    /* a long text holding text `of` `n` times counted off the marks of `of` */
    #[inline(never)]
    pub fn alloc_times(&mut self, s: String, of: Val, n: usize) -> Result<Val, VmErr> {
        let chars = self.known_chars(of).and_then(|c| c.checked_mul(n));
        self.alloc_counted(s, chars)
    }

    /* text `s` its maker knows holds ascii alone so no reader scans it */
    #[inline]
    pub fn alloc_ascii(&mut self, s: String) -> Result<Val, VmErr> {
        let n = s.len();
        self.alloc_counted(s, Some(n))
    }

    /* text `s` whose maker counted its characters so no reader scans it */
    #[inline(always)]
    pub fn alloc_counted(&mut self, s: String, chars: Option<usize>) -> Result<Val, VmErr> {
        debug_assert!(chars.is_none_or(|n| n == s.chars().count()), "a text made with a wrong count");
        // a known count marks the slot the allocation gave
        let (mark, count) = match chars { Some(n) if n == s.len() => (1, 0), Some(n) => (2, u32::try_from(n).unwrap_or(0)), None => (0, 0) };
        let v = self.alloc(HeapObj::Str(s))?;
        if mark != 0 && let Some(slot) = self.slots.get_mut(v.as_heap() as usize) { (slot.ascii, slot.chars) = (mark, count); }
        Ok(v)
    }

    /* the characters of text `v` its marks tell and none for an unread one */
    #[inline(always)]
    pub fn marked_chars(&self, v: Val) -> Option<usize> {
        let slot = self.slots.get(v.as_heap() as usize).filter(|_| v.is_heap())?;
        let Some(HeapObj::Str(s)) = &slot.obj else { return None };
        match slot.ascii { 1 => Some(s.len()), 2 if slot.chars != 0 => Some(slot.chars as usize), _ => None }
    }

    /* the characters of text `v` its marks tell with a short unread one read now */
    #[inline(always)]
    pub fn known_chars(&mut self, v: Val) -> Option<usize> {
        if !v.is_heap() { return None; }
        let slot = self.slots.get_mut(v.as_heap() as usize)?;
        let Some(HeapObj::Str(s)) = &slot.obj else { return None };
        match slot.ascii {
            1 => Some(s.len()),
            2 if slot.chars != 0 => Some(slot.chars as usize),
            0 if s.len() <= SHORT_TEXT && s.is_ascii() => { slot.ascii = 1; Some(s.len()) }
            0 if s.len() <= SHORT_TEXT => Some(self.count_text(v)),
            _ => None,
        }
    }

    /* the characters of text `v` read off its marks and counted once when unread */
    #[inline(always)]
    pub fn text_len(&mut self, v: Val) -> Option<usize> {
        if !v.is_heap() { return None; }
        let slot = self.slots.get_mut(v.as_heap() as usize)?;
        let Some(HeapObj::Str(s)) = &slot.obj else { return None };
        Some(match slot.ascii {
            1 => s.len(),
            2 if slot.chars != 0 => slot.chars as usize,
            0 if s.len() <= SHORT_TEXT && s.is_ascii() => { slot.ascii = 1; s.len() }
            _ => self.count_text(v),
        })
    }

    /* character `i` of text `v` past ascii counting a negative `i` from the end */
    pub(crate) fn char_of(&self, v: Val, s: &str, i: i64) -> Option<char> {
        let n = self.marked_chars(v);
        let i = match (usize::try_from(i), n) {
            (Ok(i), _) => i,
            _ if !self.policy.neg_index => return None,
            (_, Some(n)) => n.checked_sub(usize::try_from(i.unsigned_abs()).ok()?)?,
            // with no count kept the end is the one place known
            (_, None) => {
                let at = chars_back(s, usize::try_from(i.unsigned_abs()).ok()?);
                let walked = s.len() - at.unwrap_or(0);
                if walked > 1024 { self.spend(walked / 16); }
                return s[at?..].chars().next();
            }
        };
        s.get(self.char_start(v, s, i, n)?..)?.chars().next()
    }

    /* the byte character `i` of text `v` starts at walked from the nearest place known */
    pub(crate) fn char_start(&self, v: Val, s: &str, i: usize, n: Option<usize>) -> Option<usize> {
        let [slot, ci, cb] = self.text_at.get();
        let mut from = (0, 0);
        // where the last walk of this text ended or its end when either is nearer
        if slot == v.as_heap() as usize + 1 && ci.abs_diff(i) < i && s.is_char_boundary(cb) { from = (ci, cb); }
        if let Some(n) = n && n.abs_diff(i) < from.0.abs_diff(i) { from = (n, s.len()); }
        let (c, b) = from;
        let at = if i >= c { chars_on(s.get(b..)?, i - c).map(|k| b + k) } else { chars_back(s.get(..b)?, c - i) };
        let walked = at.map_or(if i >= c { s.len() - b } else { b }, |a| a.abs_diff(b));
        if walked > 1024 { self.spend(walked / 16); }
        let at = at?;
        self.text_at.set([v.as_heap() as usize + 1, i, at]);
        Some(at)
    }

    /* counts a text's characters once and keeps them charging a long count past ascii */
    #[cold]
    #[inline(never)]
    fn count_text(&mut self, v: Val) -> usize {
        let slot = &mut self.slots[v.as_heap() as usize];
        let Some(HeapObj::Str(s)) = &slot.obj else { return 0 };
        let ascii = slot.ascii == 1 || slot.ascii == 0 && s.is_ascii();
        let (n, len) = (if ascii { s.len() } else { s.chars().count() }, s.len());
        slot.ascii = if ascii { 1 } else { 2 };
        if ascii { return n; }
        // a count past what the slot holds is made again at each call
        slot.chars = u32::try_from(n).unwrap_or(0);
        if len > 1024 { self.spend(len / 16); }
        n
    }

    /* a builtin container's length or a text's in characters off the marks its slot keeps */
    #[inline(always)]
    pub(crate) fn plain_len(&mut self, v: Val) -> Option<usize> {
        if !v.is_heap() { return None; }
        let slot = self.slots.get_mut(v.as_heap() as usize)?;
        Some(match slot.obj.as_ref()? {
            HeapObj::List(l) => l.borrow().len(),
            HeapObj::Tuple(t) => t.len(),
            HeapObj::Dict(d) => d.borrow().len(),
            HeapObj::Set(s) => s.borrow().len(),
            HeapObj::Str(s) if slot.ascii == 1 => s.len(),
            HeapObj::Str(_) if slot.ascii == 2 && slot.chars != 0 => slot.chars as usize,
            // a short unread text is scanned in place and marked
            HeapObj::Str(s) if slot.ascii == 0 && s.len() <= SHORT_TEXT && s.is_ascii() => { slot.ascii = 1; s.len() }
            HeapObj::Str(_) => self.count_text(v),
            _ => return None,
        })
    }

    pub fn usage(&self) -> usize { self.live }

    /* the memory limit of the run in bytes */
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

    /* the octets of a bytes or bytearray value and none for any other */
    #[inline] pub fn bytes_of(&self, v: Val) -> Option<&[u8]> {
        match self.try_get(v)? {
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => Some(b),
            _ => None,
        }
    }

    /* the entries of a dict and none for any other value */
    pub fn dict_cell(&self, v: Val) -> Option<&Rc<RefCell<DictMap>>> {
        match self.try_get(v)? { HeapObj::Dict(d) => Some(d), _ => None }
    }

    /* the object of `v` or none when it names no live heap object */
    #[inline] pub fn try_get(&self, v: Val) -> Option<&HeapObj> {
        if !v.is_heap() { return None; }
        self.slots.get(v.as_heap() as usize).and_then(|s| s.obj.as_ref())
    }
    /* fact `known` of type object `v` read once per slot and zero for other values */
    #[inline]
    fn type_fact(&self, v: Val, known: u8, read: impl Fn(&crate::lang::Names, &str) -> u8) -> u8 {
        let Some(slot) = v.is_heap().then(|| self.slots.get(v.as_heap() as usize)).flatten() else { return 0 };
        let Some(HeapObj::Object { meta, .. }) = &slot.obj else { return 0 };
        if !meta.is_type_meta() { return 0; }
        let facts = slot.facts.get();
        if facts & known != 0 { return facts; }
        let Some(name) = self.class_name(v) else { return 0 };
        let facts = facts | known | read(self.names, name);
        slot.facts.set(facts);
        facts
    }

    /* the engine kind whose values type object `v` makes and none past those with methods */
    #[inline]
    pub fn type_kind(&self, v: Val) -> Option<crate::lang::KindId> {
        match self.type_fact(v, KIND_KNOWN, |n, t| n.kind_named(t).map_or(0, |k| k as u8 + 1)) & 0x1F { 0 => None, k => crate::lang::KindId::from_u8(k - 1) }
    }

    /* whether `v` is a type object of a class the runtime raises */
    #[inline]
    pub fn is_exc_type(&self, v: Val) -> bool { self.type_fact(v, EXC_KNOWN, |n, t| if n.is_exception(t) { EXCEPTION } else { 0 }) & EXCEPTION != 0 }

    /* marks a class plain in its `ascii` mark and tells whether a class is plain */
    pub(crate) fn set_plain(&mut self, v: Val) { if v.is_heap() && let Some(s) = self.slots.get_mut(v.as_heap() as usize) { s.ascii = PLAIN; } }
    pub(crate) fn is_plain(&self, v: Val) -> bool { v.is_heap() && self.slots.get(v.as_heap() as usize).is_some_and(|s| s.ascii == PLAIN && matches!(s.obj, Some(HeapObj::Object { meta, .. }) if meta.is_class_meta())) }
    #[inline] pub fn try_get_mut(&mut self, v: Val) -> Option<&mut HeapObj> {
        if !v.is_heap() { return None; }
        self.slots.get_mut(v.as_heap() as usize).and_then(|s| s.obj.as_mut())
    }

    /* points iterator `it` that ran out at the shared spent frame freeing its own */
    pub(crate) fn spend_iter(&mut self, it: Val) {
        let spent = self.spent.get_or_insert_with(|| Rc::new(RefCell::new(IterFrame::Range { cur: 0, end: 0, step: 1 }))).clone();
        if let Some(HeapObj::Iter(cell, _)) = self.try_get_mut(it) { *cell = spent; }
    }

    /* whether `child` is `ancestor` or has it among its bases compared by identity */
    pub fn is_subclass(&self, child: Val, ancestor: Val) -> bool {
        // a chain of single bases walks in place and one longer than the heap repeats
        let (mut c, mut steps) = (child, 0);
        loop {
            if c.0 == ancestor.0 { return true; }
            let Some((meta, _)) = self.object(c) else { return false };
            match self.bases_of(meta) {
                &[b] if steps <= self.slots.len() => (c, steps) = (b, steps + 1),
                &[] | &[_] => return false,
                many => return self.subclass_of_many(many, ancestor),
            }
        }
    }

    /* `is_subclass` past a class of several bases meeting each class once */
    #[cold]
    #[inline(never)]
    fn subclass_of_many(&self, bases: &[Val], ancestor: Val) -> bool {
        let (mut todo, mut seen) = (bases.to_vec(), crate::util::hash::FxHashSet::default());
        while let Some(c) = todo.pop() {
            if c.0 == ancestor.0 { return true; }
            if !seen.insert(c.0) { continue; }
            if let Some((meta, _)) = self.object(c) { todo.extend_from_slice(self.bases_of(meta)); }
        }
        false
    }
}

/* the byte `k` characters into `s` or its end once it holds exactly `k` */
#[inline]
fn chars_on(s: &str, k: usize) -> Option<usize> {
    let mut it = s.chars();
    if k != 0 { it.nth(k - 1)?; }
    Some(s.len() - it.as_str().len())
}

/* the byte the character `k` places back from the end of `s` starts at */
#[inline]
fn chars_back(s: &str, k: usize) -> Option<usize> {
    let mut it = s.chars();
    it.nth_back(k.checked_sub(1)?)?;
    Some(it.as_str().len())
}

/* an int or wide int as i128 and a bool where bools are ints */
#[inline]
pub fn as_i128(v: Val, heap: &HeapPool) -> Option<i128> {
    if v.is_int() { Some(v.as_int() as i128) }
    else if v.is_bool() { heap.policy.bool_is_int.then_some(v.as_bool() as i128) }
    else if v.is_foreign() { heap.foreign_i128(v) }
    else { None }
}
