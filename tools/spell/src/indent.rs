use edge::lexer::{index, operators, Blocks, Comments, Continuation, Ends, Keywords, Shapes, Spelling, Syntax, TokenType};
use edge::lexer::TokenType as T;

const WORDS: &[(&str, TokenType)] = &[
    ("False", T::False), ("None", T::Nil), ("True", T::True), ("and", T::And), ("as", T::As), ("assert", T::Assert),
    ("async", T::Async), ("await", T::Await), ("break", T::Break), ("class", T::Class), ("continue", T::Continue),
    ("def", T::Fn), ("del", T::Delete), ("elif", T::ElseIf), ("else", T::Else), ("except", T::Catch), ("finally", T::Finally),
    ("for", T::For), ("from", T::From), ("global", T::Global), ("if", T::If), ("import", T::Import), ("in", T::In),
    ("is", T::Is), ("lambda", T::FnExpr), ("nonlocal", T::Outer), ("not", T::Not), ("or", T::Or), ("pass", T::Empty),
    ("raise", T::Throw), ("return", T::Return), ("try", T::Try), ("while", T::While), ("with", T::With),
    ("yield", T::Yield), ("case", T::Case), ("match", T::Match), ("type", T::Type), ("_", T::Underscore),
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

static KEYWORDS: Keywords = index(WORDS);

// the token table the corpus was written in before mini where indentation opens a block
pub const INDENT: Syntax = Syntax {
    name: "indent",
    extension: "py",
    words: WORDS,
    symbols: SYMBOLS,
    operators: &OPERATORS,
    keywords: &KEYWORDS,
    precedence: mini::LADDER,
    chained: true,
    blocks: Blocks::Indent,
    ends: Ends::Either,
    comments: Comments { line: &["#"], block: None, nested: false },
    strings: mini::STRINGS,
    interp: Some(mini::INTERP),
    numbers: mini::NUMBERS,
    continuation: Continuation { join: Some(b'\\'), operator: false },
    shapes: Shapes::CORE,
    soft: &[T::Match, T::Case, T::Type],
    bom: true,
};

pub struct Indent;

impl Spelling for Indent {
    const SYNTAX: Syntax = INDENT;
    const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
}
