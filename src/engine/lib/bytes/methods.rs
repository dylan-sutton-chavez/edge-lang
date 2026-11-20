use alloc::vec::Vec;

use crate::engine::lang::{Acc, Args, Cx, Fold, KindId, Ret, STEPS, Sig, Step};
use crate::engine::value::{HeapObj, Val, VmErr, cold_heap};

use super::{Buf, Bytes, byte_of, is_space as blank, iterable, octets, octets_fold};
use crate::engine::lib::fail::{Class, Error, Fail, fail, receiver};
use crate::engine::lib::lang::Machine;
use crate::engine::lib::num::{ssize, word};
use crate::engine::lib::units::{self, Newline};
use crate::engine::lib::{Kernel, Lang, Site};

units::forward! { Bytes<B>, [B: Buf],
    /* whether a window starts with an affix or one of a tuple of them */
    starts,
    /* whether a window ends with an affix or one of a tuple of them */
    ends,
    /* where the first match in a window starts or minus one */
    find,
    /* where the first match in a window starts or a failure */
    index,
    /* where the last match in a window starts or minus one */
    rfind,
    /* where the last match in a window starts or a failure */
    rindex,
    /* how many matches a window holds without overlap */
    count,
    /* the bytes with their first matches up to a count replaced */
    replace,
    /* the pieces between separators or white space runs cut from the start */
    split,
    /* the pieces between separators or white space runs cut from the end */
    rsplit,
    /* white space or the argument's bytes dropped from both ends */
    trim,
    /* white space or the argument's bytes dropped from the start */
    trim_start,
    /* white space or the argument's bytes dropped from the end */
    trim_end,
    /* the items of an iterable joined by the receiver */
    join,
    /* the parts around the first separator */
    partition,
    /* the parts around the last separator */
    rpartition,
    /* the bytes without a prefix when they start with one */
    drop_prefix,
    /* the bytes without a suffix when they end with one */
    drop_suffix,
    /* the bytes centred in a width */
    center,
    /* the bytes padded at their end to a width */
    pad_end,
    /* the bytes padded at their start to a width */
    pad_start,
    /* zeros after any sign up to a width */
    zeros,
    /* each tab padded to the next stop */
    tabs,
}

/* the lines at every line feed carriage return or both */
pub const fn lines<S: Site, B: Buf>() -> Kernel { units::lines::<Bytes<B>, Newline>() }

/* the ascii letters lowercased */
pub const fn lower<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| cased::<B>(cx, a, <[u8]>::to_ascii_lowercase)) }

/* the ascii letters uppercased */
pub const fn upper<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| cased::<B>(cx, a, <[u8]>::to_ascii_uppercase)) }

/* the first byte uppercased and the rest lowercased */
pub const fn capitalize<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| cased::<B>(cx, a, capitalized)) }

/* each run of letters started uppercase and continued lowercase */
pub const fn title<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| cased::<B>(cx, a, titled)) }

/* each ascii letter turned to the other case */
pub const fn swap<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| cased::<B>(cx, a, |b| b.iter().map(|&c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }).collect())) }

/* whether the bytes are nonempty and all ascii letters or digits */
pub const fn is_alnum<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| every(b, u8::is_ascii_alphanumeric))) }

/* whether the bytes are nonempty and all ascii letters */
pub const fn is_alpha<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| every(b, u8::is_ascii_alphabetic))) }

/* whether every byte is ascii */
pub const fn is_ascii<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, <[u8]>::is_ascii)) }

/* whether the bytes are nonempty and all ascii digits */
pub const fn is_digit<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| every(b, u8::is_ascii_digit))) }

/* whether the bytes are nonempty and all white space */
pub const fn is_space<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| every(b, |&c| blank(c)))) }

/* whether the bytes hold a lowercase letter and no uppercase one */
pub const fn is_lower<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| b.iter().any(u8::is_ascii_lowercase) && !b.iter().any(u8::is_ascii_uppercase))) }

/* whether the bytes hold an uppercase letter and no lowercase one */
pub const fn is_upper<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| b.iter().any(u8::is_ascii_uppercase) && !b.iter().any(u8::is_ascii_lowercase))) }

/* whether each run of letters starts uppercase and goes on lowercase */
pub const fn is_title<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| test(cx, a, |b| b.iter().any(u8::is_ascii_alphabetic) && titled(b) == b)) }

/* adds one byte at the end of a mutable buffer */
pub const fn push<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), push_run::<S>) }

/* adds the bytes of a bytes-like value or of iterable ints at the end */
pub const fn extend<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), extend_run::<S>).flags(STEPS) }

/* removes and gives the byte at an index or the last */
pub const fn pop<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), pop_run::<S>) }

/* puts one byte before an index */
pub const fn insert<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), insert_run::<S>) }

/* removes the first byte equal to a value */
pub const fn remove<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), remove_run::<S>) }

/* removes every byte */
pub const fn clear<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), clear_run) }

/* turns the bytes around in place */
pub const fn reverse<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), reverse_run) }

/* a new mutable buffer of the same bytes */
pub const fn copy<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), copy_run) }

/* cuts to a size or grows with zero bytes */
pub const fn resize<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), resize_run::<S>) }

/* the first bytes cut out as immutable bytes with a negative count from the end */
pub const fn take<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), take_run::<S>) }

/* the receiver's bytes charged as native work for their length */
#[inline]
pub(crate) fn recv<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Result<&'a [u8], VmErr> {
    let b = cx.bytes_of(v).ok_or_else(receiver)?;
    cx.spend(b.len());
    Ok(b)
}

/* a bytes-like argument charged for its length or the failure naming its type */
#[inline]
pub(crate) fn like<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Result<&'a [u8], Error> {
    let b = cx.bytes_of(v).ok_or(Error::Fail(Fail::BufferArg(v)))?;
    cx.spend(b.len());
    Ok(b)
}

/* what a search looks for where an int stands for the one byte it is */
pub(crate) fn needle<'a>(cx: &'a Cx<'_, '_>, v: Val, one: &'a mut [u8; 1]) -> Result<&'a [u8], Error> {
    if let Some(b) = cx.bytes_of(v) { return Ok(b); }
    let n = match word(cx, v) {
        Some(n) => n.unwrap_or(i64::MAX),
        None => return Err(Error::Fail(Fail::ByteArg(v))),
    };
    one[0] = u8::try_from(n).map_err(|_| Error::Fail(Fail::ByteRange))?;
    Ok(&one[..])
}

/* the receiver's bytes cased through `f` as immutable bytes or as a mutable buffer when the receiver is one */
#[inline]
fn cased<B: Buf>(cx: &mut Cx<'_, '_>, a: Args<'_>, f: impl FnOnce(&[u8]) -> Vec<u8>) -> Ret {
    let buf = f(recv(cx, a.recv)?);
    Ok(cx.new_bytes(buf, B::MUTABLE)?.into())
}

/* `b` with its first byte uppercased and the rest lowercased */
fn capitalized(b: &[u8]) -> Vec<u8> {
    let mut buf = b.to_ascii_lowercase();
    if let Some(first) = buf.first_mut() { first.make_ascii_uppercase(); }
    buf
}

/* `b` with each run of letters started uppercase and continued lowercase */
fn titled(b: &[u8]) -> Vec<u8> {
    let mut prev = false;
    b.iter().map(|&c| { let out = if prev { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() }; prev = c.is_ascii_alphabetic(); out }).collect()
}

/* a test over the receiver's bytes */
fn test(cx: &mut Cx<'_, '_>, a: Args<'_>, f: impl Fn(&[u8]) -> bool) -> Ret { Ok(Val::bool(f(recv(cx, a.recv)?)).into()) }

// class tests need a byte and case tests need a cased one
fn every(b: &[u8], f: fn(&u8) -> bool) -> bool { !b.is_empty() && b.iter().all(f) }

/* the mutable buffer `recv` changed in place through `f` */
fn with_array<R>(cx: &mut Cx<'_, '_>, recv: Val, f: impl FnOnce(&mut Vec<u8>) -> R) -> Result<R, VmErr> {
    match cx.heap_mut().try_get_mut(recv) {
        Some(HeapObj::ByteArray(b)) => Ok(f(b)),
        _ => Err(receiver()),
    }
}

/* the mutable buffer `recv` grown by `more` once the memory left holds them */
pub(crate) fn extended(cx: &mut Cx<'_, '_>, recv: Val, more: &[u8]) -> Result<(), VmErr> {
    cx.reserve(more.len())?;
    // a long extension counts the bytes it copies as a long text does
    if more.len() > 64 { cx.spend(more.len()); }
    with_array(cx, recv, |b| b.extend_from_slice(more))
}

fn push_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let byte = byte_of(cx, a.pos[0], false).map_err(|e| e.into_vm::<S>(cx))?;
    with_array(cx, a.recv, |b| b.push(byte))?;
    Ok(Val::none().into())
}

/* bytes-like bytes or iterable ints added where a bad int leaves the receiver whole */
fn extend_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let it = a.pos[0];
    let more = match cx.bytes_of(it) {
        Some(b) => b.to_vec(),
        None => {
            if cx.text(it).is_some() { return Err(fail::<S>(cx, Fail::ExtendText)); }
            if !iterable(cx, it) { return Err(fail::<S>(cx, Fail::ExtendType { recv: a.recv, v: it })); }
            let Some(more) = octets(cx, it, false).map_err(|e| e.into_vm::<S>(cx))? else {
                let mut acc = Acc::new();
                (acc.n, acc.v[0]) = (2, a.recv);
                return Ok(Step::Drain { it, fold: Fold::with(octets_fold::<S>, acc) }.into());
            };
            more
        }
    };
    extended(cx, a.recv, &more)?;
    Ok(Val::none().into())
}

fn pop_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let i = match a.get(0) { Some(v) => ssize(cx, v).map_err(|e| e.into_vm::<S>(cx))?, None => -1 };
    let popped = with_array(cx, a.recv, |b| {
        if b.is_empty() { return Err(true); }
        let at = if i < 0 { i.saturating_add(b.len() as i64) } else { i };
        let at = usize::try_from(at).ok().filter(|&at| at < b.len()).ok_or(false)?;
        Ok((b.remove(at), b.len() - at))
    })?;
    let (byte, moved) = match popped {
        Ok(p) => p,
        Err(true) => return Err(fail::<S>(cx, Fail::EmptyPop(a.recv))),
        Err(false) => return Err(fail::<S>(cx, Fail::PopRange)),
    };
    // the bytes past the slot shift as native work
    cx.spend(moved);
    Ok(Val::int(byte as i64).into())
}

/* one byte at an index clamped to the ends as a list insert is */
fn insert_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let i = ssize(cx, a.pos[0]).map_err(|e| e.into_vm::<S>(cx))?;
    let byte = byte_of(cx, a.pos[1], false).map_err(|e| e.into_vm::<S>(cx))?;
    let moved = with_array(cx, a.recv, |b| {
        let n = b.len() as i64;
        let at = (if i < 0 { i.saturating_add(n) } else { i }).clamp(0, n) as usize;
        b.insert(at, byte);
        b.len() - at
    })?;
    cx.spend(moved);
    Ok(Val::none().into())
}

/* the first byte equal to the value taken out */
fn remove_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let byte = byte_of(cx, a.pos[0], false).map_err(|e| e.into_vm::<S>(cx))?;
    let moved = with_array(cx, a.recv, |b| {
        let at = b.iter().position(|&x| x == byte)?;
        b.remove(at);
        Some(b.len() - at)
    })?;
    let Some(moved) = moved else { return Err(fail::<S>(cx, Fail::ValueNotFound(a.recv))) };
    cx.spend(moved);
    Ok(Val::none().into())
}

fn clear_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    with_array(cx, a.recv, Vec::clear)?;
    Ok(Val::none().into())
}

fn reverse_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let n = with_array(cx, a.recv, |b| { b.reverse(); b.len() })?;
    cx.spend(n);
    Ok(Val::none().into())
}

fn copy_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let b = recv(cx, a.recv)?.to_vec();
    Ok(cx.new_bytes(b, true)?.into())
}

/* cut to a size or grown with zero bytes */
fn resize_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let size = ssize(cx, a.pos[0]).map_err(|e| e.into_vm::<S>(cx))?;
    if size < 0 { return Err(fail::<S>(cx, Fail::Resize(size))); }
    let size = usize::try_from(size).map_err(|_| cold_heap())?;
    cx.reserve(size)?;
    // the zeros a growth writes count as native work
    let grown = with_array(cx, a.recv, |b| { let n = size.saturating_sub(b.len()); b.resize(size, 0); n })?;
    if grown > 64 { cx.spend(grown); }
    Ok(Val::none().into())
}

/* the first bytes cut out as immutable bytes with a negative count from the end */
fn take_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let n = match a.get(0).filter(|v| !v.is_none()) {
        None => None,
        Some(v) if !(v.is_int() || crate::engine::lib::num::bool_int(cx.heap(), v).is_some() || cx.kind(v) == KindId::Int) => return Err(fail::<S>(cx, Fail::TakeType)),
        Some(v) => {
            let n = super::count(cx, v);
            if let Some(m) = <S::Lang as Lang>::MACHINE && !Machine::fits(n, m.index) { return Err(fail::<S>(cx, Fail::IndexSized(Class::Index))); }
            // a count past the word asks more bytes than any buffer holds
            Some(i64::try_from(n).unwrap_or(if n < 0 { i64::MIN } else { i64::MAX }))
        }
    };
    let taken = with_array(cx, a.recv, |buf| {
        let len = buf.len() as i64;
        let take = match n { None => len, Some(k) if k < 0 => len.saturating_add(k), Some(k) => k };
        if !(0..=len).contains(&take) { return Err((take, len)); }
        Ok((buf.drain(..take as usize).collect::<Vec<u8>>(), len as usize))
    })?;
    let (taken, len) = match taken {
        Ok(t) => t,
        Err((take, len)) => return Err(fail::<S>(cx, Fail::TakeRange { take, len })),
    };
    cx.spend(len);
    Ok(cx.new_bytes(taken, false)?.into())
}
