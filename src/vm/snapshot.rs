use alloc::{boxed::Box, rc::Rc};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::hash::Hasher;

use crate::syntax::chunk::{ImportKind, SSAChunk};
use crate::util::hash::{FxHashMap, FxHashSet, FxHasher};
use crate::util::jesc::escape as json_escape;
use super::{Pending, VM};
use crate::value::*;
use alloc::sync::Arc;
use crate::s;
use crate::r#unsafe::pinned;

pub const MAGIC: u32 = 0x4E53_5045;
const FORMAT: u32 = 42;
// where a header keeps the identity of the runtime that saved the blob
pub const RUNTIME_AT: usize = 8;
// the refusal of a blob another runtime saved
pub const OTHER_RUNTIME: &str = "snapshot was saved under another runtime";
// the refusal of a save holding a value of a kind that refuses saves
const REFUSED: &str = "snapshot: cannot save a value of the kind";
// the refusal of a blob whose heap needs more than its vm allows
const NEEDS_MEMORY: &str = "snapshot needs more memory than this VM allows";
// where a header keeps the byte length of the source it embeds
pub const SOURCE_LEN_AT: usize = 24;
// where a header keeps the byte length of the entry's name which follows the source
pub const ENTRY_LEN_AT: usize = SOURCE_LEN_AT + 8;
// where the embedded source starts right past both lengths
pub const SOURCE_AT: usize = ENTRY_LEN_AT + 8;
// a SHA-256 of everything before it closes each blob so a damaged one never restores
const SEAL: usize = 32;

pub type SnapErr = String;

type ExternMap = FxHashMap<String, ExternFn>;
// the cell holding a heap list's items shared with every iterator walking it
type ListCell = Rc<RefCell<Vec<Val>>>;

fn s_err(what: &str, detail: &str) -> SnapErr {
    crate::s!("snapshot: ", str what, " '", str detail, "'")
}

struct W<'h> {
    b: Vec<u8>,
    // the heap a dict walk's cursor is counted in
    heap: &'h HeapPool,
    // the slot of each heap list sorted by its cell for the iterators walking it
    lists: Vec<(usize, Val)>,
    // the runtime's iterator names whose places the blob stores
    iters: &'static [&'static str; crate::lang::ITERS],
    // the runtime's natives whose names the blob stores for the places values hold
    natives: &'static crate::lang::Natives,
}

impl<'h> W<'h> {
    fn new(natives: &'static crate::lang::Natives, heap: &'h HeapPool) -> Self { Self { b: Vec::with_capacity(4096), heap, lists: Vec::new(), iters: &natives.names().iters, natives } }
    fn u8(&mut self, v: u8) { self.b.push(v); }
    fn u16(&mut self, v: u16) { self.b.extend_from_slice(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.b.extend_from_slice(&v.to_le_bytes()); }
    fn u64(&mut self, v: u64) { self.b.extend_from_slice(&v.to_le_bytes()); }
    fn i64(&mut self, v: i64) { self.u64(v as u64); }
    fn i32v(&mut self, v: i32) { self.u32(v as u32); }
    fn usz(&mut self, v: usize) { self.u64(v as u64); }
    fn boolean(&mut self, v: bool) { self.u8(v as u8); }
    fn bytes(&mut self, v: &[u8]) { self.usz(v.len()); self.b.extend_from_slice(v); }
    fn str(&mut self, v: &str) { self.bytes(v.as_bytes()); }
    fn val(&mut self, v: Val) { self.u64(v.0); }
    fn seq<T>(&mut self, items: &[T], f: impl Fn(&mut Self, &T)) {
        self.usz(items.len());
        for x in items { f(self, x); }
    }
    fn vals(&mut self, v: &[Val]) { self.seq(v, |w, x| w.val(*x)); }
    fn opt<T>(&mut self, v: Option<T>, f: impl Fn(&mut Self, T)) {
        match v { Some(x) => { self.u8(1); f(self, x); } None => self.u8(0) }
    }
    fn opt_val(&mut self, v: Option<Val>) { self.opt(v, Self::val); }
    fn opt_u32(&mut self, v: Option<u32>) { self.opt(v, Self::u32); }
    fn opt_usz(&mut self, v: Option<usize>) { self.opt(v, Self::usz); }
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
    // which heap slots hold an object once the heap is decoded
    live: Option<Vec<bool>>,
    // one cell per iterated list swapped in for that list once the heap is back
    lists: FxHashMap<u64, ListCell>,
    // the foreign values the blob restored as the only ones a value may name
    foreign: FxHashSet<u64>,
    // the runtime's iterator names the stored places name
    iters: &'static [&'static str; crate::lang::ITERS],
    // the runtime's natives so restored bindings know which names stand for builtins
    natives: &'static crate::lang::Natives,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self { Self { b, p: 0, live: None, lists: FxHashMap::default(), foreign: FxHashSet::default(), iters: &crate::lang::Names::PLAIN.iters, natives: &crate::lang::Natives::EMPTY } }
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapErr> {
        let end = self.p.checked_add(n).ok_or_else(|| "snapshot truncated".to_string())?;
        if end > self.b.len() { return Err("snapshot truncated".to_string()); }
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, SnapErr> { Ok(self.take(1)?[0]) }
    fn u16(&mut self) -> Result<u16, SnapErr> { Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> Result<u32, SnapErr> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn u64(&mut self) -> Result<u64, SnapErr> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
    fn i64(&mut self) -> Result<i64, SnapErr> { Ok(self.u64()? as i64) }
    fn i32v(&mut self) -> Result<i32, SnapErr> { Ok(self.u32()? as i32) }
    fn usz(&mut self) -> Result<usize, SnapErr> {
        usize::try_from(self.u64()?).map_err(|_| "snapshot value out of range".to_string())
    }
    /* rejects counts beyond the remaining payload */
    fn count(&mut self) -> Result<usize, SnapErr> {
        let n = self.usz()?;
        if n > self.b.len() - self.p { return Err("snapshot truncated".to_string()); }
        Ok(n)
    }
    fn boolean(&mut self) -> Result<bool, SnapErr> { Ok(self.u8()? != 0) }
    fn bytes(&mut self) -> Result<Vec<u8>, SnapErr> {
        let n = self.count()?;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String, SnapErr> {
        String::from_utf8(self.bytes()?).map_err(|_| "snapshot string not utf-8".to_string())
    }
    /* a static-str decode leaks but stored errors are rare */
    fn leakstr(&mut self) -> Result<&'static str, SnapErr> {
        Ok(crate::util::leak(self.str()?.into_boxed_str()))
    }
    /* untrusted values must be well formed and past the heap name a live slot */
    fn val(&mut self) -> Result<Val, SnapErr> {
        let w = self.u64()?;
        self.checked(w)
    }
    /* the value of raw bits `w` checked as `val` checks what it reads */
    fn checked(&self, w: u64) -> Result<Val, SnapErr> {
        let v = Val(w);
        let known = v.is_float() || v.is_int() || v.is_none() || v.is_bool() || v.is_undef();
        if v.is_foreign() && !self.foreign.contains(&v.0) { return Err("snapshot references a missing object".to_string()); }
        if !known && !v.is_foreign() && !(v.is_heap() && Val::heap(v.as_heap()).0 == v.0) {
            return Err("snapshot holds a malformed value".to_string());
        }
        if v.is_heap() && let Some(live) = &self.live && !live.get(v.as_heap() as usize).copied().unwrap_or(false) {
            return Err("snapshot references a missing object".to_string());
        }
        Ok(remade(v))
    }
    fn seq<T>(&mut self, f: impl Fn(&mut Self) -> Result<T, SnapErr>) -> Result<Vec<T>, SnapErr> {
        let n = self.count()?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n { v.push(f(self)?); }
        Ok(v)
    }
    fn vals(&mut self) -> Result<Vec<Val>, SnapErr> { self.seq(Self::val) }
    fn opt<T>(&mut self, f: impl Fn(&mut Self) -> Result<T, SnapErr>) -> Result<Option<T>, SnapErr> {
        Ok(if self.u8()? == 1 { Some(f(self)?) } else { None })
    }
    fn opt_val(&mut self) -> Result<Option<Val>, SnapErr> { self.opt(Self::val) }
    fn opt_u32(&mut self) -> Result<Option<u32>, SnapErr> { self.opt(Self::u32) }
    fn opt_usz(&mut self) -> Result<Option<usize>, SnapErr> { self.opt(Self::usz) }
}

/* one declaration emits both codec sides */
macro_rules! codec {
    (struct $T:ident, $put:ident, $get:ident { $($f:ident: $s:tt),* $(,)? }) => {
        fn $put(w: &mut W, x: &$T) { $( codec!(@put w, (&x.$f), $s); )* }
        fn $get(r: &mut R) -> Result<$T, SnapErr> { Ok($T { $($f: codec!(@get r, $s)),* }) }
    };
    (enum $T:ident, $put:ident, $get:ident {
        $($tag:literal $V:ident $({ $($sf:ident: $ss:tt),* })? $(( $($tf:ident: $ts:tt),* ))?),* $(,)?
    }) => {
        fn $put(w: &mut W, x: &$T) {
            match x {
                $( $T::$V $({ $($sf),* })? $(( $($tf),* ))? => {
                    w.u8($tag);
                    $($( codec!(@put w, ($sf), $ss); )*)?
                    $($( codec!(@put w, ($tf), $ts); )*)?
                } )*
            }
        }
        fn $get(r: &mut R) -> Result<$T, SnapErr> {
            Ok(match r.u8()? {
                $( $tag => $T::$V $({ $($sf: codec!(@get r, $ss)),* })? $(( $(codec!(@get r, $ts)),* ))?, )*
                t => return Err(s_err("unknown tag", itoa::Buffer::new().format(t))),
            })
        }
    };
    (@put $w:ident, ($x:expr), str) => { $w.str($x) };
    (@put $w:ident, ($x:expr), vals) => { $w.vals($x) };
    (@put $w:ident, ($x:expr), leakstr) => { $w.str($x) };
    (@put $w:ident, ($x:expr), default) => { let _ = $x; };
    (@put $w:ident, ($x:expr), [str]) => { $w.seq($x, |w, v| w.str(v)) };
    (@put $w:ident, ($x:expr), [$m:ident]) => { $w.seq($x, |w, v| w.$m(*v)) };
    (@put $w:ident, ($x:expr), [$p:ident, $g:ident]) => { $w.seq($x, $p) };
    (@put $w:ident, ($x:expr), ($p:ident, $g:ident)) => { $p($w, $x) };
    (@put $w:ident, ($x:expr), $m:ident) => { $w.$m(*$x) };
    (@get $r:ident, str) => { $r.str()? };
    (@get $r:ident, vals) => { $r.vals()? };
    (@get $r:ident, leakstr) => { $r.leakstr()? };
    (@get $r:ident, default) => { Default::default() };
    (@get $r:ident, [str]) => { $r.seq(|r| r.str())? };
    (@get $r:ident, [$m:ident]) => { $r.seq(|r| r.$m())? };
    (@get $r:ident, [$p:ident, $g:ident]) => { $r.seq($g)? };
    (@get $r:ident, ($p:ident, $g:ident)) => { $g($r)? };
    (@get $r:ident, $m:ident) => { $r.$m()? };
}

// only the events still to take written as the queue was before its cursor
fn put_events(w: &mut W, q: &super::init::Events) { w.vals(q.unread()); }
fn get_events(r: &mut R) -> Result<super::init::Events, SnapErr> { Ok(super::init::Events::of(r.vals()?)) }
fn put_slot_val(w: &mut W, p: &(usize, Val)) { w.usz(p.0); w.val(p.1); }
fn get_slot_val(r: &mut R) -> Result<(usize, Val), SnapErr> { Ok((r.usz()?, r.val()?)) }
fn put_name_val(w: &mut W, p: &(String, Val)) { w.str(&p.0); w.val(p.1); }
fn get_name_val(r: &mut R) -> Result<(String, Val), SnapErr> { Ok((r.str()?, r.val()?)) }
fn put_handled(w: &mut W, h: &(Val, Option<u32>)) { w.val(h.0); w.opt_u32(h.1); }
fn get_handled(r: &mut R) -> Result<(Val, Option<u32>), SnapErr> { Ok((r.val()?, r.opt_u32()?)) }
fn put_i32_pair(w: &mut W, p: &(i32, i32)) { w.i32v(p.0); w.i32v(p.1); }
// a spread call's packed operands never cross a pause so their two words stay zero
fn put_spread(w: &mut W, _: &(u8, u32)) { w.i32v(0); w.i32v(0); }
fn get_spread(r: &mut R) -> Result<(u8, u32), SnapErr> { r.i32v()?; r.i32v()?; Ok((0, 0)) }
fn get_i32_pair(r: &mut R) -> Result<(i32, i32), SnapErr> { Ok((r.i32v()?, r.i32v()?)) }

codec!(enum BlockKind, put_block_kind, get_block_kind { 0 Except, 1 Finally });

codec!(enum BodyRef, put_body_ref, get_body_ref { 0 Fn(fi: usz), 1 Module });

/* list frames name lists by slot to read live and unheld lists keep their items */
fn put_iter_frame(w: &mut W, x: &IterFrame) {
    let walks = |w: &W, rc: &ListCell| { let at = Rc::as_ptr(rc) as usize; w.lists.binary_search_by_key(&at, |e| e.0).ok().map(|i| w.lists[i].1) };
    match x {
        IterFrame::Seq { items, idx } => { w.u8(0); w.vals(items); w.usz(*idx); }
        IterFrame::List { rc, idx } => match walks(w, rc) {
            Some(of) => { w.u8(11); w.val(of); w.usz(*idx); }
            None => { w.u8(0); w.vals(&rc.borrow()); w.usz(*idx); }
        },
        IterFrame::Range { cur, end, step } => { w.u8(1); w.i64(*cur); w.i64(*end); w.i64(*step); }
        IterFrame::BigRange { cur, end, step } => { w.u8(13); w.val(*cur); w.val(*end); w.val(*step); }
        IterFrame::Coroutine(v) => { w.u8(2); w.val(*v); }
        IterFrame::UserDefined(v) => { w.u8(3); w.val(*v); }
        IterFrame::ListRev { rc, idx } => match walks(w, rc) {
            Some(of) => { w.u8(12); w.val(of); w.usz(*idx); }
            None => { w.u8(0); w.vals(&rc.borrow()[..(*idx).min(rc.borrow().len())].iter().rev().copied().collect::<Vec<_>>()); w.usz(0); }
        },
        IterFrame::Map { f, its, own } => { w.u8(4); w.val(*f); w.vals(its); w.boolean(*own); }
        IterFrame::Filter { f, it, own } => { w.u8(5); w.val(*f); w.val(*it); w.boolean(*own); }
        IterFrame::Zip { its, strict } => { w.u8(6); w.vals(its); w.u8(*strict as u8); }
        IterFrame::Enumerate { it, n } => { w.u8(7); w.val(*it); w.val(*n); }
        IterFrame::Call { f, sentinel } => { w.u8(8); w.val(*f); w.val(*sentinel); }
        IterFrame::Watched { items, idx, of, len } => { w.u8(9); w.vals(items); w.usz(*idx); w.val(*of); w.usz(*len); }
        IterFrame::Octets { bytes, idx } => { w.u8(18); w.bytes(bytes); w.usz(*idx); }
        IterFrame::Chars { text, at } => { w.u8(19); w.str(text); w.usz(*at); }
        IterFrame::DictWalk { of, idx, left, len, part } => {
            // restores keep only live entries so the cursor travels as the live count before it
            let live = |m: &DictMap| (0..(*idx).min(m.entry_count())).filter(|&k| !m.key_at(k).is_undef()).count();
            let at = if *idx == SPENT { *idx } else { w.heap.dict_cell(*of).map_or(*idx, |d| live(&d.borrow())) };
            w.u8(10); w.val(*of); w.usz(at); w.usz(*left); w.usz(*len); w.u8(*part as u8);
        }
        IterFrame::Under(n) => { w.u8(16); w.usz(*n as usize); }
        IterFrame::Held(n, h) => {
            w.u8(17); w.usz(*n as usize); w.seq(&h.excs, put_exc_frame); w.seq(&h.unwinds, put_unwind); w.seq(&h.handling, put_handled);
        }
    }
}
fn get_iter_frame(r: &mut R) -> Result<IterFrame, SnapErr> {
    Ok(match r.u8()? {
        0 => IterFrame::Seq { items: r.vals()?.into(), idx: r.usz()? },
        1 => IterFrame::Range { cur: r.i64()?, end: r.i64()?, step: r.i64()? },
        13 => IterFrame::BigRange { cur: r.val()?, end: r.val()?, step: r.val()? },
        16 => IterFrame::Under(r.usz()? as u32),
        17 => {
            let n = r.usz()? as u32;
            IterFrame::Held(n, alloc::boxed::Box::new(crate::value::Held { excs: r.seq(get_exc_frame)?, unwinds: r.seq(get_unwind)?, handling: r.seq(get_handled)? }))
        }
        2 => IterFrame::Coroutine(r.val()?),
        3 => IterFrame::UserDefined(r.val()?),
        4 => IterFrame::Map { f: r.val()?, its: r.vals()?.into(), own: r.boolean()? },
        5 => IterFrame::Filter { f: r.val()?, it: r.val()?, own: r.boolean()? },
        6 => IterFrame::Zip { its: r.vals()?.into(), strict: r.u8()? != 0 },
        7 => IterFrame::Enumerate { it: r.val()?, n: r.val()? },
        8 => IterFrame::Call { f: r.val()?, sentinel: r.val()? },
        9 => IterFrame::Watched { items: r.vals()?.into(), idx: r.usz()?, of: r.val()?, len: r.usz()? },
        10 => IterFrame::DictWalk { of: r.val()?, idx: r.usz()?, left: r.usz()?, len: r.usz()?, part: *[DictPart::Keys, DictPart::Values, DictPart::Items].get(r.u8()? as usize).ok_or("snapshot names an unknown dict part")? },
        11 => { let (rc, idx) = list_cell(r)?; IterFrame::List { rc, idx } }
        12 => { let (rc, idx) = list_cell(r)?; IterFrame::ListRev { rc, idx } }
        t @ (18 | 19) => walk(t, r.bytes()?, r.usz()?)?,
        t => return Err(s_err("unknown tag", itoa::Buffer::new().format(t))),
    })
}

/* an octet walk or a character walk whose cursor must sit on a character boundary */
fn walk(tag: u8, bytes: Vec<u8>, at: usize) -> Result<IterFrame, SnapErr> {
    if tag == 18 { return Ok(IterFrame::Octets { bytes: bytes.into(), idx: at }); }
    match String::from_utf8(bytes) {
        Ok(text) if text.is_char_boundary(at) => Ok(IterFrame::Chars { text: text.into(), at }),
        _ => Err("snapshot walks a text off a character".to_string()),
    }
}

/* iterators over one list share this cell filled from it once the heap is back */
fn list_cell(r: &mut R) -> Result<(ListCell, usize), SnapErr> {
    let (of, idx) = (r.val()?, r.usz()?);
    Ok((r.lists.entry(of.0).or_insert_with(|| Rc::new(RefCell::new(Vec::new()))).clone(), idx))
}

codec!(struct ExceptionFrame, put_exc_frame, get_exc_frame {
    kind: (put_block_kind, get_block_kind),
    handler_ip: usz,
    levels: usz,
    iter_depth: usz,
    unwind_depth: usz,
    handling_depth: usz,
    turned: usz,
    slot: u16,
});

codec!(struct ParkedFrame, put_parked_frame, get_parked_frame {
    ip: usz,
    fi: usz,
    func: val,
    slots: vals,
    levels: vals,
    iter_delta: [put_iter_frame, get_iter_frame],
    exception_delta: [put_exc_frame, get_exc_frame],
    instance: val,
    layout: default,
    top: u16,
});

codec!(enum SchedulerStatus, put_sched, get_sched {
    3 PendingEvent,
    4 PendingHostCall,
    5 Preempted,
});

codec!(enum VmErr, put_vm_err, get_vm_err {
    0 CallDepth,
    1 Heap,
    2 Budget,
    3 ZeroDiv,
    4 Overflow,
    5 Name(s: str),
    6 Type(s: leakstr),
    7 TypeMsg(s: str),
    8 Value(s: leakstr),
    9 Runtime(s: leakstr),
    10 Attribute(s: str),
    11 Raised(s: str),
    12 HostYield(st: (put_sched, get_sched)),
    13 HostCallDeferred,
    14 Redo,
    15 Of(k: (put_err_kind, get_err_kind), s: str),
    16 Engine(e: (put_engine, get_engine)),
});

/* an unworded engine error kept as its kind and bare words outliving its values */
fn put_engine(w: &mut W, e: &Cold<crate::lang::EngineError>) {
    w.u8(e.kind().unwrap_or(crate::lang::ErrKind::Runtime) as u8);
    w.str(&crate::lang::bare(e));
}

fn get_engine(r: &mut R) -> Result<Cold<crate::lang::EngineError>, SnapErr> {
    let k = get_err_kind(r)?;
    Ok(Cold::new(crate::lang::EngineError::Of(k, r.str()?)))
}

fn put_err_kind(w: &mut W, k: &crate::lang::ErrKind) { w.u8(*k as u8); }

fn get_err_kind(r: &mut R) -> Result<crate::lang::ErrKind, SnapErr> {
    crate::lang::ErrKind::ALL.get(r.u8()? as usize).copied().ok_or_else(|| "snapshot names an unknown error kind".to_string())
}

codec!(enum Unwind, put_unwind, get_unwind {
    0 Normal,
    1 Return(v: val),
    2 Goto { target: usz, remaining: u16 },
    3 Reraise(e: (put_vm_err, get_vm_err), at: opt_u32, exc: val),
});

codec!(enum CoroState, put_coro_state, get_coro_state {
    0 Ready,
    3 WaitingEvent,
    4 WaitingHostCall(id: u64),
    10 Raising(e: (put_vm_err, get_vm_err), exc: opt_val),
});

codec!(struct CoroutineHandle, put_handle, get_handle {
    coro: val,
    state: (put_coro_state, get_coro_state),
});

fn put_root(w: &mut W, v: &Option<CoroutineHandle>) { w.opt(v.as_ref(), put_handle); }
fn get_root(r: &mut R) -> Result<Option<CoroutineHandle>, SnapErr> { r.opt(get_handle) }

codec!(struct Pending, put_pending, get_pending {
    spread: (put_spread, get_spread),
    delta_save: [put_i32_pair, get_i32_pair],
    call_ip: opt_u32,
    event_wait_request: boolean,
    host_call_request: boolean,
    host_call_id: u64,
    exc_val: opt_val,
    preempt_request: default,
});

/* hashes are taken again on restore so only the entries travel */
fn put_dict(w: &mut W, d: &DictMap) {
    let live: Vec<(Val, Val)> = d.iter().collect();
    w.seq(&live, |w, &(k, v)| { w.val(k); w.val(v); });
}

fn get_dict(r: &mut R) -> Result<DictMap, SnapErr> {
    Ok(DictMap::from_unhashed(r.seq(|r| Ok((r.val()?, r.val()?)))?))
}

fn put_set(w: &mut W, s: &ValSet) {
    let items: Vec<Val> = s.iter().copied().collect();
    w.seq(&items, |w, &v| w.val(v));
}

fn get_set_items(r: &mut R) -> Result<Vec<Val>, SnapErr> { r.seq(|r| r.val()) }

/* sorted so identical states produce identical blobs */
fn put_map(w: &mut W, m: &FxHashMap<String, Val>) {
    let mut pairs: Vec<(&String, &Val)> = m.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    w.usz(pairs.len());
    for (k, v) in pairs { w.str(k); w.val(*v); }
}

/* a module's bindings in index order keeping unbound ones so indices hold */
fn put_globals(w: &mut W, g: &super::scope::Globals) {
    let entries: Vec<(&str, Val)> = g.entries().collect();
    w.seq(&entries, |w, &(name, v)| { w.str(name); w.val(v); });
}

fn get_globals(r: &mut R) -> Result<super::scope::Globals, SnapErr> {
    let natives = r.natives;
    Ok(super::scope::Globals::from_entries(r.seq(|r| Ok((r.str()?, r.val()?)))?, natives))
}

fn get_map(r: &mut R) -> Result<FxHashMap<String, Val>, SnapErr> {
    let n = r.count()?;
    let mut m = FxHashMap::default();
    for _ in 0..n {
        let k = r.str()?;
        m.insert(k, r.val()?);
    }
    Ok(m)
}

/* a set slot and its items inserted in the rehash pass */
type SetFill = (u32, Vec<Val>);

fn put_obj(w: &mut W, obj: &HeapObj, heap: &HeapPool, idx: usize) {
    match obj {
        HeapObj::Str(s) => { w.u8(0); w.str(s); }
        HeapObj::Bytes(b) => { w.u8(1); w.bytes(b); }
        HeapObj::List(rc) => { w.u8(2); w.vals(&rc.borrow()); }
        HeapObj::Dict(rc) => { w.u8(3); put_dict(w, &rc.borrow()); }
        HeapObj::Set(rc) => { w.u8(4); put_set(w, &rc.borrow()); }
        HeapObj::Tuple(v) => { w.u8(6); w.vals(v); }
        HeapObj::Func { fi, defaults, cells } => {
            w.u8(7); w.usz(*fi); w.vals(defaults); w.seq(cells, put_slot_val);
            // the fields a program gave the function ride after its cells
            let fields = heap.fn_fields_at(idx).unwrap_or(&[]);
            w.usz(fields.len());
            for (n, v) in fields { w.str(n); w.val(*v); }
        }
        HeapObj::Range(s, e, st) => { w.u8(8); w.i64(*s); w.i64(*e); w.i64(*st); }
        HeapObj::BigRange(s, e, st) => { w.u8(13); w.val(*s); w.val(*e); w.val(*st); }
        &HeapObj::Native(id) => match w.natives.native(id) {
            Some(n) => { w.u8(52); put_native(w, n); }
            None => { w.u8(26); w.str(heap.host_at(id).map_or("", |f| f.name.as_str())); }
        },
        HeapObj::Object { meta, slots } => {
            // a class keeps its name and bases a type its name an instance its meta
            w.u8(18);
            match meta.head().and_then(|at| heap.heads().get(at)) {
                Some(h) if meta.is_class_meta() => { w.u8(1); w.str(&h.name); w.vals(&h.bases); w.usz(h.members.len()); for (n, v) in &h.members { w.str(n); w.val(*v); } }
                Some(h) => { w.u8(2); w.str(&h.name); }
                None => { w.u8(0); w.val(*meta); }
            }
            put_dict(w, &slots.borrow());
            // an instance's making arguments ride after its fields
            if meta.is_heap() { match heap.args_tuple(Val::heap(idx as u32)) { Some(t) => { w.u8(1); w.val(t); } None => w.u8(0) } }
        }
        &HeapObj::Bound { recv, func } => {
            w.u8(19); w.val(recv);
            // a native of a kind rides by its name and a function by its slot
            match func.as_native_ref().and_then(|id| w.natives.native(id)) { Some(n) => { w.u8(1); put_native(w, n); } None => { w.u8(0); w.val(func); } }
        }
        HeapObj::Coroutine(c) => {
            // one holding an item a builtin's step took ahead takes a tag that adds it
            w.u8(if !c.pending.is_undef() { 51 } else { 24 }); w.usz(c.ip); w.vals(&c.slots); w.vals(&c.levels); w.u16(if c.ip == super::coro::FINISHED { 0 } else { c.top }); put_body_ref(w, &c.body);
            w.seq(&c.iters, put_iter_frame); w.seq(&c.syncs, put_parked_frame); w.seq(&c.excs, put_exc_frame);
            if !c.pending.is_undef() { w.val(c.pending); }
        }
        HeapObj::Module(spec, attrs) => { w.u8(25); w.str(spec); w.seq(attrs, put_name_val); }
        HeapObj::Iter(frame, name) => {
            w.u8(33);
            let at = w.iters.iter().position(|k| k == name).unwrap_or(0);
            w.u8(at as u8);
            put_iter_frame(w, &frame.borrow());
        }
        HeapObj::Cell(v) => { w.u8(34); w.val(*v); }
        HeapObj::ByteArray(b) => { w.u8(38); w.bytes(b); }
    }
}

/* native `n` by its owner and name with a foreign owner's kind after them */
fn put_native(w: &mut W, n: &crate::lang::Native) {
    w.u8(n.owner.map_or(u8::MAX, |k| k as u8)); w.str(n.name);
    // a foreign kind's method names that kind since its run holds the name
    if n.owner == Some(crate::lang::KindId::Foreign) { w.str(w.natives.foreign_owner(n.id).unwrap_or("")); }
}

/* the native `put_native` wrote found in this runtime's table */
fn get_native(r: &mut R, natives: &'static crate::lang::Natives) -> Result<crate::lang::NativeId, SnapErr> {
    let owner = match r.u8()? { u8::MAX => None, k => Some(crate::lang::KindId::from_u8(k).ok_or("snapshot names an unknown kind")?) };
    let name = r.str()?;
    let found = match owner {
        Some(crate::lang::KindId::Foreign) => { let kind = r.str()?; natives.foreign_method(&kind, &name).or_else(|| natives.method(crate::lang::KindId::Foreign, &name)) }
        Some(k) => natives.method(k, &name).or_else(|| crate::vm::slow::attr::own(natives, k, &name)),
        None => natives.find(&name),
    };
    Ok(found.ok_or_else(|| s_err("names a native this runtime does not install", &name))?.id)
}

#[allow(clippy::too_many_arguments)]
fn get_obj(r: &mut R, externs: &ExternMap, hosts: &mut Vec<crate::value::ExternFn>, natives: &'static crate::lang::Natives, fills: &mut Vec<SetFill>, heads: &mut Vec<Head>, held: &mut Vec<(u32, Val)>, fn_fields: &mut crate::value::object::FnFields, slot: u32) -> Result<HeapObj, SnapErr> {
    let tag = r.u8()?;
    Ok(match tag {
        0 => HeapObj::Str(r.str()?),
        1 => HeapObj::Bytes(r.bytes()?),
        2 => HeapObj::List(Rc::new(RefCell::new(r.vals()?))),
        3 => HeapObj::Dict(Rc::new(RefCell::new(get_dict(r)?))),
        4 => {
            fills.push((slot, get_set_items(r)?));
            HeapObj::Set(Rc::new(RefCell::new(ValSet::default())))
        }
        6 => HeapObj::Tuple(r.vals()?),
        7 => {
            let (fi, defaults, cells) = (r.usz()?, r.vals()?, r.seq(get_slot_val)?);
            let fields = r.seq(|r| Ok((r.str()?.into_boxed_str(), r.val()?)))?;
            if !fields.is_empty() { fn_fields.push((slot, fields)); }
            HeapObj::Func { fi, defaults: defaults.into(), cells: cells.into() }
        }
        8 => HeapObj::Range(r.i64()?, r.i64()?, r.i64()?),
        13 => HeapObj::BigRange(r.val()?, r.val()?, r.val()?),
        52 => HeapObj::Native(get_native(r, natives)?),
        18 => {
            let meta = match r.u8()? {
                0 => r.val()?,
                1 => {
                    let (name, bases) = (r.str()?, r.vals()?);
                    let members = r.seq(|r| Ok((r.str()?.into_boxed_str(), r.val()?)))?;
                    crate::value::object::restored_meta(heads, true, Head { name: crate::value::object::Text::Owned(name.into()), bases: bases.into(), members })
                }
                2 => crate::value::object::restored_meta(heads, false, Head { name: crate::value::object::Text::Owned(r.str()?.into()), ..Head::default() }),
                _ => return Err("snapshot holds a malformed value".to_string()),
            };
            let slots = Rc::new(RefCell::new(get_dict(r)?));
            if meta.is_heap() {
                match r.u8()? {
                    0 => {}
                    1 => held.push((slot, r.val()?)),
                    _ => return Err("snapshot holds a malformed value".to_string()),
                }
            }
            HeapObj::Object { meta, slots }
        }
        19 => {
            let recv = r.val()?;
            let func = match r.u8()? { 0 => r.val()?, 1 => Val::native_ref(get_native(r, natives)?), _ => return Err("snapshot holds a malformed value".to_string()) };
            HeapObj::Bound { recv, func }
        }
        24 | 51 => HeapObj::Coroutine(Box::new(Coro {
            ip: r.usz()?, slots: r.vals()?, levels: r.vals()?, top: r.u16()?, body: get_body_ref(r)?,
            iters: r.seq(get_iter_frame)?, syncs: r.seq(get_parked_frame)?, excs: r.seq(get_exc_frame)?,
            running: false, layout: 0,
            pending: if tag == 51 { r.val()? } else { Val::undef() },
        })),
        25 => HeapObj::Module(r.str()?, r.seq(get_name_val)?),
        26 => {
            let name = r.str()?;
            HeapObj::Native(crate::value::callable::host_place(hosts, externs.get(&name).ok_or_else(|| s_err("unknown native binding", &name))?))
        }
        33 => {
            let iters = r.iters;
            let name = *iters.get(r.u8()? as usize).ok_or("snapshot names an unknown iterator")?;
            HeapObj::Iter(Rc::new(RefCell::new(get_iter_frame(r)?)), name)
        }
        34 => HeapObj::Cell(r.val()?),
        38 => HeapObj::ByteArray(r.bytes()?),
        t => return Err(s_err("unknown heap tag", itoa::Buffer::new().format(t))),
    })
}

/* a structural fingerprint pins the blob to its bytecode and the engine's opcode table */
pub fn fingerprint(chunk: &SSAChunk) -> u64 {
    let mut h = FxHasher::default();
    h.write_u64(crate::code::OpCode::HASH);
    fp_chunk(chunk, &mut h);
    h.finish()
}

fn fp_chunk(chunk: &SSAChunk, h: &mut FxHasher) {
    h.write_u64(chunk.instructions.len() as u64);
    for ins in &chunk.instructions {
        h.write_u64(((ins.opcode as u64) << 16) | ins.operand as u64);
    }
    h.write_u64(chunk.constants.len() as u64);
    h.write_u64(chunk.names.len() as u64);
    h.write_u64(chunk.extern_table.len() as u64);
    h.write_u64(chunk.native_sites.len() as u64);
    for s in &chunk.native_sites { h.write_u64((s.id as u64) << 32 | (s.pos as u64) << 16 | (s.kw as u64) << 8 | (s.void as u64) << 3 | (s.fast as u64) << 2 | (s.spread as u64) << 1 | s.pinned as u64); }
    h.write_u64(chunk.functions.len() as u64);
    for (params, body, defaults, name_slot) in &chunk.functions {
        h.write_u64(params.len() as u64);
        h.write_u64(((*defaults as u64) << 16) | *name_slot as u64);
        fp_chunk(body, h);
    }
    h.write_u64(chunk.classes.len() as u64);
    for body in &chunk.classes { fp_chunk(body, h); }
    h.write_u64(chunk.imports.len() as u64);
    for entry in &chunk.imports {
        h.write_u64(entry.spec.len() as u64);
        if let ImportKind::Code(sub) = &entry.kind { fp_chunk(sub, h); }
    }
}

/* a single field list drives save and restore */
macro_rules! vm_state {
    ($($f:ident: $s:tt),* $(,)?) => {
        fn put_vm_state(w: &mut W, vm: &VM) { $( codec!(@put w, (&vm.$f), $s); )* }
        fn get_vm_state(r: &mut R, vm: &mut VM) -> Result<(), SnapErr> {
            $( vm.$f = codec!(@get r, $s); )*
            Ok(())
        }
    };
}

vm_state! {
    levels: vals,
    iter_stack: [put_iter_frame, get_iter_frame],
    temp_roots: vals,
    event_queue: (put_events, get_events),
    scopes: [put_globals, get_globals],
    module_table: (put_map, get_map),
    is_async: [boolean], // filled by `MakeCoroutine` and not chunk-derivable
    laid_fns: [boolean],
    laid_mods: [str],
    learned: vals,
    grown: vals,
    exception_stack: [put_exc_frame, get_exc_frame],
    turned: usz,
    unwind_stack: [put_unwind, get_unwind],
    handling: [put_handled, get_handled],
    pending_sync_frames: [put_parked_frame, get_parked_frame],
    pending_exec_exc_base: opt_usz,
    pending: (put_pending, get_pending),
    root: (put_root, get_root),
    next_host_call_id: u64,
    yielded: boolean,
    yield_from_value: val,
    resume_ip: usz,
    error_byte_pos: opt_u32,
    output: [str],
    output_open: boolean,
}

/* a run-time chunk keeps its fingerprint and its text which compiles again on restore */
fn put_compiled(w: &mut W, c: &crate::vm::snapshot::Compiled) {
    w.str(&c.source);
    w.str(c.spec.as_deref().unwrap_or(""));
    w.str(&c.chunk().path);
    w.u64(fingerprint(c.chunk()));
}

fn put_call_frame(w: &mut W, f: &CallFrame) {
    w.usz(f.fi);
    w.u32(f.call_byte_pos);
    w.str(f.caller_path.as_deref().map_or("", |p| p.as_str()));
    w.u32(f.repeats);
}

/* `save` again after a sweep when a kind refusing saves blocked it */
pub fn save_swept(vm: &mut VM, source: &str, entry: &str) -> Result<Vec<u8>, SnapErr> {
    save(vm, source, entry).or_else(|e| if e.starts_with(REFUSED) { vm.sweep(); save(vm, source, entry) } else { Err(e) })
}

pub fn save(vm: &VM, source: &str, entry: &str) -> Result<Vec<u8>, SnapErr> {
    // a pause saves every frame in its coroutine so no register needs keeping
    debug_assert!(vm.pools.iter().all(|p| p.active == 0), "a snapshot is taken only at a pause");
    let mut w = W::new(vm.natives, &vm.heap);
    w.u32(MAGIC);
    w.u32(FORMAT);
    debug_assert_eq!(w.b.len(), RUNTIME_AT);
    w.u64(vm.rt.identity);
    w.u64(fingerprint(vm.chunk));
    debug_assert_eq!(w.b.len(), SOURCE_LEN_AT);
    // the entry names the frame a traceback ends in and roots the relative imports
    w.usz(source.len());
    w.usz(entry.len());
    w.b.extend_from_slice(source.as_bytes());
    w.b.extend_from_slice(entry.as_bytes());
    debug_assert_eq!(w.b.len(), SOURCE_AT + source.len() + entry.len());
    w.u64(vm.budget);
    w.usz(vm.heap.limit());
    w.boolean(vm.strict_input);
    w.seq(&vm.compiled, put_compiled);
    // the module of each spec in creation order since compiled code may have made some
    let mut specs: Vec<(&String, &usize)> = vm.scope_ids.iter().collect();
    specs.sort_unstable_by_key(|&(_, &i)| i);
    w.seq(&specs, |w, &(spec, &i)| { w.str(spec); w.usz(i); });
    put_foreign(&mut w, vm)?;
    for (slot, obj) in vm.heap.snapshot_objs().enumerate() {
        if let Some(HeapObj::List(rc)) = obj { w.lists.push((Rc::as_ptr(rc) as usize, Val::heap(slot as u32))); }
    }
    // sorted so a search compares cells and its cost never follows where they lie
    w.lists.sort_unstable_by_key(|e| e.0);
    w.usz(vm.heap.snapshot_objs().count());
    for (idx, obj) in vm.heap.snapshot_objs().enumerate() {
        match obj {
            None => w.u8(0),
            Some(o) => { w.u8(1); put_obj(&mut w, o, &vm.heap, idx); }
        }
    }
    // free slots keep the order later allocations take them in so addresses stay alike
    w.seq(vm.heap.free_order(), |w, &i| w.u32(i));
    for n in vm.heap.schedule() { w.usz(n); }
    // the qualified names nested classes were made with which their repr reads
    let mut quals: Vec<_> = vm.class_qualnames.iter().filter(|&(&id, _)| vm.heap.is_class(Val(id))).collect();
    quals.sort_unstable_by_key(|(id, _)| **id);
    w.seq(&quals, |w, &(&id, q)| { w.val(Val(id)); w.str(q); });
    put_vm_state(&mut w, vm);
    // the unread lines so a restore reads on from the same line
    w.seq(vm.input_buffer.get(vm.input_at..).unwrap_or_default(), |w, v| w.str(v));
    w.seq(&vm.call_stack, put_call_frame);
    put_tiers(&mut w, vm);
    put_prints(&mut w, vm);
    let seal = crate::util::sha256::sha256(&w.b);
    w.b.extend_from_slice(&seal);
    Ok(w.b)
}

/* each body's heat and calls so restored runs lower bodies when saved ones would */
fn put_tiers(w: &mut W, vm: &VM) {
    let mut chunks: Vec<&SSAChunk> = Vec::new();
    source_index(vm.chunk, &mut chunks);
    for c in vm.compiled.iter() { source_index(c.chunk(), &mut chunks); }
    let pools: Vec<Option<usize>> = vm.fn_pool.iter().map(|&p| Some(p)).chain(chunks.iter().map(|c| vm.pool_at(c))).collect();
    w.seq(&pools, |w, p| {
        let (heat, calls, ran) = p.map_or((0, 0, false), |p| (vm.pools[p].heat, vm.pools[p].calls, vm.pools[p].ran));
        w.u32(heat); w.u32(calls); w.boolean(ran);
    });
}

/* hashes the laid code runs parked in so restores resume them only in that code */
fn put_prints(w: &mut W, vm: &VM) {
    let code_at = |fi: Option<usize>, layout: u32| -> u64 {
        let pool = match fi { Some(fi) => Some(vm.fn_pool[fi]), None => vm.pool_at(vm.chunk) };
        let code = pool.and_then(|p| vm.pools[p].code.iter().chain(&vm.retired).find(|c| c.layout == layout));
        code.map_or(0, |c| super::vet::code_print(c))
    };
    let mut prints: Vec<(i64, u64)> = Vec::new();
    for obj in vm.heap.snapshot_objs().flatten() {
        let HeapObj::Coroutine(c) = obj else { continue };
        let fi = match c.body { BodyRef::Fn(fi) => Some(fi), BodyRef::Module => None };
        let chunk = fi.map_or(vm.chunk, |fi| &vm.functions[fi].1);
        if c.ip != super::coro::FINISHED && c.ip & super::frame::RESUME_RAW != 0 { prints.push((vm.code_key(chunk, fi), code_at(fi, c.layout))); }
        for f in &c.syncs { if f.ip & super::frame::RESUME_RAW != 0 { prints.push((vm.code_key(&vm.functions[f.fi].1, Some(f.fi)), code_at(Some(f.fi), f.layout))); } }
    }
    prints.sort_unstable();
    prints.dedup();
    w.seq(&prints, |w, &(k, p)| { w.i64(k); w.u64(p); });
}

/* puts back what `put_tiers` saved for the bodies the restored program holds */
fn get_tiers(r: &mut R, vm: &mut VM) -> Result<(), SnapErr> {
    let mut chunks: Vec<&SSAChunk> = Vec::new();
    source_index(vm.chunk, &mut chunks);
    let compiled: Vec<alloc::rc::Rc<SSAChunk>> = vm.compiled.iter().map(|c| c.chunk.clone()).collect();
    for c in &compiled { source_index(c, &mut chunks); }
    let mut pools: Vec<usize> = vm.fn_pool.clone();
    for c in chunks { pools.push(vm.pool_of(c)); }
    let tiers = r.seq(|r| Ok((r.u32()?, r.u32()?, r.boolean()?)))?;
    if tiers.len() != pools.len() { return Err("snapshot does not match this program or compiler version".to_string()); }
    for (p, (heat, calls, ran)) in pools.into_iter().zip(tiers) {
        let pool = &mut vm.pools[p];
        (pool.heat, pool.calls, pool.ran) = (heat.min(super::lower::HOT_LOOP), calls.min(super::lower::TIER), ran);
    }
    Ok(())
}

/* each live foreign entry by kind version and place with its store or encoded payload */
fn put_foreign(w: &mut W, vm: &VM) -> Result<(), SnapErr> {
    use crate::value::foreign::{Saved, Snapshot};
    w.usz(vm.heap.foreign.len());
    let entries: Vec<_> = vm.heap.foreign.live_entries().collect();
    w.usz(entries.len());
    for (kind, generation, index, saved) in entries {
        if matches!(kind.snapshot, Snapshot::Reject) { return Err(s_err(&REFUSED["snapshot: ".len()..], kind.name)); }
        w.str(kind.name); w.u32(kind.version); w.u16(generation); w.u32(index);
        match saved {
            Saved::Words(words) => { w.u8(0); for &x in words { w.u64(x); } }
            Saved::Vals(vals) => { w.u8(1); for v in vals { w.val(v); } }
            Saved::Shared(payload) => {
                let Snapshot::Serialize { save, .. } = kind.snapshot else { return Err(s_err("cannot save a value of the kind", kind.name)) };
                let mut body = Vec::new();
                save(payload, &mut body);
                w.u8(2); w.bytes(&body);
            }
        }
    }
    Ok(())
}

/* a blob's value with a float remade so any NaN is the engine's own */
fn remade(v: Val) -> Val { if v.is_float() { Val::float(v.as_float()) } else { v } }

/* restores foreign entries each under a kind this VM registered at the same version */
fn get_foreign(r: &mut R, vm: &mut VM) -> Result<(), SnapErr> {
    use crate::value::foreign::{Snapshot, Store};
    // the table's length counts its free slots so the bytes left bound nothing of it
    let len = r.usz()?;
    if !crate::value::foreign::ForeignTable::holds(len, vm.heap.limit()) { return Err("snapshot holds a malformed value".to_string()); }
    vm.heap.foreign.clear();
    vm.heap.dots = None;
    let n = r.count()?;
    // values a store holds are checked once every entry and slot is back
    let mut held: Vec<u64> = Vec::new();
    for _ in 0..n {
        let (name, version, generation, index) = (r.str()?, r.u32()?, r.u16()?, r.u32()?);
        let kind = vm.heap.foreign.kind_named(&name, vm.rt.kinds, &vm.natives.built).ok_or_else(|| s_err("unknown kind", &name))?;
        let k = vm.heap.foreign.kind_at(kind);
        if matches!(k.snapshot, Snapshot::Reject) { return Err(s_err("cannot restore a value of the kind", &name)); }
        if k.version != version { return Err(s_err("saved by another version of the kind", &name)); }
        let at = Val::foreign(generation, index);
        let store = match r.u8()? {
            tag @ (0 | 1) => {
                let w = [r.u64()?, r.u64()?, r.u64()?];
                if tag == 0 { Store::Words(w) } else { held.extend(w); Store::Vals(w.map(|x| remade(Val(x)))) }
            }
            2 => {
                let Snapshot::Serialize { load, .. } = k.snapshot else { return Err(s_err("cannot restore a value of the kind", &name)) };
                let body = r.bytes()?;
                Store::Shared(load(&body).ok_or_else(|| s_err("malformed value of the kind", &name))?)
            }
            _ => return Err("snapshot holds a malformed value".to_string()),
        };
        if index as usize >= len || vm.heap.foreign.restore(kind, generation, index, store).is_none() { return Err("snapshot holds a malformed value".to_string()); }
        // the ellipsis the run made stays the one every later use gives
        if vm.natives.built.ellipsis.is_some_and(|e| core::ptr::eq(e, k)) { vm.heap.dots = Some(at); }
        r.foreign.insert(at.0);
    }
    vm.heap.foreign.settle(len);
    for w in held { r.checked(w)?; }
    Ok(())
}

struct Header<'a> {
    source: &'a str,
    entry: &'a str,
    // the identity of the runtime that saved it
    runtime: u64,
    fingerprint: u64,
    body: usize,
    // the blob without its seal which every reader starts from
    data: &'a [u8],
}

fn header(blob: &[u8]) -> Result<Header<'_>, SnapErr> {
    let (data, seal) = blob.split_at(blob.len().saturating_sub(SEAL));
    let mut r = R::new(data);
    if r.u32()? != MAGIC { return Err("not an edge snapshot".to_string()); }
    if r.u32()? != FORMAT { return Err("unsupported snapshot format".to_string()); }
    if crate::util::sha256::sha256(data) != seal { return Err("snapshot is damaged".to_string()); }
    let runtime = r.u64()?;
    let fp = r.u64()?;
    let (n, e) = (r.count()?, r.count()?);
    let source = core::str::from_utf8(r.take(n)?).map_err(|_| "snapshot source not utf-8".to_string())?;
    let entry = core::str::from_utf8(r.take(e)?).map_err(|_| "snapshot entry not utf-8".to_string())?;
    Ok(Header { source, entry, runtime, fingerprint: fp, body: r.p, data })
}

/* the host re-parses the embedded source before restoring */
pub fn source_of(blob: &[u8]) -> Result<&str, SnapErr> {
    Ok(header(blob)?.source)
}

/* a blob's starting script which the host names its entry again before the restore */
pub fn entry_of(blob: &[u8]) -> Result<&str, SnapErr> {
    Ok(header(blob)?.entry)
}

/* the identity of the runtime a blob names or none for a short blob */
pub fn runtime_of(blob: &[u8]) -> Option<u64> {
    blob.get(RUNTIME_AT..RUNTIME_AT + 8)?.try_into().ok().map(u64::from_le_bytes)
}

/* whether runtime `rt` saved a blob read before its source parses */
pub fn saved_under(blob: &[u8], rt: &crate::lang::Runtime) -> bool { runtime_of(blob) == Some(rt.identity) }

/* the sandbox profile recorded at save time where `ops` is the remaining budget */
pub fn limits_of(blob: &[u8]) -> Result<Limits, SnapErr> {
    let h = header(blob)?;
    let mut r = R::new(h.data);
    r.p = h.body;
    let ops = r.u64()?;
    let memory = r.usz()?;
    Ok(Limits { ops, memory })
}

fn collect_externs(chunk: &SSAChunk, map: &mut ExternMap) {
    for f in &chunk.extern_table {
        map.entry(f.name.clone()).or_insert_with(|| f.clone());
    }
    for entry in &chunk.imports {
        match &entry.kind {
            ImportKind::Code(sub) => collect_externs(sub, map),
            ImportKind::Native { funcs, classes, consts } => {
                for f in funcs.iter().chain(consts.iter()) {
                    map.entry(f.name.clone()).or_insert_with(|| f.clone());
                }
                for c in classes {
                    for m in &c.methods {
                        map.entry(m.name.clone()).or_insert_with(|| m.clone());
                    }
                }
            }
        }
    }
    for (_, body, _, _) in &chunk.functions { collect_externs(body, map); }
    for body in &chunk.classes { collect_externs(body, map); }
}

pub(crate) fn source_index<'c>(chunk: &'c SSAChunk, out: &mut Vec<&'c SSAChunk>) {
    out.push(chunk);
    for entry in &chunk.imports {
        if let ImportKind::Code(sub) = &entry.kind { source_index(sub, out); }
    }
}

/* overlays saved state onto a freshly booted VM */
pub fn restore(vm: &mut VM, blob: &[u8]) -> Result<(), SnapErr> {
    let _paying = crate::value::meter::Paying::to(vm.heap.meter());
    let h = header(blob)?;
    if h.runtime != vm.rt.identity { return Err(OTHER_RUNTIME.to_string()); }
    if h.fingerprint != fingerprint(vm.chunk) {
        return Err("snapshot does not match this program or compiler version".to_string());
    }
    let mut r = R::new(h.data);
    (r.p, r.iters, r.natives) = (h.body, &vm.natives.names().iters, vm.natives);

    // a restore spends what its blob saved never past the limits its vm booted with
    vm.budget = r.u64()?.min(vm.budget);
    vm.heap.set_budget(vm.budget);
    let _memory = r.usz()?;
    vm.strict_input = r.boolean()?;
    // run-time code reparses now for the heap's natives and joins the program after the bindings
    let compiled = r.seq(|r| {
        let source = r.str()?;
        let (spec, path) = (r.str()?, alloc::sync::Arc::new(r.str()?));
        let names = vm.natives.names();
        let chunk = vm.parse_compiled(&source, names.compiled, &path).map_err(|e| e.render(names))?;
        if fingerprint(&chunk) != r.u64()? { return Err("snapshot does not match the code this program compiled".to_string()); }
        Ok((source, (!spec.is_empty()).then_some(spec), chunk))
    })?;
    let specs = r.seq(|r| Ok((r.str()?, r.usz()?)))?;
    get_foreign(&mut r, vm)?;

    let mut externs = ExternMap::default();
    collect_externs(vm.chunk, &mut externs);
    for (.., chunk) in &compiled { collect_externs(chunk, &mut externs); }
    let nslots = r.count()?;
    let mut objs: Vec<Option<HeapObj>> = Vec::with_capacity(nslots);
    let (mut fills, mut heads, mut held): (Vec<SetFill>, Vec<Head>, Vec<(u32, Val)>) = (Vec::new(), Vec::new(), Vec::new());
    let mut fn_fields: crate::value::object::FnFields = Vec::new();
    let mut hosts = vm.heap.objects.as_deref_mut().map(|t| core::mem::take(&mut t.externs)).unwrap_or_default();
    for slot in 0..nslots {
        match r.u8()? {
            0 => objs.push(None),
            _ => objs.push(Some(get_obj(&mut r, &externs, &mut hosts, vm.natives, &mut fills, &mut heads, &mut held, &mut fn_fields, slot as u32)?)),
        }
        // a heap past the limit of its vm is refused as it decodes
        if vm.heap.over() { return Err(NEEDS_MEMORY.to_string()); }
    }
    if !hosts.is_empty() { vm.heap.tables_mut().externs = hosts; }
    r.live = Some(objs.iter().map(Option::is_some).collect());
    vm.heap.restore_heads(heads);
    vm.heap.restore_objs(objs);
    if !vm.heap.restore_args(held) { return Err("snapshot holds a malformed value".to_string()); }
    vm.heap.restore_fn_fields(fn_fields);
    let order = r.seq(|r| r.u32())?;
    if !vm.heap.restore_free(order) { return Err("snapshot holds a malformed value".to_string()); }
    // a collection falls due when it would have so freed slots come back alike
    let schedule = [r.usz()?, r.usz()?, r.usz()?, r.usz()?, r.usz()?];
    vm.heap.set_schedule(schedule);
    for _ in 0..r.count()? {
        let (class, qual) = (r.val()?, r.str()?);
        if !vm.heap.is_class(class) { return Err("snapshot names a missing class".to_string()); }
        vm.class_qualnames.insert(class.0, qual);
    }

    // the bindings each module holds at boot which the blob's must keep in place
    let booted: Vec<Vec<String>> = vm.scopes.iter().map(|g| g.entries().map(|(n, _)| n.to_string()).collect()).collect();
    get_vm_state(&mut r, vm)?;
    (vm.input_buffer, vm.input_at) = (r.seq(|r| r.str())?, 0);
    if specs.iter().any(|&(_, i)| i >= vm.scopes.len()) { return Err("snapshot names a missing module".to_string()); }
    vm.scope_ids.extend(specs);
    // in compile order so their functions take the indices the heap names
    for (source, spec, chunk) in compiled { vm.adopt(&source, spec, chunk); }
    check_objs(vm, &fills)?;
    checked_tops(vm)?;
    // the resumed module body must be a coroutine as anything else would trap the drive
    if vm.root.as_ref().is_some_and(|h| !matches!(vm.heap.try_get(h.coro), Some(HeapObj::Coroutine(c)) if matches!(c.body, BodyRef::Module))) {
        return Err("snapshot resumes a missing module body".to_string());
    }

    let chunk = vm.chunk;
    let mut sources: Vec<&SSAChunk> = Vec::new();
    source_index(chunk, &mut sources);
    for c in vm.compiled.iter() { source_index(c.chunk(), &mut sources); }
    let nfn = vm.functions.len();
    vm.call_stack = r.seq(|r| {
        let fi = r.usz()?;
        if fi >= nfn { return Err("snapshot names a missing function".to_string()); }
        let call_byte_pos = r.u32()?;
        let path = r.str()?;
        let owner = sources.iter().find(|c| c.path.as_str() == path).copied().unwrap_or(chunk);
        Ok(CallFrame {
            fi,
            call_byte_pos,
            caller_source: Some(owner.source.clone()),
            caller_path: Some(owner.path.clone()),
            repeats: r.u32()?,
        })
    })?;
    get_tiers(&mut r, vm)?;
    let prints = r.seq(|r| Ok((r.i64()?, r.u64()?)))?;

    if r.p != r.b.len() { return Err("snapshot has trailing bytes".to_string()); }
    // the list takes its iterators' cell so they walk it and not a copy
    for (of, cell) in core::mem::take(&mut r.lists) {
        let Some(HeapObj::List(list)) = vm.heap.try_get_mut(Val(of)) else { return Err("snapshot walks a list that is not there".to_string()) };
        *cell.borrow_mut() = core::mem::take(&mut *list.borrow_mut());
        *list = cell;
    }
    // derived bits not serialized and recomputed from the restored module bindings
    let natives = vm.natives;
    let ids: Vec<u16> = vm.scopes.iter().flat_map(|g| g.iter()).filter_map(|(k, _)| natives.function_id(k)).map(|id| id.0).collect();
    for id in ids { vm.rebind_native(id); }
    rehash(vm, fills)?;
    rebuild_mro(vm)?;
    vm.vet(&booted, &prints)?;
    if vm.heap.over() { return Err(NEEDS_MEMORY.to_string()); }
    Ok(())
}

/* rejects objects decoded before the heap with dangling refs or missing functions or stepless ranges */
fn check_objs(vm: &VM, fills: &[SetFill]) -> Result<(), SnapErr> {
    let nfn = vm.functions.len();
    let dangling = |v: Val| v.is_heap() && vm.heap.try_get(v).is_none();
    let mut set_items = fills.iter().flat_map(|(_, items)| items);
    if set_items.any(|&v| dangling(v)) || vm.heap.foreign.holds_any(dangling) { return Err("snapshot references a missing object or function".to_string()); }
    if vm.heap.objects.as_deref().is_some_and(|t| t.fn_fields.iter().any(|f| f.iter().any(|&(_, v)| dangling(v)))) { return Err("snapshot references a missing object or function".to_string()); }
    if vm.heap.heads.iter().any(|h| h.members.iter().any(|&(_, v)| dangling(v))) { return Err("snapshot references a missing object or function".to_string()); }
    for obj in vm.heap.snapshot_objs().flatten() {
        let mut ok = match obj {
            &HeapObj::Func { fi, .. } => fi < nfn,
            HeapObj::Object { .. } => vm.heap.well_formed(obj),
            HeapObj::Coroutine(c) => !matches!(c.body, BodyRef::Fn(fi) if fi >= nfn),
            &HeapObj::Range(_, _, step) => step != 0,
            // a wide range keeps ints and its step is never zero
            &HeapObj::BigRange(s, e, st) => [s, e, st].iter().all(|&x| x.is_int() || vm.heap.is_wide(x)) && st != Val::int(0),
            _ => true,
        };
        crate::value::for_each_val(obj, |v| ok &= !dangling(v));
        if !ok { return Err("snapshot references a missing object or function".to_string()); }
    }
    Ok(())
}

/* checks each parked frame's top lies within the rows it saved past its names */
fn checked_tops(vm: &VM) -> Result<(), SnapErr> {
    let first = |fi: Option<usize>| -> Result<usize, SnapErr> {
        let c = match fi { None => vm.chunk, Some(fi) => &vm.functions.get(fi).ok_or("snapshot parks a missing function")?.1 };
        Ok(super::rows::first_row(c.names.len()))
    };
    let held = |s0: usize, top: u16, slots: &[Val]| -> Result<(), SnapErr> {
        if top != 0 && (usize::from(top) < s0 || usize::from(top) > slots.len()) { return Err("snapshot holds a malformed value".to_string()); }
        Ok(())
    };
    for f in &vm.pending_sync_frames { held(first(Some(f.fi))?, f.top, &f.slots)?; }
    for obj in vm.heap.snapshot_objs().flatten() {
        let HeapObj::Coroutine(c) = obj else { continue };
        // a finished generator never runs again whatever top its last yield left
        if c.ip == super::coro::FINISHED { continue; }
        held(first(match c.body { BodyRef::Fn(fi) => Some(fi), BodyRef::Module => None })?, c.top, &c.slots)?;
        for f in &c.syncs { held(first(Some(f.fi))?, f.top, &f.slots)?; }
    }
    Ok(())
}

/* each class's order cached after its bases' whatever slots they lie in */
fn rebuild_mro(vm: &mut VM) -> Result<(), SnapErr> {
    let nslots = vm.heap.snapshot_objs().count();
    let mut done: Vec<u8> = alloc::vec![0; nslots];
    for idx in 0..nslots {
        if done[idx] != 0 || !vm.heap.is_class(Val::heap(idx as u32)) { continue; }
        done[idx] = 1;
        // a class orders after its bases and one met again mid walk closes a cycle
        let mut walk: Vec<(Val, usize)> = alloc::vec![(Val::heap(idx as u32), 0)];
        while let Some(&mut (v, ref mut next)) = walk.last_mut() {
            match vm.heap.bases(v).get(*next).copied() {
                Some(b) => {
                    *next += 1;
                    let Some(state) = b.is_heap().then(|| done.get_mut(b.as_heap() as usize)).flatten().filter(|_| vm.heap.is_class(b)) else { continue };
                    if *state == 1 { return Err("snapshot class hierarchy is inconsistent".to_string()); }
                    if *state == 0 { *state = 1; walk.push((b, 0)); }
                }
                None => {
                    let order = vm.order_of(v, vm.heap.bases(v));
                    vm.mro_cache.insert(v.0, Rc::new(order));
                    done[v.as_heap() as usize] = 2;
                    walk.pop();
                    if vm.heap.over() { return Err(NEEDS_MEMORY.to_string()); }
                }
            }
        }
    }
    Ok(())
}

/* hashing reads the heap so sets and dicts index once every slot is live */
fn rehash(vm: &mut VM, fills: Vec<SetFill>) -> Result<(), SnapErr> {
    for (slot, items) in fills {
        let rc = match vm.heap.try_get(Val::heap(slot)) {
            Some(HeapObj::Set(rc)) => rc.clone(),
            _ => return Err("snapshot set slot mismatch".to_string()),
        };
        *rc.borrow_mut() = ValSet::from_vals(&items, &vm.heap);
    }
    let nslots = vm.heap.snapshot_objs().count();
    // every table indexes before a class lookup reads one
    for idx in 0..nslots {
        if let Some(HeapObj::Dict(rc) | HeapObj::Object { slots: rc, .. }) = vm.heap.try_get(Val::heap(idx as u32)) { rc.borrow_mut().rebuild_index(&vm.heap); }
    }
    for idx in 0..nslots {
        let Some((class, rc)) = vm.heap.instance(Val::heap(idx as u32)) else { continue };
        // an attribute hiding a method keeps method sites checking the instance
        if rc.borrow().iter().any(|(k, _)| matches!(vm.heap.try_get(k), Some(HeapObj::Str(n)) if vm.lookup_class_member(class, n).is_some_and(|(m, _)| vm.hides(m)))) && !core::mem::replace(&mut vm.shadowed, true) { vm.layout_epoch = vm.layout_epoch.wrapping_add(1); }
    }
    // the hashing and probes of the rebuilt tables were paid when the run built them
    vm.heap.take_work();
    Ok(())
}

/* appends `s` as a quoted JSON string */
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    json_escape(out, s);
    out.push('"');
}

/* appends `items` as a JSON array with `item` writing each element */
fn json_array<T>(out: &mut String, items: impl IntoIterator<Item = T>, mut item: impl FnMut(&mut String, T)) {
    out.push('[');
    for (i, x) in items.into_iter().enumerate() {
        if i > 0 { out.push(','); }
        item(out, x);
    }
    out.push(']');
}

/* module bindings as a JSON object mapping each name to its repr */
pub fn inspect_globals(vm: &mut VM) -> String {
    let mut out = String::from("{");
    if vm.root.is_some() {
        for (i, (name, v)) in super::init::collect_module_attrs(vm.chunk, &vm.scopes[0]).into_iter().enumerate() {
            if i > 0 { out.push(','); }
            json_str(&mut out, &name);
            out.push(':');
            json_str(&mut out, &vm.display(v));
        }
    }
    out.push('}');
    out
}

/* the suspended module body as a JSON array left empty between runs */
pub fn inspect_stack(vm: &VM) -> String {
    let fn_name = |fi: usize| -> &str {
        match vm.function_names.get(fi) {
            Some(n) if !n.is_empty() => n,
            _ => vm.natives.names().anonymous,
        }
    };
    let mut out = String::new();
    json_array(&mut out, &vm.root, |out, h| {
        let state = match &h.state {
            CoroState::Ready => "ready",
            CoroState::WaitingEvent => "waiting_event",
            CoroState::WaitingHostCall(_) => "waiting_host_call",
            CoroState::Raising(..) => "raising",
        };
        let (function, ip, frames) = match vm.heap.try_get(h.coro) {
            Some(HeapObj::Coroutine(c)) => {
                (match c.body { BodyRef::Module => vm.natives.names().module_body, BodyRef::Fn(fi) => fn_name(fi) }, c.ip, &c.syncs[..])
            }
            _ => ("", 0, &[][..]),
        };
        out.push_str("{\"state\":");
        json_str(out, state);
        out.push_str(",\"function\":");
        json_str(out, function);
        out.push_str(",\"ip\":");
        out.push_str(itoa::Buffer::new().format(ip));
        out.push_str(",\"frames\":");
        json_array(out, frames, |out, f| {
            out.push_str("{\"function\":");
            json_str(out, fn_name(f.fi));
            out.push_str(",\"ip\":");
            out.push_str(itoa::Buffer::new().format(f.ip));
            out.push('}');
        });
        out.push('}');
    });
    out
}

/* a chunk compiled mid-run kept while the VM lives since its functions point into it */
pub(crate) struct Compiled {
    pub(crate) source: String,
    // the module its names bind into
    pub(crate) spec: Option<String>,
    chunk: Rc<SSAChunk>,
}

impl Compiled {

    /* the chunk which lives as long as the VM keeps it */
    pub(crate) fn chunk(&self) -> &SSAChunk { &self.chunk }
}

/* every chunk compiled at run time in the order it was made */
#[derive(Default)]
pub(crate) struct CompiledSet { list: Vec<Compiled> }

impl CompiledSet {

    fn push(&mut self, c: Compiled) -> usize {
        self.list.push(c);
        self.list.len() - 1
    }
}

impl core::ops::Deref for CompiledSet {

    type Target = [Compiled];

    fn deref(&self) -> &[Compiled] { &self.list }
}

impl<'a> VM<'a> {

    /* makes `chunk` part of the program tabling its functions and binding its names into `spec` */
    pub(crate) fn adopt(&mut self, source: &str, spec: Option<String>, chunk: Rc<SSAChunk>) -> usize {
        // the VM keeps every compiled chunk until it drops and lends none past that
        let body: &'a SSAChunk = pinned(&chunk);
        self.adopt_functions(body, spec.as_deref());
        self.compiled.push(Compiled { source: source.to_string(), spec, chunk })
    }

    /* parses `source` without imports as statements of the file at `path` in the runtime's spelling */
    pub(crate) fn parse_compiled(&self, source: &str, filename: &str, path: &Arc<String>) -> Result<Rc<SSAChunk>, VmErr> {
        let Some(front) = &self.rt.front else { return Err(VmErr::Runtime("this runtime compiles no code while a program runs")) };
        match front.exec(source, path) {
            Ok(mut chunk) => { if let Some(chunk) = Rc::get_mut(&mut chunk) { crate::code::fold::constant_fold(chunk, front.policy); } Ok(chunk) }
            Err(errs) => {
                let Some(e) = errs.first() else { return Err(VmErr::Runtime("a front end failed without saying why")) };
                let line = source[..e.start.min(source.len())].matches('\n').count() + 1;
                Err(VmErr::Of(crate::lang::ErrKind::Syntax, s!(str &e.msg, " (", str filename, ", line ", int line as i64, ")")))
            }
        }
    }
}
