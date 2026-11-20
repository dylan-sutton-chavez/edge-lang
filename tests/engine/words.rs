use interpreter::lexer::{Blocks, BYTES, Comments, Continuation, Conversion, Ends, Escapes, FORMAT, Interp, Keywords, Literal, Numbers, Quote, RAW, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros, index, operators};
use interpreter::lexer::TokenType as T;

const RESERVED: &[(&str, TokenType)] = &[
    ("false", T::False), ("nil", T::Nil), ("true", T::True), ("and", T::And), ("as", T::As), ("assert", T::Assert),
    ("break", T::Break), ("class", T::Class), ("continue", T::Continue),
    ("function", T::Fn), ("elseif", T::ElseIf), ("else", T::Else), ("catch", T::Catch),
    ("finally", T::Finally), ("for", T::For), ("from", T::From), ("if", T::If),
    ("import", T::Import), ("in", T::In), ("is", T::Is), ("fn", T::FnExpr), ("not", T::Not),
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

// the tests' words table where `then` or `do` opens a block and `end` closes it
pub(crate) const WORDS: Syntax = Syntax::PLAIN
    .name("words")
    .extension("words")
    .words(RESERVED)
    .symbols(SYMBOLS)
    .operators(&OPERATORS)
    .keywords(&KEYWORDS)
    .precedence(LADDER)
    .chained(true)
    .blocks(Blocks::Words { implicit: true })
    .ends(Ends::Either)
    .comments(Comments::PLAIN.line(&["--"]).block(Some(("/*", "*/"))).nested(true))
    .strings(Strings::PLAIN
        .quotes(&[Quote::PLAIN.multiline(true), Quote::PLAIN.byte(b'\'').multiline(true)])
        .prefixes(&[("r", RAW), ("b", BYTES), ("br", BYTES | RAW)])
        .hashes(true)
        .adjacent(true)
        .newlines(true)
        .escapes(Escapes::PLAIN
            .simple(&[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'0', '\0'), (b'\\', '\\'), (b'\'', '\''), (b'"', '"')])
            .hex(true)
            .unicode(Unicode::Braced)
            .unknown(Unknown::Refuse)))
    .interp(Some(Interp::PLAIN
        .name("template")
        .quotes(b"\"")
        .open("#{")
        .literal(Literal::Backslash)
        .spec(Some(b':'))
        .convert(Some((b'!', &[(b'r', Conversion::Repr), (b's', Conversion::Str), (b'a', Conversion::Ascii)])))
        .echo(Some(b'='))))
    .numbers(Numbers::PLAIN
        .radixes(&[(b'x', 16), (b'o', 8), (b'b', 2)])
        .separator(Some(b'_'))
        .exponent(true)
        .suffix(Suffix::Refused))
    .continuation(Continuation::PLAIN.operator(true))
    .soft(&[T::Type])
    .bom(true);

pub(crate) struct Words;

impl Spelling for Words {
    const SYNTAX: Syntax = WORDS;
    const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
}

const BRACE_WORDS: &[(&str, TokenType)] = &[
    ("false", T::False), ("nil", T::Nil), ("true", T::True), ("as", T::As), ("assert", T::Assert),
    ("break", T::Break), ("class", T::Class), ("continue", T::Continue),
    ("fn", T::Fn), ("else if", T::ElseIf), ("else", T::Else), ("except", T::Catch),
    ("finally", T::Finally), ("for", T::For), ("from", T::From), ("if", T::If),
    ("import", T::Import), ("in", T::In), ("is", T::Is), ("lambda", T::FnExpr),
    ("pass", T::Empty), ("raise", T::Throw), ("return", T::Return), ("try", T::Try), ("while", T::While),
    ("with", T::With), ("yield", T::Yield), ("case", T::Case), ("match", T::Match), ("type", T::Type),
    ("_", T::Underscore),
];

const BRACE_SYMBOLS: &[(&str, TokenType)] = &[
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

static BRACE_OPERATORS: [[TokenType; 4]; 128] = operators(BRACE_SYMBOLS);

static BRACE_KEYWORDS: Keywords = index(BRACE_WORDS);

// the binding powers of the fixed core where a zero left power marks a prefix operator
pub(crate) const LADDER: &[(TokenType, u8, u8)] = &[
    (T::Or, 1, 2), (T::And, 3, 4), (T::Not, 0, 5),
    (T::EqualEqual, 7, 8), (T::NotEqual, 7, 8), (T::Less, 7, 8), (T::Greater, 7, 8), (T::LessEqual, 7, 8), (T::GreaterEqual, 7, 8),
    (T::In, 7, 8), (T::NotIn, 7, 8), (T::Is, 7, 8), (T::IsNot, 7, 8), (T::Not, 7, 8),
    (T::BitOr, 9, 10), (T::BitXor, 11, 12), (T::BitAnd, 13, 14), (T::LeftShift, 15, 16), (T::RightShift, 15, 16),
    (T::Plus, 17, 18), (T::Minus, 17, 18),
    (T::Star, 19, 20), (T::Slash, 19, 20), (T::Percent, 19, 20), (T::FloorDiv, 19, 20), (T::At, 19, 20),
    (T::Minus, 0, 21), (T::Plus, 0, 21), (T::Tilde, 0, 21),
    (T::Pow, 22, 21),
];

// quotes of one or three and prefixes read in either case
pub(crate) const STRINGS: Strings = Strings::PLAIN
    .quotes(&[Quote::PLAIN.triple(true), Quote::PLAIN.byte(b'\'').triple(true)])
    .prefixes(&[("f", FORMAT), ("r", RAW), ("u", 0), ("b", BYTES), ("fr", FORMAT | RAW), ("rf", FORMAT | RAW), ("br", BYTES | RAW), ("rb", BYTES | RAW)])
    .any_case(true)
    .adjacent(true)
    .newlines(true)
    .escapes(Escapes::PLAIN
        .simple(&[(b'n', '\n'), (b't', '\t'), (b'r', '\r'), (b'a', '\u{7}'), (b'b', '\u{8}'), (b'f', '\u{c}'), (b'v', '\u{b}'), (b'\\', '\\'), (b'\'', '\''), (b'"', '"')])
        .hex(true)
        .unicode(Unicode::Fixed)
        .octal(true)
        .line_join(true)
        .refused(&[(b'N', "named escapes are not supported, write the character or use \\u03b1")]));

pub(crate) const NUMBERS: Numbers = Numbers::PLAIN
    .radixes(&[(b'x', 16), (b'o', 8), (b'b', 2)])
    .separator(Some(b'_'))
    .zeros(Zeros::Refused)
    .leading_dot(true)
    .trailing_dot(true)
    .exponent(true)
    .suffix(Suffix::Refused);

// f-strings with their conversions and specs and `=` fields
pub(crate) const INTERP: Interp = Interp::PLAIN
    .name("f-string")
    .spec(Some(b':'))
    .convert(Some((b'!', &[(b'r', Conversion::Repr), (b's', Conversion::Str), (b'a', Conversion::Ascii)])))
    .echo(Some(b'='));

// the kit's fixed core read in braces where the corpus once was written
pub(crate) const BRACES: Syntax = Syntax::PLAIN
    .name("braces")
    .extension("rig")
    .words(BRACE_WORDS)
    .symbols(BRACE_SYMBOLS)
    .operators(&BRACE_OPERATORS)
    .keywords(&BRACE_KEYWORDS)
    .precedence(LADDER)
    .chained(true)
    .ends(Ends::Either)
    .comments(Comments::PLAIN.line(&["//"]))
    .strings(STRINGS)
    .interp(Some(INTERP))
    .numbers(NUMBERS)
    .continuation(Continuation::PLAIN.join(Some(b'\\')))
    .soft(&[T::Match, T::Case, T::Type, T::Fn])
    .bom(true);

pub(crate) struct Braces;

impl Spelling for Braces {
    const SYNTAX: Syntax = BRACES;
    const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
}

const INDENT_WORDS: &[(&str, TokenType)] = &[
    ("False", T::False), ("None", T::Nil), ("True", T::True), ("and", T::And), ("as", T::As), ("assert", T::Assert),
    ("break", T::Break), ("class", T::Class), ("continue", T::Continue),
    ("def", T::Fn), ("elif", T::ElseIf), ("else", T::Else), ("except", T::Catch), ("finally", T::Finally),
    ("for", T::For), ("from", T::From), ("if", T::If), ("import", T::Import), ("in", T::In),
    ("is", T::Is), ("lambda", T::FnExpr), ("not", T::Not), ("or", T::Or), ("pass", T::Empty),
    ("raise", T::Throw), ("return", T::Return), ("try", T::Try), ("while", T::While), ("with", T::With),
    ("yield", T::Yield), ("case", T::Case), ("match", T::Match), ("type", T::Type), ("_", T::Underscore),
];

static INDENT_KEYWORDS: Keywords = index(INDENT_WORDS);

// the kit table where a header colon and indentation open each block
pub(crate) const INDENT: Syntax = BRACES
    .name("indent")
    .extension("py")
    .words(INDENT_WORDS)
    .symbols(SYMBOLS)
    .operators(&OPERATORS)
    .keywords(&INDENT_KEYWORDS)
    .blocks(Blocks::Indent)
    .comments(Comments::PLAIN.line(&["#"]))
    .soft(&[T::Match, T::Case, T::Type]);

pub(crate) struct Indent;

impl Spelling for Indent {
    const SYNTAX: Syntax = INDENT;
    const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
}

const LEGACY_SYMBOLS: &[(&str, TokenType)] = &[
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

static LEGACY_OPERATORS: [[TokenType; 4]; 128] = operators(LEGACY_SYMBOLS);

// the kit's fixed core read with indentation in the words rig once had
pub(crate) const LEGACY: Syntax = BRACES
    .name("legacy")
    .symbols(LEGACY_SYMBOLS)
    .operators(&LEGACY_OPERATORS)
    .blocks(Blocks::Indent);

pub(crate) struct Legacy;

impl Spelling for Legacy {
    const SYNTAX: Syntax = LEGACY;
    const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
}

/* `src` compiled in the legacy fixture for engine parts rig no longer spells */
pub(crate) fn legacy(src: &str) -> (interpreter::code::SSAChunk, Vec<interpreter::Diagnostic>) { interpreter::compile_as::<Legacy>(src) }
