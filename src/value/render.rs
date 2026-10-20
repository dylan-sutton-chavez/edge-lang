use alloc::string::String;
use super::Val;

/* Cap on nested-container rendering depth, stops self-referential prints from overflowing the stack. */
pub(crate) const RENDER_DEPTH_MAX: usize = 100;
/* Cap on total rendered output, bounds breadth the way RENDER_DEPTH_MAX bounds depth. */
pub(crate) const MAX_REPR_LEN: usize = 1_000_000;

/* Render `bytes` as `b'...'`, printable ASCII verbatim and the rest escaped, in double quotes when it holds a single quote and no double one. */
pub(crate) fn format_bytes(buf: &[u8]) -> String { quote_bytes(buf, false) }

/* `format_bytes`, the single quote escaped even inside double quotes when `escape_single` asks, as a bytearray shows. */
pub(crate) fn quote_bytes(buf: &[u8], escape_single: bool) -> String {
    let quote = if buf.contains(&b'\'') && !buf.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::with_capacity(buf.len() + 3);
    out.push('b');
    out.push(quote as char);
    for &b in buf {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\'' if quote == b'\'' || escape_single => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                out.push_str("\\x");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out.push(quote as char);
    out
}

/* `repr` of a str, quote selection (' unless the text has ' but not ") and backslash escapes for what is not printable, the rest verbatim. */
pub(crate) fn repr_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    push_repr_str(&mut out, s);
    out
}

/* `repr_str` of `s` written onto `out`. */
pub(crate) fn push_repr_str(out: &mut String, s: &str) {
    use core::fmt::Write;
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => { out.push('\\'); out.push(c); }
            c if !crate::util::uni::is_printable(c) => {
                let n = c as u32;
                if n <= 0xff { let _ = write!(out, "\\x{:02x}", n); }
                else if n <= 0xffff { let _ = write!(out, "\\u{:04x}", n); }
                else { let _ = write!(out, "\\U{:08x}", n); }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
}

/* The ` at 0x...` a repr places an object by, the word `id` gives. */
pub(crate) fn at(v: Val) -> String { alloc::format!(" at 0x{:x}", v.0) }
