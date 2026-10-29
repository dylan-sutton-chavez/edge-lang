use alloc::{format, string::String, vec::Vec};

use edge::lang::{Args, Cx, KindId, Ret, Sig};
use edge::value::VmErr;

use super::Buf;
use crate::fail::{Coding, Error, Fail};
use crate::lang::{Codec, OnError, Order};
use crate::{Kernel, Lang, Site};

/* the text as bytes in the codec and handler a call names */
pub const fn encode<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 2), encode_run::<S>) }

/* the bytes read as text in the codec and handler a call names */
pub const fn decode<S: Site, B: Buf>() -> Kernel { Kernel::new(Sig::range(0, 2), decode_run::<S>) }

// what windows-1252 reads bytes 0x80 to 0x9f as with 0 where it reads nothing
const WINDOWS_1252: [u16; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0, 0x017D, 0,
    0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

/* the byte a single byte codec writes `c` as or none when it has none */
fn byte(codec: Codec, c: char) -> Option<u8> {
    let n = c as u32;
    match codec {
        Codec::Ascii => (n < 0x80).then_some(n as u8),
        Codec::Latin1 => (n < 0x100).then_some(n as u8),
        Codec::Windows1252 if n < 0x80 || (0xA0..0x100).contains(&n) => Some(n as u8),
        Codec::Windows1252 => WINDOWS_1252.iter().position(|&m| m != 0 && m as u32 == n).map(|i| 0x80 + i as u8),
        _ => None,
    }
}

/* the codec the runtime names or utf-8 when none is named */
fn codec<L: Lang>(name: Option<&str>) -> Result<Codec, Error> {
    match name {
        None => Ok(Codec::Utf8),
        Some(n) => L::codec(n).ok_or_else(|| Error::Fail(Fail::UnknownCodec(String::from(n)))),
    }
}

/* the handler a call names or strict looked up once a span fails */
fn handler<L: Lang>(name: Option<&str>) -> Result<OnError, Error> {
    match name {
        None => Ok(OnError::Strict),
        Some(n) => L::handler(n).ok_or_else(|| Error::Fail(Fail::UnknownHandler(String::from(n)))),
    }
}

/* `text` in the bytes of `encoding` handled as `errors` says within `room` bytes */
pub fn encode_with<L: Lang>(text: &str, encoding: Option<&str>, errors: Option<&str>, room: usize) -> Result<Vec<u8>, Error> {
    let codec = codec::<L>(encoding)?;
    let past = || Error::Vm(VmErr::Heap);
    // utf codecs write every character and a plain one leads with a little endian mark
    match codec {
        Codec::Utf8 => return if text.len() > room { Err(past()) } else { Ok(text.as_bytes().to_vec()) },
        Codec::Utf16(order) => {
            let units = if text.is_ascii() { text.len() } else { text.encode_utf16().count() };
            let mut out = sized(units.saturating_mul(2), if order == Order::Mark { &[0xFF, 0xFE] } else { &[] }, room)?;
            for u in text.encode_utf16() { out.extend_from_slice(&if order == Order::Be { u.to_be_bytes() } else { u.to_le_bytes() }); }
            return Ok(out);
        }
        Codec::Utf32(order) => {
            let chars = if text.is_ascii() { text.len() } else { text.chars().count() };
            let mut out = sized(chars.saturating_mul(4), if order == Order::Mark { &[0xFF, 0xFE, 0, 0] } else { &[] }, room)?;
            for c in text.chars() { out.extend_from_slice(&if order == Order::Be { (c as u32).to_be_bytes() } else { (c as u32).to_le_bytes() }); }
            return Ok(out);
        }
        _ => {}
    }
    // a single byte codec writes one byte a character before any handler runs
    if text.len() > room && text.chars().count() > room { return Err(past()); }
    if let Some(out) = text.chars().map(|c| byte(codec, c)).collect::<Option<Vec<u8>>>() { return Ok(out); }
    let mut out = Vec::new();
    let mut rest = text.chars().enumerate().peekable();
    while let Some((at, first)) = rest.next() {
        if let Some(b) = byte(codec, first) { out.push(b); continue; }
        // the run the codec cannot write which a strict failure names as one range
        let (how, mut c, mut len) = (handler::<L>(errors)?, first, 0);
        loop {
            len += 1;
            match how {
                OnError::Replace => out.push(b'?'),
                OnError::Escape => out.extend_from_slice(escape(c).as_bytes()),
                OnError::CharRef => out.extend_from_slice(format!("&#{};", c as u32).as_bytes()),
                OnError::Strict | OnError::Skip | OnError::Named => {}
            }
            if out.len() > room { return Err(past()); }
            match rest.peek() {
                Some(&(_, d)) if byte(codec, d).is_none() => { c = d; rest.next(); }
                _ => break,
            }
        }
        match how {
            OnError::Strict => return Err(Error::Fail(Fail::Encode { codec, first, at, len, why: Coding::Unmapped })),
            // no table here holds the names of characters
            OnError::Named => return Err(Error::Fail(Fail::UnknownHandler(String::from(errors.unwrap_or(""))))),
            _ => {}
        }
    }
    Ok(out)
}

/* a buffer for `n` bytes after `mark` refused when it passes `room` */
fn sized(n: usize, mark: &[u8], room: usize) -> Result<Vec<u8>, Error> {
    let n = n.saturating_add(mark.len());
    if n > room { return Err(Error::Vm(VmErr::Heap)); }
    let mut out = Vec::with_capacity(n);
    out.extend_from_slice(mark);
    Ok(out)
}

/* a character as its shortest backslash escape */
fn escape(c: char) -> String {
    let n = c as u32;
    if n < 0x100 { format!("\\x{n:02x}") } else if n < 0x10000 { format!("\\u{n:04x}") } else { format!("\\U{n:08x}") }
}

/* `buf` read as `encoding` with what it cannot read handled as `errors` says */
pub fn decode_with<L: Lang>(buf: &[u8], encoding: Option<&str>, errors: Option<&str>) -> Result<String, Error> {
    let codec = codec::<L>(encoding)?;
    match codec {
        Codec::Latin1 => return Ok(buf.iter().map(|&b| b as char).collect()),
        Codec::Utf8 => if let Ok(t) = core::str::from_utf8(buf) { return Ok(t.into()); },
        Codec::Ascii => if buf.is_ascii() { return Ok(buf.iter().map(|&b| b as char).collect()); },
        Codec::Utf16(_) | Codec::Utf32(_) | Codec::Windows1252 => return decode_wide::<L>(buf, codec, errors),
    }
    let mut out = String::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        // the bytes from `i` that read cleanly then the bad span after them and why
        let (good, bad, why) = match codec {
            Codec::Ascii => (buf[i..].iter().position(|&b| b >= 0x80).unwrap_or(buf.len() - i), 1, Coding::Unmapped),
            _ => match core::str::from_utf8(&buf[i..]) {
                Ok(_) => (buf.len() - i, 0, Coding::End),
                Err(e) => {
                    let at = i + e.valid_up_to();
                    let why = if e.error_len().is_none() { Coding::End }
                        else if matches!(buf[at], 0x80..=0xC1 | 0xF5..=0xFF) { Coding::Start }
                        else { Coding::Continuation };
                    (e.valid_up_to(), e.error_len().unwrap_or(buf.len() - at), why)
                }
            },
        };
        out.push_str(core::str::from_utf8(&buf[i..i + good]).unwrap_or_default());
        i += good;
        if bad == 0 || i >= buf.len() { break; }
        mend::<L>(&mut out, errors, &buf[i..i + bad], || Fail::Decode { codec, first: buf[i], at: i, len: bad, why })?;
        i += bad;
    }
    Ok(out)
}

/* what an unreadable span becomes under `errors` with the failure built only when raised */
fn mend<L: Lang>(out: &mut String, errors: Option<&str>, bad: &[u8], failure: impl FnOnce() -> Fail) -> Result<(), Error> {
    match handler::<L>(errors)? {
        OnError::Strict => return Err(Error::Fail(failure())),
        OnError::Skip => {}
        OnError::Replace => out.push('\u{FFFD}'),
        OnError::Escape => for &b in bad { out.push_str(&format!("\\x{b:02x}")); },
        // these handlers only write characters they were given
        OnError::CharRef | OnError::Named => return Err(Error::Fail(Fail::DecodeHandler)),
    }
    Ok(())
}

/* utf-16 utf-32 and windows-1252 where a plain utf codec reads its order off a mark */
fn decode_wide<L: Lang>(buf: &[u8], codec: Codec, errors: Option<&str>) -> Result<String, Error> {
    let (codec, mut i) = match codec {
        Codec::Utf16(Order::Mark) if buf.starts_with(&[0xFE, 0xFF]) => (Codec::Utf16(Order::Be), 2),
        Codec::Utf16(Order::Mark) => (Codec::Utf16(Order::Le), if buf.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 }),
        Codec::Utf32(Order::Mark) if buf.starts_with(&[0, 0, 0xFE, 0xFF]) => (Codec::Utf32(Order::Be), 4),
        Codec::Utf32(Order::Mark) => (Codec::Utf32(Order::Le), if buf.starts_with(&[0xFF, 0xFE, 0, 0]) { 4 } else { 0 }),
        c => (c, 0),
    };
    let mut out = String::with_capacity(buf.len());
    while i < buf.len() {
        // each step reads one character or names the bad span it found and why
        let step: Result<(char, usize), (usize, Coding)> = match codec {
            Codec::Windows1252 => match buf[i] {
                b @ 0x80..0xA0 => match WINDOWS_1252[(b - 0x80) as usize] {
                    0 => Err((1, Coding::Unmapped)),
                    n => Ok((char::from_u32(n as u32).unwrap_or('\u{FFFD}'), 1)),
                },
                b => Ok((b as char, 1)),
            },
            Codec::Utf16(order) => {
                let unit = |at: usize| { let p = [buf[at], buf[at + 1]]; if order == Order::Be { u16::from_be_bytes(p) } else { u16::from_le_bytes(p) } };
                if buf.len() - i < 2 { Err((buf.len() - i, Coding::Truncated)) } else {
                    match unit(i) {
                        0xD800..=0xDBFF if buf.len() - i < 4 => Err((buf.len() - i, Coding::End)),
                        u @ 0xD800..=0xDBFF => match unit(i + 2) {
                            lo @ 0xDC00..=0xDFFF => Ok((char::from_u32(0x10000 + ((u as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00)).unwrap_or('\u{FFFD}'), 4)),
                            _ => Err((2, Coding::Surrogate)),
                        },
                        0xDC00..=0xDFFF => Err((2, Coding::LowFirst)),
                        u => Ok((char::from_u32(u as u32).unwrap_or('\u{FFFD}'), 2)),
                    }
                }
            }
            _ => {
                if buf.len() - i < 4 { Err((buf.len() - i, Coding::Truncated)) } else {
                    let p = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
                    let n = if codec == Codec::Utf32(Order::Be) { u32::from_be_bytes(p) } else { u32::from_le_bytes(p) };
                    match char::from_u32(n) {
                        Some(c) => Ok((c, 4)),
                        None if (0xD800..0xE000).contains(&n) => Err((4, Coding::SurrogateRange)),
                        None => Err((4, Coding::PastUnicode)),
                    }
                }
            }
        };
        match step {
            Ok((c, n)) => { out.push(c); i += n; }
            Err((n, why)) => {
                mend::<L>(&mut out, errors, &buf[i..i + n], || Fail::Decode { codec, first: buf[i], at: i, len: n, why })?;
                i += n;
            }
        }
    }
    Ok(out)
}

/* the codec and handler names a call gives where other kinds fail */
fn names(cx: &Cx<'_, '_>, a: &Args<'_>, keep_nil: bool) -> Result<[Option<String>; 2], Error> {
    let mut out = [None, None];
    for (i, slot) in out.iter_mut().enumerate() {
        let Some(v) = a.get(i).filter(|v| !(keep_nil && v.is_none())) else { continue };
        match cx.text(v) {
            Some(t) => *slot = Some(String::from(t)),
            None => return Err(Error::Fail(Fail::ParamType { at: i as u8, v, want: KindId::Str })),
        }
    }
    Ok(out)
}

fn encode_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let cx = &*cx;
        (|| -> Result<Vec<u8>, Error> {
            let [encoding, errors] = names(cx, &a, true)?;
            let text = cx.str_of(a.recv).ok_or_else(crate::fail::receiver)?;
            encode_with::<S::Lang>(text, encoding.as_deref(), errors.as_deref(), cx.room())
        })()
    };
    let out = out.map_err(|e| e.raise::<S>(cx))?;
    // the bytes an encoding writes pay as native work once it wrote them
    if out.len() > 64 { cx.spend(out.len()); }
    Ok(cx.new_bytes(out, false)?.into())
}

fn decode_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let text = {
        let cx = &*cx;
        (|| -> Result<String, Error> {
            let [encoding, errors] = names(cx, &a, false)?;
            let buf = cx.bytes_of(a.recv).ok_or_else(crate::fail::receiver)?;
            cx.spend(buf.len());
            decode_with::<S::Lang>(buf, encoding.as_deref(), errors.as_deref())
        })()
    };
    let text = text.map_err(|e| e.raise::<S>(cx))?;
    Ok(cx.new_str(text)?.into())
}

/* bytes read as text for a caller holding them where failures name `S` */
pub fn decode_for<S: Site>(cx: &mut Cx<'_, '_>, buf: &[u8], encoding: Option<&str>, errors: Option<&str>) -> Result<String, edge::value::VmErr> {
    decode_with::<S::Lang>(buf, encoding, errors).map_err(|e| e.raise::<S>(cx))
}
