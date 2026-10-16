use alloc::{boxed::Box, rc::Rc};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::hash::Hasher;

use crate::parser::types::{ImportKind, SSAChunk};
use crate::util::hash::{FxHashMap, FxHashSet, FxHasher};
use crate::util::jesc::escape as json_escape;
use super::{Pending, VM};
use super::types::*;

const MAGIC: u32 = 0x4E53_5045;
const FORMAT: u32 = 17;
// A SHA-256 of everything before it closes each blob, so a damaged one never restores.
const SEAL: usize = 32;

pub type SnapErr = String;

type ExternMap = FxHashMap<String, ExternFn>;
// The cell a heap list keeps its items in, shared with every iterator walking it.
type ListCell = Rc<RefCell<Vec<Val>>>;

fn s_err(what: &str, detail: &str) -> SnapErr {
    crate::s!("snapshot: ", str what, " '", str detail, "'")
}

struct W {
    b: Vec<u8>,
    // The slot of each heap list, by its cell, so an iterator names the list it walks.
    lists: FxHashMap<usize, Val>,
}

impl W {
    fn new() -> Self { Self { b: Vec::with_capacity(4096), lists: FxHashMap::default() } }
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
    fn opt_u64(&mut self, v: Option<u64>) { self.opt(v, Self::u64); }
    fn opt_usz(&mut self, v: Option<usize>) { self.opt(v, Self::usz); }
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
    // Which heap slots hold an object, known once the heap is decoded.
    live: Option<Vec<bool>>,
    // One cell per list an iterator walks, swapped in for that list once the heap is back.
    lists: FxHashMap<u64, ListCell>,
    // The FOREIGN values the blob restored, the only ones a value may name.
    foreign: FxHashSet<u64>,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self { Self { b, p: 0, live: None, lists: FxHashMap::default(), foreign: FxHashSet::default() } }
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
    /* Rejects counts beyond the remaining payload. */
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
    /* Static-str decode leaks, stored errors are rare. */
    fn leakstr(&mut self) -> Result<&'static str, SnapErr> {
        Ok(crate::util::leak(self.str()?.into_boxed_str()))
    }
    /* A blob is untrusted, a value must be well formed and past the heap name a live slot. */
    fn val(&mut self) -> Result<Val, SnapErr> {
        let v = Val(self.u64()?);
        let known = v.is_float() || v.is_int() || v.is_none() || v.is_bool() || v.is_undef();
        if v.is_foreign() && !self.foreign.contains(&v.0) { return Err("snapshot references a missing object".to_string()); }
        if !known && !v.is_foreign() && !(v.is_heap() && Val::heap(v.as_heap()).0 == v.0) {
            return Err("snapshot holds a malformed value".to_string());
        }
        if v.is_heap() && let Some(live) = &self.live && !live.get(v.as_heap() as usize).copied().unwrap_or(false) {
            return Err("snapshot references a missing object".to_string());
        }
        // A float goes back through `Val::float`, so a NaN in the blob is the one NaN the engine makes.
        Ok(if v.is_float() { Val::float(v.as_float()) } else { v })
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
    fn opt_u64(&mut self) -> Result<Option<u64>, SnapErr> { self.opt(Self::u64) }
    fn opt_usz(&mut self) -> Result<Option<usize>, SnapErr> { self.opt(Self::usz) }
}

/* One declaration emits both codec sides. */
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

fn put_slot_val(w: &mut W, p: &(usize, Val)) { w.usz(p.0); w.val(p.1); }
fn get_slot_val(r: &mut R) -> Result<(usize, Val), SnapErr> { Ok((r.usz()?, r.val()?)) }
fn put_name_val(w: &mut W, p: &(String, Val)) { w.str(&p.0); w.val(p.1); }
fn get_name_val(r: &mut R) -> Result<(String, Val), SnapErr> { Ok((r.str()?, r.val()?)) }
fn put_handled(w: &mut W, h: &(Val, Option<u32>)) { w.val(h.0); w.opt_u32(h.1); }
fn get_handled(r: &mut R) -> Result<(Val, Option<u32>), SnapErr> { Ok((r.val()?, r.opt_u32()?)) }
fn put_i32_pair(w: &mut W, p: &(i32, i32)) { w.i32v(p.0); w.i32v(p.1); }
fn get_i32_pair(r: &mut R) -> Result<(i32, i32), SnapErr> { Ok((r.i32v()?, r.i32v()?)) }

codec!(enum BlockKind, put_block_kind, get_block_kind { 0 Except, 1 Finally });

codec!(enum BodyRef, put_body_ref, get_body_ref { 0 Fn(fi: usz), 1 Module });

/* A list frame names its list by slot, so it reads the list live after a restore, one nothing else holds keeps its items. */
fn put_iter_frame(w: &mut W, x: &IterFrame) {
    let walks = |w: &W, rc: &ListCell| w.lists.get(&(Rc::as_ptr(rc) as usize)).copied();
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
        IterFrame::Map { f, its } => { w.u8(4); w.val(*f); w.vals(its); }
        IterFrame::Filter { f, it } => { w.u8(5); w.val(*f); w.val(*it); }
        IterFrame::Zip { its } => { w.u8(6); w.vals(its); }
        IterFrame::Enumerate { it, n } => { w.u8(7); w.val(*it); w.val(*n); }
        IterFrame::Call { f, sentinel } => { w.u8(8); w.val(*f); w.val(*sentinel); }
        IterFrame::Watched { items, idx, of, len } => { w.u8(9); w.vals(items); w.usz(*idx); w.val(*of); w.usz(*len); }
        IterFrame::DictWalk { of, idx, left, len, kind } => { w.u8(10); w.val(*of); w.usz(*idx); w.usz(*left); w.usz(*len); w.u8(*kind as u8); }
    }
}
fn get_iter_frame(r: &mut R) -> Result<IterFrame, SnapErr> {
    Ok(match r.u8()? {
        0 => IterFrame::Seq { items: r.vals()?.into(), idx: r.usz()? },
        1 => IterFrame::Range { cur: r.i64()?, end: r.i64()?, step: r.i64()? },
        13 => IterFrame::BigRange { cur: r.val()?, end: r.val()?, step: r.val()? },
        2 => IterFrame::Coroutine(r.val()?),
        3 => IterFrame::UserDefined(r.val()?),
        4 => IterFrame::Map { f: r.val()?, its: r.vals()?.into() },
        5 => IterFrame::Filter { f: r.val()?, it: r.val()? },
        6 => IterFrame::Zip { its: r.vals()?.into() },
        7 => IterFrame::Enumerate { it: r.val()?, n: r.val()? },
        8 => IterFrame::Call { f: r.val()?, sentinel: r.val()? },
        9 => IterFrame::Watched { items: r.vals()?.into(), idx: r.usz()?, of: r.val()?, len: r.usz()? },
        10 => {
            let (of, idx, left, len) = (r.val()?, r.usz()?, r.usz()?, r.usz()?);
            let kind = *[View::Keys, View::Values, View::Items].get(r.u8()? as usize).ok_or("snapshot names an unknown dict view")?;
            IterFrame::DictWalk { of, idx, left, len, kind }
        }
        11 => { let (rc, idx) = list_cell(r)?; IterFrame::List { rc, idx } }
        12 => { let (rc, idx) = list_cell(r)?; IterFrame::ListRev { rc, idx } }
        t => return Err(s_err("unknown tag", itoa::Buffer::new().format(t))),
    })
}

/* The cell every iterator over one list shares, filled from that list once the heap is restored. */
fn list_cell(r: &mut R) -> Result<(ListCell, usize), SnapErr> {
    let (of, idx) = (r.val()?, r.usz()?);
    Ok((r.lists.entry(of.0).or_insert_with(|| Rc::new(RefCell::new(Vec::new()))).clone(), idx))
}

codec!(struct ExceptionFrame, put_exc_frame, get_exc_frame {
    kind: (put_block_kind, get_block_kind),
    handler_ip: usz,
    stack_depth: usz,
    iter_depth: usz,
    with_depth: usz,
    unwind_depth: usz,
    handling_depth: usz,
});

codec!(struct SyncFrame, put_sync_frame, get_sync_frame {
    ip: usz,
    fi: usz,
    func: val,
    slots: vals,
    stack_delta: vals,
    iter_delta: [put_iter_frame, get_iter_frame],
    exception_delta: [put_exc_frame, get_exc_frame],
    instance: val,
});

codec!(enum SchedulerStatus, put_sched, get_sched {
    1 PendingTimer(d: u64),
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
});

codec!(enum Unwind, put_unwind, get_unwind {
    0 Normal,
    1 Return(v: val),
    2 Goto { target: usz, remaining: u16 },
    3 Reraise(e: (put_vm_err, get_vm_err), at: opt_u32),
});

codec!(enum WaitKind, put_wait_kind, get_wait_kind {
    0 Run(v: val),
    1 Gather,
    2 Timeout { deadline_ns: u64, target: val },
});

codec!(enum CoroState, put_coro_state, get_coro_state {
    0 Ready,
    1 Sleeping(d: u64),
    3 WaitingEvent,
    4 WaitingHostCall(id: u64),
    5 WaitingForChildren { tasks: vals, kind: (put_wait_kind, get_wait_kind) },
    6 CancelPending,
    7 Done(v: val),
    8 Errored(e: (put_vm_err, get_vm_err)),
    9 Cancelled,
    10 Raising(e: (put_vm_err, get_vm_err), exc: opt_val),
});

codec!(struct CoroutineHandle, put_handle, get_handle {
    coro: val,
    state: (put_coro_state, get_coro_state),
});

fn put_wfc(w: &mut W, v: &Option<(Vec<Val>, WaitKind)>) { w.opt(v.as_ref(), |w, (tasks, kind)| { w.vals(tasks); put_wait_kind(w, kind); }); }
fn get_wfc(r: &mut R) -> Result<Option<(Vec<Val>, WaitKind)>, SnapErr> { r.opt(|r| Ok((r.vals()?, get_wait_kind(r)?))) }
fn put_binding(w: &mut W, v: &Option<(Val, Val)>) { w.opt(*v, |w, (a, b)| { w.val(a); w.val(b); }); }
fn get_binding(r: &mut R) -> Result<Option<(Val, Val)>, SnapErr> { r.opt(|r| Ok((r.val()?, r.val()?))) }

codec!(struct Pending, put_pending, get_pending {
    pos_delta: i32v,
    kw_delta: i32v,
    delta_save: [put_i32_pair, get_i32_pair],
    call_ip: opt_u32,
    sleep_until_ns: opt_u64,
    event_wait_request: boolean,
    host_call_request: boolean,
    host_call_id: u64,
    waiting_for_children: (put_wfc, get_wfc),
    exc_val: opt_val,
    method_binding: (put_binding, get_binding),
    preempt_request: default,
});

/* Hashes are taken again on restore, so only the entries travel. */
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

/* Sorted so identical states produce identical blobs. */
fn put_map(w: &mut W, m: &FxHashMap<String, Val>) {
    let mut pairs: Vec<(&String, &Val)> = m.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    w.usz(pairs.len());
    for (k, v) in pairs { w.str(k); w.val(*v); }
}

/* A module's bindings in index order, unbound ones kept so indices hold. */
fn put_globals(w: &mut W, g: &super::scope::Globals) {
    let entries: Vec<(&str, Val)> = g.entries().collect();
    w.seq(&entries, |w, &(name, v)| { w.str(name); w.val(v); });
}

fn get_globals(r: &mut R) -> Result<super::scope::Globals, SnapErr> {
    Ok(super::scope::Globals::from_entries(r.seq(|r| Ok((r.str()?, r.val()?)))?))
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

/* Set items are inserted in the rehash pass. */
enum SetFill {
    Mutable(Vec<Val>),
    Frozen(Vec<Val>),
}

fn put_obj(w: &mut W, obj: &HeapObj) {
    match obj {
        HeapObj::Str(s) => { w.u8(0); w.str(s); }
        HeapObj::Bytes(b) => { w.u8(1); w.bytes(b); }
        HeapObj::List(rc) => { w.u8(2); w.vals(&rc.borrow()); }
        HeapObj::Dict(rc) => { w.u8(3); put_dict(w, &rc.borrow()); }
        HeapObj::Set(rc) => { w.u8(4); put_set(w, &rc.borrow()); }
        HeapObj::FrozenSet(rc) => { w.u8(5); put_set(w, rc); }
        HeapObj::Tuple(v) => { w.u8(6); w.vals(v); }
        HeapObj::Func(fi, captures, defaults, attrs) => {
            w.u8(7); w.usz(*fi); w.vals(captures); w.seq(defaults, put_slot_val); w.seq(&attrs.borrow(), put_name_val);
        }
        HeapObj::Range(s, e, st) => { w.u8(8); w.i64(*s); w.i64(*e); w.i64(*st); }
        HeapObj::BigRange(s, e, st) => { w.u8(13); w.val(*s); w.val(*e); w.val(*st); }
        HeapObj::Slice(a, b, c) => { w.u8(9); w.val(*a); w.val(*b); w.val(*c); }
        HeapObj::Ellipsis => w.u8(10),
        HeapObj::Type(n) => { w.u8(11); w.str(n); }
        HeapObj::NotImplemented => w.u8(12),
        HeapObj::ExcInstance(n, args, chain) => { w.u8(14); w.str(n); w.vals(args); w.val(*chain); }
        HeapObj::BoundMethod(recv, id) => { w.u8(15); w.val(*recv); w.u8(id.raw()); }
        HeapObj::NativeFn(id) => { w.u8(16); w.str(id.name()); }
        HeapObj::Class(n, bases, members) => {
            w.u8(17); w.str(n); w.vals(bases); w.seq(&members.borrow(), put_name_val);
        }
        HeapObj::Instance(cls, dict) => { w.u8(18); w.val(*cls); put_dict(w, &dict.borrow()); }
        HeapObj::BoundUserMethod(a, b, c) => { w.u8(19); w.val(*a); w.val(*b); w.val(*c); }
        HeapObj::Super(a, b) => { w.u8(20); w.val(*a); w.val(*b); }
        HeapObj::Property(a, b) => { w.u8(21); w.val(*a); w.val(*b); }
        HeapObj::PropertySetter(a) => { w.u8(22); w.val(*a); }
        HeapObj::StaticMethod(a) => { w.u8(23); w.val(*a); }
        HeapObj::ClassMethod(a) => { w.u8(27); w.val(*a); }
        HeapObj::Coroutine(c) => {
            w.u8(24); w.usz(c.ip); w.vals(&c.slots); w.vals(&c.stack); put_body_ref(w, &c.body);
            w.seq(&c.iters, put_iter_frame); w.seq(&c.syncs, put_sync_frame); w.seq(&c.excs, put_exc_frame);
        }
        HeapObj::Module(spec, attrs) => { w.u8(25); w.str(spec); w.seq(attrs, put_name_val); }
        HeapObj::GenericAlias(o, a) => { w.u8(29); w.val(*o); w.val(*a); }
        HeapObj::TypeAlias(n, f) => { w.u8(30); w.str(n); w.val(*f); }
        HeapObj::Union(a) => { w.u8(31); w.val(*a); }
        HeapObj::TypeVar(n) => { w.u8(32); w.str(n); }
        HeapObj::Iter(frame, name) => {
            w.u8(33);
            w.u8(crate::value::ITER_KINDS.iter().position(|k| k == name).unwrap_or(0) as u8);
            put_iter_frame(w, &frame.borrow());
        }
        HeapObj::Extern(f) => { w.u8(26); w.str(&f.name); }
        HeapObj::Cell(v) => { w.u8(34); w.val(*v); }
        HeapObj::DictView(d, kind) => { w.u8(35); w.val(*d); w.u8(*kind as u8); }
        HeapObj::Code(c) => { w.u8(36); w.str(&c.source); w.str(&c.filename); w.u8(c.mode as u8); }
    }
}

fn get_obj(r: &mut R, externs: &ExternMap, fills: &mut Vec<(u32, SetFill)>, slot: u32) -> Result<HeapObj, SnapErr> {
    Ok(match r.u8()? {
        0 => HeapObj::Str(r.str()?),
        1 => HeapObj::Bytes(r.bytes()?),
        2 => HeapObj::List(Rc::new(RefCell::new(r.vals()?))),
        3 => HeapObj::Dict(Rc::new(RefCell::new(get_dict(r)?))),
        4 => {
            fills.push((slot, SetFill::Mutable(get_set_items(r)?)));
            HeapObj::Set(Rc::new(RefCell::new(ValSet::default())))
        }
        5 => {
            fills.push((slot, SetFill::Frozen(get_set_items(r)?)));
            HeapObj::FrozenSet(Rc::new(ValSet::default()))
        }
        6 => HeapObj::Tuple(r.vals()?),
        7 => HeapObj::Func(r.usz()?, r.vals()?, r.seq(get_slot_val)?, Rc::new(RefCell::new(r.seq(get_name_val)?))),
        8 => HeapObj::Range(r.i64()?, r.i64()?, r.i64()?),
        13 => HeapObj::BigRange(r.val()?, r.val()?, r.val()?),
        9 => HeapObj::Slice(r.val()?, r.val()?, r.val()?),
        10 => HeapObj::Ellipsis,
        11 => HeapObj::Type(r.str()?),
        12 => HeapObj::NotImplemented,
        14 => HeapObj::ExcInstance(r.str()?, r.vals()?, r.val()?),
        15 => {
            let recv = r.val()?;
            let id = BuiltinMethodId::from_raw(r.u8()?).ok_or_else(|| "unknown builtin method id".to_string())?;
            HeapObj::BoundMethod(recv, id)
        }
        16 => {
            let name = r.str()?;
            HeapObj::NativeFn(NativeFnId::from_name(&name).ok_or_else(|| s_err("unknown builtin", &name))?)
        }
        17 => HeapObj::Class(r.str()?, r.vals()?, Rc::new(RefCell::new(r.seq(get_name_val)?))),
        18 => HeapObj::Instance(r.val()?, Rc::new(RefCell::new(get_dict(r)?))),
        19 => HeapObj::BoundUserMethod(r.val()?, r.val()?, r.val()?),
        20 => HeapObj::Super(r.val()?, r.val()?),
        21 => HeapObj::Property(r.val()?, r.val()?),
        22 => HeapObj::PropertySetter(r.val()?),
        23 => HeapObj::StaticMethod(r.val()?),
        24 => HeapObj::Coroutine(Box::new(Coro {
            ip: r.usz()?, slots: r.vals()?, stack: r.vals()?, body: get_body_ref(r)?,
            iters: r.seq(get_iter_frame)?, syncs: r.seq(get_sync_frame)?, excs: r.seq(get_exc_frame)?,
        })),
        25 => HeapObj::Module(r.str()?, r.seq(get_name_val)?),
        26 => {
            let name = r.str()?;
            HeapObj::Extern(externs.get(&name).ok_or_else(|| s_err("unknown native binding", &name))?.clone())
        }
        27 => HeapObj::ClassMethod(r.val()?),
        29 => HeapObj::GenericAlias(r.val()?, r.val()?),
        30 => HeapObj::TypeAlias(r.str()?, r.val()?),
        31 => HeapObj::Union(r.val()?),
        32 => HeapObj::TypeVar(r.str()?),
        33 => {
            let name = *crate::value::ITER_KINDS.get(r.u8()? as usize).ok_or("snapshot names an unknown iterator")?;
            HeapObj::Iter(Rc::new(RefCell::new(get_iter_frame(r)?)), name)
        }
        34 => HeapObj::Cell(r.val()?),
        35 => {
            let d = r.val()?;
            HeapObj::DictView(d, *[View::Keys, View::Values, View::Items].get(r.u8()? as usize).ok_or("snapshot names an unknown dict view")?)
        }
        36 => {
            let (source, filename) = (r.str()?, r.str()?);
            HeapObj::Code(Rc::new(CodeObj { source, filename, mode: code_mode(r.u8()?)? }))
        }
        t => return Err(s_err("unknown heap tag", itoa::Buffer::new().format(t))),
    })
}

/* Structural fingerprint pins the blob to its bytecode. */
pub fn fingerprint(chunk: &SSAChunk) -> u64 {
    let mut h = FxHasher::default();
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

/* Single field list drives save and restore. */
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
    stack: vals,
    iter_stack: [put_iter_frame, get_iter_frame],
    yields: vals,
    with_stack: vals,
    temp_roots: vals,
    event_queue: vals,
    scopes: [put_globals, get_globals],
    module_table: (put_map, get_map),
    is_async: [boolean], // Filled by MakeCoroutine, not chunk-derivable.
    exception_stack: [put_exc_frame, get_exc_frame],
    unwind_stack: [put_unwind, get_unwind],
    handling: [put_handled, get_handled],
    pending_sync_frames: [put_sync_frame, get_sync_frame],
    pending_exec_exc_base: opt_usz,
    pending: (put_pending, get_pending),
    scheduler: [put_handle, get_handle],
    next_host_call_id: u64,
    yielded: boolean,
    yield_from_value: val,
    builtins_module: val,
    resume_ip: usz,
    virtual_clock_ns: u64,
    error_byte_pos: opt_u32,
    output: [str],
    output_open: boolean,
    input_buffer: [str],
}

fn code_mode(b: u8) -> Result<CodeMode, SnapErr> {
    [CodeMode::Exec, CodeMode::Eval, CodeMode::Single].get(b as usize).copied().ok_or_else(|| "snapshot names an unknown code mode".to_string())
}

/* A chunk compiled at run time keeps its text, which compiles again on restore, and its fingerprint. */
fn put_compiled(w: &mut W, c: &super::globals::compile::Compiled) {
    w.str(&c.source);
    w.u8(c.form.mode as u8);
    w.boolean(c.form.closed);
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

pub fn save(vm: &VM, source: &str) -> Result<Vec<u8>, SnapErr> {
    // A pause leaves every frame saved in its coroutine, so the register stack holds nothing to keep.
    debug_assert!(vm.regs.is_empty() && vm.bindings.is_empty(), "a snapshot is taken only at a pause");
    let mut w = W::new();
    w.u32(MAGIC);
    w.u32(FORMAT);
    w.u64(fingerprint(vm.chunk));
    w.str(source);
    w.u64(vm.budget);
    w.usz(vm.heap.limit());
    w.boolean(vm.strict_input);
    w.seq(&vm.compiled, put_compiled);
    // The module each spec names, in the order the program made them, since compiled code may have made some.
    let mut specs: Vec<(&String, &usize)> = vm.scope_ids.iter().collect();
    specs.sort_unstable_by_key(|&(_, &i)| i);
    w.seq(&specs, |w, &(spec, &i)| { w.str(spec); w.usz(i); });
    put_foreign(&mut w, vm)?;
    w.seq(&vm.linked, |w, l| { w.val(l.dict); w.usz(l.scope); w.usz(l.fns.start); w.usz(l.fns.end); });
    for (slot, obj) in vm.heap.snapshot_objs().enumerate() {
        if let Some(HeapObj::List(rc)) = obj { w.lists.insert(Rc::as_ptr(rc) as usize, Val::heap(slot as u32)); }
    }
    w.usz(vm.heap.snapshot_objs().count());
    for obj in vm.heap.snapshot_objs() {
        match obj {
            None => w.u8(0),
            Some(o) => { w.u8(1); put_obj(&mut w, o); }
        }
    }
    // A dict that `vars` gave shares its instance attributes, so the restore links them.
    let mut owners: FxHashMap<usize, usize> = FxHashMap::default();
    for (slot, obj) in vm.heap.snapshot_objs().enumerate() {
        if let Some(HeapObj::Instance(_, rc)) = obj { owners.insert(Rc::as_ptr(rc) as usize, slot); }
    }
    let shared: Vec<(usize, usize)> = vm.heap.snapshot_objs().enumerate()
        .filter_map(|(slot, obj)| match obj { Some(HeapObj::Dict(rc)) => owners.get(&(Rc::as_ptr(rc) as usize)).map(|&inst| (slot, inst)), _ => None })
        .collect();
    w.seq(&shared, |w, &(dict, inst)| { w.usz(dict); w.usz(inst); });
    // The dicts and lists user classes made, in slot order so a blob stays the same.
    let mut classed: Vec<_> = vm.heap.classed_slots().collect();
    classed.sort_unstable_by_key(|(idx, _)| **idx);
    w.usz(classed.len());
    for (&idx, (cls, attrs)) in classed { w.val(Val::heap(idx)); w.val(*cls); put_dict(&mut w, &attrs.borrow()); }
    put_vm_state(&mut w, vm);
    w.seq(&vm.call_stack, put_call_frame);
    let seal = crate::util::sha256::sha256(&w.b);
    w.b.extend_from_slice(&seal);
    Ok(w.b)
}

/* Each live FOREIGN entry by kind name, version, generation and index, its payload as the kind encodes it. */
fn put_foreign(w: &mut W, vm: &VM) -> Result<(), SnapErr> {
    w.usz(vm.heap.foreign.len());
    let entries: Vec<_> = vm.heap.foreign.live_entries().collect();
    w.usz(entries.len());
    for (kind, generation, index, payload) in entries {
        let crate::value::foreign::Snapshot::Serialize { save, .. } = kind.snapshot else { return Err(s_err("cannot save a value of the kind", kind.name)) };
        let mut body = Vec::new();
        save(payload, &mut body);
        w.str(kind.name); w.u32(kind.version); w.u16(generation); w.u32(index); w.bytes(&body);
    }
    Ok(())
}

/* Puts back the FOREIGN entries, each under a kind this VM registered with the same version. */
fn get_foreign(r: &mut R, vm: &mut VM) -> Result<(), SnapErr> {
    let len = r.count()?;
    vm.heap.foreign.clear();
    for _ in 0..r.count()? {
        let (name, version, generation, index, body) = (r.str()?, r.u32()?, r.u16()?, r.u32()?, r.bytes()?);
        let kind = vm.heap.foreign.kind_named(&name).ok_or_else(|| s_err("unknown kind", &name))?;
        let k = vm.heap.foreign.kind_at(kind);
        let crate::value::foreign::Snapshot::Serialize { load, .. } = k.snapshot else { return Err(s_err("cannot restore a value of the kind", &name)) };
        if k.version != version { return Err(s_err("saved by another version of the kind", &name)); }
        let payload = load(&body).ok_or_else(|| s_err("malformed value of the kind", &name))?;
        if index as usize >= len || vm.heap.foreign.restore(kind, generation, index, crate::value::foreign::Payload::decoded(payload)).is_none() { return Err("snapshot holds a malformed value".to_string()); }
        r.foreign.insert(Val::foreign(generation, index).0);
    }
    vm.heap.foreign.settle(len);
    Ok(())
}

struct Header<'a> {
    source: &'a str,
    fingerprint: u64,
    body: usize,
    // The blob without its seal, which every reader starts from.
    data: &'a [u8],
}

fn header(blob: &[u8]) -> Result<Header<'_>, SnapErr> {
    let (data, seal) = blob.split_at(blob.len().saturating_sub(SEAL));
    let mut r = R::new(data);
    if r.u32()? != MAGIC { return Err("not an edge-python snapshot".to_string()); }
    if r.u32()? != FORMAT { return Err("unsupported snapshot format".to_string()); }
    if crate::util::sha256::sha256(data) != seal { return Err("snapshot is damaged".to_string()); }
    let fp = r.u64()?;
    let n = r.count()?;
    let start = r.p;
    let source = core::str::from_utf8(r.take(n)?).map_err(|_| "snapshot source not utf-8".to_string())?;
    Ok(Header { source, fingerprint: fp, body: start + source.len(), data })
}

/* Host re-parses the embedded source before restoring. */
pub fn source_of(blob: &[u8]) -> Result<&str, SnapErr> {
    Ok(header(blob)?.source)
}

/* Sandbox profile recorded at save time, ops is the remaining budget. */
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

fn source_index<'c>(chunk: &'c SSAChunk, out: &mut Vec<&'c SSAChunk>) {
    out.push(chunk);
    for entry in &chunk.imports {
        if let ImportKind::Code(sub) = &entry.kind { source_index(sub, out); }
    }
}

/* Overlay saved state onto a freshly booted VM. */
pub fn restore(vm: &mut VM, blob: &[u8]) -> Result<(), SnapErr> {
    let _paying = crate::value::meter::Paying::to(vm.heap.meter());
    let h = header(blob)?;
    if h.fingerprint != fingerprint(vm.chunk) {
        return Err("snapshot does not match this program or compiler version".to_string());
    }
    let mut r = R::new(h.data);
    r.p = h.body;

    // A blob may spend what it saved but never past the boot limits, the memory cap included.
    vm.budget = r.u64()?.min(vm.budget);
    vm.heap.set_budget(vm.budget);
    let _memory = r.usz()?;
    vm.strict_input = r.boolean()?;
    // Code compiled at run time parses again now, so the heap finds its natives, and joins the program once the bindings are back.
    let compiled = r.seq(|r| {
        let (source, form) = (r.str()?, super::globals::compile::Form { mode: code_mode(r.u8()?)?, closed: r.boolean()? });
        let (spec, path) = (r.str()?, alloc::sync::Arc::new(r.str()?));
        let chunk = vm.parse_compiled(&source, form, "<string>", &path).map_err(|e| e.render())?;
        if fingerprint(&chunk) != r.u64()? { return Err("snapshot does not match the code this program compiled".to_string()); }
        Ok((source, form, (!spec.is_empty()).then_some(spec), chunk))
    })?;
    let specs = r.seq(|r| Ok((r.str()?, r.usz()?)))?;
    get_foreign(&mut r, vm)?;
    let linked = r.seq(|r| Ok((r.val()?, r.usz()?, r.usz()?..r.usz()?)))?;

    let mut externs = ExternMap::default();
    collect_externs(vm.chunk, &mut externs);
    for (.., chunk) in &compiled { collect_externs(chunk, &mut externs); }
    let nslots = r.count()?;
    let mut objs: Vec<Option<HeapObj>> = Vec::with_capacity(nslots);
    let mut fills: Vec<(u32, SetFill)> = Vec::new();
    for slot in 0..nslots {
        match r.u8()? {
            0 => objs.push(None),
            _ => objs.push(Some(get_obj(&mut r, &externs, &mut fills, slot as u32)?)),
        }
    }
    r.live = Some(objs.iter().map(Option::is_some).collect());
    vm.heap.restore_objs(objs);
    for _ in 0..r.count()? {
        let mut slot = || r.usz().and_then(|n| u32::try_from(n).map(Val::heap).map_err(|_| "snapshot slot out of range".to_string()));
        let (dict, inst) = (slot()?, slot()?);
        let Some(HeapObj::Instance(_, rc)) = vm.heap.try_get(inst) else { return Err("snapshot shares the dict of a missing instance".to_string()) };
        let rc = rc.clone();
        match vm.heap.try_get_mut(dict) { Some(slot @ HeapObj::Dict(_)) => *slot = HeapObj::Dict(rc), _ => return Err("snapshot shares a missing dict".to_string()) }
    }
    for _ in 0..r.count()? {
        let (obj, cls, attrs) = (r.val()?, r.val()?, get_dict(&mut r)?);
        let fits = matches!(vm.heap.try_get(obj), Some(HeapObj::Dict(_) | HeapObj::List(_))) && matches!(vm.heap.try_get(cls), Some(HeapObj::Class(..)));
        if !fits || attrs.iter().any(|(k, v)| [k, v].iter().any(|x| x.is_heap() && vm.heap.try_get(*x).is_none())) {
            return Err("snapshot classes a missing object".to_string());
        }
        vm.heap.set_class(obj, cls, Rc::new(RefCell::new(attrs)));
    }

    get_vm_state(&mut r, vm)?;
    if specs.iter().any(|&(_, i)| i >= vm.scopes.len()) { return Err("snapshot names a missing module".to_string()); }
    vm.scope_ids.extend(specs);
    // In the order the program compiled them, so their functions take the indices the heap names.
    for (source, form, spec, chunk) in compiled { vm.adopt(&source, form, spec, chunk); }
    for (dict, scope, fns) in linked {
        if !matches!(vm.heap.try_get(dict), Some(HeapObj::Dict(_))) || scope >= vm.scopes.len() || fns.end > vm.functions.len() { return Err("snapshot links a missing dict or scope".to_string()); }
        vm.link_scope(super::globals::compile::Linked::new(dict, scope, fns));
    }
    check_objs(vm, &fills)?;
    // Each handle resumes a distinct coroutine, anything else would trap the scheduler.
    let mut coros: Vec<u64> = vm.scheduler.iter().map(|h| h.coro.0).collect();
    coros.sort_unstable();
    coros.dedup();
    if coros.len() != vm.scheduler.len() || vm.scheduler.iter().any(|h| !matches!(vm.heap.try_get(h.coro), Some(HeapObj::Coroutine(..)))) {
        return Err("snapshot schedules a missing or repeated coroutine".to_string());
    }
    vm.waiting_for_children_count = vm.scheduler.iter()
        .filter(|h| matches!(h.state, CoroState::WaitingForChildren { .. }))
        .count();

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

    if r.p != r.b.len() { return Err("snapshot has trailing bytes".to_string()); }
    // The list takes the cell its iterators hold, so they walk it and not a copy.
    for (of, cell) in core::mem::take(&mut r.lists) {
        let Some(HeapObj::List(list)) = vm.heap.try_get_mut(Val(of)) else { return Err("snapshot walks a list that is not there".to_string()) };
        *cell.borrow_mut() = core::mem::take(&mut *list.borrow_mut());
        *list = cell;
    }
    // Derived flag, not serialized, recompute from the restored module bindings.
    vm.builtins_rebound = vm.scopes.iter().flat_map(|g| g.iter()).filter_map(|(k, _)| NativeFnId::from_name(k)).fold(0, |bits, id| bits | 1 << id as u32);
    rehash(vm, fills)?;
    rebuild_mro(vm)
}

/* Rejects objects decoded before the heap with dangling refs, missing functions or stepless ranges. */
fn check_objs(vm: &VM, fills: &[(u32, SetFill)]) -> Result<(), SnapErr> {
    let nfn = vm.functions.len();
    let dangling = |v: Val| v.is_heap() && vm.heap.try_get(v).is_none();
    let mut set_items = fills.iter().flat_map(|(_, SetFill::Mutable(items) | SetFill::Frozen(items))| items);
    if set_items.any(|&v| dangling(v)) { return Err("snapshot references a missing object or function".to_string()); }
    for obj in vm.heap.snapshot_objs().flatten() {
        let mut ok = match obj {
            &HeapObj::Func(fi, ..) => fi < nfn,
            HeapObj::Coroutine(c) => !matches!(c.body, BodyRef::Fn(fi) if fi >= nfn),
            &HeapObj::Range(_, _, step) => step != 0,
            // A wide range keeps ints, its step never zero.
            &HeapObj::BigRange(s, e, st) => [s, e, st].iter().all(|&x| x.is_int() || crate::frontend::int::is_big(&vm.heap, x)) && st != Val::int(0),
            &HeapObj::DictView(d, _) => matches!(vm.heap.try_get(d), Some(HeapObj::Dict(_))),
            _ => true,
        };
        crate::value::for_each_val(obj, |v| ok &= !dangling(v));
        if !ok { return Err("snapshot references a missing object or function".to_string()); }
    }
    Ok(())
}

/* Slot order caches bases before their subclasses. */
fn rebuild_mro(vm: &mut VM) -> Result<(), SnapErr> {
    let nslots = vm.heap.snapshot_objs().count();
    for idx in 0..nslots {
        let v = Val::heap(idx as u32);
        let bases = match vm.heap.try_get(v) {
            Some(HeapObj::Class(_, bases, _)) => bases.clone(),
            _ => continue,
        };
        let tail = vm.c3_merge(&bases).map_err(|_| "snapshot class hierarchy is inconsistent".to_string())?;
        let mut mro = Vec::with_capacity(tail.len() + 1);
        mro.push(v);
        mro.extend(tail);
        vm.mro_cache.insert(v.0, Rc::new(mro));
    }
    Ok(())
}

/* Hashing reads the heap, so index once slots are live, every frozenset before what hashes it. */
fn rehash(vm: &mut VM, fills: Vec<(u32, SetFill)>) -> Result<(), SnapErr> {
    let mut frozen: FxHashMap<u32, Vec<Val>> = FxHashMap::default();
    let mut mutable = Vec::new();
    for (slot, fill) in fills {
        match fill {
            SetFill::Frozen(items) => { frozen.insert(slot, items); }
            SetFill::Mutable(items) => mutable.push((slot, items)),
        }
    }
    let mut order: Vec<u32> = frozen.keys().copied().collect();
    order.sort_unstable();
    let mut seen = FxHashSet::default();
    for slot in order { fill_frozen(vm, slot, &mut frozen, &mut seen); }
    for (slot, items) in mutable {
        let rc = match vm.heap.try_get(Val::heap(slot)) {
            Some(HeapObj::Set(rc)) => rc.clone(),
            _ => return Err("snapshot set slot mismatch".to_string()),
        };
        *rc.borrow_mut() = ValSet::from_vals(&items, &vm.heap);
    }
    let nslots = vm.heap.snapshot_objs().count();
    for idx in 0..nslots {
        let v = Val::heap(idx as u32);
        let mut dicts: Vec<Rc<RefCell<DictMap>>> = match vm.heap.try_get(v) {
            Some(HeapObj::Dict(rc)) => alloc::vec![rc.clone()],
            Some(HeapObj::Instance(class, rc)) => {
                // An attribute hiding a method or property keeps method sites checking the instance.
                vm.shadowed |= rc.borrow().iter().any(|(k, _)| matches!(vm.heap.try_get(k), Some(HeapObj::Str(n)) if vm.lookup_class_member(*class, n).is_some_and(|(m, _)| vm.hides(m))));
                alloc::vec![rc.clone()]
            }
            _ => Vec::new(),
        };
        if let Some((_, attrs)) = vm.heap.classed(v) { dicts.push(attrs.clone()); }
        for rc in dicts { rc.borrow_mut().rebuild_index(&vm.heap); }
    }
    Ok(())
}

/* Builds frozenset `root` after the frozensets its items reach through tuples, a stack in place of recursion since the blob is untrusted. */
fn fill_frozen(vm: &mut VM, root: u32, pending: &mut FxHashMap<u32, Vec<Val>>, seen: &mut FxHashSet<u32>) {
    let mut stack = alloc::vec![(root, false)];
    while let Some((slot, ready)) = stack.pop() {
        if ready {
            if let Some(items) = pending.remove(&slot) {
                let set = ValSet::from_vals(&items, &vm.heap);
                vm.heap.replace_obj(slot, HeapObj::FrozenSet(Rc::new(set)));
            }
            continue;
        }
        if !pending.contains_key(&slot) || !seen.insert(slot) { continue; }
        stack.push((slot, true));
        let mut walk: Vec<Val> = pending[&slot].clone();
        while let Some(v) = walk.pop() {
            if !v.is_heap() { continue; }
            let at = v.as_heap();
            if pending.contains_key(&at) { if !seen.contains(&at) { stack.push((at, false)); } }
            else if seen.insert(at) && let Some(HeapObj::Tuple(t)) = vm.heap.try_get(v) { walk.extend(t.iter().copied()); }
        }
    }
}

/* Appends `s` as a quoted JSON string. */
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    json_escape(out, s);
    out.push('"');
}

/* Appends `items` as a JSON array, `item` writes each element. */
fn json_array<T>(out: &mut String, items: impl IntoIterator<Item = T>, mut item: impl FnMut(&mut String, T)) {
    out.push('[');
    for (i, x) in items.into_iter().enumerate() {
        if i > 0 { out.push(','); }
        item(out, x);
    }
    out.push(']');
}

/* Module bindings as a {name: repr} JSON object. */
pub fn inspect_globals(vm: &VM) -> String {
    let mut out = String::from("{");
    let running = vm.scheduler.iter().any(|h| matches!(vm.heap.try_get(h.coro), Some(HeapObj::Coroutine(c)) if matches!(c.body, BodyRef::Module)));
    if running {
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

/* Scheduled coroutines as a JSON array. */
pub fn inspect_stack(vm: &VM) -> String {
    let fn_name = |fi: usize| -> &str {
        match vm.function_names.get(fi) {
            Some(n) if !n.is_empty() => n,
            _ => "<lambda>",
        }
    };
    let mut out = String::new();
    json_array(&mut out, &vm.scheduler, |out, h| {
        let state = match &h.state {
            CoroState::Ready => "ready",
            CoroState::Sleeping(_) => "sleeping",
            CoroState::WaitingEvent => "waiting_event",
            CoroState::WaitingHostCall(_) => "waiting_host_call",
            CoroState::WaitingForChildren { .. } => "waiting_for_children",
            CoroState::CancelPending => "cancel_pending",
            CoroState::Done(_) => "done",
            CoroState::Errored(_) => "errored",
            CoroState::Cancelled => "cancelled",
            CoroState::Raising(..) => "raising",
        };
        let (function, ip, frames) = match vm.heap.try_get(h.coro) {
            Some(HeapObj::Coroutine(c)) => {
                (match c.body { BodyRef::Module => "<module>", BodyRef::Fn(fi) => fn_name(fi) }, c.ip, &c.syncs[..])
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
