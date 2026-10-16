/* FOREIGN values, objects the engine holds without knowing their insides, each one owned by the kind that made it.

   A FOREIGN Val is the tag 0x7FFD with a 16-bit generation and a 32-bit index into the table below. The table
   lives in the heap, so the collector marks and sweeps its entries the way it does heap slots. The kind sits in
   the entry, never in the Val, and a freed entry takes the next generation, so a stale or forged Val finds no
   object. A slot whose generation runs out is retired instead of reused.

   Only the code that registered a kind holds its token, and only the token reads a payload. The engine and the
   other frontends pass the Val around and call the operations of the kind. Two kinds of different families are
   never equal, and inside a family the family decides the hash, so `a == b` keeps implying equal hashes.

   SNAPSHOTS AND STATE OUTSIDE THE ENGINE. A kind either serializes its payload into the blob or makes the snapshot
   fail with an error that names it, so nothing is saved broken without a word. Two owners will need more and are
   wired here when their stage lands:
   - C objects (stage 5). Natively the engine, the C extension and its memory all run inside the unikernel guest,
     so a snapshot there is the memory of the whole guest and the C pointers stay valid when it is restored. What
     lives outside the guest, files, sockets and other resources, only exists through system calls the host checks
     against the grants of edge.json, so the host knows each one and rebuilds it on restore. A C kind then gets a
     third policy, kept by the guest snapshot, and this table only records which entries it covers.
   - JavaScript objects (stage 7). They live in the browser, not in any guest memory, so no snapshot carries them.
     Their rule, rebinding by name or refusing the snapshot, is decided with the browser host in that stage. */
use alloc::{rc::Rc, string::String, vec::Vec};
use core::any::Any;
use core::cmp::Ordering;
use super::{HeapPool, Val, VmErr};

/* Binary operators a kind may answer, `None` from its hook meaning NotImplemented. */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp { Add, Sub, Mul, FloorDiv, Mod, TrueDiv, Pow, And, Or, Xor, Shl, Shr }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp { Neg, Pos, Abs, Invert }

/* What a snapshot does with a live entry of the kind. */
pub enum Snapshot {
    Serialize { save: fn(&dyn Any, &mut Vec<u8>), load: fn(&[u8]) -> Option<Rc<dyn Any>> },
    Reject,
}

/* How a value of the kind crosses to another VM, by its snapshot codec or not at all. */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transfer { Copy, Deny }

/* What an entry holds, behind an Rc, or inline for a wide int that fits i128 so boxing it allocates nothing. */
#[derive(Clone)]
pub(crate) enum Payload { Shared(Rc<dyn Any>), Int(i128) }

impl Payload {
    fn get(&self) -> &dyn Any { match self { Self::Shared(p) => &**p, Self::Int(i) => i } }
    /* A payload a snapshot decoded, an i128 moving inline. */
    pub(crate) fn decoded(p: Rc<dyn Any>) -> Self { match p.downcast_ref::<i128>() { Some(&i) => Self::Int(i), None => Self::Shared(p) } }
}

// The signatures of the optional operations a kind answers.
type CmpFn = fn(&HeapPool, &dyn Any, Val) -> Option<Ordering>;
type BinFn = fn(&mut HeapPool, BinOp, Val, Val) -> Result<Option<Val>, VmErr>;
type UnFn = fn(&mut HeapPool, UnOp, Val) -> Result<Option<Val>, VmErr>;
type F64Fn = fn(&dyn Any) -> Result<f64, VmErr>;
type BoxFn = fn(&mut HeapPool, i128) -> Result<Val, VmErr>;
type TraceFn = fn(&dyn Any, &mut dyn FnMut(Val));

/* A kind, registered once by name, with every operation it answers. An absent operation is `None`. */
pub struct Kind {
    pub name: &'static str,
    pub version: u32,
    // What `type()` and error messages call a value of the kind.
    pub type_name: &'static str,
    // Values of different families never compare equal, so their hashes need not agree.
    pub family: &'static str,
    // An immutable value may be copied or shared freely, its identity never observed.
    pub immutable: bool,
    pub repr: fn(&dyn Any) -> Result<String, VmErr>,
    pub truthy: fn(&dyn Any) -> bool,
    // The table hash, equal for every value of the family that compares equal.
    pub hash: fn(&dyn Any) -> u64,
    // Equality with `other`, never called across families, and never running user code.
    pub eq: fn(&HeapPool, &dyn Any, Val) -> bool,
    pub cmp: Option<CmpFn>,
    pub binop: Option<BinFn>,
    pub unop: Option<UnFn>,
    pub to_f64: Option<F64Fn>,
    pub to_i128: Option<fn(&dyn Any) -> Option<i128>>,
    // Boxes an i128 too wide for an inline int, set only on the kind that holds the wide ints.
    pub from_i128: Option<BoxFn>,
    // The engine values a payload keeps alive, for kinds that hold any.
    pub trace: Option<TraceFn>,
    pub snapshot: Snapshot,
    pub transfer: Transfer,
}

/* Proof of owning a kind, made only by registering it. */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindToken(u16);

// A slot tag, the generation in the top 16 bits, then the kind, then whether it holds an int inline and whether it is live.
const LIVE: u32 = 1;
const INT: u32 = 2;
const fn tag(generation: u16, kind: u16, int: bool) -> u32 { (generation as u32) << 16 | (kind as u32) << 2 | (int as u32) << 1 | LIVE }
const fn kind_of_tag(t: u32) -> u16 { (t >> 2 & 0x3FFF) as u16 }

/* An inline int at eight-byte alignment, so the table never needs the aligned allocator path. */
#[derive(Clone, Copy, Default)]
pub struct Inline([u64; 2]);

impl Inline {
    fn new(n: i128) -> Self { Self([n as u64, (n >> 64) as u64]) }
    pub fn get(&self) -> i128 { (self.0[1] as i128) << 64 | self.0[0] as i128 }
}

/* One entry, its tag checked in a single compare, then the inline int or the shared payload. */
#[derive(Default)]
struct Slot { tag: u32, int: Inline, shared: Option<Rc<dyn Any>> }

#[derive(Default)]
pub struct ForeignTable {
    kinds: Vec<&'static Kind>,
    // A free slot keeps in its tag the generation its next value takes.
    slots: Vec<Slot>,
    // Which slots are live and which the collector reached, 64 to a word so a sweep skips empty stretches.
    bits: Vec<[u64; 2]>,
    free: Vec<u32>,
    live: usize,
    // The kind boxing the ints an i128 cannot hold, None for a frontend without one, registered on its first box.
    wide_int: Option<&'static Kind>,
    // Its index once registered, so a wide int boxes and reads without a call into the kind.
    wide: Option<u16>,
    // Kinds the frontend makes on first use, which a restore may name before any value made one.
    known: Vec<&'static Kind>,
}

const ALIVE: usize = 0;
const MARK: usize = 1;

impl ForeignTable {
    /* Registers `kind` under its name, the same token again for a name already known. */
    pub fn register(&mut self, kind: &'static Kind) -> KindToken {
        if let Some(i) = self.kinds.iter().position(|k| k.name == kind.name) { return KindToken(i as u16); }
        self.kinds.push(kind);
        let i = (self.kinds.len() - 1) as u16;
        if self.wide_int.is_some_and(|w| core::ptr::eq(w, kind)) { self.wide = Some(i); }
        KindToken(i)
    }

    /* The token of a kind already registered, found by its name. */
    pub(crate) fn token_of(&self, kind: &'static Kind) -> Option<KindToken> {
        if let Some(w) = self.wide && core::ptr::eq(self.kinds[w as usize], kind) { return Some(KindToken(w)); }
        self.kinds.iter().position(|&k| core::ptr::eq(k, kind)).map(|i| KindToken(i as u16))
    }

    pub(crate) fn wide_int(&self) -> Option<&'static Kind> { self.wide_int }

    /* The index of the kind named `name`, registering a known kind a restore names before any box. */
    pub(crate) fn kind_named(&mut self, name: &str) -> Option<u16> {
        if let Some(i) = self.kinds.iter().position(|k| k.name == name) { return Some(i as u16); }
        let kind = self.wide_int.into_iter().chain(self.known.iter().copied()).find(|k| k.name == name)?;
        Some(self.register(kind).0)
    }
    pub(crate) fn kind_at(&self, kind: u16) -> &'static Kind { self.kinds[kind as usize] }

    /* The live slot `v` names and its tag, None for a stale or forged handle. */
    #[inline]
    fn slot(&self, v: Val) -> Option<(&Slot, u32)> {
        let (generation, index) = v.foreign_parts()?;
        let s = self.slots.get(index as usize)?;
        (s.tag & LIVE != 0 && (s.tag >> 16) as u16 == generation).then_some((s, s.tag))
    }

    fn get(s: &Slot, t: u32) -> Option<&dyn Any> { if t & INT != 0 { Some(&s.int) } else { s.shared.as_deref() } }

    pub(crate) fn kind_of(&self, v: Val) -> Option<&'static Kind> { self.slot(v).map(|(_, t)| self.kinds[kind_of_tag(t) as usize]) }

    /* The kind and the payload of `v`, for the operations that only read. */
    pub(crate) fn open(&self, v: Val) -> Option<(&'static Kind, &dyn Any)> {
        let (s, t) = self.slot(v)?;
        Some((self.kinds[kind_of_tag(t) as usize], Self::get(s, t)?))
    }

    /* The i128 a wide int keeps inline, one compare of the tag the handle implies. */
    #[inline]
    pub(crate) fn wide_value(&self, v: Val) -> Option<i128> {
        let (generation, index) = v.foreign_parts()?;
        let s = self.slots.get(index as usize)?;
        (s.tag == tag(generation, self.wide?, true)).then(|| s.int.get())
    }

    /* The payload of `v` for the owner of its kind. */
    pub fn payload<T: Any>(&self, token: KindToken, v: Val) -> Option<&T> {
        let (s, t) = self.slot(v).filter(|&(_, t)| kind_of_tag(t) == token.0)?;
        Self::get(s, t)?.downcast_ref::<T>()
    }

    /* A free index with the generation its next value takes, the table doubling when none is left. */
    #[inline]
    fn claim(&mut self) -> Option<(usize, u16)> {
        if self.free.is_empty() { self.grow()?; }
        let i = self.free.pop()? as usize;
        Some((i, (self.slots[i].tag >> 16) as u16))
    }

    /* Doubles the table from four slots, so claiming one is a pop and a program that boxes once pays for four. */
    #[cold]
    fn grow(&mut self) -> Option<()> {
        let start = self.slots.len();
        let end = (start * 2).max(4);
        u32::try_from(end).ok()?;
        self.slots.resize_with(end, Slot::default);
        self.bits.resize(end.div_ceil(64), [0; 2]);
        // Popped from the end, so the lowest index goes first.
        self.free.extend((start as u32..end as u32).rev());
        Some(())
    }

    fn place(&mut self, i: usize, generation: u16, kind: u16, payload: Payload) -> Val {
        let s = &mut self.slots[i];
        match payload {
            Payload::Int(n) => { s.int = Inline::new(n); s.tag = tag(generation, kind, true); }
            Payload::Shared(p) => { s.shared = Some(p); s.tag = tag(generation, kind, false); }
        }
        self.bits[i / 64][ALIVE] |= 1 << (i % 64);
        self.live += 1;
        Val::foreign(generation, i as u32)
    }

    fn insert(&mut self, kind: u16, payload: Payload) -> Option<Val> {
        let (i, generation) = self.claim()?;
        Some(self.place(i, generation, kind, payload))
    }

    /* Boxes a wide int straight into its slot, None before its kind is registered. */
    #[inline]
    pub(crate) fn push_wide(&mut self, n: i128) -> Option<Val> {
        let kind = self.wide?;
        let (i, generation) = self.claim()?;
        Some(self.place(i, generation, kind, Payload::Int(n)))
    }

    /* Marks the slot of `v`, giving its kind and payload the first time when the kind traces. */
    pub(crate) fn mark(&mut self, v: Val) -> Option<(&'static Kind, Payload)> {
        let (i, t) = { let (generation, index) = v.foreign_parts()?; let s = self.slots.get(index as usize)?; (s.tag & LIVE != 0 && (s.tag >> 16) as u16 == generation).then_some((index as usize, s.tag))? };
        let bit = 1u64 << (i % 64);
        if self.bits[i / 64][MARK] & bit != 0 { return None; }
        self.bits[i / 64][MARK] |= bit;
        let kind = self.kinds[kind_of_tag(t) as usize];
        kind.trace?;
        let s = &self.slots[i];
        Some((kind, if t & INT != 0 { Payload::Int(s.int.get()) } else { Payload::Shared(s.shared.clone()?) }))
    }

    /* Frees the unmarked slots, all of them first and their payloads after, so no drop sees a half swept table. */
    pub(crate) fn sweep(&mut self) {
        if self.live == 0 { return; }
        let mut dead: Vec<Rc<dyn Any>> = Vec::new();
        for w in 0..self.bits.len() {
            let [alive, marks] = self.bits[w];
            let mut gone = alive & !marks;
            self.bits[w] = [alive & !gone, 0];
            while gone != 0 {
                let i = w * 64 + gone.trailing_zeros() as usize;
                gone &= gone - 1;
                let s = &mut self.slots[i];
                dead.extend(s.shared.take());
                self.live -= 1;
                // A generation that wraps would let an old Val match again, so its slot retires.
                let generation = (s.tag >> 16) as u16;
                match generation.checked_add(1) {
                    Some(g) => { s.tag = (g as u32) << 16; self.free.push(i as u32); }
                    None => s.tag = (generation as u32) << 16,
                }
            }
        }
        drop(dead);
    }

    /* Every live slot with its kind name, generation and index, for a snapshot. */
    pub(crate) fn live_entries(&self) -> impl Iterator<Item = (&'static Kind, u16, u32, &dyn Any)> {
        self.slots.iter().enumerate().filter(|(_, s)| s.tag & LIVE != 0).filter_map(|(i, s)| Some((self.kinds[kind_of_tag(s.tag) as usize], (s.tag >> 16) as u16, i as u32, Self::get(s, s.tag)?)))
    }

    pub(crate) fn len(&self) -> usize { self.slots.len() }

    /* Puts back a slot a snapshot saved, at its index and generation, so every Val naming it still does. */
    pub(crate) fn restore(&mut self, kind: u16, generation: u16, index: u32, payload: Payload) -> Option<()> {
        while self.slots.len() <= index as usize { self.grow()?; }
        if self.slots[index as usize].tag & LIVE != 0 { return None; }
        self.place(index as usize, generation, kind, payload);
        Some(())
    }

    /* Sizes the table to the `len` slots it had and rebuilds the free list once a restore placed every slot. */
    pub(crate) fn settle(&mut self, len: usize) {
        while self.slots.len() < len { if self.grow().is_none() { break; } }
        self.free = self.slots.iter().enumerate().filter(|(_, s)| s.tag & LIVE == 0 && (s.tag >> 16) as u16 != u16::MAX).map(|(i, _)| i as u32).collect();
    }

    /* Empties the table for a restore, keeping the registered kinds. */
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.bits.clear();
        self.free.clear();
        self.live = 0;
    }
}

impl HeapPool {
    /* Boxes `payload` as a new value of the kind `token` owns. */
    pub fn foreign_box(&mut self, token: KindToken, payload: Rc<dyn Any>) -> Result<Val, VmErr> { self.foreign_put(token, Payload::Shared(payload)) }

    /* Boxes an i128 of the kind `token` owns inline in its entry, the payload its operations see as an `i128`. */
    pub fn foreign_box_int(&mut self, token: KindToken, i: i128) -> Result<Val, VmErr> { self.foreign_put(token, Payload::Int(i)) }

    fn foreign_put(&mut self, token: KindToken, payload: Payload) -> Result<Val, VmErr> {
        let Some(v) = self.foreign.insert(token.0, payload) else { return Err(VmErr::Heap) };
        self.note_alloc();
        Ok(v)
    }
    pub fn register_kind(&mut self, kind: &'static Kind) -> KindToken { self.foreign.register(kind) }
    /* Names the kind that boxes the ints an i128 cannot hold, without registering it yet. */
    pub fn set_wide_int(&mut self, kind: &'static Kind) {
        self.foreign.wide_int = Some(kind);
        self.foreign.wide = self.foreign.kinds.iter().position(|&k| core::ptr::eq(k, kind)).map(|i| i as u16);
    }

    /* Names a kind the frontend registers on first use, so a restore can find it by name. */
    pub fn know_kind(&mut self, kind: &'static Kind) { self.foreign.known.push(kind); }

    /* Boxes a wide int without a call into its kind, None until the kind registered on its first box. */
    #[inline]
    pub(crate) fn wide_box(&mut self, n: i128) -> Option<Val> {
        let v = self.foreign.push_wide(n)?;
        self.note_alloc();
        Some(v)
    }
    pub fn foreign_payload<T: Any>(&self, token: KindToken, v: Val) -> Option<&T> { self.foreign.payload(token, v) }
    pub fn foreign_kind(&self, v: Val) -> Option<&'static Kind> { if v.is_foreign() { self.foreign.kind_of(v) } else { None } }

    /* `op` through the kind of a FOREIGN operand, or of the wide ints when two ints overflowed, None for NotImplemented. */
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

    /* `v` as an f64 through its kind, None for a kind without one. */
    pub fn foreign_f64(&self, v: Val) -> Option<Result<f64, VmErr>> {
        let (kind, p) = self.foreign.open(v)?;
        Some(kind.to_f64?(p))
    }

    pub fn foreign_i128(&self, v: Val) -> Option<i128> {
        // A wide int kept inline answers without a call into its kind.
        if let Some(n) = self.foreign.wide_value(v) { return Some(n); }
        let (kind, p) = self.foreign.open(v)?;
        kind.to_i128?(p)
    }

    pub fn foreign_repr(&self, v: Val) -> Option<Result<String, VmErr>> {
        let (kind, p) = self.foreign.open(v)?;
        Some((kind.repr)(p))
    }

    pub fn foreign_truthy(&self, v: Val) -> Option<bool> {
        let (kind, p) = self.foreign.open(v)?;
        Some((kind.truthy)(p))
    }

    pub fn foreign_hash(&self, v: Val) -> Option<u64> {
        let (kind, p) = self.foreign.open(v)?;
        Some((kind.hash)(p))
    }

    /* Equality when either side is FOREIGN, false across families. */
    pub fn foreign_eq(&self, a: Val, b: Val) -> Option<bool> {
        let (fa, fb) = (self.foreign.open(a), self.foreign.open(b));
        let ((kind, p), other) = match (fa, fb) { (Some(x), _) => (x, b), (None, Some(y)) => (y, a), (None, None) => return None };
        let family = self.family_of(other);
        Some(family == Some(kind.family) && (kind.eq)(self, p, other))
    }

    /* Order when either side is FOREIGN, None when the kinds do not order. */
    pub fn foreign_cmp(&self, a: Val, b: Val) -> Option<Ordering> {
        if let Some((kind, p)) = self.foreign.open(a) { return kind.cmp?(self, p, b); }
        let (kind, p) = self.foreign.open(b)?;
        kind.cmp?(self, p, a).map(Ordering::reverse)
    }

    /* The equality family of `v`, a FOREIGN kind or the numbers the wide int kind shares a family with. */
    fn family_of(&self, v: Val) -> Option<&'static str> {
        if let Some(k) = self.foreign_kind(v) { return Some(k.family); }
        (v.is_int() || v.is_bool() || v.is_float()).then(|| self.foreign.wide_int().map(|k| k.family)).flatten()
    }

    /* Marks a FOREIGN Val and pushes what its payload keeps alive. */
    pub(crate) fn mark_foreign(&mut self, v: Val, out: &mut Vec<Val>) {
        if let Some((kind, p)) = self.foreign.mark(v) && let Some(trace) = kind.trace { trace(p.get(), &mut |c| out.push(c)); }
    }
}
