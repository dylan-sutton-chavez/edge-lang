use super::TokenType;

/* Byte-class flags packed into a u8, combine with bitwise OR for membership tests. */
pub const ID_START: u8 = 1;
pub const ID_CONT: u8 = 2;
pub const DIGIT: u8 = 4;
pub const SPACE: u8 = 8;

// 256-byte LUT replacing per-byte branches with indexed flag loads.
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
        if b >= 0x80 { t[i] = ID_START | ID_CONT; } // Non-ASCII always starts/continues an identifier.
        i += 1;
    }
    t
};

/* Operators by first byte, alone, followed by `=`, doubled, and doubled then `=`, Endmarker for none. */
pub static OPERATORS: [[TokenType; 4]; 128] = {
    use TokenType::*;
    const N: TokenType = Endmarker;
    let mut t = [[N; 4]; 128];
    t[b'(' as usize] = [Lpar, N, N, N]; t[b')' as usize] = [Rpar, N, N, N];
    t[b'[' as usize] = [Lsqb, N, N, N]; t[b']' as usize] = [Rsqb, N, N, N];
    t[b'{' as usize] = [Lbrace, N, N, N];
    t[b'!' as usize] = [Exclamation, NotEqual, N, N]; t[b'%' as usize] = [Percent, PercentEqual, N, N];
    t[b'&' as usize] = [Amper, AmperEqual, N, N]; t[b'*' as usize] = [Star, StarEqual, DoubleStar, DoubleStarEqual];
    t[b'+' as usize] = [Plus, PlusEqual, N, N]; t[b'-' as usize] = [Minus, MinEqual, N, N];
    t[b'.' as usize] = [Dot, N, N, N]; t[b'/' as usize] = [Slash, SlashEqual, DoubleSlash, DoubleSlashEqual];
    t[b'<' as usize] = [Less, LessEqual, LeftShift, LeftShiftEqual]; t[b'=' as usize] = [Equal, EqEqual, N, N];
    t[b'>' as usize] = [Greater, GreaterEqual, RightShift, RightShiftEqual]; t[b'@' as usize] = [At, AtEqual, N, N];
    t[b'^' as usize] = [Circumflex, CircumflexEqual, N, N]; t[b'|' as usize] = [Vbar, VbarEqual, N, N];
    t[b'~' as usize] = [Tilde, N, N, N]; t[b',' as usize] = [Comma, N, N, N];
    t[b':' as usize] = [Colon, ColonEqual, N, N]; t[b';' as usize] = [Semi, N, N, N];
    t
};

/* Routes by (length, first byte) to skip most memcmps. */
pub fn keyword(s: &[u8]) -> Option<TokenType> {
    match (s.len(), s[0]) {
        (1, b'_') => Some(TokenType::Underscore),

        (2, b'a') => if s[1] == b's' { Some(TokenType::As) } else { None },
        (2, b'i') => match s[1] {
            b'f' => Some(TokenType::If),
            b'n' => Some(TokenType::In),
            b's' => Some(TokenType::Is),
            _ => None,
        },
        (2, b'o') => if s[1] == b'r' { Some(TokenType::Or) } else { None },

        (3, b'a') => if s[1] == b'n' && s[2] == b'd' { Some(TokenType::And) } else { None },
        (3, b'd') => if s[1] == b'e' {
            match s[2] { b'f' => Some(TokenType::Def), b'l' => Some(TokenType::Del), _ => None }
        } else { None },
        (3, b'f') => if s[1] == b'o' && s[2] == b'r' { Some(TokenType::For) } else { None },
        (3, b'n') => if s[1] == b'o' && s[2] == b't' { Some(TokenType::Not) } else { None },
        (3, b't') => if s[1] == b'r' && s[2] == b'y' { Some(TokenType::Try) } else { None },

        (4, b'c') => if s == b"case" { Some(TokenType::Case) } else { None },
        (4, b'e') => match s[2] {
            b'i' => if s == b"elif" { Some(TokenType::Elif) } else { None },
            b's' => if s == b"else" { Some(TokenType::Else) } else { None },
            _ => None,
        },
        (4, b'f') => if s == b"from" { Some(TokenType::From) } else { None },
        (4, b'N') => if s == b"None" { Some(TokenType::None) } else { None },
        (4, b'p') => if s == b"pass" { Some(TokenType::Pass) } else { None },
        (4, b'T') => if s == b"True" { Some(TokenType::True) } else { None },
        (4, b't') => if s == b"type" { Some(TokenType::Type) } else { None },
        (4, b'w') => if s == b"with" { Some(TokenType::With) } else { None },

        (5, b'a') => match s[1] {
            b's' => if s == b"async" { Some(TokenType::Async) } else { None },
            b'w' => if s == b"await" { Some(TokenType::Await) } else { None },
            _ => None,
        },
        (5, b'b') => if s == b"break" { Some(TokenType::Break) } else { None },
        (5, b'c') => if s == b"class" { Some(TokenType::Class) } else { None },
        (5, b'F') => if s == b"False" { Some(TokenType::False) } else { None },
        (5, b'm') => if s == b"match" { Some(TokenType::Match) } else { None },
        (5, b'r') => if s == b"raise" { Some(TokenType::Raise) } else { None },
        (5, b'w') => if s == b"while" { Some(TokenType::While) } else { None },
        (5, b'y') => if s == b"yield" { Some(TokenType::Yield) } else { None },

        (6, b'a') => if s == b"assert" { Some(TokenType::Assert) } else { None },
        (6, b'e') => if s == b"except" { Some(TokenType::Except) } else { None },
        (6, b'g') => if s == b"global" { Some(TokenType::Global) } else { None },
        (6, b'i') => if s == b"import" { Some(TokenType::Import) } else { None },
        (6, b'l') => if s == b"lambda" { Some(TokenType::Lambda) } else { None },
        (6, b'r') => if s == b"return" { Some(TokenType::Return) } else { None },

        (7, b'f') => if s == b"finally" { Some(TokenType::Finally) } else { None },

        (8, b'c') => if s == b"continue" { Some(TokenType::Continue) } else { None },
        (8, b'n') => if s == b"nonlocal" { Some(TokenType::Nonlocal) } else { None },

        _ => None,
    }
}


/* What a prefix opens before a quote, an f-string, a string or bytes, in any case. */
#[inline]
pub fn string_prefix(s: &[u8]) -> Option<TokenType> {
    let mut seen = 0u8;
    for &c in s {
        seen |= match c.to_ascii_lowercase() { b'f' => 1, b'r' => 2, b'u' => 4, b'b' => 8, _ => return None };
    }
    match (s.len(), seen) {
        (1, 1) | (2, 3) => Some(TokenType::FstringStart),
        (1, 2) | (1, 4) => Some(TokenType::String),
        (1, 8) | (2, 10) => Some(TokenType::Bytes),
        _ => None,
    }
}

/* UTF-8 byte length from lead byte, supports non-ASCII identifiers. */
#[inline]
pub fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/* Reverse lookup for diagnostics, TokenType -> quoted display name. */
pub const fn token_to_str(kind: &TokenType) -> &'static str {
    match kind {
        // Keywords
        TokenType::False => "'False'", TokenType::None => "'None'", TokenType::True => "'True'",
        TokenType::And => "'and'", TokenType::As => "'as'", TokenType::Assert => "'assert'",
        TokenType::Async => "'async'", TokenType::Await => "'await'", TokenType::Break => "'break'",
        TokenType::Class => "'class'", TokenType::Continue => "'continue'", TokenType::Def => "'def'",
        TokenType::Del => "'del'", TokenType::Elif => "'elif'", TokenType::Else => "'else'",
        TokenType::Except => "'except'", TokenType::Finally => "'finally'", TokenType::For => "'for'",
        TokenType::From => "'from'", TokenType::Global => "'global'", TokenType::If => "'if'",
        TokenType::Import => "'import'", TokenType::In => "'in'", TokenType::Is => "'is'",
        TokenType::Lambda => "'lambda'", TokenType::Nonlocal => "'nonlocal'", TokenType::Not => "'not'",
        TokenType::Or => "'or'", TokenType::Pass => "'pass'", TokenType::Raise => "'raise'",
        TokenType::Return => "'return'", TokenType::Try => "'try'", TokenType::While => "'while'",
        TokenType::With => "'with'", TokenType::Yield => "'yield'",

        // Soft keywords
        TokenType::Case => "'case'", TokenType::Match => "'match'", 
        TokenType::Type => "'type'", TokenType::Underscore => "'_'",

        // Operators (3-char)
        TokenType::DoubleStarEqual => "'**='", TokenType::DoubleSlashEqual => "'//='", 
        TokenType::LeftShiftEqual => "'<<='", TokenType::RightShiftEqual => "'>>='",

        // Operators (2-char)
        TokenType::NotEqual => "'!='", TokenType::PercentEqual => "'%='", TokenType::AmperEqual => "'&='", 
        TokenType::DoubleStar => "'**'", TokenType::StarEqual => "'*='", TokenType::PlusEqual => "'+='", 
        TokenType::MinEqual => "'-='", TokenType::Rarrow => "'->'", TokenType::Ellipsis => "'...'", 
        TokenType::DoubleSlash => "'//'", TokenType::SlashEqual => "'/='", TokenType::ColonEqual => "':='", 
        TokenType::LeftShift => "'<<'", TokenType::LessEqual => "'<='", TokenType::EqEqual => "'=='", 
        TokenType::GreaterEqual => "'>='", TokenType::RightShift => "'>>'", TokenType::AtEqual => "'@='", 
        TokenType::CircumflexEqual => "'^='", TokenType::VbarEqual => "'|='",

        // Operators (1-char)
        TokenType::Exclamation => "'!'", TokenType::Percent => "'%'", TokenType::Amper => "'&'", 
        TokenType::Star => "'*'", TokenType::Plus => "'+'", TokenType::Minus => "'-'", 
        TokenType::Dot => "'.'", TokenType::Slash => "'/'", TokenType::Less => "'<'", 
        TokenType::Equal => "'='", TokenType::Greater => "'>'", TokenType::At => "'@'", 
        TokenType::Circumflex => "'^'", TokenType::Vbar => "'|'", TokenType::Tilde => "'~'", 
        TokenType::Comma => "','", TokenType::Colon => "':'", TokenType::Semi => "';'",

        // Delimiters
        TokenType::Lpar => "'('", TokenType::Rpar => "')'", 
        TokenType::Lsqb => "'['", TokenType::Rsqb => "']'", 
        TokenType::Lbrace => "'{'", TokenType::Rbrace => "'}'",

        // Literals
        TokenType::Name => "identifier",
        TokenType::Float => "float",
        TokenType::Int => "integer",
        TokenType::String => "string",
        TokenType::Bytes => "bytes literal",

        // F-string
        TokenType::FstringStart => "f-string start", 
        TokenType::FstringMiddle => "f-string content", 
        TokenType::FstringEnd => "f-string end",

        // Whitespace and structure
        TokenType::Comment => "comment", 
        TokenType::Newline | TokenType::Nl => "newline", 
        TokenType::Indent => "indentation", 
        TokenType::Dedent => "dedentation", 
        TokenType::Endmarker => "EOF",
    }
}
