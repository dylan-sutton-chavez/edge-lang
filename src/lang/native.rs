use alloc::{boxed::Box, string::String, vec::Vec};

use crate::value::{Val, VmErr};
use super::{Cx, KindId, Names, KINDS};

// a native's place in its runtime's table which bytecode will name
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NativeId(pub u16);

impl NativeId {
    pub const NONE: NativeId = NativeId(u16::MAX);
    // the init every class ends in which no table lists
    pub const INIT: NativeId = NativeId(u16::MAX - 1);
    // the first place of a binding a host gave past every table's natives
    pub const EXTERNS: u16 = 0x8000;

    /* the place of the host binding at `i` of the heap's list */
    pub const fn host(i: usize) -> NativeId { NativeId(Self::EXTERNS + i as u16) }

    /* the index of the host binding this place names */
    #[inline(always)]
    pub const fn host_at(self) -> Option<usize> { if self.0 >= Self::EXTERNS && self.0 < Self::INIT.0 { Some((self.0 - Self::EXTERNS) as usize) } else { None } }
}

/* a native body which never runs user code itself and returns a step for that */
pub type NativeFn = fn(&mut Cx<'_, '_>, Args<'_>) -> Ret;
pub type Ret = Result<Out, VmErr>;
/* the one argument path a register call will try before the full call */
pub type Fast1 = fn(&mut crate::value::HeapPool, Val) -> Option<Val>;
/* the two argument path a call will try before the full call */
pub type Fast2 = fn(&mut crate::value::HeapPool, Val, Val) -> Option<Val>;
/* the line plain positionals write with its units or none where the full call runs */
pub type FastOut = fn(&crate::value::HeapPool, &[Val], &mut String) -> Option<usize>;

/* the native may return a step so stepping it counts as user code */
pub const STEPS: u8 = 1;
/* its name reads as a type object whose call runs it */
pub const TYPE: u8 = 2;
/* a call on a list local lowers to the in place push */
pub const PUSH: u8 = 4;
/* read off the type itself it binds the type as dict fromkeys does */
pub const CLASS: u8 = 8;

/* an engine operation a native stands for so the lowering keeps its form */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intrinsic { None, Len, Range, Next, IsA }

// the intrinsics a runtime's table may name each at its own place
const INTRINSICS: usize = Intrinsic::IsA as usize + 1;

/* a native as its runtime declares it */
#[derive(Clone, Copy)]
pub struct Native {
    pub name: &'static str,
    pub sig: Sig,
    pub run: NativeFn,
    pub fast1: Option<Fast1>,
    pub fast2: Option<Fast2>,
    pub fast_out: Option<FastOut>,
    pub flags: u8,
    pub intrinsic: Intrinsic,
    // its place in the table that holds it which values and sites compare it by
    pub id: NativeId,
    // the kind whose methods hold it and none for a function
    pub owner: Option<KindId>,
}

impl core::fmt::Debug for Native {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.debug_struct("Native").field("name", &self.name).finish() }
}

impl Native {
    pub const fn new(name: &'static str, sig: Sig, run: NativeFn) -> Self {
        Self { name, sig, run, fast1: None, fast2: None, fast_out: None, flags: 0, intrinsic: Intrinsic::None, id: NativeId::NONE, owner: None }
    }
    /* the native at place `id` of the table holding it */
    #[doc(hidden)]
    pub const fn at(self, id: usize) -> Self { Self { id: NativeId(id as u16), ..self } }
    /* the method at place `id` the methods of kind `owner` hold */
    #[doc(hidden)]
    pub const fn method_at(self, id: usize, owner: KindId) -> Self { Self { id: NativeId(id as u16), owner: Some(owner), ..self } }
    pub const fn fast(self, f: Fast1) -> Self { Self { fast1: Some(f), ..self } }
    pub const fn fast2(self, f: Fast2) -> Self { Self { fast2: Some(f), ..self } }
    pub const fn fast_out(self, f: FastOut) -> Self { Self { fast_out: Some(f), ..self } }
    pub const fn flags(self, flags: u8) -> Self { Self { flags: self.flags | flags, ..self } }
    pub const fn intrinsic(self, intrinsic: Intrinsic) -> Self { Self { intrinsic, ..self } }
    /* the same kernel under the name a runtime's table gives it */
    pub const fn named(self, name: &'static str) -> Self { Self { name, ..self } }
    pub const fn steps(&self) -> bool { self.flags & STEPS != 0 }
    pub const fn is_type(&self) -> bool { self.flags & TYPE != 0 }
    pub const fn on_class(&self) -> bool { self.flags & CLASS != 0 }
    #[doc(hidden)]
    pub const BLANK: Native = Native::new("", Sig::exact(0), blank);
}

fn blank(_: &mut Cx<'_, '_>, _: Args<'_>) -> Ret { Err(VmErr::Runtime("a blank native ran")) }

// the positional count of a variadic signature
pub const VAR: u8 = u8::MAX;

/* what a native takes checked before it runs so its body counts nothing */
#[derive(Clone, Copy, Debug)]
pub struct Sig {
    pub min: u8,
    pub max: u8,
    // the positional parameters a keyword may fill by position and empty for positional only
    pub names: &'static [&'static str],
    // keyword only parameters that `kw` reads by index
    pub kw: &'static [&'static str],
    // other keywords pass as name and value pairs
    pub extra: bool,
}

impl Sig {
    pub const fn exact(n: u8) -> Self { Self { min: n, max: n, names: &[], kw: &[], extra: false } }
    pub const fn range(min: u8, max: u8) -> Self { Self { min, max, names: &[], kw: &[], extra: false } }
    pub const fn var(min: u8) -> Self { Self { min, max: VAR, names: &[], kw: &[], extra: false } }
    pub const fn named(self, names: &'static [&'static str]) -> Self { Self { names, ..self } }
    pub const fn kw(self, kw: &'static [&'static str]) -> Self { Self { kw, ..self } }
    pub const fn extra(self) -> Self { Self { extra: true, ..self } }
    /* whether `n` positionals fit with a variadic past 255 left to the bound path */
    #[inline(always)]
    pub const fn fits(&self, n: usize) -> bool { n.wrapping_sub(self.min as usize) <= self.max.wrapping_sub(self.min) as usize }
    /* whether any keyword may reach the native */
    pub const fn keywords(&self) -> bool { !self.names.is_empty() || !self.kw.is_empty() || self.extra }
}

/* what a call gives a native already checked against its signature */
#[derive(Clone, Copy)]
pub struct Args<'a> {
    // a method's receiver and undef for a function
    pub recv: Val,
    // positionals with a keyword in its place and an omitted one undef
    pub pos: &'a [Val],
    // keyword only values in signature order and undef when not given
    pub kw: &'a [Val],
    // other keywords as name and value pairs when the signature takes them
    pub named: &'a [Val],
}

impl Args<'_> {
    /* positional `i` and None when it is absent or omitted */
    #[inline]
    pub fn get(&self, i: usize) -> Option<Val> { self.pos.get(i).copied().filter(|v| !v.is_undef()) }
    /* keyword only parameter `i` and None when not given */
    #[inline]
    pub fn kw(&self, i: usize) -> Option<Val> { self.kw.get(i).copied().filter(|v| !v.is_undef()) }
    /* how many positionals the call passed */
    #[inline]
    pub fn len(&self) -> usize { self.pos.len() }
    #[inline]
    pub fn is_empty(&self) -> bool { self.pos.is_empty() }
}

/* a native's value or a boxed step so the common return stays small */
pub enum Out {
    Val(Val),
    Step(Box<Step>),
    // generator `g` run to its next item as frames with no box for the step
    Next(Val),
}

impl From<Val> for Out { fn from(v: Val) -> Self { Out::Val(v) } }
impl From<Step> for Out { fn from(s: Step) -> Self { Out::Step(Box::new(s)) } }

/* user code a native needs which runs as frames of the running loop */
pub enum Step {
    // calls `f` on its first `n` arguments and hands the value to `then`
    Call { f: Val, args: [Val; 3], n: u8, then: Then },
    // walks `it` item by item through the fold whose stop is the value
    Drain { it: Val, fold: Fold },
    // resumes generator `g` as send throw or close do and gives what they give
    Resume { g: Val, sent: Sent },
    // parks the run until the host pushes an event which is the value
    Wait,
}

impl Step {
    /* a call of `f` on `args` whose value is the native's */
    pub fn call(f: Val, args: &[Val]) -> Step { Step::call_then(f, args, Then::Give) }
    /* a call of `f` on up to three `args` whose value goes to `then` */
    pub fn call_then(f: Val, args: &[Val], then: Then) -> Step {
        let mut a = [Val::undef(); 3];
        let n = args.len().min(3);
        a[..n].copy_from_slice(&args[..n]);
        Step::Call { f, args: a, n: n as u8, then }
    }
}

/* what a call's value goes to */
pub enum Then {
    Give,
    Fn(ThenFn),
    Cont(Box<dyn Cont>),
}

pub type ThenFn = fn(&mut Cx<'_, '_>, Val) -> Ret;

/* a continuation holding values across a call which the collector reaches through `trace` */
pub trait Cont {
    fn resume(self: Box<Self>, cx: &mut Cx<'_, '_>, got: Val) -> Ret;
    fn trace(&self, mark: &mut dyn FnMut(Val));
}

/* how a resume enters a generator */
#[derive(Clone, Copy, Debug)]
pub enum Sent {
    Value(Val),
    // the exception or its type and then its value or undef
    Throw(Val, Val),
    Close,
}

/* a walk over an iterable folding each item into `acc` */
#[derive(Clone)]
pub struct Fold { pub on: FoldFn, pub acc: Acc, pub inline: Inline, pub take: Option<TakeFn> }

/* what the engine does with each item before the fold's function sees it */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Inline {
    // every item goes to the function
    None,
    // stops at the first item of that truth giving the flag else the other
    Truth(bool),
    // every item lands in `acc.items` and the function sees only the end
    Collect,
    // word ints add into `acc.n` and from another item on all go to the function
    Words,
    // plain numbers keep the best in `acc.v[0]` and other pairs go to the function
    Extreme(bool),
    // the key in `acc.v[0]` runs on every item into `acc.items` and the end must stop
    Keys,
    // the key in `acc.v[3]` runs on every item and plain keys compare in the engine
    Best(bool),
    // the first item stops the walk and an end gives the default in `acc.v[0]`
    First,
}

impl Fold {
    pub fn new(on: FoldFn) -> Self { Self::of(on, Acc::new(), Inline::None) }
    pub fn with(on: FoldFn, acc: Acc) -> Self { Self::of(on, acc, Inline::None) }
    const fn of(on: FoldFn, acc: Acc, inline: Inline) -> Self { Self { on, acc, inline, take: None } }
    /* any for `true` and all for `false` decided by the engine alone */
    pub fn truth(any: bool) -> Self { Self::of(settled, Acc::new(), Inline::Truth(any)) }
    /* the items gathered into `acc.items` by the engine before `on` sees the end */
    pub fn collect(on: FoldFn, acc: Acc) -> Self { Self::of(on, acc, Inline::Collect) }
    /* word ints summed into `acc.n` by the engine with every other item handed to `on` */
    pub fn words(on: FoldFn, acc: Acc) -> Self { Self::of(on, acc, Inline::Words) }
    /* a min for `true` or a max for `false` comparing plain numbers in the engine */
    pub fn extreme(min: bool, on: FoldFn, acc: Acc) -> Self { Self::of(on, acc, Inline::Extreme(min)) }
    /* each item's value under `key` gathered into `acc.items` before `on` sees the end */
    pub fn keys(key: Val, on: FoldFn, mut acc: Acc) -> Self {
        acc.v[0] = key;
        Self::of(on, acc, Inline::Keys)
    }
    /* the least or most item by `key` kept in `v[0]` with its key in `v[1]` */
    pub fn best(min: bool, key: Val, on: FoldFn, mut acc: Acc) -> Self {
        acc.v[3] = key;
        Self::of(on, acc, Inline::Best(min))
    }
    /* the first item or the default `acc.v[0]` with the function seeing an end without one */
    pub fn first(on: FoldFn, acc: Acc) -> Self { Self::of(on, acc, Inline::First) }
    /* the fold with `take` trying each item before its function sees it */
    pub fn taking(mut self, take: TakeFn) -> Self {
        self.take = Some(take);
        self
    }
}

/* adds an item to `acc` with no engine op or false for the fold's function */
pub type TakeFn = fn(&mut Acc, Val) -> bool;

/* the function of a fold the engine settles alone which only an end reaches */
fn settled(_: &mut Acc, _: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item { Item::End(_) => Ok(Flow::Stop(Val::none())), _ => Ok(Flow::More) }
}

pub type FoldFn = fn(&mut Acc, &mut Cx<'_, '_>, Item) -> Result<Flow, VmErr>;

/* what reaches a fold as an item or a call's value or the end */
#[derive(Clone, Copy, Debug)]
pub enum Item { Next(Val), Called(Val), End(Val) }

/* what a fold does next */
#[derive(Clone, Copy, Debug)]
pub enum Flow {
    More,
    // calls the first on the second as a frame whose value returns as Called
    Call(Val, Val),
    Stop(Val),
    // an end's answer that walks this iterable through the same fold which keeps it alive
    Drain(Val),
}

/* what a fold keeps between items with every value traced while it lives */
#[derive(Clone)]
pub struct Acc { pub v: [Val; 4], pub n: i64, pub f: (f64, f64), pub items: Vec<Val> }

impl Acc {
    pub const fn new() -> Self { Self { v: [Val(crate::abi::nan_box::TAG_UNDEF); 4], n: 0, f: (0.0, 0.0), items: Vec::new() } }
    pub(crate) fn trace(&self, mark: &mut dyn FnMut(Val)) {
        for &v in self.v.iter().chain(&self.items) { mark(v); }
    }
}

impl Default for Acc {
    fn default() -> Self { Self::new() }
}

/* how a render hook writes a value */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode { Str, Repr, Ascii }

pub type RenderFn = fn(&mut Cx<'_, '_>, Val, Mode) -> Result<String, VmErr>;
pub type FormatFn = fn(&mut Cx<'_, '_>, Val, &str) -> Result<String, VmErr>;
pub type MatchFn = fn(&crate::value::HeapPool, Val, Val) -> Result<bool, VmErr>;
pub type ArityFn = fn(&Native, Option<&str>, super::Misfit<'_>) -> VmErr;

/* a raised error's report from the source its head and the byte it rose at */
pub type WordsFn = fn(&str, &str, Option<usize>) -> String;

/* a source made a chunk as its unit asks or the diagnostics that stopped it */
pub type ParseFn = fn(&str, crate::syntax::parser::Unit) -> crate::syntax::parser::Parsed;

/* how sources become chunks through a token table or a front end of its own */
#[derive(Clone, Copy)]
pub struct Front {
    // the source file extension an import implies without its dot
    pub extension: &'static str,
    pub parse: ParseFn,
    // the specs a source imports which the module walk fetches
    pub imports: fn(&str) -> Vec<crate::deps::ImportSpec>,
    // the token table the sources are read through when the runtime has one
    pub syntax: Option<crate::syntax::lexer::Syntax>,
    // the rules its code folds constants under
    pub policy: &'static super::Policy,
}

impl Front {
    /* a front end that compiles nothing for a machine booted with no runtime */
    pub const NONE: Front = Front { extension: "", parse: refused, imports: no_imports, syntax: None, policy: &super::Policy::ENGINE };

    /* the front end of table `S` evaluated in the runtime crate which builds its parser */
    pub const fn spelled<S: crate::syntax::lexer::Spelling>() -> Front {
        Front { extension: S::SYNTAX.extension, parse: crate::syntax::parser::spelled::<S>, imports: crate::deps::scan_imports::<S>, syntax: Some(S::SYNTAX), policy: &S::NATIVES.policy }
    }

    /* what a program of this front end parses as with its imports through `resolver` */
    pub fn unit(&self, resolver: Box<dyn crate::deps::Resolver>) -> crate::syntax::parser::Unit {
        crate::syntax::parser::Unit::new(resolver, crate::syntax::parser::Body::Program, self.parse)
    }

    /* `source` as a program whose imports go through `resolver` */
    pub fn program(&self, source: &str, resolver: Box<dyn crate::deps::Resolver>) -> crate::syntax::parser::Parsed {
        (self.parse)(source, self.unit(resolver))
    }

    /* `source` as statements run as code of the file at `path` with nothing to import */
    pub fn exec(&self, source: &str, path: &alloc::sync::Arc<String>) -> crate::syntax::parser::Parsed {
        let unit = crate::syntax::parser::Unit::new(Box::new(crate::deps::NoopResolver), crate::syntax::parser::Body::Exec, self.parse);
        (self.parse)(source, crate::syntax::parser::Unit { path: Some(path.clone()), ..unit })
    }
}

/* the parse of a front end that compiles nothing */
fn refused(_source: &str, _unit: crate::syntax::parser::Unit) -> crate::syntax::parser::Parsed {
    Err(alloc::vec![crate::syntax::diag::Diagnostic { start: 0, end: 0, msg: String::from("this runtime compiles no code") }])
}

/* the imports of a source no front end reads */
fn no_imports(_source: &str) -> Vec<crate::deps::ImportSpec> { Vec::new() }

/* a runtime's natives as static tables that `natives!` builds with no work at boot */
pub struct Natives {
    fns: &'static [Native],
    nf: u16,
    index: &'static [(u32, u16)],
    methods: &'static [(u32, u16)],
    ranges: [(u16, u16); KINDS],
    // a bit per method name shape of each kind so most misses read no name
    shapes: [u64; KINDS],
    // the function each intrinsic names plus one and zero for none
    intrinsics: [u16; INTRINSICS],
    // each function's place plus one at its outline slot or the next free one
    outlines: [u8; 1024],
    // each type name's place plus one at its outline slot or the next free one
    typed: [u8; 1024],
    // a bit per outline of every name a load may find among the builtins
    kin: [u64; 16],
    // the names a load gives as type objects
    pub types: &'static [&'static str],
    // what the runtime calls the engine's kinds errors and conventions
    names: &'static Names,
    // the rules of the language the tables were built for
    pub policy: super::Policy,
    // the init every class ends in under the runtime's constructor name
    pub(crate) init: Native,
    // each foreign kind with methods by its registered name and where its run lies
    foreign: &'static [(&'static str, u16, u16)],
    // every name in table order which a snapshot keeps
    pub digest: u64,
    // the kinds of the values the engine builds itself
    pub built: crate::value::foreign::Built,
    // the runtime the tables were built for and empty for none
    pub runtime: &'static str,
}

impl Natives {
    pub const EMPTY: Natives = Natives { fns: &[], nf: 0, index: &[(0, 0)], methods: &[], ranges: [(0, 0); KINDS], shapes: [0; KINDS], intrinsics: [0; INTRINSICS], outlines: [0; 1024], typed: [0; 1024], kin: kin(&[], 0, &[], &Names::PLAIN), types: &[], names: &Names::PLAIN, policy: super::Policy::ENGINE, init: init_of(&Names::PLAIN), foreign: &[], digest: 0, built: crate::value::foreign::Built::NONE, runtime: "" };

    /* these natives as built under the rules `policy` states */
    pub(crate) const fn under(mut self, policy: super::Policy) -> Natives {
        self.policy = policy;
        self
    }

    // `all` holds the same table as a constant since const evaluation reads no static
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub const fn tables<L: super::Language>(fns: &'static [Native], all: &[Native], nf: usize, index: &'static [(u32, u16)], methods: &'static [(u32, u16)], ranges: [(u16, u16); KINDS], foreign: &'static [(&'static str, u16, u16)], types: &'static [&'static str]) -> Self {
        let names = L::NAMES;
        Natives { fns, nf: nf as u16, index, methods, ranges, shapes: shapes(all, nf, &ranges), intrinsics: intrinsic_ids(all, nf), outlines: outlines(all, nf), typed: typed(types), kin: kin(all, nf, types, names), types, names, policy: L::POLICY, init: init_of(names), foreign, digest: digest(all, nf, &ranges, foreign), built: L::BUILT, runtime: L::NAME }
    }

    /* the function named `name` found with one hash and one probe on a hit */
    pub fn find(&self, name: &str) -> Option<&'static Native> {
        let mask = self.index.len() - 1;
        let h = fnv(name);
        let mut s = h as usize & mask;
        loop {
            let (hh, id) = self.index[s];
            if hh == 0 { return None; }
            if hh == h && same_text(self.fns[id as usize].name, name) { return Some(&self.fns[id as usize]); }
            s = (s + 1) & mask;
        }
    }

    /* the method `name` of kind `kind` where a shape the kind lacks misses at once */
    #[inline(always)]
    pub fn method(&self, kind: KindId, name: &str) -> Option<&'static Native> {
        if self.shapes[kind as usize] & shape(name) == 0 { return None; }
        self.method_of(kind, name)
    }

    /* `method` by binary search in the kind's run */
    #[inline(never)]
    fn method_of(&self, kind: KindId, name: &str) -> Option<&'static Native> {
        let (lo, hi) = self.ranges[kind as usize];
        self.in_run(lo, hi, name)
    }

    /* the method `name` in the sorted run of `methods` from `lo` to `hi` */
    fn in_run(&self, lo: u16, hi: u16, name: &str) -> Option<&'static Native> {
        let part = &self.methods[lo as usize..hi as usize];
        let i = part.binary_search_by_key(&fnv(name), |e| e.0).ok()?;
        let n = &self.fns[part[i].1 as usize];
        same_text(n.name, name).then_some(n)
    }

    /* method `name` of the foreign kind registered under `kind` */
    #[inline(never)]
    pub fn foreign_method(&self, kind: &str, name: &str) -> Option<&'static Native> {
        let &(_, lo, hi) = self.foreign.iter().find(|(k, ..)| same_text(k, kind))?;
        self.in_run(lo, hi, name)
    }

    /* whether any foreign kind has methods of its own */
    #[inline(always)]
    pub fn has_foreign(&self) -> bool { !self.foreign.is_empty() }

    /* the name of the foreign kind whose run holds the native at `id` */
    pub fn foreign_owner(&self, id: NativeId) -> Option<&'static str> {
        let at = id.0.checked_sub(self.nf)?;
        self.foreign.iter().find(|&&(_, lo, hi)| (lo..hi).contains(&at)).map(|&(k, ..)| k)
    }

    #[inline(always)]
    pub fn get(&self, id: NativeId) -> Option<&'static Native> { self.fns.get(id.0 as usize) }

    /* the native at `id` with the init every class ends in among them */
    #[inline]
    pub fn native(&'static self, id: NativeId) -> Option<&'static Native> { if id == NativeId::INIT { Some(&self.init) } else { self.get(id) } }

    /* the kind whose methods hold the native at `id` and none for a function */
    #[inline]
    pub fn owner(&'static self, id: NativeId) -> Option<KindId> { self.native(id)?.owner }

    pub fn name(&self, id: NativeId) -> &'static str { if id == NativeId::INIT { self.init.name } else { self.get(id).map_or("", |n| n.name) } }

    /* how many functions lead the table */
    pub fn functions(&self) -> &'static [Native] { &self.fns[..self.nf as usize] }

    /* the methods of kind `kind` in the order the runtime declared them */
    pub fn kind_methods(&self, kind: KindId) -> &'static [Native] {
        let (lo, hi) = self.ranges[kind as usize];
        self.fns.get(self.nf as usize + lo as usize..self.nf as usize + hi as usize).unwrap_or(&[])
    }

    /* the function that stands for engine operation `k` and None when the runtime gives none */
    #[inline(always)]
    pub fn intrinsic(&self, k: Intrinsic) -> Option<&'static Native> {
        match self.intrinsics[k as usize] { 0 => None, i => self.fns.get(i as usize - 1) }
    }

    /* the function named `name` where most other names miss on their shape */
    #[inline(always)]
    pub fn function(&self, name: &str) -> Option<&'static Native> {
        probe(&self.outlines, name, |i| self.fns[i].name).map(|i| &self.fns[i])
    }

    /* the place of the function named `name` which a call of it names */
    #[inline]
    pub fn function_id(&self, name: &str) -> Option<NativeId> {
        probe(&self.outlines, name, |i| self.fns[i].name).map(|i| NativeId(i as u16))
    }

    /* the type name equal to `name` where most other names miss on their shape */
    #[inline]
    pub fn type_named(&self, name: &str) -> Option<&'static str> {
        probe(&self.typed, name, |i| self.types[i]).map(|i| self.types[i])
    }

    /* whether `name` may name a builtin while most other names miss at once */
    #[inline(always)]
    pub(crate) fn may_name(&self, name: &str) -> bool { marked(&self.kin, name) }

    /* what the runtime calls the engine's kinds errors and conventions */
    #[inline(always)]
    pub fn names(&self) -> &'static Names { self.names }
}

/* one of 64 bits for a name by its length and its outer bytes */
#[inline(always)]
const fn shape(name: &str) -> u64 {
    let b = name.as_bytes();
    let n = b.len();
    let (first, last) = if n == 0 { (0, 0) } else { (b[0] as u32, b[n - 1] as u32) };
    1 << ((n as u32 * 7 + first + last * 3) & 63)
}

/* the shape bits of each kind's method names */
const fn shapes(fns: &[Native], nf: usize, ranges: &[(u16, u16); KINDS]) -> [u64; KINDS] {
    let mut out = [0u64; KINDS];
    let mut k = 0;
    while k < KINDS {
        let (lo, hi) = ranges[k];
        let mut i = lo as usize;
        while i < hi as usize { out[k] |= shape(fns[nf + i].name); i += 1; }
        k += 1;
    }
    out
}

/* the 32-bit FNV-1a hash of a name where 0 marks an empty slot */
pub const fn fnv(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut h: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < b.len() {
        h = (h ^ b[i] as u32).wrapping_mul(0x0100_0193);
        i += 1;
    }
    if h == 0 { 1 } else { h }
}

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() { return false; }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] { return false; }
        i += 1;
    }
    true
}

#[doc(hidden)]
pub const fn count(parts: &[&[Native]]) -> usize {
    let (mut n, mut i) = (0, 0);
    while i < parts.len() { n += parts[i].len(); i += 1; }
    n
}

#[doc(hidden)]
pub const fn count_kinds(kinds: &[(KindId, &[&[Native]])]) -> usize {
    let (mut n, mut i) = (0, 0);
    while i < kinds.len() { n += count(kinds[i].1); i += 1; }
    n
}

#[doc(hidden)]
pub const fn count_foreign(kinds: &[(&str, &[&[Native]])]) -> usize {
    let (mut n, mut i) = (0, 0);
    while i < kinds.len() { n += count(kinds[i].1); i += 1; }
    n
}

/* the name table's slots as twice the functions rounded up to a power of two */
#[doc(hidden)]
pub const fn slots(nf: usize) -> usize { (2 * nf).next_power_of_two() }

/* every native in id order with functions then kinds' methods then foreign kinds' methods */
#[doc(hidden)]
pub const fn concat<const N: usize>(fns: &[&[Native]], kinds: &[(KindId, &[&[Native]])], foreign: &[(&str, &[&[Native]])]) -> [Native; N] {
    let mut out = [Native::BLANK; N];
    let mut k = 0;
    let mut p = 0;
    while p < fns.len() {
        let mut i = 0;
        while i < fns[p].len() { out[k] = fns[p][i].at(k); k += 1; i += 1; }
        p += 1;
    }
    let mut q = 0;
    while q < kinds.len() {
        let mut r = q + 1;
        while r < kinds.len() { assert!(kinds[r].0 as u8 != kinds[q].0 as u8, "a kind listed twice in natives"); r += 1; }
        let parts = kinds[q].1;
        let mut p = 0;
        while p < parts.len() {
            let mut i = 0;
            while i < parts[p].len() { out[k] = parts[p][i].method_at(k, kinds[q].0); k += 1; i += 1; }
            p += 1;
        }
        q += 1;
    }
    let mut q = 0;
    while q < foreign.len() {
        let mut r = q + 1;
        while r < foreign.len() { assert!(!same(foreign[r].0, foreign[q].0), "a foreign kind listed twice in natives"); r += 1; }
        let parts = foreign[q].1;
        let mut p = 0;
        while p < parts.len() {
            let mut i = 0;
            while i < parts[p].len() { out[k] = parts[p][i].method_at(k, KindId::Foreign); k += 1; i += 1; }
            p += 1;
        }
        q += 1;
    }
    assert!(k == N, "natives counted wrong");
    out
}

/* the open addressed name table of the first `nf` natives */
#[doc(hidden)]
pub const fn name_index<const M: usize>(fns: &[Native], nf: usize) -> [(u32, u16); M] {
    let mut slots = [(0u32, 0u16); M];
    let mask = M - 1;
    assert!(nf < u16::MAX as usize, "too many natives");
    let mut i = 0;
    while i < nf {
        let h = fnv(fns[i].name);
        let mut s = h as usize & mask;
        loop {
            if slots[s].0 == 0 { slots[s] = (h, i as u16); break; }
            assert!(!same(fns[slots[s].1 as usize].name, fns[i].name), "two native functions share a name");
            s = (s + 1) & mask;
        }
        i += 1;
    }
    slots
}

// the method keys in kind order and where each kind's run of them lies
#[doc(hidden)]
pub type MethodIndex<const NM: usize> = ([(u32, u16); NM], [(u16, u16); KINDS]);

/* each kind's methods sorted by hash with where each run lies and foreign runs last */
#[doc(hidden)]
pub const fn method_index<const NM: usize>(nf: usize, fns: &[Native], kinds: &[(KindId, &[&[Native]])], foreign: &[(&str, &[&[Native]])]) -> MethodIndex<NM> {
    let mut out = [(0u32, 0u16); NM];
    let mut ranges = [(0u16, 0u16); KINDS];
    let (mut at, mut q) = (0, 0);
    while q < kinds.len() {
        let n = count(kinds[q].1);
        out = sorted_run(out, nf, fns, at, n);
        ranges[kinds[q].0 as usize] = (at as u16, (at + n) as u16);
        at += n;
        q += 1;
    }
    let mut q = 0;
    while q < foreign.len() {
        let n = count(foreign[q].1);
        out = sorted_run(out, nf, fns, at, n);
        at += n;
        q += 1;
    }
    (out, ranges)
}

/* `out` with `n` method keys from `start` sorted by unique hashes */
const fn sorted_run<const NM: usize>(mut out: [(u32, u16); NM], nf: usize, fns: &[Native], start: usize, n: usize) -> [(u32, u16); NM] {
    let mut i = 0;
    while i < n {
        let key = (fnv(fns[nf + start + i].name), (nf + start + i) as u16);
        let mut j = start + i;
        while j > start && out[j - 1].0 > key.0 { out[j] = out[j - 1]; j -= 1; }
        out[j] = key;
        i += 1;
    }
    let mut k = start + 1;
    while k < start + n { assert!(out[k - 1].0 != out[k].0, "two methods of one kind share a lookup hash"); k += 1; }
    out
}

/* where each foreign kind's run of methods lies past the kinds' runs */
#[doc(hidden)]
pub const fn foreign_runs<const NK: usize>(kinds: &[(KindId, &[&[Native]])], foreign: &[(&'static str, &[&[Native]])]) -> [(&'static str, u16, u16); NK] {
    let mut out = [("", 0u16, 0u16); NK];
    let mut at = count_kinds(kinds);
    let mut q = 0;
    while q < NK {
        let n = count(foreign[q].1);
        out[q] = (foreign[q].0, at as u16, (at + n) as u16);
        at += n;
        q += 1;
    }
    out
}

/* the engine's init every class ends in named as the runtime names its constructor */
const fn init_of(names: &Names) -> Native { Native { id: NativeId::INIT, owner: Some(KindId::Instance), ..Native::new(names.constructor, Sig::var(0), crate::vm::slow::attr::init) } }

/* the function standing for each intrinsic plus one where two never share one */
const fn intrinsic_ids(fns: &[Native], nf: usize) -> [u16; INTRINSICS] {
    let mut out = [0u16; INTRINSICS];
    let mut i = 0;
    while i < nf {
        let k = fns[i].intrinsic as usize;
        if k != Intrinsic::None as usize {
            assert!(out[k] == 0, "two natives stand for one intrinsic");
            out[k] = i as u16 + 1;
        }
        i += 1;
    }
    out
}

/* the first `n` functions each at its outline slot or the next free one */
const fn outlines(fns: &[Native], nf: usize) -> [u8; 1024] {
    let mut out = [0u8; 1024];
    let mut i = 0;
    while i < nf { out = placed(out, fns[i].name, i); i += 1; }
    out
}

/* every type name at its outline slot or the next free one */
const fn typed(types: &[&str]) -> [u8; 1024] {
    let mut out = [0u8; 1024];
    let mut i = 0;
    while i < types.len() { out = placed(out, types[i], i); i += 1; }
    out
}

/* `table` with place `i` of `name` at its outline slot or the next free one */
const fn placed(mut table: [u8; 1024], name: &str, i: usize) -> [u8; 1024] {
    assert!(i < u8::MAX as usize, "too many names for one outline table");
    let mut s = outline(name);
    while table[s] != 0 { s = (s + 1) & 1023; }
    table[s] = i as u8 + 1;
    table
}

/* the place of `name` in an outline table whose entries `at` names */
#[inline(always)]
fn probe(table: &[u8; 1024], name: &str, at: impl Fn(usize) -> &'static str) -> Option<usize> {
    let mut s = outline(name);
    loop {
        match table[s] {
            0 => return None,
            i if same_text(at(i as usize - 1), name) => return Some(i as usize - 1),
            _ => s = (s + 1) & 1023,
        }
    }
}

/* the classes a runtime raises each with its parent and lookups built at compile time */
pub struct Tree {
    // each class beside its parent with the root's parent empty
    pub pairs: &'static [(&'static str, &'static str)],
    // each class's place plus one at its outline slot or the next free one
    at: [u8; 1024],
    // the place of each class's parent and the root's own place
    up: [u8; 255],
}

impl Tree {
    pub const EMPTY: Tree = Tree { pairs: &[], at: [0; 1024], up: [0; 255] };

    /* the tree of `pairs` whose parents are its own classes but the root's empty one */
    pub const fn of(pairs: &'static [(&'static str, &'static str)]) -> Tree {
        let (mut at, mut up) = ([0u8; 1024], [0u8; 255]);
        let mut i = 0;
        while i < pairs.len() { at = placed(at, pairs[i].0, i); i += 1; }
        let mut i = 0;
        while i < pairs.len() {
            up[i] = i as u8;
            let mut j = 0;
            while j < pairs.len() { if same(pairs[j].0, pairs[i].1) { up[i] = j as u8; } j += 1; }
            assert!(pairs[i].1.is_empty() || up[i] != i as u8, "a class names a parent outside its tree");
            i += 1;
        }
        assert!(Tree::rooted(&up, pairs.len()), "a class tree holds a cycle");
        Tree { pairs, at, up }
    }

    /* whether each of `n` classes whose parents `up` holds reaches a root within `n` steps */
    pub const fn rooted(up: &[u8], n: usize) -> bool {
        let mut i = 0;
        while i < n {
            let (mut at, mut steps) = (i, 0);
            while up[at] as usize != at {
                if steps == n { return false; }
                at = up[at] as usize;
                steps += 1;
            }
            i += 1;
        }
        true
    }

    /* the place of class `name` in the tree */
    #[inline]
    fn place(&self, name: &str) -> Option<usize> { probe(&self.at, name, |i| self.pairs[i].0) }

    /* whether `name` is a class of the tree */
    #[inline]
    pub fn contains(&self, name: &str) -> bool { self.place(name).is_some() }

    /* the parent of class `name` and None for the root or any other name */
    pub fn parent(&self, name: &str) -> Option<&'static str> {
        let i = self.place(name)?;
        let up = self.up[i] as usize;
        (up != i).then(|| self.pairs[up].0)
    }

    /* whether class `actual` is `expected` or one of its parents is */
    pub fn derives(&self, actual: &str, expected: &str) -> bool {
        if same_text(actual, expected) { return true; }
        let (Some(mut at), Some(to)) = (self.place(actual), self.place(expected)) else { return false };
        loop {
            let up = self.up[at] as usize;
            if up == at { return false; }
            if up == to { return true; }
            at = up;
        }
    }
}

/* whether two names hold the same bytes with the lengths compared in place first */
#[inline(always)]
pub fn same_text(a: &str, b: &str) -> bool { a.len() == b.len() && same_bytes(a.as_bytes(), b.as_bytes()) }

/* whether `text` opens with `head` compared a word at a time */
#[inline]
pub fn leads(text: &str, head: &str) -> bool { text.as_bytes().get(..head.len()).is_some_and(|t| same_bytes(t, head.as_bytes())) }

/* whether two byte strings hold the same bytes compared a word at a time */
#[inline]
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() { return false; }
    if n < 4 { return (0..n).all(|i| a[i] == b[i]); }
    if n < 8 { return word4(a, 0) == word4(b, 0) && word4(a, n - 4) == word4(b, n - 4); }
    // the last word then every whole word before it
    if word8(a, n - 8) != word8(b, n - 8) { return false; }
    let mut i = 0;
    while i + 8 < n {
        if word8(a, i) != word8(b, i) { return false; }
        i += 8;
    }
    true
}

#[inline(always)]
fn word4(s: &[u8], i: usize) -> u32 { s.get(i..i + 4).and_then(|w| w.try_into().ok()).map_or(0, u32::from_le_bytes) }

#[inline(always)]
fn word8(s: &[u8], i: usize) -> u64 { s.get(i..i + 8).and_then(|w| w.try_into().ok()).map_or(0, u64::from_le_bytes) }

/* one of 1024 bits for a name by its length and its outer bytes */
#[inline(always)]
const fn outline(name: &str) -> usize {
    let b = name.as_bytes();
    let (first, last) = if b.is_empty() { (0, 0) } else { (b[0] as u32, b[b.len() - 1] as u32) };
    ((b.len() as u32) << 16 | first << 8 | last).wrapping_mul(0x9E37_79B1) as usize >> 22
}

/* `set` with the outline bit of `name` on */
const fn mark(mut set: [u64; 16], name: &str) -> [u64; 16] {
    let o = outline(name);
    set[o >> 6 & 15] |= 1 << (o & 63);
    set
}

/* whether the outline bit of `name` is on in `set` */
#[inline(always)]
const fn marked(set: &[u64; 16], name: &str) -> bool {
    let o = outline(name);
    set[o >> 6 & 15] & 1 << (o & 63) != 0
}

/* the outline bits of every type function and builtin convention a load may find */
const fn kin(fns: &[Native], nf: usize, types: &[&str], names: &Names) -> [u64; 16] {
    let mut out = mark(mark(mark([0u64; 16], names.ellipsis), names.doc), super::ASSERT_CLASS);
    let mut i = 0;
    while i < types.len() { out = mark(out, types[i]); i += 1; }
    let mut i = 0;
    while i < nf { out = mark(out, fns[i].name); i += 1; }
    out
}

/* a hash over every name and each method's kind so other tables differ */
const fn digest(fns: &[Native], nf: usize, ranges: &[(u16, u16); KINDS], foreign: &[(&str, u16, u16)]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < fns.len() {
        let mut kind = 0xFFu8;
        let mut k = 0;
        while k < KINDS {
            let (lo, hi) = ranges[k];
            if i >= nf && i - nf >= lo as usize && i - nf < hi as usize { kind = k as u8; }
            k += 1;
        }
        // a foreign kind's method mixes in the name its kind registers under
        let mut f = 0;
        while f < foreign.len() {
            let (name, lo, hi) = foreign[f];
            if i >= nf && i - nf >= lo as usize && i - nf < hi as usize {
                let b = name.as_bytes();
                let mut j = 0;
                while j < b.len() { h = (h ^ b[j] as u64).wrapping_mul(0x0100_0000_01b3); j += 1; }
            }
            f += 1;
        }
        h = (h ^ kind as u64).wrapping_mul(0x0100_0000_01b3);
        let b = fns[i].name.as_bytes();
        let mut j = 0;
        while j < b.len() { h = (h ^ b[j] as u64).wrapping_mul(0x0100_0000_01b3); j += 1; }
        h = (h ^ 0xFF).wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

/* a runtime's static natives tables with ids in order and names checked at build */
#[macro_export]
macro_rules! natives {
    (
        language: $lang:ty,
        fns: [$($f:expr),* $(,)?],
        methods: [$($kind:ident => [$($m:expr),* $(,)?]),* $(,)?],
        $(foreign: [$($fk:path => [$($fm:expr),* $(,)?]),* $(,)?],)?
        types: $types:expr $(,)?
    ) => {{
        const FN_PARTS: &[&[$crate::lang::Native]] = &[$($f),*];
        const KIND_PARTS: &[($crate::lang::KindId, &[&[$crate::lang::Native]])] = &[$(($crate::lang::KindId::$kind, &[$($m),*])),*];
        const FOREIGN_PARTS: &[(&str, &[&[$crate::lang::Native]])] = &[$($(($fk.name, &[$($fm),*])),*)?];
        const NF: usize = $crate::lang::count(FN_PARTS);
        const NM: usize = $crate::lang::count_kinds(KIND_PARTS) + $crate::lang::count_foreign(FOREIGN_PARTS);
        const ALL: [$crate::lang::Native; NF + NM] = $crate::lang::concat::<{ NF + NM }>(FN_PARTS, KIND_PARTS, FOREIGN_PARTS);
        const INDEX: $crate::lang::MethodIndex<NM> = $crate::lang::method_index::<NM>(NF, &ALL, KIND_PARTS, FOREIGN_PARTS);
        static FNS: [$crate::lang::Native; NF + NM] = ALL;
        static NAME_INDEX: [(u32, u16); $crate::lang::slots(NF)] = $crate::lang::name_index::<{ $crate::lang::slots(NF) }>(&ALL, NF);
        static METHODS: [(u32, u16); NM] = INDEX.0;
        static FOREIGN: [(&str, u16, u16); FOREIGN_PARTS.len()] = $crate::lang::foreign_runs::<{ FOREIGN_PARTS.len() }>(KIND_PARTS, FOREIGN_PARTS);
        $crate::lang::Natives::tables::<$lang>(&FNS, &ALL, NF, &NAME_INDEX, &METHODS, INDEX.1, &FOREIGN, $types)
    }};
}
