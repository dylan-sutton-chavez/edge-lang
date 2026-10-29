use edge::lexer::{index, operators, Blocks, Comments, Continuation, Ends, Escapes, Interp, Keywords, Literal, Mode, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros, BYTES, FORMAT, RAW};
use edge::lexer::TokenType as T;

const WORDS: &[(&str, TokenType)] = &[
    ("false", T::False), ("nil", T::Nil), ("true", T::True), ("as", T::As), ("assert", T::Assert),
    ("async", T::Async), ("await", T::Await), ("break", T::Break), ("class", T::Class), ("continue", T::Continue),
    ("fn", T::Fn), ("del", T::Delete), ("else if", T::ElseIf), ("else", T::Else), ("except", T::Catch),
    ("finally", T::Finally), ("for", T::For), ("from", T::From), ("global", T::Global), ("if", T::If),
    ("import", T::Import), ("in", T::In), ("is", T::Is), ("lambda", T::FnExpr), ("nonlocal", T::Outer),
    ("pass", T::Empty), ("raise", T::Throw), ("return", T::Return), ("try", T::Try), ("while", T::While),
    ("with", T::With), ("yield", T::Yield), ("case", T::Case), ("match", T::Match), ("type", T::Type),
    ("_", T::Underscore),
];

const SYMBOLS: &[(&str, TokenType)] = &[
    ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose),
    ("**=", T::PowEqual), ("~/=", T::FloorDivEqual), ("<<=", T::LeftShiftEqual), (">>=", T::RightShiftEqual),
    ("!=", T::NotEqual), ("%=", T::PercentEqual), ("&=", T::BitAndEqual), ("**", T::Pow), ("*=", T::StarEqual),
    ("+=", T::PlusEqual), ("-=", T::MinusEqual), ("->", T::Returns), ("...", T::Ellipsis), ("~/", T::FloorDiv),
    ("/=", T::SlashEqual), (":=", T::ColonEqual), ("<<", T::LeftShift), ("<=", T::LessEqual), ("==", T::EqualEqual),
    (">=", T::GreaterEqual), (">>", T::RightShift), ("@=", T::AtEqual), ("^=", T::BitXorEqual), ("|=", T::BitOrEqual),
    ("&&", T::And), ("||", T::Or), ("!", T::Not),
    ("!", T::Convert), ("%", T::Percent), ("&", T::BitAnd), ("*", T::Star), ("+", T::Plus), ("-", T::Minus),
    (".", T::Dot), ("/", T::Slash), ("<", T::Less), ("=", T::Equal), (">", T::Greater), ("@", T::At),
    ("^", T::BitXor), ("|", T::BitOr), ("~", T::Tilde), (",", T::Comma), (":", T::Colon), (";", T::Semi),
];

static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);

static KEYWORDS: Keywords = index(WORDS);

// the binding powers mini's operators take where a zero left power marks a prefix operator
pub const LADDER: &[(TokenType, u8, u8)] = &[
    (T::Or, 1, 2), (T::And, 3, 4), (T::Not, 0, 5),
    (T::EqualEqual, 7, 8), (T::NotEqual, 7, 8), (T::Less, 7, 8), (T::Greater, 7, 8), (T::LessEqual, 7, 8), (T::GreaterEqual, 7, 8),
    (T::In, 7, 8), (T::NotIn, 7, 8), (T::Is, 7, 8), (T::IsNot, 7, 8), (T::Not, 7, 8),
    (T::BitOr, 9, 10), (T::BitXor, 11, 12), (T::BitAnd, 13, 14), (T::LeftShift, 15, 16), (T::RightShift, 15, 16),
    (T::Plus, 17, 18), (T::Minus, 17, 18),
    (T::Star, 19, 20), (T::Slash, 19, 20), (T::Percent, 19, 20), (T::FloorDiv, 19, 20), (T::At, 19, 20),
    (T::Minus, 0, 21), (T::Plus, 0, 21), (T::Tilde, 0, 21), (T::Await, 0, 21),
    (T::Pow, 22, 21),
];

// quotes of one or three and the prefixes mini reads in either case
pub const STRINGS: Strings = Strings {
    quotes: &[Quote { byte: b'"', multiline: false, triple: true }, Quote { byte: b'\'', multiline: false, triple: true }],
    prefixes: &[("f", FORMAT), ("r", RAW), ("u", 0), ("b", BYTES), ("fr", FORMAT | RAW), ("rf", FORMAT | RAW), ("br", BYTES | RAW), ("rb", BYTES | RAW)],
    any_case: true,
    hashes: false,
    adjacent: true,
    newlines: true,
    escapes: Escapes {
        simple: &[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'a', '\u{7}'), (b'b', '\u{8}'), (b'f', '\u{c}'), (b'v', '\u{b}'), (b'\\', '\\'), (b'\'', '\''), (b'"', '"')],
        hex: true,
        unicode: Unicode::Fixed,
        octal: true,
        line_join: true,
        unknown: Unknown::Keep,
        refused: &[(b'N', "named escapes are not supported, write the character or use \\u03b1")],
    },
};

pub const NUMBERS: Numbers = Numbers {
    radixes: &[(b'x', 16), (b'o', 8), (b'b', 2)],
    separator: Some(b'_'),
    zeros: Zeros::Refused,
    leading_dot: true,
    trailing_dot: true,
    exponent: true,
    suffix: Suffix::Refused,
};

// mini's f-strings with their conversions and specs and `=` fields
pub const INTERP: Interp = Interp {
    name: "f-string",
    quotes: &[],
    open: "{",
    close: b'}',
    literal: Literal::Doubled,
    short: None,
    spec: Some(b':'),
    convert: Some((b'!', &[(b'r', Mode::Repr), (b's', Mode::Str), (b'a', Mode::Ascii)])),
    echo: Some(b'='),
};

// the reference runtime's table where a header colon and indentation open each block
pub const MINI: Syntax = Syntax {
    name: "mini",
    extension: "mini",
    words: WORDS,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: LADDER,
    chained: true,
    blocks: Blocks::Indent,
    ends: Ends::Either,
    comments: Comments { line: &["//"], block: None, nested: false },
    strings: STRINGS,
    interp: Some(INTERP),
    numbers: NUMBERS,
    continuation: Continuation { join: Some(b'\\'), operator: false },
    shapes: Shapes::CORE,
    soft: &[T::Match, T::Case, T::Type, T::Fn],
    bom: true,
};

pub struct Mini;

impl Spelling for Mini {
    const SYNTAX: Syntax = MINI;
    const NATIVES: &'static edge::lang::Natives = &super::NATIVES;
}
