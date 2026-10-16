/* The text encodings the engine knows, UTF-8, ASCII and Latin-1, and the error handlers that decide what an
   unencodable character or an undecodable byte becomes. */

use alloc::{string::String, vec::Vec};
use crate::s;
use super::types::VmErr;

#[derive(Clone, Copy, PartialEq)]
enum Codec { Utf8, Ascii, Latin1 }

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
            _ => return Err(VmErr::Raised(s!("LookupError: unknown encoding: ", str name))),
        })
    }

    fn label(self) -> &'static str { match self { Self::Utf8 => "utf-8", Self::Ascii => "ascii", Self::Latin1 => "latin-1" } }

    /* The highest code point the codec writes in one byte, None for UTF-8, which writes them all. */
    fn limit(self) -> Option<u32> { match self { Self::Utf8 => None, Self::Ascii => Some(0x7F), Self::Latin1 => Some(0xFF) } }
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
    let Some(limit) = codec.limit() else { return Ok(text.as_bytes().to_vec()) };
    if text.chars().all(|c| (c as u32) <= limit) { return Ok(text.chars().map(|c| c as u8).collect()); }
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c as u32) <= limit { out.push(c as u8); i += 1; continue; }
        // The run of characters the codec cannot write, which a strict error names as one range.
        let end = (i..chars.len()).find(|&j| (chars[j] as u32) <= limit).unwrap_or(chars.len());
        match errors {
            "strict" => {
                let range = if end - i == 1 { s!("character ", str &quoted(c), " in position ", int i as i64) }
                    else { s!("characters in position ", int i as i64, "-", int (end - 1) as i64) };
                return Err(VmErr::Raised(s!("UnicodeEncodeError: '", str codec.label(), "' codec can't encode ", str &range, ": ordinal not in range(", int limit as i64 + 1, ")")));
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
