use edge::code::OpCode as O;
use edge::lexer::grammar::{Emit, Grammar, Mode, Row};
use edge::lexer::{index, operators, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Interp, Keywords, Lambda, Literal, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros, BYTES};
use edge::lexer::TokenType as T;

const WORDS: &[(&str, TokenType)] = &[
    ("fn", T::Fn), ("return", T::Return), ("use", T::Import), ("as", T::As), ("if", T::If), ("else if", T::ElseIf), ("else", T::Else),
    ("for", T::For), ("in", T::In), ("not in", T::NotIn), ("while", T::While), ("break", T::Break), ("continue", T::Continue),
    ("try", T::Try), ("catch", T::Catch), ("finally", T::Finally), ("throw", T::Throw), ("yield", T::Yield), ("match", T::Match),
    ("and", T::And), ("or", T::Or), ("not", T::Not), ("nil", T::Nil), ("true", T::True), ("false", T::False), ("_", T::Underscore),
];

const SYMBOLS: &[(&str, TokenType)] = &[
    ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose),
    ("{", T::BlockOpen), ("}", T::BlockClose),
    ("**=", T::PowEqual), ("~/=", T::FloorDivEqual), ("<<=", T::LeftShiftEqual), (">>=", T::RightShiftEqual),
    ("!=", T::NotEqual), ("%=", T::PercentEqual), ("&=", T::BitAndEqual), ("**", T::Pow), ("*=", T::StarEqual),
    ("+=", T::PlusEqual), ("-=", T::MinusEqual), ("~/", T::FloorDiv), ("/=", T::SlashEqual), ("<<", T::LeftShift),
    ("<=", T::LessEqual), ("==", T::EqualEqual), ("=>", T::Arm), (">=", T::GreaterEqual), (">>", T::RightShift),
    ("^=", T::BitXorEqual), ("|=", T::BitOrEqual), ("%", T::Percent), ("&", T::BitAnd), ("*", T::Star), ("+", T::Plus),
    ("-", T::Minus), (".", T::Dot), ("/", T::Slash), ("<", T::Less), ("=", T::Equal), (">", T::Greater),
    ("^", T::BitXor), ("|", T::BitOr), ("~", T::Tilde), (",", T::Comma), (":", T::Colon), (";", T::Semi),
];

static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);

static KEYWORDS: Keywords = index(WORDS);

// each operator's binding powers where a zero left power marks a prefix
pub const LADDER: &[(TokenType, u8, u8)] = &[
    (T::Or, 1, 2), (T::And, 3, 4), (T::Not, 0, 5),
    (T::EqualEqual, 7, 8), (T::NotEqual, 7, 8), (T::Less, 7, 8), (T::Greater, 7, 8), (T::LessEqual, 7, 8), (T::GreaterEqual, 7, 8),
    (T::In, 7, 8), (T::NotIn, 7, 8), (T::BitOr, 9, 10), (T::BitXor, 11, 12), (T::BitAnd, 13, 14), (T::LeftShift, 15, 16), (T::RightShift, 15, 16),
    (T::Plus, 17, 18), (T::Minus, 17, 18), (T::Star, 19, 20), (T::Slash, 19, 20), (T::Percent, 19, 20), (T::FloorDiv, 19, 20),
    (T::Minus, 0, 21), (T::Plus, 0, 21), (T::Tilde, 0, 21), (T::Pow, 22, 21),
];

const fn op(tok: TokenType, left: u8, right: u8, code: O) -> Row { Row { tok, left, right, emit: Emit::Op(code) } }

// the same ladder as the rows a grammar reads values by
const ROWS: &[Row] = &[
    Row { tok: T::Or, left: 1, right: 2, emit: Emit::Lazy(false) }, Row { tok: T::And, left: 3, right: 4, emit: Emit::Lazy(true) }, op(T::Not, 0, 5, O::Not),
    op(T::EqualEqual, 7, 8, O::Eq), op(T::NotEqual, 7, 8, O::NotEq), op(T::Less, 7, 8, O::Lt), op(T::Greater, 7, 8, O::Gt),
    op(T::LessEqual, 7, 8, O::LtEq), op(T::GreaterEqual, 7, 8, O::GtEq), op(T::In, 7, 8, O::In), op(T::NotIn, 7, 8, O::NotIn),
    op(T::BitOr, 9, 10, O::BitOr), op(T::BitXor, 11, 12, O::BitXor), op(T::BitAnd, 13, 14, O::BitAnd),
    op(T::LeftShift, 15, 16, O::Shl), op(T::RightShift, 15, 16, O::Shr), op(T::Plus, 17, 18, O::Add), op(T::Minus, 17, 18, O::Sub),
    op(T::Star, 19, 20, O::Mul), op(T::Slash, 19, 20, O::Div), op(T::Percent, 19, 20, O::Mod), op(T::FloorDiv, 19, 20, O::FloorDiv),
    op(T::Minus, 0, 21, O::Minus), op(T::Plus, 0, 21, O::Pos), op(T::Tilde, 0, 21, O::BitNot), op(T::Pow, 22, 21, O::Pow),
];

// values read by the ladder and a match no arm takes calls `unmatched`
static GRAMMAR: Grammar = Grammar { modes: &[Mode { rows: ROWS, words: &[] }], unmatched: "unmatched", ..Grammar::NONE };

// one quote whose literals fill their braced fields
pub const STRINGS: Strings = Strings {
    quotes: &[Quote { byte: b'"', multiline: false, triple: false }],
    prefixes: &[("b", BYTES)],
    any_case: false,
    hashes: false,
    adjacent: false,
    newlines: false,
    escapes: Escapes {
        simple: &[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'0', '\0'), (b'\\', '\\'), (b'"', '"')],
        hex: true,
        unicode: Unicode::Braced,
        octal: false,
        line_join: false,
        unknown: Unknown::Refuse,
        refused: &[],
    },
};

pub const NUMBERS: Numbers = Numbers {
    radixes: &[(b'x', 16), (b'o', 8), (b'b', 2)],
    separator: Some(b'_'),
    zeros: Zeros::Refused,
    leading_dot: false,
    trailing_dot: false,
    exponent: true,
    suffix: Suffix::Refused,
};

// a field holds an expression and a spec after a colon
pub const INTERP: Interp = Interp { name: "text", quotes: b"\"", open: "{", close: b'}', literal: Literal::Doubled, short: None, spec: Some(b':'), convert: None, echo: None };

// braces bound blocks and a line end or a semicolon ends a statement
pub const MINI: Syntax = Syntax {
    name: "mini",
    extension: "mini",
    words: WORDS,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: LADDER,
    chained: false,
    blocks: Blocks::Braces,
    ends: Ends::Either,
    comments: Comments { line: &["//"], block: None, nested: false },
    strings: STRINGS,
    interp: Some(INTERP),
    numbers: NUMBERS,
    continuation: Continuation { join: None, operator: true },
    shapes: Shapes {
        cond: Cond::None, lambda: Lambda::Pipes, keyword_arg: T::Equal, splat: true, slices: true, dot: Dot::Attr,
        header_parens: false, comprehensions: true, decorators: false, named: false, binding: Binding::None, arms: true, ranges: false, grammar: Some(&GRAMMAR),
    },
    soft: &[],
    bom: true,
};

pub struct Mini;

impl Spelling for Mini {
    const SYNTAX: Syntax = MINI;
    const NATIVES: &'static edge::lang::Natives = &super::NATIVES;
}
