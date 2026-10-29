use alloc::{rc::Rc, string::String, vec::Vec};
use core::any::Any;
use core::cmp::Ordering;
use super::{HeapPool, IterFrame, Val, VmErr};

/* the binary operators a kind may answer where a `None` declines the operation */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp { Add, Sub, Mul, FloorDiv, Mod, TrueDiv, Pow, And, Or, Xor, Shl, Shr, MatMul }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp { Neg, Pos, Abs, Invert }

/* a snapshot carries a live entry of the kind or fails naming it */
pub enum Snapshot {
    // inline stores ride as they are and a shared payload through the codec
    Serialize { save: fn(&dyn Any, &mut Vec<u8>), load: fn(&[u8]) -> Option<Rc<dyn Any>> },
    // every value of the kind keeps an inline store which rides as it is
    Store,
    Reject,
}

/* a value crosses the plugin wire as its kind's `to_i128` int or as a handle */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transfer { Copy, Deny }

/* what a value keeps in its entry so boxing it takes no allocation */
pub enum Store {
    // plain words the kind reads as its own
    Words([u64; 3]),
    // three values the collector traces
    Vals([Val; 3]),
    // a payload of the kind's own type behind one allocation
    Shared(Rc<dyn Any>),
}

type CmpFn = fn(&HeapPool, &dyn Any, Val) -> Option<Ordering>;
type BinFn = fn(&mut HeapPool, BinOp, Val, Val) -> Result<Option<Val>, VmErr>;
type UnFn = fn(&mut HeapPool, UnOp, Val) -> Result<Option<Val>, VmErr>;
type F64Fn = fn(&dyn Any) -> Result<f64, VmErr>;
type BoxFn = fn(&mut HeapPool, i128) -> Result<Val, VmErr>;
type DigitsFn = fn(&mut HeapPool, bool, &[u8], u32) -> Result<Val, VmErr>;
type TraceFn = fn(&dyn Any, &mut dyn FnMut(Val));
type TextFn = fn(&dyn Any) -> Result<String, VmErr>;
/* attribute `name` of foreign value `v` and None where its kind gives none */
pub type GetFn = fn(&mut HeapPool, Val, &str) -> Result<Option<Val>, VmErr>;
/* how many items a value holds */
pub type LenFn = fn(&mut HeapPool, Val) -> Result<i128, VmErr>;
/* the item of a value at an index that is no slice */
pub type ItemFn = fn(&mut HeapPool, Val, Val) -> Result<Val, VmErr>;
/* the cut of a value by its start stop and step */
pub type CutFn = fn(&mut HeapPool, Val, [Val; 3]) -> Result<Val, VmErr>;
/* the frame a loop over a value steps */
pub type IterFn = fn(&HeapPool, Val) -> Option<IterFrame>;
/* what a call of a value with its positionals gives where None declines */
pub type CallFn = fn(&mut HeapPool, Val, &[Val]) -> Result<Option<Val>, VmErr>;

/* a kind registered once by name with `None` for each operation it does not answer */
pub struct Kind {
    pub name: &'static str,
    pub version: u32,
    // what `type()` and error messages call a value of the kind
    pub type_name: &'static str,
    // values of different families never compare equal so their hashes need not agree
    pub family: &'static str,
    // an immutable value may be copied or shared freely as its identity is never observed
    pub immutable: bool,
    pub repr: TextFn,
    // the text its values show as text where it differs from `repr`
    pub str: Option<TextFn>,
    pub truthy: fn(&HeapPool, &dyn Any) -> bool,
    // the table hash which agrees for values of the family that compare equal
    pub hash: fn(&HeapPool, &dyn Any) -> u64,
    // equality with `other` never called across families and never running user code
    pub eq: fn(&HeapPool, &dyn Any, Val) -> bool,
    pub cmp: Option<CmpFn>,
    pub binop: Option<BinFn>,
    pub unop: Option<UnFn>,
    pub to_f64: Option<F64Fn>,
    pub to_i128: Option<fn(&dyn Any) -> Option<i128>>,
    // boxes an i128 past the inline range and only the wide int kind sets it
    pub from_i128: Option<BoxFn>,
    // the int that digits in a base spell which only the wide int kind sets
    pub from_digits: Option<DigitsFn>,
    // the engine values a shared payload keeps alive for the kinds that hold any
    pub trace: Option<TraceFn>,
    // what an attribute load of its values reads where its methods hold no such name
    pub get: Option<GetFn>,
    // the length a sequence reports and the item and cut an index reads
    pub len: Option<LenFn>,
    pub item: Option<ItemFn>,
    pub cut: Option<CutFn>,
    // the frame a loop over its values steps
    pub iter: Option<IterFn>,
    pub call: Option<CallFn>,
    pub snapshot: Snapshot,
    pub transfer: Transfer,
}

impl Kind {
    /* a kind whose values answer no operation for a literal to fill from */
    pub const PLAIN: Kind = Kind {
        name: "", version: 1, type_name: "", family: "", immutable: true,
        repr: |_| Ok(String::new()), str: None, truthy: |_, _| true, hash: |_, _| 0, eq: |_, _, _| false,
        cmp: None, binop: None, unop: None, to_f64: None, to_i128: None, from_i128: None, from_digits: None,
        trace: None, get: None, len: None, item: None, cut: None, iter: None, call: None,
        snapshot: Snapshot::Store, transfer: Transfer::Deny,
    };
}

/* the kinds of the values the engine builds itself which a runtime names */
#[derive(Clone, Copy)]
pub struct Built {
    // the slice a key built of bounds makes
    pub slice: Option<&'static Kind>,
    // the one value the ellipsis names
    pub ellipsis: Option<&'static Kind>,
}

impl Built {
    pub const NONE: Built = Built { slice: None, ellipsis: None };
}

/* the proof of owning a kind that only registering makes and every payload read needs */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindToken(u16);

impl KindToken {
    /* the place of the kind in its heap's registry which a site's rule compares */
    pub const fn index(self) -> u16 { self.0 }
}

// a slot tag packs generation and kind above the live bit
const LIVE: u32 = 1;
const fn tag(generation: u16, kind: u16) -> u32 { (generation as u32) << 16 | (kind as u32) << 2 | LIVE }
const fn kind_of_tag(t: u32) -> u16 { (t >> 2 & 0x3FFF) as u16 }

/* an entry whose tag every variant keeps in one place before its store */
#[repr(u8)]
enum Slot {
    // a free entry keeps the generation its next value takes
    Free(u32),
    Words(u32, [u64; 3]),
    Vals(u32, [Val; 3]),
    Shared(u32, Rc<dyn Any>),
}

impl Default for Slot { fn default() -> Self { Slot::Free(0) } }

// every store fits four words beside its tag so an entry keeps its size
const _: () = assert!(core::mem::size_of::<Slot>() <= 32);

impl Slot {
    #[inline(always)]
    fn tag(&self) -> u32 {
        match *self { Slot::Free(t) | Slot::Words(t, _) | Slot::Vals(t, _) | Slot::Shared(t, _) => t }
    }

    /* the store as the kind's own operations read it */
    #[inline]
    fn any(&self) -> Option<&dyn Any> {
        Some(match self {
            Slot::Free(_) => return None,
            Slot::Words(_, w) => w,
            Slot::Vals(_, v) => v,
            Slot::Shared(_, p) => &**p,
        })
    }

    fn of(t: u32, store: Store) -> Self {
        match store {
            Store::Words(w) => Slot::Words(t, w),
            Store::Vals(v) => Slot::Vals(t, v),
            Store::Shared(p) => Slot::Shared(t, p),
        }
    }
}

/* the bytes a value of the table holds with its place on the free list */
pub(crate) const ENTRY_BYTES: usize = core::mem::size_of::<Slot>() + core::mem::size_of::<u32>();

/* what a value marked for the first time keeps alive besides itself */
pub(crate) enum Kept { Nothing, Vals([Val; 3]), Shared(TraceFn, Rc<dyn Any>) }

/* a live entry as a snapshot carries it */
pub(crate) enum Saved<'a> { Words(&'a [u64; 3]), Vals([Val; 3]), Shared(&'a dyn Any) }

/* the kinds a table registered in the order their tokens name them */
#[derive(Default)]
struct Kinds(Vec<&'static Kind>);

impl Kinds {
    #[inline(always)]
    fn get(&self, i: u16) -> &'static Kind { self.0[i as usize] }
    fn iter(&self) -> impl Iterator<Item = &'static Kind> + '_ { self.0.iter().copied() }
    fn push(&mut self, k: &'static Kind) -> u16 {
        self.0.push(k);
        (self.0.len() - 1) as u16
    }
}

#[derive(Default)]
pub struct ForeignTable {
    kinds: Kinds,
    slots: Vec<Slot>,
    // live and reached bits 64 to a word so a sweep skips empty stretches
    bits: Vec<[u64; 2]>,
    // freed slots and past `fresh` the slots never used so growing fills no list
    free: Vec<u32>,
    fresh: u32,
    live: usize,
    // the live entries that count as heap slots toward a collection
    paced: usize,
    // the kind boxing ints past i128 which registers on its first box
    wide_int: Option<&'static Kind>,
    // its index once registered so a wide int boxes and reads without calling the kind
    wide: Option<u16>,
    // kinds made on first use which a restore may name before any value exists
    known: Vec<&'static Kind>,
}

const ALIVE: usize = 0;
const MARK: usize = 1;

impl ForeignTable {
    /* a known kind gets its token back and another kind of its name is refused */
    pub fn register(&mut self, kind: &'static Kind) -> Result<KindToken, VmErr> {
        if let Some(i) = self.kinds.iter().position(|k| k.name == kind.name) {
            // snapshots find a kind by its name so no two kinds share one
            if !core::ptr::eq(self.kinds.get(i as u16), kind) { return Err(super::cold_runtime("a second kind takes a name another kind holds")); }
            return Ok(KindToken(i as u16));
        }
        let i = self.kinds.push(kind);
        if self.wide_int.is_some_and(|w| core::ptr::eq(w, kind)) { self.wide = Some(i); }
        Ok(KindToken(i))
    }

    pub(crate) fn token_of(&self, kind: &'static Kind) -> Option<KindToken> {
        if let Some(w) = self.wide && core::ptr::eq(self.kinds.get(w), kind) { return Some(KindToken(w)); }
        self.kinds.iter().position(|k| core::ptr::eq(k, kind)).map(|i| KindToken(i as u16))
    }

    pub(crate) fn wide_int(&self) -> Option<&'static Kind> { self.wide_int }

    /* registers a kind a restore names from those `declared` or `built` or known */
    pub(crate) fn kind_named(&mut self, name: &str, declared: &[&'static Kind], built: &Built) -> Option<u16> {
        if let Some(i) = self.kinds.iter().position(|k| k.name == name) { return Some(i as u16); }
        let kind = [self.wide_int, built.slice, built.ellipsis].into_iter().flatten().chain(declared.iter().copied()).chain(self.known.iter().copied()).find(|k| k.name == name)?;
        self.register(kind).ok().map(|t| t.0)
    }
    pub(crate) fn kind_at(&self, kind: u16) -> &'static Kind { self.kinds.get(kind) }

    /* the live and reached bits of the 64 slots of word `w` once made */
    #[inline(always)]
    fn word(&mut self, w: usize) -> Option<&mut [u64; 2]> { self.bits.get_mut(w) }

    /* the bits a collection reads made from the tags so boxing never needs them */
    #[cold]
    #[inline(never)]
    fn ensure_bits(&mut self) {
        if !self.bits.is_empty() { return; }
        self.bits.resize(self.slots.len().div_ceil(64), [0; 2]);
        for (i, s) in self.slots.iter().enumerate() { self.bits[i / 64][ALIVE] |= u64::from(s.tag() & LIVE) << (i % 64); }
    }

    /* the live slot `v` names with `None` for a stale or forged handle */
    #[inline]
    fn slot(&self, v: Val) -> Option<(&Slot, u32)> {
        let (generation, index) = v.foreign_parts()?;
        let s = self.slots.get(index as usize)?;
        let t = s.tag();
        (t & LIVE != 0 && (t >> 16) as u16 == generation).then_some((s, t))
    }

    pub(crate) fn kind_of(&self, v: Val) -> Option<&'static Kind> { self.slot(v).map(|(_, t)| self.kinds.get(kind_of_tag(t))) }

    /* the kind and store of `v` for the operations that only read */
    pub(crate) fn open(&self, v: Val) -> Option<(&'static Kind, &dyn Any)> {
        let (s, t) = self.slot(v)?;
        Some((self.kinds.get(kind_of_tag(t)), s.any()?))
    }

    /* one compare against the tag the handle implies reads the inline i128 */
    #[inline]
    pub(crate) fn wide_value(&self, v: Val) -> Option<i128> {
        let (generation, index) = v.foreign_parts()?;
        match self.slots.get(index as usize)? {
            &Slot::Words(t, [lo, hi, _]) if t == tag(generation, self.wide?) => Some((hi as i128) << 64 | lo as i128),
            _ => None,
        }
    }

    /* the store of `v` for the owner of its kind */
    pub fn payload<T: Any>(&self, token: KindToken, v: Val) -> Option<&T> {
        let (s, _) = self.slot(v).filter(|&(_, t)| kind_of_tag(t) == token.0)?;
        s.any()?.downcast_ref::<T>()
    }

    /* a freed or never used index with its next generation doubling the table when full */
    #[inline]
    fn claim(&mut self) -> Option<(usize, u16)> {
        let i = match self.free.pop() {
            Some(i) => i as usize,
            None => {
                if self.fresh as usize == self.slots.len() { self.grow()?; }
                self.fresh += 1;
                self.fresh as usize - 1
            }
        };
        Some((i, (self.slots[i].tag() >> 16) as u16))
    }

    /* doubles from four slots so one box pays for four */
    #[cold]
    fn grow(&mut self) -> Option<()> {
        let start = self.slots.len();
        let end = (start * 2).max(4);
        u32::try_from(end).ok()?;
        self.slots.resize_with(end, Slot::default);
        if !self.bits.is_empty() { self.bits.resize(end.div_ceil(64), [0; 2]); }
        Some(())
    }

    /* writes a store into a free entry which a sweep left with no payload */
    #[inline]
    fn place(&mut self, i: usize, generation: u16, kind: u16, store: Store) -> Val {
        core::mem::forget(core::mem::replace(&mut self.slots[i], Slot::of(tag(generation, kind), store)));
        if let Some(w) = self.word(i / 64) { w[ALIVE] |= 1 << (i % 64); }
        self.live += 1;
        Val::foreign(generation, i as u32)
    }

    #[inline]
    fn insert(&mut self, kind: u16, store: Store) -> Option<Val> {
        let (i, generation) = self.claim()?;
        Some(self.place(i, generation, kind, store))
    }

    /* boxes a wide int straight into its slot and `None` before its kind registers */
    #[inline]
    pub(crate) fn push_wide(&mut self, n: i128) -> Option<Val> {
        let kind = self.wide?;
        let (i, generation) = self.claim()?;
        core::mem::forget(core::mem::replace(&mut self.slots[i], Slot::Words(tag(generation, kind), int_words(n))));
        if let Some(w) = self.word(i / 64) { w[ALIVE] |= 1 << (i % 64); }
        self.live += 1;
        Some(Val::foreign(generation, i as u32))
    }

    /* marks `v` and gives what its store keeps alive on its first mark */
    pub(crate) fn mark(&mut self, v: Val) -> Option<Kept> {
        let (generation, index) = v.foreign_parts()?;
        let i = index as usize;
        let s = self.slots.get(i)?;
        let t = s.tag();
        if t & LIVE == 0 || (t >> 16) as u16 != generation { return None; }
        let bit = 1u64 << (i % 64);
        self.ensure_bits();
        let word = &mut self.bits[i / 64];
        if word[MARK] & bit != 0 { return None; }
        word[MARK] |= bit;
        Some(match &self.slots[i] {
            &Slot::Vals(_, v) => Kept::Vals(v),
            Slot::Shared(_, p) => match self.kinds.get(kind_of_tag(t)).trace { Some(f) => Kept::Shared(f, p.clone()), None => Kept::Nothing },
            _ => Kept::Nothing,
        })
    }

    /* frees unmarked slots dropping shared payloads and gives how many freed ones paced collections */
    pub(crate) fn sweep(&mut self) -> usize {
        if self.live == 0 { return 0; }
        let mut paced = 0;
        // only a table holding paced entries reads the store of each freed one
        let counts = self.paced != 0;
        self.ensure_bits();
        let mut dead: Vec<Slot> = Vec::new();
        for w in 0..self.bits.len() {
            let [alive, marks] = self.bits[w];
            let mut gone = alive & !marks;
            self.bits[w] = [alive & !gone, 0];
            while gone != 0 {
                let i = w * 64 + gone.trailing_zeros() as usize;
                gone &= gone - 1;
                self.live -= 1;
                // a generation that wrapped would let an old `Val` match again so its slot retires
                let (generation, kind) = ((self.slots[i].tag() >> 16) as u16, kind_of_tag(self.slots[i].tag()));
                if counts { paced += usize::from(!matches!(self.slots[i], Slot::Shared(..)) && self.wide != Some(kind)); }
                let next = match generation.checked_add(1) {
                    Some(g) => { self.free.push(i as u32); g }
                    None => generation,
                };
                match &mut self.slots[i] {
                    // plain words and values stay where the next box of their shape writes over them
                    Slot::Free(t) | Slot::Words(t, _) | Slot::Vals(t, _) => *t = (next as u32) << 16,
                    s => { let old = core::mem::replace(s, Slot::Free((next as u32) << 16)); if matches!(old, Slot::Shared(..)) { dead.push(old); } }
                }
            }
        }
        drop(dead);
        self.paced -= paced;
        paced
    }

    /* counts the live entries pacing collections as heap slots do after a restore */
    pub(crate) fn recount(&mut self) -> usize {
        self.paced = self.slots.iter().filter(|s| s.tag() & LIVE != 0 && !matches!(s, Slot::Shared(..)) && self.wide != Some(kind_of_tag(s.tag()))).count();
        self.paced
    }

    /* every live slot for a snapshot with its kind and its generation and index */
    pub(crate) fn live_entries(&self) -> impl Iterator<Item = (&'static Kind, u16, u32, Saved<'_>)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| {
            let t = s.tag();
            if t & LIVE == 0 { return None; }
            let saved = match s {
                Slot::Free(_) => return None,
                Slot::Words(_, w) => Saved::Words(w),
                &Slot::Vals(_, v) => Saved::Vals(v),
                Slot::Shared(_, p) => Saved::Shared(&**p),
            };
            Some((self.kinds.get(kind_of_tag(t)), (t >> 16) as u16, i as u32, saved))
        })
    }

    pub(crate) fn len(&self) -> usize { self.slots.len() }

    /* puts a saved slot back where it was so every `Val` naming it still does */
    pub(crate) fn restore(&mut self, kind: u16, generation: u16, index: u32, store: Store) -> Option<()> {
        while self.slots.len() <= index as usize { self.grow()?; }
        if self.slots[index as usize].tag() & LIVE != 0 { return None; }
        self.place(index as usize, generation, kind, store);
        Some(())
    }

    /* whether a value a live store holds meets `f` for the checks a restore makes */
    pub(crate) fn holds_any(&self, f: impl Fn(Val) -> bool) -> bool {
        self.slots.iter().any(|s| matches!(s, &Slot::Vals(t, v) if t & LIVE != 0 && v.into_iter().any(&f)))
    }

    /* whether `len` slots are a length the table grows to and fit in `limit` bytes */
    pub(crate) fn holds(len: usize, limit: usize) -> bool {
        len == 0 || len >= 4 && len.is_power_of_two() && len.saturating_mul(core::mem::size_of::<Slot>()) <= limit
    }

    /* regrows to `len` slots and rebuilds the free list once a restore placed every slot */
    pub(crate) fn settle(&mut self, len: usize) {
        while self.slots.len() < len { if self.grow().is_none() { break; } }
        self.free = self.slots.iter().enumerate().filter(|(_, s)| s.tag() & LIVE == 0 && (s.tag() >> 16) as u16 != u16::MAX).map(|(i, _)| i as u32).collect();
        self.fresh = self.slots.len() as u32;
    }

    /* empties the table for a restore and keeps the registered kinds */
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        (self.fresh, self.paced) = (0, 0);
        self.bits.clear();
        self.free.clear();
        self.live = 0;
    }
}

/* an i128 as a store's low then high words and a zero third */
#[inline(always)]
pub const fn int_words(n: i128) -> [u64; 3] { [n as u64, (n >> 64) as u64, 0] }

/* the i128 the first two words of a store hold */
#[inline(always)]
pub const fn words_int(w: &[u64; 3]) -> i128 { (w[1] as i128) << 64 | w[0] as i128 }

impl HeapPool {
    /* boxes `payload` as a new value of the kind `token` owns */
    pub fn foreign_box(&mut self, token: KindToken, payload: Rc<dyn Any>) -> Result<Val, VmErr> { self.foreign_new(token, Store::Shared(payload)) }

    /* boxes an i128 inline in its entry as a store of words */
    pub fn foreign_box_int(&mut self, token: KindToken, i: i128) -> Result<Val, VmErr> { self.foreign_new(token, Store::Words(int_words(i))) }

    /* boxes `store` inline as a new value of the kind `token` owns */
    #[inline(never)]
    pub fn foreign_new(&mut self, token: KindToken, store: Store) -> Result<Val, VmErr> {
        if self.foreign_full() { return Err(VmErr::Heap); }
        // an inline store of any kind but the wide int paces as a heap slot
        let paces = !matches!(store, Store::Shared(_)) && self.foreign.wide != Some(token.0);
        let Some(v) = self.foreign.insert(token.0, store) else { return Err(VmErr::Heap) };
        if paces { self.live += 1; self.foreign.paced += 1; }
        self.note_alloc();
        Ok(v)
    }
    pub fn register_kind(&mut self, kind: &'static Kind) -> Result<KindToken, VmErr> { self.foreign.register(kind) }

    /* a new value of a kind the runtime names for a value the engine builds */
    fn built_new(&mut self, kind: Option<&'static Kind>, store: Store) -> Result<Val, VmErr> {
        let Some(kind) = kind else { return Err(super::cold_runtime("the runtime names no kind the engine builds")) };
        let token = self.foreign.register(kind)?;
        self.foreign_new(token, store)
    }

    /* a new slice of the runtime's slice kind over three bounds */
    pub(crate) fn new_slice(&mut self, bounds: [Val; 3]) -> Result<Val, VmErr> { self.built_new(self.natives.built.slice, Store::Vals(bounds)) }

    /* the bounds of `v` where it is a slice of the runtime's slice kind */
    #[inline]
    pub fn slice_of(&self, v: Val) -> Option<[Val; 3]> { if v.is_foreign() { self.slice_bounds(v) } else { None } }

    #[inline(never)]
    fn slice_bounds(&self, v: Val) -> Option<[Val; 3]> {
        let (s, t) = self.foreign.slot(v)?;
        let kind = self.natives.built.slice?;
        match s { &Slot::Vals(_, b) if core::ptr::eq(self.foreign.kinds.get(kind_of_tag(t)), kind) => Some(b), _ => None }
    }

    /* the one value of the runtime's ellipsis kind made on its first use */
    pub(crate) fn ellipsis(&mut self) -> Result<Val, VmErr> {
        // a swept ellipsis is made again on its next use
        if let Some(d) = self.dots && self.foreign.kind_of(d).is_some() { return Ok(d); }
        let v = self.built_new(self.natives.built.ellipsis, Store::Words([0; 3]))?;
        self.dots = Some(v);
        Ok(v)
    }

    /* whether the table must grow past twice the run's limit where heap slots stop too */
    #[inline]
    fn foreign_full(&self) -> bool { self.foreign.free.is_empty() && self.meter.held() > self.limit.saturating_mul(2) }
    /* names the kind boxing ints past i128 without registering it yet */
    pub fn set_wide_int(&mut self, kind: &'static Kind) {
        self.foreign.wide_int = Some(kind);
        self.foreign.wide = self.foreign.kinds.iter().position(|k| core::ptr::eq(k, kind)).map(|i| i as u16);
    }

    /* names a kind registered on first use so a restore can find it by name */
    pub fn know_kind(&mut self, kind: &'static Kind) { self.foreign.known.push(kind); }

    /* boxes a wide int without calling its kind and `None` until that kind registers */
    #[inline]
    pub(crate) fn wide_box(&mut self, n: i128) -> Option<Val> {
        if self.foreign_full() { return None; }
        let v = self.foreign.push_wide(n)?;
        self.note_alloc();
        Some(v)
    }
    /* the store of `v` read as `T` where the kind `token` owns it */
    pub fn foreign_payload<T: Any>(&self, token: KindToken, v: Val) -> Option<&T> { self.foreign.payload(token, v) }

    pub fn foreign_kind(&self, v: Val) -> Option<&'static Kind> { if v.is_foreign() { self.foreign.kind_of(v) } else { None } }

    /* `op` through a foreign operand kind or the wide int kind after two ints overflowed */
    pub fn foreign_binop(&mut self, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        let (ka, kb) = (self.foreign_kind(a), self.foreign_kind(b));
        for kind in [ka, kb.filter(|k| ka.is_none_or(|a| a.name != k.name)), (ka.is_none() && kb.is_none()).then(|| self.foreign.wide_int()).flatten()].into_iter().flatten() {
            if let Some(f) = kind.binop && let Some(r) = f(self, op, a, b)? { return Ok(Some(r)); }
        }
        Ok(None)
    }

    pub fn foreign_unop(&mut self, op: UnOp, v: Val) -> Result<Option<Val>, VmErr> {
        match self.foreign_kind(v).and_then(|k| k.unop) { Some(f) => f(self, op, v), None => Ok(None) }
    }

    /* `v` as an f64 through its kind and `None` for a kind without one */
    pub fn foreign_f64(&self, v: Val) -> Option<Result<f64, VmErr>> {
        let (kind, p) = self.foreign.open(v)?;
        Some(kind.to_f64?(p))
    }

    pub fn foreign_i128(&self, v: Val) -> Option<i128> {
        // a wide int kept inline answers without a call into its kind
        if let Some(n) = self.foreign.wide_value(v) { return Some(n); }
        let (kind, p) = self.foreign.open(v)?;
        kind.to_i128?(p)
    }

    pub fn foreign_repr(&self, v: Val) -> Option<Result<String, VmErr>> {
        let (kind, p) = self.foreign.open(v)?;
        Some((kind.repr)(p))
    }

    /* the text foreign `v` shows as text where its kind gives one of its own */
    pub fn foreign_str(&self, v: Val) -> Option<Result<String, VmErr>> {
        let (kind, p) = self.foreign.open(v)?;
        Some(kind.str?(p))
    }

    /* attribute `name` of foreign `v` through its kind or None when it gives none */
    pub fn foreign_get(&mut self, v: Val, name: &str) -> Result<Option<Val>, VmErr> {
        match self.foreign_kind(v).and_then(|k| k.get) { Some(f) => f(self, v, name), None => Ok(None) }
    }

    pub fn foreign_truthy(&self, v: Val) -> Option<bool> {
        let (kind, p) = self.foreign.open(v)?;
        Some((kind.truthy)(self, p))
    }

    pub fn foreign_hash(&self, v: Val) -> Option<u64> {
        let (kind, p) = self.foreign.open(v)?;
        Some((kind.hash)(self, p))
    }

    /* equality when either side is foreign and false across families */
    pub fn foreign_eq(&self, a: Val, b: Val) -> Option<bool> {
        // a foreign value equals itself whatever its kind answers
        if a.0 == b.0 { return Some(true); }
        // two wide ints kept inline compare their values with no call into their kind
        if a.is_foreign() && b.is_foreign() && let (Some(x), Some(y)) = (self.foreign.wide_value(a), self.foreign.wide_value(b)) { return Some(x == y); }
        let (fa, fb) = (self.foreign.open(a), self.foreign.open(b));
        let ((kind, p), other) = match (fa, fb) { (Some(x), _) => (x, b), (None, Some(y)) => (y, a), (None, None) => return None };
        let family = self.family_of(other);
        Some(family == Some(kind.family) && (kind.eq)(self, p, other))
    }

    /* order when either side is foreign and `None` when the kinds do not order */
    pub fn foreign_cmp(&self, a: Val, b: Val) -> Option<Ordering> {
        if let Some((kind, p)) = self.foreign.open(a) { return kind.cmp?(self, p, b); }
        let (kind, p) = self.foreign.open(b)?;
        kind.cmp?(self, p, a).map(Ordering::reverse)
    }

    /* the family of a foreign kind or of the wide int kind for a number */
    fn family_of(&self, v: Val) -> Option<&'static str> {
        if let Some(k) = self.foreign_kind(v) { return Some(k.family); }
        (v.is_int() || v.is_bool() && self.policy.bool_is_int || v.is_float()).then(|| self.foreign.wide_int().map(|k| k.family)).flatten()
    }

    /* how many items foreign `v` holds through its kind and None without a length */
    #[inline]
    pub(crate) fn foreign_len(&mut self, v: Val) -> Option<Result<i128, VmErr>> { if v.is_foreign() { self.kind_len(v) } else { None } }

    #[inline(never)]
    fn kind_len(&mut self, v: Val) -> Option<Result<i128, VmErr>> {
        let f = self.foreign_kind(v)?.len?;
        Some(f(self, v))
    }

    /* item `i` of foreign `v` through its kind and None where it takes no index */
    #[inline]
    pub(crate) fn foreign_item(&mut self, v: Val, i: Val) -> Option<Result<Val, VmErr>> { if v.is_foreign() { self.kind_item(v, i) } else { None } }

    #[inline(never)]
    fn kind_item(&mut self, v: Val, i: Val) -> Option<Result<Val, VmErr>> {
        let f = self.foreign_kind(v)?.item?;
        Some(f(self, v, i))
    }

    /* the cut of foreign `v` through its kind and None where it takes none */
    #[inline]
    pub(crate) fn foreign_cut(&mut self, v: Val, bounds: [Val; 3]) -> Option<Result<Val, VmErr>> { if v.is_foreign() { self.kind_cut(v, bounds) } else { None } }

    #[inline(never)]
    fn kind_cut(&mut self, v: Val, bounds: [Val; 3]) -> Option<Result<Val, VmErr>> {
        let f = self.foreign_kind(v)?.cut?;
        Some(f(self, v, bounds))
    }

    /* the frame a loop over foreign `v` steps and None where its kind gives none */
    #[inline]
    pub(crate) fn foreign_frame(&self, v: Val) -> Option<IterFrame> { if v.is_foreign() { self.kind_frame(v) } else { None } }

    #[inline(never)]
    fn kind_frame(&self, v: Val) -> Option<IterFrame> { (self.foreign_kind(v)?.iter?)(self, v) }

    /* a call of foreign `v` with positionals through its kind and None where it declines */
    pub(crate) fn foreign_call(&mut self, v: Val, args: &[Val]) -> Result<Option<Val>, VmErr> {
        match self.foreign_kind(v).and_then(|k| k.call) { Some(f) => f(self, v, args), None => Ok(None) }
    }

    /* whether foreign `v` never changes and a loop steps it as it is */
    #[inline]
    pub(crate) fn foreign_fixed(&self, v: Val) -> bool { v.is_foreign() && self.foreign_kind(v).is_some_and(|k| k.immutable && k.iter.is_some()) }

    /* marks a foreign `Val` and pushes what its store keeps alive */
    pub(crate) fn mark_foreign(&mut self, v: Val, out: &mut Vec<Val>) {
        match self.foreign.mark(v) {
            Some(Kept::Vals(vals)) => out.extend(vals),
            Some(Kept::Shared(trace, p)) => trace(&*p, &mut |c| out.push(c)),
            Some(Kept::Nothing) | None => {}
        }
    }
}
