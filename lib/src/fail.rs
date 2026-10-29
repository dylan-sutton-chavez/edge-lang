use alloc::string::String;
use alloc::vec;

use edge::lang::{Cx, KindId, Mode};
use edge::value::{Val, VmErr};

use crate::{Lang, Site};

/* the classes a failure raises past the type and value errors the engine names */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class { Value, Overflow, Index, Key, Lookup, Decode, Encode, End, Exit, Runtime, ZeroDiv }

/* the native a failure happened in as its table entry names it */
#[derive(Clone, Copy, Debug)]
pub struct At {
    pub name: &'static str,
    pub params: &'static [&'static str],
    pub keywords: &'static [&'static str],
}

impl At {
    // a failure raised where no native was called
    pub const NONE: At = At { name: "", params: &[], keywords: &[] };

    pub const fn of<S: Site>() -> At { At { name: S::NAME, params: S::PARAMS, keywords: S::KEYWORDS } }

    /* positional parameter `i` as the runtime names it */
    pub fn param(&self, i: usize) -> &'static str { self.params.get(i).copied().unwrap_or("") }

    /* keyword only slot `i` as the runtime names it */
    pub fn keyword(&self, i: usize) -> &'static str { self.keywords.get(i).copied().unwrap_or("") }
}

/* why a format spec was refused */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Spec {
    ZeroInText, ZeroInInt, AltWithChar, SignWithChar, AltInText, SpaceInText, SignInText, EqInText, PrecisionInInt,
    // a separator then the type it cannot go with
    SepWith(u8, u8),
    SepTwice(u8),
    SepBoth,
    BadWidth, BadPrecision, MissingPrecision, BadType,
    NeedsInt, NeedsNumber, CharNeedsInt, CharRange, CharWide, CharInvalid,
}

/* why a codec could not write or read a span */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Coding {
    // a character or byte the codec holds no mapping for
    Unmapped,
    // the data ends inside a character
    End,
    // a byte no character starts with
    Start,
    // a byte that does not continue the character before it
    Continuation,
    // fewer bytes than one unit of the codec
    Truncated,
    // a high surrogate with no low one after it
    Surrogate,
    // a low surrogate with no high one before it
    LowFirst,
    // a code point among the surrogates
    SurrogateRange,
    // a code point past the last one unicode holds
    PastUnicode,
}

impl Coding {
    /* lib's neutral words for the reason */
    pub const fn words(self) -> &'static str {
        match self {
            Coding::Unmapped => "which the codec maps to nothing",
            Coding::End => "where the data ends inside a character",
            Coding::Start => "which starts no character",
            Coding::Continuation => "which does not continue the character before it",
            Coding::Truncated => "where the data ends inside a unit",
            Coding::Surrogate => "a high surrogate with no low surrogate after it",
            Coding::LowFirst => "a low surrogate with no high surrogate before it",
            Coding::SurrogateRange => "a code point among the surrogates",
            Coding::PastUnicode => "a code point past 0x10FFFF",
        }
    }
}

/* why a JSON text does not read */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Json {
    Utf8, Deep, Unexpected, End, Extra, CommaOrBracket, CommaOrBrace, Key, Colon, Literal,
    Control, Unterminated, Escape, Unicode, Surrogate, Number, LeadingZero,
}

impl Json {
    /* lib's neutral words for the reason */
    pub const fn words(self) -> &'static str {
        match self {
            Json::Utf8 => "invalid utf-8",
            Json::Deep => "nested past 512 levels",
            Json::Unexpected => "unexpected character",
            Json::End => "unexpected end",
            Json::Extra => "extra text after the value",
            Json::CommaOrBracket => "expected a comma or a closing bracket",
            Json::CommaOrBrace => "expected a comma or a closing brace",
            Json::Key => "expected a key",
            Json::Colon => "expected a colon",
            Json::Literal => "invalid literal",
            Json::Control => "control character in a string",
            Json::Unterminated => "unterminated string",
            Json::Escape => "invalid escape",
            Json::Unicode => "invalid unicode escape",
            Json::Surrogate => "lone surrogate",
            Json::Number => "invalid number",
            Json::LeadingZero => "a number has a leading zero",
        }
    }
}

/* why a value has no JSON text */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToJson { Deep, Cycle, Key, Kind, Infinite }

impl ToJson {
    /* lib's neutral words for the reason */
    pub const fn words(self) -> &'static str {
        match self {
            ToJson::Deep => "nests past 512 levels",
            ToJson::Cycle => "holds itself",
            ToJson::Key => "cannot be an object key",
            ToJson::Kind => "has no JSON form",
            ToJson::Infinite => "value is not finite",
        }
    }
}

/* why a CSV record does not read */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Csv { Unclosed }

impl Csv {
    /* lib's neutral words for the reason */
    pub const fn words(self) -> &'static str { match self { Csv::Unclosed => "a quoted field never closes" } }
}

// each reason shows in lib's words so a runtime may print one as it stands
macro_rules! shown_as_words {
    ($($t:ty),*) => {$(
        impl core::fmt::Display for $t {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str(self.words()) }
        }
    )*};
}

shown_as_words!(Coding, Json, ToJson, Csv);

/* a failure of a kernel which the runtime words */
#[derive(Clone, Debug)]
pub enum Fail {
    NotIterable(Val),
    NotIterator(Val),
    NotReversible(Val),
    NotIndex(Val),
    NoLength(Val),
    // a one operand native given a value it cannot take
    BadOperand(Val),
    // a two operand native given values it cannot take together
    BadOperands(Val, Val),
    NotOrdered(Val, Val),
    MissingKey(Val),
    // a removal from an empty map or set whose receiver names the kind
    EmptyMap,
    EmptySet(Val),
    EmptyPop(Val),
    PopRange,
    // a search over the receiver that found nothing
    NotFound(Val),
    NotInSequence,
    IndexType(Val),
    SubstringNotFound,
    SubsectionNotFound,
    EmptySeparator,
    // positional parameter `at` given a value of the wrong kind
    ParamType { at: u8, v: Val, want: KindId },
    ParamLength(u8),
    ParamAscii(u8),
    // parameter `at` given with no text to convert
    ParamWithoutText(u8),
    TextWithoutCodec,
    // keyword only slot `at` that takes text or nil
    KeywordText { at: u8, v: Val },
    KeywordDefault,
    AttrName(Val),
    TypeArg,
    // a handler naming something that is no class
    HandlerArg,
    ClassArg,
    ClassesArg,
    CodePoint,
    CharKind(Val),
    CharLength(usize),
    ByteRange,
    BytesRange,
    NegativeCount,
    // an int past what an index holds raised as the class it gives
    IndexSized(Class),
    CannotConvert(Val),
    BufferArg(Val),
    ByteArg(Val),
    BufferFill(Val),
    TextFill,
    TabsWide,
    SliceIndex,
    AffixType { v: Val, want: KindId },
    AffixItem { v: Val, want: KindId },
    JoinIterable,
    JoinItem { at: usize, v: Val, want: KindId },
    // a record to write that is no list or tuple
    NotRecord(Val),
    ExtendText,
    ExtendType { recv: Val, v: Val },
    ValueNotFound(Val),
    Resize(i64),
    TakeRange { take: i64, len: i64 },
    HexArg(Val),
    // the argument of a native taking text or bytes given something else
    TextOrBytes(Val),
    // a chunk of neither text nor bytes or a rest of the other family
    LineKinds { chunk: Val, rest: Val },
    // the argument of a one argument native that takes text
    NeedText(Val),
    // positional argument `at` counted from one that takes text
    TextArg { at: u8, v: Val },
    MustBeText(Val),
    SepText(Val),
    // a separator that is no single character or is a quote or a line break
    SepChar(Val),
    StripArg,
    FillText(Val),
    HexOdd,
    HexDigit(usize),
    HexFloat,
    HexFloatRange,
    DecodeNeedsBuffer(Val),
    DecodeHandler,
    UnknownCodec(String),
    UnknownHandler(String),
    // a span a codec could not write with its first character and length
    Encode { codec: crate::lang::Codec, first: char, at: usize, len: usize, why: Coding },
    // a span a codec could not read with its first byte and its length
    Decode { codec: crate::lang::Codec, first: u8, at: usize, len: usize, why: Coding },
    // a JSON text that does not read at byte `at` for the reason `why`
    BadJson { at: usize, why: Json },
    // a value JSON cannot hold and why
    NotJson { v: Val, why: ToJson },
    // a CSV record that does not read at byte `at` for the reason `why`
    BadCsv { at: usize, why: Csv },
    IntMissing,
    IntArg(Val),
    IntBaseType,
    // a count start that is no int
    StartType,
    IntBase,
    IntExplicitBase,
    IntLiteral { base: i64, v: Val },
    FloatArg(Val),
    FloatLiteral(Val),
    // text that spells no RFC 3339 time
    BadTime(Val),
    // a time moved past the years a time reads in
    TimeRange,
    // a duration past the nanoseconds one holds
    DurationRange,
    // a field read on a value of another kind than the field's named `want`
    FieldOf { recv: Val, want: &'static str },
    FloatNan,
    FloatInf,
    RoundArg,
    RoundRange,
    PowArgs,
    PowThird,
    PowZero,
    PowInverse,
    ExtremeEmpty,
    SumText(Val),
    PairNotIterable,
    PairLength { at: usize, len: usize },
    RangeArgs,
    ZeroStep,
    IterCallable,
    SortCount(usize),
    SortPositional,
    SortKeyword(String),
    ByteOrder,
    LengthNegative,
    LengthType,
    TakeType,
    // a native taking immutable bytes first given something else
    BytesFirst,
    NegativeUnsigned,
    TooBig,
    WideToFloat,
    WideDivFloat,
    NegativeShift,
    ShiftDigits,
    ZeroNegativePower,
    Digits(Option<usize>),
    Spec(Spec),
    SpecInvalid { spec: String, v: Val },
    SpecUnknown { code: char, v: Val },
    SpecRefused(Val),
    SpecWidth,
    SpecPrecision,
    EndOfInput,
    ExitCode(Val),
    Exit(Val),
    SendText(u8),
    NoScheduler,
}

/* a failure the runtime words or an error the engine raised */
pub enum Error { Fail(Fail), Vm(VmErr) }

impl From<VmErr> for Error { fn from(e: VmErr) -> Self { Error::Vm(e) } }

impl From<Fail> for Error { fn from(f: Fail) -> Self { Error::Fail(f) } }

impl Error {
    /* the error this raises in the runtime's words for the native of site `S` */
    #[cold]
    pub fn raise<S: Site>(self, cx: &mut Cx<'_, '_>) -> VmErr { match self { Error::Fail(f) => fail::<S>(cx, f), Error::Vm(e) => e } }
}

/* a method run on a receiver of another kind which the engine never allows */
#[cold]
#[inline(never)]
pub fn receiver() -> VmErr { VmErr::Type("a method ran on a receiver of another kind") }

/* the error `f` raises in the runtime's words for the native of site `S` */
#[cold]
#[inline(never)]
pub fn fail<S: Site>(cx: &mut Cx<'_, '_>, f: Fail) -> VmErr { <S::Lang as Lang>::words(cx, At::of::<S>(), f) }

/* `text` raised as class `c` the runtime names */
#[cold]
pub fn raise<L: Lang>(c: Class, text: &str) -> VmErr { raised::<L>(c, &[text]) }

/* the text of `parts` raised as class `c` in one allocation of its exact length */
#[cold]
pub fn raised<L: Lang>(c: Class, parts: &[&str]) -> VmErr {
    let class = L::class(c);
    let mut out = String::with_capacity(class.len() + 2 + parts.iter().map(|p| p.len()).sum::<usize>());
    out.push_str(class);
    out.push_str(": ");
    for p in parts { out.push_str(p); }
    VmErr::Raised(out)
}

/* `parts` as one text allocated once at its exact length */
pub fn joined(parts: &[&str]) -> String {
    let mut out = String::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts { out.push_str(p); }
    out
}

/* the type a failure names for `v` where an exception names its class */
pub fn type_of<'a>(cx: &'a Cx<'_, '_>, v: Val) -> &'a str { cx.type_name(v) }

/* the type name of `v` where nil reads as the runtime's word for it */
pub fn shown<'a, L: Lang>(cx: &'a Cx<'_, '_>, v: Val) -> &'a str { if v.is_none() { L::NAMES.nil } else { cx.type_name(v) } }

/* lib's words in plain neutral english which every runtime starts from */
pub fn english<L: Lang>(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr {
    let name = who(at.name);
    match f {
        Fail::MissingKey(k) => key::<L>(cx, k),
        Fail::EmptyMap => { let map = cx.names().kind(KindId::Dict); raised::<L>(Class::Key, &[name, " found the ", map, " empty"]) }
        Fail::EmptySet(recv) => { let t = joined(&[name, " found the ", cx.type_name(recv), " empty"]); raise::<L>(Class::Key, &t) }
        Fail::IntLiteral { base, v } => match repr::<L>(cx, v) {
            Ok(r) if base == 0 => raised::<L>(Class::Value, &[&r, " spells no ", cx.names().kind(KindId::Int)]),
            Ok(r) => raised::<L>(Class::Value, &[&r, " spells no ", cx.names().kind(KindId::Int), " in base ", itoa::Buffer::new().format(base)]),
            Err(e) => e,
        },
        Fail::FloatLiteral(v) => match repr::<L>(cx, v) {
            Ok(r) => raised::<L>(Class::Value, &[&r, " spells no ", cx.names().kind(KindId::Float)]),
            Err(e) => e,
        },
        Fail::BadTime(v) => match repr::<L>(cx, v) {
            Ok(r) => raised::<L>(Class::Value, &["invalid RFC 3339 time: ", &r]),
            Err(e) => e,
        },
        Fail::Exit(code) => {
            let text = joined(&[itoa::Buffer::new().format(code.as_int())]);
            cx.raised(L::class(Class::Exit), vec![code], &text)
        }
        f => plain::<L>(cx, at, f),
    }
}

/* the name a failure gives its native and words for a call of no name */
fn who(name: &'static str) -> &'static str { if name.is_empty() { "the call" } else { name } }

/* positional parameter `i` by its name or by its place counted from one */
fn slot(at: &At, i: u8) -> String {
    match at.param(i as usize) { "" => joined(&["argument ", itoa::Buffer::new().format(u16::from(i) + 1)]), p => String::from(p) }
}

/* the words of failures naming only types and the native */
fn plain<L: Lang>(cx: &Cx<'_, '_>, at: At, f: Fail) -> VmErr {
    let name = who(at.name);
    let ty = |v: Val| cx.type_name(v);
    let kind = |k: KindId| cx.names().kind(k);
    let type_error = |parts: &[&str]| VmErr::TypeMsg(joined(parts));
    match f {
        Fail::NotIterable(v) => type_error(&["cannot iterate over ", ty(v)]),
        Fail::NotIterator(v) => type_error(&[ty(v), " is no iterator"]),
        Fail::NotReversible(v) => type_error(&["cannot reverse ", ty(v)]),
        Fail::NotIndex(v) => type_error(&[name, " takes ", kind(KindId::Int), ", not ", ty(v)]),
        Fail::NoLength(v) => type_error(&[ty(v), " has no length"]),
        Fail::BadOperand(v) => type_error(&[name, " does not apply to ", ty(v)]),
        Fail::BadOperands(a, b) => type_error(&[name, " does not apply to ", ty(a), " and ", ty(b)]),
        Fail::NotOrdered(a, b) => type_error(&["cannot compare ", ty(a), " and ", ty(b)]),
        Fail::EmptyPop(recv) => raised::<L>(Class::Index, &[name, " found the ", ty(recv), " empty"]),
        Fail::PopRange => raised::<L>(Class::Index, &[name, " found no item at that index"]),
        Fail::NotFound(recv) => raised::<L>(Class::Value, &[name, " found no such item in the ", ty(recv)]),
        Fail::NotInSequence => raised::<L>(Class::Value, &[name, " found no such item"]),
        Fail::IndexType(recv) => type_error(&["an index of ", ty(recv), " must be ", kind(KindId::Int)]),
        Fail::SubstringNotFound | Fail::SubsectionNotFound => raised::<L>(Class::Value, &[name, " found no match"]),
        Fail::EmptySeparator => raised::<L>(Class::Value, &[name, " takes a separator that is not empty"]),
        Fail::ParamType { at: i, v, want } => type_error(&[name, " takes ", kind(want), " as ", &slot(&at, i), ", not ", shown::<L>(cx, v)]),
        Fail::ParamLength(i) => raised::<L>(Class::Value, &[&slot(&at, i), " of ", name, " must be one character"]),
        Fail::ParamAscii(i) => raised::<L>(Class::Value, &[&slot(&at, i), " of ", name, " must be ascii"]),
        Fail::ParamWithoutText(i) => type_error(&[name, " takes ", &slot(&at, i), " only with ", kind(KindId::Str), " to convert"]),
        Fail::TextWithoutCodec => type_error(&[name, " needs ", &slot(&at, 1), " to convert ", kind(KindId::Str)]),
        Fail::KeywordText { at: i, v } => type_error(&[at.keyword(i as usize), " of ", name, " must be ", kind(KindId::Str), " or ", L::NAMES.nil, ", not ", ty(v)]),
        Fail::KeywordDefault => type_error(&[name, " takes ", at.keyword(1), " only with one positional argument"]),
        Fail::AttrName(v) => type_error(&["a field name must be ", kind(KindId::Str), ", not ", ty(v)]),
        Fail::TypeArg => type_error(&[name, " takes a type or ", kind(KindId::Tuple), " of types second"]),
        Fail::ClassArg => type_error(&[name, " takes a class first"]),
        Fail::ClassesArg => type_error(&[name, " takes a class or ", kind(KindId::Tuple), " of classes second"]),
        Fail::CodePoint => raised::<L>(Class::Value, &[name, " takes a code point from 0 to 0x10FFFF"]),
        Fail::CharKind(v) => type_error(&[name, " takes one character, not ", ty(v)]),
        Fail::CharLength(n) => type_error(&[name, " takes one character, not ", itoa::Buffer::new().format(n)]),
        Fail::ByteRange => raised::<L>(Class::Value, &[name, " takes a byte from 0 to 255"]),
        Fail::BytesRange => raised::<L>(Class::Value, &[name, " takes bytes from 0 to 255"]),
        Fail::NegativeCount => raised::<L>(Class::Value, &[name, " takes a count that is not negative"]),
        Fail::IndexSized(c) => raised::<L>(c, &[kind(KindId::Int), " too large for an index"]),
        Fail::CannotConvert(v) => type_error(&[name, " cannot convert ", ty(v)]),
        Fail::BufferArg(v) => type_error(&[name, " takes ", kind(KindId::Bytes), ", not ", ty(v)]),
        Fail::ByteArg(v) => type_error(&[name, " takes ", kind(KindId::Int), " or ", kind(KindId::Bytes), ", not ", ty(v)]),
        Fail::BufferFill(v) => type_error(&[name, " takes one byte as its fill, not ", shown::<L>(cx, v)]),
        Fail::TextFill => type_error(&[name, " takes one character as its fill"]),
        Fail::TabsWide => raised::<L>(Class::Overflow, &[name, " takes a tab size too large to hold"]),
        Fail::SliceIndex => type_error(&["the bounds of a slice must be ", kind(KindId::Int)]),
        Fail::AffixType { v, want } => type_error(&[name, " takes ", kind(want), ", not ", ty(v)]),
        Fail::AffixItem { v, want } => type_error(&["each affix of ", name, " must be ", kind(want), ", not ", ty(v)]),
        Fail::JoinIterable => type_error(&[name, " takes the items to join"]),
        Fail::JoinItem { at: i, v, want } => type_error(&["item ", itoa::Buffer::new().format(i), " of ", name, " is ", ty(v), ", not ", kind(want)]),
        Fail::NotRecord(v) => type_error(&[name, " takes ", kind(KindId::List), " or ", kind(KindId::Tuple), ", not ", ty(v)]),
        Fail::ExtendText => type_error(&[name, " takes ", kind(KindId::Bytes), " or ", kind(KindId::Int), " values, not ", kind(KindId::Str)]),
        Fail::ExtendType { recv, v } => type_error(&["cannot extend ", ty(recv), " with ", ty(v)]),
        Fail::ValueNotFound(recv) => raised::<L>(Class::Value, &[name, " found no such value in the ", ty(recv)]),
        Fail::Resize(n) => raised::<L>(Class::Value, &[name, " takes a size that is not negative, not ", itoa::Buffer::new().format(n)]),
        Fail::TakeRange { take, len } => raised::<L>(Class::Index, &["cannot take ", itoa::Buffer::new().format(take), " bytes of ", itoa::Buffer::new().format(len)]),
        Fail::HexArg(v) | Fail::TextOrBytes(v) => type_error(&[name, " takes ", kind(KindId::Str), " or ", kind(KindId::Bytes), ", not ", ty(v)]),
        Fail::LineKinds { chunk, rest } => match (cx.text(chunk).is_some(), cx.bytes_of(chunk).is_some()) {
            (true, _) => type_error(&[name, " takes ", kind(KindId::Str), " after ", kind(KindId::Str), ", not ", ty(rest)]),
            (_, true) => type_error(&[name, " takes ", kind(KindId::Bytes), " after ", kind(KindId::Bytes), ", not ", ty(rest)]),
            _ => type_error(&[name, " takes ", kind(KindId::Str), " or ", kind(KindId::Bytes), ", not ", ty(chunk)]),
        },
        Fail::NeedText(v) | Fail::MustBeText(v) => type_error(&[name, " takes ", kind(KindId::Str), ", not ", ty(v)]),
        Fail::TextArg { at: i, v } => type_error(&[name, " takes ", kind(KindId::Str), " as argument ", itoa::Buffer::new().format(i), ", not ", ty(v)]),
        Fail::SepText(v) => type_error(&[name, " takes ", kind(KindId::Str), " or ", L::NAMES.nil, " as its separator, not ", ty(v)]),
        Fail::SepChar(_) => type_error(&[name, " takes a separator of one character other than a quote or a line break"]),
        Fail::StripArg => type_error(&[name, " takes ", kind(KindId::Str), " or ", L::NAMES.nil]),
        Fail::FillText(v) => type_error(&[name, " takes one character as its fill, not ", ty(v)]),
        Fail::HexOdd => raised::<L>(Class::Value, &[name, " takes an even count of hex digits"]),
        Fail::HexDigit(i) => raised::<L>(Class::Value, &[name, " found no hex digit at position ", itoa::Buffer::new().format(i)]),
        Fail::HexFloat => raised::<L>(Class::Value, &[name, " found no hex float in the text"]),
        Fail::HexFloatRange => raise::<L>(Class::Overflow, "the hex float is too large"),
        Fail::DecodeNeedsBuffer(v) => type_error(&[name, " decodes ", kind(KindId::Bytes), ", not ", ty(v)]),
        Fail::DecodeHandler => type_error(&["the error handler cannot repair a failed decode"]),
        Fail::UnknownCodec(n) => raised::<L>(Class::Lookup, &["no codec is named ", &n]),
        Fail::UnknownHandler(n) => raised::<L>(Class::Lookup, &["no error handler is named ", &n]),
        Fail::Encode { codec, first, at: i, len, why } => {
            let span = if len == 1 { alloc::format!("U+{:04X} at character {i}", first as u32) } else { alloc::format!("characters {i} to {}", i + len - 1) };
            raised::<L>(Class::Encode, &[L::codec_name(codec), " cannot encode ", &span, ", ", why.words()])
        }
        Fail::Decode { codec, first, at: i, len, why } => {
            let span = if len == 1 { joined(&["byte 0x", &hex2(first), " at ", itoa::Buffer::new().format(i)]) } else { alloc::format!("bytes {i} to {}", i + len - 1) };
            raised::<L>(Class::Decode, &[L::codec_name(codec), " cannot decode ", &span, ", ", why.words()])
        }
        Fail::BadJson { at: i, why } => raised::<L>(Class::Value, &["invalid JSON at byte ", itoa::Buffer::new().format(i), ": ", why.words()]),
        Fail::NotJson { v, why } => type_error(&[ty(v), " ", why.words(), " so no JSON text holds it"]),
        Fail::BadCsv { at: i, why } => raised::<L>(Class::Value, &["invalid CSV at byte ", itoa::Buffer::new().format(i), ": ", why.words()]),
        Fail::FieldOf { recv, want } => type_error(&[name, " applies to ", want, " values, not ", ty(recv)]),
        Fail::IntMissing => type_error(&[name, " takes a value to convert"]),
        Fail::IntArg(v) => type_error(&[name, " takes ", kind(KindId::Str), ", ", kind(KindId::Bytes), " or a number, not ", ty(v)]),
        Fail::IntBaseType | Fail::StartType => type_error(&[name, " takes ", kind(KindId::Int), " as ", &slot(&at, 1)]),
        Fail::IntBase => raised::<L>(Class::Value, &[&slot(&at, 1), " of ", name, " must be 0 or from 2 to 36"]),
        Fail::IntExplicitBase => type_error(&[name, " takes ", &slot(&at, 1), " only with ", kind(KindId::Str), " or ", kind(KindId::Bytes)]),
        Fail::FloatArg(v) => type_error(&[name, " takes ", kind(KindId::Str), " or a number, not ", ty(v)]),
        Fail::FloatNan => raised::<L>(Class::Value, &["cannot convert NaN to ", kind(KindId::Int)]),
        Fail::FloatInf => raised::<L>(Class::Overflow, &["cannot convert infinity to ", kind(KindId::Int)]),
        Fail::RoundArg => type_error(&[name, " takes a number"]),
        Fail::RoundRange => raise::<L>(Class::Overflow, "the rounded value is too large"),
        Fail::PowArgs => type_error(&[name, " takes numbers"]),
        Fail::PowThird => type_error(&[name, " takes a modulus only when every operand is ", kind(KindId::Int)]),
        Fail::PowZero => raised::<L>(Class::Value, &[name, " takes a modulus other than 0"]),
        Fail::PowInverse => raise::<L>(Class::Value, "the base has no inverse for this modulus"),
        Fail::ExtremeEmpty => raised::<L>(Class::Value, &[name, " found no items"]),
        Fail::SumText(v) => type_error(&[name, " cannot add ", ty(v), " values"]),
        Fail::PairNotIterable => type_error(&["each entry must be a pair of a key and a value"]),
        Fail::PairLength { at: i, len } => raised::<L>(Class::Value, &["entry ", itoa::Buffer::new().format(i), " holds ", itoa::Buffer::new().format(len), " items where a pair holds 2"]),
        Fail::RangeArgs => type_error(&[name, " takes 1 to 3 arguments"]),
        Fail::ZeroStep => raised::<L>(Class::Value, &[name, " takes a step other than 0"]),
        Fail::IterCallable => type_error(&[name, " takes something to call first when given two arguments"]),
        Fail::SortCount(n) => type_error(&[name, " takes at most 2 arguments, not ", itoa::Buffer::new().format(n)]),
        Fail::SortPositional => type_error(&[name, " takes no positional arguments"]),
        Fail::SortKeyword(k) => {
            let hint = edge::util::suggest::did_you_mean(&k, at.keywords.iter().copied());
            type_error(&[name, " takes no keyword ", &k, &hint])
        }
        Fail::ByteOrder => raise::<L>(Class::Value, "the call names no byte order the runtime knows"),
        Fail::LengthNegative => raise::<L>(Class::Value, "the length must not be negative"),
        Fail::LengthType => type_error(&["the length must be ", kind(KindId::Int)]),
        Fail::TakeType => type_error(&["the count must be ", kind(KindId::Int), " or ", L::NAMES.nil]),
        Fail::BytesFirst => type_error(&[name, " takes ", kind(KindId::Bytes), " first"]),
        Fail::Spec(why) => spec::<L>(why),
        Fail::SpecInvalid { spec, v } => raised::<L>(Class::Value, &["invalid format spec ", &spec, " for ", ty(v)]),
        Fail::SpecUnknown { code, v } => raised::<L>(Class::Value, &["the format code ", &*(code).encode_utf8(&mut [0u8; 4]), " does not apply to ", ty(v)]),
        Fail::SpecRefused(v) => type_error(&[type_of(cx, v), " takes no format spec"]),
        Fail::SpecWidth => raise::<L>(Class::Value, "the format width passes the limit"),
        Fail::SpecPrecision => raise::<L>(Class::Value, "the format precision passes the limit"),
        Fail::EndOfInput => raise::<L>(Class::End, "the input ended"),
        Fail::ExitCode(v) => type_error(&[name, " takes ", kind(KindId::Int), ", not ", ty(v)]),
        Fail::SendText(i) => type_error(&[name, " takes ", kind(KindId::Str), " as argument ", itoa::Buffer::new().format(i)]),
        Fail::NoScheduler => raised::<L>(Class::Runtime, &[name, " needs an actor scheduler which this runtime lacks"]),
        f => bare_english::<L>(f),
    }
}

/* the error a failure raised with no running call gives in the runtime's words */
#[cold]
pub fn bare<L: Lang>(f: Fail) -> VmErr { L::words_bare(f) }

/* lib's neutral words for failures raised with no running call */
pub fn bare_english<L: Lang>(f: Fail) -> VmErr {
    match f {
        Fail::NegativeUnsigned => raise::<L>(Class::Overflow, "a negative number has no unsigned bytes"),
        Fail::TooBig => raise::<L>(Class::Overflow, "the number takes more bytes than it was given"),
        Fail::WideToFloat => raise::<L>(Class::Overflow, "the number is too large for floating point"),
        Fail::WideDivFloat => raise::<L>(Class::Overflow, "the quotient is too large for floating point"),
        Fail::NegativeShift => raise::<L>(Class::Value, "a shift count cannot be negative"),
        Fail::ShiftDigits => raise::<L>(Class::Overflow, "the shift makes the number too large"),
        Fail::ZeroNegativePower => raise::<L>(Class::ZeroDiv, "zero has no negative power"),
        Fail::TimeRange => raise::<L>(Class::Value, "the time falls outside the years 0000 to 9999"),
        Fail::DurationRange => raise::<L>(Class::Value, "the duration is longer than a duration holds"),
        Fail::HandlerArg => VmErr::Type("a handler must name a class or a group of classes"),
        Fail::Spec(why) => spec::<L>(why),
        Fail::Digits(count) => {
            let limit = L::DIGITS.unwrap_or(0);
            match count {
                Some(n) => raised::<L>(Class::Value, &["the number has ", itoa::Buffer::new().format(n), " digits, past the limit of ", itoa::Buffer::new().format(limit), " for its text"]),
                None => raised::<L>(Class::Value, &["the number has more digits than the limit of ", itoa::Buffer::new().format(limit), " for its text"]),
            }
        }
        f => VmErr::TypeMsg(alloc::format!("{f:?}")),
    }
}

/* the words of a refused format spec */
fn spec<L: Lang>(why: Spec) -> VmErr {
    let mut a = [0u8; 4];
    let mut b = [0u8; 4];
    match why {
        Spec::ZeroInText => raise::<L>(Class::Value, "the z flag does not apply to text"),
        Spec::ZeroInInt => raise::<L>(Class::Value, "the z flag does not apply to whole numbers"),
        Spec::AltWithChar => raise::<L>(Class::Value, "the # flag does not apply to the c type"),
        Spec::SignWithChar => raise::<L>(Class::Value, "a sign does not apply to the c type"),
        Spec::AltInText => raise::<L>(Class::Value, "the # flag does not apply to text"),
        Spec::SpaceInText | Spec::SignInText => raise::<L>(Class::Value, "a sign does not apply to text"),
        Spec::EqInText => raise::<L>(Class::Value, "the = align does not apply to text"),
        Spec::PrecisionInInt => raise::<L>(Class::Value, "a precision does not apply to whole numbers"),
        Spec::SepWith(s, t) => raised::<L>(Class::Value, &["the separator ", &*char::from(s).encode_utf8(&mut a), " does not apply to the ", &*char::from(t).encode_utf8(&mut b), " type"]),
        Spec::SepTwice(s) => raised::<L>(Class::Value, &["the separator ", &*char::from(s).encode_utf8(&mut a), " is given twice"]),
        Spec::SepBoth => raise::<L>(Class::Value, "a format spec takes one separator"),
        Spec::BadWidth => raise::<L>(Class::Value, "invalid width in format spec"),
        Spec::BadPrecision => raise::<L>(Class::Value, "invalid precision in format spec"),
        Spec::MissingPrecision => raise::<L>(Class::Value, "missing precision in format spec"),
        Spec::BadType => raise::<L>(Class::Value, "invalid type in format spec"),
        Spec::NeedsInt => raise::<L>(Class::Value, "the format type takes a whole number"),
        Spec::NeedsNumber => raise::<L>(Class::Value, "the format type takes a number"),
        Spec::CharNeedsInt => raise::<L>(Class::Value, "the c type takes a whole number"),
        Spec::CharRange | Spec::CharWide => raise::<L>(Class::Overflow, "the c type takes a code point from 0 to 0x10FFFF"),
        Spec::CharInvalid => raise::<L>(Class::Value, "the c type found no character at that code point"),
    }
}

/* the repr of `v` in the runtime's render */
fn repr<L: Lang>(cx: &mut Cx<'_, '_>, v: Val) -> Result<String, VmErr> { L::show(cx, v, Mode::Repr) }

/* the key error whose text is the repr of the missing key its args hold */
pub fn key<L: Lang>(cx: &mut Cx<'_, '_>, k: Val) -> VmErr {
    match repr::<L>(cx, k) { Ok(text) => cx.raised(L::class(Class::Key), vec![k], &text), Err(e) => e }
}

/* the key error whose key is the text `msg` */
pub fn text_key<L: Lang>(cx: &mut Cx<'_, '_>, msg: String) -> VmErr {
    match cx.new_str(msg) { Ok(k) => key::<L>(cx, k), Err(e) => e }
}

/* a byte as two lowercase hex digits */
fn hex2(b: u8) -> String {
    let digits = b"0123456789abcdef";
    let mut out = String::with_capacity(2);
    out.push(char::from(digits[(b >> 4) as usize]));
    out.push(char::from(digits[(b & 15) as usize]));
    out
}
