use edge::lexer::{index, operators, Blocks, Comments, Continuation, Ends, Keywords, Shapes, Spelling, Syntax, TokenType};
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
    ("{", T::BlockOpen), ("}", T::BlockClose),
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

// the table the corpus was written in before mini read indentation
pub const BRACES: Syntax = Syntax {
    name: "braces",
    extension: "mini",
    words: WORDS,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: mini::LADDER,
    chained: true,
    blocks: Blocks::Braces,
    ends: Ends::Either,
    comments: Comments { line: &["//"], block: None, nested: false },
    strings: mini::STRINGS,
    interp: Some(mini::INTERP),
    numbers: mini::NUMBERS,
    continuation: Continuation { join: Some(b'\\'), operator: false },
    shapes: Shapes::CORE,
    soft: &[T::Match, T::Case, T::Type, T::Fn],
    bom: true,
};

pub struct Braces;

impl Spelling for Braces {
    const SYNTAX: Syntax = BRACES;
    const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
}
