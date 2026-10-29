pub const ID_START: u8 = 1;
pub const ID_CONT: u8 = 2;
pub const DIGIT: u8 = 4;
pub const SPACE: u8 = 8;

// a 256 entry table replacing per-byte branches with indexed flag loads
pub static BYTE_CLASS: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        let b = i as u8;
        if b == b' ' || b == b'\t' || b == b'\r' { t[i] = SPACE; }
        if b >= b'0' && b <= b'9' { t[i] = DIGIT | ID_CONT; }
        if (b >= b'a' && b <= b'z') || (b >= b'A' && b <= b'Z') {
            t[i] = ID_START | ID_CONT;
        }
        if b == b'_' { t[i] = ID_START | ID_CONT; }
        if b >= 0x80 { t[i] = ID_START | ID_CONT; } // a non-ASCII byte always starts or continues an identifier
        i += 1;
    }
    t
};

/* the bytes a braced code point takes when one to six hex digits sit inside */
pub fn braced_len(s: &[u8]) -> Option<usize> {
    let digits = s.strip_prefix(b"{")?.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    ((1..=6).contains(&digits) && s.get(digits + 1) == Some(&b'}')).then_some(digits + 2)
}

/* the UTF-8 length from its lead byte for non-ASCII identifiers */
#[inline]
pub fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}
