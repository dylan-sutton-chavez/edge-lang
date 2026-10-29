use edge::lexer::{index, operators, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Interp, Keywords, Lambda, Literal, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros, BYTES};
use edge::lexer::TokenType as T;

// the words a check is written in where a header's line end opens its block
const WORDS: &[(&str, TokenType)] = &[
    ("let", T::Let), ("fn", T::Fn), ("return", T::Return), ("use", T::Import), ("check", T::Assert),
    ("if", T::If), ("else if", T::ElseIf), ("else", T::Else), ("for", T::For), ("in", T::In), ("not in", T::NotIn),
    ("while", T::While), ("break", T::Break), ("continue", T::Continue), ("try", T::Try), ("catch", T::Catch), ("as", T::As),
    ("do", T::BlockOpen), ("end", T::BlockClose), ("and", T::And), ("or", T::Or), ("not", T::Not),
    ("nil", T::Nil), ("true", T::True), ("false", T::False),
];

const SYMBOLS: &[(&str, TokenType)] = &[
    ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose),
    ("==", T::EqualEqual), ("!=", T::NotEqual), ("<=", T::LessEqual), (">=", T::GreaterEqual), ("<", T::Less), (">", T::Greater),
    ("+=", T::PlusEqual), ("-=", T::MinusEqual), ("+", T::Plus), ("-", T::Minus), ("*", T::Star), ("/", T::Slash), ("%", T::Percent),
    ("=", T::Equal), (".", T::Dot), (",", T::Comma), (":", T::Colon),
];

static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);

static KEYWORDS: Keywords = index(WORDS);

// each operator's left and right binding power where a zero left power marks a prefix
const LADDER: &[(TokenType, u8, u8)] = &[
    (T::Or, 1, 2), (T::And, 3, 4), (T::Not, 0, 5),
    (T::EqualEqual, 7, 8), (T::NotEqual, 7, 8), (T::Less, 7, 8), (T::Greater, 7, 8), (T::LessEqual, 7, 8), (T::GreaterEqual, 7, 8),
    (T::In, 7, 8), (T::NotIn, 7, 8), (T::Plus, 9, 10), (T::Minus, 9, 10), (T::Star, 11, 12), (T::Slash, 11, 12), (T::Percent, 11, 12),
    (T::Minus, 0, 13),
];

// blocks close at end and texts fill their fields while a first store needs let
pub const UPTIME: Syntax = Syntax {
    name: "uptime",
    extension: "up",
    words: WORDS,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: LADDER,
    chained: false,
    blocks: Blocks::Words { implicit: true },
    ends: Ends::Newline,
    comments: Comments { line: &["#"], block: None, nested: false },
    strings: Strings {
        quotes: &[Quote { byte: b'"', multiline: false, triple: false }],
        prefixes: &[("b", BYTES)],
        any_case: false,
        hashes: false,
        adjacent: false,
        newlines: false,
        escapes: Escapes {
            simple: &[(b'n', '\n'), (b't', '\t'), (b'\\', '\\'), (b'"', '"')],
            hex: false,
            unicode: Unicode::Braced,
            octal: false,
            line_join: false,
            unknown: Unknown::Refuse,
            refused: &[],
        },
    },
    interp: Some(Interp { name: "text", quotes: b"\"", open: "{", close: b'}', literal: Literal::Doubled, short: None, spec: Some(b':'), convert: None, echo: None }),
    numbers: Numbers { radixes: &[], separator: Some(b'_'), zeros: Zeros::Refused, leading_dot: false, trailing_dot: false, exponent: true, suffix: Suffix::Refused },
    continuation: Continuation { join: None, operator: true },
    shapes: Shapes {
        cond: Cond::None, lambda: Lambda::None, keyword_arg: T::Equal, splat: false, slices: false, dot: Dot::Attr,
        header_parens: false, comprehensions: false, decorators: false, named: false, binding: Binding::Required, arms: false, ranges: false,
    },
    soft: &[],
    bom: true,
};

pub struct Uptime;

impl Spelling for Uptime {
    const SYNTAX: Syntax = UPTIME;
    const NATIVES: &'static edge::lang::Natives = &crate::NATIVES;
}
