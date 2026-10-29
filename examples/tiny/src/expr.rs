use edge::lexer::{index, operators, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Keywords, Lambda, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros};
use edge::lexer::TokenType as T;

const WORDS: &[(&str, TokenType)] = &[
    ("and", T::And), ("or", T::Or), ("not", T::Not), ("in", T::In), ("not in", T::NotIn),
    ("True", T::True), ("False", T::False), ("None", T::Nil),
];

const SYMBOLS: &[(&str, TokenType)] = &[
    ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose),
    ("**", T::Pow), ("//", T::FloorDiv), ("==", T::EqualEqual), ("!=", T::NotEqual), ("<=", T::LessEqual), (">=", T::GreaterEqual),
    ("<", T::Less), (">", T::Greater), ("+", T::Plus), ("-", T::Minus), ("*", T::Star), ("/", T::Slash),
    ("%", T::Percent), (".", T::Dot), (",", T::Comma), (":", T::Colon),
];

static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);

static KEYWORDS: Keywords = index(WORDS);

// the binding powers of tiny's operators where a zero left power marks a prefix operator
const LADDER: &[(TokenType, u8, u8)] = &[
    (T::Or, 1, 2), (T::And, 3, 4), (T::Not, 0, 5),
    (T::EqualEqual, 7, 8), (T::NotEqual, 7, 8), (T::Less, 7, 8), (T::Greater, 7, 8), (T::LessEqual, 7, 8), (T::GreaterEqual, 7, 8),
    (T::In, 7, 8), (T::NotIn, 7, 8),
    (T::Plus, 17, 18), (T::Minus, 17, 18),
    (T::Star, 19, 20), (T::Slash, 19, 20), (T::Percent, 19, 20), (T::FloorDiv, 19, 20),
    (T::Minus, 0, 21), (T::Plus, 0, 21),
    (T::Pow, 22, 21),
];

// expressions written as plain YAML values where a dot reads a field
pub const TINY: Syntax = Syntax {
    name: "tiny",
    extension: "flow.yaml",
    words: WORDS,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: LADDER,
    chained: true,
    blocks: Blocks::Indent,
    ends: Ends::Newline,
    comments: Comments { line: &[], block: None, nested: false },
    strings: Strings {
        quotes: &[Quote { byte: b'"', multiline: false, triple: false }, Quote { byte: b'\'', multiline: false, triple: false }],
        prefixes: &[],
        any_case: false,
        hashes: false,
        adjacent: false,
        newlines: false,
        escapes: Escapes {
            simple: &[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'\\', '\\'), (b'"', '"'), (b'\'', '\''), (b'/', '/')],
            hex: true,
            unicode: Unicode::Fixed,
            octal: false,
            line_join: false,
            unknown: Unknown::Refuse,
            refused: &[],
        },
    },
    interp: None,
    numbers: Numbers { radixes: &[], separator: None, zeros: Zeros::Refused, leading_dot: false, trailing_dot: false, exponent: true, suffix: Suffix::Refused },
    continuation: Continuation { join: None, operator: true },
    shapes: Shapes {
        cond: Cond::None, lambda: Lambda::None, keyword_arg: T::Equal, splat: false, slices: true, dot: Dot::Key,
        header_parens: false, comprehensions: false, decorators: false, named: false, binding: Binding::None, arms: false, ranges: false,
    },
    soft: &[],
    bom: false,
};

pub struct TinyExpr;

impl Spelling for TinyExpr {
    const SYNTAX: Syntax = TINY;
    const NATIVES: &'static edge::lang::Natives = &crate::NATIVES;
}
