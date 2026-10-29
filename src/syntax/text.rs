use crate::syntax::lexer::Spelling;

use alloc::string::String;

// the escapes of a table as lookups a literal's decoding reads
struct Esc<S>(core::marker::PhantomData<S>);

impl<S: Spelling> Esc<S> {
    // the character each simple escape stands for by the byte after its backslash
    const SIMPLE: &'static [Option<char>; 128] = &{
        let e = S::SYNTAX.strings.escapes.simple;
        let mut t = [None; 128];
        let mut i = 0;
        while i < e.len() { t[e[i].0 as usize & 127] = Some(e[i].1); i += 1; }
        t
    };
    // one more than the index of each quote byte among the table's quotes
    const QUOTE: &'static [u8; 256] = &{
        let q = S::SYNTAX.strings.quotes;
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < q.len() { t[q[i].byte as usize] = i as u8 + 1; i += 1; }
        t
    };

    // one more than the bits of each one letter prefix in either case or zero
    const PREFIX1: &'static [u8; 256] = &{
        let (p, any) = (S::SYNTAX.strings.prefixes, S::SYNTAX.strings.any_case);
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < p.len() {
            let b = p[i].0.as_bytes();
            if b.len() == 1 {
                t[b[0] as usize] = p[i].1 + 1;
                if any { t[b[0].to_ascii_uppercase() as usize] = p[i].1 + 1; }
            }
            i += 1;
        }
        t
    };

    #[inline]
    fn simple(c: u8) -> Option<char> { if c < 128 { Self::SIMPLE[c as usize] } else { None } }
}

// the bytes of a literal's prefix before its quote or a raw one's hashes
#[inline]
fn prefix_len<S: Spelling>(b: &[u8]) -> usize {
    // most literals carry no prefix so their first byte is the quote
    if b.first().is_some_and(|&c| Esc::<S>::QUOTE[c as usize] != 0) { return 0; }
    b.iter().position(|&c| Esc::<S>::QUOTE[c as usize] != 0 || c == b'#').unwrap_or(b.len())
}

// the RAW BYTES and FORMAT bits of the `n` byte prefix opening `b` in `S`
#[inline]
fn flags_of<S: Spelling>(b: &[u8], n: usize) -> u8 {
    match n {
        0 => 0,
        1 => Esc::<S>::PREFIX1[b[0] as usize].saturating_sub(1),
        _ => {
            let (prefix, strings) = (&b[..n], &S::SYNTAX.strings);
            strings.prefixes.iter().find(|(p, _)| p.len() == n && p.bytes().zip(prefix).all(|(a, &c)| a == c || (strings.any_case && a == c.to_ascii_lowercase()))).map_or(0, |&(_, f)| f)
        }
    }
}

// the RAW BYTES and FORMAT bits the prefix of a literal sets in table `S`
pub(super) fn prefix_flags<S: Spelling>(s: &str) -> u8 {
    let b = s.as_bytes();
    flags_of::<S>(b, prefix_len::<S>(b))
}

// a literal's prefix bits and where its body starts and ends between its quotes
#[inline]
fn literal_parts<S: Spelling>(b: &[u8]) -> (u8, usize, usize) {
    let n = prefix_len::<S>(b);
    let flags = flags_of::<S>(b, n);
    let hashes = if S::SYNTAX.strings.hashes { b[n..].iter().take_while(|&&c| c == b'#').count() } else { 0 };
    let at = n + hashes;
    let Some(&quote) = b.get(at) else { return (flags, 0, 0) };
    let triple = S::SYNTAX.strings.quotes.get((Esc::<S>::QUOTE[quote as usize] as usize).wrapping_sub(1)).is_some_and(|q| q.triple);
    let open = if hashes == 0 && triple && b.len() >= at + 6 && b[at + 1] == quote && b[at + 2] == quote { 3 } else { 1 };
    (flags, at + open, b.len().saturating_sub(open + hashes).max(at + open))
}

// strips the prefix and quotes and unescapes all but raw strings
pub(super) fn parse_string<S: Spelling>(s: &str) -> String {
    let (flags, from, to) = literal_parts::<S>(s.as_bytes());
    let inner = s.get(from..to).unwrap_or("");
    // source CR and CRLF read as LF in the literal
    let owned;
    let inner: &str = if S::SYNTAX.strings.newlines && inner.contains('\r') {
        owned = inner.replace("\r\n", "\n").replace('\r', "\n");
        &owned
    } else { inner };
    if flags & crate::syntax::lexer::RAW != 0 { inner.into() } else { unescape::<S>(inner) }
}

/* decodes bytes where hex and octal give one byte and code point escapes stay verbatim */
pub(super) fn parse_bytes_literal<S: Spelling>(s: &str) -> alloc::vec::Vec<u8> {
    let (flags, from, to) = literal_parts::<S>(s.as_bytes());
    let body = s.as_bytes().get(from..to).unwrap_or(&[]);
    // source CR and CRLF read as LF as in a str literal
    let owned;
    let body: &[u8] = if S::SYNTAX.strings.newlines && body.contains(&b'\r') {
        owned = String::from_utf8_lossy(body).replace("\r\n", "\n").replace('\r', "\n").into_bytes();
        &owned
    } else { body };
    if flags & crate::syntax::lexer::RAW != 0 { return body.to_vec(); }
    let e = &S::SYNTAX.strings.escapes;
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(body.len());
    let mut j = 0;
    while j < body.len() {
        if body[j] != b'\\' { out.push(body[j]); j += 1; continue; }
        if j + 1 >= body.len() { out.push(b'\\'); break; }
        if let Some(c) = Esc::<S>::simple(body[j + 1]) && (c as u32) < 256 { out.push(c as u8); j += 2; continue; }
        match body[j + 1] {
            // a backslash before a newline joins the lines
            b'\n' if e.line_join => j += 2,
            b'0'..=b'7' if e.octal => {
                // octal takes up to three digits as in `push_escape` truncated to one byte
                let mut v = (body[j + 1] - b'0') as u32;
                let mut k = j + 2;
                while k < body.len() && k < j + 4 && matches!(body[k], b'0'..=b'7') {
                    v = v * 8 + (body[k] - b'0') as u32;
                    k += 1;
                }
                out.push(v as u8);
                j = k;
            }
            b'x' if e.hex => {
                if j + 3 < body.len() {
                    let hi = (body[j + 2] as char).to_digit(16);
                    let lo = (body[j + 3] as char).to_digit(16);
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        out.push((hi as u8) * 16 + lo as u8);
                        j += 4;
                        continue;
                    }
                }
                // a malformed `\x` stays verbatim
                out.push(b'\\'); out.push(b'x'); j += 2;
            }
            other => { out.push(b'\\'); out.push(other); j += 2; }
        }
    }
    out
}

fn unescape<S: Spelling>(s: &str) -> String {
    // text with no backslash is copied whole
    if !s.contains('\\') { return s.into(); }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' { push_escape::<S>(&mut out, &mut chars); } else { out.push(c); }
    }
    out
}

// decodes one escape past its backslash keeping it for unknown and malformed escapes
pub(super) fn push_escape<S: Spelling>(out: &mut String, chars: &mut core::iter::Peekable<core::str::Chars>) {
    use crate::syntax::lexer::Unicode;
    // exactly `n` hex digits or none taken so a short or signed run stays text
    let take_hex = |chars: &mut core::iter::Peekable<core::str::Chars>, n: usize| -> Option<char> {
        let mut probe = chars.clone();
        let mut v = 0u32;
        for _ in 0..n { v = v * 16 + probe.next()?.to_digit(16)?; }
        *chars = probe;
        Some(char::from_u32(v).unwrap_or('\u{FFFD}'))
    };
    // one to six hex digits between braces or none taken
    let take_braced = |chars: &mut core::iter::Peekable<core::str::Chars>| -> Option<char> {
        let mut probe = chars.clone();
        if probe.next() != Some('{') { return None; }
        let (mut v, mut n) = (0u32, 0);
        loop {
            match probe.next()? {
                '}' if n > 0 => break,
                c if n < 6 => { v = v * 16 + c.to_digit(16)?; n += 1; }
                _ => return None,
            }
        }
        *chars = probe;
        Some(char::from_u32(v).unwrap_or('\u{FFFD}'))
    };
    let e = &S::SYNTAX.strings.escapes;
    let next = chars.next();
    if let Some(c) = next.and_then(|c| u8::try_from(c).ok()).and_then(Esc::<S>::simple) { out.push(c); return; }
    let decoded = match next {
        // a backslash before a newline joins the lines
        Some('\n') if e.line_join => return,
        Some('x') if e.hex => take_hex(chars, 2),
        Some('u') if e.unicode == Unicode::Fixed => take_hex(chars, 4),
        Some('U') if e.unicode == Unicode::Fixed => take_hex(chars, 8),
        Some('u') if e.unicode == Unicode::Braced => take_braced(chars),
        Some(c @ '0'..='7') if e.octal => {
            let mut digits = String::from(c);
            while digits.len() < 3 && matches!(chars.peek(), Some('0'..='7')) {
                digits.push(chars.next().unwrap());
            }
            let code = u32::from_str_radix(&digits, 8).unwrap_or(0);
            Some(char::from_u32(code).unwrap_or('\u{FFFD}'))
        }
        _ => None,
    };
    match (decoded, next) {
        (Some(c), _) => out.push(c),
        (None, Some(c)) => { out.push('\\'); out.push(c); }
        (None, None) => out.push('\\'),
    }
}
