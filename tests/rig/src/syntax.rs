use interpreter::lang::{Cmp, Op};
use interpreter::value::foreign::{BinOp, UnOp};
use interpreter::lexer::grammar::{Emit, Form, Grammar, Reading, Row, Slot};
use interpreter::lexer::{BYTES, Comments, Cond, Continuation, Ends, Escapes, Interp, Keywords, Lambda, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros, index, operators};
use interpreter::lexer::TokenType as T;

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
    ("->", T::Returns), ("<=", T::LessEqual), ("==", T::EqualEqual), ("=>", T::Arm), (">=", T::GreaterEqual), (">>", T::RightShift),
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

const fn op(tok: TokenType, left: u8, right: u8, code: Op) -> Row { Row::new(tok, left, right, Emit::Op(code)) }

// the same ladder as the rows a grammar reads values by
const ROWS: &[Row] = &[
    Row::new(T::Or, 1, 2, Emit::Lazy(false)), Row::new(T::And, 3, 4, Emit::Lazy(true)), op(T::Not, 0, 5, Op::Not),
    op(T::EqualEqual, 7, 8, Op::Cmp(Cmp::Eq)), op(T::NotEqual, 7, 8, Op::Cmp(Cmp::Ne)), op(T::Less, 7, 8, Op::Cmp(Cmp::Lt)), op(T::Greater, 7, 8, Op::Cmp(Cmp::Gt)),
    op(T::LessEqual, 7, 8, Op::Cmp(Cmp::Le)), op(T::GreaterEqual, 7, 8, Op::Cmp(Cmp::Ge)), op(T::In, 7, 8, Op::In), op(T::NotIn, 7, 8, Op::NotIn),
    op(T::BitOr, 9, 10, Op::Bin(BinOp::Or)), op(T::BitXor, 11, 12, Op::Bin(BinOp::Xor)), op(T::BitAnd, 13, 14, Op::Bin(BinOp::And)),
    op(T::LeftShift, 15, 16, Op::Bin(BinOp::Shl)), op(T::RightShift, 15, 16, Op::Bin(BinOp::Shr)), op(T::Plus, 17, 18, Op::Bin(BinOp::Add)), op(T::Minus, 17, 18, Op::Bin(BinOp::Sub)),
    op(T::Star, 19, 20, Op::Bin(BinOp::Mul)), op(T::Slash, 19, 20, Op::Bin(BinOp::TrueDiv)), op(T::Percent, 19, 20, Op::Bin(BinOp::Mod)), op(T::FloorDiv, 19, 20, Op::Bin(BinOp::FloorDiv)),
    op(T::Minus, 0, 21, Op::Unary(UnOp::Neg)), op(T::Plus, 0, 21, Op::Unary(UnOp::Pos)), op(T::Tilde, 0, 21, Op::Unary(UnOp::Invert)), op(T::Pow, 22, 21, Op::Bin(BinOp::Pow)),
];

// limits where one leads another and one takes a unit word after its count
const LIMITS: &[(&str, &str)] = &[("near values", "near_values"), ("near", "near"), ("time _ s", "time"), ("work", "work")];

// forms over several bound names, braced items, generators and limits each calling a function of the program
const FORMS: &[Form] = &[
    Form::new("lift", &[Slot::Bind, Slot::Word(":"), Slot::Body(0)], "lifted"),
    Form::new("gather", &[Slot::Items(0)], "gathered"),
    Form::new("cross", &[Slot::Each(0)], "crossed"),
    Form::new("quota", &[Slot::Limits("within", LIMITS)], "quoted"),
];

// values read by the ladder and a match no arm takes calls `unmatched`
static GRAMMAR: Grammar = Grammar::PLAIN.readings(&[Reading::new(ROWS, &[])]).forms(FORMS).unmatched("unmatched");

// one quote whose literals fill their braced fields
pub const STRINGS: Strings = Strings::PLAIN
    .quotes(&[Quote::PLAIN])
    .prefixes(&[("b", BYTES)])
    .escapes(Escapes::PLAIN
        .simple(&[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'0', '\0'), (b'\\', '\\'), (b'"', '"')])
        .hex(true)
        .unicode(Unicode::Braced)
        .unknown(Unknown::Refuse));

pub const NUMBERS: Numbers = Numbers::PLAIN
    .radixes(&[(b'x', 16), (b'o', 8), (b'b', 2)])
    .separator(Some(b'_'))
    .zeros(Zeros::Refused)
    .exponent(true)
    .suffix(Suffix::Refused);

// a field holds an expression and a spec after a colon
pub const INTERP: Interp = Interp::PLAIN.name("text").quotes(b"\"").spec(Some(b':'));

// braces bound blocks and a line end or a semicolon ends a statement
pub const RIG: Syntax = Syntax::PLAIN
    .name("rig")
    .extension("rig")
    .words(WORDS)
    .symbols(SYMBOLS)
    .operators(&OPERATORS)
    .keywords(&KEYWORDS)
    .precedence(LADDER)
    .ends(Ends::Either)
    .comments(Comments::PLAIN.line(&["//"]))
    .strings(STRINGS)
    .interp(Some(INTERP))
    .numbers(NUMBERS)
    .continuation(Continuation::PLAIN.operator(true))
    .shapes(Shapes::PLAIN
        .cond(Cond::None)
        .lambda(Lambda::Pipes)
        .decorators(false)
        .named(false)
        .arms(true)
        .grammar(Some(&GRAMMAR)))
    .bom(true);

pub struct Rig;

impl Spelling for Rig {
    const SYNTAX: Syntax = RIG;
    const NATIVES: &'static interpreter::lang::Natives = &super::NATIVES;
}
