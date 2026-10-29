use alloc::{boxed::Box, rc::Rc, vec::Vec};

use super::VM;
use super::act::{Act, BASES, Dest, JOB, NO_DEST};
use super::frame::Floor;
use super::lower::{self, Code};
use crate::value::*;
use crate::r#unsafe::{CodePtr, ChunkPtr, get_in, open, peek, pinned, step_ready};

/* a builtin whose callback runs as loop frames keeping state until its caller resumes */
pub(crate) enum Job {
    // `map` a `for` steps with `before` the exception pending at start and `ctor` for classes
    Map { before: Val, ctor: bool },
    // the call a step of builtin iterator `target` makes run first as a frame
    Feed { target: Val, item: Val, sentinel: Val, kind: FeedKind, drive: bool },
    // a `filter` a `for` steps with `retake` set when the step reruns its test
    Filter { it: Val, item: Val, before: Val, retake: bool },
    // a builtin over a generator run as a loop frame whose yields the job takes
    Gather(Box<Gather>),
    // the key a builtin over a generator calls on a yielded value as a frame
    Fold(Fold),
    // a stand-in the end of a frame pops once its own job is gone
    Stand,
    // `next` running a generator as a loop frame and the default an ended one gives
    NextGen(Val, Dest),
    // a builtin whose user calls run as loop frames and the exception pending at start
    Calls(Box<dyn Calls>, Val),
    // a native's keys fold over a container whose key runs in place on each item
    Keyed(Box<Keyed>),
}

/* a keys fold's items still to key and the frame its key runs again in */
pub(crate) struct Keyed { pub(crate) fold: Option<Box<super::steps::Folding>>, pub(crate) cur: IterFrame, pub(crate) again: Again, pub(crate) to: Dest }

/* a builtin whose user calls run as frames of a job `step` drives */
pub(crate) trait Calls {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<Call, VmErr>;
    /* every value it holds which a collection keeps */
    fn for_each_val(&self, f: &mut dyn FnMut(Val));
}

/* what a Calls does next a call of up to three arguments or its result */
pub(crate) enum Call { Frame(Val, [Val; 3]), Then(u32, Result<crate::vm::coro::Step, VmErr>), Give(Val) }

/* calls before a job frame counts toward lowering its function so per-object hooks stay unlowered */
const JOB_LOWERS: u32 = 4;

/* a `Frame` call's arguments are those before the first undef */
pub(crate) fn call_args(args: &[Val; 3]) -> &[Val] { &args[..args.iter().position(|a| a.is_undef()).unwrap_or(3)] }

/* the truth of an inline value or str at once and none for any other */
#[inline(always)]
fn bare_truth(heap: &HeapPool, v: Val) -> Option<bool> { heap.bare_truth(v) }

/* what a `Feed`'s call makes of a step as an item or a filter test */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FeedKind { Map, Filter, Call }

/* a key a builtin's job calls on an item with where its generator resumes */
#[derive(Clone, Copy)]
pub(crate) struct Fold { pub(crate) item: Val, next: Val, resume: u32 }

/* `resume` of a native's fold over a container whose caller's frame sits under the fold's */
pub(crate) const NATIVE: usize = u32::MAX as usize - 1;

/* the `resume` of a native fold's key whose value and flow the fold kept */
pub(crate) const TOOK: usize = NATIVE - 1;

/* the slot of a retry whose register op takes its items from `retake` */
pub(crate) const RETAKE: u32 = u32::MAX;

/* what a builtin stepping a container itself does next */
pub(crate) enum Drive { Fold(usize, Val, Fold, [Val; 2], usize), Done(Val, Dest), Opened }

/* the error of an unpack whose generator gave more items than its `limit` takes */
#[cold]
fn unpacked_past(limit: u16) -> VmErr { crate::lang::EngineError::Unpack { want: u32::from(limit).saturating_sub(1), got: None, more: true, rest: false }.into() }

/* what an iterator's next step takes first as `peek` finds it */
pub(crate) enum Peek { Item, End, Gen(Val), Other }

impl Fold {
    /* where the generator under the fold's frame carries on */
    pub(crate) fn resume(&self) -> usize { self.resume as usize }
    /* the key frame of `item` for a native's fold resuming at `resume` */
    pub(crate) fn at(item: Val, resume: usize) -> Self { Self { item, next: Val::undef(), resume: resume as u32 } }
    /* the key frame of `item` for a native's fold over a container */
    pub(crate) fn native(item: Val) -> Self { Self::at(item, NATIVE) }
    /* whether the key is a native's fold over a container taken or not */
    pub(crate) fn is_native(&self) -> bool { self.resume as usize >= TOOK }
}

/* what a builtin a generator runs under makes of what it yields */
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Takes {
    // every item as an iterator over a list once the generator ends
    All,
    // `any` which the first true item ends
    Any,
    // `all` which the first false item ends
    Every,
    // `next` giving the first item or if the generator ends first its default if any
    Next(Val),
    // `in` or `not in` when true which the first item equal to the value ends
    Contains(Val, bool),
    // `close` where the close signal ends it and any item it yields fails it
    Close,
    // an `await` whose yield or return goes back to the op with `await_back` saying which
    Await,
    // a delegated generator's step whose end runs the waits in `conts` from the given index
    Then(u32),
    // items up to a limit for the op to retake at its slot or `retake`
    Retry(u32, u16),
    // every item kept in the generator then the job's leading native called on what follows
    Hold,
    // a list's `+=` appending each item as it comes like `extend` and giving the list
    Extend(Val),
    // yield held for an iterator step and true steps the job under not the `for`
    Prefetch(bool),
    // each item to the fold of a native which the gather keeps
    Fold,
    // each item into the job's items which a native's collect fold takes at its end
    FoldItems,
    // machine word ints into the job's total which a native's words fold takes past them
    FoldWords,
    // the least or most plain number kept as the job's best for an extreme fold
    FoldExtreme(bool),
}

/* how far a words fold went with word ints added exactly before any other value */
#[derive(Clone, Copy)]
pub(crate) enum Total { Word(i64), Any }

/* a builtin over a generator with gathered items and `extra` holding the arguments after it */
pub(crate) struct Gather { pub(crate) takes: Takes, pub(crate) items: Vec<Val>, pub(crate) extra: Vec<Val>, pub(crate) acc: Val, pub(crate) total: Total, pub(crate) co: Val, pub(crate) before: Val, pub(crate) cur: Option<IterFrame>, pub(crate) pull: Pull, again: Again, pub(crate) fold: Option<Box<super::steps::Folding>>, pub(crate) to: Dest }

/* source and filter flag and item of a builtin over user `map` or `filter` */
#[derive(Clone, Copy)]
pub(crate) struct Pull { pub(crate) src: Val, src2: Val, filter: bool, item: Val }

impl Gather {
    /* takes the gather out leaving an empty one that keeps where its value goes */
    pub(crate) fn taken(&mut self) -> Gather {
        let mut rest = Gather::new(Takes::Any, Vec::new());
        rest.to = self.to;
        core::mem::replace(self, rest)
    }

    pub(crate) fn new(takes: Takes, extra: Vec<Val>) -> Self { Self { takes, items: Vec::new(), extra, acc: Val::undef(), total: Total::Any, co: Val::undef(), before: Val::undef(), cur: None, pull: Pull { src: Val::undef(), src2: Val::undef(), filter: false, item: Val::undef() }, again: Again::default(), fold: None, to: Dest::Top } }
}

/* a callback's rerun frame clearing `names` where zero marks one keeping bases defaults or cells */
#[derive(Clone, Copy)]
pub(crate) struct Again { code: CodePtr, chunk: ChunkPtr, entry: usize, names: usize }

impl Default for Again {
    fn default() -> Self { Self { code: CodePtr::NONE, chunk: ChunkPtr::NONE, entry: 0, names: 0 } }
}

/* what a job does with what its callback returned */
pub(crate) enum Step {
    // calls the callback again on the value
    Again(Val),
    // calls the callback again on the two values
    Again2(Val, Val),
    // the `for` the frame returns to takes the value as its item
    Item(Val),
    // the `for` the frame returns to steps again as the iterator under it is spent
    Redo,
    // the value is the result of the call
    Give(Val),
    // the builtin's job under the frame steps its container on
    Drive,
}

impl Job {
    /* the job of a `filter` whose test runs on `item` taken from `it` */
    pub(crate) fn filter(it: Val, item: Val, before: Val) -> Job { Job::Filter { it, item, before, retake: false } }

    /* the code its callback runs again in which a collection keeps while the job lives */
    pub(crate) fn again_code(&self) -> Option<CodePtr> {
        match self { Job::Keyed(j) => Some(j.again.code), _ => None }
    }

    /* every value the job holds which a collection keeps */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        match self {
            Job::Map { before, .. } => f(*before),
            Job::Feed { target, item, sentinel, .. } => { f(*target); f(*item); f(*sentinel); }
            Job::Filter { it, item, before, .. } => { f(*it); f(*item); f(*before); }
            Job::Fold(d) => { f(d.item); f(d.next); }
            Job::NextGen(d, _) => f(*d),
            Job::Calls(c, before) => { c.for_each_val(f); f(*before); }
            Job::Stand => {}
            Job::Keyed(j) => {
                if let Some(n) = &j.fold { n.for_each_val(f); }
                j.cur.for_each_val(f);
            }
            Job::Gather(j) => {
                for &v in j.items.iter().chain(&j.extra) { f(v) }
                f(j.acc);
                f(j.co);
                f(j.before);
                f(j.pull.src);
                f(j.pull.src2);
                f(j.pull.item);
                if let Some(c) = &j.cur { c.for_each_val(f); }
                if let Some(n) = &j.fold { n.for_each_val(f); }
                if let Takes::Contains(k, _) | Takes::Extend(k) = j.takes { f(k) }
                if let Takes::Next(d) = j.takes { f(d) }
            }
        }
    }
}

impl<'a> VM<'a> {
    /* a job for builtin `takes` over generator or iterator `of` with `extra` its arguments after */
    pub(crate) fn gather_of(&self, takes: Takes, extra: Vec<Val>, of: Val) -> Gather {
        let mut j = Gather::new(takes, extra);
        // the source of the items stays held while user code between them collects
        (j.co, j.to) = (of, self.job_to);
        if matches!(takes, Takes::Fold | Takes::FoldItems | Takes::FoldWords | Takes::FoldExtreme(_)) { j.fold = self.next_fold.take(); }
        // a words fold starts at its native's total and an extreme fold at its best
        if let Some(f) = j.fold.as_deref() {
            match takes { Takes::FoldWords => j.total = Total::Word(f.words()), Takes::FoldExtreme(_) => j.acc = f.best(), _ => {} }
        }
        j
    }

    /* the answer of a truth fold whose items ended undecided */
    fn truth_end(&mut self, j: &Gather) -> Result<Val, VmErr> {
        match j.takes {
            Takes::Any => Ok(Val::bool(false)),
            Takes::Every => Ok(Val::bool(true)),
            _ => Err(cold_runtime("a gather that is no fold")),
        }
    }

    /* `f` as a builtin's `n`-argument frame or none and `once` runs a one-off unlowered */
    #[inline(always)]
    pub(crate) fn job_fn_as(&mut self, f: Val, n: usize, once: bool) -> Option<usize> {
        let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(f) else { return None };
        // past recursion or memory limits the call raises as the builtin's and opens no frame
        if self.too_deep() { return None; }
        let facts = &self.call_facts[fi];
        let p = &self.pools[facts.pool];
        if facts.plain && facts.arity == Some(n) && p.code.is_some() && !p.cold && p.consts.is_some() && !self.is_async.get(fi).copied().unwrap_or(false) { return Some(fi); }
        self.job_fn_cold(fi, n, once)
    }

    /* `job_fn_as` for a callback of one parameter the builtin calls again and again */
    pub(crate) fn job_fn(&mut self, f: Val) -> Option<usize> { self.job_fn_as(f, 1, false) }

    /* `job_fn_as` for a callback of two parameters */
    pub(crate) fn job_fn2(&mut self, f: Val) -> Option<usize> { self.job_fn_as(f, 2, false) }

    /* `job_fn` for a never-run body lowered as `lower_callees` does or one taking defaults or cells */
    #[inline(never)]
    fn job_fn_cold(&mut self, fi: usize, n: usize, once: bool) -> Option<usize> {
        let facts = self.call_facts[fi];
        // a fresh closure fills its cells as it opens
        let bare = facts.plain || facts.closure && facts.in_loop && facts.arity == Some(facts.required as usize);
        // a one-off keeps first code until `TIER` calls and a repeated callback lowers at once
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

    /* the next item of builtin iterator `it` if it steps without user code else none */
    #[inline(always)]
    fn plain_next(&mut self, it: Val) -> Option<Result<Option<Val>, VmErr>> {
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return None };
        // the iterator stays live and a step that runs no user code frees no object
        let mut frame = pinned(frame).try_borrow_mut().ok()?;
        Some(Ok(match &mut *frame {
            IterFrame::List { rc, idx } => {
                // nothing writes the list while one item is copied out as the loop reads it
                let v = peek(rc)?.get(*idx).copied();
                *idx = if v.is_some() { *idx + 1 } else { crate::value::frames::SPENT };
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

    /* opens `callee` on `arg` as a `job` frame whose caller carries on at `ret` */
    pub(crate) fn open_job(&mut self, fi: usize, callee: Val, arg: Val, ret: usize, dest: u16, job: Job) -> (CodePtr, ChunkPtr) {
        let opened = self.open_block(fi, callee, &[arg], ret as i64 | (dest as i64) << 24 | JOB << 40);
        let opened = opened.unwrap_or_else(|| self.open_job_frame(fi, callee, arg, ret, dest));
        self.jobs.push(job);
        opened
    }

    /* `open_job` for a two-parameter callback on `x` and `y` whose caller carries on at `ret` */
    pub(crate) fn open_job2(&mut self, fi: usize, callee: Val, x: Val, y: Val, ret: usize, job: Job) {
        // the result goes on the stack unless the handler opening the frame sets its place
        let opened = self.open_block(fi, callee, &[x, y], ret as i64 | (NO_DEST as i64) << 24 | JOB << 40);
        if opened.is_none() {
            let cost = 1 + self.heap.take_work() as u64;
            self.budget = self.budget.saturating_sub(cost);
            self.heap.set_budget(self.budget);
            let (pool, facts) = (self.fn_pool[fi], self.call_facts[fi]);
            let base = self.bind_plain(fi, callee, &[x, y]);
            if facts.closure || !facts.plain { self.fill_frame(callee, base, 2, facts.arity.unwrap_or(2)); }
            let p = &self.pools[pool];
            if !p.laid && p.calls >= lower::TIER { self.relower(fi); }
            let rc = self.pools[pool].code.as_ref().expect("a job's frame runs the code its pool holds");
            let (code, flat) = (CodePtr::of(rc), rc.flat());
            let cold = self.cold_bases(flat);
            self.push_act(Act {
                code, pool, cold, ret, dest: NO_DEST, fi, callee, call_ip: None,
                levels: self.levels.len(), instance: Val::undef(), generator: Val::undef(), job: true, dunder: false,
            });
            self.run_in(pool);
            if !self.laid(code.get(), pool) { self.grow_pool(&self.functions[fi].1, pool); }
        }
        self.jobs.push(job);
        (self.entered, self.yielded) = (true, true);
    }

    /* `open_job` for a function its pool's first window does not hold */
    #[inline(never)]
    fn open_job_frame(&mut self, fi: usize, callee: Val, arg: Val, ret: usize, dest: u16) -> (CodePtr, ChunkPtr) {
        // it pays a step like any call and spending the budget fails the next one
        let cost = 1 + self.heap.take_work() as u64;
        self.budget = self.budget.saturating_sub(cost);
        self.heap.set_budget(self.budget);
        let (pool, facts) = (self.fn_pool[fi], self.call_facts[fi]);
        let base = self.bind_plain(fi, callee, &[arg]);
        if facts.closure { self.fill_frame(callee, base, 1, facts.arity.unwrap_or(1)); }
        let p = &self.pools[pool];
        if !p.laid && p.calls >= lower::TIER { self.relower(fi); }
        let rc = self.pools[pool].code.as_ref().expect("a job's frame runs the code its pool holds");
        let (code, flat) = (CodePtr::of(rc), rc.flat());
        let cold = self.cold_bases(flat);
        self.push_act(Act {
            code, pool, cold, ret, dest, fi, callee, call_ip: None,
            levels: self.levels.len(), instance: Val::undef(), generator: Val::undef(), job: true, dunder: false,
        });
        self.run_in(pool);
        let fun = self.functions[fi];
        if !self.laid(code.get(), pool) { self.grow_pool(&fun.1, pool); }
        (code, ChunkPtr::of(&fun.1))
    }

    /* reruns the job's callback on `arg` in frame `fi` of `callee` over `levels` recursion levels */
    #[inline]
    pub(crate) fn rearm(&mut self, pool: usize, fi: usize, callee: Val, levels: usize, bases: bool, arg: Val) {
        debug_assert!(pool < self.pools.len() && fi < self.call_facts.len());
        // a callback that ran its first call in its first code runs the rest lowered
        if self.pools[pool].cold { self.warm_up(fi, pool); }
        let (base, facts) = (get_in(&self.pools, pool).window, get_in(&self.call_facts, fi));
        let (names, refill, arity) = (facts.names as usize, !facts.plain || facts.arity != Some(1), facts.arity.unwrap_or(1));
        open::<0>(&mut self.regs, base, None, &[arg], names);
        if refill { self.fill_frame(callee, base, 1, arity); }
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        if self.levels.len() > levels { self.levels.truncate(levels); }
        if bases && let Some(b) = self.frame_bases.last() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        // a callback runs again from its first row
        self.top_fresh(pool);
    }

    /* `rearm` for a two-parameter callback run again on the pair `x` and `y` */
    #[inline(never)]
    pub(crate) fn rearm2(&mut self, pool: usize, fi: usize, callee: Val, levels: usize, bases: bool, (x, y): (Val, Val)) {
        if self.pools[pool].cold { self.warm_up(fi, pool); }
        let (base, facts) = (self.pools[pool].window, self.call_facts[fi]);
        let (names, arity) = (facts.names as usize, facts.arity.unwrap_or(2));
        let r = &mut self.regs[base..base + names.max(2)];
        (r[0], r[1]) = (x, y);
        for v in r.iter_mut().skip(2) { *v = Val::undef(); }
        if !facts.plain || arity != 2 { self.fill_frame(callee, base, 2, arity); }
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        if self.levels.len() > levels { self.levels.truncate(levels); }
        if bases && let Some(b) = self.frame_bases.last() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        // a callback runs again from its first row
        self.top_fresh(pool);
    }

    /* top job's step on `v` else none unchanged for user code raises or uncharged work */
    #[inline(always)]
    fn job_fast(&mut self, v: Val) -> Option<Step> {
        // another call pays a step as any call does
        let pays = self.budget > 0 && !self.heap.has_work();
        let step = match self.jobs.last_mut()? {
            Job::Map { ctor: false, .. } => return Some(Step::Item(v)),
            // a native's fold keys its next item in this frame or keeps its flow
            &mut Job::Fold(d) if d.resume() == NATIVE && pays => Step::Again(self.fold_next(v)?),
            Job::Keyed(j) if pays => {
                let f = j.fold.as_mut()?;
                if !f.key_ready(v, &self.heap) { return None; }
                let next = j.cur.next_ready(&self.heap)?;
                f.take_key(v, next);
                Step::Again(next)
            }
            &mut Job::Filter { it, item, retake, .. } => {
                if retake { return None; }
                if bare_truth(&self.heap, v)? { return Some(Step::Item(item)); }
                if !pays { return None; }
                // a failure sticks so `job_take` meets it again
                match self.plain_next(it)?.ok()? {
                    Some(x) => { if let Some(Job::Filter { item, .. }) = self.jobs.last_mut() { *item = x; } Step::Again(x) }
                    None => Step::Redo,
                }
            }
            Job::Gather(_) | Job::Fold(_) | Job::Stand | Job::NextGen(..) | Job::Feed { .. } | Job::Map { .. } | Job::Calls(..) | Job::Keyed(_) => return None,
        };
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        Some(step)
    }

    /* the top job's next step on callback result `v` running user code and raising errors */
    #[inline(never)]
    pub(crate) fn job_take(&mut self, v: Val) -> Result<Step, VmErr> {
        match self.jobs.last() {
            Some(&Job::Feed { target, item, sentinel, kind, drive }) => {
                // a constructor's init method gives the instance it built
                let v = self.top().map_or(v, |f| if f.instance.is_undef() { v } else { f.instance });
                self.feed_take(target, item, sentinel, kind, v);
                Ok(if drive { Step::Drive } else { Step::Redo })
            }
            Some(&Job::Map { ctor, .. }) => Ok(Step::Item(if ctor { self.top().map_or(v, |f| f.instance) } else { v })),
            Some(&Job::Filter { it, item, retake, .. }) => {
                // an init gives its instance and a `for` test takes its next item by step
                let v = if retake { self.top().map_or(v, |t| if t.instance.is_undef() { v } else { t.instance }) } else { v };
                if self.truthy(v) { return Ok(Step::Item(item)); }
                self.charge_step()?;
                if retake { return Ok(Step::Redo); }
                match self.plain_next(it) {
                    Some(Ok(Some(x))) => {
                        if let Some(Job::Filter { item, .. }) = self.jobs.last_mut() { *item = x; }
                        Ok(Step::Again(x))
                    }
                    // a failure sticks so the `for` meets it again and raises it
                    _ => Ok(Step::Redo),
                }
            }
            Some(Job::Keyed(_)) => self.keyed_take(v),
            Some(Job::Gather(j)) if !j.pull.src.is_undef() => self.pull_take(v),
            Some(Job::Gather(_) | Job::Fold(_) | Job::Stand | Job::NextGen(..) | Job::Calls(..)) | None => Err(cold_runtime("a job frame without its job")),
        }
    }

    /* where the caller of a job's frame carries on after `step` at `ret` for `to` */
    pub(crate) fn job_done(&mut self, step: Step, code: &Code, ret: usize, to: Dest) -> usize {
        match step {
            // the `for` takes the item into the frame's named register or as its step would
            Step::Item(v) | Step::Give(v) if let Dest::Word(d) = to => self.deliver(v, ret, d),
            Step::Item(v) if to == Dest::Top => { self.hand_to_for(code, ret, v); ret + 1 }
            Step::Item(v) | Step::Give(v) => self.deliver_to(v, ret, to),
            Step::Redo | Step::Again(_) | Step::Again2(..) | Step::Drive => ret,
        }
    }

    /* runs the builtin `run` names giving its result or None with its first frame opened */
    pub(crate) fn calls_run(&mut self, run: Box<dyn Calls>, frames: bool) -> Result<Option<Val>, VmErr> {
        let before = self.pending.exc_val.unwrap_or(Val::undef());
        let frames = frames && !self.redo_replays();
        self.jobs.push(Job::Calls(run, before));
        self.calls_go(None, frames)
    }

    /* steps the top `Calls` job to its result or none with its next frame open */
    pub(crate) fn calls_go(&mut self, got: Option<Result<Val, VmErr>>, frames: bool) -> Result<Option<Val>, VmErr> {
        let call = self.calls_step(got);
        self.calls_from(call, frames)
    }

    /* the top `Calls` job stepped with a stand-in in its place and its value before */
    fn step_calls(&mut self, got: Option<Result<Val, VmErr>>) -> Option<(Val, Result<Call, VmErr>)> {
        if !matches!(self.jobs.last(), Some(Job::Calls(..))) { return None; }
        let at = self.jobs.len() - 1;
        let Job::Calls(mut run, before) = core::mem::replace(&mut self.jobs[at], Job::Stand) else { return None };
        let stepped = run.step(self, got);
        // a step leaves the jobs below its own as it found them
        self.jobs[at] = Job::Calls(run, before);
        Some((before, stepped))
    }

    /* what the top `Calls` job does after `got` with the job popped if it fails */
    pub(crate) fn calls_step(&mut self, got: Option<Result<Val, VmErr>>) -> Result<Call, VmErr> {
        let took = matches!(got, Some(Err(_)));
        let Some((before, call)) = self.step_calls(got) else { return Err(cold_runtime("a job frame without its job")) };
        let call = match call {
            Ok(c) => c,
            Err(e) => { self.jobs.pop(); return Err(e); }
        };
        // a taken error restores the exception pending at start unless a delegation's waits take theirs
        if took && !matches!(call, Call::Then(..)) {
            self.pending.exc_val = Some(before).filter(|b| !b.is_undef());
            self.error_byte_pos = None;
            self.call_stack.clear();
        }
        Ok(call)
    }

    /* `calls_go` from `call` which the top job asked for */
    pub(crate) fn calls_from(&mut self, call: Result<Call, VmErr>, frames: bool) -> Result<Option<Val>, VmErr> {
        let mut call = call?;
        loop {
            let called = match call {
                Call::Then(base, step) => { self.jobs.pop(); self.entered = false; self.then_out = Some((base, step)); return Ok(Some(Val::none())); }
                Call::Give(v) => { self.jobs.pop(); self.entered = false; return Ok(Some(v)); }
                Call::Frame(g, args) => self.job_call(g, call_args(&args), frames),
            };
            match called {
                None => {
                    // the opened frame is the job's so leaving it takes the job off
                    let at = self.head_at(self.top_pool);
                    self.regs[at] = Val::int(self.head(at).as_int() | JOB << 40);
                    (self.entered, self.yielded) = (true, true);
                    return Ok(None);
                }
                Some(r) => call = self.calls_step(Some(r))?,
            }
        }
    }

    /* unmarks the running `Calls` frame so leaving it keeps the job for its next step */
    pub(crate) fn calls_keep(&mut self) {
        let at = self.head_at(self.top_pool);
        self.regs[at] = Val::int(self.head(at).as_int() & !(JOB << 40));
    }

    /* calls g on args and None with its frame opened when frames allows */
    fn job_call(&mut self, g: Val, args: &[Val], frames: bool) -> Option<Result<Val, VmErr>> {
        let chunk = self.chunk;
        let func = match self.heap.try_get(g) { Some(&HeapObj::Bound { func, .. }) if func.is_heap() => func, _ => g };
        let fi = match self.heap.try_get(func) {
            Some(&HeapObj::Func { fi, .. }) if !self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false) => Some(fi),
            _ => None,
        };
        let opens = frames && self.budget != 0 && fi.is_some();
        if !opens { return Some(self.call_with(g, None, args, &[], chunk).and_then(|v| match v { Some(v) => Ok(v), None => self.pop() })); }
        // a call a rerun would replay runs afresh and one under `JOB_LOWERS` calls stays unlowered
        let (on, lazy) = (self.redo.on, self.redo.lazy);
        (self.redo.on, self.redo.lazy) = (false, false);
        (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
        self.entered = false;
        let pool = fi.map_or(0, |fi| self.fn_pool[fi]);
        let keep = (self.pools[pool].code.is_none() || self.pools[pool].cold) && self.pools[pool].calls < JOB_LOWERS;
        let ran = self.pools[pool].ran;
        if keep { self.pools[pool].ran = false; }
        let r = self.call_args(g, args, &[], chunk);
        (self.inline_call, self.pending_exec_safe) = (false, false);
        (self.redo.on, self.redo.lazy) = (on, lazy);
        if keep { self.pools[pool].ran = ran; }
        match r {
            Ok(None) if core::mem::take(&mut self.entered) => {
                self.yielded = false;
                None
            }
            Ok(Some(v)) => Some(Ok(v)),
            Ok(None) => Some(self.pop()),
            Err(e) => Some(Err(e)),
        }
    }

    /* ok reruns in place or err gives the `for`'s item or none means full path */
    #[inline(always)]
    pub(crate) fn job_again(&mut self, v: Val) -> Option<Result<(CodePtr, ChunkPtr, usize), Val>> {
        // another call pays a step and uncharged native work takes the full path
        let pays = self.budget != 0 && !self.heap.has_work();
        let (next, again) = match self.jobs.last_mut()? {
            Job::Map { ctor: false, .. } => return Some(Err(v)),
            &mut Job::Filter { it, item, retake, .. } => {
                if retake { return None; }
                if bare_truth(&self.heap, v)? { return Some(Err(item)); }
                let again = self.again_in_place().filter(|_| pays)?;
                // a spent or failed iterator takes the full path which ends the loop or raises
                let next = self.plain_next(it)?.ok()??;
                if let Some(Job::Filter { item, .. }) = self.jobs.last_mut() { *item = next; }
                (next, again)
            }
            // a keys fold takes the key and runs the frame again on the next item
            Job::Keyed(j) if j.again.names != 0 && pays => {
                let f = j.fold.as_mut()?;
                if !f.key_ready(v, &self.heap) { return None; }
                let next = j.cur.next_ready(&self.heap)?;
                f.take_key(v, next);
                (next, j.again)
            }
            // a native's fold over a gathered container takes each result and runs again in place
            &mut Job::Fold(d) if d.resume() == NATIVE && pays => {
                let n = self.jobs.len();
                if n < 2 || !matches!(self.jobs.get(n - 2), Some(Job::Gather(j)) if matches!(j.takes, Takes::Fold)) { return None; }
                return self.fold_again(v).map(Ok);
            }
            _ => return None,
        };
        Some(Ok(self.run_again(next, again)))
    }

    /* runs the job frame on top again in place on `next` paying a step */
    #[inline(always)]
    fn run_again(&mut self, next: Val, again: Again) -> (CodePtr, ChunkPtr, usize) {
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        let base = self.base;
        open::<0>(&mut self.regs, base, None, &[next], again.names);
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        (again.code, again.chunk, again.entry)
    }

    /* the key frame on top run again in place once its native fold took `v` */
    #[inline(never)]
    fn fold_again(&mut self, v: Val) -> Option<(CodePtr, ChunkPtr, usize)> {
        let g = self.jobs.len() - 2;
        let at = self.head_at(self.top_pool);
        let (callee, code) = (self.head(at + 2), CodePtr::from_word(self.head(at + 4)));
        let Job::Gather(j) = &self.jobs[g] else { return None };
        j.cur.as_ref()?;
        // how the frame runs again found anew for other code and kept when it cannot
        let again = if j.again.code == code { j.again } else {
            let fi = (self.head(at + 1).as_int() >> 24) as usize;
            self.again_of(fi, code, ChunkPtr::of(&self.functions[fi].1))
        };
        let Job::Gather(j) = &mut self.jobs[g] else { return None };
        j.again = again;
        if again.names == 0 { return None; }
        let f = j.fold.take()?;
        let next = self.fold_on(f, v, callee)?;
        Some(self.run_again(next, again))
    }

    /* the argument the key frame runs again on once its native fold took `v` */
    #[inline(never)]
    fn fold_next(&mut self, v: Val) -> Option<Val> {
        let callee = self.head(self.head_at(self.top_pool) + 2);
        let Some(Job::Gather(j)) = self.jobs.len().checked_sub(2).and_then(|g| self.jobs.get_mut(g)) else { return None };
        if !matches!(j.takes, Takes::Fold) || j.cur.is_none() { return None; }
        let f = j.fold.take()?;
        self.fold_on(f, v, callee)
    }

    /* lowers `fi` so its top job frame runs lowered in a window laid for it */
    #[cold]
    #[inline(never)]
    fn warm_up(&mut self, fi: usize, pool: usize) {
        let f = self.functions[fi];
        let code = self.lowered_code(&f.1, pool, false);
        let at = self.head_at(self.top_pool);
        self.regs[at + 4] = CodePtr::of(&code).word();
        if !self.laid(&code, pool) { self.grow_frame(&f.1, &code, pool); }
        self.refresh_again(CodePtr::of(&code));
    }

    /* the top job frame now runs `code` so its job's `Again` reruns that code instead */
    #[cold]
    #[inline(never)]
    pub(crate) fn refresh_again(&mut self, code: CodePtr) {
        let at = self.head_at(self.top_pool);
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        let again = self.again_of(fi, code, ChunkPtr::of(&self.functions[fi].1));
        let n = self.jobs.len();
        match self.jobs.last_mut() {
            Some(Job::Keyed(j)) => j.again = again,
            // the key of a fold the frame runs is the job's under its own
            Some(Job::Fold(_)) => if let Some(Job::Gather(j)) = self.jobs.get_mut(n - 2) { j.again = again; },
            _ => {}
        }
    }

    /* how the running job frame reruns in place when plain flat and one-parameter else none */
    #[inline(never)]
    fn again_in_place(&self) -> Option<Again> {
        let at = self.head_at(self.top_pool);
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        let code = CodePtr::from_word(self.head(at + 4));
        let again = self.again_of(fi, code, ChunkPtr::of(&self.functions[fi].1));
        (again.names != 0).then_some(again)
    }

    /* what a job frame of `fi` that `open_job` just opened runs again in place */
    fn again_of(&self, fi: usize, code: CodePtr, chunk: ChunkPtr) -> Again {
        let facts = &self.call_facts[fi];
        let c = code.get();
        // one in its first code reruns the full way which lowers it for the rest
        let names = if facts.plain && facts.arity == Some(1) && c.flat() && !self.pools[facts.pool].cold { facts.names as usize } else { 0 };
        Again { code, chunk, entry: c.lowered(0), names }
    }

    /* the job frame at `at` returns `v` like `leave_site` then reruns or resumes the caller */
    #[inline(never)]
    pub(crate) fn leave_job(&mut self, v: Val, at: usize, link: i64, floor: &Floor) -> Option<(CodePtr, ChunkPtr, usize)> {
        let bases = link >> 40 & BASES != 0;
        // a frame that left a handler or a cleanup takes the full path
        if bases && let Some(b) = self.frame_bases.last() && (self.exception_stack.len() > b.exc || self.unwind_stack.len() > b.unwind) { return None; }
        let step = self.job_fast(v)?;
        let (pool, below) = (self.top_pool, self.head(at + 1).as_int());
        let fi = (below >> 24) as usize;
        if let Step::Again(_) | Step::Again2(..) = step {
            match step {
                Step::Again(arg) => self.rearm(pool, fi, self.head(at + 2), super::act::mark_of(self.head(at + 3)), bases, arg),
                Step::Again2(x, y) => self.rearm2(pool, fi, self.head(at + 2), super::act::mark_of(self.head(at + 3)), bases, (x, y)),
                _ => {}
            }
            // a lowering in the rearm may have moved the window and its record with it
            let code = CodePtr::from_word(self.head(self.head_at(self.top_pool) + 4));
            // the pool holds the code while the VM lives and `functions` holds the frame's function
            return Some((code, ChunkPtr::of(&get_in(&self.functions, fi).1), code.get().lowered(0)));
        }
        if bases && let Some(b) = self.frame_bases.pop() && self.iter_stack.len() > b.iter { self.iter_stack.truncate(b.iter); }
        self.pop_act();
        self.close_window(pool);
        let below = (below & 0xFF_FFFF) as usize;
        let (code, chunk) = if self.acts > floor.depth {
            self.run_in(below);
            let h = self.head_at(below);
            let fi = (self.head(h + 1).as_int() >> 24) as usize;
            (CodePtr::from_word(self.head(h + 4)), ChunkPtr::of(&self.functions[fi].1))
        } else {
            self.run_in(floor.pool);
            (floor.code, floor.chunk)
        };
        Some((code, chunk, self.job_done(step, code.get(), (link & 0xFF_FFFF) as usize, super::act::word_of((link >> 24) as u16))))
    }

    /* the `for` at `i` steps the top `map` or `filter` by entering its callback frame */
    #[inline(never)]
    pub(crate) fn step_called(&mut self, i: usize, ins: crate::code::Ins) {
        self.entered = if ins.x == 0 { self.map_step(i, ins.b) } else { self.for_job(i, None) }.is_some();
    }

    /* opens a plain function's `map` on the next item returning straight to `reg` else `for_job` */
    #[inline(always)]
    fn map_step(&mut self, i: usize, reg: u16) -> Option<(CodePtr, ChunkPtr)> {
        let (f, it, filter) = match self.iter_stack.last() {
            Some(IterFrame::Map { f, its, .. }) if its.len() == 1 => (*f, its[0], false),
            Some(&IterFrame::Filter { f, it, .. }) if !f.is_undef() => (f, it, true),
            _ => return self.for_job(i, Some(reg)),
        };
        if self.budget == 0 || self.pending.exc_val.is_some() { return self.for_job(i, Some(reg)); }
        let Some(fi) = self.job_fn(f) else { return self.for_job(i, Some(reg)) };
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(it) else { return self.for_job(i, Some(reg)) };
        // nothing holds the iterator's state while it steps
        let Some(item) = step_ready(frame, &self.heap) else { return self.for_job(i, Some(reg)) };
        // a `map` returns straight to the register but a `filter` goes through its testing job
        let link = i as i64 | (reg as i64) << 24 | if filter { JOB << 40 } else { 0 };
        let job = if filter { Job::filter(it, item, Val::undef()) } else { Job::Map { before: Val::undef(), ctor: false } };
        match self.open_block(fi, f, &[item], link) {
            Some(opened) => { if filter { self.jobs.push(job); } Some(opened) }
            None => Some(self.open_job(fi, f, item, i, reg, job)),
        }
    }

    /* ends the job frame on a plain register or stack return as `regs_return` does */
    #[inline(never)]
    pub(crate) fn job_return(&mut self, ins: crate::code::Ins, floor: &Floor) -> Option<(usize, CodePtr, ChunkPtr)> {
        let at = self.head_at(self.top_pool);
        // code over rows returns its top row and None with no rows
        let v = if ins.op == crate::code::OpCode::ReturnR { self.regs[self.pools[self.top_pool].window + ins.b as usize] }
            // a frame's record holds code its pool keeps while the VM lives
            else { self.rows_value(CodePtr::from_word(self.head(at + 4)).get()) };
        self.leave_job(v, at, self.head(at).as_int(), floor).map(|(c, k, next)| (next, c, k))
    }

    /* opens a `map` or `filter` frame on the next item else none for the handler */
    #[inline(never)]
    pub(crate) fn for_job(&mut self, i: usize, reg: Option<u16>) -> Option<(CodePtr, ChunkPtr)> {
        let (f, it, filter) = match self.iter_stack.last() {
            Some(IterFrame::Map { f, its, .. }) if its.len() == 1 => (*f, its[0], false),
            Some(&IterFrame::Filter { f, it, .. }) if !f.is_undef() => (f, it, true),
            _ => return None,
        };
        if self.budget == 0 { return None; }
        let fi = self.job_fn(f)?;
        // a spent or failed iterator ends or raises in the handler
        let Ok(Some(item)) = self.plain_next(it)? else { return None };
        // its end of iteration restores the pending exception a plain frame keeps nowhere
        let before = self.pending.exc_val;
        if !filter && before.is_none() && let Some(r) = reg && let Some(opened) = self.open_block(fi, f, &[item], i as i64 | (r as i64) << 24) { return Some(opened); }
        let before = before.unwrap_or(Val::undef());
        let job = if filter { Job::filter(it, item, before) } else { Job::Map { before, ctor: false } };
        Some(self.open_job(fi, f, item, i, reg.unwrap_or(NO_DEST), job))
    }

    /* opens builtin callback `key` on `first` as a job whose return the opening handler sets */
    pub(crate) fn key_job(&mut self, fi: usize, key: Val, first: Val, job: Job) {
        let (code, chunk) = self.open_job(fi, key, first, 0, NO_DEST, job);
        let again = self.again_of(fi, code, chunk);
        if let Some(Job::Keyed(j)) = self.jobs.last_mut() { j.again = again; }
        self.entered = true;
        self.yielded = true;
    }

    /* runs generator `co` as a frame per `takes` or false for the builtin's own path */
    #[inline(never)]
    pub(crate) fn gather_job(&mut self, co: Val, takes: Takes, extra: Vec<Val>) -> Result<bool, VmErr> { self.gen_job(co, takes, extra, false) }

    /* `gather_job` and for a coroutine too when `coro` the step its call or await makes */
    pub(crate) fn gen_job(&mut self, co: Val, takes: Takes, extra: Vec<Val>, coro: bool) -> Result<bool, VmErr> {
        if self.budget == 0 { return Ok(false); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return Ok(false) };
        let BodyRef::Fn(fi) = c.body else { return Ok(false) };
        let (generator, is_async) = (self.functions[fi].1.is_generator, self.is_async_fn(fi));
        if !(generator && !is_async || coro && is_async) { return Ok(false); }
        let pool = self.call_facts[fi].pool;
        let job = match takes { Takes::Next(d) => Job::NextGen(d, self.job_to), _ => Job::Gather(Box::new(self.gather_of(takes, extra, co))) };
        self.jobs.push(job);
        let jobs = self.jobs.len();
        if self.pools[pool].code.is_none() { self.gen_cold_code(fi); }
        // recursion through builtins over generators ends as deep as nested resumes with jobs counting twice
        if 2 * self.jobs.len() + self.depth >= self.max_calls { self.jobs.truncate(jobs - 1); return Err(crate::vm::cold_depth()); }
        let Some((_, _, at)) = self.enter_gen(co, 0) else {
            // a refused gather hands its native's fold back for the plain path
            if let Some(Job::Gather(j)) = self.jobs.get_mut(jobs - 1) && let Some(f) = j.fold.take() { self.next_fold.set(Some(f)); }
            self.jobs.truncate(jobs - 1);
            return Ok(false);
        };
        self.mark_job();
        let thrown = self.resume_raise.is_some();
        (self.gen_entry, self.entered, self.thrown, self.yielded) = (at, !thrown, thrown, true);
        Ok(true)
    }

    /* whether a slice store at `rip` gathers its value as frames to run again */
    #[inline(never)]
    pub(crate) fn slice_job(&mut self, rip: usize) -> Result<bool, VmErr> {
        let Ok(at) = self.under(3) else { return Ok(false) };
        if self.heap.slice_of(self.slot(at + 1)).is_none() { return Ok(false); }
        match self.heap.try_get(self.slot(at + 2)) {
            Some(HeapObj::Coroutine(..)) => self.retry_gen(),
            Some(HeapObj::Iter(..)) => self.list_job(rip),
            _ => Ok(false),
        }
    }

    /* what the register op at `rip` takes of `v` or None while frames gather it */
    #[inline(never)]
    pub(crate) fn retake_job(&mut self, v: Val, rip: usize) -> Result<Option<Val>, VmErr> {
        self.delivering(Dest::Retake, |vm| match vm.heap.try_get(v) {
            Some(HeapObj::Coroutine(_)) => Ok(if vm.retry_of(v, 0)? { None } else { Some(v) }),
            Some(HeapObj::Iter(..)) => vm.lazy_items(v, rip),
            _ => Ok(Some(v)),
        })
    }

    /* whether the stack's top is a generator `retry_gen` looks at */
    #[inline(always)]
    pub(crate) fn top_may_gen(&self) -> bool {
        match self.under(1) { Ok(at) => { let v = self.slot(at); v.is_heap() && matches!(self.heap.get(v), HeapObj::Coroutine(_)) } Err(_) => false }
    }

    /* whether the top generator runs as frames listing items the next spread or unpack retakes */
    #[cold]
    #[inline(never)]
    pub(crate) fn retry_gen(&mut self) -> Result<bool, VmErr> {
        let Ok(at) = self.under(1) else { return Ok(false) };
        self.retry_of(self.slot(at), 0)
    }

    /* whether generator `v` runs as frames giving its op at most `limit` items */
    #[cold]
    #[inline(never)]
    pub(crate) fn retry_of(&mut self, v: Val, limit: u16) -> Result<bool, VmErr> {
        // a stack op meets it on the stack top and a register op in `retake`
        let at = if self.job_to == Dest::Retake { RETAKE } else { self.sp().saturating_sub(1) as u32 };
        self.gather_job(v, Takes::Retry(at, limit), Vec::new())
    }

    /* puts the list of the `items` a retry gathered at `slot` for its op */
    pub(crate) fn retried(&mut self, slot: u32, items: Vec<Val>) -> Result<(), VmErr> {
        let l = self.alloc_list(items)?;
        if slot == RETAKE { self.retake = l; return Ok(()); }
        if (slot as usize) >= self.sp() { return Err(cold_runtime("a spread lost its value")); }
        self.set_slot(slot as usize, l);
        Ok(())
    }

    /* what the next step of iterator `it` takes first that a builtin's step does not */
    pub(crate) fn peek(&self, it: Val, depth: u8) -> Peek {
        match self.heap.try_get(it) {
            Some(HeapObj::Coroutine(c)) if !c.pending.is_undef() => Peek::Item,
            Some(HeapObj::Coroutine(c)) if c.ip == crate::vm::coro::FINISHED => Peek::End,
            Some(HeapObj::Coroutine(c)) if !c.running && !c.pinned() && c.syncs.is_empty() && matches!(c.body, BodyRef::Fn(fi) if self.functions[fi].1.is_generator && !self.is_async.get(fi).copied().unwrap_or(false)) => Peek::Gen(it),
            Some(HeapObj::Iter(f, _)) if depth < 8 => match f.try_borrow() { Ok(f) => self.peek_frame(&f, depth + 1), Err(_) => Peek::Other },
            _ => Peek::Other,
        }
    }

    /* `peek` of an iterator's frame where zip or map stop at the first that ends */
    pub(crate) fn peek_frame(&self, f: &IterFrame, depth: u8) -> Peek {
        match f {
            IterFrame::Zip { its, .. } | IterFrame::Map { its, .. } => {
                for &v in its.iter() { match self.peek(v, depth) { Peek::Item => {} p => return p } }
                Peek::Item
            }
            &IterFrame::Enumerate { it, .. } | &IterFrame::Filter { it, .. } | &IterFrame::UserDefined(it) | &IterFrame::Coroutine(it) => self.peek(it, depth),
            IterFrame::Seq { .. } | IterFrame::List { .. } | IterFrame::Octets { .. } => if f.ready(&self.heap).is_some() { Peek::Item } else { Peek::End },
            f => if f.ready(&self.heap).is_some() { Peek::Item } else { Peek::Other },
        }
    }

    /* whether the top `for` steps a builtin iterator not yet found free of user code */
    #[inline(always)]
    pub(crate) fn lazy_over(&self) -> bool {
        match self.iter_stack.last() {
            Some(&IterFrame::UserDefined(it) | &IterFrame::Filter { it, .. }) => it.0 != self.lazy_seen.0,
            Some(IterFrame::Map { its, .. }) => its[0].0 != self.lazy_seen.0,
            _ => false,
        }
    }

    /* whether stepping `v` may run user code or with `user` false reach a generator */
    pub(crate) fn holds_user(&self, v: Val, depth: u8, user: bool) -> bool {
        match self.heap.try_get(v) {
            Some(HeapObj::Iter(f, _)) if depth < 8 => f.try_borrow().map_or(true, |f| self.frame_holds_user(&f, depth + 1, user)),
            Some(HeapObj::Coroutine(_)) => true,
            _ => user,
        }
    }

    fn frame_holds_user(&self, f: &IterFrame, depth: u8, user: bool) -> bool {
        // a native stepping its argument runs user code where its items hold some
        let call = |f: Val| user && (self.heap.is_user_call(f) || self.heap.is_class(f) || self.is_next(f));
        match f {
            IterFrame::Zip { its, .. } => its.iter().any(|&v| self.holds_user(v, depth, user)),
            IterFrame::Map { f, its, .. } => call(*f)
                || its.iter().any(|&v| self.holds_user(v, depth, user))
                || user && self.steps_whole(*f) && its.iter().any(|&v| self.leaf_ahead(v, depth)),
            &IterFrame::Filter { f, it, .. } => call(f) || self.holds_user(it, depth, user) || user && self.steps_whole(f) && self.leaf_ahead(it, depth),
            &IterFrame::Enumerate { it, .. } | &IterFrame::UserDefined(it) => self.holds_user(it, depth, user),
            IterFrame::Seq { .. } | IterFrame::List { .. } | IterFrame::Range { .. } | IterFrame::BigRange { .. } | IterFrame::ListRev { .. } | IterFrame::Watched { .. } | IterFrame::DictWalk { .. }
            | IterFrame::Octets { .. } | IterFrame::Chars { .. } => false,
            IterFrame::Coroutine(_) => true,
            _ => user,
        }
    }

    /* steps the `for` at `i` as frames when its builtin iterator runs user code */
    #[inline(never)]
    pub(crate) fn lazy_prefetch(&mut self, i: usize, code: &Code) -> Result<bool, VmErr> {
        if self.budget == 0 { return Ok(false); }
        if self.heap.needs_gc() { self.collect_point()?; }
        let (fun, its, filter) = match self.iter_stack.last() {
            // a builtin iterator steps through what it steps
            Some(&IterFrame::UserDefined(it)) => {
                if self.pull_ready(it, false, i, 0)? { return Ok(true); }
                self.pass_by();
                return Ok(false);
            }
            Some(IterFrame::Map { f, its, .. }) => (*f, its.clone(), false),
            Some(&IterFrame::Filter { f, it, .. }) if !f.is_undef() => (f, Rc::from([it]), true),
            // a filter of truth runs its iterator's generator to an item as a frame first
            Some(&IterFrame::Filter { it, .. }) => {
                if self.pull_ready(it, false, i, 0)? { return Ok(true); }
                self.pass_by();
                return Ok(false);
            }
            _ => { self.pass_by(); return Ok(false) }
        };
        for &c in its.iter() { if self.pull_ready(c, false, i, 0)? { return Ok(true); } }
        if self.call_job(fun, &its, if filter { FeedKind::Filter } else { FeedKind::Map }, None, i)? || self.loop_call(fun, &its, filter, i, code)? { return Ok(true); }
        self.pass_by();
        Ok(false)
    }

    /* `lazy_prefetch` calling a class or varargs lambda or closure a `for` maps or filters plainly */
    #[inline(never)]
    fn loop_call(&mut self, fun: Val, its: &[Val], filter: bool, i: usize, code: &Code) -> Result<bool, VmErr> {
        let class = self.heap.is_class(fun);
        if !(class || matches!(self.heap.try_get(fun), Some(HeapObj::Func { .. }))) || its.len() > 8 { return Ok(false); }
        let chunk = self.chunk;
        loop {
            let mut args = [Val::undef(); 8];
            for (k, &c) in its.iter().enumerate() {
                match self.next_of(c)? {
                    Some(x) => args[k] = x,
                    None => { self.give_back(&its[..k], &args); self.pend.push((c, Val::undef())); return Ok(false); }
                }
            }
            let n = its.len();
            let before = self.pending.exc_val.unwrap_or(Val::undef());
            (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
            let called = self.call_args(fun, &args[..n], &[], chunk);
            (self.inline_call, self.pending_exec_safe) = (false, false);
            let called = called?;
            if self.entered {
                // its frame returns to the `for` with the job making its item
                let at = self.head_at(self.top_pool);
                let flags = self.head(at).as_int() >> 40 | JOB;
                self.regs[at] = Val::int(i as i64 | (NO_DEST as i64) << 24 | flags << 40);
                self.jobs.push(if filter { Job::Filter { it: its[0], item: args[0], before, retake: true } } else { Job::Map { before, ctor: class } });
                (self.entered, self.yielded) = (true, true);
                return Ok(true);
            }
            let v = self.got(called)?;
            if !filter { self.hand_to_for(code, i, v); return Ok(true); }
            if self.truthy(v) { self.hand_to_for(code, i, args[0]); return Ok(true); }
            self.charge_step()?;
        }
    }

    /* readies a step of `it` free of user code running its generators as jobs */
    #[inline(never)]
    pub(crate) fn pull_ready(&mut self, it: Val, drive: bool, ret: usize, depth: u8) -> Result<bool, VmErr> {
        if depth > 8 || self.budget == 0 || self.pend.iter().any(|p| p.0.0 == it.0) { return Ok(false); }
        let frame = match self.heap.try_get(it) {
            Some(HeapObj::Coroutine(_)) => return match self.peek(it, 0) { Peek::Gen(g) => self.gather_job(g, Takes::Prefetch(drive), Vec::new()), _ => Ok(false) },
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
            IterFrame::Filter { f, it: src, .. } => loop {
                if self.pull_ready(src, drive, ret, depth + 1)? { return Ok(true); }
                if !f.is_undef() && self.call_job(f, &[src], FeedKind::Filter, Some((it, Val::undef(), drive)), ret)? { return Ok(true); }
                // a test run in place keeps a passing pulled item for the filter's own step
                let ready = matches!(self.heap.try_get(src), Some(HeapObj::Coroutine(_))) && matches!(self.peek(src, 0), Peek::Item | Peek::End);
                if !ready && !self.pend.iter().any(|p| p.0.0 == src.0) { return Ok(false); }
                let Some(v) = self.next_of(src)? else { self.pend.push((it, Val::undef())); return Ok(false) };
                self.charge_step()?;
                match self.passes(f, v)? {
                    Some(true) => { self.pend.push((it, v)); return Ok(false); }
                    Some(false) => {}
                    None => { self.pend.push((it, Val::undef())); return Ok(false); }
                }
            },
            IterFrame::Call { f, sentinel } => self.call_job(f, &[], FeedKind::Call, Some((it, sentinel, drive)), ret),
            _ => Ok(false),
        }
    }

    /* calls `fun` on the next items of `its` as a job returning to `ret` */
    fn call_job(&mut self, fun: Val, its: &[Val], kind: FeedKind, feed: Option<(Val, Val, bool)>, ret: usize) -> Result<bool, VmErr> {
        let (fun, recv) = match self.heap.try_get(fun) {
            // a bound method runs its function on its receiver first
            Some(&HeapObj::Bound { recv, func }) if func.is_heap() => (func, Some(recv)),
            _ => (fun, None),
        };
        // builtins stepping generators in Rust go through a loop function except a bare generator's `next`
        let next_gen = || its.len() == 1 && self.is_next(fun) && self.next_item(its[0]).is_some_and(|x| matches!(self.heap.try_get(x), Some(HeapObj::Coroutine(_))));
        let fun = if recv.is_none() && matches!(kind, FeedKind::Map | FeedKind::Filter) && self.steps_whole(fun) && self.next_leaf(its) && !next_gen() { self.through_loop(fun, its.len())?.unwrap_or(fun) } else { fun };
        if feed.is_some() && recv.is_none() && its.len() == 1 && self.is_next(fun) { return self.next_ready(its[0], feed.is_some_and(|f| f.2)); }
        let lead = recv.is_some() as usize;
        let n = its.len() + lead;
        if n > 8 { return Ok(false); }
        // a class or varargs lambda no plain frame opens runs as a plain call
        let fi = match self.heap.try_get(fun) { Some(HeapObj::Func { .. }) => self.job_fn_as(fun, n, false), _ => None };
        if fi.is_none() && (feed.is_none() || !(matches!(self.heap.try_get(fun), Some(HeapObj::Func { .. })) || self.heap.is_class(fun))) { return Ok(false); }
        let mut args = [Val::undef(); 8];
        if let Some(r) = recv { args[0] = r; }
        for (k, &c) in its.iter().enumerate() {
            match self.next_of(c)? {
                Some(x) => args[lead + k] = x,
                // one spent ends the step which meets it spent again and the items taken before
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
                // a no-argument closure no block opens runs in its plain call's frame rerun in place
                None if n == 0 => return self.call_as_job(fun, &[], ret, job),
                None => { self.give_back(its, &args[lead..]); return Ok(false); }
            },
        }
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* whether an item left in `it` or an iterator under it is a `user_leaf` leaf */
    pub(crate) fn leaf_ahead(&self, it: Val, depth: u8) -> bool { self.ahead(it, depth, false) }

    /* `leaf_ahead` where with `calls` a function of user code counts as a leaf too */
    fn ahead(&self, it: Val, depth: u8, calls: bool) -> bool {
        if depth > 8 { return false; }
        let leaf = |v: &Val| self.leaf_at(*v, depth + 1) || calls && (self.heap.is_user_call(*v) || self.heap.is_class(*v));
        // the item a job's call made for the next step is one it has left
        if self.pend.iter().any(|p| p.0.0 == it.0 && leaf(&p.1)) { return true; }
        let Some(HeapObj::Iter(f, _)) = self.heap.try_get(it) else { return false };
        match f.try_borrow().as_deref() {
            Ok(IterFrame::Seq { items, idx }) => items.get(*idx..).is_some_and(|r| r.iter().any(leaf)),
            Ok(IterFrame::List { rc, idx }) => rc.try_borrow().is_ok_and(|l| l.get(*idx..).is_some_and(|r| r.iter().any(leaf))),
            // a native map may build an iterator that calls a function its sources hold
            Ok(IterFrame::Map { its, .. }) => its.iter().any(|&c| self.ahead(c, depth + 1, true)),
            Ok(IterFrame::Zip { its, .. }) => its.iter().any(|&c| self.ahead(c, depth + 1, calls)),
            Ok(&IterFrame::Enumerate { it, .. } | &IterFrame::Filter { it, .. } | &IterFrame::UserDefined(it)) => self.ahead(it, depth + 1, calls),
            _ => false,
        }
    }

    /* whether a builtin stepping `v` in rust would run a generator or other user code */
    pub(crate) fn user_leaf(&self, v: Val) -> bool { self.leaf_at(v, 0) }

    /* `user_leaf` at `depth` where an iterator whose steps run user code is a leaf too */
    fn leaf_at(&self, v: Val, depth: u8) -> bool {
        match self.heap.try_get(v) {
            Some(HeapObj::Coroutine(_)) => matches!(self.peek(v, 0), Peek::Gen(_)),
            Some(HeapObj::Iter(..)) => self.holds_user(v, depth, true),
            _ => false,
        }
    }

    /* the generator `v` is or wraps which a builtin stepping `v` in Rust runs first */
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

    /* whether f called on a generator steps it in Rust as a builtin does */
    pub(crate) fn steps_whole(&self, f: Val) -> bool {
        match self.heap.try_get(f) {
            Some(HeapObj::Object { meta, .. }) if meta.is_type_meta() => { use crate::lang::KindId as K; matches!(self.heap.type_kind(f), Some(K::List | K::Tuple | K::Set | K::Dict | K::Bytes | K::ByteArray)) }
            Some(&HeapObj::Native(id)) if id.host_at().is_some() => true,
            Some(HeapObj::Native(_) | HeapObj::Bound { .. }) => self.native_parts(f).is_some_and(|(_, n, _)| n.steps()),
            _ => false,
        }
    }

    /* whether `f` is the native that steps an iterator to its next item */
    pub(crate) fn is_next(&self, f: Val) -> bool { self.heap.native_of(f).and_then(|id| self.natives.native(id)).is_some_and(|n| n.owner.is_none() && n.intrinsic == crate::lang::Intrinsic::Next) }

    /* whether a next item of `its` or one left under them is a leaf */
    fn next_leaf(&self, its: &[Val]) -> bool {
        its.iter().any(|&c| self.next_item(c).is_some_and(|x| self.user_leaf(x)) || self.leaf_ahead(c, 0))
    }

    /* the item iterator `c` gives next where it is read without stepping it */
    fn next_item(&self, c: Val) -> Option<Val> {
        match self.heap.try_get(c) {
            Some(HeapObj::Iter(f, _)) => f.try_borrow().ok().and_then(|f| f.ready(&self.heap)),
            Some(HeapObj::Coroutine(g)) => Some(g.pending).filter(|p| !p.is_undef()),
            _ => None,
        }
    }

    /* `f` bound to an `n`-argument loop function so a callback stepping a generator opens frames */
    #[cold]
    #[inline(never)]
    pub(crate) fn through_loop(&mut self, f: Val, n: usize) -> Result<Option<Val>, VmErr> {
        if n == 0 || n > 3 { return Ok(None); }
        if self.calls_through.is_none() {
            // a runtime that brings no spelling leaves the builtin calling `f` itself
            let Some(src) = self.rt.front.as_ref().and_then(|f| f.syntax.as_ref()).and_then(calls_source) else { return Ok(None) };
            let path = self.chunk.path.clone();
            let chunk = self.parse_compiled(&src, "<builtin>", &path)?;
            let start = self.functions.len();
            self.adopt(&src, Some("\u{0}calls".into()), chunk);
            self.calls_through = core::num::NonZeroUsize::new(start + 1);
        }
        let Some(first) = self.calls_through.map(|past| past.get() - 1) else { return Ok(None) };
        Ok(Some(self.heap.alloc(HeapObj::Func { fi: first + n - 1, defaults: alloc::vec![f].into(), cells: Box::new([]) })?))
    }

    /* puts all `items` generator `g` gave back in it for what steps it next */
    pub(crate) fn hold_items(&mut self, g: Val, mut items: Vec<Val>) {
        items.reverse();
        let Some(first) = items.pop() else { return };
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) { c.pending = first; }
        if !items.is_empty() { self.held.push((g, items)); }
    }

    /* holds the first generator under `call`'s arguments as a job and false for none */
    pub(crate) fn hold_start(&mut self, call: Vec<Val>) -> Result<bool, VmErr> {
        let Some(g) = call.get(1..).and_then(|r| r.iter().find_map(|&v| self.gen_leaf(v, 0))) else { return Ok(false) };
        self.with_roots(call.iter().copied(), |vm| vm.gather_job(g, Takes::Hold, call.clone()))
    }

    /* holds the next generator `call` needs or calls it once none is left */
    #[inline(never)]
    pub(crate) fn hold_then(&mut self, call: Vec<Val>) -> Result<Option<Val>, VmErr> {
        if self.hold_start(call.clone())? { return Ok(None); }
        self.with_roots(call.iter().copied(), |vm| vm.extern_call(&call))
    }

    /* readies the next item of `it` for the `next` a builtin iterator's step calls */
    fn next_ready(&mut self, it: Val, drive: bool) -> Result<bool, VmErr> {
        let Some(x) = self.next_of(it)? else { self.pend.push((it, Val::undef())); return Ok(false) };
        self.give_back(&[it], &[x]);
        if !matches!(self.peek(x, 0), Peek::Gen(_)) { return Ok(false); }
        self.gather_job(x, Takes::Prefetch(drive), Vec::new())
    }

    /* what a `Feed` call's result `v` makes of the next step of `target` per `kind` */
    pub(crate) fn feed_take(&mut self, target: Val, item: Val, sentinel: Val, kind: FeedKind, v: Val) {
        match kind {
            FeedKind::Map => self.pend.push((target, v)),
            FeedKind::Filter => if self.truthy(v) { self.pend.push((target, item)); },
            // a callable iterator stays spent once it meets the sentinel
            FeedKind::Call if eq_member(v, sentinel, &self.heap) => if let Some(HeapObj::Iter(f, _)) = self.heap.try_get(target) { *f.borrow_mut() = IterFrame::Seq { items: Vec::new().into(), idx: 0 }; },
            FeedKind::Call => self.pend.push((target, v)),
        }
    }

    /* calls `callee` plainly as the job's frame or keeps its result and gives false */
    #[inline(never)]
    fn call_as_job(&mut self, callee: Val, args: &[Val], ret: usize, job: Job) -> Result<bool, VmErr> {
        let chunk = self.chunk;
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (true, self.frame_safe, false);
        let called = self.call_args(callee, args, &[], chunk);
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (false, false, false);
        let called = called?;
        if !self.entered {
            let v = self.got(called)?;
            let Job::Feed { target, item, sentinel, kind, .. } = job else { return Err(cold_runtime("a job's call opened no frame")) };
            self.feed_take(target, item, sentinel, kind, v);
            return Ok(false);
        }
        let at = self.head_at(self.top_pool);
        let flags = self.head(at).as_int() >> 40 | JOB;
        self.regs[at] = Val::int(ret as i64 | (NO_DEST as i64) << 24 | flags << 40);
        self.jobs.push(job);
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* marks the top `for` iterator as free of user code for later steps to skip */
    fn pass_by(&mut self) {
        let Some(f) = self.iter_stack.last() else { return };
        if self.frame_holds_user(f, 0, true) { return; }
        self.lazy_seen = match f { &IterFrame::UserDefined(it) | &IterFrame::Filter { it, .. } => it, IterFrame::Map { its, .. } => its[0], _ => Val::undef() };
    }

    /* keeps `items` taken from `its` for their next steps to meet again */
    pub(crate) fn give_back(&mut self, its: &[Val], items: &[Val]) {
        for (&it, &v) in its.iter().zip(items) { self.pend.push((it, v)); }
    }

    /* the item kept for `it`'s next step and `Some(None)` when that step ends it */
    #[inline]
    pub(crate) fn pended(&mut self, it: Val) -> Option<Option<Val>> {
        if self.pend.is_empty() { return None; }
        let k = self.pend.iter().position(|p| p.0.0 == it.0)?;
        let v = self.pend.swap_remove(k).1;
        Some((!v.is_undef()).then_some(v))
    }

    /* ends a step whose `Feed` call ended telling whether the job under it drives on */
    #[inline(never)]
    pub(crate) fn feed_stopped(&mut self) -> Option<bool> {
        let Some(&Job::Feed { target, drive, .. }) = self.jobs.last() else { return None };
        self.pend.push((target, Val::undef()));
        (self.error_byte_pos, _) = (None, self.swallow_stop(None));
        self.call_stack.clear();
        Some(drive)
    }

    /* runs builtin `takes` over iterator `it` whose next step first takes a plain generator */
    pub(crate) fn lazy_pull(&mut self, it: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        if self.budget == 0 || !matches!(self.peek(it, 0), Peek::Gen(_)) { return Ok(false); }
        let mut j = self.gather_of(takes, extra.to_vec(), it);
        j.before = self.pending.exc_val.unwrap_or(Val::undef());
        self.jobs.push(Job::Gather(Box::new(j)));
        match self.gather_drive()? {
            // a builtin done at once hands its result where its job would
            Drive::Done(r, to) => self.put(r, to),
            // the frame returns where the builtin's call carries on which its opening handler sets
            Drive::Opened => (self.entered, self.yielded) = (true, true),
            Drive::Fold(fi, func, fold, args, n) => {
                self.open_fold(fi, func, fold, args, n);
                (self.entered, self.yielded) = (true, true);
            }
        }
        Ok(true)
    }

    /* gives generator function `fi` cold-marked unlowered code until its entries or a loop heat it */
    #[inline(never)]
    pub(crate) fn gen_cold_code(&mut self, fi: usize) {
        let pool = self.call_facts[fi].pool;
        let chunk = &self.functions[fi].1;
        // its first code runs on until the entries `enter_gen` counts heat it
        if self.pools[pool].cold { return; }
        if self.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(chunk, &mut self.heap) { self.pools[pool].consts = Some(consts); }
        if self.pools[pool].consts.is_none() { return; }
        let (shape, f) = (self.pools[pool].shape, self.functions[fi]);
        let code = self.first_identity(&lower::fuse_method_calls(&f.1, shape), &f.1, pool, self.fn_scope[fi].kinds.clone(), self.fn_module_id(fi));
        let p = &mut self.pools[pool];
        (p.code, p.cold, p.ran, p.heat) = (Some(code), true, true, p.heat + 1);
    }

    /* steps the job's container past a fold to the next one or the builtin's result */
    pub(crate) fn gather_drive(&mut self) -> Result<Drive, VmErr> { self.drive_to(false) }

    /* `gather_drive` where a fold's end that `waits` steps once the key frame on top left */
    fn drive_to(&mut self, waits: bool) -> Result<Drive, VmErr> {
        // the jobs a step opens deliver where the gather does
        let to = self.gather_to();
        if to == self.job_to { return self.drive_gather(waits); }
        self.delivering(to, |vm| vm.drive_gather(waits))
    }

    /* `drive_to` with the gather's destination set */
    fn drive_gather(&mut self, waits: bool) -> Result<Drive, VmErr> {
        // an item a pull took before the job stepped its iterator itself comes first
        if let Some(Job::Gather(j)) = self.jobs.last_mut() && j.pull.src.is_undef() && !j.pull.item.is_undef() {
            let v = core::mem::replace(&mut j.pull.item, Val::undef());
            // an item whose flow the fold kept takes no second step
            let kept = j.fold.as_ref().is_some_and(|f| f.kept(v));
            if (kept || !self.gather_fast(v)) && let Some(d) = self.gather_item(v)? { return Ok(d); }
        }
        loop {
            if let Some((v, to)) = self.fold_done() { return Ok(Drive::Done(v, to)); }
            let next = match self.jobs.last_mut() {
                // a container steps here and items it takes without user code are taken at once
                Some(Job::Gather(j)) => match j.cur.take() {
                    Some(mut c) => {
                        // out of the job while it steps which runs no user code and collects nothing
                        let next = loop {
                            let next = c.next_item(&mut self.heap);
                            if let Ok(Some(v)) = next && self.budget != 0 && !self.heap.has_work() && take_plain(j, v, &self.heap) { self.budget -= 1; continue; }
                            self.heap.set_budget(self.budget);
                            break next;
                        };
                        j.cur = Some(c);
                        next
                    }
                    // a builtin iterator steps as itself once a generator it takes runs as a frame
                    None => {
                        let co = j.co;
                        if self.pull_ready(co, true, 0, 0)? { return Ok(Drive::Opened); }
                        match self.plain_next(co) {
                            Some(next) => next,
                            None if matches!(self.heap.try_get(co), Some(HeapObj::Iter(..))) => {
                                self.step_soft = true;
                                let r = self.iter_step(co);
                                self.step_soft = false;
                                // a step that left a generator owing its item runs it as a frame first
                                if matches!(r, Ok(Some(v)) if v.is_undef()) { continue; }
                                r
                            }
                            None => self.next_of(co),
                        }
                    }
                },
                _ => return Err(cold_runtime("a builtin's job without its container")),
            };
            let next = match next { Ok(n) => n, Err(e) => { self.jobs.pop(); return Err(e); } };
            let Some(v) = next else {
                // the end may drain on into frames which open over the caller
                if waits && matches!(self.jobs.last(), Some(Job::Gather(j)) if j.fold.is_some()) { return Ok(Drive::Done(Val::undef(), Dest::Top)); }
                let mut j = self.take_gather();
                self.jobs.pop();
                let r = self.gather_end(&mut j, Val::none())?;
                return Ok(if self.entered || self.floor_req { Drive::Opened } else { Drive::Done(r, j.to) });
            };
            if self.gather_fast(v) { continue; }
            if let Some(d) = self.gather_item(v)? { return Ok(d); }
        }
    }

    /* takes the job's item `v` as a fold to run or the builtin's result */
    fn gather_item(&mut self, v: Val) -> Result<Option<Drive>, VmErr> {
        if let Err(e) = self.charge_step() { self.jobs.pop(); return Err(e); }
        if let Some((fi, func, fold, args, n)) = self.fold_of(v, NATIVE) { return Ok(Some(Drive::Fold(fi, func, fold, args, n))); }
        match self.gather_take(v) {
            Ok(None) => Ok(None),
            Ok(Some(r)) => { let to = self.gather_to(); self.jobs.pop(); Ok(Some(Drive::Done(r, to))) }
            Err(e) => { self.jobs.pop(); Err(e) }
        }
    }

    /* the loop's next index after `drive` past its result or at the opened frame's entry */
    #[inline(never)]
    pub(crate) fn after_drive(&mut self, drive: Drive, ret: usize, dest: u16) -> usize {
        match drive {
            Drive::Done(r, to) => return self.deliver_to(r, ret, super::act::job_dest(to, dest)),
            Drive::Fold(fi, func, d, args, n) => { self.open_fold(fi, func, d, args, n); self.set_ret(ret, dest); }
            // text the step called for runs as the next floor before carrying on at `ret`
            Drive::Opened if self.floor_req => { self.entered = false; return ret; }
            Drive::Opened => {
                (self.entered, self.yielded) = (false, false);
                if self.top().is_some_and(|f| !f.generator.is_undef()) { self.gather_ret(ret); return self.gen_entry; }
                self.set_ret(ret, dest);
            }
        }
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        // the pool holds the frame's code for as long as the VM lives
        let code = CodePtr::from_word(self.head(at + 4)).get();
        self.lay(&self.functions[fi].1, code, pool);
        self.top_fresh(pool);
        code.lowered(0)
    }

    /* where the value of the gather on top goes */
    pub(crate) fn gather_to(&self) -> Dest {
        match self.jobs.last() { Some(Job::Gather(j)) => j.to, Some(&Job::NextGen(_, to)) => to, Some(Job::Keyed(j)) => j.to, _ => Dest::Top }
    }

    /* what the top job gathered leaving the job empty */
    #[inline(never)]
    pub(crate) fn take_gather(&mut self) -> Gather {
        match self.jobs.last_mut() { Some(Job::Gather(j)) => j.taken(), _ => Gather::new(Takes::Any, Vec::new()) }
    }

    /* whether `o` is a generator or an iterator a builtin's job may take as frames */
    #[inline(always)]
    pub(crate) fn may_job(&self, o: Val) -> bool { o.is_heap() && matches!(self.heap.get(o), HeapObj::Coroutine(_) | HeapObj::Iter(..)) }

    /* `gather_job` when `o` is a generator and `pull_map` for a `map` or `filter` */
    #[inline(never)]
    pub(crate) fn gather_gen(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        match self.heap.try_get(o) {
            Some(HeapObj::Coroutine(_)) => self.gather_job(o, takes, extra.to_vec()),
            Some(HeapObj::Iter(..)) => Ok(self.holds_user(o, 0, true) && (self.lazy_pull(o, takes, extra)? || self.pull_map(o, takes, extra)? || self.drive_iter(o, takes, extra)?)),
            _ => Ok(false),
        }
    }

    /* runs builtin `takes` over a user `map` or `filter` `o` with its calls as frames */
    #[inline(never)]
    pub(crate) fn pull_map(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        if self.budget == 0 { return Ok(false); }
        let Some(HeapObj::Iter(frame, _)) = self.heap.try_get(o) else { return Ok(false) };
        let (f, src, src2, filter) = match &*frame.borrow() {
            IterFrame::Map { f, its, .. } if its.len() == 1 => (*f, its[0], Val::undef(), false),
            IterFrame::Map { f, its, .. } if its.len() == 2 => (*f, its[0], its[1], false),
            &IterFrame::Filter { f, it, .. } if !f.is_undef() => (f, it, Val::undef(), true),
            _ => return Ok(false),
        };
        let two = !src2.is_undef();
        // both iterators a two-iterator map steps must step without user code or neither steps here
        if two && !(self.plain_iter(src) && self.plain_iter(src2)) { return Ok(false); }
        let Some(fi) = (if two { self.job_fn2(f) } else { self.job_fn(f) }) else { return Ok(false) };
        let first = match self.pull_next(src, src2) {
            None => return Ok(false),
            Some(r) => r?,
        };
        let mut g = self.gather_of(takes, extra.to_vec(), o);
        let Some((x, y)) = first else {
            // an empty one gives what the builtin gives of no items
            let r = self.gather_end(&mut g, Val::none())?;
            // a native's fold that drains on may have opened the next iterable's frames
            if !(r.is_undef() && self.entered) { self.put(r, g.to); }
            return Ok(true);
        };
        g.pull = Pull { src, src2, filter, item: x };
        if two { self.open_job2(fi, f, x, y, 0, Job::Gather(Box::new(g))); } else { self.key_job(fi, f, x, Job::Gather(Box::new(g))); }
        Ok(true)
    }

    /* op `rip` over a stacked `map` or `filter` runs as a job and then again */
    #[inline(never)]
    pub(crate) fn list_job(&mut self, rip: usize) -> Result<bool, VmErr> {
        // an op at the code's start names no rerun place being a link's empty one
        let Ok(at) = self.under(1) else { return Ok(false) };
        let it = self.slot(at);
        if rip == 0 || !self.holds_user(it, 0, true) { return Ok(false); }
        self.cut_to(at);
        if !self.with_roots([it], |vm| vm.gather_gen(it, Takes::All, &[]))? { self.push(it); return Ok(false); }
        if !self.entered { return Ok(false); }
        self.set_ret(rip, NO_DEST);
        Ok(true)
    }

    /* `list_job` of `it` a register op reads which takes the list in `retake` */
    #[inline(never)]
    pub(crate) fn list_job_of(&mut self, it: Val, rip: usize) -> Result<bool, VmErr> {
        if rip == 0 || !self.holds_user(it, 0, true) { return Ok(false); }
        if !self.with_roots([it], |vm| vm.gather_gen(it, Takes::All, &[]))? { return Ok(false); }
        if !self.entered { return Ok(false); }
        self.set_ret(rip, NO_DEST);
        Ok(true)
    }

    /* `l += it` appending as frames for a generator or an iterator running user code */
    #[inline(never)]
    pub(crate) fn extend_job(&mut self, l: Val, it: Val) -> Result<bool, VmErr> {
        if !matches!(self.heap.try_get(l), Some(HeapObj::List(_))) { return Ok(false); }
        match self.heap.try_get(it) {
            Some(HeapObj::Coroutine(_)) => self.with_roots([l, it], |vm| vm.gather_job(it, Takes::Extend(l), Vec::new())),
            Some(HeapObj::Iter(..)) if self.holds_user(it, 0, true) => self.with_roots([l, it], |vm| vm.gather_gen(it, Takes::Extend(l), &[])),
            _ => Ok(false),
        }
    }

    /* membership in iterator `o` stepped by the job so its user code runs as frames */
    #[inline(never)]
    pub(crate) fn contains_job(&mut self, o: Val, takes: Takes) -> Result<bool, VmErr> {
        Ok(self.lazy_pull(o, takes, &[])? || self.drive_iter(o, takes, &[])?)
    }

    /* runs builtin `takes` over iterator `o` whose steps run user code as frames first */
    #[inline(never)]
    pub(crate) fn drive_iter(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> {
        // a generator's or a method's list reaches these takes only where a generator frame ends
        if matches!(takes, Takes::Retry(..) | Takes::Prefetch(_) | Takes::Close | Takes::Hold) { return Ok(false); }
        if self.budget == 0 || !self.holds_user(o, 0, true) { return Ok(false); }
        let depth = self.jobs.len();
        self.jobs.push(Job::Gather(Box::new(self.gather_of(takes, extra.to_vec(), o))));
        let drive = self.gather_drive();
        if drive.is_err() { self.jobs.truncate(depth); }
        match drive? {
            Drive::Done(r, to) => self.put(r, to),
            Drive::Fold(fi, func, d, args, n) => { self.open_fold(fi, func, d, args, n); (self.entered, self.yielded) = (true, true); }
            Drive::Opened => (self.entered, self.yielded) = (true, true),
        }
        Ok(true)
    }

    /* `gather_drive` after a fold with a frame-opening step left as `Done(undef)` for `drive_on` */
    #[inline(never)]
    pub(crate) fn fold_drive(&mut self) -> Result<Drive, VmErr> {
        match self.jobs.last() {
            Some(Job::Gather(j)) if !(j.cur.is_some() || self.plain_iter(j.co)) => Ok(Drive::Done(Val::undef(), Dest::Top)),
            _ => self.drive_to(true),
        }
    }

    /* what `fold_drive` gave with the step it left now done */
    #[inline(never)]
    pub(crate) fn drive_on(&mut self, out: Result<Drive, VmErr>) -> Result<Drive, VmErr> {
        match out { Ok(Drive::Done(v, _)) if v.is_undef() => self.gather_drive(), out => out }
    }

    /* whether `it` is a builtin iterator that steps without user code */
    fn plain_iter(&self, it: Val) -> bool {
        matches!(self.heap.try_get(it), Some(HeapObj::Iter(f, _)) if f.try_borrow().is_ok_and(|f| !matches!(*f, IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. } | IterFrame::Call { .. } | IterFrame::Coroutine(_) | IterFrame::UserDefined(_) | IterFrame::Under(_))))
    }

    /* a map's next items from one or two iterators and none unless they step plainly */
    fn pull_next(&mut self, src: Val, src2: Val) -> Option<Result<Option<(Val, Val)>, VmErr>> {
        let x = match self.plain_next(src)? { Ok(Some(x)) => x, Ok(None) => return Some(Ok(None)), Err(e) => return Some(Err(e)) };
        if src2.is_undef() { return Some(Ok(Some((x, Val::undef())))); }
        Some(self.plain_next(src2)?.map(|y| y.map(|y| (x, y))))
    }

    /* the top `map` or `filter` takes or skips `v` then reruns or ends once spent */
    fn pull_take(&mut self, v: Val) -> Result<Step, VmErr> {
        let Some(Job::Gather(j)) = self.jobs.last() else { return Err(cold_runtime("a job frame without its job")) };
        let Pull { src, src2, filter, item } = j.pull;
        let take = if !filter { Some(v) } else if self.truthy(v) { Some(item) } else { None };
        // an item whose key the builtin runs makes the job step the iterator itself
        if let Some(x) = take && self.fold_of(x, NATIVE).is_some() && let Some(Job::Gather(j)) = self.jobs.last_mut() {
            (j.pull.item, j.pull.src) = (x, Val::undef());
            // the frame's end pops this stand-in in the job's place as with a `Feed`
            self.jobs.push(Job::Stand);
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
                // a native's fold ends after this frame since the next iterable may open frames
                if matches!(j.takes, Takes::Fold | Takes::FoldItems | Takes::FoldWords | Takes::FoldExtreme(_)) {
                    // an empty cursor ends the drive at once with no step of the spent source
                    (j.pull.item, j.pull.src, j.cur) = (Val::undef(), Val::undef(), Some(IterFrame::Range { cur: 0, end: 0, step: 1 }));
                    self.jobs.push(Job::Stand);
                    return Ok(Step::Drive);
                }
                let mut g = j.taken();
                Ok(Step::Give(self.gather_end(&mut g, Val::none())?))
            }
            Some(Err(e)) => Err(e),
            None => Err(cold_runtime("a map's iterator no longer steps")),
        }
    }

    /* runs the builtin `j` gathered for or none once it opens a frame or floor */
    #[inline(never)]
    pub(crate) fn collected(&mut self, j: Gather) -> Result<Option<Val>, VmErr> {
        self.delivering(j.to, |vm| vm.collect_end(j))
    }

    /* `collected` with the gather's destination set */
    fn collect_end(&mut self, j: Gather) -> Result<Option<Val>, VmErr> {
        match j.takes {
            Takes::Hold => {
                self.hold_items(j.co, j.items);
                let v = self.hold_then(j.extra)?;
                if self.entered || self.floor_req { return Ok(None); }
                v.map(Some).ok_or(cold_runtime("a held call gave no value"))
            }
            _ => Err(cold_runtime("a gathered list for no builtin")),
        }
    }

    /* marks the generator frame on top as a job's */
    pub(crate) fn mark_job(&mut self) {
        let at = self.head_at(self.top_pool);
        self.regs[at] = Val::int(self.head(at).as_int() | JOB << 40);
    }

    /* makes the top job generator frame's caller carry on at `ret` past the builtin's call */
    #[inline(never)]
    pub(crate) fn gather_ret(&mut self, ret: usize) {
        let at = self.head_at(self.top_pool);
        let link = self.head(at).as_int();
        if link & 0xFF_FFFF == 0 { self.regs[at] = Val::int(link | ret as i64); }
    }

    /* whether the running frame is a generator a builtin's job runs */
    #[inline(always)]
    pub(crate) fn top_gather(&self) -> bool { self.acts != 0 && self.head(self.head_at(self.top_pool)).as_int() >> 40 & (JOB | super::act::GEN) == JOB | super::act::GEN }

    /* takes yield `v` without user code or failure else false unchanged for the full path */
    #[inline(always)]
    pub(crate) fn gather_fast(&mut self, v: Val) -> bool {
        if self.budget == 0 || self.heap.has_work() { return false; }
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return false };
        // a native's fold takes the item through its function with no frame
        if !take_plain(j, v, &self.heap) { return self.fold_fast(v); }
        self.budget -= 1;
        self.heap.set_budget(self.budget);
        true
    }

    /* takes `v` from the suspended generator and gives the builtin's result once that ends it */
    pub(crate) fn gather_take(&mut self, v: Val) -> Result<Option<Val>, VmErr> {
        self.charge_step()?;
        let Some(Job::Gather(j)) = self.jobs.last() else { return Err(cold_runtime("a job frame without its job")) };
        let takes = j.takes;
        Ok(match takes {
            Takes::All | Takes::Retry(..) | Takes::Hold | Takes::FoldItems => {
                let n = j.items.len();
                if (n + 1).saturating_mul(core::mem::size_of::<Val>()) >= self.heap.room() { return Err(crate::vm::cold_heap()); }
                if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.items.push(v); }
                // an unpack that took one item past its targets fails leaving the generator suspended
                match takes { Takes::Retry(_, limit) if limit != 0 && n + 1 >= limit as usize => return Err(unpacked_past(limit)), _ => None }
            }
            Takes::Any => self.truthy(v).then_some(Val::bool(true)),
            Takes::Every => (!self.truthy(v)).then_some(Val::bool(false)),
            Takes::Next(_) | Takes::Prefetch(_) => Some(v),
            Takes::Await => { self.await_back = 1; Some(v) }
            Takes::Then(b) => { self.then_out = Some((b, Ok(crate::vm::coro::Step::Yield(v)))); Some(v) }
            Takes::Close => return Err(crate::lang::EngineError::Generator(crate::lang::Gen::IgnoredClose(crate::lang::Body::Generator)).into()),
            Takes::Contains(item, not) => {
                let base = self.temp_roots.len();
                self.temp_roots.push(v);
                // the item compares first as `in` over an iterator does
                let hit = self.member_eq(v, item);
                self.temp_roots.truncate(base);
                hit?.then_some(Val::bool(!not))
            }
            Takes::Extend(l) => {
                if let Some(HeapObj::List(rc)) = self.heap.try_get(l) { rc.borrow_mut().push(v); }
                None
            }
            Takes::Fold => self.fold_take_item(v)?,
            Takes::FoldWords => {
                // a word int adds in place and any other hands the rest to the fold
                let plain = match self.jobs.last_mut() { Some(Job::Gather(j)) => take_plain(j, v, &self.heap), _ => false };
                if plain { None } else {
                    self.fold_spill();
                    self.fold_take_item(v)?
                }
            }
            Takes::FoldExtreme(_) => self.fold_best(v)?,
        })
    }

    /* the key function the job on top calls on `v` as a frame or None */
    pub(crate) fn fold_of(&mut self, v: Val, resume: usize) -> Option<(usize, Val, Fold, [Val; 2], usize)> {
        let Some(Job::Gather(j)) = self.jobs.last() else { return None };
        if j.takes == Takes::Fold { return self.fold_call(v, resume); }
        None
    }

    /* takes `r` the key fold `d` gave into the native's fold on top */
    #[inline(never)]
    pub(crate) fn fold_take(&mut self, d: Fold, r: Val) -> Result<(), VmErr> {
        if d.resume() == TOOK {
            let first = self.fold_kept()?;
            if let Some(v) = first && let Some(Job::Gather(j)) = self.jobs.last_mut() { j.pull.item = v; }
            return Ok(());
        }
        if matches!(self.jobs.last(), Some(Job::Gather(j)) if j.takes == Takes::Fold) { return self.fold_called(r, d.resume() == NATIVE); }
        Err(cold_runtime("a key without its fold"))
    }

    /* opens key `func` of function `fi` on `args` as the frame of fold `d` */
    pub(crate) fn open_fold(&mut self, fi: usize, func: Val, d: Fold, args: [Val; 2], n: usize) {
        let job = Job::Fold(d);
        // a generator over rows keeps its rows in its window while the key runs
        if n == 2 { self.open_job2(fi, func, args[0], args[1], 0, job); } else { self.open_job(fi, func, args[0], 0, super::act::NO_DEST, job); }
        (self.entered, self.yielded) = (false, false);
    }

    /* `next` of a generator that returned `ret` unyielded gives `d` or an end carrying `ret` */
    pub(crate) fn next_end(&mut self, d: Val, ret: Val) -> Result<Val, VmErr> {
        if !d.is_undef() { return Ok(d); }
        if ret.is_none() { return Err(VmErr::Of(crate::lang::ErrKind::Stop, alloc::string::String::new())); }
        let exc = self.heap.engine_error(crate::lang::ErrKind::Stop, alloc::vec![ret])?;
        self.pending.exc_val = Some(exc);
        Err(VmErr::Of(crate::lang::ErrKind::Stop, self.display(ret)))
    }

    /* the builtin's result once its generator returned `ret` and undef when it opened frames */
    pub(crate) fn gather_end(&mut self, j: &mut Gather, ret: Val) -> Result<Val, VmErr> {
        // only an end running a builtin on the items opens jobs that deliver like it
        if !matches!(j.takes, Takes::Fold | Takes::FoldItems | Takes::FoldWords | Takes::FoldExtreme(_) | Takes::Hold) { return self.end_gather(j, ret); }
        self.delivering(j.to, |vm| vm.end_gather(j, ret))
    }

    /* `gather_end` with the gather's destination set */
    fn end_gather(&mut self, j: &mut Gather, ret: Val) -> Result<Val, VmErr> {
        match j.takes {
            Takes::All => {
                let frame = IterFrame::Seq { items: core::mem::take(&mut j.items).into(), idx: 0 };
                self.heap.alloc(HeapObj::Iter(Rc::new(core::cell::RefCell::new(frame)), self.natives.names().iters[0]))
            }
            Takes::Any | Takes::Every => self.truth_end(j),
            Takes::Next(d) => self.next_end(d, ret),
            Takes::Contains(_, not) => Ok(Val::bool(not)),
            Takes::Extend(l) => Ok(l),
            Takes::Close => Ok(ret),
            Takes::Await => { self.await_back = 2; Ok(ret) }
            Takes::Then(b) => { self.then_out = Some((b, Ok(crate::vm::coro::Step::Return(ret)))); Ok(ret) }
            // a held native binding runs on the items its generator gave
            Takes::Hold => {
                let (items, extra) = (core::mem::take(&mut j.items), core::mem::take(&mut j.extra));
                self.hold_items(j.co, items);
                let v = self.hold_then(extra)?;
                if self.entered || self.floor_req { return Ok(Val::undef()); }
                v.ok_or(cold_runtime("a held call gave no value"))
            }
            Takes::Fold | Takes::FoldItems | Takes::FoldWords | Takes::FoldExtreme(_) => self.fold_ended(j, ret),
            Takes::Retry(..) | Takes::Prefetch(_) => Err(cold_runtime("a collection ended as a builtin's result")),
        }
    }
}

/* takes `v` into builtin `j` without user code or failure and false leaves it unchanged */
#[inline(always)]
pub(crate) fn take_plain(j: &mut Gather, v: Val, heap: &HeapPool) -> bool {
    match j.takes {
        // a words fold's total stays a word while its items are word ints
        Takes::FoldWords => match j.total {
            Total::Word(w) if v.is_int() && let Some(n) = w.checked_add(v.as_int()) => j.total = Total::Word(n),
            _ => return false,
        },
        Takes::FoldExtreme(is_min) => {
            let best = j.acc;
            let wins = if best.is_undef() { true }
                else if v.is_int() && best.is_int() { if is_min { v.as_int() < best.as_int() } else { v.as_int() > best.as_int() } }
                else if v.is_float() && best.is_float() && !v.as_float().is_nan() && !best.as_float().is_nan() { if is_min { v.as_float() < best.as_float() } else { v.as_float() > best.as_float() } }
                else { return false };
            if wins { j.acc = v; }
        }
        // the last item a retry takes ends it on the full path
        Takes::Retry(_, limit) if limit != 0 && j.items.len() + 1 >= limit as usize => return false,
        Takes::All | Takes::Retry(..) | Takes::Hold | Takes::FoldItems => {
            // inline ints take no slot so memory left caps items where the full path raises
            if (j.items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= heap.room() { return false; }
            j.items.push(v);
        }
        Takes::Any | Takes::Every => {
            let find = matches!(j.takes, Takes::Any);
            let truth = heap.bare_truth(v);
            if truth != Some(!find) { return false; }
        }
        Takes::Extend(l) => match heap.try_get(l) {
            Some(HeapObj::List(rc)) if (rc.borrow().len() + 1).saturating_mul(core::mem::size_of::<Val>()) < heap.room() => rc.borrow_mut().push(v),
            _ => return false,
        },
        // a native's fold takes what its own take adds with no call
        Takes::Fold => return j.fold.as_mut().is_some_and(|f| f.take_inline(v, heap)),
        Takes::Next(_) | Takes::Contains(..) | Takes::Close | Takes::Await | Takes::Then(_) | Takes::Prefetch(_) => return false,
    }
    true
}

/* the helpers `through_loop` binds written in the runtime's own table */
fn calls_source(syntax: &crate::lexer::Syntax) -> Option<alloc::string::String> {
    use crate::lexer::TokenType as T;
    let w = |k| syntax.spelling(k);
    let (def, ret, none, eq, comma, lpar, rpar) = (w(T::Fn)?, w(T::Return)?, w(T::Nil)?, w(T::Equal)?, w(T::Comma)?, w(T::ParenOpen)?, w(T::ParenClose)?);
    // braces keep each body on its header line where indentation moves it below
    let (open, close) = if syntax.delimited() { (crate::s!(" ", str w(T::BlockOpen)?, " "), crate::s!(" ", str w(T::BlockClose)?)) } else { (crate::s!(str w(T::Colon)?, "\n    "), alloc::string::String::new()) };
    let (mut src, sep) = (alloc::string::String::new(), crate::s!(str comma, " "));
    for n in 1..=3u8 {
        let (args, num) = (["a", "b", "c"][..n as usize].join(sep.as_str()), crate::s!(int n as i64));
        let parts: [&str; 18] = [def, " call", &num, lpar, &args, comma, " f", eq, none, rpar, &open, ret, " f", lpar, &args, rpar, &close, "\n"];
        for part in parts { src.push_str(part); }
    }
    Some(src)
}
