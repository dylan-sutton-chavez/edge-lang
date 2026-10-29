use alloc::{string::String, vec::Vec};

use edge::lang::{Args, CLASS, Cx, Ret, Sig};
use edge::value::{HeapObj, HeapPool, Val};

use super::{Buf, is_space, methods::recv};
use crate::num::ssize;
use crate::fail::{Error, Fail};
use crate::{Kernel, Site};

/* lowercase hex with a separator between groups counted from either end */
pub const fn hex<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::range(0, 2), hex_run::<S>) }

/* the bytes hex digit pairs spell which reads off the type itself */
pub const fn from_hex<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::exact(1), from_hex_run::<S, B>).flags(CLASS) }

/* the bytes hex digit pairs spell as a function of the digits */
pub const fn from_hex_fn<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), from_hex_run::<S, super::Fixed>).fast(from_hex_fast) }

fn hex_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let cx = &*cx;
        (|| -> Result<String, Error> {
            let sep = match a.get(0) {
                None => None,
                Some(v) => {
                    // a text separator counts characters so one past ascii is refused for being so
                    let unit = match cx.text(v) {
                        Some(t) => { let mut cs = t.chars(); match (cs.next(), cs.next()) { (Some(c), None) => Some(c as u32), _ => None } }
                        None => match cx.bytes_of(v) { Some(&[b]) => Some(b as u32), Some(_) => None, None => return Err(Error::Fail(Fail::NoLength(v))) },
                    };
                    let Some(unit) = unit else { return Err(Error::Fail(Fail::ParamLength(0))) };
                    if unit > 0x7F { return Err(Error::Fail(Fail::ParamAscii(0))); }
                    Some(unit as u8 as char)
                }
            };
            let group = match a.get(1) { Some(v) => ssize(cx, v)?, None => 1 };
            // a group wider than any buffer marks no boundary on any target
            let width = usize::try_from(group.unsigned_abs()).unwrap_or(usize::MAX);
            let len = cx.bytes_of(a.recv).map_or(0, <[u8]>::len);
            let marks = if sep.is_some() && width > 0 { len.saturating_sub(1) / width } else { 0 };
            let size = len.saturating_mul(2).saturating_add(marks);
            // a long text is refused past the memory left before it exists or is charged
            if size > crate::iter::items::SHORT { cx.reserve(size)?; }
            let buf = recv(cx, a.recv)?;
            let mut out = String::with_capacity(size);
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for (i, &b) in buf.iter().enumerate() {
                // a group ends where the bytes before it divide by its size
                let before = if group < 0 { i } else { buf.len() - i };
                if let Some(c) = sep && i > 0 && width > 0 && before % width == 0 { out.push(c); }
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
            Ok(out)
        })()
    };
    let out = out.map_err(|e| e.raise::<S>(cx))?;
    Ok(cx.new_str(out)?.into())
}

fn from_hex_run<S: Site, B: Buf>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let buf = unhexed(cx, a.pos[0]).map_err(|e| e.raise::<S>(cx))?;
    Ok(cx.new_bytes(buf, B::MUTABLE)?.into())
}

/* the bytes a text or bytes-like value of hex digit pairs spells */
fn unhexed(cx: &Cx<'_, '_>, v: Val) -> Result<Vec<u8>, Error> {
    let digits = match cx.str_of(v) {
        // a text counts characters so its first one past ascii is where it fails
        Some(s) => match s.chars().position(|c| !c.is_ascii()) { Some(at) => return Err(Error::Fail(Fail::HexDigit(at))), None => s.as_bytes() },
        None => {
            let Some(b) = cx.bytes_of(v) else { return Err(Error::Fail(Fail::HexArg(v))) };
            cx.spend(b.len());
            b
        }
    };
    unhex(digits).map_err(|at| Error::Fail(match at { Some(at) => Fail::HexDigit(at), None => Fail::HexOdd }))
}

/* hex digit pairs with white space between them where an odd count fails */
fn unhex(d: &[u8]) -> Result<Vec<u8>, Option<usize>> {
    let digit = |b: u8| (b as char).to_digit(16).filter(|_| b.is_ascii()).map(|n| n as u8);
    let (mut out, mut i) = (Vec::with_capacity(d.len() / 2), 0);
    while i < d.len() {
        if is_space(d[i]) { i += 1; continue; }
        let hi = digit(d[i]).ok_or(Some(i))?;
        let lo = digit(*d.get(i + 1).ok_or(None)?).ok_or(Some(i + 1))?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/* the bytes of plain hex text answered with no call */
fn from_hex_fast(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let digits: &[u8] = match heap.try_get(v)? {
        HeapObj::Str(s) => s.as_bytes(),
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b,
        _ => return None,
    };
    heap.spend(digits.len());
    let buf = unhex(digits).ok()?;
    heap.alloc(HeapObj::Bytes(buf)).ok()
}
