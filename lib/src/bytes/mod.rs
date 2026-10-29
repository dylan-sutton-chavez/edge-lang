use alloc::{vec, vec::Vec};

use edge::lang::{Acc, Args, Cx, Flow, Fold, Item, KindId, Ret, STEPS, Sig, Step, TYPE};
use edge::value::{HeapObj, HeapPool, Val, VmErr, cold_heap};

use crate::iter::items::SHORT;
use crate::fail::{Class, Error, Fail, fail};
use crate::lang::Machine;
use crate::units::{Family, Role};
use crate::{Kernel, Lang, Site};

pub mod codec;
pub mod hex;
pub mod methods;

/* whether a buffer's kernels give back mutable buffers */
pub trait Buf: 'static { const MUTABLE: bool; }

/* a buffer whose results are immutable bytes */
pub struct Fixed;

/* a buffer whose results are mutable buffers */
pub struct Mutable;

impl Buf for Fixed { const MUTABLE: bool = false; }

impl Buf for Mutable { const MUTABLE: bool = true; }

/* bytes as the units kernels read where `B` tells whether results are mutable buffers */
pub struct Bytes<B>(core::marker::PhantomData<B>);

impl<B: Buf> Family for Bytes<B> {
    type Hay = [u8];
    const KIND: KindId = KindId::Bytes;
    const SHARES: bool = !B::MUTABLE;

    #[inline(always)]
    fn ascii(_: &mut Cx<'_, '_>, _: Val) -> bool { true }

    #[inline(always)]
    fn recv<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Result<&'a [u8], VmErr> { methods::recv(cx, v) }

    #[inline(always)]
    fn arg<'a>(cx: &'a Cx<'_, '_>, v: Val, _: Role) -> Result<&'a [u8], Error> { methods::like(cx, v) }

    #[inline(always)]
    fn needle<'a>(cx: &'a Cx<'_, '_>, v: Val, one: &'a mut [u8; 1]) -> Result<&'a [u8], Error> { methods::needle(cx, v, one) }

    #[inline(always)]
    fn fill(cx: &Cx<'_, '_>, v: Option<Val>) -> Result<u8, Error> {
        match v {
            None => Ok(b' '),
            Some(f) => match cx.bytes_of(f) { Some(&[b]) => Ok(b), _ => Err(Error::Fail(Fail::BufferFill(f))) },
        }
    }

    #[inline(always)]
    fn part<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Option<&'a [u8]> { cx.bytes_of(v) }

    #[inline(always)]
    fn give(cx: &mut Cx<'_, '_>, out: Vec<u8>) -> Result<Val, VmErr> { cx.new_bytes(out, B::MUTABLE) }

    #[inline(always)]
    fn keep(cx: &mut Cx<'_, '_>, v: Val) -> Result<Val, VmErr> {
        if !B::MUTABLE { return Ok(v); }
        let b = cx.bytes_of(v).unwrap_or_default().to_vec();
        cx.new_bytes(b, true)
    }

    #[inline(always)]
    fn missing() -> Fail { Fail::SubsectionNotFound }
}

/* zeros or a copy or byte ints or encoded text as the buffer `B` names */
pub const fn new<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::range(0, 3), new_run::<S, B>).fast(new_fast::<B>).flags(TYPE | STEPS) }

fn new_run<S: Site, B: Buf>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mutable = B::MUTABLE;
    let (source, encoding, errors) = (a.get(0), a.get(1), a.get(2));
    for (v, at) in [(encoding, 1), (errors, 2)] {
        if let Some(v) = v && cx.text(v).is_none() { return Err(fail::<S>(cx, Fail::ParamType { at, v, want: KindId::Str })); }
    }
    let Some(src) = source else {
        if encoding.is_some() { return Err(fail::<S>(cx, Fail::ParamWithoutText(1))); }
        if errors.is_some() { return Err(fail::<S>(cx, Fail::ParamWithoutText(2))); }
        return Ok(cx.new_bytes(Vec::new(), mutable)?.into());
    };
    if encoding.is_some() || errors.is_some() {
        let buf: Result<Vec<u8>, Error> = {
            let cx = &*cx;
            match (cx.str_of(src), encoding) {
                (Some(text), Some(e)) => codec::encode_with::<S::Lang>(text, cx.text(e), errors.and_then(|v| cx.text(v)), cx.room()),
                (Some(_), None) => Err(Error::Fail(Fail::TextWithoutCodec)),
                (None, Some(_)) => Err(Error::Fail(Fail::ParamWithoutText(1))),
                (None, None) => Err(Error::Fail(Fail::ParamWithoutText(2))),
            }
        };
        let buf = buf.map_err(|e| e.raise::<S>(cx))?;
        if buf.len() > 64 { cx.spend(buf.len()); }
        return Ok(cx.new_bytes(buf, mutable)?.into());
    }
    // bytes are immutable so one given whole is its own copy
    if !mutable && matches!(cx.heap().try_get(src), Some(HeapObj::Bytes(_))) { return Ok(src.into()); }
    if src.is_int() || crate::num::bool_int(cx.heap(), src).is_some() || cx.kind(src) == KindId::Int {
        let n = count(cx, src);
        if let Some(m) = <S::Lang as Lang>::MACHINE && !Machine::fits(n, m.index) { return Err(fail::<S>(cx, Fail::IndexSized(Class::Overflow))); }
        if n < 0 { return Err(fail::<S>(cx, Fail::NegativeCount)); }
        // a count no buffer can hold is refused before the zeros exist
        let n = usize::try_from(n).map_err(|_| cold_heap())?;
        cx.reserve(n)?;
        if n > 4096 { cx.spend(n / 64); }
        return Ok(cx.new_bytes(vec![0; n], mutable)?.into());
    }
    let buf = if let Some(b) = cx.bytes_of(src) {
        // a long copy is refused past the memory left and counts the bytes it reads
        if b.len() > SHORT { cx.reserve(b.len())?; }
        if b.len() > 64 { cx.spend(b.len()); }
        b.to_vec()
    } else {
        if cx.text(src).is_some() { return Err(fail::<S>(cx, Fail::TextWithoutCodec)); }
        if !iterable(cx, src) { return Err(fail::<S>(cx, Fail::CannotConvert(src))); }
        match octets(cx, src, !mutable).map_err(|e| e.raise::<S>(cx))? {
            Some(buf) => buf,
            None => {
                let mut acc = Acc::new();
                acc.n = mutable as i64;
                return Ok(Step::Drain { it: src, fold: Fold::with(octets_fold::<S>, acc) }.into());
            }
        }
    };
    Ok(cx.new_bytes(buf, mutable)?.into())
}

/* an int count where a wide int past an i128 saturates by its sign */
#[inline]
pub(crate) fn count(cx: &Cx<'_, '_>, v: Val) -> i128 {
    match cx.int(v) { Some(n) => n, None if crate::num::negative(cx, v) => i128::MIN, None => i128::MAX }
}

/* the byte ints of an iterable or none when stepping it runs user code */
pub(crate) fn octets(cx: &mut Cx<'_, '_>, it: Val, plural: bool) -> Result<Option<Vec<u8>>, Error> {
    // a list or tuple reads in place with no copy of its items
    let len = match cx.heap().try_get(it) { Some(HeapObj::List(rc)) => Some(rc.borrow().len()), Some(HeapObj::Tuple(t)) => Some(t.len()), _ => None };
    if let Some(len) = len {
        cx.charge(len)?;
        let cx = &*cx;
        return match cx.heap().try_get(it) {
            Some(HeapObj::List(rc)) => rc.borrow().iter().map(|&v| byte_of(cx, v, plural)).collect::<Result<_, _>>().map(Some),
            Some(HeapObj::Tuple(t)) => t.iter().map(|&v| byte_of(cx, v, plural)).collect::<Result<_, _>>().map(Some),
            _ => Ok(Some(Vec::new())),
        };
    }
    let Some(items) = cx.items(it)? else { return Ok(None) };
    items.into_iter().map(|v| byte_of(cx, v, plural)).collect::<Result<_, _>>().map(Some)
}

/* a drain of byte ints that stops at the first bad one */
pub(crate) fn octets_fold<S: Site>(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => {
            let b = byte_of(cx, v, acc.n == 0).map_err(|e| e.raise::<S>(cx))?;
            if (acc.items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= cx.room() { return Err(cold_heap()); }
            acc.items.push(Val::int(b as i64));
            Ok(Flow::More)
        }
        Item::End(_) => {
            let buf: Vec<u8> = acc.items.iter().map(|v| v.as_int() as u8).collect();
            // `n` 0 makes bytes 1 a mutable buffer and 2 extends the buffer in `v[0]`
            if acc.n < 2 { return Ok(Flow::Stop(cx.new_bytes(buf, acc.n == 1)?)); }
            methods::extended(cx, acc.v[0], &buf)?;
            Ok(Flow::Stop(Val::none()))
        }
        Item::Called(_) => Ok(Flow::More),
    }
}

/* `v` as one byte where `plural` words the range for immutable bytes */
#[inline]
pub(crate) fn byte_of(cx: &Cx<'_, '_>, v: Val, plural: bool) -> Result<u8, Error> {
    let n = if v.is_int() { v.as_int() as i128 } else if let Some(b) = crate::num::bool_int(cx.heap(), v) { b as i128 } else if cx.kind(v) == KindId::Int { cx.int(v).unwrap_or(i128::MAX) } else {
        return Err(Error::Fail(Fail::NotIndex(v)));
    };
    u8::try_from(n).map_err(|_| Error::Fail(if plural { Fail::BytesRange } else { Fail::ByteRange }))
}

/* whether walking `v` yields items rather than failing at once */
pub(crate) fn iterable(cx: &Cx<'_, '_>, v: Val) -> bool {
    match cx.kind(v) {
        KindId::Str | KindId::Bytes | KindId::ByteArray | KindId::List | KindId::Tuple | KindId::Dict | KindId::Set | KindId::Range | KindId::Iter => true,
        KindId::Coro => cx.is_generator(v),
        // a foreign kind steps when it gives a frame
        KindId::Foreign => cx.heap().foreign_kind(v).is_some_and(|k| k.iter.is_some()),
        _ => false,
    }
}

/* the white space of bytes which takes the vertical tab and form feed too */
pub(crate) fn is_space(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) }

/* the buffer of a count bytes or byte ints answered with no call */
fn new_fast<B: Buf>(heap: &mut HeapPool, v: Val) -> Option<Val> {
    if !B::MUTABLE && matches!(heap.try_get(v), Some(HeapObj::Bytes(_))) { return Some(v); }
    let buf = plain(heap, v)?;
    heap.alloc(if B::MUTABLE { HeapObj::ByteArray(buf) } else { HeapObj::Bytes(buf) }).ok()
}

/* a fitting count or a bytes-like value or byte ints of a list or tuple */
fn plain(heap: &HeapPool, v: Val) -> Option<Vec<u8>> {
    if v.is_int() {
        let n = usize::try_from(v.as_int()).ok()?;
        heap.reserve(n).ok()?;
        // zeros fill as fast as a copy runs so each 64 bytes count once
        if n > 4096 { heap.spend(n / 64); }
        return Some(vec![0; n]);
    }
    let ints = |items: &[Val]| -> Option<Vec<u8>> {
        heap.spend(items.len());
        items.iter().map(|v| if v.is_int() { u8::try_from(v.as_int()).ok() } else { None }).collect()
    };
    match heap.try_get(v)? {
        // a copy is refused before it is built and pays for its bytes
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => {
            if b.len() > SHORT { heap.reserve(b.len()).ok()?; }
            if b.len() > 64 { heap.spend(b.len()); }
            Some(b.clone())
        }
        HeapObj::List(rc) => ints(&rc.borrow()),
        HeapObj::Tuple(t) => ints(t),
        _ => None,
    }
}
