use alloc::string::String;

use edge::lang::{Args, Cx, KindId, Language, Mode, Ret, Sig, TYPE};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::fail::{Error, Fail, fail, receiver};
use crate::fmt::render::{foreign_owned, text_fast, text_val};
use crate::units::{Family, Role};
use crate::{Kernel, Site};

pub mod case;
pub mod class;
pub mod cut;
pub mod edit;
pub mod find;
pub mod pad;

/* the text of a value or of bytes decoded by the codec a call names */
pub const fn text<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 3), text_run::<S>).fast(text_fast::<S::Lang>).flags(TYPE) }

fn text_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    if a.len() > 1 { return decoded::<S>(cx, a); }
    let Some(v) = a.get(0) else { return Ok(cx.new_str(String::new())?.into()) };
    if let Some(s) = text_fast::<S::Lang>(cx.heap_mut(), v) { return Ok(s.into()); }
    if let Some(text) = foreign_owned::<S::Lang>(cx, v, Mode::Str) { return Ok(cx.new_str(text?)?.into()); }
    let out = <S::Lang as Language>::show(cx, v, Mode::Str)?;
    Ok(cx.new_str(out)?.into())
}

/* the text bytes decode to through the codec and handler the call names */
fn decoded<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let text = {
        let mut names: [Option<String>; 2] = [None, None];
        for (i, slot) in names.iter_mut().enumerate() {
            let Some(v) = a.get(i + 1) else { continue };
            match cx.text(v) {
                Some(t) => *slot = Some(String::from(t)),
                None => return Err(fail::<S>(cx, Fail::ParamType { at: i as u8 + 1, v, want: edge::lang::KindId::Str })),
            }
        }
        let buf: alloc::vec::Vec<u8> = match a.get(0) {
            Some(v) => match cx.bytes_of(v) { Some(b) => b.to_vec(), None => return Err(fail::<S>(cx, Fail::DecodeNeedsBuffer(v))) },
            // with no object the codec reads empty bytes
            None => alloc::vec::Vec::new(),
        };
        cx.spend(buf.len());
        crate::bytes::codec::decode_for::<S>(cx, &buf, names[0].as_deref(), names[1].as_deref())?
    };
    Ok(cx.new_str(text)?.into())
}

/* the text of one code point */
pub const fn char_of<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), char_of_run::<S>).fast(char_of_fast) }

/* the code point of one character or the value of one byte */
pub const fn code_of<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), code_of_run::<S>).fast(code_of_fast) }

/* the receiver's text charged as native work for its length */
#[inline]
pub(crate) fn recv_str<'a>(cx: &'a Cx<'_, '_>, recv: Val) -> Result<&'a str, VmErr> { cx.str_of(recv).ok_or_else(receiver) }

pub(crate) fn new_str(cx: &mut Cx<'_, '_>, s: String) -> Ret { Ok(cx.new_str(s)?.into()) }

/* text as the units kernels read where a unit is a character */
pub struct Text;

impl Family for Text {
    type Hay = str;
    const KIND: KindId = KindId::Str;
    const SHARES: bool = true;

    #[inline(always)]
    fn ascii(cx: &mut Cx<'_, '_>, v: Val) -> bool { cx.is_ascii(v) }

    #[inline(always)]
    fn recv<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Result<&'a str, VmErr> { recv_str(cx, v) }

    #[inline(always)]
    fn arg<'a>(cx: &'a Cx<'_, '_>, v: Val, role: Role) -> Result<&'a str, Error> {
        cx.str_of(v).ok_or(Error::Fail(match role {
            Role::Old => Fail::TextArg { at: 1, v },
            Role::New => Fail::TextArg { at: 2, v },
            Role::Sep => Fail::SepText(v),
            Role::Part => Fail::MustBeText(v),
            Role::Affix => Fail::NeedText(v),
            Role::Chars => Fail::StripArg,
            Role::Item => Fail::AffixItem { v, want: KindId::Str },
        }))
    }

    #[inline(always)]
    fn needle<'a>(cx: &'a Cx<'_, '_>, v: Val, _: &'a mut [u8; 1]) -> Result<&'a [u8], Error> { Ok(Self::arg(cx, v, Role::Old)?.as_bytes()) }

    #[inline(always)]
    fn fill(cx: &Cx<'_, '_>, v: Option<Val>) -> Result<char, Error> {
        let Some(f) = v else { return Ok(' ') };
        let mut cs = cx.str_of(f).ok_or(Error::Fail(Fail::FillText(f)))?.chars();
        match (cs.next(), cs.next()) { (Some(c), None) => Ok(c), _ => Err(Error::Fail(Fail::TextFill)) }
    }

    #[inline(always)]
    fn part<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Option<&'a str> { match cx.heap().try_get(v) { Some(HeapObj::Str(s)) => Some(s.as_str()), _ => None } }

    #[inline(always)]
    fn give(cx: &mut Cx<'_, '_>, out: String) -> Result<Val, VmErr> { cx.new_str(out) }

    #[inline(always)]
    fn keep(_: &mut Cx<'_, '_>, v: Val) -> Result<Val, VmErr> { Ok(v) }

    #[inline(always)]
    fn missing() -> Fail { Fail::SubstringNotFound }
}

/* the lowercase of `c` where ascii skips the case tables */
#[inline]
pub(crate) fn push_lower(out: &mut String, c: char) { if c.is_ascii() { out.push(c.to_ascii_lowercase()); } else { out.extend(c.to_lowercase()); } }

/* the uppercase of `c` where ascii skips the case tables */
#[inline]
pub(crate) fn push_upper(out: &mut String, c: char) { if c.is_ascii() { out.push(c.to_ascii_uppercase()); } else { out.extend(c.to_uppercase()); } }

fn char_of_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let x = a.pos[0];
    if let Some(v) = char_of_fast(cx.heap_mut(), x) { return Ok(v.into()); }
    if !(x.is_int() || crate::num::bool_int(cx.heap(), x).is_some() || cx.heap().is_wide(x)) { return Err(fail::<S>(cx, Fail::NotIndex(x))); }
    let Some(c) = cx.int(x).and_then(|i| u32::try_from(i).ok()).and_then(char::from_u32) else { return Err(fail::<S>(cx, Fail::CodePoint)) };
    Ok(cx.new_str(String::from(c))?.into())
}

/* the text of an inline int or bool naming a code point */
fn char_of_fast(heap: &mut HeapPool, x: Val) -> Option<Val> {
    let n = if x.is_int() { x.as_int() } else { crate::num::bool_int(heap, x)? };
    let c = char::from_u32(u32::try_from(n).ok()?)?;
    let mut buf = [0u8; 4];
    text_val(heap, c.encode_utf8(&mut buf))
}

fn code_of_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let x = a.pos[0];
    if let Some(v) = code_of_fast(cx.heap_mut(), x) { return Ok(v.into()); }
    let found = match cx.heap().try_get(x) {
        Some(HeapObj::Str(s)) => s.chars().count(),
        _ => match cx.bytes_of(x) {
            Some(b) => b.len(),
            None => return Err(fail::<S>(cx, Fail::CharKind(x))),
        },
    };
    Err(fail::<S>(cx, Fail::CharLength(found)))
}

/* the code point of one character or the value of one byte */
fn code_of_fast(heap: &mut HeapPool, x: Val) -> Option<Val> {
    match heap.try_get(x)? {
        HeapObj::Str(s) => {
            if let &[b] = s.as_bytes() { return Some(Val::int(i64::from(b))); }
            let mut cs = s.chars();
            let c = cs.next()?;
            cs.as_str().is_empty().then(|| Val::int(i64::from(u32::from(c))))
        }
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => match b.as_slice() { &[b] => Some(Val::int(i64::from(b))), _ => None },
        _ => None,
    }
}
