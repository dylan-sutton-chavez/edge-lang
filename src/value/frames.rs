use alloc::vec::Vec;

use super::Val;
use super::{HeapObj, HeapPool, View};
use super::err::VmErr;

/* Scheduler state per coroutine, stepped round-robin until the target leaves Ready/Sleeping. */
#[derive(Clone, Debug)]
pub enum CoroState {
    /// Resumable on next tick.
    Ready,
    /// Suspended until `until_ns`, the scheduler fast-forwards when all are Sleeping.
    Sleeping(u64),
    /// Parked in `receive()` with an empty queue, resumed when the host pushes a message.
    WaitingEvent,
    /// Parked mid-`CallExtern` with its correlation id, resumed when the host calls `set_host_result_by_id(id)`.
    WaitingHostCall(u64),
    /// Parked in `run(...)` / `gather(...)` / `with_timeout(...)` until `tasks` all terminate, `kind` selects how to finalize.
    WaitingForChildren { tasks: Vec<Val>, kind: WaitKind },
    /// Next resume injects a `CancelledError` raise.
    CancelPending,
    /// Next resume raises this error at the park point, with the user instance when one exists.
    Raising(VmErr, Option<Val>),
    /// Returned with this Val.
    Done(Val),
    /// Raised, stored verbatim for `gather` / `with_timeout`.
    Errored(VmErr),
    /// Cancellation already observed, yields `None` to gather() peers.
    Cancelled,
}

impl CoroState {
    /* Finished for good, by a value, an error or a cancel. */
    pub fn is_terminal(&self) -> bool { matches!(self, Self::Done(_) | Self::Errored(_) | Self::Cancelled) }
}

/* How `WaitingForChildren` finalizes when its tasks all reach terminal. `Run` returns target's value (or its error), `Gather` returns a list of all values (or the first error), `Timeout` returns the target's value, or `TimeoutError` if the deadline expired before completion. */
#[derive(Clone, Debug)]
pub enum WaitKind {
    Run(Val),
    Gather,
    Timeout { deadline_ns: u64, target: Val },
}

#[derive(Clone, Debug)]
pub struct CoroutineHandle {
    /// User-provided Coroutine HeapObj.
    pub coro: Val,
    pub state: CoroState,
}

// Suspended sync helper frame, a plain user fn called from a coroutine hit a yielding builtin mid-execution, so its state is snapshotted and parked on the enclosing Coroutine. Frames stack innermost-last, resume walks inside-out so each return value lands on the next frame's stack at the Call site. `exception_delta` carries the helper's try/except frames pushed in its exec so they survive the yield.
#[derive(Clone, Debug)]
pub struct SyncFrame {
    pub ip: usize,
    pub fi: usize,
    // The function object, whose captured cells take its nonlocal writes once the resumed frame returns.
    pub func: Val,
    pub slots: Vec<Val>,
    pub stack_delta: Vec<Val>,
    pub iter_delta: Vec<IterFrame>,
    pub exception_delta: Vec<ExceptionFrame>,
    // The instance an `__init__` hands back in place of what it returns, undef for any other call.
    pub instance: Val,
}

/* Block-stack frame role. Except catches exceptions, Finally runs on every exit path. */
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlockKind { Except, Finally }

/* One reason a finally/with cleanup body is running, pushed on entry, popped by EndFinally. */
#[derive(Clone, Debug)]
pub enum Unwind {
    // Reached by normal fall-through, EndFinally just continues.
    Normal,
    Return(Val),
    // break/continue, run `remaining` more cleanups, then resume at `target`.
    Goto { target: usize, remaining: u16 },
    // An exception passing through, with the byte offset it was raised at.
    Reraise(VmErr, Option<u32>),
}

/* Saved stack/iter/with/unwind depths for unwinding to a handler. Stored on the active Coroutine so `try`/`except` survives yields. */
#[derive(Clone, Debug)]
pub struct ExceptionFrame {
    pub kind: BlockKind,
    pub handler_ip: usize,
    pub stack_depth: usize,
    pub iter_depth: usize,
    pub with_depth: usize,
    pub unwind_depth: usize,
    // Exceptions being handled when the block began, the ones its handlers add end with it.
    pub handling_depth: usize,
}

// Coroutine body, user fn (Fn) or the implicit module-body coro (Module -> self.chunk).
#[derive(Clone, Copy, Debug)]
pub enum BodyRef {
    Fn(usize),
    Module,
}

/* The `fi` of a frame for the call of `exec` or `eval` that ran a text, which names no function. */
pub const EXEC_FRAME: usize = usize::MAX;
pub const EVAL_FRAME: usize = usize::MAX - 1;

/* Call-site snapshot for traceback rendering, built only as an error unwinds through the call. */
#[derive(Clone, Debug)]
pub struct CallFrame {
    pub fi: usize,
    pub call_byte_pos: u32,
    // The caller's source and path, taken only once the frame stays for a traceback.
    pub caller_source: Option<alloc::sync::Arc<alloc::string::String>>,
    pub caller_path: Option<alloc::sync::Arc<alloc::string::String>>,
    // How many more times the same call unwound right after this one, a recursion kept once.
    pub repeats: u32,
}

/* ForIter state, consumed one item per `next_item`. */
#[derive(Clone, Debug)]
pub enum IterFrame {
    // Shared items, so a generator saving its frames on every yield copies none of them.
    Seq { items: alloc::rc::Rc<[Val]>, idx: usize },
    // Live list view, items appended during the loop are visited.
    List { rc: alloc::rc::Rc<core::cell::RefCell<Vec<Val>>>, idx: usize },
    Range { cur: i64, end: i64, step: i64 },
    // A range with a bound past i64, stepped through the wide int kind.
    BigRange { cur: Val, end: Val, step: Val },
    Coroutine(Val),
    // User-defined iterator, holds the value returned by `__iter__`, each step calls its `__next__`.
    UserDefined(Val),
    // `reversed(list)` walking the live list from its end, `idx` counting the items still ahead.
    ListRev { rc: alloc::rc::Rc<core::cell::RefCell<Vec<Val>>>, idx: usize },
    // `map(f, *its)`, calling `f` as each item is asked for.
    Map { f: Val, its: alloc::rc::Rc<[Val]> },
    // `filter(f, it)`, a None `f` keeping the truthy items.
    Filter { f: Val, it: Val },
    Zip { its: alloc::rc::Rc<[Val]> },
    // `enumerate(it, start)`, `n` the count the next item pairs with.
    Enumerate { it: Val, n: Val },
    // `iter(f, sentinel)`, calling `f` until it returns `sentinel`.
    Call { f: Val, sentinel: Val },
    // A set or a reversed dict read up front, a later step failing once `of` changed size.
    Watched { items: alloc::rc::Rc<[Val]>, idx: usize, of: Val, len: usize },
    // A dict or its view read live, `left` the items it still owes, failing once the size or the keys change.
    DictWalk { of: Val, idx: usize, left: usize, len: usize, kind: View },
}

/* The size of the dict or set a watched frame reads. */
#[inline]
pub(crate) fn watched_len(heap: &HeapPool, of: Val) -> usize {
    match heap.try_get(of) {
        Some(HeapObj::Dict(d) | HeapObj::FrozenDict(d)) => d.borrow().len(),
        Some(HeapObj::Set(s)) => s.borrow().len(),
        _ => 0,
    }
}

/* One step of a range with a bound past i64, kept apart so the plain range step stays small enough to inline. */
#[inline(never)]
fn big_range_step(heap: &mut HeapPool, cur: &mut Val, end: Val, step: Val) -> Result<Option<Val>, VmErr> {
    let order = |a: Val, b: Val| if a.is_int() && b.is_int() { a.as_int().cmp(&b.as_int()) } else { heap.foreign_cmp(a, b).unwrap_or(core::cmp::Ordering::Equal) };
    let ahead = if order(step, Val::int(0)).is_gt() { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater };
    if order(*cur, end) != ahead { return Ok(None); }
    let v = *cur;
    *cur = heap.foreign_binop(super::BinOp::Add, v, step)?.unwrap_or(end);
    Ok(Some(v))
}

impl IterFrame {
    /* Stateless steps only, the frames that call user code or other iterators step in the VM. */
    pub fn next_item(&mut self, heap: &mut HeapPool) -> Result<Option<Val>, VmErr> {
        match self {
            Self::Coroutine(_) | Self::UserDefined(_) | Self::Map { .. } | Self::Filter { .. } | Self::Zip { .. } | Self::Enumerate { .. } | Self::Call { .. } => Ok(None),
            Self::Seq { items, idx } => {
                if *idx < items.len() { let v = items[*idx]; *idx += 1; Ok(Some(v)) } else { Ok(None) }
            }
            // The failure sticks, every later step fails the same way.
            Self::Watched { items, idx, of, len } => {
                if watched_len(heap, *of) != *len {
                    *len = usize::MAX;
                    let set = matches!(heap.try_get(*of), Some(HeapObj::Set(_)));
                    return Err(VmErr::Runtime(if set { "Set changed size during iteration" } else { "dictionary changed size during iteration" }));
                }
                if *idx < items.len() { let v = items[*idx]; *idx += 1; Ok(Some(v)) } else { Ok(None) }
            }
            // Both failures stick, so every later step fails the same way.
            Self::DictWalk { of, idx, left, len, kind } => {
                let Some(d) = heap.dict_cell(*of) else { return Ok(None) };
                let d = d.clone();
                let m = d.borrow();
                if m.len() != *len {
                    *len = usize::MAX;
                    return Err(VmErr::Runtime("dictionary changed size during iteration"));
                }
                while *idx < m.entry_count() {
                    let (k, v) = (m.key_at(*idx), m.value_at(*idx));
                    if k.is_undef() { *idx += 1; continue; }
                    if *left == 0 { return Err(VmErr::Runtime("dictionary keys changed during iteration")); }
                    (*idx, *left) = (*idx + 1, *left - 1);
                    drop(m);
                    return Ok(Some(match kind {
                        View::Keys => k,
                        View::Values => v,
                        View::Items => heap.alloc(HeapObj::Tuple(alloc::vec![k, v]))?,
                    }));
                }
                Ok(None)
            }
            Self::List { rc, idx } => {
                let items = rc.borrow();
                if *idx < items.len() { let v = items[*idx]; *idx += 1; Ok(Some(v)) } else { Ok(None) }
            }
            // A list that shrank past the cursor ends the walk.
            Self::ListRev { rc, idx } => {
                let items = rc.borrow();
                if *idx == 0 || *idx > items.len() { *idx = 0; return Ok(None); }
                *idx -= 1;
                Ok(Some(items[*idx]))
            }
            Self::Range { cur, end, step } => {
                let done = if *step > 0 { *cur >= *end } else { *cur <= *end };
                if done { Ok(None) } else {
                    let v = *cur;
                    // Clamp past the i64 edge so the next `done` check ends the range, never overflows.
                    *cur = cur.checked_add(*step).unwrap_or(if *step > 0 { i64::MAX } else { i64::MIN });
                    // Box magnitudes beyond the 48-bit inline range.
                    Ok(Some(heap.int(v as i128)?))
                }
            }
            Self::BigRange { cur, end, step } => big_range_step(heap, cur, *end, *step),
        }
    }

    /* Visit each Val in this frame, an i64 Range holds none. */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        match self {
            IterFrame::Seq { items, .. } => for &v in items.iter() { f(v); },
            IterFrame::Watched { items, of, .. } => { f(*of); for &v in items.iter() { f(v); } }
            IterFrame::DictWalk { of, .. } => f(*of),
            IterFrame::List { rc, .. } | IterFrame::ListRev { rc, .. } => for &v in rc.borrow().iter() { f(v); },
            Self::Coroutine(v) | Self::UserDefined(v) => f(*v),
            IterFrame::Map { f: g, its } => { f(*g); for &v in its.iter() { f(v); } }
            IterFrame::Filter { f: g, it } => { f(*g); f(*it); }
            IterFrame::Zip { its } => for &v in its.iter() { f(v); },
            IterFrame::Enumerate { it, n } => { f(*it); f(*n); }
            IterFrame::Call { f: g, sentinel } => { f(*g); f(*sentinel); }
            IterFrame::Range { .. } => {}
            IterFrame::BigRange { cur, end, step } => { f(*cur); f(*end); f(*step); }
        }
    }
}

impl SyncFrame {
    /* Visit all Vals across slots, stack delta, and iter frames. */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        f(self.func);
        f(self.instance);
        for &v in &self.slots { f(v); }
        for &v in &self.stack_delta { f(v); }
        for fr in &self.iter_delta { fr.for_each_val(f); }
    }
}
