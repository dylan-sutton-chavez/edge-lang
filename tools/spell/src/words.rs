use edge::lexer::{index, operators, Blocks, Comments, Continuation, Ends, Escapes, Interp, Keywords, Literal, Mode, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros, BYTES, RAW};
use edge::lexer::TokenType as T;

const RESERVED: &[(&str, TokenType)] = &[
    ("false", T::False), ("nil", T::Nil), ("true", T::True), ("and", T::And), ("as", T::As), ("assert", T::Assert),
    ("async", T::Async), ("await", T::Await), ("break", T::Break), ("class", T::Class), ("continue", T::Continue),
    ("function", T::Fn), ("delete", T::Delete), ("elseif", T::ElseIf), ("else", T::Else), ("catch", T::Catch),
    ("finally", T::Finally), ("for", T::For), ("from", T::From), ("global", T::Global), ("if", T::If),
    ("import", T::Import), ("in", T::In), ("is", T::Is), ("fn", T::FnExpr), ("outer", T::Outer), ("not", T::Not),
    ("or", T::Or), ("skip", T::Empty), ("throw", T::Throw), ("return", T::Return), ("try", T::Try), ("while", T::While),
    ("with", T::With), ("yield", T::Yield), ("case", T::Case), ("match", T::Match), ("type", T::Type), ("_", T::Underscore),
    ("then", T::BlockOpen), ("do", T::BlockOpen), ("end", T::BlockClose), ("is not", T::IsNot), ("not in", T::NotIn),
];

const SYMBOLS: &[(&str, TokenType)] = &[
    ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose),
    ("**=", T::PowEqual), ("//=", T::FloorDivEqual), ("<<=", T::LeftShiftEqual), (">>=", T::RightShiftEqual),
    ("!=", T::NotEqual), ("%=", T::PercentEqual), ("&=", T::BitAndEqual), ("**", T::Pow), ("*=", T::StarEqual),
    ("+=", T::PlusEqual), ("-=", T::MinusEqual), ("->", T::Returns), ("...", T::Ellipsis), ("//", T::FloorDiv),
    ("/=", T::SlashEqual), (":=", T::ColonEqual), ("<<", T::LeftShift), ("<=", T::LessEqual), ("==", T::EqualEqual),
    (">=", T::GreaterEqual), (">>", T::RightShift), ("@=", T::AtEqual), ("^=", T::BitXorEqual), ("|=", T::BitOrEqual),
    ("!", T::Convert), ("%", T::Percent), ("&", T::BitAnd), ("*", T::Star), ("+", T::Plus), ("-", T::Minus),
    (".", T::Dot), ("/", T::Slash), ("<", T::Less), ("=", T::Equal), (">", T::Greater), ("@", T::At),
    ("^", T::BitXor), ("|", T::BitOr), ("~", T::Tilde), (",", T::Comma), (":", T::Colon), (";", T::Semi),
];

static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);

static KEYWORDS: Keywords = index(RESERVED);

// the tests' third table where `then` or `do` opens a block and `end` closes it
pub const WORDS: Syntax = Syntax {
    name: "words",
    extension: "words",
    words: RESERVED,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: mini::LADDER,
    chained: true,
    blocks: Blocks::Words { implicit: true },
    ends: Ends::Either,
    comments: Comments { line: &["--"], block: Some(("/*", "*/")), nested: true },
    strings: Strings {
        quotes: &[Quote { byte: b'"', multiline: true, triple: false }, Quote { byte: b'\'', multiline: true, triple: false }],
        prefixes: &[("r", RAW), ("b", BYTES), ("br", BYTES | RAW)],
        any_case: false,
        hashes: true,
        adjacent: true,
        newlines: true,
        escapes: Escapes {
            simple: &[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'0', '\0'), (b'\\', '\\'), (b'\'', '\''), (b'"', '"')],
            hex: true,
            unicode: Unicode::Braced,
            octal: false,
            line_join: false,
            unknown: Unknown::Refuse,
            refused: &[],
        },
    },
    interp: Some(Interp {
        name: "template",
        quotes: b"\"",
        open: "#{",
        close: b'}',
        literal: Literal::Backslash,
        short: None,
        spec: Some(b':'),
        convert: Some((b'!', &[(b'r', Mode::Repr), (b's', Mode::Str), (b'a', Mode::Ascii)])),
        echo: Some(b'='),
    }),
    numbers: Numbers {
        radixes: &[(b'x', 16), (b'o', 8), (b'b', 2)],
        separator: Some(b'_'),
        zeros: Zeros::Allowed,
        leading_dot: false,
        trailing_dot: false,
        exponent: true,
        suffix: Suffix::Refused,
    },
    continuation: Continuation { join: None, operator: true },
    shapes: Shapes::CORE,
    soft: &[T::Type],
    bom: true,
};

pub struct Words;

impl Spelling for Words {
    const SYNTAX: Syntax = WORDS;
    const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
}
