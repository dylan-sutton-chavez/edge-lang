use alloc::{boxed::Box, rc::Rc, vec::Vec};

use super::VM;
use super::act::{Act, BASES, JOB, NO_DEST};
use super::dispatch::Floor;
use super::keys::PairScan;
use super::lower::{self, Code};
use super::opcodes::function::Passed;
use super::types::*;
use crate::parser::{OpCode, SSAChunk};

/* A builtin whose callback runs as a frame of the loop, what it keeps between calls, its frame's caller carrying on only once it is done. */
pub(crate) enum Job {
    // `map(f, it)` a `for` steps, each call giving its item, `before` the exception pending at its start (undef for none), `ctor` for a class call.
    Map { before: Val, ctor: bool },
    // A call the next step of builtin iterator `target` makes, run first as a frame, `item` a `filter`'s test item, `sentinel` the end of `iter(f, sentinel)`.
    Feed { target: Val, item: Val, sentinel: Val, kind: FeedKind, drive: bool },
    // `filter(f, it)` a `for` steps, `f` set while a test result's `__bool__` or `__len__` runs, and always when `retake` reruns the test through its step.
    Filter { it: Val, item: Val, before: Val, f: Val, retake: bool, kind: FeedKind },
    Sorted(Box<Sorted>),
    // A builtin over a generator, the generator run as a frame of the loop whose yields the job takes.
    Gather(Box<Gather>),
    // `sorted` or `list.sort` of instances of one class, its `__lt__` run as frames of the loop.
    Merge(Box<Merge>),
    // `count`, `index` or `remove` of a list or tuple, the `__eq__` of its items or of what it seeks run as frames of the loop.
    Scan(Box<Scan>),
    // The dunder a builtin running a generator calls on what the generator yielded, run as a frame over the generator's.
    Fold(Fold),
    // The one dunder a builtin calls on its argument, run as a frame whose result the builtin checks.
    Check(Check),
    // A builtin that carries on with what dunders convert, the one running now checked as `Check` checks it, and the dunder it found last.
    Convert(Check, Box<dyn Resume>, Memo),
    // The `__iter__` of a user iterable a builtin steps, run as a frame, the builtin's job under it.
    IterOf,
    // `next` of a generator run as a frame of the loop to its next yield, with the default an ended one gives (undef for none).
    NextGen(Val),
    // The `__hash__` and `__eq__` that class a key an op takes, run as frames of the loop, the op run again once the key is classed.
    Key(Box<KeyOf>),
    // A class's `__new__`, then the `__init__` of the instance it made, run as frames of the loop, the instance the call's value.
    New(Box<NewOf>),
    // The keys a class makes of the items a sort takes, each instance's `__init__` a frame of the loop, the sort run once the last is made.
    Keys(Box<ClassKeys>),
    // A builtin whose Python calls run as frames of the loop as `Calls` steps it, with the exception pending at its start (undef for none).
    Calls(Box<dyn Calls>, Val),
}

/* A sort's items and the keys a class made of them so far, its order and list, the class, its `__init__` and owner, and the instance being made. */
pub(crate) struct ClassKeys { items: Vec<Val>, keys: Vec<Val>, reverse: bool, into: Val, init: Val, defining: Val, class: Val, cur: Val }

/* A builtin whose Python calls run as frames held as a job, `step` taking the last call's value or error and naming the next call or the result. */
pub(crate) trait Calls {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<Call, VmErr>;
    /* Every value it holds, which a collection keeps. */
    fn for_each_val(&self, f: &mut dyn FnMut(Val));
}

/* What a `Calls` does next, a bound call on up to three arguments or on one plus keyword pairs, its result, or a delegation's step. */
pub(crate) enum Call { Frame(Val, Val, [Val; 3]), Named(Val, Val, Val, Vec<Val>), Give(Val), Then(u32, Result<crate::vm::globals::async_ops::Step, VmErr>) }

/* The calls a function takes before a job's frame of it counts toward lowering it, past the hooks run once per object made. */
const JOB_LOWERS: u32 = 4;

/* The arguments of a call `Call::Frame` names, those before the first undef. */
pub(crate) fn call_args(args: &[Val; 3]) -> &[Val] { &args[..args.iter().position(|a| a.is_undef()).unwrap_or(3)] }


/* An instance a job constructs, with its class, the call's positionals and keyword pairs, the instance `__new__` made and the `__init__` it runs. */
pub(crate) struct NewOf { cls: Val, few: [Val; 2], n: u8, more: Option<Box<(Vec<Val>, Vec<Val>)>>, obj: Val, init: Option<(Val, Val)> }

impl NewOf {
    /* The construction of an instance of `cls` on `pos` and the keyword pairs `kw`, the `__init__` found for it given. */
    pub(crate) fn new(cls: Val, pos: &[Val], kw: &[Val], init: Option<(Val, Val)>) -> Self {
        let mut few = [Val::undef(); 2];
        let short = kw.is_empty() && pos.len() <= 2;
        if short { few[..pos.len()].copy_from_slice(pos); }
        let more = (!short).then(|| Box::new((pos.to_vec(), kw.to_vec())));
        NewOf { cls, few, n: pos.len() as u8, more, obj: Val::undef(), init }
    }

    /* The call's positionals and keyword pairs. */
    fn args(&self) -> (&[Val], &[Val]) { match &self.more { Some(m) => (&m.0, &m.1), None => (&self.few[..self.n as usize], &[]) } }
}

/* A key a job classes, its hashes once known, the keys under that hash it may equal, the next of them, the `__eq__` running and its reflected one. */
pub(crate) struct KeyOf { v: Val, hash: Option<(i64, u64)>, reps: Vec<Val>, i: usize, eq: Val, then: (Val, Val, Val) }

impl KeyOf {
    pub(crate) fn new(v: Val) -> Self { KeyOf { v, hash: None, reps: Vec::new(), i: 0, eq: Val::undef(), then: (Val::undef(), Val::undef(), Val::undef()) } }
}

/* What the call a `Feed` runs makes of its iterator's step, its item, or a `filter`'s truth through `__bool__` (`Truth`) or `__len__` (`Truth(true)`). */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FeedKind { Map, Filter, Call, Next, Truth(bool) }

/* What taking a builtin on to a user iterable's iterator did, a frame opened, the result pushed, or an item whose dunder runs next as a frame. */
pub(crate) enum Pulled { Opened, Pushed, Fold(usize, Val, Fold, [Val; 2], usize) }

/* What a builtin of one dunder makes of the dunder's result. */
#[derive(Clone, Copy)]
pub(crate) enum Check {
    // `bool` from `__bool__`'s bool.
    Bool,
    // `bool` from `__len__`'s length past 0.
    LenTruth,
    // `len` from `__len__`'s int, no less than 0.
    Len,
    // `int` from the int `__int__` gives, a bool as its int.
    Int(Val),
    // `float` from the float `__float__` gives.
    Float(Val),
    // `abs`, whatever `__abs__` gives.
    Abs,
    // `str`, or `repr` when true, the string `__str__` or `__repr__` gives.
    Str(bool),
    // `print` of one instance, the string printed on its line.
    Print(bool),
    // `next`, what `__next__` gives, or its default (undef for none) once it raises StopIteration, with the exception pending before.
    Next(Val, Val),
    // `__index__`, the int it gives, a bool as its int.
    #[allow(dead_code)]
    Index,
    // `format`, the str `__format__` gives for the spec.
    #[allow(dead_code)]
    Format(Val),
    // `obj[key]`, whatever `__getitem__` gives for the key.
    #[allow(dead_code)]
    GetItem(Val),
    // The key a dict subclass lacks, whatever `__missing__` gives for it.
    #[allow(dead_code)]
    Missing(Val),
}

impl Check {
    pub(crate) fn dunder(self) -> &'static str {
        match self {
            Check::Bool => "__bool__", Check::LenTruth | Check::Len => "__len__", Check::Int(_) => "__int__", Check::Float(_) => "__float__",
            Check::Abs => "__abs__", Check::Str(false) | Check::Print(false) => "__str__", Check::Str(true) | Check::Print(true) => "__repr__",
            Check::Next(..) => "__next__", Check::Index => "__index__", Check::Format(_) => "__format__", Check::GetItem(_) => "__getitem__",
            Check::Missing(_) => "__missing__",
        }
    }

    /* The argument the dunder takes past its receiver, None for none. */
    pub(crate) fn arg(self) -> Option<Val> { match self { Check::Format(a) | Check::GetItem(a) | Check::Missing(a) => Some(a), _ => None } }
}

/* A foreign builtin carrying on with values dunders convert as frames, `resume` taking the last one (None at start) and asking the next or ending. */
pub(crate) trait Resume {
    fn resume(&mut self, vm: &mut VM<'_>, v: Option<Val>) -> Result<Next, VmErr>;
    /* Whether it converts few values, each dunder then run in its first code as a one-off call is, not lowered as a container's repr lowers it. */
    fn few(&self) -> bool { false }
    /* Every value it holds, which a collection keeps. */
    fn for_each_val(&self, f: &mut dyn FnMut(Val));
}

/* What a `Resume` does next, give the builtin's result, end giving nothing as a print does, or convert a value. */
pub(crate) enum Next { Give(Val), Quiet, Convert(Val, Check) }

/* What `convert_run` did, a dunder's frame opened with the job carrying on once it returns, or the builtin done with its result or nothing. */
pub(crate) enum Converted { Opened, Give(Val), Quiet }

/* The plain function `func` the dunder `name` is on instances of `class`, which `convert_on` found last, and whether the builtin converts few values. */
#[derive(Clone, Copy)]
pub(crate) struct Memo { class: Val, name: &'static str, func: Val, few: bool }

impl Memo { fn none() -> Memo { Memo { class: Val::undef(), name: "", func: Val::undef(), few: false } } }

/* Where `convert_on` stopped, at the dunder `func` it calls as a frame on the value and argument given, or at the builtin's end. */
enum Conv { Frame(usize, Val, Val, Option<Val>), Give(Val), Quiet }

/* A dunder a builtin's job calls on an item, `resume` where the generator under it carries on (`PULL` for a `__next__` frame), `next` the reflected one. */
#[derive(Clone, Copy)]
pub(crate) struct Fold { pub(crate) item: Val, next: Val, resume: u32, pub(crate) kind: Folds, item_first: bool }

/* The `resume` of a fold over the `__next__` frame of a user iterator. */
pub(crate) const PULL: usize = u32::MAX as usize;

/* The `resume` of a fold of a builtin stepping a container itself, the frame of its caller under the fold's. */
pub(crate) const NATIVE: usize = u32::MAX as usize - 1;

/* What a builtin stepping a container itself does next, run a fold's dunder as a frame, give its result, or leave the step to `drive_on`. */
pub(crate) enum Drive { Fold(usize, Val, Fold, [Val; 2], usize), Done(Val), Opened }

/* What the next step of an iterator takes first, as `peek` finds it. */
pub(crate) enum Peek { Item, End, Gen(Val), Other }

impl Fold {
    /* Where the generator under the fold's frame carries on, `PULL` for a user iterator's `__next__` frame. */
    pub(crate) fn resume(&self) -> usize { self.resume as usize }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Folds {
    // `sum`, the result is the total.
    Add,
    // `min`, or `max` when false, the item is the best so far when the result is true.
    Wins(bool),
    // `any` or `all`, the result of `__bool__` is the item's truth.
    Truth,
    // `any` or `all`, the result of `__len__` is, past 0, the item's truth.
    Len,
    // `min` or `max` with a key, the result is the item's key.
    Key,
    // `min`, or `max` when false, with a key, the item and pending key are the best so far when the result is true.
    WinsKey(bool),
    // `in`, or `not in` when true, a match by the truth of `__eq__` on the item and the value sought, or past index 0 on the pair before it.
    Eq(bool, u16),
    // `min`, or `max` when false, of two sequences (keys when `keyed`), `__eq__` on the pair before `at` walking on, `decide` then picking by `<` or `>`.
    Lex { min: bool, at: u16, decide: bool, keyed: bool },
}

/* Where `eq_scan` stopped, at a pair an instance makes, at the end with all equal, at a pair or lengths that differ, or at a pair of containers. */
enum EqScan { Dunder(usize), Equal, Differ, Deep(usize) }

/* What a fold's dunder result makes of the builtin, it takes more, it is done with its result, or the reflected dunder runs. */
pub(crate) enum FoldStep { Go, Done(Val), Call(usize, Val, Fold, [Val; 2], usize) }

/* `repr` of a container as `repr_deep` writes it, held between `__repr__` calls, its text so far, open containers outermost first, and whether it prints. */
pub(crate) struct Render { out: alloc::string::String, levels: Vec<Level>, seen: Vec<u32>, print: bool, repr: Val }

impl Resume for Render {
    fn resume(&mut self, vm: &mut VM<'_>, v: Option<Val>) -> Result<Next, VmErr> {
        if let Some(s) = v && let Some(HeapObj::Str(s)) = vm.heap.try_get(s) { self.out.push_str(s); }
        let chunk = vm.chunk;
        Ok(match vm.render_on(self, chunk)? {
            Some(inst) => Next::Convert(inst, Check::Str(true)),
            None => match vm.render_done(core::mem::take(&mut self.out), self.print)? { Some(s) => Next::Give(s), None => Next::Quiet },
        })
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { for l in &self.levels { for &v in &l.items { f(v) } } f(self.repr); }
}

/* A container `Render` writes, its items (a dict's keys and values in turn), the next one, its shape and where its body starts in the text. */
struct Level { items: Vec<Val>, at: usize, kind: Shape, start: usize }

#[derive(Clone, Copy, PartialEq)]
enum Shape { List, Tuple, Set, Frozen, Dict, FrozenDict }

/* `count`, `index` or `remove` of the list or tuple `recv`, each `__eq__` a frame, with its target, position, stop, mode, matches and pending calls. */
pub(crate) struct Scan { recv: Val, x: Val, i: usize, stop: usize, seek: Seek, hits: i64, eq: Val, then: (Val, Val, Val), pair: usize }

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Seek { Count, Index, Remove }

/* What a scan does next, call an `__eq__` on two values, or end with its result. */
enum Scanned { Call(Val, Val, Val), Done(Val) }

/* A `sorted` or `list.sort` held between its `__lt__` frames, sorting `items` into `into` (undef for a new list), `out` what a key sort places. */
pub(crate) struct Merge { pub(crate) items: Vec<Val>, pub(crate) out: Vec<Val>, runs: crate::vm::timsort::Held, order: Vec<usize>, reverse: bool, pub(crate) lt: Val, pub(crate) into: Val, done: bool, seq: Option<SeqLt> }

/* Where a sort's comparison of two sequences stands, its pair, whether it asks `__lt__` past `__eq__`, the dunder running and the reflected one. */
#[derive(Clone, Copy)]
pub(crate) struct SeqLt { k: usize, lt: bool, cur: Val, then: (Val, Val, Val) }

impl SeqLt { fn new() -> Self { SeqLt { k: 0, lt: false, cur: Val::undef(), then: (Val::undef(), Val::undef(), Val::undef()) } } }

/* What the comparison of two lists or tuples does next, a dunder run as a frame on two values or its answer. */
enum SeqNext { Call(Val, Val, Val), Less(bool) }

/* Writes the decimal digits of `n` onto `out`. */
fn push_int(out: &mut alloc::string::String, n: i64) {
    let mut buf = [0u8; 20];
    let (mut at, mut m) = (buf.len(), n.unsigned_abs());
    loop {
        at -= 1;
        buf[at] = b'0' + (m % 10) as u8;
        m /= 10;
        if m == 0 { break; }
    }
    if n < 0 { out.push('-'); }
    // SAFETY the digits are ASCII.
    out.push_str(unsafe { core::str::from_utf8_unchecked(&buf[at..]) });
}

impl Merge {
    fn new(items: Vec<Val>, out: Vec<Val>, reverse: bool, lt: Val, into: Val) -> Self {
        let order: Vec<usize> = if reverse { (0..items.len()).rev().collect() } else { (0..items.len()).collect() };
        Self { runs: crate::vm::timsort::Held::new(order), order: Vec::new(), items, out, reverse, lt, into, done: false, seq: None }
    }

    /* The two items the next comparison asks `__lt__` of. */
    fn pair(&self) -> (Val, Val) {
        let (x, y) = self.runs.pair();
        (self.items[x], self.items[y])
    }

    fn take(&mut self, less: bool) { self.runs.take(less) }

    /* Runs on to the next comparison, false once the sort is done. */
    fn advance(&mut self) -> bool {
        match self.runs.advance() {
            Some(order) => { self.order = order; if self.reverse { self.order.reverse(); } false }
            None => true,
        }
    }

    /* The items in the order the sort found. */
    fn sorted(&self) -> Vec<Val> { let from = if self.out.is_empty() { &self.items } else { &self.out }; self.order.iter().map(|&i| from[i]).collect() }
}

/* What a builtin a generator runs under makes of what it yields. */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Takes {
    // Every item, the builtin of `op` called on them as a list once the generator ends, `operand` its count.
    All(OpCode, u16),
    // `any`, the first true item ends it.
    Any,
    // `all`, the first false item ends it.
    Every,
    // `next`, the first item, or its default (undef for none) once the generator ends first.
    Next(Val),
    // `sum`, each item added as it comes as CPython 3.15 adds it.
    Sum,
    // `min`, or `max` when false, each item compared with the best so far as it comes.
    Extreme(bool),
    // `min`, or `max` when false, with a key, each item's key compared with the best key so far as it comes.
    Keyed(bool, Val),
    // `in`, or `not in` when true, the first item equal to the value ends it.
    Contains(Val, bool),
    // `close`, the GeneratorExit thrown in ends it, or an item it yields fails it.
    Close,
    // An `await` of a generator or coroutine, whose yield or return goes back to the op with `await_back` saying which.
    Await,
    // `Await` for the awaitable an `async for` steps, whose StopAsyncIteration ends the loop.
    AwaitFor,
    // A step of an async generator, whose yield or end goes back to the op awaiting it with `await_back` saying which.
    Agen,
    // A step of a generator another delegates to, whose end runs the waits from the index given in `conts`.
    Then(u32),
    // Every item, as a list put at the stack's index given over the generator there, for the builtin and argument count given to run on once it ends.
    Collect(crate::vm::types::NativeFnId, u16, u32),
    // Every item, as a list in place of the given argument of the builtin method given, which then runs on the job's own receiver and arguments.
    Method(crate::vm::methods::BuiltinMethodId, u8),
    // Every item, as a list in place of the generator at the stack's index given, the instruction that met it run again.
    Retry(u32),
    // Every item, kept in the generator for the native binding the job keeps first, then called on the values after it.
    Hold,
    // `list += it`, each item appended to the list given as it comes, as `extend` appends it, the list the op's value.
    Extend(Val),
    // A generator's next item a builtin iterator steps, kept for that step, then the `for` reruns (false) or the job under it steps on (true).
    Prefetch(bool),
}

/* How far `sum` went, ints of a machine word added exactly, then floats compensated, then any value through `+`. */
#[derive(Clone, Copy)]
pub(crate) enum Total { Word(i64), Float(f64, f64), Any }

/* A builtin over a generator, what it takes of the yields, the items gathered, and the arguments that came after the generator. */
pub(crate) struct Gather { pub(crate) takes: Takes, pub(crate) items: Vec<Val>, pub(crate) extra: Vec<Val>, pub(crate) acc: Val, pub(crate) total: Total, pub(crate) co: Val, pub(crate) before: Val, pub(crate) best_key: Val, pub(crate) pending: Val, pub(crate) cur: Option<IterFrame>, pub(crate) pull: Pull, native: bool, gens: bool, again: Again }

/* What a builtin over a Python `map` or `filter` steps, its source iterator (undef for none), whether it filters, and the item the function runs on. */
#[derive(Clone, Copy)]
pub(crate) struct Pull { pub(crate) src: Val, src2: Val, filter: bool, item: Val }

impl Gather {
    pub(crate) fn new(takes: Takes, extra: Vec<Val>) -> Self { Self { takes, items: Vec::new(), extra, acc: Val::undef(), total: Total::Any, co: Val::undef(), before: Val::undef(), best_key: Val::undef(), pending: Val::undef(), cur: None, pull: Pull { src: Val::undef(), src2: Val::undef(), filter: false, item: Val::undef() }, native: false, gens: true, again: Again::default() } }
}

/* `sorted(items, key=f)`, the keys found so far, one per item in order, and the list `list.sort` sorts in place (undef for a new one). */
pub(crate) struct Sorted { pub(crate) items: Vec<Val>, pub(crate) keys: Vec<Val>, pub(crate) reverse: bool, pub(crate) again: Again, pub(crate) into: Val }

/* The frame a job reruns its callback in, its code, chunk, first index and names it clears, 0 for one keeping bases, defaults or cells. */
#[derive(Clone, Copy)]
pub(crate) struct Again { code: *const Code, chunk: *const SSAChunk, entry: usize, names: usize }

impl Default for Again {
    fn default() -> Self { Self { code: core::ptr::null(), chunk: core::ptr::null(), entry: 0, names: 0 } }
}

/* What a job does with what its callback returned. */
pub(crate) enum Step {
    // Calls the callback again on the value.
    Again(Val),
    // Calls the callback again on the two values.
    Again2(Val, Val),
    // Calls another function on the two values, its frame in place of the callback's.
    Call2(Val, Val, Val),
    // Calls another function on the value, its frame in place of the callback's.
    Call1(Val, Val),
    // The `for` the frame returns to takes the value as its item.
    Item(Val),
    // The `for` the frame returns to steps again, the iterator under it spent.
    Redo,
    // The value is the result of the call.
    Give(Val),
    // The builtin's job under the frame steps its container on.
    Drive,
    // The op the frame's caller ran runs again, past which the frame returns.
    Rerun,
    // Calls the `__init__` of the class given on the instance given and the construction's arguments, bound to it, in place of the callback's frame.
    Bound(Val, Val, Val),
    // Calls the function given, of the class given, on the value given and the next, bound to that value, in place of the callback's frame.
    Open(Val, Val, Val, Val),
}

impl Job {
    /* The job of a `filter` whose test runs on `item`, taken from `it`. */
    pub(crate) fn filter(it: Val, item: Val, before: Val) -> Job { Job::Filter { it, item, before, f: Val::undef(), retake: false, kind: FeedKind::Filter } }

    /* The code its callback runs again in, which a collection keeps while the job lives. */
    pub(crate) fn again_code(&self) -> Option<*const Code> {
        match self { Job::Sorted(j) => Some(j.again.code), _ => None }
    }

    /* Every value the job holds, which a collection keeps. */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        match self {
            Job::Map { before, .. } => f(*before),
            Job::Feed { target, item, sentinel, .. } => { f(*target); f(*item); f(*sentinel); }
            Job::Filter { it, item, before, f: g, .. } => { f(*it); f(*item); f(*before); f(*g); }
            Job::Sorted(j) => { for &v in j.items.iter().chain(&j.keys) { f(v) } f(j.into); }
            Job::Merge(m) => { for &v in m.items.iter().chain(&m.out) { f(v) } f(m.lt); f(m.into); if let Some(q) = m.seq { f(q.cur); f(q.then.0); f(q.then.1); f(q.then.2); } }
            Job::Scan(j) => { f(j.recv); f(j.x); f(j.eq); f(j.then.0); f(j.then.1); f(j.then.2); }
            Job::Fold(d) => { f(d.item); f(d.next); }
            Job::Check(Check::Next(d, b)) => { f(*d); f(*b); }
            Job::Check(Check::Format(a) | Check::GetItem(a) | Check::Missing(a)) => f(*a),
            Job::Convert(c, r, m) => { if let Some(a) = c.arg() { f(a) } r.for_each_val(&mut |v| f(v)); f(m.class); f(m.func); }
            Job::Check(Check::Int(o) | Check::Float(o)) => f(*o),
            Job::NextGen(d) => f(*d),
            Job::Key(k) => { f(k.v); f(k.eq); f(k.then.0); f(k.then.1); f(k.then.2); for &r in &k.reps { f(r) } }
            Job::Keys(k) => { for &v in k.items.iter().chain(&k.keys) { f(v) } f(k.into); f(k.init); f(k.defining); f(k.class); f(k.cur); }
            Job::New(n) => { f(n.cls); f(n.obj); let (p, k) = n.args(); for &v in p.iter().chain(k) { f(v) } if let Some((g, d)) = n.init { f(g); f(d); } }
            Job::Calls(c, before) => { c.for_each_val(f); f(*before); }
            Job::Check(_) | Job::IterOf => {}
            Job::Gather(j) => {
                for &v in j.items.iter().chain(&j.extra) { f(v) }
                f(j.acc);
                f(j.co);
                f(j.before);
                f(j.best_key);
                f(j.pending);
                f(j.pull.src);
                f(j.pull.src2);
                f(j.pull.item);
                if let Some(c) = &j.cur { c.for_each_val(f); }
                if let Takes::Keyed(_, k) | Takes::Contains(k, _) | Takes::Extend(k) = j.takes { f(k) }
                if let Takes::Next(d) = j.takes { f(d) }
            }
        }
    }
}

impl<'a> VM<'a> {
    /* The function `f` a builtin calls with `n` arguments as a frame, `once` for a one-off callback run in its first code, None when only a nested call runs it. */
    #[inline(always)]
    pub(crate) fn job_fn_as(&mut self, f: Val, n: usize, once: bool) -> Option<usize> {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(f) else { return None };
        // Past the recursion or the memory limit the call raises as the builtin's call, no frame opened.
        if self.too_deep() { return None; }
        let facts = &self.call_facts[fi];
        let p = &self.pools[facts.pool];
        if facts.plain && facts.arity == Some(n) && p.code.is_some() && !p.cold && p.consts.is_some() && !self.is_async.get(fi).copied().unwrap_or(false) { return Some(fi); }
        self.job_fn_cold(fi, n, once)
    }

    /* `job_fn_as` for a dunder of a builtin with few conversions, keeping its first code until its calls pass `TIER` as a plain call's do. */
    #[inline(never)]
    pub(crate) fn job_fn_few(&mut self, f: Val, n: usize, few: bool) -> Option<usize> {
        if few && let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(f) {
            let facts = &self.call_facts[fi];
            let p = &self.pools[facts.pool];
            if facts.plain && facts.arity == Some(n) && p.code.is_some() && p.consts.is_some() && !self.is_async.get(fi).copied().unwrap_or(false) { return Some(fi); }
        }
        self.job_fn_as(f, n, few)
    }

    /* `job_fn_as` for a callback of one parameter the builtin calls again and again. */
    pub(crate) fn job_fn(&mut self, f: Val) -> Option<usize> { self.job_fn_as(f, 1, false) }

    /* `job_fn_as` for a callback of one parameter the builtin calls once. */
    pub(crate) fn job_fn_once(&mut self, f: Val) -> Option<usize> { self.job_fn_as(f, 1, true) }

    /* `job_fn_as` for a callback of two parameters. */
    pub(crate) fn job_fn2(&mut self, f: Val) -> Option<usize> { self.job_fn_as(f, 2, false) }

    /* `job_fn` for a body that never ran, lowered first as `lower_callees` lowers one, or one that takes defaults or cells. */
    #[inline(never)]
    fn job_fn_cold(&mut self, fi: usize, n: usize, once: bool) -> Option<usize> {
        let facts = self.call_facts[fi];
        // A body run once keeps its first code until `TIER` calls, one called again and again lowers at once, and a fresh closure fills its cells as it opens.
        let bare = facts.plain || facts.closure && facts.in_loop && facts.arity == Some(facts.required as usize) && !self.uses_super(fi);
        let warm = !once || { let p = &mut self.pools[facts.pool]; p.calls = (p.calls + p.cold as u32).min(lower::TIER); p.calls >= lower::TIER };
        let p = &self.pools[facts.pool];
        if ((once || facts.closure && bare) && p.code.is_none() || p.cold && warm) && !self.is_async.get(fi).copied().unwrap_or(false) { self.cold_code(fi); }
        let p = &self.pools[facts.pool];
        if bare && facts.arity == Some(n) && p.cold && p.code.is_some() && p.consts.is_some() { return Some(fi); }
        if p.code.is_none() && facts.in_loop && !facts.closure && !self.is_async.get(fi).copied().unwrap_or(false) {
            let body = self.functions[fi];
            if self.pools[facts.pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(&body.1, &mut self.heap) { self.pools[facts.pool].consts = Some(consts); }
            if self.pools[facts.pool].consts.is_some() { self.lowered_code(&body.1, facts.pool, false); }
        }
        self.opens_in_loop(fi, n).then_some(fi)
    }

    /* The next item of the builtin iterator `it` when it steps without user code, None for any other. */
    #[inline(always)]
    fn plain_next(&mut self, it: Val) -> Option<Result<Option<Val>, VmErr>> {
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return None };
        let frame = Rc::as_ptr(frame);
        // SAFETY the iterator is live, and a step that runs no user code frees no object.
        let mut frame = unsafe { &*frame }.try_borrow_mut().ok()?;
        Some(Ok(match &mut *frame {
            IterFrame::List { rc, idx } => {
                // SAFETY nothing writes the list while one item is copied out, as the register loop reads it.
                let v = unsafe { rc.try_borrow_unguarded() }.ok()?.get(*idx).copied();
                if v.is_some() { *idx += 1; }
                v
            }
            IterFrame::Seq { items, idx } => {
                let v = items.get(*idx).copied();
                if v.is_some() { *idx += 1; }
                v
            }
            IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. } | IterFrame::Call { .. } => return None,
            other => return Some(other.next_item(&mut self.heap)),
        }))
    }

    /* Opens `callee`, the function `fi`, on `arg` as a frame whose result `job` takes, its caller carrying on at `ret`, giving the code and chunk it runs. */
    pub(crate) fn open_job(&mut self, fi: usize, callee: Val, arg: Val, ret: usize, dest: u16, job: Job) -> (*const Code, *const SSAChunk) {
        let opened = self.open_block(fi, callee, &[arg], ret as i64 | (dest as i64) << 24 | JOB << 40);
        let opened = opened.unwrap_or_else(|| self.open_job_frame(fi, callee, arg, ret, dest));
        self.jobs.push(job);
        opened
    }

    /* `open_job` for a callback of two parameters, on `x` and `y`, its caller carrying on at `ret`. */
    pub(crate) fn open_job2(&mut self, fi: usize, callee: Val, x: Val, y: Val, ret: usize, job: Job) {
        // Its caller takes the result where the frame the handler opens sets it, on the stack until then.
        let opened = self.open_block(fi, callee, &[x, y], ret as i64 | (NO_DEST as i64) << 24 | JOB << 40);
        if opened.is_none() {
            let cost = 1 + self.heap.take_work() as u64;
            self.budget = self.budget.saturating_sub(cost);
            self.heap.set_budget(self.budget);
            let (pool, facts) = (self.fn_pool[fi], self.call_facts[fi]);
            let base = self.bind_plain(fi, callee, &Passed::Regs([x, y, Val::undef()]), 2);
            if facts.closure || !facts.plain { self.fill_frame(callee, base, 2, facts.arity.unwrap_or(2)); }
            let p = &self.pools[pool];
            if !p.laid && p.calls >= lower::TIER { self.relower(fi); }
            let code = self.pools[pool].code.as_ref().map_or(core::ptr::null(), Rc::as_ptr);
            // SAFETY the pool holds the code for as long as the VM lives.
            let cold = self.cold_bases(unsafe { (*code).flat() });
            self.push_act(Act {
                code, pool, cold, ret, dest: NO_DEST, fi, callee, call_ip: None,
                stack_base: self.stack.len(), bound: false, instance: Val::undef(), generator: Val::undef(), job: true, dunder: false,
            });
            self.run_in(pool);
            let chunk = &self.functions[fi].1 as *const SSAChunk;
            // SAFETY as above, and the chunk lives as long as the VM.
            if !self.laid(unsafe { &*code }, pool) { self.grow_frame(unsafe { &*chunk }, unsafe { &*code }, pool); }
        }
        self.jobs.push(job);
        (self.entered, self.yielded) = (true, true);
    }

    /* `repr` of the container `o`, its instances' plain `__repr__` run as frames, Ok(true) once opened, pushed or printed, Ok(false) for any other `o`. */
    #[inline(never)]
    pub(crate) fn render_job(&mut self, o: Val, print: bool, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if self.budget == 0 || self.heap.classed(o).is_some() || self.heap.foreign_repr(o).is_some() { return Ok(false); }
        if !matches!(self.heap.try_get(o), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_))) { return Ok(false); }
        let mut r = Box::new(Render { out: alloc::string::String::new(), levels: Vec::new(), seen: Vec::new(), print, repr: Val::undef() });
        self.render_open(&mut *r, o);
        if let Converted::Give(s) = self.convert_run(r, chunk)? { self.push(s); }
        Ok(true)
    }

    /* `repr` of the plain container `v` as a render writes it, its instances' `__repr__` called nested, `seen` the containers open around it. */
    pub(crate) fn render_now(&mut self, v: Val, chunk: &SSAChunk, seen: &mut Vec<u32>) -> Result<alloc::string::String, VmErr> {
        let base = seen.len();
        let mut r = Render { out: alloc::string::String::new(), levels: Vec::new(), seen: core::mem::take(seen), print: false, repr: Val::undef() };
        self.render_open(&mut r, v);
        let out = loop {
            match self.render_on(&mut r, chunk) {
                Ok(None) => break Ok(core::mem::take(&mut r.out)),
                Ok(Some(x)) => match self.convert_now(x, Check::Str(true), chunk) {
                    Ok(s) => if let Some(HeapObj::Str(s)) = self.heap.try_get(s) { r.out.push_str(s) },
                    Err(e) => break Err(e),
                },
                Err(e) => break Err(e),
            }
        };
        r.seen.truncate(base);
        *seen = r.seen;
        out
    }

    /* The text `out` a render made, printed on its line when `print`, else given as a string. */
    pub(crate) fn render_done(&mut self, out: alloc::string::String, print: bool) -> Result<Option<Val>, VmErr> {
        self.charge_steps(out.len())?;
        if print {
            let mut out = out;
            out.push('\n');
            self.write_out(&out);
            return Ok(None);
        }
        Ok(Some(self.heap.alloc(HeapObj::Str(out))?))
    }

    /* Opens the container `o` in the job's text `rp`, its opening written, an empty set or frozenset written whole. */
    fn render_open(&mut self, rp: *mut Render, o: Val) {
        let (items, kind) = match self.heap.get(o) {
            HeapObj::List(rc) => (rc.borrow().clone(), Shape::List),
            HeapObj::Tuple(t) => (t.to_vec(), Shape::Tuple),
            HeapObj::Set(s) => (s.borrow().iter().copied().collect(), Shape::Set),
            HeapObj::FrozenSet(s) => (s.iter().copied().collect(), Shape::Frozen),
            HeapObj::Dict(d) => (d.borrow().iter().flat_map(|(k, v)| [k, v]).collect(), Shape::Dict),
            HeapObj::FrozenDict(d) => (d.borrow().iter().flat_map(|(k, v)| [k, v]).collect(), Shape::FrozenDict),
            _ => return,
        };
        // SAFETY the render's box stays put while its job is on the jobs, nothing else reaching it while this runs.
        let r = unsafe { &mut *rp };
        let start = r.out.len();
        let opening = match kind {
            Shape::Set if items.is_empty() => { r.out.push_str("set()"); return; }
            Shape::Frozen if items.is_empty() => { r.out.push_str("frozenset()"); return; }
            Shape::FrozenDict if items.is_empty() => { r.out.push_str("frozendict()"); return; }
            Shape::List => "[", Shape::Tuple => "(", Shape::Set | Shape::Dict => "{", Shape::Frozen => "frozenset({", Shape::FrozenDict => "frozendict({",
        };
        r.out.push_str(opening);
        r.seen.push(o.as_heap());
        r.levels.push(Level { items, at: 0, kind, start });
    }

    /* Writes the list or tuple `o` whole onto `out` when it holds only values off the heap, false with nothing written for any other. */
    fn render_flat(&self, out: &mut alloc::string::String, o: Val) -> bool {
        let write = |out: &mut alloc::string::String, items: &[Val], tuple: bool| {
            if items.iter().any(|v| v.is_heap()) { return false; }
            let start = out.len();
            out.push(if tuple { '(' } else { '[' });
            for (i, &x) in items.iter().enumerate() {
                if i > 0 {
                    if !(tuple && items.len() == 1) && out.len() - start > super::value_ops::MAX_REPR_LEN { out.push_str(", ..."); break; }
                    out.push_str(", ");
                }
                if x.is_int() && !x.is_bool() { push_int(out, x.as_int()); } else { out.push_str(&self.repr(x)); }
            }
            out.push_str(if !tuple { "]" } else if items.len() == 1 { ",)" } else { ")" });
            true
        };
        match self.heap.get(o) { HeapObj::List(rc) => write(out, &rc.borrow(), false), HeapObj::Tuple(t) => write(out, t, true), _ => false }
    }

    /* Writes the job's text `rp` on until an instance whose `__repr__` the job's frame runs, which it gives, None once the text is whole. */
    pub(crate) fn render_on(&mut self, rp: *mut Render, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        use super::value_ops::{MAX_REPR_LEN, RENDER_DEPTH_MAX};
        loop {
            // SAFETY as in `render_open`, the reference ending before any call that runs user code.
            let r = unsafe { &mut *rp };
            let Some(level) = r.levels.last_mut() else { return Ok(None) };
            let (i, kind, n) = (level.at, level.kind, level.items.len());
            let checks = i > 0 && i < n && match kind { Shape::Dict | Shape::FrozenDict => i % 2 == 0, Shape::Tuple => n != 1, _ => true };
            if checks && r.out.len() - level.start > MAX_REPR_LEN { r.out.push_str(", ..."); level.at = n; }
            if level.at >= n {
                let closing = match kind { Shape::List => "]", Shape::Tuple if n == 1 => ",)", Shape::Tuple => ")", Shape::Set | Shape::Dict => "}", Shape::Frozen | Shape::FrozenDict => "})" };
                r.out.push_str(closing);
                r.levels.pop();
                r.seen.pop();
                continue;
            }
            if i > 0 { r.out.push_str(if matches!(kind, Shape::Dict | Shape::FrozenDict) && i % 2 == 1 { ": " } else { ", " }); }
            let x = level.items[i];
            level.at += 1;
            if x.is_int() && !x.is_bool() { push_int(unsafe { &mut (*rp).out }, x.as_int()); continue; }
            if !x.is_heap() { let s = self.repr(x); unsafe { (*rp).out.push_str(&s); } continue; }
            if let Some(HeapObj::Str(s)) = self.heap.try_get(x) && self.heap.classed(x).is_none() { super::value_ops::push_repr_str(unsafe { &mut (*rp).out }, s); continue; }
            if self.heap.classed(x).is_none() && self.heap.foreign_repr(x).is_none() && matches!(self.heap.try_get(x), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_))) {
                if r.seen.contains(&x.as_heap()) {
                    r.out.push_str(match self.heap.get(x) { HeapObj::Dict(_) | HeapObj::Set(_) => "{...}", HeapObj::Tuple(_) => "(...)", HeapObj::FrozenSet(_) => "frozenset({...})", HeapObj::FrozenDict(_) => "frozendict({...})", _ => "[...]" });
                } else if r.seen.len() > RENDER_DEPTH_MAX { r.out.push_str("..."); } else if !self.render_flat(unsafe { &mut (*rp).out }, x) { self.render_open(rp, x); }
                continue;
            }
            // An instance's own `__repr__` converts it, as a frame of the loop when a plain function, the class of the last one kept.
            if let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(x) && (class.0 == r.repr.0 || self.lookup_class_member(class, "__repr__").is_some()) {
                // SAFETY as above.
                unsafe { (*rp).repr = class; }
                return Ok(Some(x));
            }
            // SAFETY as above, the containers open taken out while user code may run and put back after.
            let mut seen = core::mem::take(unsafe { &mut (*rp).seen });
            let s = self.repr_deep(x, chunk, &mut seen);
            let r = unsafe { &mut *rp };
            r.seen = seen;
            r.out.push_str(&s?);
        }
    }

    /* The plain `__bool__` of instance `v` a job runs as a frame and its function, or past it its `__len__` (true then), None for anything else. */
    fn bool_dunder(&mut self, v: Val) -> Option<(Val, usize, bool)> {
        let class = self.instance_class(v)?;
        let (b, len) = match self.lookup_class_member(class, "__bool__") { Some((b, _)) => (b, false), None => (self.lookup_class_member(class, "__len__")?.0, true) };
        self.job_fn(b).map(|fi| (b, fi, len))
    }

    /* `count`, `index` or `remove` of the list or tuple `recv` on `pos` as job frames when an item or target is an instance, Ok(false) otherwise. */
    #[inline(never)]
    pub(crate) fn scan_job(&mut self, recv: Val, name: &str, pos: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        if self.budget == 0 || pos.is_empty() { return Ok(false); }
        let len = match self.heap.try_get(recv) { Some(HeapObj::List(rc)) => rc.borrow().len(), Some(HeapObj::Tuple(t)) => t.len(), _ => return Ok(false) };
        // Only a walk meeting an instance calls Python, any other is the method's own.
        let x = pos[0];
        let any = |vm: &Self, v: Val| matches!(vm.heap.try_get(v), Some(HeapObj::Instance(..)));
        let seen = any(self, x) || match self.heap.try_get(recv) { Some(HeapObj::List(rc)) => rc.borrow().iter().any(|&v| any(self, v)), Some(HeapObj::Tuple(t)) => t.iter().any(|&v| any(self, v)), _ => false };
        if !seen { return Ok(false); }
        self.heap.spend(len);
        let n = len as i64;
        let as_i = |v: Val| -> i64 { if v.is_bool() { v.as_bool() as i64 } else { v.as_int() } };
        let norm = |v: Val| (if as_i(v) < 0 { n + as_i(v) } else { as_i(v) }).clamp(0, n) as usize;
        let (seek, start, stop) = match name {
            "count" => (Seek::Count, 0, len),
            "index" => {
                let start = pos.get(1).filter(|v| v.is_int() || v.is_bool()).map_or(0, |&v| norm(v));
                (Seek::Index, start, pos.get(2).filter(|v| v.is_int() || v.is_bool()).map_or(len, |&v| norm(v)).max(start))
            }
            _ => (Seek::Remove, 0, len),
        };
        let mut j = Box::new(Scan { recv, x, i: start, stop, seek, hits: 0, eq: Val::undef(), then: (Val::undef(), Val::undef(), Val::undef()), pair: 0 });
        match self.scan_walk(&mut j, chunk)? {
            Scanned::Done(r) => { self.push(r); Ok(true) }
            Scanned::Call(g, a, b) => {
                let Some(fi) = self.job_fn2(g) else { return Err(cold_runtime("a scan's __eq__ is no function")) };
                j.eq = g;
                self.open_job2(fi, g, a, b, 0, Job::Scan(j));
                Ok(true)
            }
        }
    }

    /* The step calling `call`'s function on its two values, the frame running again when it is the one the scan runs. */
    fn scan_step(&mut self, j: &mut Scan, call: (Val, Val, Val)) -> Step {
        if call.0.0 == j.eq.0 { return Step::Again2(call.1, call.2); }
        j.eq = call.0;
        Step::Call2(call.0, call.1, call.2)
    }

    /* Takes the answer of the comparison the scan ran, `hit` whether the pair was equal, and walks on. */
    fn scan_on(&mut self, j: &mut Scan, mut hit: bool, chunk: &SSAChunk) -> Result<Scanned, VmErr> {
        // A pair of a walk that was equal walks on to the next pair of the item and what the scan seeks.
        let from = core::mem::take(&mut j.pair);
        if from > 0 && hit && let Some(v) = self.scan_item(j) {
            match self.scan_pairs(j, v, from)? { Ok(hit_now) => hit = hit_now, Err(call) => return Ok(call) }
        }
        if hit && let Some(r) = self.scan_hit(j)? { return Ok(Scanned::Done(r)); }
        j.i += 1;
        self.scan_walk(j, chunk)
    }

    /* What a match at the scan's item does, the result it ends with, None while it counts on. */
    fn scan_hit(&mut self, j: &mut Scan) -> Result<Option<Val>, VmErr> {
        Ok(match j.seek {
            Seek::Count => { j.hits += 1; None }
            Seek::Index => Some(Val::int(j.i as i64)),
            Seek::Remove => {
                let i = j.i;
                if let Some(HeapObj::List(rc)) = self.heap.try_get(j.recv) { let mut l = rc.borrow_mut(); if i < l.len() { l.remove(i); } }
                Some(Val::none())
            }
        })
    }

    /* Walks the scan from its item, comparing as `member_eq` does, to the next `__eq__` it calls as a frame or to its result. */
    fn scan_walk(&mut self, j: &mut Scan, chunk: &SSAChunk) -> Result<Scanned, VmErr> {
        while let Some(v) = self.scan_item(j) {
            let hit = if v.0 == j.x.0 { true } else {
                match self.scan_call(j, v, j.x) {
                    Some((g, a, b)) => return Ok(Scanned::Call(g, a, b)),
                    None if self.seq_vals(v).is_some() && self.seq_vals(j.x).is_some() => match self.scan_pairs(j, v, 0)? { Ok(hit) => hit, Err(call) => return Ok(call) },
                    None => self.member_eq(v, j.x, chunk)?,
                }
            };
            if hit && let Some(r) = self.scan_hit(j)? { return Ok(Scanned::Done(r)); }
            j.i += 1;
        }
        match j.seek {
            Seek::Count => Ok(Scanned::Done(Val::int(j.hits))),
            Seek::Index if matches!(self.heap.try_get(j.recv), Some(HeapObj::Tuple(_))) => Err(cold_value("tuple.index(x): x not in tuple")),
            Seek::Index => Err(cold_value("list.index(x): x not in list")),
            Seek::Remove => Err(cold_value("list.remove(x): x not in list")),
        }
    }

    /* The item the scan is at, read live, as a `__eq__` that shrinks the list ends the walk early. */
    fn scan_item(&self, j: &Scan) -> Option<Val> {
        if j.i >= j.stop { return None; }
        match self.heap.try_get(j.recv) {
            Some(HeapObj::List(rc)) => rc.borrow().get(j.i).copied(),
            Some(HeapObj::Tuple(t)) => t.get(j.i).copied(),
            _ => None,
        }
    }

    /* The walk from pair `from` of sequences `v` and the scan's target, Ok with whether they are equal, or Err with the next pair's `__eq__` as a frame. */
    fn scan_pairs(&mut self, j: &mut Scan, v: Val, from: usize) -> Result<Result<bool, Scanned>, VmErr> {
        Ok(Ok(match self.eq_scan(v, j.x, from) {
            EqScan::Equal => true,
            EqScan::Differ => false,
            EqScan::Dunder(k) => {
                let Some((a, b)) = self.eq_pair(v, j.x, k) else { return Ok(Ok(false)) };
                if let Some((g, a, b)) = self.scan_call(j, a, b) { j.pair = k + 1; return Ok(Err(Scanned::Call(g, a, b))); }
                self.with_roots([v], |vm| vm.eq_rest(v, j.x, k))?
            }
            EqScan::Deep(k) => self.with_roots([v], |vm| vm.eq_rest(v, j.x, k))?,
        }))
    }

    /* The `__eq__` call comparing `a` with `b` in `dispatch_reflected`'s order, the reflected one kept for a decline, None when it compares itself. */
    fn scan_call(&mut self, j: &mut Scan, a: Val, b: Val) -> Option<(Val, Val, Val)> {
        let (ac, bc) = (self.instance_class(a), self.instance_class(b));
        if ac.is_none() && bc.is_none() { return None; }
        // A subclass's own comparison on the right runs first, as `dispatch_reflected` orders it.
        let b_first = matches!((ac, bc), (Some(x), Some(y)) if x.0 != y.0 && self.heap.is_subclass(y, x));
        let order = if b_first { [(b, a), (a, b)] } else { [(a, b), (b, a)] };
        let eq_of = |vm: &mut Self, recv: Val| -> Result<Option<Val>, ()> {
            let Some(f) = vm.instance_class(recv).and_then(|c| vm.lookup_class_member(c, "__eq__")).map(|m| m.0) else { return Ok(None) };
            if matches!(vm.heap.try_get(f), Some(HeapObj::Func(..))) && vm.job_fn2(f).is_some() { Ok(Some(f)) } else { Err(()) }
        };
        let (f0, f1) = (eq_of(self, order[0].0).ok()?, eq_of(self, order[1].0).ok()?);
        match (f0, f1) {
            (Some(g), Some(h)) => { j.then = (h, order[1].0, order[1].1); Some((g, order[0].0, order[0].1)) }
            (Some(g), None) => Some((g, order[0].0, order[0].1)),
            (None, Some(h)) => Some((h, order[1].0, order[1].1)),
            (None, None) => None,
        }
    }

    /* Sorts `items` as `sorted` or `list.sort` into `into` (undef for new), a plain `__lt__` of one class run as frames, Err with the items otherwise. */
    #[inline(never)]
    pub(crate) fn merge_job(&mut self, items: Vec<Val>, reverse: bool, into: Val) -> Result<(), Vec<Val>> {
        if let Some((fi, lt)) = self.merge_lt(&items) {
            let mut m = Merge::new(items, Vec::new(), reverse, lt, into);
            m.advance();
            let (x, y) = m.pair();
            self.open_job2(fi, lt, x, y, 0, Job::Merge(Box::new(m)));
            return Ok(());
        }
        // Lists or tuples holding instances compare item by item, each dunder a frame of the job, the native pairs at once.
        if !self.merge_seqs(&items) { return Err(items); }
        let mut m = Box::new(Merge::new(items, Vec::new(), reverse, Val::undef(), into));
        m.advance();
        m.seq = Some(SeqLt::new());
        let chunk = self.chunk;
        match self.merge_walk(&mut m, None, chunk) {
            Ok(Some((g, a, b))) => match self.job_fn2(g) {
                Some(fi) => { self.open_job2(fi, g, a, b, 0, Job::Merge(m)); Ok(()) }
                None => Err(core::mem::take(&mut m.items)),
            },
            // A sort no dunder ended, or one that raised, sorts the nested way, which meets the same comparisons again.
            _ => Err(core::mem::take(&mut m.items)),
        }
    }

    /* Whether `items` are lists, or tuples, of which one holds an instance a sort compares through its dunders. */
    fn merge_seqs(&self, items: &[Val]) -> bool {
        if items.len() < 2 || self.budget == 0 || !self.heap.users { return false; }
        let list = matches!(self.heap.try_get(items[0]), Some(HeapObj::List(_)));
        let mut inst = false;
        for &v in items {
            match self.heap.try_get(v) {
                Some(HeapObj::List(x)) if list => inst |= x.borrow().iter().any(|&e| self.is_instance(e)),
                Some(HeapObj::Tuple(x)) if !list => inst |= x.iter().any(|&e| self.is_instance(e)),
                _ => return false,
            }
        }
        inst
    }

    /* Item `k` of the list or tuple `v` as it is now. */
    fn seq_at(&self, v: Val, k: usize) -> Option<Val> {
        match self.heap.try_get(v) { Some(HeapObj::List(x)) => x.borrow().get(k).copied(), Some(HeapObj::Tuple(x)) => x.get(k).copied(), _ => None }
    }

    /* The length of the list or tuple `v` as it is now. */
    fn seq_len_of(&self, v: Val) -> usize {
        match self.heap.try_get(v) { Some(HeapObj::List(x)) => x.borrow().len(), Some(HeapObj::Tuple(x)) => x.len(), _ => 0 }
    }

    /* Carries the sort of sequences `m` on from `answer`, what its last dunder gave, to the next dunder it runs as a frame, None once the sort is done. */
    fn merge_walk(&mut self, m: &mut Merge, mut answer: Option<Val>, chunk: &SSAChunk) -> Result<Option<(Val, Val, Val)>, VmErr> {
        loop {
            let (x, y) = m.pair();
            let Some(mut st) = m.seq else { return Err(cold_runtime("a sort of sequences without its walk")) };
            let next = self.seq_lt(x, y, &mut st, answer.take(), chunk)?;
            m.seq = Some(st);
            match next {
                SeqNext::Call(g, a, b) => { if let Some(q) = m.seq.as_mut() { q.cur = g; } return Ok(Some((g, a, b))); }
                SeqNext::Less(less) => {
                    self.charge_step()?;
                    m.take(less);
                    m.seq = Some(SeqLt { cur: st.cur, ..SeqLt::new() });
                    if !m.advance() { m.done = true; return Ok(None); }
                }
            }
        }
    }

    /* `x < y` of sequences from where `st` stands, as `walk_lt` makes it, each pair's `__eq__` until one differs then its `__lt__`, as frames. */
    fn seq_lt(&mut self, x: Val, y: Val, st: &mut SeqLt, answer: Option<Val>, chunk: &SSAChunk) -> Result<SeqNext, VmErr> {
        let none = (Val::undef(), Val::undef(), Val::undef());
        if let Some(v) = answer {
            if self.heap.is_not_implemented(v) && !st.then.0.is_undef() {
                let t = core::mem::replace(&mut st.then, none);
                return Ok(SeqNext::Call(t.0, t.1, t.2));
            }
            let (a, b) = (self.seq_at(x, st.k).unwrap_or(Val::undef()), self.seq_at(y, st.k).unwrap_or(Val::undef()));
            if st.lt {
                if self.heap.is_not_implemented(v) { return Ok(SeqNext::Less(self.lt_vals(a, b)?)); }
                return Ok(SeqNext::Less(self.truthy(v)));
            }
            let equal = !self.heap.is_not_implemented(v) && self.truthy(v);
            if equal { st.k += 1; } else { st.lt = true; }
        }
        loop {
            let (Some(a), Some(b)) = (self.seq_at(x, st.k), self.seq_at(y, st.k)) else { return Ok(SeqNext::Less(self.seq_len_of(x) < self.seq_len_of(y))) };
            let user = self.is_instance(a) || self.is_instance(b);
            if !st.lt {
                if a.0 == b.0 { st.k += 1; continue; }
                if !user {
                    match crate::value::eq::eq_checked(a, b, &self.heap, 1) {
                        Some(true) => { st.k += 1; continue; }
                        Some(false) => return Ok(SeqNext::Less(self.with_roots([a, b], |vm| vm.values_lt(a, b, chunk))?)),
                        None => { if self.with_roots([a, b], |vm| vm.values_eq(a, b, chunk))? { st.k += 1; continue; } st.lt = true; }
                    }
                } else {
                    let (plan, n) = self.reflected_plan(a, b, "__eq__", "__eq__", false);
                    if n > 0 && plan[..n].iter().all(|c| self.job_fn2(c.0).is_some()) {
                        st.then = if n > 1 { plan[1] } else { none };
                        return Ok(SeqNext::Call(plan[0].0, plan[0].1, plan[0].2));
                    }
                    if n > 0 && self.with_roots([a, b], |vm| vm.values_eq(a, b, chunk))? { st.k += 1; continue; }
                    st.lt = true;
                }
            }
            // The first pair that differs decides.
            if !user { return Ok(SeqNext::Less(self.with_roots([a, b], |vm| vm.values_lt(a, b, chunk))?)); }
            let (plan, n) = self.reflected_plan(a, b, "__lt__", "__gt__", false);
            if n > 0 && plan[..n].iter().all(|c| self.job_fn2(c.0).is_some()) {
                st.then = if n > 1 { plan[1] } else { none };
                return Ok(SeqNext::Call(plan[0].0, plan[0].1, plan[0].2));
            }
            return Ok(SeqNext::Less(self.with_roots([a, b], |vm| vm.sort_lt(a, b, chunk))?));
        }
    }

    /* Sorts `items` by their `keys` in place of the job on top once it found the last, its comparisons frames of the loop when they run Python. */
    fn sort_keyed(&mut self, items: Vec<Val>, keys: Vec<Val>, reverse: bool, into: Val) -> Result<Step, VmErr> {
        let chunk = self.chunk;
        // Keys whose `__lt__` runs Python, or sequences holding instances, sort as a job whose comparisons are frames of the loop.
        if let Some((_, lt)) = self.merge_lt(&keys) {
            let mut m = Merge::new(keys, items, reverse, lt, into);
            m.advance();
            let (x, y) = m.pair();
            if let Some(j) = self.jobs.last_mut() { *j = Job::Merge(Box::new(m)); }
            self.charge_step()?;
            return Ok(Step::Call2(lt, x, y));
        }
        if self.merge_seqs(&keys) {
            let mut m = Box::new(Merge::new(keys, items, reverse, Val::undef(), into));
            m.advance();
            m.seq = Some(SeqLt::new());
            let next = self.merge_walk(&mut m, None, chunk)?;
            if let Some(j) = self.jobs.last_mut() { *j = Job::Merge(m); }
            return match next { Some((g, a, b)) => Ok(Step::Call2(g, a, b)), None => self.merge_end() };
        }
        let order = self.sorted_order(&keys, &items, reverse, chunk)?;
        let sorted: Vec<Val> = order.into_iter().map(|i| items[i]).collect();
        if let Some(HeapObj::List(rc)) = self.heap.try_get(into) { *rc.borrow_mut() = sorted; return Ok(Step::Give(Val::none())); }
        Ok(Step::Give(self.alloc_list(sorted)?))
    }

    /* Starts sorting `items` into `into` (undef for new) by keys a class makes, each `__init__` a job frame, false when the class cannot run so. */
    pub(crate) fn class_keys(&mut self, class: Val, items: &[Val], reverse: bool, into: Val) -> Result<bool, VmErr> {
        let Some(&first) = items.first() else { return Ok(false) };
        if !matches!(self.heap.try_get(class), Some(HeapObj::Class(..))) || self.native_base(class).is_some() || self.special_class(class) || self.exc_base(class).is_some() { return Ok(false); }
        let Some((init, defining)) = self.lookup_class_member(class, "__init__") else { return Ok(false) };
        if self.new_fn(init, 2).is_none() { return Ok(false); }
        let inst = self.new_instance(class, 0)?;
        let job = Job::Keys(Box::new(ClassKeys { items: items.to_vec(), keys: Vec::with_capacity(items.len()), reverse, into, init, defining, class, cur: inst }));
        self.open_new(init, inst, &[first], &[], (defining, inst), job)
    }

    /* The `__init__` and class of a class key a builtin makes keys with, None for any other key or a class that constructs its own way. */
    fn key_init(&self, key: Val) -> Option<(Val, Val)> {
        if !matches!(self.heap.try_get(key), Some(HeapObj::Class(..))) || self.native_base(key).is_some() || self.special_class(key) || self.exc_base(key).is_some() { return None; }
        let (init, _) = self.lookup_class_member(key, "__init__")?;
        matches!(self.heap.try_get(init), Some(HeapObj::Func(..))).then_some((init, key))
    }

    /* What the job finding a class's keys makes of `v`, an `__init__`'s result, the next `__init__` or the sort by the keys. */
    #[inline(never)]
    fn keys_take(&mut self, v: Val) -> Result<Step, VmErr> {
        self.charge_step()?;
        if !v.is_none() { return Err(VmErr::TypeMsg(crate::s!("__init__() should return None, not '", str self.type_name(v), "'"))); }
        let Some(Job::Keys(k)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
        k.keys.push(k.cur);
        if let Some(&next) = k.items.get(k.keys.len()) {
            let (init, defining, class) = (k.init, k.defining, k.class);
            let inst = self.new_instance(class, 0)?;
            if let Some(Job::Keys(k)) = self.jobs.last_mut() { k.cur = inst; }
            return Ok(Step::Open(init, defining, inst, next));
        }
        let (items, keys, reverse, into) = (core::mem::take(&mut k.items), core::mem::take(&mut k.keys), k.reverse, k.into);
        self.sort_keyed(items, keys, reverse, into)
    }

    /* The end of the sort the job on top ran, the list it sorts given its order or a new one made. */
    fn merge_end(&mut self) -> Result<Step, VmErr> {
        let Some(Job::Merge(m)) = self.jobs.last() else { return Err(cold_runtime("a job frame without its job")) };
        let (sorted, into) = (m.sorted(), m.into);
        if let Some(HeapObj::List(rc)) = self.heap.try_get(into) {
            let rc = rc.clone();
            *rc.borrow_mut() = sorted;
            return Ok(Step::Give(Val::none()));
        }
        Ok(Step::Give(self.alloc_list(sorted)?))
    }

    /* The `__lt__` and its function that `merge_job` runs as frames, when every item is an instance of one class with a plain one. */
    fn merge_lt(&mut self, items: &[Val]) -> Option<(usize, Val)> {
        if items.len() < 2 || self.budget == 0 { return None; }
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(items[0]) else { return None };
        if !items.iter().all(|&v| matches!(self.heap.try_get(v), Some(&HeapObj::Instance(c, _)) if c.0 == class.0)) { return None; }
        let (lt, _) = self.lookup_class_member(class, "__lt__")?;
        self.job_fn2(lt).map(|fi| (fi, lt))
    }

    /* `open_job` for a function its pool's first window does not hold. */
    #[inline(never)]
    fn open_job_frame(&mut self, fi: usize, callee: Val, arg: Val, ret: usize, dest: u16) -> (*const Code, *const SSAChunk) {
        // The call pays a step as any call does, a budget it spends failing at the next one.
        let cost = 1 + self.heap.take_work() as u64;
        self.budget = self.budget.saturating_sub(cost);
        self.heap.set_budget(self.budget);
        let (pool, facts) = (self.fn_pool[fi], self.call_facts[fi]);
        let base = self.bind_plain(fi, callee, &Passed::Regs([arg, Val::undef(), Val::undef()]), 1);
        if facts.closure { self.fill_frame(callee, base, 1, facts.arity.unwrap_or(1)); }
        let p = &self.pools[pool];
        if !p.laid && p.calls >= lower::TIER { self.relower(fi); }
        let code = self.pools[pool].code.as_ref().map_or(core::ptr::null(), Rc::as_ptr);
        // SAFETY the pool holds the code for as long as the VM lives.
        let cold = self.cold_bases(unsafe { (*code).flat() });
        self.push_act(Act {
            code, pool, cold, ret, dest, fi, callee, call_ip: None,
            stack_base: self.stack.len(), bound: false, instance: Val::undef(), generator: Val::undef(), job: true, dunder: false,
        });
        self.run_in(pool);
        let chunk = &self.functions[fi].1 as *const SSAChunk;
        // SAFETY as above, and the chunk lives as long as the VM.
        if !self.laid(unsafe { &*code }, pool) { self.grow_frame(unsafe { &*chunk }, unsafe { &*code }, pool); }
        (code, chunk)
    }

    /* Reruns the job's callback on `arg` in the frame of `fi`, `callee` in `pool` over a stack `stack_base` long, `bases` when it keeps them. */
    #[inline]
    pub(crate) fn rearm(&mut self, pool: usize, fi: usize, callee: Val, stack_base: usize, bases: bool, arg: Val) {
        debug_assert!(pool < self.pools.len() && fi < self.call_facts.len());
        // A callback that ran its first call in its first code runs the rest lowered.
        if self.pools[pool].cold { self.warm_up(fi, pool); }
        // SAFETY a running frame's pool is one of `pools` and its function one of `call_facts`.
        let (base, facts) = unsafe { (self.pools.get_unchecked(pool).window, self.call_facts.get_unchecked(fi)) };
        let (names, refill, arity) = (facts.names as usize, !facts.plain || facts.arity != Some(1), facts.arity.unwrap_or(1));
        debug_assert!(base + names <= self.regs.len());
        // SAFETY the window holds the names of its function.
        unsafe {
            let r = self.regs.as_mut_ptr().add(base);
            *r = arg;
            for k in 1..names { *r.add(k) = Val::undef(); }
        }
        if refill { self.fill_frame(callee, base, 1, arity); }
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        if self.stack.len() > stack_base { self.stack.truncate(stack_base); }
        if bases && let Some(b) = self.frame_bases.last() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
    }

    /* `rearm` for a callback of two parameters, run again on the pair `(x, y)`. */
    #[inline(never)]
    pub(crate) fn rearm2(&mut self, pool: usize, fi: usize, callee: Val, stack_base: usize, bases: bool, (x, y): (Val, Val)) {
        if self.pools[pool].cold { self.warm_up(fi, pool); }
        let (base, facts) = (self.pools[pool].window, self.call_facts[fi]);
        let (names, arity) = (facts.names as usize, facts.arity.unwrap_or(2));
        let r = &mut self.regs[base..base + names.max(2)];
        (r[0], r[1]) = (x, y);
        for v in r.iter_mut().skip(2) { *v = Val::undef(); }
        if !facts.plain || arity != 2 { self.fill_frame(callee, base, 2, arity); }
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        if self.stack.len() > stack_base { self.stack.truncate(stack_base); }
        if bases && let Some(b) = self.frame_bases.last() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
    }

    /* The job on top's take of its callback's result `v` in the loop, the next step, or None with nothing changed for user code, raises or charged work. */
    #[inline(always)]
    fn job_fast(&mut self, v: Val) -> Option<Step> {
        // Another call pays a step, as a call does.
        let pays = self.budget > 0 && !self.heap.has_work();
        let step = match self.jobs.last_mut()? {
            Job::Map { ctor: false, .. } => return Some(Step::Item(v)),
            Job::Sorted(j) => {
                let next = *j.items.get(j.keys.len() + 1).filter(|_| pays)?;
                j.keys.push(v);
                Step::Again(next)
            }
            &mut Job::Filter { it, item, f, .. } => {
                if !f.is_undef() { return None; }
                if self.plain_truth(v)? { return Some(Step::Item(item)); }
                if !pays { return None; }
                // A failure sticks, so `job_take` meets it again.
                match self.plain_next(it)?.ok()? {
                    Some(x) => { if let Some(Job::Filter { item, .. }) = self.jobs.last_mut() { *item = x; } Step::Again(x) }
                    None => Step::Redo,
                }
            }
            // A user iterator's `__next__` the job calls again on the iterator while the builtin takes more.
            Job::Gather(j) if j.pull.src.is_undef() => { let it = j.co; return self.gather_fast(v).then_some(Step::Again(it)); }
            Job::Gather(_) => return None,
            Job::Merge(m) if v.is_bool() && pays && !m.done && m.seq.is_none() => {
                m.take(v.as_bool());
                // A sort done gives its list through `job_take`, which allocates, the comparison already taken.
                if !m.advance() { m.done = true; return None; }
                let (x, y) = m.pair();
                Step::Again2(x, y)
            }
            &mut Job::Check(c) => {
                let fine = match c {
                    Check::Bool => v.is_bool(),
                    Check::Abs | Check::Next(..) => true,
                    Check::Int(_) => v.is_int(),
                    Check::Float(_) => v.is_float(),
                    Check::Len => v.is_int() && v.as_int() >= 0,
                    Check::Str(_) => matches!(self.heap.try_get(v), Some(HeapObj::Str(_))),
                    Check::Index => v.is_int(),
                    Check::GetItem(_) | Check::Missing(_) => true,
                    Check::LenTruth | Check::Print(_) | Check::Format(_) => false,
                };
                return fine.then_some(Step::Give(v));
            }
            Job::Merge(_) | Job::Fold(_) | Job::IterOf | Job::NextGen(_) | Job::Scan(_) | Job::Convert(..) | Job::Feed { .. } | Job::Map { .. } | Job::Key(_) | Job::New(_) | Job::Keys(_) | Job::Calls(..) => return None,
        };
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        Some(step)
    }

    /* What the job on top makes of `v`, its callback's result, the next step, with user code run and errors raised. */
    #[inline(never)]
    pub(crate) fn job_take(&mut self, v: Val) -> Result<Step, VmErr> {
        let chunk = self.chunk;
        match self.jobs.last() {
            Some(&Job::Feed { target, item, sentinel, kind, drive }) => {
                // A constructor's `__init__` gives the instance it built, and a `filter`'s test giving an instance runs its `__bool__` as the next frame.
                let v = self.top().map_or(v, |f| if f.instance.is_undef() { v } else { f.instance });
                if kind == FeedKind::Filter && let Some((b, _, len)) = self.bool_dunder(v) {
                    if let Some(Job::Feed { kind, .. }) = self.jobs.last_mut() { *kind = FeedKind::Truth(len); }
                    self.charge_step()?;
                    return Ok(Step::Call1(b, v));
                }
                self.feed_take(target, item, sentinel, kind, v, chunk)?;
                Ok(if drive { Step::Drive } else { Step::Redo })
            }
            Some(&Job::Map { ctor, .. }) => Ok(Step::Item(if ctor { self.top().map_or(v, |f| f.instance) } else { v })),
            Some(&Job::Filter { it, item, f, retake, kind, .. }) => {
                // A constructor's `__init__` gives the instance it built, and a test the `for` calls again takes its next item through its step.
                let v = if retake && kind == FeedKind::Filter { self.top().map_or(v, |t| if t.instance.is_undef() { v } else { t.instance }) } else { v };
                let keep = if let FeedKind::Truth(len) = kind {
                    // What the `__bool__` or `__len__` the test gave answered, the test's function called next.
                    if let Some(Job::Filter { f, kind, .. }) = self.jobs.last_mut() { (*f, *kind) = (if retake { *f } else { Val::undef() }, FeedKind::Filter); }
                    self.truth_of(v, len)?
                } else if let Some((b, _, len)) = self.bool_dunder(v) {
                    // An instance's `__bool__` runs as a frame of the loop in place of the test's.
                    let g = if retake { f } else { self.top().map_or(Val::undef(), |t| t.callee) };
                    if let Some(Job::Filter { f, kind, .. }) = self.jobs.last_mut() { (*f, *kind) = (g, FeedKind::Truth(len)); }
                    self.charge_step()?;
                    return Ok(Step::Call1(b, v));
                } else {
                    self.push(v);
                    let keep = self.truthy_op(v, chunk);
                    self.pop()?;
                    keep?
                };
                if keep { return Ok(Step::Item(item)); }
                self.charge_step()?;
                if retake { return Ok(Step::Redo); }
                match self.plain_next(it) {
                    Some(Ok(Some(x))) => {
                        if let Some(Job::Filter { item, .. }) = self.jobs.last_mut() { *item = x; }
                        // The test's function runs again, in place of the `__bool__` that ran last.
                        if !f.is_undef() { return Ok(Step::Call1(f, x)); }
                        Ok(Step::Again(x))
                    }
                    // A failure sticks, so the `for` meets it again and raises it.
                    _ => Ok(Step::Redo),
                }
            }
            Some(Job::Sorted(j)) => {
                if let Some(&next) = j.items.get(j.keys.len() + 1) {
                    self.charge_step()?;
                    if let Some(Job::Sorted(j)) = self.jobs.last_mut() { j.keys.push(v); }
                    return Ok(Step::Again(next));
                }
                // The last key sorts, which compares through user code.
                let Some(Job::Sorted(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                let (items, mut keys, reverse, into) = (core::mem::take(&mut j.items), core::mem::take(&mut j.keys), j.reverse, j.into);
                keys.push(v);
                self.sort_keyed(items, keys, reverse, into)
            }
            Some(Job::Keys(_)) => self.keys_take(v),
            Some(Job::Gather(j)) if !j.pull.src.is_undef() => self.pull_take(v, chunk),
            Some(Job::Gather(j)) => {
                let it = j.co;
                Ok(match self.gather_take(v)? { None => Step::Again(it), Some(r) => Step::Give(r) })
            }
            Some(Job::Scan(_)) => {
                let Some(Job::Scan(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                let then = core::mem::replace(&mut j.then, (Val::undef(), Val::undef(), Val::undef()));
                let jp: *mut Scan = &mut **j;
                // A declined `__eq__` asks the other side's, the pair unequal once both declined, as a distinct pair is.
                let hit = if self.heap.is_not_implemented(v) {
                    if !then.0.is_undef() { self.charge_step()?; return Ok(self.scan_step(unsafe { &mut *jp }, then)); }
                    false
                } else { self.truthy(v) };
                self.charge_step()?;
                // SAFETY the job lives on `jobs` while its frame runs, and the walk pushes no job.
                Ok(match self.scan_on(unsafe { &mut *jp }, hit, chunk)? { Scanned::Call(g, a, b) => self.scan_step(unsafe { &mut *jp }, (g, a, b)), Scanned::Done(r) => Step::Give(r) })
            }
            Some(Job::Merge(m)) if m.seq.is_some() && !m.done => {
                let Some(Job::Merge(m)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                let mp: *mut Merge = &mut **m;
                // SAFETY the job lives on `jobs` while its frame runs, and the walk pushes no job.
                match self.merge_walk(unsafe { &mut *mp }, Some(v), chunk)? {
                    Some((g, a, b)) => {
                        let same = self.top().is_some_and(|t| t.callee.0 == g.0);
                        Ok(if same { Step::Again2(a, b) } else { Step::Call2(g, a, b) })
                    }
                    None => self.merge_end(),
                }
            }
            Some(Job::Merge(m)) => {
                if !m.done {
                    // An answer other than a bool is its truth, NotImplemented the comparison as the nested sort makes it.
                    let less = if v.is_bool() { v.as_bool() } else if v.is_heap() && matches!(self.heap.get(v), HeapObj::NotImplemented) {
                        let (x, y) = m.pair();
                        self.sort_lt(x, y, chunk)?
                    } else { self.truthy(v) };
                    self.charge_step()?;
                    let Some(Job::Merge(m)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                    m.take(less);
                    if m.advance() { let (x, y) = m.pair(); return Ok(Step::Again2(x, y)); }
                    m.done = true;
                }
                self.merge_end()
            }
            Some(&Job::Check(Check::Print(repr))) => {
                let mut s = self.require_str(v, if repr { "__repr__" } else { "__str__" })?;
                self.charge_steps(s.len())?;
                s.push('\n');
                self.write_out(&s);
                // A print leaves nothing, as its statement leaves nothing.
                Ok(Step::Redo)
            }
            Some(&Job::Check(c)) => Ok(Step::Give(self.checked(c, v)?)),
            Some(&Job::Convert(c, _, Memo { few, .. })) => {
                let r = self.checked(c, v)?;
                Ok(match self.convert_on(Some(r), chunk)? {
                    Conv::Frame(_, func, x, arg) => {
                        // The running dunder reruns in place, others in its stead, but few conversions reopen it in first code, which a rerun would lower.
                        let same = !few && self.top().is_some_and(|t| t.callee.0 == func.0);
                        match (arg, same) { (None, true) => Step::Again(x), (None, false) => Step::Call1(func, x), (Some(a), true) => Step::Again2(x, a), (Some(a), false) => Step::Call2(func, x, a) }
                    }
                    Conv::Give(x) => Step::Give(x),
                    Conv::Quiet => Step::Redo,
                })
            }
            Some(Job::Key(_)) => self.key_take(v),
            Some(Job::New(_)) => self.new_take(v),
            Some(Job::Fold(_) | Job::IterOf | Job::NextGen(_) | Job::Calls(..)) | None => Err(cold_runtime("a job frame without its job")),
        }
    }

    /* Where the caller of a job's frame carries on after `step`, `code` the code it runs, `ret` and `dest` where its frame returns to. */
    pub(crate) fn job_done(&mut self, step: Step, code: &Code, ret: usize, dest: u16) -> usize {
        match step {
            // A `for` the frame returns to takes its item in the register its frame names, or as its own step would.
            Step::Item(v) if dest != NO_DEST => self.deliver(v, ret, dest),
            Step::Item(v) => { self.hand_to_for(code, ret, v); ret + 1 }
            Step::Redo | Step::Again(_) | Step::Again2(..) | Step::Call2(..) | Step::Call1(..) | Step::Drive => ret,
            Step::Give(v) => self.deliver(v, ret, dest),
            Step::Rerun => { self.redo_rerun(ret - 1); ret - 1 }
            Step::Bound(..) | Step::Open(..) => ret,
        }
    }

    /* The next `__eq__` the job classing a key runs after its last frame gave `v`, or the op run again once the key is classed. */
    #[inline(never)]
    fn key_take(&mut self, v: Val) -> Result<Step, VmErr> {
        self.charge_step()?;
        let Some(Job::Key(k)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
        let (key, hash, then, eq) = (k.v, k.hash, k.then, k.eq);
        let (h, d) = match hash {
            None => {
                let Some(hd) = self.key_hash(key, v)? else { return Ok(self.key_done(key)) };
                let reps = self.key_candidates(key, hd.1);
                let Some(Job::Key(k)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                (k.hash, k.reps) = (Some(hd), reps);
                hd
            }
            Some(hd) => {
                // A declined `__eq__` asks the other side's, the pair unequal once both declined.
                let r = self.key_rep();
                let equal = if self.heap.is_not_implemented(v) {
                    if !then.0.is_undef() {
                        if let Some(Job::Key(k)) = self.jobs.last_mut() { (k.then, k.eq) = ((Val::undef(), Val::undef(), Val::undef()), then.0); }
                        return Ok(if then.0.0 == eq.0 { Step::Again2(then.1, then.2) } else { Step::Call2(then.0, then.1, then.2) });
                    }
                    false
                } else { self.truthy(v) };
                if equal { let class = self.heap.user_key(r).map(|u| u.class); self.key_join(key, hd.0, hd.1, class); return Ok(self.key_done(key)); }
                if let Some(Job::Key(k)) = self.jobs.last_mut() { k.i += 1; }
                hd
            }
        };
        self.key_walk(key, h, d)
    }

    /* Opens a construction's `__new__` or `__init__` on `args` as a frame with `job`, bound to `binding` when it names `super`, false with no frame. */
    pub(crate) fn open_new(&mut self, func: Val, first: Val, pos: &[Val], kw: &[Val], binding: (Val, Val), job: Job) -> Result<bool, VmErr> {
        if !kw.is_empty() || pos.len() > 2 { return self.open_new_keyed(func, first, pos, kw, binding, job); }
        let args = [first, pos.first().copied().unwrap_or(Val::undef()), pos.get(1).copied().unwrap_or(Val::undef())];
        let args = &args[..1 + pos.len()];
        let Some(fi) = self.new_fn(func, args.len()) else { return self.open_new_keyed(func, first, pos, kw, binding, job) };
        let bound = self.uses_super(fi);
        let link = (NO_DEST as i64) << 24 | (JOB | if bound { super::act::BOUND } else { 0 }) << 40;
        if self.open_block_as(fi, func, args, link, bound).is_some() {
            if bound { self.bindings.push((self.level(), binding.0, binding.1)); }
        } else {
            let (pool, facts) = (self.fn_pool[fi], self.call_facts[fi]);
            let mut regs = [Val::undef(); 3];
            regs[..args.len()].copy_from_slice(args);
            let base = self.bind_plain(fi, func, &Passed::Regs(regs), args.len());
            if facts.closure || !facts.plain { self.fill_frame(func, base, args.len(), facts.arity.unwrap_or(args.len())); }
            let code = self.pools[pool].code.as_ref().map_or(core::ptr::null(), Rc::as_ptr);
            // SAFETY the pool holds the code for as long as the VM lives.
            let cold = self.cold_bases(unsafe { (*code).flat() });
            if bound { self.bindings.push((self.level() + 1, binding.0, binding.1)); }
            self.push_act(Act {
                code, pool, cold, ret: 0, dest: NO_DEST, fi, callee: func, call_ip: None,
                stack_base: self.stack.len(), bound, instance: Val::undef(), generator: Val::undef(), job: true, dunder: false,
            });
            self.run_in(pool);
            let chunk = &self.functions[fi].1 as *const SSAChunk;
            // SAFETY as above, and the chunk lives as long as the VM.
            if !self.laid(unsafe { &*code }, pool) { self.grow_frame(unsafe { &*chunk }, unsafe { &*code }, pool); }
        }
        self.jobs.push(job);
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* `open_new` of a call that names its arguments or passes many, bound as any call through the stack binds them. */
    #[inline(never)]
    fn open_new_keyed(&mut self, func: Val, first: Val, pos: &[Val], kw: &[Val], binding: (Val, Val), job: Job) -> Result<bool, VmErr> {
        if self.near_limit() || !self.pending_instance.is_undef() { return Ok(false); }
        // A body that never ran, or ran only in its first code, opens in that code as the general call binds it, a heated one lowered first.
        if let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) && !self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false)
            && (self.pools[self.fn_pool[fi]].code.is_none() || self.pools[self.fn_pool[fi]].cold && self.pools[self.fn_pool[fi]].calls + 1 < lower::TIER) && self.link_of(fi).is_none() {
            if self.pools[self.fn_pool[fi]].code.is_none() { self.cold_code(fi); }
            if !self.runs_in_loop(fi, self.frame_safe) { return Ok(false); }
            return self.open_new_bound(func, first, pos, kw, binding, job);
        }
        if let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) && self.pools[self.fn_pool[fi]].cold { self.relower(fi); }
        let at = self.stack.len();
        self.stack.push(func);
        self.stack.push(first);
        self.stack.extend_from_slice(pos);
        self.stack.extend_from_slice(kw);
        self.pending.method_binding = Some(binding);
        if !self.open_keyed(1 + pos.len(), kw.len() / 2)? {
            self.stack.truncate(at);
            self.pending.method_binding = None;
            return Ok(false);
        }
        self.mark_job();
        self.jobs.push(job);
        Ok(true)
    }

    /* `open_new_keyed` of a body in its first code, its arguments bound as a general call binds them, its frame the job's. */
    #[inline(never)]
    fn open_new_bound(&mut self, func: Val, first: Val, pos: &[Val], kw: &[Val], binding: (Val, Val), job: Job) -> Result<bool, VmErr> {
        let Some(HeapObj::Func(fi, defaults, captures, _)) = self.heap.try_get(func) else { return Ok(false) };
        let (fi, defaults, captures) = (*fi, defaults.clone(), captures.clone());
        self.charge_step()?;
        let mut args = Vec::with_capacity(1 + pos.len());
        args.push(first);
        args.extend_from_slice(pos);
        let pool = self.fn_pool[fi];
        let names = self.functions[fi].1.names.len();
        let base = self.open_window(pool, names + self.functions[fi].1.constants.len() + 3, names, true);
        if let Err(e) = self.bind_function_args(fi, &defaults, &args, kw, base).and_then(|()| self.enter_scope(fi, &captures, base)) {
            self.close_window(pool);
            return Err(e);
        }
        self.pending.method_binding = Some(binding);
        self.enter_frame(fi, func)?;
        self.mark_job();
        self.jobs.push(job);
        Ok(true)
    }

    /* Runs the builtin `run` stands for, its Python calls frames when `frames`, giving its result, or None with its first frame opened. */
    pub(crate) fn calls_run(&mut self, run: Box<dyn Calls>, frames: bool) -> Result<Option<Val>, VmErr> {
        let before = self.pending.exc_val.unwrap_or(Val::undef());
        let frames = frames && !self.redo_replays();
        self.jobs.push(Job::Calls(run, before));
        self.calls_go(None, frames)
    }

    /* Steps the `Calls` job on top from `got` (None at start) to its result with the job taken off, or to None with its next frame opened. */
    pub(crate) fn calls_go(&mut self, got: Option<Result<Val, VmErr>>, frames: bool) -> Result<Option<Val>, VmErr> {
        let call = self.calls_step(got);
        self.calls_from(call, frames)
    }

    /* What the `Calls` job on top does after `got`, the job taken off when it fails. */
    pub(crate) fn calls_step(&mut self, got: Option<Result<Val, VmErr>>) -> Result<Call, VmErr> {
        let took = matches!(got, Some(Err(_)));
        let Some(Job::Calls(run, before)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
        let before = *before;
        let p: *mut dyn Calls = &mut **run;
        // SAFETY the job's box stays put while the job is on the jobs, and `step` leaves the jobs past it as it found them.
        let call = match unsafe { (*p).step(self, got) } {
            Ok(c) => c,
            Err(e) => { self.jobs.pop(); return Err(e); }
        };
        // An error the builtin took leaves the exception pending as it began, a delegation's waits taking theirs.
        if took && !matches!(call, Call::Then(..)) {
            self.pending.exc_val = Some(before).filter(|b| !b.is_undef());
            self.error_byte_pos = None;
            self.call_stack.clear();
        }
        Ok(call)
    }

    /* `calls_go` from `call`, what the job on top asked for. */
    pub(crate) fn calls_from(&mut self, call: Result<Call, VmErr>, frames: bool) -> Result<Option<Val>, VmErr> {
        let mut call = call?;
        loop {
            let called = match call {
                Call::Give(v) => { self.jobs.pop(); self.entered = false; return Ok(Some(v)); }
                Call::Then(base, step) => { self.jobs.pop(); self.entered = false; self.then_out = Some((base, step)); return Ok(Some(Val::none())); }
                Call::Frame(g, class, args) => self.job_call(g, class, call_args(&args), &[], frames),
                Call::Named(g, class, x, kw) => self.job_call(g, class, &[x], &kw, frames),
            };
            match called {
                None => {
                    // The frame opened is the job's, which leaving it takes off.
                    let at = self.head_at(self.top_pool);
                    self.regs[at] = Val::int(self.head(at).as_int() | JOB << 40);
                    (self.entered, self.yielded) = (true, true);
                    return Ok(None);
                }
                Some(r) => call = self.calls_step(Some(r))?,
            }
        }
    }

    /* Makes the running frame, a `Calls` job's, no longer its job's, so leaving it keeps the job for its next step. */
    pub(crate) fn calls_keep(&mut self) {
        let at = self.head_at(self.top_pool);
        self.regs[at] = Val::int(self.head(at).as_int() & !(JOB << 40));
    }

    /* Calls `g` on `args` and pairs `kw`, bound to `class` and `args[0]` unless undef, None with its frame opened when `frames` allows, else its nested result. */
    fn job_call(&mut self, g: Val, class: Val, args: &[Val], kw: &[Val], frames: bool) -> Option<Result<Val, VmErr>> {
        let chunk = self.chunk;
        let func = match self.heap.try_get(g) { Some(&HeapObj::BoundUserMethod(_, f, _)) => f, _ => g };
        let fi = match self.heap.try_get(func) {
            Some(&HeapObj::Func(fi, ..)) if !self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false) && self.link_of(fi).is_none() => Some(fi),
            _ => None,
        };
        let opens = frames && self.budget != 0 && fi.is_some();
        if !class.is_undef() { self.pending.method_binding = Some((class, args[0])); }
        if !opens { return Some(self.call_with(g, None, args, kw, chunk).and_then(|()| self.pop())); }
        // A call a rerun op would replay is made afresh, and one called under `JOB_LOWERS` times stays unlowered, as a per-object hook would.
        let (on, lazy) = (self.redo.on, self.redo.lazy);
        (self.redo.on, self.redo.lazy) = (false, false);
        (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
        self.entered = false;
        let pool = fi.map_or(0, |fi| self.fn_pool[fi]);
        let keep = (self.pools[pool].code.is_none() || self.pools[pool].cold) && self.pools[pool].calls < JOB_LOWERS;
        let ran = self.pools[pool].ran;
        if keep { self.pools[pool].ran = false; }
        self.push(g);
        self.stack.extend_from_slice(args);
        self.stack.extend_from_slice(kw);
        let r = self.exec_call_n(args.len(), kw.len() / 2, chunk);
        (self.inline_call, self.pending_exec_safe) = (false, false);
        (self.redo.on, self.redo.lazy) = (on, lazy);
        if keep { self.pools[pool].ran = ran; }
        match r {
            Ok(()) if core::mem::take(&mut self.entered) => {
                self.yielded = false;
                None
            }
            Ok(()) => Some(self.pop()),
            Err(e) => Some(Err(e)),
        }
    }

    /* The function `f` of exactly `n` parameters a construction opens as a frame, one naming `super` too, None for any other. */
    fn new_fn(&mut self, f: Val, n: usize) -> Option<usize> {
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(f) else { return None };
        if self.too_deep() || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return None; }
        let facts = self.call_facts[fi];
        if !facts.in_loop || facts.closure || facts.arity != Some(n) || facts.required as usize != n { return None; }
        // A body that never ran runs in its code as the compiler wrote it, as a first call does.
        if self.pools[facts.pool].code.is_none() || self.pools[facts.pool].cold { self.cold_code(fi); }
        let p = &self.pools[facts.pool];
        (p.code.is_some() && p.consts.is_some()).then_some(fi)
    }

    /* The `__init__` the job constructing an instance runs after `__new__` gave `v`, or the instance as the call's value. */
    #[inline(never)]
    fn new_take(&mut self, v: Val) -> Result<Step, VmErr> {
        self.charge_step()?;
        let Some(Job::New(n)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
        let (cls, obj, init) = (n.cls, n.obj, n.init);
        if !obj.is_undef() {
            if !v.is_none() { return Err(VmErr::TypeMsg(crate::s!("__init__() should return None, not '", str self.type_name(v), "'"))); }
            return Ok(Step::Give(obj));
        }
        // `__init__` runs only on what `__new__` made of this class, the one the class's call found for an instance of the class itself.
        let made = self.instance_class(v).filter(|&c| c.0 == cls.0 || self.heap.is_subclass(c, cls));
        let Some(init) = (match made { Some(c) if c.0 == cls.0 => init, Some(c) => self.lookup_class_member(c, "__init__").or(init), None => None }) else { return Ok(Step::Give(v)) };
        if let Some(Job::New(n)) = self.jobs.last_mut() { n.obj = v; }
        Ok(Step::Bound(init.0, init.1, v))
    }

    /* Opens the `__init__` `g` of `class` on the instance `x` the construction `job` made as its frame, false once it ran nested without one. */
    pub(crate) fn open_init(&mut self, g: Val, class: Val, x: Val, job: Job) -> Result<bool, VmErr> {
        let Job::New(n) = &job else { return Ok(false) };
        let (few, k, more) = (n.few, n.n as usize, n.more.as_ref().map(|m| (m.0.clone(), m.1.clone())));
        let (pos, kw): (&[Val], &[Val]) = match &more { Some(m) => (&m.0, &m.1), None => (&few[..k], &[]) };
        if self.open_new(g, x, pos, kw, (class, x), job)? { return Ok(true); }
        // One that takes no frame of the job runs nested, as the job's last step.
        let chunk = self.chunk;
        let v = self.with_roots([x], |vm| { vm.pending.method_binding = Some((class, x)); vm.call_with(g, Some(x), pos, kw, chunk)?; vm.pop() })?;
        if !v.is_none() { return Err(VmErr::TypeMsg(crate::s!("__init__() should return None, not '", str self.type_name(v), "'"))); }
        Ok(false)
    }

    /* The `__eq__` calls of two keys in the order Python makes them, one class's own looked up once for two of its instances. */
    fn key_eq_plan(&self, a: Val, b: Val) -> ([(Val, Val, Val); 2], usize) {
        let none = (Val::undef(), Val::undef(), Val::undef());
        if let (Some(&HeapObj::Instance(c, _)), Some(&HeapObj::Instance(d, _))) = (self.heap.try_get(a), self.heap.try_get(b)) && c.0 == d.0 {
            return match self.lookup_class_member(c, "__eq__") {
                Some((g, _)) if matches!(self.heap.try_get(g), Some(HeapObj::Func(..))) => ([(g, a, b), (g, b, a)], 2),
                _ => ([none; 2], 0),
            };
        }
        self.reflected_plan(a, b, "__eq__", "__eq__", false)
    }

    /* The key the job classing a key compares with now. */
    fn key_rep(&self) -> Val { match self.jobs.last() { Some(Job::Key(k)) => k.reps.get(k.i).copied().unwrap_or(Val::undef()), _ => Val::undef() } }

    /* Walks the keys the job's key may equal from the next, to the `__eq__` it runs as a frame or to the key's class. */
    fn key_walk(&mut self, key: Val, hash: i64, dict: u64) -> Result<Step, VmErr> {
        let chunk = self.chunk;
        loop {
            let r = self.key_rep();
            if r.is_undef() { self.key_join(key, hash, dict, None); return Ok(self.key_done(key)); }
            // The `__eq__` each side defines runs as a frame in the order Python calls them, a pair whose dunders take no frame nested.
            let (plan, n) = self.key_eq_plan(key, r);
            let running = matches!(self.jobs.last(), Some(Job::Key(k)) if k.eq.0 == plan[0].0.0);
            if n > 0 && (running || self.job_fn2(plan[0].0).is_some()) && (n < 2 || plan[1].0.0 == plan[0].0.0 || self.job_fn2(plan[1].0).is_some()) {
                let Some(Job::Key(k)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                let (g, a, b) = plan[0];
                let same = k.eq.0 == g.0;
                (k.eq, k.then) = (g, if n > 1 { plan[1] } else { (Val::undef(), Val::undef(), Val::undef()) });
                return Ok(if same { Step::Again2(a, b) } else { Step::Call2(g, a, b) });
            }
            if self.with_roots([key, r], |vm| vm.values_eq(key, r, chunk))? {
                let class = self.heap.user_key(r).map(|u| u.class);
                self.key_join(key, hash, dict, class);
                return Ok(self.key_done(key));
            }
            if let Some(Job::Key(k)) = self.jobs.last_mut() { k.i += 1; }
        }
    }

    /* Ends the job that classed `key`, the op that asked for it running again and taking it classed. */
    fn key_done(&mut self, key: Val) -> Step {
        let acts = self.acts.saturating_sub(1);
        self.keyed_now.push((key, acts));
        Step::Rerun
    }

    /* The job's take of its callback's result `v`, Ok with the code, chunk and index to rerun in place, Err with the `for`'s item, None for the full path. */
    #[inline(always)]
    pub(crate) fn job_again(&mut self, v: Val) -> Option<Result<(*const Code, *const SSAChunk, usize), Val>> {
        // Another call pays a step, native work waiting to be charged taking the full path.
        let pays = self.budget != 0 && !self.heap.has_work();
        let (next, again) = match self.jobs.last_mut()? {
            Job::Map { ctor: false, .. } => return Some(Err(v)),
            &mut Job::Filter { it, item, f, .. } => {
                if !f.is_undef() { return None; }
                if self.plain_truth(v)? { return Some(Err(item)); }
                let again = self.again_in_place().filter(|_| pays)?;
                // A spent iterator, or one that failed, takes the full path, which ends the loop or raises.
                let next = self.plain_next(it)?.ok()??;
                if let Some(Job::Filter { item, .. }) = self.jobs.last_mut() { *item = next; }
                (next, again)
            }
            Job::Sorted(j) if j.again.names != 0 && pays => {
                let &next = j.items.get(j.keys.len() + 1)?;
                j.keys.push(v);
                (next, j.again)
            }
            // An int key of `min` or `max` over a container compares at once, the key found again in place on its next item.
            &mut Job::Fold(d) if d.kind == Folds::Key && d.resume() == NATIVE && v.is_int() && pays => {
                let n = self.jobs.len();
                if n < 2 { return None; }
                let jobs = self.jobs.as_mut_ptr();
                // SAFETY the job under the fold's is in bounds, and nothing else reaches the jobs while this runs.
                let Job::Gather(j) = (unsafe { &mut *jobs.add(n - 2) }) else { return None };
                let Takes::Keyed(is_min, _) = j.takes else { return None };
                let best = j.best_key;
                if !(best.is_undef() || best.is_int()) { return None; }
                // The frame of the key runs again as it did, found once.
                if j.again.names == 0 { j.again = self.again_in_place()?; }
                let next = j.cur.as_mut()?.next_ready(&self.heap)?;
                if best.is_undef() || if is_min { v.as_int() < best.as_int() } else { v.as_int() > best.as_int() } { (j.acc, j.best_key) = (d.item, v); }
                // SAFETY as above, the fold's job on top.
                if let Job::Fold(f) = unsafe { &mut *jobs.add(n - 1) } { f.item = next; }
                (next, j.again)
            }
            _ => return None,
        };
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        let base = self.base;
        debug_assert!(base + again.names <= self.regs.len());
        // SAFETY the running frame's window holds its names, its parameter the first.
        unsafe {
            let r = self.regs.as_mut_ptr().add(base);
            *r = next;
            for k in 1..again.names { *r.add(k) = Val::undef(); }
        }
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        Some(Ok((again.code, again.chunk, again.entry)))
    }

    /* Lowers the body of `fi`, whose job frame on top ran its first code, the frame running the lowered code from here in a window laid for it. */
    #[cold]
    #[inline(never)]
    fn warm_up(&mut self, fi: usize, pool: usize) {
        let f = self.functions[fi];
        let code = self.lowered_code(&f.1, pool, false);
        let at = self.head_at(self.top_pool);
        self.regs[at + 4] = Val::int(Rc::as_ptr(&code) as usize as i64);
        if !self.laid(&code, pool) { self.grow_frame(&f.1, &code, pool); }
        self.refresh_again(Rc::as_ptr(&code));
    }

    /* Takes `code`, which the job frame on top runs now in place of the code its again names, as the code it runs again. */
    #[cold]
    #[inline(never)]
    pub(crate) fn refresh_again(&mut self, code: *const Code) {
        let at = self.head_at(self.top_pool);
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        let again = self.again_of(fi, code, &self.functions[fi].1);
        let n = self.jobs.len();
        match self.jobs.last_mut() {
            Some(Job::Sorted(j)) => j.again = again,
            // The key of a fold the frame runs is the job's under its own.
            Some(Job::Fold(_)) => if let Some(Job::Gather(j)) = self.jobs.get_mut(n - 2) { j.again = again; },
            _ => {}
        }
    }

    /* How the running job frame, a plain flat frame of a function of one parameter, runs again in place, None for any other. */
    #[inline(never)]
    fn again_in_place(&self) -> Option<Again> {
        let at = self.head_at(self.top_pool);
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        let code = self.head(at + 4).as_int() as usize as *const Code;
        let again = self.again_of(fi, code, &self.functions[fi].1);
        (again.names != 0).then_some(again)
    }

    /* What a job frame of `fi` that `open_job` just opened runs again in place. */
    fn again_of(&self, fi: usize, code: *const Code, chunk: *const SSAChunk) -> Again {
        let facts = &self.call_facts[fi];
        // SAFETY the pool holds the code for as long as the VM lives.
        let c = unsafe { &*code };
        // One in its first code runs again the full way, which lowers it for the rest.
        let names = if facts.plain && facts.arity == Some(1) && c.flat() && !self.pools[facts.pool].cold { facts.names as usize } else { 0 };
        Again { code, chunk, entry: c.lowered(0), names }
    }

    /* Ends a return of `v` from the job frame at `at` (first register `link`) as `leave_site` does, rerunning the callback or resuming the caller. */
    #[inline(never)]
    pub(crate) fn leave_job(&mut self, v: Val, at: usize, link: i64, floor: &Floor) -> Option<(*const Code, *const SSAChunk, usize)> {
        let bases = link >> 40 & BASES != 0;
        // A frame that left a handler or a cleanup takes the full path.
        if bases && let Some(b) = self.frame_bases.last() && (self.exception_stack.len() > b.exc || self.unwind_stack.len() > b.unwind) { return None; }
        let step = self.job_fast(v)?;
        let (pool, below) = (self.top_pool, self.head(at + 1).as_int());
        let fi = (below >> 24) as usize;
        if let Step::Again(_) | Step::Again2(..) = step {
            match step {
                Step::Again(arg) => self.rearm(pool, fi, self.head(at + 2), self.head(at + 3).as_int() as usize, bases, arg),
                Step::Again2(x, y) => self.rearm2(pool, fi, self.head(at + 2), self.head(at + 3).as_int() as usize, bases, (x, y)),
                _ => {}
            }
            // A lowering the rearm made may have moved the window, its record with it.
            let code = self.head(self.head_at(self.top_pool) + 4).as_int() as usize as *const Code;
            debug_assert!(fi < self.functions.len());
            // SAFETY the pool holds the code for as long as the VM lives, and the frame's function is one of `functions`.
            return Some((code, unsafe { &self.functions.get_unchecked(fi).1 } as *const SSAChunk, unsafe { &*code }.lowered(0)));
        }
        if bases && let Some(b) = self.frame_bases.pop() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        self.pop_act();
        self.close_window(pool);
        let below = (below & 0xFF_FFFF) as usize;
        let (code, chunk) = if self.acts > floor.depth {
            self.run_in(below);
            let h = self.head_at(below);
            let fi = (self.head(h + 1).as_int() >> 24) as usize;
            (self.head(h + 4).as_int() as usize as *const Code, &self.functions[fi].1 as *const SSAChunk)
        } else {
            self.run_in(floor.pool);
            (floor.code, floor.chunk)
        };
        // SAFETY the caller runs code its pool or the floor keeps.
        Some((code, chunk, self.job_done(step, unsafe { &*code }, (link & 0xFF_FFFF) as usize, (link >> 24) as u16)))
    }

    /* The `for` at `i` steps the iterator stack's `map` or `filter` as `map_step` or `for_job` do, entering its frame as the register loop gives way. */
    #[inline(never)]
    pub(crate) fn step_called(&mut self, i: usize, ins: super::lower::Ins) {
        self.entered = if ins.x == 0 { self.map_step(i, ins.b) } else { self.for_job(i, None) }.is_some();
    }

    /* A `for` at `i` stepping a plain function's `map` over a sequence into `reg` opens it on the next item returning straight to `reg`, others via `for_job`. */
    #[inline(always)]
    fn map_step(&mut self, i: usize, reg: u16) -> Option<(*const Code, *const SSAChunk)> {
        let (f, it, filter) = match self.iter_stack.last() {
            Some(IterFrame::Map { f, its, .. }) if its.len() == 1 => (*f, its[0], false),
            Some(&IterFrame::Filter { f, it, .. }) if !f.is_none() => (f, it, true),
            _ => return self.for_job(i, Some(reg)),
        };
        if self.budget == 0 || self.pending.exc_val.is_some() { return self.for_job(i, Some(reg)); }
        let Some(fi) = self.job_fn(f) else { return self.for_job(i, Some(reg)) };
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return self.for_job(i, Some(reg)) };
        // SAFETY nothing holds the iterator's state while it steps.
        let Some(item) = unsafe { &mut *frame.as_ptr() }.next_ready(&self.heap) else { return self.for_job(i, Some(reg)) };
        // A `map` returns straight to the register, a `filter` through its job, which tests what it returns.
        let link = i as i64 | (reg as i64) << 24 | if filter { JOB << 40 } else { 0 };
        let job = if filter { Job::filter(it, item, Val::undef()) } else { Job::Map { before: Val::undef(), ctor: false } };
        match self.open_block(fi, f, &[item], link) {
            Some(opened) => { if filter { self.jobs.push(job); } Some(opened) }
            None => Some(self.open_job(fi, f, item, i, reg, job)),
        }
    }

    /* Ends the job frame on a plain return of a register or the stack top as `regs_return` does, where it carries on and its code and chunk. */
    #[inline(never)]
    pub(crate) fn job_return(&mut self, ins: super::lower::Ins, floor: &Floor) -> Option<(usize, *const Code, *const SSAChunk)> {
        let (at, stacked) = (self.head_at(self.top_pool), ins.op != crate::parser::OpCode::ReturnR);
        let v = if !stacked { self.regs[self.pools[self.top_pool].window + ins.b as usize] } else if self.stack.len() > self.top_stack_base() { self.stack.pop()? } else { return None };
        let left = self.leave_job(v, at, self.head(at).as_int(), floor);
        if left.is_none() && stacked { self.stack.push(v); }
        left.map(|(c, k, next)| (next, c, k))
    }

    /* A `for` at `i` stepping a `map` or `filter` over a plain iterator opens its function on the next item as a frame, None when its handler steps it. */
    #[inline(never)]
    pub(crate) fn for_job(&mut self, i: usize, reg: Option<u16>) -> Option<(*const Code, *const SSAChunk)> {
        let (f, it, filter) = match self.iter_stack.last() {
            Some(IterFrame::Map { f, its, .. }) if its.len() == 1 => (*f, its[0], false),
            Some(&IterFrame::Filter { f, it, .. }) if !f.is_none() => (f, it, true),
            _ => return None,
        };
        if self.budget == 0 { return None; }
        let fi = self.job_fn(f)?;
        // A spent iterator, or one that failed, ends or raises in the handler.
        let Ok(Some(item)) = self.plain_next(it)? else { return None };
        // A StopIteration it raises gives back the exception pending now, which a plain frame keeps nowhere.
        let before = self.pending.exc_val;
        if !filter && before.is_none() && let Some(r) = reg && let Some(opened) = self.open_block(fi, f, &[item], i as i64 | (r as i64) << 24) { return Some(opened); }
        let before = before.unwrap_or(Val::undef());
        let job = if filter { Job::filter(it, item, before) } else { Job::Map { before, ctor: false } };
        Some(self.open_job(fi, f, item, i, reg.unwrap_or(NO_DEST), job))
    }

    /* Opens the callback `key` of a builtin call as a job on `first`, the call carrying on where the frame the handler opens sets it. */
    pub(crate) fn key_job(&mut self, fi: usize, key: Val, first: Val, job: Job) {
        let (code, chunk) = self.open_job(fi, key, first, 0, NO_DEST, job);
        let again = self.again_of(fi, code, chunk);
        if let Some(Job::Sorted(j)) = self.jobs.last_mut() { j.again = again; }
        self.entered = true;
        self.yielded = true;
    }

    /* Runs generator `co` for the builtin that took it as `takes` says, `extra` the arguments after, as a frame once lowered, Ok(false) for its own path. */
    #[inline(never)]
    pub(crate) fn gather_job(&mut self, co: Val, takes: Takes, extra: Vec<Val>) -> Result<bool, VmErr> { self.gen_job(co, takes, extra, false) }

    /* `gather_job`, for a coroutine or an async generator too when `coro`, the step its call or awaitable makes. */
    pub(crate) fn gen_job(&mut self, co: Val, takes: Takes, extra: Vec<Val>, coro: bool) -> Result<bool, VmErr> {
        if self.budget == 0 { return Ok(false); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return Ok(false) };
        let BodyRef::Fn(fi) = c.body else { return Ok(false) };
        let (generator, is_async) = (self.functions[fi].1.is_generator, self.is_async_fn(fi));
        if !(generator && !is_async || coro && is_async) { return Ok(false); }
        let pool = self.call_facts[fi].pool;
        let job = match takes { Takes::Next(d) => Job::NextGen(d), _ => Job::Gather(Box::new(self.gather_of(takes, extra, co))) };
        self.jobs.push(job);
        let jobs = self.jobs.len();
        if self.pools[pool].code.is_none() { self.gen_cold_code(fi); }
        // A recursion through builtins over generators ends as deep as nested resumes would, a job counting as the builtin's call and the resume under it.
        if 2 * self.jobs.len() + self.depth >= self.max_calls { self.jobs.truncate(jobs - 1); return Err(crate::vm::cold_depth()); }
        let Some((_, _, at)) = self.enter_gen(co, 0) else { self.jobs.truncate(jobs - 1); return Ok(false); };
        self.mark_job();
        let thrown = self.resume_raise.is_some();
        (self.gen_entry, self.entered, self.thrown, self.yielded) = (at, !thrown, thrown, true);
        Ok(true)
    }

    /* The generator a plain generator `__iter__` of instance `v` gives by its call, which runs nothing of its body, None for any other value. */
    #[cold]
    #[inline(never)]
    pub(crate) fn gen_of_iter(&mut self, v: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(v) else { return Ok(None) };
        let Some((func, _)) = self.lookup_class_member(class, "__iter__") else { return Ok(None) };
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(func) else { return Ok(None) };
        if !self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) || self.depth >= self.max_calls { return Ok(None); }
        let job_ok = core::mem::take(&mut self.job_ok);
        let made = self.with_roots([v], |vm| vm.call_with(func, Some(v), &[], &[], chunk).and_then(|()| vm.pop()));
        self.job_ok = job_ok;
        let g = made?;
        Ok(matches!(self.heap.try_get(g), Some(HeapObj::Coroutine(_))).then_some(g))
    }

    /* What the `__iter__` of instance `v` gives, None for any other value. */
    pub(crate) fn iter_of_user(&mut self, v: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        if !matches!(self.heap.try_get(v), Some(HeapObj::Instance(..))) { return Ok(None); }
        if let Some(g) = self.gen_of_iter(v, chunk)? { return Ok(Some(g)); }
        let ok = self.job_ok;
        let it = self.with_roots([v], |vm| vm.try_call_dunder(v, "__iter__", &[], chunk));
        self.job_ok = ok;
        it
    }

    /* Whether a slice store's value on the stack's top is a generator or an instance, whose items `retry_gen` gathers first. */
    #[inline]
    pub(crate) fn slice_gen(&self) -> bool {
        let n = self.stack.len();
        n >= 3 && matches!(self.heap.try_get(self.stack[n - 2]), Some(HeapObj::Slice(..))) && matches!(self.heap.try_get(self.stack[n - 1]), Some(HeapObj::Coroutine(..) | HeapObj::Instance(..)))
    }

    /* Whether the stack's top is a generator, or an instance that may give one, which `retry_gen` looks at. */
    #[inline(always)]
    pub(crate) fn top_may_gen(&self) -> bool {
        match self.stack.last() { Some(&v) => v.is_heap() && matches!(self.heap.get(v), HeapObj::Coroutine(_) | HeapObj::Instance(..)), None => false }
    }

    /* Whether the generator on top of the stack, which the next instruction spreads or unpacks, runs as a frame, its items the list that instruction retakes. */
    #[cold]
    #[inline(never)]
    pub(crate) fn retry_gen(&mut self, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let Some(&v) = self.stack.last() else { return Ok(false) };
        let at = (self.stack.len() - 1) as u32;
        // An instance stands for what its `__iter__` gives once called.
        let v = match self.iter_of_user(v, chunk)? {
            // A user iterator gives its items through `__next__`, so `__iter__` runs once.
            Some(g) if matches!(self.heap.try_get(g), Some(HeapObj::Instance(..))) => {
                let items = self.with_roots([v, g], |vm| vm.iter_items_op(v, g, chunk))?;
                self.stack[at as usize] = self.alloc_list(items)?;
                return Ok(false);
            }
            Some(g) => { self.stack[at as usize] = g; g }
            None => v,
        };
        self.gather_job(v, Takes::Retry(at), Vec::new())
    }

    /* What the next step of iterator `it` takes first that a builtin's step does not, an item, its end, a plain generator to run as a frame, or else. */
    pub(crate) fn peek(&self, it: Val, depth: u8) -> Peek {
        match self.heap.try_get(it) {
            Some(HeapObj::Coroutine(c)) if !c.pending.is_undef() => Peek::Item,
            Some(HeapObj::Coroutine(c)) if c.ip == super::globals::async_ops::FINISHED => Peek::End,
            Some(HeapObj::Coroutine(c)) if !c.running && c.layout == 0 && c.syncs.is_empty() && matches!(c.body, BodyRef::Fn(fi) if self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false)) => Peek::Gen(it),
            Some(HeapObj::Iter(f, _)) if depth < 8 => match f.try_borrow() { Ok(f) => self.peek_frame(&f, depth + 1), Err(_) => Peek::Other },
            _ => Peek::Other,
        }
    }

    /* `peek` of a builtin iterator's frame, a zip or map taking its iterators in order and ending at the first that ends. */
    pub(crate) fn peek_frame(&self, f: &IterFrame, depth: u8) -> Peek {
        match f {
            IterFrame::Zip { its, .. } | IterFrame::Map { its, .. } => {
                for &v in its.iter() { match self.peek(v, depth) { Peek::Item => {} p => return p } }
                Peek::Item
            }
            &IterFrame::Enumerate { it, .. } | &IterFrame::Filter { it, .. } | &IterFrame::UserDefined(it) | &IterFrame::Coroutine(it) => self.peek(it, depth),
            IterFrame::Seq { .. } | IterFrame::List { .. } => if f.ready(&self.heap).is_some() { Peek::Item } else { Peek::End },
            f => if f.ready(&self.heap).is_some() { Peek::Item } else { Peek::Other },
        }
    }

    /* Whether the `for` on top steps a builtin iterator `lazy_prefetch` has not yet found to run no Python. */
    #[inline(always)]
    pub(crate) fn lazy_over(&self) -> bool {
        match self.iter_stack.last() {
            Some(&IterFrame::UserDefined(it) | &IterFrame::Filter { it, .. }) => it.0 != self.lazy_seen.0,
            Some(IterFrame::Map { its, .. }) => its[0].0 != self.lazy_seen.0,
            _ => false,
        }
    }

    /* Whether a step of iterator `v` may run Python (a generator, instance or function under it), or with `py` false only whether a generator lies under it. */
    fn holds_py(&self, v: Val, depth: u8, py: bool) -> bool {
        match self.heap.try_get(v) {
            Some(HeapObj::Iter(f, _)) if depth < 8 => f.try_borrow().map_or(true, |f| self.frame_holds_py(&f, depth + 1, py)),
            Some(HeapObj::Coroutine(_)) => true,
            _ => py,
        }
    }

    fn frame_holds_py(&self, f: &IterFrame, depth: u8, py: bool) -> bool {
        let call = |f: Val| py && matches!(self.heap.try_get(f), Some(HeapObj::Func(..) | HeapObj::Class(..) | HeapObj::Instance(..) | HeapObj::BoundUserMethod(..) | HeapObj::NativeFn(crate::vm::types::NativeFnId::Next)));
        match f {
            IterFrame::Zip { its, .. } => its.iter().any(|&v| self.holds_py(v, depth, py)),
            // `eval` or `exec` a `map` calls runs text, as a floor of the loop.
            IterFrame::Map { f, its, .. } => call(*f) || py && self.runs_text(*f)
                || its.iter().any(|&v| self.holds_py(v, depth, py))
                // A builtin stepping its argument in Rust runs a generator its source still holds.
                || py && self.steps_whole(*f) && its.iter().any(|&v| self.leaf_ahead(v, 0)),
            &IterFrame::Filter { f, it, .. } => call(f) || py && self.runs_text(f) || self.holds_py(it, depth, py) || py && self.steps_whole(f) && self.leaf_ahead(it, 0),
            &IterFrame::Enumerate { it, .. } | &IterFrame::UserDefined(it) => self.holds_py(it, depth, py),
            IterFrame::Seq { .. } | IterFrame::List { .. } | IterFrame::Range { .. } | IterFrame::BigRange { .. } | IterFrame::ListRev { .. } | IterFrame::Watched { .. } | IterFrame::DictWalk { .. } => false,
            IterFrame::Coroutine(_) => true,
            _ => py,
        }
    }

    /* Steps the `for` at `i` over a builtin iterator as frames when it runs Python, its sources first, then its `map` or `filter` function as a job. */
    #[inline(never)]
    pub(crate) fn lazy_prefetch(&mut self, i: usize, code: &Code) -> Result<bool, VmErr> {
        if self.budget == 0 { return Ok(false); }
        if self.heap.needs_gc() { self.collect_point()?; }
        let (fun, its, filter) = match self.iter_stack.last() {
            // A user iterator the `for` steps itself, a builtin one through what it steps.
            Some(&IterFrame::UserDefined(it)) => {
                if matches!(self.heap.try_get(it), Some(HeapObj::Iter(..))) && self.pull_ready(it, false, i, 0)? { return Ok(true); }
                self.pass_by();
                return Ok(false);
            }
            Some(IterFrame::Map { f, its, .. }) => (*f, its.clone(), false),
            Some(&IterFrame::Filter { f, it, .. }) if !f.is_none() => (f, Rc::from([it]), true),
            _ => { self.pass_by(); return Ok(false) }
        };
        for &c in its.iter() { if self.pull_ready(c, false, i, 0)? { return Ok(true); } }
        // `eval` or `exec` runs each text as a floor of the loop, which hands the `for` what it gave.
        if self.runs_text(fun) { return self.text_for(fun, &its, i, filter); }
        if self.call_job(fun, &its, if filter { FeedKind::Filter } else { FeedKind::Map }, None, i)? || self.loop_call(fun, &its, filter, i, code)? { return Ok(true); }
        self.pass_by();
        Ok(false)
    }

    /* `lazy_prefetch` for a `for`'s own `map` or `filter` of a class, varargs lambda or closure, called as a plain call does, Ok(false) otherwise. */
    #[inline(never)]
    fn loop_call(&mut self, fun: Val, its: &[Val], filter: bool, i: usize, code: &Code) -> Result<bool, VmErr> {
        let class = matches!(self.heap.try_get(fun), Some(HeapObj::Class(..)));
        if !(class || matches!(self.heap.try_get(fun), Some(HeapObj::Func(..)))) || its.len() > 8 { return Ok(false); }
        let chunk = self.chunk;
        loop {
            let mut args = [Val::undef(); 8];
            for (k, &c) in its.iter().enumerate() {
                match self.next_of(c, chunk)? {
                    Some(x) => args[k] = x,
                    None => { self.give_back(&its[..k], &args); self.pend.push((c, Val::undef())); return Ok(false); }
                }
            }
            let n = its.len();
            let before = self.pending.exc_val.unwrap_or(Val::undef());
            self.push(fun);
            for &a in &args[..n] { self.push(a); }
            (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
            let called = self.exec_call_n(n, 0, chunk);
            (self.inline_call, self.pending_exec_safe) = (false, false);
            called?;
            if self.entered {
                // Its frame returns to the `for`, the job making its item.
                let at = self.head_at(self.top_pool);
                let flags = self.head(at).as_int() >> 40 | JOB;
                self.regs[at] = Val::int(i as i64 | (NO_DEST as i64) << 24 | flags << 40);
                self.jobs.push(if filter { Job::Filter { it: its[0], item: args[0], before, f: fun, retake: true, kind: FeedKind::Filter } } else { Job::Map { before, ctor: class } });
                (self.entered, self.yielded) = (true, true);
                return Ok(true);
            }
            let v = self.pop()?;
            if !filter { self.hand_to_for(code, i, v); return Ok(true); }
            if let Some((b, fi, len)) = self.bool_dunder(v) {
                self.open_job(fi, b, v, i, NO_DEST, Job::Filter { it: its[0], item: args[0], before, f: fun, retake: true, kind: FeedKind::Truth(len) });
                (self.entered, self.yielded) = (true, true);
                return Ok(true);
            }
            self.push(v);
            let keep = self.truthy_op(v, chunk);
            self.pop()?;
            if keep? { self.hand_to_for(code, i, args[0]); return Ok(true); }
            self.charge_step()?;
        }
    }

    /* Readies the next step of iterator `it` to run no Python, running a generator or call it takes first as a job, Ok(true) once opened. */
    #[inline(never)]
    pub(crate) fn pull_ready(&mut self, it: Val, drive: bool, ret: usize, depth: u8) -> Result<bool, VmErr> {
        if depth > 8 || self.budget == 0 || self.pend.iter().any(|p| p.0.0 == it.0) { return Ok(false); }
        let frame = match self.heap.try_get(it) {
            Some(HeapObj::Coroutine(_)) => return match self.peek(it, 0) { Peek::Gen(g) => self.gather_job(g, Takes::Prefetch(drive), Vec::new()), _ => Ok(false) },
            // A user iterator's `__next__` runs as the job.
            Some(&HeapObj::Instance(class, _)) => {
                let Some((next, _)) = self.lookup_class_member(class, "__next__") else { return Ok(false) };
                return self.call_job(next, &[], FeedKind::Next, Some((it, it, drive)), ret);
            }
            Some(HeapObj::Iter(f, _)) => match f.try_borrow() { Ok(f) => f.clone(), Err(_) => return Ok(false) },
            _ => return Ok(false),
        };
        match frame {
            IterFrame::Zip { its, .. } => { for &c in its.iter() { if self.pull_ready(c, drive, ret, depth + 1)? { return Ok(true); } } Ok(false) }
            IterFrame::Enumerate { it: c, .. } | IterFrame::UserDefined(c) | IterFrame::Coroutine(c) => self.pull_ready(c, drive, ret, depth + 1),
            IterFrame::Map { f, its, .. } => {
                for &c in its.iter() { if self.pull_ready(c, drive, ret, depth + 1)? { return Ok(true); } }
                self.call_job(f, &its, FeedKind::Map, Some((it, Val::undef(), drive)), ret)
            }
            IterFrame::Filter { f, it: src, .. } if !f.is_none() => {
                if self.pull_ready(src, drive, ret, depth + 1)? { return Ok(true); }
                self.call_job(f, &[src], FeedKind::Filter, Some((it, Val::undef(), drive)), ret)
            }
            // A filter of truth takes what its iterator gives first.
            IterFrame::Filter { it: src, .. } => self.pull_ready(src, drive, ret, depth + 1),
            IterFrame::Call { f, sentinel } => self.call_job(f, &[], FeedKind::Call, Some((it, sentinel, drive)), ret),
            _ => Ok(false),
        }
    }

    /* Calls `fun` as a job returning to `ret` on the next items of `its`, for the `for`'s own `map` or `filter` or the step `feed` names. */
    fn call_job(&mut self, fun: Val, its: &[Val], kind: FeedKind, feed: Option<(Val, Val, bool)>, ret: usize) -> Result<bool, VmErr> {
        let (fun, recv) = match self.heap.try_get(fun) {
            Some(&HeapObj::Instance(class, _)) => match self.lookup_class_member(class, "__call__") { Some((c, _)) => (c, Some(fun)), None => return Ok(false) },
            // A bound method runs its function on its receiver first.
            Some(&HeapObj::BoundUserMethod(r, f, _)) => (f, Some(r)),
            _ if kind == FeedKind::Next => (fun, feed.map(|f| f.1)),
            _ => (fun, None),
        };
        // A builtin that steps a generator it is given in Rust is called through a function of the loop, which runs it as a job stepping that generator.
        // `next` of a generator itself takes its item ahead below, a wrapping iterator's runs through the loop too.
        let next_gen = || its.len() == 1 && matches!(self.heap.try_get(fun), Some(HeapObj::NativeFn(crate::vm::types::NativeFnId::Next))) && self.next_item(its[0]).is_some_and(|x| matches!(self.heap.try_get(x), Some(HeapObj::Coroutine(_))));
        let fun = if recv.is_none() && matches!(kind, FeedKind::Map | FeedKind::Filter) && self.steps_whole(fun) && self.next_leaf(its) && !next_gen() { self.through_loop(fun, its.len())?.unwrap_or(fun) } else { fun };
        if feed.is_some() && recv.is_none() && its.len() == 1 && matches!(self.heap.try_get(fun), Some(HeapObj::NativeFn(crate::vm::types::NativeFnId::Next))) { return self.next_ready(its[0], feed.is_some_and(|f| f.2)); }
        let lead = recv.is_some() as usize;
        let n = its.len() + lead;
        if n > 8 { return Ok(false); }
        // A function no plain frame opens, a class, a varargs lambda, or an `eval` or `exec` run as text, is called as a plain call does.
        let fi = match self.heap.try_get(fun) { Some(HeapObj::Func(..)) => self.job_fn_as(fun, n, false).filter(|&fi| !self.uses_super(fi)), _ => None };
        let source = feed.is_some() && matches!(kind, FeedKind::Map | FeedKind::Filter) && self.runs_text(fun);
        if fi.is_none() && !source && (feed.is_none() || !matches!(self.heap.try_get(fun), Some(HeapObj::Func(..) | HeapObj::Class(..)))) { return Ok(false); }
        let mut args = [Val::undef(); 8];
        if let Some(r) = recv { args[0] = r; }
        let chunk = self.chunk;
        for (k, &c) in its.iter().enumerate() {
            match self.next_of(c, chunk)? {
                Some(x) => args[lead + k] = x,
                // One spent ends the step, which meets it spent again and the items taken before it.
                None => { self.give_back(&its[..k], &args[lead..]); self.pend.push((c, Val::undef())); return Ok(false); }
            }
        }
        let before = self.pending.exc_val.unwrap_or(Val::undef());
        let last = args[n.saturating_sub(1)];
        let job = match feed {
            None if kind == FeedKind::Filter => Job::filter(its[0], last, before),
            None => Job::Map { before, ctor: false },
            Some((target, other, drive)) => Job::Feed {
                target, kind, drive,
                item: if kind == FeedKind::Filter { last } else { Val::undef() },
                sentinel: if kind == FeedKind::Call { other } else { Val::undef() },
            },
        };
        let ret = if matches!(feed, Some((_, _, true))) { 0 } else { ret };
        let Some(fi) = fi else { return self.call_as_job(fun, &args[..n], ret, job) };
        match n {
            1 => { self.open_job(fi, fun, args[0], ret, NO_DEST, job); }
            2 => self.open_job2(fi, fun, args[0], args[1], ret, job),
            _ => match self.open_block(fi, fun, &args[..n], ret as i64 | (NO_DEST as i64) << 24 | JOB << 40) {
                Some(_) => self.jobs.push(job),
                // A closure of no argument no block opens runs in the frame its plain call opens, which the job reruns in place with nothing to bind.
                None if n == 0 => return self.call_as_job(fun, &[], ret, job),
                None => { self.give_back(its, &args[lead..]); return Ok(false); }
            },
        }
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* Whether an item builtin iterator `it` has left, read without stepping it, holds a generator `gen_leaf` finds, or the items of an iterator under it. */
    pub(crate) fn leaf_ahead(&self, it: Val, depth: u8) -> bool {
        let Some(HeapObj::Iter(f, _)) = self.heap.try_get(it) else { return false };
        if depth > 8 { return false; }
        let leaf = |v: &Val| self.steps_py(*v);
        match f.try_borrow().as_deref() {
            Ok(IterFrame::Seq { items, idx }) => items.get(*idx..).is_some_and(|r| r.iter().any(leaf)),
            Ok(IterFrame::List { rc, idx }) => rc.try_borrow().is_ok_and(|l| l.get(*idx..).is_some_and(|r| r.iter().any(leaf))),
            Ok(IterFrame::Zip { its, .. } | IterFrame::Map { its, .. }) => its.iter().any(|&c| self.leaf_ahead(c, depth + 1)),
            Ok(&IterFrame::Enumerate { it, .. } | &IterFrame::Filter { it, .. } | &IterFrame::UserDefined(it)) => self.leaf_ahead(it, depth + 1),
            _ => false,
        }
    }

    /* Whether a builtin stepping `v` in Rust would run Python, a generator under it or a user iterable's `__iter__`. */
    pub(crate) fn steps_py(&self, v: Val) -> bool {
        match self.heap.try_get(v) {
            Some(&HeapObj::Instance(class, _)) => self.lookup_class_member(class, "__iter__").is_some(),
            _ => self.gen_leaf(v, 0).is_some(),
        }
    }

    /* The generator a builtin stepping `v` in Rust runs first, `v` itself or one under the builtin iterators wrapping it. */
    pub(crate) fn gen_leaf(&self, v: Val, depth: u8) -> Option<Val> {
        if depth > 8 { return None; }
        match self.heap.try_get(v) {
            Some(HeapObj::Coroutine(_)) => matches!(self.peek(v, 0), Peek::Gen(_)).then_some(v),
            Some(HeapObj::Iter(f, _)) => match &*f.try_borrow().ok()? {
                IterFrame::Zip { its, .. } | IterFrame::Map { its, .. } => its.iter().find_map(|&c| self.gen_leaf(c, depth + 1)),
                &IterFrame::Enumerate { it, .. } | &IterFrame::Filter { it, .. } | &IterFrame::UserDefined(it) | &IterFrame::Coroutine(it) => self.gen_leaf(it, depth + 1),
                _ => None,
            },
            _ => None,
        }
    }

    /* Whether `f` called on a generator steps it in Rust, a builtin taking its iterable, `next`, an iterating method, a native binding or a class over a container. */
    pub(crate) fn steps_whole(&self, f: Val) -> bool {
        use crate::vm::types::NativeFnId::*;
        match self.heap.try_get(f) {
            Some(&HeapObj::NativeFn(id)) => matches!(id, Sum | Sorted | Set | FrozenSet | Bytes | ByteArray | Dict | Min | Max | List | Tuple | Any | All | Next),
            Some(HeapObj::Type(n)) => matches!(n.as_str(), "list" | "tuple" | "set" | "frozenset" | "dict" | "bytes" | "bytearray"),
            Some(&HeapObj::BoundMethod(_, id)) => matches!(id.kind(), crate::vm::methods::MethodKind::Iterates),
            Some(HeapObj::Extern(_)) => true,
            // A class over a builtin container makes its value from the iterable it is given.
            Some(HeapObj::Class(..)) => self.native_base(f).is_some_and(|n| matches!(self.heap.try_get(n), Some(HeapObj::Type(t)) if matches!(t.as_str(), "list" | "tuple" | "dict" | "set" | "frozenset" | "bytes"))),
            _ => false,
        }
    }

    /* Whether the next item of one of `its`, or one an iterator under it has left, read without stepping it, holds a generator `gen_leaf` finds. */
    fn next_leaf(&self, its: &[Val]) -> bool {
        its.iter().any(|&c| self.next_item(c).is_some_and(|x| self.steps_py(x)) || self.leaf_ahead(c, 0))
    }

    /* The item iterator `c` gives next where it is read without stepping it. */
    fn next_item(&self, c: Val) -> Option<Val> {
        match self.heap.try_get(c) {
            Some(HeapObj::Iter(f, _)) => f.try_borrow().ok().and_then(|f| f.ready(&self.heap)),
            Some(HeapObj::Coroutine(g)) => Some(g.pending).filter(|p| !p.is_undef()),
            _ => None,
        }
    }

    /* `f` bound to a function of the loop calling it on `n` arguments, so a builtin's callback that steps a generator opens frames as a plain call does. */
    #[cold]
    #[inline(never)]
    pub(crate) fn through_loop(&mut self, f: Val, n: usize) -> Result<Option<Val>, VmErr> {
        if n == 0 || n > 3 { return Ok(None); }
        if self.calls_through.is_none() {
            let src = "def call1(a, f=None):\n    return f(a)\ndef call2(a, b, f=None):\n    return f(a, b)\ndef call3(a, b, c, f=None):\n    return f(a, b, c)\ndef remake(a, f=None, c=None):\n    return c(f(a))\n";
            let (form, path) = (super::globals::compile::Form { mode: super::types::CodeMode::Exec, closed: false }, self.chunk.path.clone());
            let chunk = self.parse_compiled(src, form, "<builtin>", &path)?;
            let start = self.functions.len();
            self.adopt(src, form, Some("\u{0}calls".into()), chunk);
            self.calls_through = Some(start);
        }
        let Some(first) = self.calls_through else { return Ok(None) };
        Ok(Some(self.heap.alloc(HeapObj::Func(first + n - 1, alloc::vec![f], Vec::new(), Rc::new(core::cell::RefCell::new(Vec::new()))))?))
    }

    /* A function of the loop making class `cls` over builtin `native` from what `native` makes of its argument in the loop. */
    #[cold]
    #[inline(never)]
    pub(crate) fn remake_through(&mut self, native: Val, cls: Val) -> Result<Option<Val>, VmErr> {
        let Some(call) = self.through_loop(native, 1)? else { return Ok(None) };
        let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(call) else { return Ok(None) };
        Ok(Some(self.heap.alloc(HeapObj::Func(fi + 3, alloc::vec![native, cls], Vec::new(), Rc::new(core::cell::RefCell::new(Vec::new()))))?))
    }

    /* Keeps `items`, all generator `g` gave, in it for what steps it next. */
    pub(crate) fn hold_items(&mut self, g: Val, mut items: Vec<Val>) {
        items.reverse();
        let Some(first) = items.pop() else { return };
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) { c.pending = first; }
        if !items.is_empty() { self.held.push((g, items)); }
    }

    /* Holds the first generator under the arguments of the native binding `call` keeps first as a job, false for none. */
    pub(crate) fn hold_start(&mut self, call: Vec<Val>) -> Result<bool, VmErr> {
        let Some(g) = call.get(1..).and_then(|r| r.iter().find_map(|&v| self.gen_leaf(v, 0))) else { return Ok(false) };
        self.with_roots(call.iter().copied(), |vm| vm.gather_job(g, Takes::Hold, call.clone()))
    }

    /* Holds the next generator the binding `call` keeps first needs, or calls it once none is left, its result on the stack. */
    #[inline(never)]
    pub(crate) fn hold_then(&mut self, call: Vec<Val>) -> Result<(), VmErr> {
        if self.hold_start(call.clone())? { return Ok(()); }
        self.with_roots(call.iter().copied(), |vm| vm.extern_call(&call))
    }

    /* Readies the next item of `it` for the `next` a builtin iterator's step calls. */
    fn next_ready(&mut self, it: Val, drive: bool) -> Result<bool, VmErr> {
        let chunk = self.chunk;
        let Some(x) = self.next_of(it, chunk)? else { self.pend.push((it, Val::undef())); return Ok(false) };
        self.give_back(&[it], &[x]);
        if !matches!(self.peek(x, 0), Peek::Gen(_)) { return Ok(false); }
        self.gather_job(x, Takes::Prefetch(drive), Vec::new())
    }

    /* What a `Feed` job's call gave, `v`, makes of the next step of its iterator `target`, as `kind` takes it. */
    pub(crate) fn feed_take(&mut self, target: Val, item: Val, sentinel: Val, kind: FeedKind, v: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        match kind {
            FeedKind::Map | FeedKind::Next => self.pend.push((target, v)),
            FeedKind::Filter => { self.push(v); let keep = self.truthy_op(v, chunk); self.pop()?; if keep? { self.pend.push((target, item)); } }
            FeedKind::Truth(len) => if self.truth_of(v, len)? { self.pend.push((target, item)); },
            // A callable iterator stays spent once it meets the sentinel.
            FeedKind::Call if eq_member(v, sentinel, &self.heap) => if let Some(HeapObj::Iter(f, _)) = self.heap.try_get(target) { *f.borrow_mut() = IterFrame::Seq { items: Vec::new().into(), idx: 0 }; },
            FeedKind::Call => self.pend.push((target, v)),
        }
        Ok(())
    }

    /* Calls `callee` on `args` as a plain call binds them, its frame the job's returning to `ret`, Ok(false) with the result kept when it opens none. */
    #[inline(never)]
    fn call_as_job(&mut self, callee: Val, args: &[Val], ret: usize, job: Job) -> Result<bool, VmErr> {
        let chunk = self.chunk;
        self.push(callee);
        for &a in args { self.push(a); }
        // The text `eval` or `exec` runs is set aside as a floor the loop takes next, whose end hands the step what it gave.
        let source = self.runs_text(callee);
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (true, self.frame_safe, source);
        let called = self.exec_call_n(args.len(), 0, chunk);
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (false, false, false);
        called?;
        if source && self.floor_req {
            let Job::Feed { target, item, kind, drive, .. } = job else { return Err(cold_runtime("a run of text a job called for no step")) };
            self.feed_floor(super::globals::compile::Fed::Feed(target, item, kind, drive));
            return Ok(true);
        }
        if !self.entered {
            let v = self.pop()?;
            let Job::Feed { target, item, sentinel, kind, .. } = job else { return Err(cold_runtime("a job's call opened no frame")) };
            self.feed_take(target, item, sentinel, kind, v, chunk)?;
            return Ok(false);
        }
        let at = self.head_at(self.top_pool);
        let flags = self.head(at).as_int() >> 40 | JOB;
        self.regs[at] = Val::int(ret as i64 | (NO_DEST as i64) << 24 | flags << 40);
        self.jobs.push(job);
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* Marks the iterator the `for` on top steps, one that runs no Python, which stays so, for a later step to pass by. */
    fn pass_by(&mut self) {
        let Some(f) = self.iter_stack.last() else { return };
        if self.frame_holds_py(f, 0, true) { return; }
        self.lazy_seen = match f { &IterFrame::UserDefined(it) | &IterFrame::Filter { it, .. } => it, IterFrame::Map { its, .. } => its[0], _ => Val::undef() };
    }

    /* Keeps the items `items` taken from the iterators `its` for their next steps, which meet them again. */
    pub(crate) fn give_back(&mut self, its: &[Val], items: &[Val]) {
        for (&it, &v) in its.iter().zip(items) { self.pend.push((it, v)); }
    }

    /* Whether the conversion job on top converts few values. */
    fn convert_few(&self) -> bool { matches!(self.jobs.last(), Some(Job::Convert(_, _, m)) if m.few) }

    /* The frame of `g`, the function the job on top calls next on `n` arguments, in its first code for a conversion of few. */
    #[inline(never)]
    pub(crate) fn next_fn(&mut self, g: Val, n: usize) -> Option<usize> { let few = self.convert_few(); self.job_fn_few(g, n, few) }

    /* The item kept for the next step of the iterator `it`, Some(None) for a step that ends it, None for none kept. */
    #[inline]
    pub(crate) fn pended(&mut self, it: Val) -> Option<Option<Val>> {
        if self.pend.is_empty() { return None; }
        let k = self.pend.iter().position(|p| p.0.0 == it.0)?;
        let v = self.pend.swap_remove(k).1;
        Some((!v.is_undef()).then_some(v))
    }

    /* Ends a step whose `Feed` call raised StopIteration, its iterator spent, giving whether the builtin's job under it drives on, None for no job. */
    #[inline(never)]
    pub(crate) fn feed_stopped(&mut self) -> Option<bool> {
        let Some(&Job::Feed { target, drive, .. }) = self.jobs.last() else { return None };
        self.pend.push((target, Val::undef()));
        (self.error_byte_pos, _) = (None, self.swallow_stop(None));
        self.call_stack.clear();
        Some(drive)
    }

    /* A job for the builtin `takes` stands for over `of`, a generator or a user iterator, `extra` its arguments after. */
    fn gather_of(&self, takes: Takes, extra: Vec<Val>, of: Val) -> Gather {
        let mut j = Gather::new(takes, extra);
        // What the items come from stays held while user code the builtin runs between them collects.
        j.co = of;
        if takes == Takes::Sum {
            // The start the items add to, 0 unless passed.
            let start = j.extra.first().copied().unwrap_or(Val::int(0));
            let word = if start.is_int() || start.is_foreign() || start.is_bool() { self.as_i128(start).and_then(|n| i64::try_from(n).ok()) } else { None };
            j.total = match word { Some(w) => Total::Word(w), None if start.is_float() => Total::Float(start.as_float(), 0.0), None => Total::Any };
            j.acc = start;
        }
        j
    }

    /* Runs the builtin `takes` stands for over user iterable `o`, `__iter__` once then a plain `__next__` as frames, Ok(false) for any other `o`. */
    #[inline(never)]
    pub(crate) fn pull_job(&mut self, o: Val, takes: Takes, extra: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        if self.budget == 0 || !matches!(self.heap.try_get(o), Some(HeapObj::Instance(..))) { return Ok(false); }
        // A plain `__iter__` runs as a frame of the loop too, the builtin's job waiting under it.
        if let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(o) && let Some((f, _)) = self.lookup_class_member(class, "__iter__")
            && f.is_heap() && matches!(self.heap.get(f), HeapObj::Func(..)) && let Some(fi) = self.job_fn_once(f) && !self.uses_super(fi) {
            let mut j = self.gather_of(takes, extra.to_vec(), Val::undef());
            j.before = self.pending.exc_val.unwrap_or(Val::undef());
            self.jobs.push(Job::Gather(Box::new(j)));
            self.open_job(fi, f, o, 0, super::act::NO_DEST, Job::IterOf);
            (self.entered, self.yielded) = (true, true);
            return Ok(true);
        }
        let base = self.temp_roots.len();
        self.temp_roots.push(o);
        self.temp_roots.extend_from_slice(extra);
        let it = self.try_call_dunder(o, "__iter__", &[], chunk);
        self.temp_roots.truncate(base);
        let Some(it) = it? else { return Ok(false) };
        self.pull_on(it, takes, extra, chunk)
    }

    /* Runs the builtin `takes` stands for over iterator `it`, `extra` its arguments after, as `pull_from` takes it on, pushing or opening a frame. */
    fn pull_on(&mut self, it: Val, takes: Takes, extra: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        let mut j = self.gather_of(takes, extra.to_vec(), Val::undef());
        j.before = self.pending.exc_val.unwrap_or(Val::undef());
        self.jobs.push(Job::Gather(Box::new(j)));
        match self.pull_from(it, chunk)? {
            Pulled::Pushed => {}
            // The frame returns where the builtin's call carries on, which the handler that opens it sets.
            Pulled::Opened => (self.entered, self.yielded) = (true, true),
            Pulled::Fold(fi, func, fold, args, n) => {
                self.open_fold(fi, func, fold, args, n);
                (self.entered, self.yielded) = (true, true);
            }
        }
        Ok(true)
    }

    /* `pull_on` for a builtin iterator whose next step takes a plain generator first, run as a frame, Ok(false) with nothing run for any other. */
    pub(crate) fn lazy_pull(&mut self, it: Val, takes: Takes, extra: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        if self.budget == 0 || !matches!(self.peek(it, 0), Peek::Gen(_)) { return Ok(false); }
        self.pull_on(it, takes, extra, chunk)
    }

    /* Takes the job's builtin on to `it`, a user iterable's iterator, a generator as `gather_job` runs one, a plain `__next__` as frames, others itself. */
    pub(crate) fn pull_from(&mut self, it: Val, chunk: &SSAChunk) -> Result<Pulled, VmErr> {
        let j = self.take_gather();
        self.jobs.pop();
        if matches!(self.heap.try_get(it), Some(HeapObj::Coroutine(_))) && self.gather_job(it, j.takes, j.extra.clone())? { return Ok(Pulled::Opened); }
        let before = j.before;
        let mut j = self.gather_of(j.takes, j.extra, it);
        j.before = before;
        let next = match self.heap.try_get(it) { Some(&HeapObj::Instance(class, _)) => self.lookup_class_member(class, "__next__").map(|m| m.0), _ => None };
        if let Some(f) = next && let Some(fi) = self.job_fn(f) {
            self.open_job(fi, f, it, 0, super::act::NO_DEST, Job::Gather(Box::new(j)));
            return Ok(Pulled::Opened);
        }
        self.jobs.push(Job::Gather(Box::new(j)));
        if matches!(self.heap.try_get(it), Some(HeapObj::Iter(..))) {
            return match self.gather_drive()? {
                Drive::Fold(fi, func, fold, args, n) => Ok(Pulled::Fold(fi, func, fold, args, n)),
                Drive::Done(r) => { self.push(r); Ok(Pulled::Pushed) }
                Drive::Opened => Ok(Pulled::Opened),
            };
        }
        // Any other iterator steps through the nested call, the job holding what it took.
        let jobs = self.jobs.len();
        let r = loop {
            if let Err(e) = self.charge_step() { break Err(e); }
            let before = self.pending.exc_val;
            match self.iter_next_proto(it, chunk) {
                Ok(Some(v)) => match self.gather_take(v) { Ok(None) => {}, done => break done.map(|r| r.unwrap_or(Val::none())) },
                Ok(None) => { let mut j = self.take_gather(); break self.gather_end(&mut j, Val::none(), chunk); }
                Err(VmErr::Raised(ref m)) if m == "StopIteration" || m.starts_with("StopIteration:") => {
                    self.swallow_stop(before);
                    let mut j = self.take_gather();
                    break self.gather_end(&mut j, Val::none(), chunk);
                }
                Err(e) => break Err(e),
            }
        };
        self.jobs.truncate(jobs - 1);
        self.push(r?);
        Ok(Pulled::Pushed)
    }

    /* Gives generator function `fi` the code its frames run, first unlowered with the cold mark set, lowered once its entries or a loop heat it. */
    #[inline(never)]
    pub(crate) fn gen_cold_code(&mut self, fi: usize) {
        let pool = self.call_facts[fi].pool;
        let chunk = &self.functions[fi].1;
        // Its first code runs on until its entries heat it, which `enter_gen` counts.
        if self.pools[pool].cold { return; }
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        if self.pools[pool].consts.is_none() { return; }
        let shape = self.pools[pool].shape;
        let code = Rc::new(lower::identity(&lower::fuse_method_calls(chunk, shape), chunk.names.len(), self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi)));
        let p = &mut self.pools[pool];
        (p.code, p.cold, p.ran, p.heat) = (Some(code), true, true, p.heat + 1);
    }

    /* Steps the job's container past the item a fold took, to the next fold whose dunder runs as a frame or to the builtin's result. */
    pub(crate) fn gather_drive(&mut self) -> Result<Drive, VmErr> {
        let chunk = self.chunk;
        // An item a pull took before the job stepped its iterator itself comes first, kept where the pull kept its item.
        if let Some(Job::Gather(j)) = self.jobs.last_mut() && j.pull.src.is_undef() && !j.pull.item.is_undef() {
            let v = core::mem::replace(&mut j.pull.item, Val::undef());
            if !self.gather_fast(v) && let Some(d) = self.gather_item(v)? { return Ok(d); }
        }
        loop {
            let next = match self.jobs.last_mut() {
                // A container steps here, the items it takes with no user code taken at once.
                Some(Job::Gather(j)) => match j.cur.take() {
                    Some(mut c) => {
                        // Out of the job while it steps, which runs no user code and collects nothing.
                        let next = loop {
                            let next = c.next_item(&mut self.heap);
                            if let Ok(Some(v)) = next && self.budget != 0 && !self.heap.has_work() && take_plain(j, v, &self.heap) { self.budget -= 1; continue; }
                            self.heap.set_budget(self.budget);
                            break next;
                        };
                        j.cur = Some(c);
                        next
                    }
                    // A builtin iterator steps as itself, a generator it takes first running as a frame before, any other iterator through the nested call.
                    None => {
                        let (co, gens) = (j.co, j.gens && !j.native);
                        if gens && self.pull_ready(co, true, 0, 0)? { return Ok(Drive::Opened); }
                        match self.plain_next(co) {
                            Some(next) => next,
                            None if matches!(self.heap.try_get(co), Some(HeapObj::Iter(..))) => {
                                self.step_soft = true;
                                let r = self.iter_step(co);
                                self.step_soft = false;
                                // A step that left a generator owing its item runs it as a frame first.
                                if matches!(r, Ok(Some(v)) if v.is_undef()) { continue; }
                                r
                            }
                            None => self.next_of(co, chunk),
                        }
                    }
                },
                _ => return Err(cold_runtime("a builtin's job without its container")),
            };
            let next = match next { Ok(n) => n, Err(e) => { self.jobs.pop(); return Err(e); } };
            let Some(v) = next else {
                let mut j = self.take_gather();
                self.jobs.pop();
                let r = self.gather_end(&mut j, Val::none(), chunk)?;
                return Ok(if self.entered || self.floor_req { Drive::Opened } else { Drive::Done(r) });
            };
            if self.gather_fast(v) { continue; }
            if let Some(d) = self.gather_item(v)? { return Ok(d); }
        }
    }

    /* Takes the job's item `v` that may call user code, a fold whose dunder runs as a frame or the builtin's result, None while it takes more. */
    fn gather_item(&mut self, v: Val) -> Result<Option<Drive>, VmErr> {
        if let Err(e) = self.charge_step() { self.jobs.pop(); return Err(e); }
        let native = matches!(self.jobs.last(), Some(Job::Gather(j)) if j.native);
        if !native && let Some((fi, func, fold, args, n)) = self.fold_of(v, NATIVE) { return Ok(Some(Drive::Fold(fi, func, fold, args, n))); }
        match self.gather_take(v) {
            Ok(None) => Ok(None),
            Ok(Some(r)) => { self.jobs.pop(); Ok(Some(Drive::Done(r))) }
            Err(e) => { self.jobs.pop(); Err(e) }
        }
    }

    /* Runs the builtin `takes` stands for over `o` as one fold, generators and user or Python iterators through their own jobs when `frames`. */
    pub(crate) fn fold_run(&mut self, o: Val, takes: Takes, extra: &[Val], frames: bool, chunk: &SSAChunk) -> Result<(), VmErr> {
        if frames && o.is_heap() {
            let taken = match self.heap.get(o) {
                HeapObj::Coroutine(_) => self.gather_job(o, takes, extra.to_vec())?,
                HeapObj::Instance(..) => self.pull_job(o, takes, extra, chunk)?,
                HeapObj::Iter(..) => self.lazy_pull(o, takes, extra, chunk)? || self.pull_map(o, takes, extra)? || self.drive_iter(o, takes, extra)?,
                _ => false,
            };
            if taken { return Ok(()); }
        }
        // An iterator steps as asked, so the fold stops at the item that decides it, and a container is read through its cursor.
        let lazy = matches!(self.heap.try_get(o), Some(HeapObj::Iter(..) | HeapObj::Coroutine(..) | HeapObj::Instance(..)));
        let (co, mut cur) = if lazy { (self.iter_of(o, chunk)?, None) } else { (o, Some(self.iter_cursor(o)?)) };
        let mut j = self.gather_of(takes, extra.to_vec(), co);
        let (mut first, mut held) = (None, None);
        if let Some(c) = cur.as_mut() {
            // Ints of a machine word add with no look at the job.
            if let (Takes::Sum, Total::Word(mut w)) = (j.takes, j.total) {
                while self.budget != 0 && let Some(v) = c.next_ready(&self.heap) {
                    match v.is_int().then(|| w.checked_add(v.as_int())).flatten() {
                        Some(n) => { w = n; self.budget -= 1; }
                        None => { held = Some(v); break; }
                    }
                }
                j.total = Total::Word(w);
            }
            while first.is_none() {
                let next = match held.take() { Some(v) => Some(v), None => c.next_item(&mut self.heap)? };
                match next {
                    None => { let r = self.gather_end(&mut j, Val::none(), chunk)?; self.push(r); return Ok(()); }
                    Some(v) if self.budget != 0 && !self.heap.has_work() && take_plain(&mut j, v, &self.heap) => self.budget -= 1,
                    // An item that is no instance, the best or total no instance either, takes no user code either.
                    Some(v) if !self.is_instance(v) && !self.is_instance(j.acc) && match j.takes {
                        // Two lists or tuples whose walk meets an instance compare through its dunders.
                        Takes::Extreme(_) => !matches!(self.pair_scan(v, j.acc, 0), Some(PairScan::Dunder(_))),
                        Takes::Sum | Takes::Any | Takes::Every => true,
                        Takes::Keyed(_, k) => matches!(self.heap.try_get(k), Some(HeapObj::NativeFn(_))),
                        _ => false,
                    } => {
                        self.charge_step()?;
                        match j.takes {
                            Takes::Sum => (j.total, j.acc) = self.sum_on(j.total, j.acc, v, chunk)?,
                            Takes::Extreme(is_min) => if j.acc.is_undef() || self.beats(v, j.acc, is_min, chunk)? { j.acc = v; },
                            // A builtin key runs at once, the keys it gives held while any `__lt__` of theirs compares them.
                            Takes::Keyed(is_min, k) => {
                                self.push(k);
                                self.push(v);
                                self.exec_call(1, chunk)?;
                                let kv = self.pop()?;
                                let best = j.best_key;
                                if best.is_undef() || self.with_roots([kv, best, j.acc, v], |vm| vm.beats(kv, best, is_min, chunk))? { (j.acc, j.best_key) = (v, kv); }
                            }
                            t => if self.truthy(v) == (t == Takes::Any) { self.push(Val::bool(t == Takes::Any)); return Ok(()); },
                        }
                    }
                    Some(v) => first = Some(v),
                }
            }
            self.heap.set_budget(self.budget);
        }
        (j.cur, j.native, j.gens) = (cur, !frames, lazy && frames && self.holds_py(co, 0, true));
        let depth = self.jobs.len();
        self.jobs.push(Job::Gather(Box::new(j)));
        let drive = match first.map(|v| self.gather_item(v)) { Some(Ok(Some(d))) => Ok(d), Some(Err(e)) => Err(e), _ => self.gather_drive() };
        if drive.is_err() { self.jobs.truncate(depth); }
        match drive? {
            Drive::Done(r) => self.push(r),
            Drive::Fold(fi, func, d, args, n) => { self.open_fold(fi, func, d, args, n); (self.entered, self.yielded) = (true, true); }
            Drive::Opened => (self.entered, self.yielded) = (true, true),
        }
        Ok(())
    }

    /* Takes `drive`, what a builtin stepping its container did, into the index the loop carries on at, past its result or its opened frame's entry. */
    #[inline(never)]
    pub(crate) fn after_drive(&mut self, drive: Drive, ret: usize, dest: u16) -> usize {
        match drive {
            Drive::Done(r) => return self.deliver(r, ret, dest),
            Drive::Fold(fi, func, d, args, n) => { self.open_fold(fi, func, d, args, n); self.set_ret(ret, dest); }
            // A run of text the step called for opens as a floor the loop takes next, carrying on at `ret` once it ends.
            Drive::Opened if self.floor_req => { self.entered = false; return ret; }
            Drive::Opened => {
                (self.entered, self.yielded) = (false, false);
                if self.top().is_some_and(|f| !f.generator.is_undef()) { self.gather_ret(ret); return self.gen_entry; }
                self.set_ret(ret, dest);
            }
        }
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        // SAFETY the pool holds the frame's code for as long as the VM lives.
        let code = unsafe { &*(self.head(at + 4).as_int() as usize as *const Code) };
        let chunk: *const SSAChunk = &self.functions[fi].1;
        // SAFETY every function's chunk lives as long as the VM.
        self.lay(unsafe { &*chunk }, code, pool);
        code.lowered(0)
    }

    /* What the job on top gathered, the job left empty. */
    #[inline(never)]
    pub(crate) fn take_gather(&mut self) -> Gather {
        match self.jobs.last_mut() { Some(Job::Gather(j)) => core::mem::replace(&mut **j, Gather::new(Takes::Any, Vec::new())), _ => Gather::new(Takes::Any, Vec::new()) }
    }

    /* Calls the dunder of `check` on instance `o` as a frame for the builtin of one dunder, false when no plain function or frame runs it. */
    #[inline(never)]
    pub(crate) fn check_job(&mut self, o: Val, check: Check) -> bool {
        let Some((fi, func)) = self.convert_fn(o, check, true, false) else { return false };
        self.open_job(fi, func, o, 0, super::act::NO_DEST, Job::Check(check));
        (self.entered, self.yielded) = (true, true);
        true
    }

    /* `str` of the instance `o`, or `print` of it alone when `print`, its `__str__` or past it its `__repr__` as a frame of the loop. */
    pub(crate) fn str_job(&mut self, o: Val, print: bool) -> bool {
        let class = self.instance_class(o).unwrap_or(Val::undef());
        let repr = self.lookup_class_member(class, "__str__").is_none();
        // One with neither writes as the builtins write it.
        if repr && self.lookup_class_member(class, "__repr__").is_none() { return false; }
        let check = if print { Check::Print(repr) } else { Check::Str(repr) };
        self.check_job(o, check)
    }

    /* The truth of what a `__bool__` gave, or a `__len__` when `len`, or the error a wrong one raises. */
    fn truth_of(&mut self, v: Val, len: bool) -> Result<bool, VmErr> { Ok(self.checked(if len { Check::LenTruth } else { Check::Bool }, v)?.as_bool()) }

    /* `v`, what the dunder of `check` gave, as the conversion takes it, checked or the error a wrong one raises. */
    pub(crate) fn checked(&mut self, check: Check, v: Val) -> Result<Val, VmErr> {
        Ok(match check {
            Check::Bool if v.is_bool() => v,
            Check::Bool => return Err(VmErr::TypeMsg(crate::s!("__bool__ should return bool, returned ", str self.type_name(v)))),
            Check::LenTruth => Val::bool(self.len_to_bool(v)?),
            Check::Len => {
                let n = if v.is_int() { v.as_int() as i128 } else if let Some(i) = crate::vm::types::as_i128(v, &self.heap) { i } else { return Err(crate::vm::cold_type("__len__ must return int")) };
                if n < 0 { return Err(crate::vm::cold_value("__len__() should return >= 0")); }
                self.int_to_val(Some(n))?
            }
            Check::Abs | Check::Next(..) | Check::GetItem(_) | Check::Missing(_) => v,
            Check::Int(_) | Check::Index if v.is_bool() => Val::int(v.as_bool() as i64),
            Check::Int(_) | Check::Index if v.is_int() || crate::frontend::int::is_big(&self.heap, v) => v,
            Check::Int(o) => return Err(self.returned_wrong(o, "__int__", "an int", v)),
            Check::Index => return Err(VmErr::TypeMsg(crate::s!("__index__ returned non-int (type ", str self.type_name(v), ")"))),
            Check::Float(_) if v.is_float() => v,
            Check::Float(o) => return Err(self.returned_wrong(o, "__float__", "a float", v)),
            Check::Str(_) | Check::Print(_) if matches!(self.heap.try_get(v), Some(HeapObj::Str(_))) => v,
            Check::Str(repr) | Check::Print(repr) => { self.require_str(v, if repr { "__repr__" } else { "__str__" })?; v }
            Check::Format(_) if matches!(self.heap.try_get(v), Some(HeapObj::Str(_))) => v,
            Check::Format(_) => return Err(VmErr::TypeMsg(crate::s!("__format__ must return a str, not ", str self.type_name(v)))),
        })
    }

    /* The plain function and index of the dunder of `check` on `v` (or a user class's dict or list), as `once` and `few` call it, None for nested only. */
    fn convert_fn(&mut self, v: Val, check: Check, once: bool, few: bool) -> Option<(usize, Val)> {
        if self.budget == 0 { return None; }
        let class = self.instance_class(v)?;
        let (func, _) = self.lookup_class_member(class, check.dunder())?;
        if !(func.is_heap() && matches!(self.heap.get(func), HeapObj::Func(..))) { return None; }
        let n = 1 + check.arg().is_some() as usize;
        let fi = if few { self.job_fn_few(func, n, true)? } else { self.job_fn_as(func, n, once)? };
        // One that names `super` binds as the method it is, which the nested call does.
        (!self.uses_super(fi)).then_some((fi, func))
    }

    /* `v` converted by the dunder of `check`, called nested as a builtin calls it, and checked. */
    pub(crate) fn convert_now(&mut self, v: Val, check: Check, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let arg = check.arg();
        match self.try_call_dunder(v, check.dunder(), arg.as_slice(), chunk)? {
            Some(r) => self.checked(check, r),
            None => Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(v), "' object has no ", str check.dunder()))),
        }
    }

    /* Runs the builtin `then` stands for, the values it asks converted by their dunders, plain ones as frames while the builtin waits as a job. */
    pub(crate) fn convert_run(&mut self, then: Box<dyn Resume>, chunk: &SSAChunk) -> Result<Converted, VmErr> {
        // The builtin's state stays on the jobs while it runs, so a collection its dunders run keeps what it holds.
        let few = then.few();
        self.jobs.push(Job::Convert(Check::Abs, then, Memo { few, ..Memo::none() }));
        let r = self.convert_on(None, chunk);
        let Some(job) = self.jobs.pop() else { return Err(cold_runtime("a conversion without its job")) };
        Ok(match r? {
            Conv::Frame(fi, func, x, None) => { self.open_job(fi, func, x, 0, NO_DEST, job); (self.entered, self.yielded) = (true, true); Converted::Opened }
            Conv::Frame(fi, func, x, Some(a)) => { self.open_job2(fi, func, x, a, 0, job); Converted::Opened }
            Conv::Give(x) => Converted::Give(x),
            Conv::Quiet => Converted::Quiet,
        })
    }

    /* Carries the `Convert` job on top on from `v` (None at start) through nested conversions to the next dunder run as a frame, or to its end. */
    fn convert_on(&mut self, mut v: Option<Val>, chunk: &SSAChunk) -> Result<Conv, VmErr> {
        loop {
            let Some(Job::Convert(_, then, _)) = self.jobs.last_mut() else { return Err(cold_runtime("a conversion without its job")) };
            let p: *mut dyn Resume = &mut **then;
            // SAFETY the job's box stays put while the job is on the jobs, which only its own run pops, and `resume` leaves the jobs past it as it found them.
            let (x, c) = match unsafe { (*p).resume(self, v) }? {
                Next::Give(x) => return Ok(Conv::Give(x)),
                Next::Quiet => return Ok(Conv::Quiet),
                Next::Convert(x, c) => (x, c),
            };
            // An instance of the class whose dunder ran last runs the same one, as a frame while it still opens one.
            let memo = match self.jobs.last() { Some(Job::Convert(_, _, m)) => *m, _ => Memo::none() };
            let once = memo.few;
            let class = match self.heap.try_get(x) { Some(&HeapObj::Instance(k, _)) => k, _ => self.instance_class(x).unwrap_or(Val::undef()) };
            let found = if !class.is_undef() && class.0 == memo.class.0 && core::ptr::eq(memo.name, c.dunder()) && self.budget != 0 {
                let n = 1 + c.arg().is_some() as usize;
                (if once { self.job_fn_few(memo.func, n, true) } else { self.job_fn_as(memo.func, n, false) }).map(|fi| (fi, memo.func))
            } else {
                let f = self.convert_fn(x, c, false, once);
                if let Some((_, func)) = f && let Some(Job::Convert(_, _, m)) = self.jobs.last_mut() { *m = Memo { class, name: c.dunder(), func, few: m.few }; }
                f
            };
            if let Some((fi, func)) = found {
                if let Some(Job::Convert(check, ..)) = self.jobs.last_mut() { *check = c; }
                return Ok(Conv::Frame(fi, func, x, c.arg()));
            }
            v = Some(self.with_roots([x], |vm| vm.convert_now(x, c, chunk))?);
        }
    }

    /* Whether `v` is an instance of a user class. */
    #[inline(always)]
    pub(crate) fn is_instance(&self, v: Val) -> bool { v.is_heap() && matches!(self.heap.get(v), HeapObj::Instance(..)) }

    /* Whether `o` is a generator or an instance, whose items a builtin's job may take as frames of the loop. */
    #[inline(always)]
    pub(crate) fn may_job(&self, o: Val) -> bool { o.is_heap() && matches!(self.heap.get(o), HeapObj::Coroutine(_) | HeapObj::Instance(..) | HeapObj::Iter(..)) }

    /* `gather_job` for a generator `o`, `pull_job` for an instance, a builtin of one argument taking their items. */
    #[cold]
    #[inline(never)]
    pub(crate) fn items_job(&mut self, o: Val, takes: Takes, chunk: &SSAChunk) -> Result<bool, VmErr> {
        match self.heap.get(o) {
            HeapObj::Coroutine(_) => self.gather_job(o, takes, Vec::new()),
            HeapObj::Iter(..) => Ok(self.lazy_pull(o, takes, &[], chunk)? || self.pull_map(o, takes, &[])? || self.drive_iter(o, takes, &[])?),
            _ => self.pull_job(o, takes, &[], chunk),
        }
    }

    /* `gather_job` when `o` is a generator, `pull_map` for a `map` or `filter`, the builtin's arguments after it `extra`. */
    #[inline(never)]
    pub(crate) fn gather_gen(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        match self.heap.try_get(o) {
            Some(HeapObj::Coroutine(_)) => self.gather_job(o, takes, extra.to_vec()),
            Some(HeapObj::Iter(..)) => { let chunk = self.chunk; Ok(self.lazy_pull(o, takes, extra, chunk)? || self.pull_map(o, takes, extra)? || self.drive_iter(o, takes, extra)?) }
            _ => Ok(false),
        }
    }

    /* Runs the builtin `takes` stands for over `o`, a Python function's `map` or `filter` on a plain iterator, its calls as frames, Ok(false) otherwise. */
    #[inline(never)]
    pub(crate) fn pull_map(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        if self.budget == 0 { return Ok(false); }
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(o) else { return Ok(false) };
        let (f, src, src2, filter) = match &*frame.borrow() {
            IterFrame::Map { f, its, .. } if its.len() == 1 => (*f, its[0], Val::undef(), false),
            IterFrame::Map { f, its, .. } if its.len() == 2 => (*f, its[0], its[1], false),
            &IterFrame::Filter { f, it, .. } if !f.is_none() => (f, it, Val::undef(), true),
            _ => return Ok(false),
        };
        let two = !src2.is_undef();
        // Both a two-iterator map steps must step without user code, or neither is stepped here.
        if two && !(self.plain_iter(src) && self.plain_iter(src2)) { return Ok(false); }
        let Some(fi) = (if two { self.job_fn2(f) } else { self.job_fn(f) }) else { return Ok(false) };
        let first = match self.pull_next(src, src2) {
            None => return Ok(false),
            Some(r) => r?,
        };
        let mut g = self.gather_of(takes, extra.to_vec(), o);
        let Some((x, y)) = first else {
            // An empty one gives what the builtin gives of no items.
            let r = self.gather_end(&mut g, Val::none(), self.chunk)?;
            self.push(r);
            return Ok(true);
        };
        g.pull = Pull { src, src2, filter, item: x };
        if two { self.open_job2(fi, f, x, y, 0, Job::Gather(Box::new(g))); } else { self.key_job(fi, f, x, Job::Gather(Box::new(g))); }
        Ok(true)
    }

    /* The op at `rip` on a stacked `map` or `filter` whose steps run Python, a job taking its items, its end putting an iterator there and rerunning the op. */
    #[inline(never)]
    pub(crate) fn list_job(&mut self, rip: usize) -> Result<bool, VmErr> {
        // An op at the code's start names no place to run again at, a link's empty one.
        let Some(&it) = self.stack.last() else { return Ok(false) };
        if rip == 0 || !self.holds_py(it, 0, true) { return Ok(false); }
        self.stack.pop();
        let chunk = self.chunk;
        if !self.with_roots([it], |vm| vm.items_job(it, Takes::All(OpCode::GetIter, 1), chunk))? { self.push(it); return Ok(false); }
        if self.floor_req { self.floor_back(rip); return Ok(true); }
        if !self.entered { return Ok(false); }
        self.set_ret(rip, NO_DEST);
        Ok(true)
    }

    /* `list += it` with builtin iterator `it` whose steps run Python, its items appended as frames of the loop, Ok(false) with the operands left otherwise. */
    #[inline(never)]
    pub(crate) fn extend_job(&mut self, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let n = self.stack.len();
        if n < 2 { return Ok(false); }
        let (l, it) = (self.stack[n - 2], self.stack[n - 1]);
        if !matches!(self.heap.try_get(it), Some(HeapObj::Iter(..))) || !matches!(self.heap.plain(l), Some(HeapObj::List(_))) || !self.holds_py(it, 0, true) { return Ok(false); }
        self.stack.truncate(n - 2);
        if self.with_roots([l, it], |vm| vm.items_job(it, Takes::Extend(l), chunk))? { return Ok(true); }
        self.stack.extend_from_slice(&[l, it]);
        Ok(false)
    }

    /* `in` or `not in` over builtin iterator `o`, the job stepping it itself so the items' `__eq__` runs as folds, as over a generator. */
    #[inline(never)]
    pub(crate) fn contains_job(&mut self, o: Val, takes: Takes, chunk: &SSAChunk) -> Result<bool, VmErr> {
        Ok(self.lazy_pull(o, takes, &[], chunk)? || self.drive_iter(o, takes, &[])?)
    }

    /* Runs the builtin `takes` stands for over builtin iterator `o` whose steps run Python, each call a step makes a frame first, Ok(false) for none. */
    #[inline(never)]
    pub(crate) fn drive_iter(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        // Those whose list a generator's end hands on take it only where a generator's frame ends, a method's list here too.
        if matches!(takes, Takes::Retry(..) | Takes::Prefetch(_) | Takes::Close | Takes::Hold) { return Ok(false); }
        if self.budget == 0 || !self.holds_py(o, 0, true) { return Ok(false); }
        let depth = self.jobs.len();
        self.jobs.push(Job::Gather(Box::new(self.gather_of(takes, extra.to_vec(), o))));
        let drive = self.gather_drive();
        if drive.is_err() { self.jobs.truncate(depth); }
        match drive? {
            Drive::Done(r) => self.push(r),
            Drive::Fold(fi, func, d, args, n) => { self.open_fold(fi, func, d, args, n); (self.entered, self.yielded) = (true, true); }
            Drive::Opened => (self.entered, self.yielded) = (true, true),
        }
        Ok(true)
    }

    /* `gather_drive` for the builtin whose fold just ran, a step that may open a frame left as `Done(undef)` so `drive_on` steps once the frame is left. */
    #[inline(never)]
    pub(crate) fn fold_drive(&mut self) -> Result<Drive, VmErr> {
        match self.jobs.last() {
            Some(Job::Gather(j)) if !(j.cur.is_some() || !j.gens || j.native || self.plain_iter(j.co)) => Ok(Drive::Done(Val::undef())),
            _ => self.gather_drive(),
        }
    }

    /* What `fold_drive` gave, the step it left done now. */
    #[inline(never)]
    pub(crate) fn drive_on(&mut self, out: Result<Drive, VmErr>) -> Result<Drive, VmErr> {
        match out { Ok(Drive::Done(v)) if v.is_undef() => self.gather_drive(), out => out }
    }

    /* Whether `it` is a builtin iterator that steps without user code. */
    fn plain_iter(&self, it: Val) -> bool {
        matches!(self.heap.try_get(it), Some(HeapObj::Iter(f, _)) if f.try_borrow().is_ok_and(|f| !matches!(*f, IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. } | IterFrame::Call { .. } | IterFrame::Coroutine(_) | IterFrame::UserDefined(_) | IterFrame::Under(_))))
    }

    /* The next items of the one or two iterators a map steps, `src2` undef for one, None when one does not step plainly. */
    fn pull_next(&mut self, src: Val, src2: Val) -> Option<Result<Option<(Val, Val)>, VmErr>> {
        let x = match self.plain_next(src)? { Ok(Some(x)) => x, Ok(None) => return Some(Ok(None)), Err(e) => return Some(Err(e)) };
        if src2.is_undef() { return Some(Ok(Some((x, Val::undef())))); }
        Some(self.plain_next(src2)?.map(|y| y.map(|y| (x, y))))
    }

    /* What the `map` or `filter` job on top makes of its function's result `v`, the item taken or skipped, then a rerun or the result once spent. */
    fn pull_take(&mut self, v: Val, chunk: &SSAChunk) -> Result<Step, VmErr> {
        let Some(Job::Gather(j)) = self.jobs.last() else { return Err(cold_runtime("a job frame without its job")) };
        let Pull { src, src2, filter, item } = j.pull;
        let take = if !filter { Some(v) } else if self.truthy_op(v, chunk)? { Some(item) } else { None };
        // An item whose dunder the builtin runs hands the job to its own stepping of the iterator, the dunder a fold and the steps' calls its `Feed`s.
        if let Some(x) = take && self.fold_of(x, NATIVE).is_some() && let Some(Job::Gather(j)) = self.jobs.last_mut() {
            (j.pull.item, j.pull.src, j.native, j.gens) = (x, Val::undef(), false, true);
            // The frame's end takes a stand-in off the jobs in the job's place, as a `Feed`'s end takes the `Feed`.
            self.jobs.push(Job::IterOf);
            return Ok(Step::Drive);
        }
        let took = match take { Some(x) => self.gather_take(x)?, None => None };
        if let Some(r) = took { return Ok(Step::Give(r)); }
        self.charge_step()?;
        match self.pull_next(src, src2) {
            Some(Ok(Some((x, y)))) => {
                if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.pull.item = x; }
                Ok(if src2.is_undef() { Step::Again(x) } else { Step::Again2(x, y) })
            }
            Some(Ok(None)) => {
                let Some(Job::Gather(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
                // A builtin that runs Python on the items gathered runs once this frame is left, as the job's drive ends it.
                if matches!(j.takes, Takes::Collect(..)) { return Ok(Step::Drive); }
                let mut g = core::mem::replace(&mut **j, Gather::new(Takes::Any, Vec::new()));
                Ok(Step::Give(self.gather_end(&mut g, Val::none(), chunk)?))
            }
            Some(Err(e)) => Err(e),
            None => Err(cold_runtime("a map's iterator no longer steps")),
        }
    }

    /* Runs the builtin or method the items a generator gave `j` were gathered for, its result, or None when it opened a frame or floor. */
    #[inline(never)]
    pub(crate) fn collected(&mut self, j: Gather, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        match j.takes {
            Takes::Collect(id, operand, slot) => {
                self.job_ok = true;
                let ran = self.native_collected(id, operand, slot as usize, j.items, &j.extra, chunk);
                self.job_ok = false;
                ran?;
                if self.entered || self.floor_req { return Ok(None); }
            }
            // Another generator among its arguments runs as frames of the loop the same way.
            Takes::Method(id, i) => {
                self.job_ok = true;
                let ran = self.method_collected(id, i as usize, j.items, j.extra, chunk);
                self.job_ok = false;
                ran?;
                if self.entered || self.floor_req { return Ok(None); }
            }
            Takes::Hold => {
                self.hold_items(j.co, j.items);
                self.hold_then(j.extra)?;
                if self.entered || self.floor_req { return Ok(None); }
            }
            _ => return Err(cold_runtime("a gathered list for no builtin")),
        }
        self.pop().map(Some)
    }

    /* Marks the generator frame on top as a job's. */
    pub(crate) fn mark_job(&mut self) {
        let at = self.head_at(self.top_pool);
        self.regs[at] = Val::int(self.head(at).as_int() | JOB << 40);
    }

    /* Sets where the caller of the job's generator frame on top carries on, `ret` past the builtin's call. */
    #[inline(never)]
    pub(crate) fn gather_ret(&mut self, ret: usize) {
        let at = self.head_at(self.top_pool);
        let link = self.head(at).as_int();
        if link & 0xFF_FFFF == 0 { self.regs[at] = Val::int(link | ret as i64); }
    }

    /* Whether the running frame is a generator a builtin's job runs. */
    #[inline(always)]
    pub(crate) fn top_gather(&self) -> bool { self.acts != 0 && self.head(self.head_at(self.top_pool)).as_int() >> 40 & (JOB | super::act::GEN) == JOB | super::act::GEN }

    /* Takes `v`, the job's generator's yield, in the loop with no user code and no failure, false with nothing changed for the full path. */
    #[inline(always)]
    pub(crate) fn gather_fast(&mut self, v: Val) -> bool {
        if self.budget == 0 || self.heap.has_work() { return false; }
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return false };
        if !take_plain(j, v, &self.heap) { return false; }
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        true
    }

    /* Takes `v`, which the job's generator yielded with the generator suspended, Some with the builtin's result once that ends it. */
    pub(crate) fn gather_take(&mut self, v: Val) -> Result<Option<Val>, VmErr> {
        self.charge_step()?;
        let Some(Job::Gather(j)) = self.jobs.last() else { return Err(cold_runtime("a job frame without its job")) };
        let takes = j.takes;
        let chunk_of = |vm: &Self| -> &'a SSAChunk { vm.chunk };
        let truth = |vm: &mut Self| { let chunk = vm.chunk; vm.push(v); let t = vm.truthy_op(v, chunk); vm.pop()?; t };
        Ok(match takes {
            Takes::All(..) | Takes::Collect(..) | Takes::Method(..) | Takes::Retry(..) | Takes::Hold => {
                let n = j.items.len();
                if (n + 1).saturating_mul(core::mem::size_of::<Val>()) >= self.heap.room() { return Err(crate::vm::cold_heap()); }
                if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.items.push(v); }
                None
            }
            Takes::Any => truth(self)?.then_some(Val::bool(true)),
            Takes::Every => (!truth(self)?).then_some(Val::bool(false)),
            Takes::Next(_) | Takes::Prefetch(_) => Some(v),
            Takes::Await | Takes::AwaitFor => { self.await_back = 1; Some(v) }
            Takes::Agen => { self.await_back = 4; Some(v) }
            Takes::Then(b) => { self.then_out = Some((b, Ok(crate::vm::globals::async_ops::Step::Yield(v)))); Some(v) }
            Takes::Close => return Err(VmErr::Raised("RuntimeError: generator ignored GeneratorExit".into())),
            Takes::Contains(item, not) => {
                let base = self.temp_roots.len();
                self.temp_roots.push(v);
                // The item compares first, as CPython's `in` over an iterator compares.
                let hit = self.eq_op(v, item, chunk_of(self));
                self.temp_roots.truncate(base);
                hit?.then_some(Val::bool(!not))
            }
            Takes::Extend(l) => {
                if let Some(HeapObj::List(rc)) = self.heap.try_get(l) { rc.borrow_mut().push(v); }
                None
            }
            Takes::Sum => { self.sum_take(v, chunk_of(self))?; None }
            // A key no frame runs is called as a builtin's is, its key then taken as a frame's would be.
            Takes::Keyed(_, key) => {
                let base = self.temp_roots.len();
                self.temp_roots.push(v);
                self.push(key);
                self.push(v);
                let kv = self.exec_call(1, chunk_of(self)).and_then(|_| self.pop());
                self.temp_roots.truncate(base);
                let d = Fold { item: v, next: Val::undef(), resume: 0, kind: Folds::Key, item_first: true };
                return match self.key_taken(d, kv?, false)? { FoldStep::Done(r) => Ok(Some(r)), _ => Ok(None) };
            }
            Takes::Extreme(is_min) => {
                let best = j.acc;
                if (best.is_undef() || {
                    let base = self.temp_roots.len();
                    self.temp_roots.extend([v, best]);
                    let w = self.beats(v, best, is_min, chunk_of(self));
                    self.temp_roots.truncate(base);
                    w?
                }) && let Some(Job::Gather(j)) = self.jobs.last_mut() { j.acc = v; }
                None
            }
        })
    }

    /* Adds `v` to the `sum` job's total as CPython 3.15 does, word ints exactly, then compensated floats, then anything through `+`, never going back. */
    fn sum_take(&mut self, v: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        let Some(Job::Gather(j)) = self.jobs.last() else { return Ok(()) };
        let (total, acc) = self.sum_on(j.total, j.acc, v, chunk)?;
        if let Some(Job::Gather(j)) = self.jobs.last_mut() { (j.total, j.acc) = (total, acc); }
        Ok(())
    }

    /* The total of a `sum` that went as far as `total`, `acc` its value past the floats, once `v` adds to it. */
    fn sum_on(&mut self, total: Total, acc: Val, v: Val, chunk: &SSAChunk) -> Result<(Total, Val), VmErr> {
        let word = |vm: &Self, v: Val| if v.is_int() || v.is_bool() || v.is_foreign() { vm.as_i128(v).and_then(|n| i64::try_from(n).ok()) } else { None };
        let (total, acc) = match total {
            Total::Word(w) => match word(self, v).and_then(|b| w.checked_add(b)) {
                Some(n) => (Total::Word(n), acc),
                None => {
                    let a = self.int_to_val(Some(w as i128))?;
                    let a = self.sum_through(a, v, chunk)?;
                    if a.is_float() { (Total::Float(a.as_float(), 0.0), a) } else { (Total::Any, a) }
                }
            },
            Total::Float(s, c) if v.is_float() => { let (s, c) = super::globals::numeric::neumaier(s, c, v.as_float()); (Total::Float(s, c), acc) }
            Total::Float(s, c) => match word(self, v) {
                Some(b) => (Total::Float(s + b as f64, c), acc),
                None => (Total::Any, self.sum_through(Val::float(super::globals::numeric::settle((s, c))), v, chunk)?),
            },
            Total::Any => (Total::Any, self.sum_through(acc, v, chunk)?),
        };
        Ok((total, acc))
    }

    /* `a + b` for `sum`, both rooted while a user `__add__` runs. */
    pub(crate) fn sum_through(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let base = self.temp_roots.len();
        self.temp_roots.extend([a, b]);
        let r = self.sum_add(a, b, chunk);
        self.temp_roots.truncate(base);
        r
    }

    /* The dunders `a` and `b` call for `lname` (`rname` reflected) in `dispatch_reflected`'s order, each a plain function with its operand and the other. */
    fn reflected_plan(&self, a: Val, b: Val, lname: &str, rname: &str, arith: bool) -> ([(Val, Val, Val); 2], usize) {
        let class = |x: Val| match self.heap.try_get(x) { Some(&HeapObj::Instance(c, _)) => Some(c), _ => None };
        let b_overrides = match (class(a), class(b)) {
            (Some(ac), Some(bc)) => ac.0 != bc.0 && self.heap.is_subclass(bc, ac)
                && (!arith || self.lookup_class_member(bc, rname).map(|m| m.0.0) != self.lookup_class_member(ac, rname).map(|m| m.0.0)),
            _ => false,
        };
        let calls = if b_overrides { [(b, rname, a), (a, lname, b)] } else { [(a, lname, b), (b, rname, a)] };
        let mut out = [(Val::undef(), Val::undef(), Val::undef()); 2];
        let mut n = 0;
        for (recv, name, arg) in calls {
            let Some(c) = class(recv) else { continue };
            let Some((func, _)) = self.lookup_class_member(c, name) else { continue };
            if func.is_heap() && matches!(self.heap.get(func), HeapObj::Func(..)) { out[n] = (func, recv, arg); n += 1; }
        }
        (out, n)
    }

    /* The frame of the dunder `func` takes, None when it runs only nested. */
    fn fold_fn(&mut self, func: Val, n: usize) -> Option<usize> { if n == 2 { self.job_fn2(func) } else { self.job_fn(func) } }

    /* The user dunder the job on top calls on `v` as a frame, its function, step and one or two arguments, None when `v` takes no user code or frame. */
    pub(crate) fn fold_of(&mut self, v: Val, resume: usize) -> Option<(usize, Val, Fold, [Val; 2], usize)> {
        let Some(Job::Gather(j)) = self.jobs.last() else { return None };
        let (takes, total, acc) = (j.takes, j.total, j.acc);
        let is_instance = |vm: &Self, x: Val| matches!(vm.heap.try_get(x), Some(HeapObj::Instance(..)));
        let fold = |kind, item_first: bool, next: Val| Fold { kind, item: v, resume: resume as u32, next, item_first };
        let (plan, kind, lhs, rhs) = match takes {
            Takes::Sum => {
                if !is_instance(self, v) && !is_instance(self, acc) { return None; }
                // The total so far as a value, an int of the word phase or the float of the float phase.
                let a = match total {
                    Total::Word(w) if !(Val::INT_MIN..=Val::INT_MAX).contains(&w) => return None,
                    Total::Word(w) => Val::int(w),
                    Total::Float(s, c) => Val::float(super::globals::numeric::settle((s, c))),
                    Total::Any => acc,
                };
                (self.reflected_plan(a, v, "__add__", "__radd__", true), Folds::Add, a, v)
            }
            Takes::Extreme(is_min) => {
                if acc.is_undef() { return None; }
                // Two lists or tuples walk their pairs, the first an instance makes take its `__eq__` as the fold.
                if !is_instance(self, v) && !is_instance(self, acc) { return self.lex_call(v, v, acc, 0, is_min, false, resume); }
                let (l, r) = if is_min { ("__lt__", "__gt__") } else { ("__gt__", "__lt__") };
                (self.reflected_plan(v, acc, l, r, false), Folds::Wins(is_min), v, acc)
            }
            Takes::Keyed(_, key) => {
                // A class key's instance is made here and its `__init__` runs as the frame, the instance the key.
                if let Some((init, class)) = self.key_init(key) {
                    let fi = self.job_fn2(init)?;
                    let inst = self.new_instance(class, 0).ok()?;
                    return Some((fi, init, fold(Folds::Key, true, inst), [inst, v], 2));
                }
                let fi = self.job_fn(key)?;
                return Some((fi, key, fold(Folds::Key, true, Val::undef()), [v, Val::undef()], 1));
            }
            // An item compares with the value sought as CPython's `in` compares them, the item first.
            Takes::Contains(x, not) => {
                if v.0 == x.0 { return None; }
                if !is_instance(self, v) && !is_instance(self, x) {
                    // Two lists or tuples walk their pairs, the first an instance makes take its `__eq__` as the fold.
                    let EqScan::Dunder(k) = self.eq_scan(v, x, 0) else { return None };
                    let (a, b) = self.eq_pair(v, x, k)?;
                    let (calls, n) = self.reflected_plan(a, b, "__eq__", "__eq__", false);
                    if n == 0 { return None; }
                    let fi = self.fold_fn(calls[0].0, 2)?;
                    let next = if n > 1 { calls[1].0 } else { Val::undef() };
                    return Some((fi, calls[0].0, fold(Folds::Eq(not, k as u16 + 1), calls[0].1.0 == a.0, next), [calls[0].1, calls[0].2], 2));
                }
                (self.reflected_plan(v, x, "__eq__", "__eq__", false), Folds::Eq(not, 0), v, x)
            }
            Takes::Any | Takes::Every => {
                let Some(&HeapObj::Instance(c, _)) = self.heap.try_get(v) else { return None };
                let (name, kind) = match self.lookup_class_member(c, "__bool__") { Some(_) => ("__bool__", Folds::Truth), None => ("__len__", Folds::Len) };
                let func = self.lookup_class_member(c, name)?.0;
                if !(func.is_heap() && matches!(self.heap.get(func), HeapObj::Func(..))) { return None; }
                let fi = self.job_fn(func)?;
                return Some((fi, func, fold(kind, true, Val::undef()), [v, Val::undef()], 1));
            }
            _ => return None,
        };
        let (calls, n) = plan;
        let _ = (lhs, rhs);
        if n == 0 { return None; }
        let (func, recv, arg) = calls[0];
        let fi = self.fold_fn(func, 2)?;
        // The reflected one, if any, is called on the operands the other way round.
        let next = if n > 1 { calls[1].0 } else { Val::undef() };
        Some((fi, func, fold(kind, recv.0 == v.0, next), [recv, arg], 2))
    }

    /* Takes `r`, the dunder result of fold `d`, into the job on top, Go while it takes more, Done with its result, Call with the reflected dunder. */
    #[inline(never)]
    pub(crate) fn fold_take(&mut self, d: Fold, r: Val) -> Result<FoldStep, VmErr> {
        let not_impl = self.heap.is_not_implemented(r);
        self.fold_answer(d, (!not_impl).then_some(r))
    }

    /* The operand of the op a fold's dunder runs other than its item, the total so far or the best item. */
    fn fold_other(&self, d: Fold) -> Val {
        let Some(Job::Gather(j)) = self.jobs.last() else { return Val::none() };
        match (d.kind, j.total) {
            (Folds::WinsKey(_) | Folds::Lex { keyed: true, .. }, _) => j.best_key,
            (Folds::Eq(..), _) => match j.takes { Takes::Contains(x, _) => x, _ => Val::none() },
            (Folds::Add, Total::Word(w)) => Val::int(w),
            (Folds::Add, Total::Float(s, c)) => Val::float(super::globals::numeric::settle((s, c))),
            _ => j.acc,
        }
    }

    /* Where the `==` walk of sequences `a` and `b` goes from pair `from`, an instance pair, their equal end, a difference, or containers to walk. */
    fn eq_scan(&self, a: Val, b: Val, from: usize) -> EqScan {
        // Lists of other lengths differ at once, tuples once their shorter one ends, as CPython compares them.
        if matches!(self.heap.plain(a), Some(HeapObj::List(x)) if matches!(self.heap.plain(b), Some(HeapObj::List(y)) if x.borrow().len() != y.borrow().len())) { return EqScan::Differ; }
        match self.pair_scan(a, b, from) {
            Some(PairScan::Dunder(k)) => EqScan::Dunder(k),
            Some(PairScan::Deep(k)) => EqScan::Deep(k),
            Some(PairScan::Ended) if self.seq_vals(a).map(|x| x.len()) == self.seq_vals(b).map(|y| y.len()) => EqScan::Equal,
            _ => EqScan::Differ,
        }
    }

    /* The `__eq__` the `min` or `max` on top runs as `item`'s fold on the next instance pair of `mine` and `other` from `from`, None for none. */
    #[allow(clippy::too_many_arguments)]
    fn lex_call(&mut self, item: Val, mine: Val, other: Val, from: usize, min: bool, keyed: bool, resume: usize) -> Option<(usize, Val, Fold, [Val; 2], usize)> {
        let Some(PairScan::Dunder(k)) = self.pair_scan(mine, other, from) else { return None };
        let (a, b) = self.eq_pair(mine, other, k)?;
        let (calls, n) = self.reflected_plan(a, b, "__eq__", "__eq__", false);
        if n == 0 { return None; }
        let fi = self.fold_fn(calls[0].0, 2)?;
        let next = if n > 1 { calls[1].0 } else { Val::undef() };
        let fold = Fold { kind: Folds::Lex { min, at: k as u16 + 1, decide: false, keyed }, item, resume: resume as u32, next, item_first: calls[0].1.0 == a.0 };
        Some((fi, calls[0].0, fold, [calls[0].1, calls[0].2], 2))
    }

    /* The walk of `mine` and `other` from their pair `from` on, compared nested, taken by the `min` or `max` on top. */
    #[allow(clippy::too_many_arguments)]
    fn lex_rest(&mut self, item: Val, mine: Val, other: Val, from: usize, min: bool, keyed: bool) -> Result<FoldStep, VmErr> {
        let (Some(xs), Some(ys)) = (self.seq_vals(mine), self.seq_vals(other)) else { return self.lex_won(item, keyed, false) };
        let chunk = self.chunk;
        let base = self.temp_roots.len();
        self.temp_roots.extend([item, mine, other]);
        let mut wins = Ok(if min { xs.len() < ys.len() } else { xs.len() > ys.len() });
        // The walk goes no further than where it stops with no user code.
        let from = match self.pair_scan(mine, other, from) { Some(PairScan::Differ(k) | PairScan::Dunder(k) | PairScan::Deep(k)) => k, _ => xs.len().min(ys.len()) };
        for (&x, &y) in xs.iter().zip(&ys).skip(from) {
            match self.member_eq(x, y, chunk) {
                Ok(true) => continue,
                Ok(false) => wins = self.beats(x, y, min, chunk),
                Err(e) => wins = Err(e),
            }
            break;
        }
        self.temp_roots.truncate(base);
        self.lex_won(item, keyed, wins?)
    }

    /* Takes whether `item` won its comparison into the `min` or `max` on top, its key pending the best key when `keyed`. */
    fn lex_won(&mut self, item: Val, keyed: bool, wins: bool) -> Result<FoldStep, VmErr> {
        if wins && let Some(Job::Gather(j)) = self.jobs.last_mut() {
            j.acc = item;
            if keyed { j.best_key = j.pending; }
        }
        Ok(FoldStep::Go)
    }

    /* The items of the builtin list or tuple `v`, None for any other. */
    fn seq_vals(&self, v: Val) -> Option<Vec<Val>> {
        match self.heap.plain(v) { Some(HeapObj::List(x)) => Some(x.borrow().clone()), Some(HeapObj::Tuple(x)) => Some(x.to_vec()), _ => None }
    }

    /* The pair `k` of the lists or tuples `a` and `b`. */
    fn eq_pair(&self, a: Val, b: Val, k: usize) -> Option<(Val, Val)> {
        Some((*self.seq_vals(a)?.get(k)?, *self.seq_vals(b)?.get(k)?))
    }

    /* Whether the pairs of `a` and `b` from `k` on are all equal, compared nested. */
    fn eq_rest(&mut self, a: Val, b: Val, k: usize) -> Result<bool, VmErr> {
        let (Some(xs), Some(ys)) = (self.seq_vals(a), self.seq_vals(b)) else { return Ok(false) };
        let chunk = self.chunk;
        for (&x, &y) in xs.iter().zip(&ys).skip(k) { if !self.with_roots([a, b], |vm| vm.eq_op(x, y, chunk))? { return Ok(false); } }
        Ok(xs.len() == ys.len())
    }

    /* `fold_take` of the answer `r` of the fold `d`'s dunder, None for a NotImplemented. */
    fn fold_answer(&mut self, d: Fold, r: Option<Val>) -> Result<FoldStep, VmErr> {
        let not_impl = r.is_none();
        let r = r.unwrap_or(Val::none());
        let other = self.fold_other(d);
        // What the op compares in the item's place, the item or its pending key, a walk's pair standing for both sides.
        let mine = match (d.kind, self.jobs.last()) { (Folds::WinsKey(_) | Folds::Lex { keyed: true, .. }, Some(Job::Gather(j))) => j.pending, _ => d.item };
        let (whole, whole_other) = (mine, other);
        let (mine, other) = match d.kind { Folds::Eq(_, at) | Folds::Lex { at, .. } if at > 0 => self.eq_pair(mine, other, at as usize - 1).unwrap_or((mine, other)), _ => (mine, other) };
        let (lhs, rhs) = match d.kind { Folds::Add => (other, mine), _ => (mine, other) };
        if not_impl && !d.next.is_undef() {
            let next = Fold { next: Val::undef(), item_first: !d.item_first, ..d };
            let args = if d.item_first { [other, mine] } else { [mine, other] };
            // The reflected dunder no frame runs is called nested, its answer taken as a frame's would be.
            match self.fold_fn(d.next, 2) {
                Some(fi) => return Ok(FoldStep::Call(fi, d.next, next, args, 2)),
                None => {
                    let chunk = self.chunk;
                    let base = self.temp_roots.len();
                    self.temp_roots.extend([other, mine, d.item]);
                    let name = match d.kind { Folds::Add => "__radd__", Folds::Eq(..) | Folds::Lex { decide: false, .. } => "__eq__", Folds::Wins(true) | Folds::WinsKey(true) | Folds::Lex { min: true, .. } => "__gt__", _ => "__lt__" };
                    let got = self.try_call_dunder(args[0], name, &[args[1]], chunk);
                    self.temp_roots.truncate(base);
                    return self.fold_answer(next, got?);
                }
            }
        }
        let Some(Job::Gather(j)) = self.jobs.last() else { return Err(cold_runtime("a fold without its job")) };
        let takes = j.takes;
        let step = match d.kind {
            Folds::Add => {
                // With neither dunder answering, the op runs as the builtins make it, which for two instances raises.
                let r = if not_impl { self.add_vals(lhs, rhs)? } else { r };
                if let Some(Job::Gather(j)) = self.jobs.last_mut() { (j.acc, j.total) = (r, Total::Any); }
                FoldStep::Go
            }
            Folds::Wins(is_min) => {
                if not_impl {
                    let op = if is_min { "<" } else { ">" };
                    return Err(VmErr::TypeMsg(crate::s!("'", str op, "' not supported between instances of '", str self.type_name(lhs), "' and '", str self.type_name(rhs), "'")));
                }
                if self.truthy(r) && let Some(Job::Gather(j)) = self.jobs.last_mut() { j.acc = d.item; }
                FoldStep::Go
            }
            Folds::Key if !d.next.is_undef() => {
                if !r.is_none() { return Err(cold_type("__init__() should return None")); }
                return self.key_taken(d, d.next, true);
            }
            Folds::Key => return self.key_taken(d, r, true),
            Folds::WinsKey(is_min) => {
                if not_impl {
                    let op = if is_min { "<" } else { ">" };
                    return Err(VmErr::TypeMsg(crate::s!("'", str op, "' not supported between instances of '", str self.type_name(lhs), "' and '", str self.type_name(rhs), "'")));
                }
                if self.truthy(r) && let Some(Job::Gather(j)) = self.jobs.last_mut() { (j.acc, j.best_key) = (d.item, j.pending); }
                FoldStep::Go
            }
            Folds::Eq(not, at) => {
                // With neither dunder answering, the two are equal when one object, and a walk goes on to the next instance pair or ends with its pairs.
                if !(if not_impl { lhs.0 == rhs.0 } else { self.truthy(r) }) { return Ok(FoldStep::Go); }
                if at == 0 { return Ok(FoldStep::Done(Val::bool(!not))); }
                let other = self.fold_other(d);
                match self.eq_scan(d.item, other, at as usize) {
                    EqScan::Equal => FoldStep::Done(Val::bool(!not)),
                    EqScan::Differ => FoldStep::Go,
                    EqScan::Dunder(k) => {
                        let Some((a, b)) = self.eq_pair(d.item, other, k) else { return Ok(FoldStep::Go) };
                        let (calls, n) = self.reflected_plan(a, b, "__eq__", "__eq__", false);
                        let next = if n > 1 { calls[1].0 } else { Val::undef() };
                        let fold = Fold { kind: Folds::Eq(not, k as u16 + 1), next, item_first: calls[0].1.0 == a.0, ..d };
                        match (n, self.fold_fn(calls[0].0, 2)) {
                            (1.., Some(fi)) => return Ok(FoldStep::Call(fi, calls[0].0, fold, [calls[0].1, calls[0].2], 2)),
                            // One no frame runs compares the rest nested.
                            _ => if self.eq_rest(d.item, other, k)? { FoldStep::Done(Val::bool(!not)) } else { FoldStep::Go },
                        }
                    }
                    EqScan::Deep(k) => if self.eq_rest(d.item, other, k)? { FoldStep::Done(Val::bool(!not)) } else { FoldStep::Go },
                }
            }
            Folds::Lex { min, at, decide: false, keyed } => {
                // With neither dunder answering, a pair is equal when one object, which a walk passes, and a pair that differs decides by `<` (`>` for `max`).
                if if not_impl { lhs.0 == rhs.0 } else { self.truthy(r) } {
                    if let Some(call) = self.lex_call(d.item, whole, whole_other, at as usize, min, keyed, d.resume()) { return Ok(FoldStep::Call(call.0, call.1, call.2, call.3, call.4)); }
                    return self.lex_rest(d.item, whole, whole_other, at as usize, min, keyed);
                }
                let (l, rn) = if min { ("__lt__", "__gt__") } else { ("__gt__", "__lt__") };
                let (calls, n) = self.reflected_plan(lhs, rhs, l, rn, false);
                if n > 0 && let Some(fi) = self.fold_fn(calls[0].0, 2) {
                    let next = if n > 1 { calls[1].0 } else { Val::undef() };
                    let fold = Fold { kind: Folds::Lex { min, at, decide: true, keyed }, next, item_first: calls[0].1.0 == lhs.0, ..d };
                    return Ok(FoldStep::Call(fi, calls[0].0, fold, [calls[0].1, calls[0].2], 2));
                }
                let chunk = self.chunk;
                let wins = self.with_roots([lhs, rhs], |vm| vm.beats(lhs, rhs, min, chunk))?;
                self.lex_won(d.item, keyed, wins)?
            }
            Folds::Lex { min, keyed, .. } => {
                if not_impl {
                    let op = if min { "<" } else { ">" };
                    return Err(VmErr::TypeMsg(crate::s!("'", str op, "' not supported between instances of '", str self.type_name(lhs), "' and '", str self.type_name(rhs), "'")));
                }
                let wins = self.truthy(r);
                self.lex_won(d.item, keyed, wins)?
            }
            Folds::Truth | Folds::Len => {
                let t = if d.kind == Folds::Truth {
                    if !r.is_bool() { return Err(VmErr::TypeMsg(crate::s!("__bool__ should return bool, returned ", str self.type_name(r)))); }
                    r.as_bool()
                } else { self.len_to_bool(r)? };
                match takes { Takes::Any if t => FoldStep::Done(Val::bool(true)), Takes::Every if !t => FoldStep::Done(Val::bool(false)), _ => FoldStep::Go }
            }
        };
        Ok(step)
    }

    /* Takes `kv`, the key fold `d` found, into the `min` or `max` on top, compared natively when no user code decides, else by its dunder as a frame. */
    fn key_taken(&mut self, d: Fold, kv: Val, frames: bool) -> Result<FoldStep, VmErr> {
        let Some(Job::Gather(j)) = self.jobs.last() else { return Err(cold_runtime("a fold without its job")) };
        let (Takes::Keyed(is_min, _), best) = (j.takes, j.best_key) else { return Err(cold_runtime("a key without its job")) };
        if best.is_undef() {
            if let Some(Job::Gather(j)) = self.jobs.last_mut() { (j.acc, j.best_key) = (d.item, kv); }
            return Ok(FoldStep::Go);
        }
        let is_instance = |vm: &Self, x: Val| matches!(vm.heap.try_get(x), Some(HeapObj::Instance(..)));
        let (l, r) = if is_min { ("__lt__", "__gt__") } else { ("__gt__", "__lt__") };
        let (calls, n) = if frames && (is_instance(self, kv) || is_instance(self, best)) { self.reflected_plan(kv, best, l, r, false) } else { ([(Val::undef(), Val::undef(), Val::undef()); 2], 0) };
        if n > 0 && let Some(fi) = self.fold_fn(calls[0].0, 2) {
            if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.pending = kv; }
            let next = if n > 1 { calls[1].0 } else { Val::undef() };
            let fold = Fold { kind: Folds::WinsKey(is_min), item: d.item, next, item_first: calls[0].1.0 == kv.0, ..d };
            return Ok(FoldStep::Call(fi, calls[0].0, fold, [calls[0].1, calls[0].2], 2));
        }
        // Two keys that are lists or tuples walk their pairs, the first an instance makes take its `__eq__` as the fold.
        if frames && !is_instance(self, kv) && !is_instance(self, best) && let Some(call) = self.lex_call(d.item, kv, best, 0, is_min, true, d.resume()) {
            if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.pending = kv; }
            return Ok(FoldStep::Call(call.0, call.1, call.2, call.3, call.4));
        }
        let (base, chunk) = (self.temp_roots.len(), self.chunk);
        self.temp_roots.extend([kv, best, d.item]);
        let wins = self.beats(kv, best, is_min, chunk);
        self.temp_roots.truncate(base);
        if wins? && let Some(Job::Gather(j)) = self.jobs.last_mut() { (j.acc, j.best_key) = (d.item, kv); }
        Ok(FoldStep::Go)
    }

    /* Opens the dunder `func`, the function `fi`, on `args` as the frame of the fold `d`, over the frame on top. */
    pub(crate) fn open_fold(&mut self, fi: usize, func: Val, d: Fold, args: [Val; 2], n: usize) {
        let job = Job::Fold(d);
        // A dunder that names `super` binds as the method it is.
        let class = match self.heap.try_get(args[0]) { Some(&HeapObj::Instance(c, _)) => c, _ => Val::undef() };
        if self.uses_super(fi) && !class.is_undef() { self.pending.method_binding = Some((self.lookup_class_member(class, self.dunder_name(d)).map_or(class, |m| m.1), args[0])); }
        if n == 2 { self.open_job2(fi, func, args[0], args[1], 0, job); } else { self.open_job(fi, func, args[0], 0, super::act::NO_DEST, job); }
        (self.entered, self.yielded) = (false, false);
    }

    /* The name of the dunder the fold `d` calls first. */
    fn dunder_name(&self, d: Fold) -> &'static str {
        match d.kind { Folds::Add => "__add__", Folds::Wins(true) | Folds::WinsKey(true) => "__lt__", Folds::Wins(false) | Folds::WinsKey(false) => "__gt__", Folds::Truth => "__bool__", Folds::Len => "__len__", Folds::Eq(..) | Folds::Lex { decide: false, .. } => "__eq__", Folds::Lex { min: true, .. } => "__lt__", Folds::Lex { .. } => "__gt__", Folds::Key => "" }
    }

    /* What `next` gives of a generator that returned `ret` before yielding, its default `d`, or with none the StopIteration carrying the value. */
    pub(crate) fn next_end(&mut self, d: Val, ret: Val) -> Result<Val, VmErr> {
        if !d.is_undef() { return Ok(d); }
        if ret.is_none() { return Err(VmErr::Raised(crate::s!("StopIteration"))); }
        let exc = self.heap.alloc(HeapObj::ExcInstance("StopIteration".into(), alloc::vec![ret], Val::undef()))?;
        self.pending.exc_val = Some(exc);
        Err(VmErr::Raised(crate::s!("StopIteration: ", str &self.display(ret))))
    }

    /* The result of the builtin of `j` once its generator returned `ret`, its items taken out of it. */
    pub(crate) fn gather_end(&mut self, j: &mut Gather, ret: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        match j.takes {
            Takes::All(OpCode::CallList, _) => self.alloc_list(core::mem::take(&mut j.items)),
            Takes::All(OpCode::GetIter, _) => {
                let frame = IterFrame::Seq { items: core::mem::take(&mut j.items).into(), idx: 0 };
                self.heap.alloc(HeapObj::Iter(Rc::new(core::cell::RefCell::new(frame)), "list_iterator"))
            }
            Takes::All(op, operand) => {
                let list = self.alloc_list(core::mem::take(&mut j.items))?;
                self.push(list);
                // The builtin itself makes its value of the items, with no job, as it would of a list.
                self.job_ok = false;
                match op {
                    OpCode::CallTuple => self.call_tuple(1, chunk)?,
                    OpCode::CallSet => self.call_set(1)?,
                    OpCode::CallDict => self.call_dict(operand)?,
                    _ => return Err(cold_runtime("a builtin no job gathers for")),
                }
                self.pop()
            }
            Takes::Any => Ok(Val::bool(false)),
            Takes::Every => Ok(Val::bool(true)),
            Takes::Sum => Ok(match j.total {
                Total::Word(w) => return self.int_to_val(Some(w as i128)),
                Total::Float(s, c) => Val::float(super::globals::numeric::settle((s, c))),
                Total::Any => j.acc,
            }),
            Takes::Extreme(is_min) | Takes::Keyed(is_min, _) if j.acc.is_undef() => match j.extra.first() {
                // A `default` given is the result of an empty iterable.
                Some(&d) => Ok(d),
                None => Err(crate::vm::cold_value(if is_min { "min() iterable argument is empty" } else { "max() iterable argument is empty" })),
            },
            Takes::Extreme(_) | Takes::Keyed(..) => Ok(j.acc),
            Takes::Next(d) => self.next_end(d, ret),
            Takes::Contains(_, not) => Ok(Val::bool(not)),
            Takes::Extend(l) => Ok(l),
            Takes::Close => Ok(ret),
            Takes::Await | Takes::AwaitFor => { self.await_back = 2; Ok(ret) }
            Takes::Agen => { self.await_back = 5; Ok(Val::none()) }
            Takes::Then(b) => { self.then_out = Some((b, Ok(crate::vm::globals::async_ops::Step::Return(ret)))); Ok(ret) }
            // A method or builtin the items were gathered for runs on them as a list in its argument's place, a `sorted`'s key opening a frame or floor.
            Takes::Method(id, i) => {
                let (items, call) = (core::mem::take(&mut j.items), core::mem::take(&mut j.extra));
                self.job_ok = true;
                let ran = self.method_collected(id, i as usize, items, call, chunk);
                self.job_ok = false;
                ran?;
                if self.entered || self.floor_req { return Ok(Val::undef()); }
                self.pop()
            }
            Takes::Hold => {
                let (items, extra) = (core::mem::take(&mut j.items), core::mem::take(&mut j.extra));
                self.hold_items(j.co, items);
                self.hold_then(extra)?;
                if self.entered || self.floor_req { return Ok(Val::undef()); }
                self.pop()
            }
            Takes::Collect(id, operand, slot) => {
                let items = core::mem::take(&mut j.items);
                self.job_ok = true;
                let ran = self.native_collected(id, operand, slot as usize, items, &j.extra, chunk);
                self.job_ok = false;
                ran?;
                if self.entered || self.floor_req { return Ok(Val::undef()); }
                self.pop()
            }
            Takes::Retry(..) | Takes::Prefetch(_) => Err(cold_runtime("a collection ended as a builtin's result")),
        }
    }
}

/* Takes `v` into the builtin `j` with no user code and no failure, false with nothing changed when that takes the full path. */
#[inline(always)]
fn take_plain(j: &mut Gather, v: Val, heap: &HeapPool) -> bool {
    match j.takes {
        Takes::Sum => match j.total {
            Total::Word(w) if v.is_int() && let Some(n) = w.checked_add(v.as_int()) => j.total = Total::Word(n),
            Total::Float(s, c) if v.is_float() => j.total = { let (s, c) = super::globals::numeric::neumaier(s, c, v.as_float()); Total::Float(s, c) },
            Total::Float(s, c) if v.is_int() => j.total = Total::Float(s + v.as_int() as f64, c),
            _ => return false,
        },
        Takes::Extreme(is_min) => {
            let best = j.acc;
            let wins = if best.is_undef() { true }
                else if v.is_int() && best.is_int() { if is_min { v.as_int() < best.as_int() } else { v.as_int() > best.as_int() } }
                else if v.is_float() && best.is_float() && !v.as_float().is_nan() && !best.as_float().is_nan() { if is_min { v.as_float() < best.as_float() } else { v.as_float() > best.as_float() } }
                else { return false };
            if wins { j.acc = v; }
        }
        Takes::All(..) | Takes::Collect(..) | Takes::Method(..) | Takes::Retry(..) | Takes::Hold => {
            // Inline ints take no slot, so the items are capped by the memory left, which the full path raises past.
            if (j.items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= heap.room() { return false; }
            j.items.push(v);
        }
        Takes::Any | Takes::Every => {
            let find = j.takes == Takes::Any;
            let truth = if v.is_bool() { Some(v.as_bool()) } else if v.is_int() { Some(v.as_int() != 0) } else if v.is_none() { Some(false) } else { match heap.try_get(v) { Some(HeapObj::Str(s)) => Some(!s.is_empty()), _ => None } };
            if truth != Some(!find) { return false; }
        }
        Takes::Extend(l) => match heap.try_get(l) {
            Some(HeapObj::List(rc)) if (rc.borrow().len() + 1).saturating_mul(core::mem::size_of::<Val>()) < heap.room() => rc.borrow_mut().push(v),
            _ => return false,
        },
        Takes::Next(_) | Takes::Keyed(..) | Takes::Contains(..) | Takes::Close | Takes::Await | Takes::AwaitFor | Takes::Agen | Takes::Then(_) | Takes::Prefetch(_) => return false,
    }
    true
}
