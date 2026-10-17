/* The text encodings the engine knows, UTF-8, UTF-16, UTF-32, ASCII, Latin-1 and cp1252, and the error handlers
   that decide what an unencodable character or an undecodable byte becomes. */

use alloc::{string::String, vec::Vec};
use crate::s;
use super::types::VmErr;

#[derive(Clone, Copy, PartialEq)]
enum Codec { Utf8, Ascii, Latin1, Cp1252, Utf16(Order), Utf32(Order) }

/* The byte order of a UTF-16 or UTF-32 codec, `Bom` the plain name that writes a mark and reads one. */
#[derive(Clone, Copy, PartialEq)]
enum Order { Bom, Le, Be }

/* What cp1252 reads bytes 0x80 to 0x9F as, 0 where it reads nothing. */
const CP1252: [u16; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0, 0x017D, 0,
    0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

impl Codec {
    /* The codec an encoding name means, in any case and with `_` for `-`. */
    fn named(name: &str) -> Result<Self, VmErr> {
        // The spellings programs write skip the normalizing copy.
        match name { "utf-8" | "utf8" => return Ok(Self::Utf8), "ascii" => return Ok(Self::Ascii), "latin-1" => return Ok(Self::Latin1), _ => {} }
        let n = name.to_ascii_lowercase().replace('_', "-");
        Ok(match n.as_str() {
            "utf-8" | "utf8" | "u8" | "utf" | "cp65001" => Self::Utf8,
            "ascii" | "us-ascii" | "646" => Self::Ascii,
            "latin-1" | "latin1" | "latin" | "l1" | "iso-8859-1" | "iso8859-1" | "8859" | "cp819" => Self::Latin1,
            "cp1252" | "windows-1252" | "1252" => Self::Cp1252,
            "utf-16" | "utf16" | "u16" => Self::Utf16(Order::Bom),
            "utf-16-le" | "utf-16le" | "utf16le" => Self::Utf16(Order::Le),
            "utf-16-be" | "utf-16be" | "utf16be" => Self::Utf16(Order::Be),
            "utf-32" | "utf32" | "u32" => Self::Utf32(Order::Bom),
            "utf-32-le" | "utf-32le" | "utf32le" => Self::Utf32(Order::Le),
            "utf-32-be" | "utf-32be" | "utf32be" => Self::Utf32(Order::Be),
            _ => return Err(VmErr::Raised(s!("LookupError: unknown encoding: ", str name))),
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8", Self::Ascii => "ascii", Self::Latin1 => "latin-1", Self::Cp1252 => "charmap",
            Self::Utf16(Order::Be) => "utf-16-be", Self::Utf16(_) => "utf-16-le",
            Self::Utf32(Order::Be) => "utf-32-be", Self::Utf32(_) => "utf-32-le",
        }
    }

    /* The one byte a single-byte codec writes `c` as, None when it has none. */
    fn byte(self, c: char) -> Option<u8> {
        let n = c as u32;
        match self {
            Self::Ascii => (n < 0x80).then_some(n as u8),
            Self::Latin1 => (n < 0x100).then_some(n as u8),
            Self::Cp1252 if n < 0x80 || (0xA0..0x100).contains(&n) => Some(n as u8),
            Self::Cp1252 => CP1252.iter().position(|&m| m != 0 && m as u32 == n).map(|i| 0x80 + i as u8),
            _ => None,
        }
    }

    /* Why a single-byte codec cannot write a character. */
    fn refusal(self) -> String {
        match self { Self::Ascii => "ordinal not in range(128)".into(), Self::Latin1 => "ordinal not in range(256)".into(), _ => "character maps to <undefined>".into() }
    }
}

fn unknown_handler(errors: &str) -> VmErr { VmErr::Raised(s!("LookupError: unknown error handler name '", str errors, "'")) }

/* `'\xe9'`, a character the way an encode error quotes it. */
fn quoted(c: char) -> String {
    let n = c as u32;
    if n < 0x100 { s!("'\\x", str &alloc::format!("{n:02x}"), "'") }
    else if n < 0x10000 { s!("'\\u", str &alloc::format!("{n:04x}"), "'") }
    else { s!("'\\U", str &alloc::format!("{n:08x}"), "'") }
}

/* `text` in the bytes of `encoding`, the characters it cannot write handled as `errors` says. */
pub(crate) fn encode(text: &str, encoding: &str, errors: &str) -> Result<Vec<u8>, VmErr> {
    let codec = Codec::named(encoding)?;
    // The UTF codecs write every character, a plain UTF-16 or UTF-32 behind a little-endian mark.
    match codec {
        Codec::Utf8 => return Ok(text.as_bytes().to_vec()),
        Codec::Utf16(order) => {
            let mut out = if order == Order::Bom { alloc::vec![0xFF, 0xFE] } else { Vec::with_capacity(text.len() * 2) };
            for u in text.encode_utf16() { out.extend_from_slice(&if order == Order::Be { u.to_be_bytes() } else { u.to_le_bytes() }); }
            return Ok(out);
        }
        Codec::Utf32(order) => {
            let mut out = if order == Order::Bom { alloc::vec![0xFF, 0xFE, 0, 0] } else { Vec::with_capacity(text.len() * 4) };
            for c in text.chars() { out.extend_from_slice(&if order == Order::Be { (c as u32).to_be_bytes() } else { (c as u32).to_le_bytes() }); }
            return Ok(out);
        }
        _ => {}
    }
    if let Some(out) = text.chars().map(|c| codec.byte(c)).collect::<Option<Vec<u8>>>() { return Ok(out); }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(b) = codec.byte(c) { out.push(b); i += 1; continue; }
        // The run of characters the codec cannot write, which a strict error names as one range.
        let end = (i..chars.len()).find(|&j| codec.byte(chars[j]).is_some()).unwrap_or(chars.len());
        match errors {
            "strict" => {
                let range = if end - i == 1 { s!("character ", str &quoted(c), " in position ", int i as i64) }
                    else { s!("characters in position ", int i as i64, "-", int (end - 1) as i64) };
                return Err(VmErr::Raised(s!("UnicodeEncodeError: '", str codec.label(), "' codec can't encode ", str &range, ": ", str &codec.refusal())));
            }
            "ignore" => {}
            "replace" => out.extend(core::iter::repeat_n(b'?', end - i)),
            "backslashreplace" => for &c in &chars[i..end] {
                let e = quoted(c);
                out.extend_from_slice(&e.as_bytes()[1..e.len() - 1]);
            },
            "xmlcharrefreplace" => for &c in &chars[i..end] { out.extend_from_slice(alloc::format!("&#{};", c as u32).as_bytes()); },
            _ => return Err(unknown_handler(errors)),
        }
        i = end;
    }
    Ok(out)
}

/* The text in `buf` read as `encoding`, the bytes it cannot read handled as `errors` says. */
pub(crate) fn decode(buf: &[u8], encoding: &str, errors: &str) -> Result<String, VmErr> {
    let codec = Codec::named(encoding)?;
    match codec {
        Codec::Latin1 => return Ok(buf.iter().map(|&b| b as char).collect()),
        Codec::Utf8 => if let Ok(t) = core::str::from_utf8(buf) { return Ok(t.into()); },
        Codec::Ascii => if buf.is_ascii() { return Ok(buf.iter().map(|&b| b as char).collect()); },
        Codec::Utf16(_) | Codec::Utf32(_) | Codec::Cp1252 => return decode_wide(buf, codec, errors),
    }
    let mut out = String::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        // The bytes from `i` that read cleanly, then the bad span after them and why it is bad.
        let (good, bad, reason) = match codec {
            Codec::Ascii => {
                let good = buf[i..].iter().position(|&b| b >= 0x80).unwrap_or(buf.len() - i);
                (good, 1, "ordinal not in range(128)")
            }
            _ => match core::str::from_utf8(&buf[i..]) {
                Ok(_) => (buf.len() - i, 0, ""),
                Err(e) => {
                    let at = i + e.valid_up_to();
                    let reason = if e.error_len().is_none() { "unexpected end of data" }
                        else if matches!(buf[at], 0x80..=0xC1 | 0xF5..=0xFF) { "invalid start byte" }
                        else { "invalid continuation byte" };
                    (e.valid_up_to(), e.error_len().unwrap_or(buf.len() - at), reason)
                }
            },
        };
        out.push_str(core::str::from_utf8(&buf[i..i + good]).unwrap_or_default());
        i += good;
        if bad == 0 || i >= buf.len() { break; }
        match errors {
            "strict" => {
                let range = if bad == 1 { s!("byte 0x", str &alloc::format!("{:02x}", buf[i]), " in position ", int i as i64) }
                    else { s!("bytes in position ", int i as i64, "-", int (i + bad - 1) as i64) };
                return Err(VmErr::Raised(s!("UnicodeDecodeError: '", str codec.label(), "' codec can't decode ", str &range, ": ", str reason)));
            }
            "ignore" => {}
            "replace" => out.push('\u{FFFD}'),
            "backslashreplace" => for &b in &buf[i..i + bad] { out.push_str(&alloc::format!("\\x{b:02x}")); },
            _ => return Err(unknown_handler(errors)),
        }
        i += bad;
    }
    Ok(out)
}

/* The error of an undecodable span, one byte or a range of them. */
fn decode_error(label: &str, buf: &[u8], at: usize, len: usize, reason: &str) -> VmErr {
    let range = if len == 1 { s!("byte 0x", str &alloc::format!("{:02x}", buf[at]), " in position ", int at as i64) }
        else { s!("bytes in position ", int at as i64, "-", int (at + len - 1) as i64) };
    VmErr::Raised(s!("UnicodeDecodeError: '", str label, "' codec can't decode ", str &range, ": ", str reason))
}

/* UTF-16, UTF-32 and cp1252 text, a plain UTF name reading its byte order off a leading mark. */
fn decode_wide(buf: &[u8], codec: Codec, errors: &str) -> Result<String, VmErr> {
    if !matches!(errors, "strict" | "ignore" | "replace" | "backslashreplace") { return Err(unknown_handler(errors)); }
    let (codec, mut i) = match codec {
        Codec::Utf16(Order::Bom) if buf.starts_with(&[0xFE, 0xFF]) => (Codec::Utf16(Order::Be), 2),
        Codec::Utf16(Order::Bom) => (Codec::Utf16(Order::Le), if buf.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 }),
        Codec::Utf32(Order::Bom) if buf.starts_with(&[0, 0, 0xFE, 0xFF]) => (Codec::Utf32(Order::Be), 4),
        Codec::Utf32(Order::Bom) => (Codec::Utf32(Order::Le), if buf.starts_with(&[0xFF, 0xFE, 0, 0]) { 4 } else { 0 }),
        c => (c, 0),
    };
    let label = codec.label();
    let mut out = String::with_capacity(buf.len());
    while i < buf.len() {
        // Each step reads one character, or names the bad span it found and why.
        let step: Result<(char, usize), (usize, &str)> = match codec {
            Codec::Cp1252 => {
                let b = buf[i];
                let n = if (0x80..0xA0).contains(&b) { CP1252[(b - 0x80) as usize] as u32 } else { b as u32 };
                if n == 0 { Err((1, "character maps to <undefined>")) } else { Ok((char::from_u32(n).unwrap_or('\u{FFFD}'), 1)) }
            }
            Codec::Utf16(order) => {
                let unit = |at: usize| { let p = [buf[at], buf[at + 1]]; if order == Order::Be { u16::from_be_bytes(p) } else { u16::from_le_bytes(p) } };
                if buf.len() - i < 2 { Err((buf.len() - i, "truncated data")) }
                else {
                    let u = unit(i);
                    match u {
                        0xD800..=0xDBFF if buf.len() - i < 4 => Err((buf.len() - i, "unexpected end of data")),
                        0xD800..=0xDBFF => match unit(i + 2) {
                            lo @ 0xDC00..=0xDFFF => Ok((char::from_u32(0x10000 + (((u as u32) - 0xD800) << 10) + (lo as u32 - 0xDC00)).unwrap_or('\u{FFFD}'), 4)),
                            _ => Err((2, "illegal UTF-16 surrogate")),
                        },
                        0xDC00..=0xDFFF => Err((2, "illegal encoding")),
                        u => Ok((char::from_u32(u as u32).unwrap_or('\u{FFFD}'), 2)),
                    }
                }
            }
            _ => {
                if buf.len() - i < 4 { Err((buf.len() - i, "truncated data")) }
                else {
                    let p = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
                    let n = if codec == Codec::Utf32(Order::Be) { u32::from_be_bytes(p) } else { u32::from_le_bytes(p) };
                    match char::from_u32(n) {
                        Some(c) => Ok((c, 4)),
                        None if (0xD800..0xE000).contains(&n) => Err((4, "code point in surrogate code point range(0xd800, 0xe000)")),
                        None => Err((4, "code point not in range(0x110000)")),
                    }
                }
            }
        };
        match step {
            Ok((c, n)) => { out.push(c); i += n; }
            Err((n, reason)) => {
                match errors {
                    "strict" => return Err(decode_error(label, buf, i, n, reason)),
                    "ignore" => {}
                    "replace" => out.push('\u{FFFD}'),
                    _ => for &b in &buf[i..i + n] { out.push_str(&alloc::format!("\\x{b:02x}")); },
                }
                i += n;
            }
        }
    }
    Ok(out)
}
