use alloc::vec::Vec;

use super::Val;
use super::{HeapObj, HeapPool};
use super::err::VmErr;

/* what the module body waits for while it is suspended to the host */
#[derive(Clone, Debug)]
pub enum CoroState {
    Ready,
    // parked on an empty event queue until the host pushes a message
    WaitingEvent,
    // parked inside a `CallExtern` until the host calls `set_host_result_by_id` with its id
    WaitingHostCall(u64),
    // the next resume raises this error where it parked with the user instance if any
    Raising(VmErr, Option<Val>),
}

/* the module body the host drives and what it waits for */
#[derive(Clone, Debug)]
pub struct CoroutineHandle {
    pub coro: Val,
    pub state: CoroState,
}

// a plain function frame parked on its coroutine when a builtin it called yielded
#[derive(Clone, Debug)]
pub struct ParkedFrame {
    pub ip: usize,
    pub fi: usize,
    // the function whose captured cells take its nonlocal writes once the resumed frame returns
    pub func: Val,
    pub slots: Vec<Val>,
    // the recursion levels it kept through sections as it parked
    pub levels: Vec<Val>,
    pub iter_delta: Vec<IterFrame>,
    pub exception_delta: Vec<ExceptionFrame>,
    // the instance a constructor call hands back instead of the return value and undef otherwise
    pub instance: Val,
    // the code layout a parked section resumes in and 0 for none
    pub layout: u32,
    // the top it parked over rows with and zero when it held no row
    pub top: u16,
}

/* `Finally` runs on every exit path while `Except` only catches */
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlockKind { Except, Finally }

/* why a finally body runs pushed on entry and popped by EndFinally */
#[derive(Clone, Debug)]
pub enum Unwind {
    // reached by falling through so `EndFinally` just continues
    Normal,
    Return(Val),
    // a break or continue that runs `remaining` more cleanups then resumes at `target`
    Goto { target: usize, remaining: u16 },
    // an exception passing through with where it was raised and its own object
    Reraise(VmErr, Option<u32>, Val),
}

/* stack iter and unwind depths a handler unwinds to kept on the coroutine across yields */
#[derive(Clone, Copy, Debug)]
pub struct ExceptionFrame {
    pub kind: BlockKind,
    pub handler_ip: usize,
    // the recursion levels kept as the block began which a handler cuts back to
    pub levels: usize,
    pub iter_depth: usize,
    pub unwind_depth: usize,
    // the handling depth as the block began so exceptions its handlers add end with it
    pub handling_depth: usize,
    // the section recursion levels as the block began which a handler cuts back to
    pub turned: usize,
    // the register plus one a lowered handler takes the exception in or 0 for none
    pub slot: u16,
}

// the body of a coroutine as a function index or the module body
#[derive(Clone, Copy, Debug)]
pub enum BodyRef {
    Fn(usize),
    Module,
}

/* a call site for the traceback built only as an error unwinds through the call */
#[derive(Clone, Debug)]
pub struct CallFrame {
    pub fi: usize,
    pub call_byte_pos: u32,
    // the caller source and path taken only once the frame stays for a traceback
    pub caller_source: Option<alloc::sync::Arc<alloc::string::String>>,
    pub caller_path: Option<alloc::sync::Arc<alloc::string::String>>,
    // how many more times the same call unwound right after so a recursion shows once
    pub repeats: u32,
}

/* the state of a `ForIter` loop consumed one item per `next_item` */
#[derive(Clone, Debug)]
pub enum IterFrame {
    // shared items so a generator saving its frames on every yield copies none of them
    Seq { items: alloc::rc::Rc<[Val]>, idx: usize },
    // a live view of the list so items appended during the loop are visited
    List { rc: alloc::rc::Rc<core::cell::RefCell<Vec<Val>>>, idx: usize },
    Range { cur: i64, end: i64, step: i64 },
    // a range with a bound past i64 stepped through the wide int kind
    BigRange { cur: Val, end: Val, step: Val },
    Coroutine(Val),
    // calls `f` per item asked for and `own` when only it holds its first iterator
    Map { f: Val, its: alloc::rc::Rc<[Val]>, own: bool },
    // an undef `f` keeps the items true in themselves and `own` is as for `Map`
    Filter { f: Val, it: Val, own: bool },
    // a builtin iterator object the loop steps in place
    UserDefined(Val),
    // walks the live list from its end with `idx` counting the items still ahead
    ListRev { rc: alloc::rc::Rc<core::cell::RefCell<Vec<Val>>>, idx: usize },
    Zip { its: alloc::rc::Rc<[Val]>, strict: bool },
    // `n` is the count the next item pairs with
    Enumerate { it: Val, n: Val },
    // calls `f` until it returns `sentinel`
    Call { f: Val, sentinel: Val },
    // a set or reversed dict read up front and failing once `of` changes size
    Watched { items: alloc::rc::Rc<[Val]>, idx: usize, of: Val, len: usize },
    // a dict read live with `left` keys owed failing once its size or keys change
    DictWalk { of: Val, idx: usize, left: usize, len: usize, part: DictPart },
    // a generator section mark over `n` iterators of its own that the loop `PopIter` drops
    Under(u32),
    // a generator section yielding inside `try` and what its handlers stacked
    Held(u32, alloc::boxed::Box<Held>),
    // the octets of a bytes or bytearray value copied once and read one a step
    Octets { bytes: alloc::rc::Rc<[u8]>, idx: usize },
    // a text copied once and read one character a step from byte `at`
    Chars { text: alloc::rc::Rc<str>, at: usize },
}

/* what a walk over a dict gives for each entry */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictPart { Keys, Values, Items }

/* the handler unwind and handled stacks a generator section left while the loop runs */
#[derive(Clone, Debug, Default)]
pub struct Held {
    pub excs: Vec<ExceptionFrame>,
    pub unwinds: Vec<Unwind>,
    pub handling: Vec<(Val, Option<u32>)>,
}

/* the size of the dict or set a watched frame reads */
#[inline]
pub(crate) fn watched_len(heap: &HeapPool, of: Val) -> usize {
    match heap.try_get(of) {
        Some(HeapObj::Dict(d)) => d.borrow().len(),
        Some(HeapObj::Set(s)) => s.borrow().len(),
        _ => 0,
    }
}

/* kept apart so the plain range step stays small enough to inline */
#[inline(never)]
fn big_range_step(heap: &mut HeapPool, cur: &mut Val, end: Val, step: Val) -> Result<Option<Val>, VmErr> {
    let order = |a: Val, b: Val| if a.is_int() && b.is_int() { a.as_int().cmp(&b.as_int()) } else { heap.foreign_cmp(a, b).unwrap_or(core::cmp::Ordering::Equal) };
    let ahead = if order(step, Val::int(0)).is_gt() { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater };
    if order(*cur, end) != ahead { return Ok(None); }
    let v = *cur;
    *cur = heap.foreign_binop(super::BinOp::Add, v, step)?.unwrap_or(end);
    Ok(Some(v))
}

/* the cursor of a walk over a live container once it gave its end */
pub(crate) const SPENT: usize = usize::MAX;

/* an ascii character already interned at byte `at` with any other left to the handler */
#[inline(never)]
fn char_ready(heap: &HeapPool, text: &str, at: usize) -> Option<Val> { heap.string_slot(text.get(at..at + 1)?).map(Val::heap) }

/* the character at byte `at` as an interned byte or a text of its own */
#[inline(never)]
fn next_char(heap: &mut HeapPool, text: &str, at: &mut usize) -> Result<Option<Val>, VmErr> {
    let Some(c) = text.get(*at..).and_then(|t| t.chars().next()) else { return Ok(None) };
    let one = &text[*at..*at + c.len_utf8()];
    *at += one.len();
    Ok(Some(heap.alloc(HeapObj::Str(alloc::string::String::from(one)))?))
}

impl IterFrame {
    /* peeks the next item with `None` at the end or where the handler steps */
    #[inline]
    pub(crate) fn ready(&self, heap: &HeapPool) -> Option<Val> {
        match self {
            Self::Seq { items, idx } => items.get(*idx).copied(),
            Self::Watched { items, idx, of, len } if watched_len(heap, *of) == *len => items.get(*idx).copied(),
            Self::List { rc, idx } => rc.borrow().get(*idx).copied(),
            &Self::Range { cur, end, step } if step > 0 && cur < end && (Val::INT_MIN..=Val::INT_MAX).contains(&cur) => Some(Val::int(cur)),
            Self::Octets { bytes, idx } => bytes.get(*idx).map(|&b| Val::int(b as i64)),
            Self::Chars { text, at } => char_ready(heap, text, *at),
            _ => None,
        }
    }

    /* the one step of a plain loop with `None` where the handler steps instead */
    #[inline(always)]
    pub(crate) fn next_ready(&mut self, heap: &HeapPool) -> Option<Val> {
        match self {
            Self::Range { cur, end, step } if *step > 0 && *cur < *end && (Val::INT_MIN..=Val::INT_MAX).contains(cur) => { let v = *cur; *cur = cur.checked_add(*step).unwrap_or(i64::MAX); Some(Val::int(v)) }
            Self::List { rc, idx } => { let v = rc.borrow().get(*idx).copied()?; *idx += 1; Some(v) }
            Self::Seq { items, idx } => { let v = *items.get(*idx)?; *idx += 1; Some(v) }
            Self::Watched { items, idx, of, len } if watched_len(heap, *of) == *len => { let v = *items.get(*idx)?; *idx += 1; Some(v) }
            // keys and values step with no allocation and leave a changed dict to next_item
            Self::DictWalk { of, idx, left, len, part } if *part != DictPart::Items => {
                let m = heap.dict_cell(*of)?.borrow();
                if m.len() != *len { return None; }
                while *idx < m.entry_count() {
                    let k = m.key_at(*idx);
                    if k.is_undef() { *idx += 1; continue; }
                    if *left == 0 { return None; }
                    let v = if *part == DictPart::Keys { k } else { m.value_at(*idx) };
                    (*idx, *left) = (*idx + 1, *left - 1);
                    return Some(v);
                }
                None
            }
            Self::Octets { bytes, idx } => { let b = *bytes.get(*idx)?; *idx += 1; Some(Val::int(b as i64)) }
            Self::Chars { text, at } => { let v = heap.string_slot(text.get(*at..*at + 1)?)?; *at += 1; Some(Val::heap(v)) }
            _ => None,
        }
    }

    /* stateless steps only as frames calling user code or other iterators step in the vm */
    pub fn next_item(&mut self, heap: &mut HeapPool) -> Result<Option<Val>, VmErr> {
        match self {
            Self::Coroutine(_) | Self::UserDefined(_) | Self::Map { .. } | Self::Filter { .. } | Self::Zip { .. } | Self::Enumerate { .. } | Self::Call { .. } | Self::Under(_) | Self::Held(..) => Ok(None),
            Self::Seq { items, idx } => {
                if *idx < items.len() { let v = items[*idx]; *idx += 1; Ok(Some(v)) } else { Ok(None) }
            }
            // the failure sticks so every later step fails the same way
            Self::Watched { items, idx, of, len } => {
                if watched_len(heap, *of) != *len {
                    // a walk that gave its end stays spent whatever its source does
                    if *idx == SPENT { return Ok(None); }
                    *len = usize::MAX;
                    let kind = if matches!(heap.try_get(*of), Some(HeapObj::Set(_))) { crate::lang::KindId::Set } else { crate::lang::KindId::Dict };
                    return Err(crate::lang::EngineError::Changed(kind).into());
                }
                if *idx < items.len() { let v = items[*idx]; *idx += 1; Ok(Some(v)) } else { *idx = SPENT; Ok(None) }
            }
            // both failures stick so every later step fails the same way
            Self::DictWalk { of, idx, left, len, part } => {
                let Some(d) = heap.dict_cell(*of) else { return Ok(None) };
                let m = d.borrow();
                if m.len() != *len {
                    if *idx == SPENT { return Ok(None); }
                    *len = usize::MAX;
                    return Err(crate::lang::EngineError::Changed(crate::lang::KindId::Dict).into());
                }
                while *idx < m.entry_count() {
                    let (k, v) = (m.key_at(*idx), m.value_at(*idx));
                    if k.is_undef() { *idx += 1; continue; }
                    if *left == 0 { return Err(VmErr::Runtime("dictionary keys changed during iteration")); }
                    (*idx, *left) = (*idx + 1, *left - 1);
                    return match part {
                        DictPart::Keys => Ok(Some(k)),
                        DictPart::Values => Ok(Some(v)),
                        DictPart::Items => { drop(m); Ok(Some(heap.alloc(HeapObj::Tuple(alloc::vec![k, v]))?)) }
                    };
                }
                *idx = SPENT;
                Ok(None)
            }
            // items appended after the walk gave its end stay unread
            Self::List { rc, idx } => {
                let items = rc.borrow();
                if *idx < items.len() { let v = items[*idx]; *idx += 1; Ok(Some(v)) } else { *idx = SPENT; Ok(None) }
            }
            // a list that shrank past the cursor ends the walk
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
                    // clamped at the i64 edge so the next `done` check ends the range without overflow
                    *cur = cur.checked_add(*step).unwrap_or(if *step > 0 { i64::MAX } else { i64::MIN });
                    Ok(Some(heap.int(v as i128)?))
                }
            }
            Self::BigRange { cur, end, step } => big_range_step(heap, cur, *end, *step),
            Self::Octets { bytes, idx } => match bytes.get(*idx) {
                Some(&b) => { *idx += 1; Ok(Some(Val::int(b as i64))) }
                None => Ok(None),
            },
            Self::Chars { text, at } => next_char(heap, text, at),
        }
    }

    /* an i64 range holds no `Val` to visit */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        match self {
            IterFrame::Seq { items, .. } => for &v in items.iter() { f(v); },
            IterFrame::Watched { items, of, .. } => { f(*of); for &v in items.iter() { f(v); } }
            IterFrame::DictWalk { of, .. } => f(*of),
            IterFrame::List { rc, .. } | IterFrame::ListRev { rc, .. } => for &v in rc.borrow().iter() { f(v); },
            Self::Coroutine(v) | Self::UserDefined(v) => f(*v),
            IterFrame::Map { f: g, its, .. } => { f(*g); for &v in its.iter() { f(v); } }
            IterFrame::Filter { f: g, it, .. } => { f(*g); f(*it); }
            IterFrame::Zip { its, .. } => for &v in its.iter() { f(v); },
            IterFrame::Enumerate { it, n } => { f(*it); f(*n); }
            IterFrame::Call { f: g, sentinel } => { f(*g); f(*sentinel); }
            IterFrame::Range { .. } | IterFrame::Under(_) | IterFrame::Octets { .. } | IterFrame::Chars { .. } => {}
            IterFrame::Held(_, h) => {
                for u in &h.unwinds { if let Unwind::Return(v) | Unwind::Reraise(_, _, v) = u { f(*v); } }
                for &(v, _) in &h.handling { f(v); }
            }
            IterFrame::BigRange { cur, end, step } => { f(*cur); f(*end); f(*step); }
        }
    }
}

impl ParkedFrame {
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        f(self.func);
        f(self.instance);
        for &v in &self.slots { f(v); }
        for &v in &self.levels { f(v); }
        for fr in &self.iter_delta { fr.for_each_val(f); }
    }
}
