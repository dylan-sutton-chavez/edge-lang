pub(crate) const ID_START: u8 = 1;
pub(crate) const ID_CONT: u8 = 2;
pub(crate) const DIGIT: u8 = 4;
pub(crate) const SPACE: u8 = 8;

// a 256 entry table replacing per-byte branches with indexed flag loads
pub(crate) static BYTE_CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        let b = i as u8;
        // a non-ASCII byte always starts or continues an identifier
        t[i] = match b { b' ' | b'\t' | b'\r' => SPACE, b'0'..=b'9' => DIGIT | ID_CONT, b'a'..=b'z' | b'A'..=b'Z' | b'_' | 0x80.. => ID_START | ID_CONT, _ => 0 };
        i += 1;
    }
    t
};

/* the bytes a braced code point takes when one to six hex digits sit inside */
pub(crate) fn braced_len(s: &[u8]) -> Option<usize> {
    let digits = s.strip_prefix(b"{")?.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    ((1..=6).contains(&digits) && s.get(digits + 1) == Some(&b'}')).then_some(digits + 2)
}

/* the UTF-8 length from its lead byte for non-ASCII identifiers */
#[inline]
pub(crate) fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}
