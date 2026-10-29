use alloc::string::String;
use alloc::vec;

use edge::lang::{Cx, KindId, Language, Misfit, Mode, Native, VAR};
use edge::s;
use edge::value::{Val, VmErr};
use lib::Lang;
use lib::fail::{At, Class, Coding, Fail, Spec, joined, key, raise, raised, shown, text_key, type_of};
use lib::lang::{Codec, Derived};

use crate::{Mini, NAMES};

// nil as mini's names spell it so texts holding only constants stay constant
const NIL: &str = NAMES.nil;

/* mini's words for every failure of lib's kernels as its corpus reads them */
pub fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr {
    let name = at.name;
    match f {
        Fail::TypeArg => VmErr::TypeMsg(joined(&[name, "() arg 2 must be a type, a tuple of types, or a union"])),
        Fail::ClassesArg => VmErr::TypeMsg(joined(&[name, "() arg 2 must be a class, a tuple of classes, or a union"])),
        Fail::NoLength(v) => VmErr::TypeMsg(joined(&["object of type '", cx.type_name(v), "' has no len()"])),
        Fail::SliceIndex => VmErr::Type("slice indices must be integers or have an __index__ method"),
        Fail::SpecRefused(v) => VmErr::TypeMsg(joined(&["unsupported format string passed to ", type_of(cx, v), ".__format__"])),
        Fail::TabsWide => raise::<Mini>(Class::Overflow, "Python int too large to convert to C int"),
        Fail::SumText(v) => VmErr::TypeMsg(match cx.kind(v) {
            KindId::Str => joined(&[name, "() can't sum strings [use ''.join(seq) instead]"]),
            k => joined(&[name, "() can't sum ", cx.names().kind(k), " [use b''.join(seq) instead]"]),
        }),
        Fail::MissingKey(k) => key::<Mini>(cx, k),
        Fail::EmptyMap => text_key::<Mini>(cx, joined(&[name, "(): dictionary is empty"])),
        Fail::EmptySet(recv) => { let t = joined(&[name, " from an empty ", cx.type_name(recv)]); text_key::<Mini>(cx, t) }
        Fail::IntLiteral { base, v } => match repr(cx, v) {
            Ok(r) => raised::<Mini>(Class::Value, &["invalid literal for ", name, "() with base ", itoa::Buffer::new().format(base), ": ", &r]),
            Err(e) => e,
        },
        Fail::FloatLiteral(v) => match repr(cx, v) {
            Ok(r) => raised::<Mini>(Class::Value, &["could not convert string to float: ", &r]),
            Err(e) => e,
        },
        Fail::BadTime(v) => match repr(cx, v) {
            Ok(r) => raised::<Mini>(Class::Value, &["invalid RFC 3339 time: ", &r]),
            Err(e) => e,
        },
        Fail::Exit(code) => {
            let text = joined(&[itoa::Buffer::new().format(code.as_int())]);
            cx.raised(Mini::class(Class::Exit), vec![code], &text)
        }
        f => plain(cx, at, f),
    }
}

/* the words of failures naming only types and the native */
fn plain(cx: &Cx<'_, '_>, at: At, f: Fail) -> VmErr {
    let name = at.name;
    let ty = |v: Val| cx.type_name(v);
    let kind = |k: KindId| cx.names().kind(k);
    match f {
        Fail::NotIterable(v) => VmErr::TypeMsg(joined(&["'", ty(v), "' object is not iterable"])),
        Fail::NotIterator(v) => VmErr::TypeMsg(joined(&["'", ty(v), "' object is not an iterator"])),
        Fail::NotReversible(v) => VmErr::TypeMsg(joined(&["'", ty(v), "' object is not reversible"])),
        Fail::NotIndex(v) => VmErr::TypeMsg(joined(&["'", ty(v), "' object cannot be interpreted as an integer"])),
        Fail::BadOperand(v) => VmErr::TypeMsg(joined(&["bad operand type for ", name, "(): '", ty(v), "'"])),
        Fail::BadOperands(a, b) => VmErr::TypeMsg(joined(&["unsupported operand type(s) for ", name, "(): '", ty(a), "' and '", ty(b), "'"])),
        Fail::NotOrdered(a, b) => VmErr::TypeMsg(joined(&["'>' not supported between instances of '", ty(a), "' and '", ty(b), "'"])),
        Fail::EmptyPop(recv) => raised::<Mini>(Class::Index, &[name, " from empty ", ty(recv)]),
        Fail::PopRange => raised::<Mini>(Class::Index, &[name, " index out of range"]),
        Fail::NotFound(recv) => { let k = ty(recv); raised::<Mini>(Class::Value, &[k, ".", name, "(x): x not in ", k]) }
        Fail::NotInSequence => raised::<Mini>(Class::Value, &["sequence.", name, "(x): x not in sequence"]),
        Fail::IndexType(recv) => VmErr::TypeMsg(joined(&[ty(recv), " indices must be integers"])),
        Fail::SubstringNotFound => VmErr::Value("substring not found"),
        Fail::SubsectionNotFound => VmErr::Value("subsection not found"),
        Fail::EmptySeparator => VmErr::Value("empty separator"),
        Fail::ParamType { at: i, v, want } => VmErr::TypeMsg(joined(&[name, "() argument '", at.param(i as usize), "' must be ", kind(want), ", not ", shown::<Mini>(cx, v)])),
        Fail::ParamLength(i) => raised::<Mini>(Class::Value, &[at.param(i as usize), " must be length 1."]),
        Fail::ParamAscii(i) => raised::<Mini>(Class::Value, &[at.param(i as usize), " must be ASCII."]),
        Fail::ParamWithoutText(i) => VmErr::TypeMsg(joined(&[at.param(i as usize), " without a string argument"])),
        Fail::TextWithoutCodec => VmErr::TypeMsg(joined(&["string argument without an ", at.param(1)])),
        Fail::KeywordText { at: i, v } => VmErr::TypeMsg(joined(&[at.keyword(i as usize), " must be ", NIL, " or a string, not ", ty(v)])),
        Fail::KeywordDefault => VmErr::TypeMsg(joined(&["Cannot specify a ", at.keyword(1), " for ", name, "() with multiple positional arguments"])),
        Fail::AttrName(v) => VmErr::TypeMsg(joined(&["attribute name must be string, not '", ty(v), "'"])),
        Fail::ClassArg => VmErr::TypeMsg(joined(&[name, "() arg 1 must be a class"])),
        Fail::CodePoint => raised::<Mini>(Class::Value, &[name, "() arg not in range(0x110000)"]),
        Fail::CharKind(v) => VmErr::TypeMsg(joined(&[name, "() expected string of length 1, but ", ty(v), " found"])),
        Fail::CharLength(n) => VmErr::TypeMsg(joined(&[name, "() expected a character, but string of length ", itoa::Buffer::new().format(n), " found"])),
        Fail::ByteRange => VmErr::Value("byte must be in range(0, 256)"),
        Fail::BytesRange => VmErr::Value("bytes must be in range(0, 256)"),
        Fail::NegativeCount => VmErr::Value("negative count"),
        Fail::IndexSized(c) => raised::<Mini>(c, &["cannot fit '", kind(KindId::Int), "' into an index-sized integer"]),
        Fail::CannotConvert(v) => VmErr::TypeMsg(joined(&["cannot convert '", ty(v), "' object to ", name])),
        Fail::BufferArg(v) => VmErr::TypeMsg(joined(&["a bytes-like object is required, not '", ty(v), "'"])),
        Fail::ByteArg(v) => VmErr::TypeMsg(joined(&["argument should be integer or bytes-like object, not '", ty(v), "'"])),
        Fail::BufferFill(v) => match cx.bytes_of(v) {
            Some(b) => VmErr::TypeMsg(joined(&[name, "(): argument 2 must be a byte string of length 1, not a ", ty(v), " object of length ", itoa::Buffer::new().format(b.len())])),
            None => VmErr::TypeMsg(joined(&[name, "() argument 2 must be a byte string of length 1, not ", shown::<Mini>(cx, v)])),
        },
        Fail::TextFill => VmErr::Type("The fill character must be exactly one character long"),
        Fail::AffixType { v, want } => VmErr::TypeMsg(joined(&[name, " first arg must be ", kind(want), " or a tuple of ", kind(want), ", not ", ty(v)])),
        Fail::AffixItem { v, want } => VmErr::TypeMsg(joined(&[kind(KindId::Tuple), " for ", name, " must only contain ", kind(want), ", not ", ty(v)])),
        Fail::JoinIterable => VmErr::Type("can only join an iterable"),
        Fail::JoinItem { at: i, v, want: KindId::Str } => VmErr::TypeMsg(joined(&["sequence item ", itoa::Buffer::new().format(i), ": expected ", kind(KindId::Str), " instance, ", ty(v), " found"])),
        Fail::JoinItem { at: i, v, .. } => VmErr::TypeMsg(joined(&["sequence item ", itoa::Buffer::new().format(i), ": expected a bytes-like object, ", ty(v), " found"])),
        Fail::NotRecord(v) => VmErr::TypeMsg(joined(&[name, "() takes a ", kind(KindId::List), " or a ", kind(KindId::Tuple), ", not ", ty(v)])),
        Fail::ExtendText => VmErr::TypeMsg(joined(&["expected iterable of integers; got: '", kind(KindId::Str), "'"])),
        Fail::ExtendType { recv, v } => VmErr::TypeMsg(joined(&["can't extend ", ty(recv), " with ", ty(v)])),
        Fail::ValueNotFound(recv) => raised::<Mini>(Class::Value, &["value not found in ", ty(recv)]),
        Fail::Resize(n) => raised::<Mini>(Class::Value, &["Can only resize to positive sizes, got ", itoa::Buffer::new().format(n)]),
        Fail::TakeRange { take, len } => raised::<Mini>(Class::Index, &["can't take ", itoa::Buffer::new().format(take), " bytes outside size ", itoa::Buffer::new().format(len)]),
        Fail::HexArg(v) => VmErr::TypeMsg(joined(&[name, "() argument must be str or bytes-like, not ", ty(v)])),
        Fail::TextOrBytes(v) => VmErr::TypeMsg(joined(&[name, "() argument must be ", kind(KindId::Str), " or bytes-like, not ", ty(v)])),
        Fail::LineKinds { chunk, rest } => VmErr::TypeMsg(match (cx.text(chunk).is_some(), cx.bytes_of(chunk).is_some()) {
            (true, _) => joined(&[name, "() argument 2 must be ", kind(KindId::Str), " as argument 1 is, not ", ty(rest)]),
            (_, true) => joined(&[name, "() argument 2 must be bytes-like as argument 1 is, not ", ty(rest)]),
            _ => joined(&[name, "() argument 1 must be ", kind(KindId::Str), " or bytes-like, not ", ty(chunk)]),
        }),
        Fail::NeedText(v) => VmErr::TypeMsg(joined(&[name, "() argument must be ", kind(KindId::Str), ", not ", ty(v)])),
        Fail::TextArg { at: i, v } => VmErr::TypeMsg(joined(&[name, "() argument ", itoa::Buffer::new().format(i), " must be ", kind(KindId::Str), ", not ", ty(v)])),
        Fail::MustBeText(v) => VmErr::TypeMsg(joined(&["must be ", kind(KindId::Str), ", not ", ty(v)])),
        Fail::SepText(v) => VmErr::TypeMsg(joined(&["must be ", kind(KindId::Str), " or ", NIL, ", not ", ty(v)])),
        Fail::SepChar(_) => VmErr::TypeMsg(joined(&[name, "() separator must be one character other than a quote or a line break"])),
        Fail::StripArg => VmErr::TypeMsg(joined(&[name, " arg must be ", NIL, " or ", kind(KindId::Str)])),
        Fail::FillText(v) => VmErr::TypeMsg(joined(&["The fill character must be a unicode character, not ", ty(v)])),
        Fail::HexOdd => raised::<Mini>(Class::Value, &[name, "() arg must contain an even number of hexadecimal digits"]),
        Fail::HexDigit(i) => raised::<Mini>(Class::Value, &["non-hexadecimal number found in ", name, "() arg at position ", itoa::Buffer::new().format(i)]),
        Fail::HexFloat => VmErr::Value("invalid hexadecimal floating-point string"),
        Fail::HexFloatRange => raise::<Mini>(Class::Overflow, "hexadecimal value too large to represent as a float"),
        Fail::DecodeNeedsBuffer(v) => VmErr::TypeMsg(joined(&["decoding to ", kind(KindId::Str), ": need a bytes-like object, ", ty(v), " found"])),
        Fail::DecodeHandler => VmErr::TypeMsg(joined(&["don't know how to handle ", Mini::class(Class::Decode), " in error callback"])),
        Fail::UnknownCodec(n) => raised::<Mini>(Class::Lookup, &["unknown encoding: ", &n]),
        Fail::UnknownHandler(n) => raised::<Mini>(Class::Lookup, &["unknown error handler name '", &n, "'"]),
        Fail::Encode { codec, first, at: i, len, why } => {
            let span = if len == 1 { joined(&["character ", &escaped(first), " in position ", itoa::Buffer::new().format(i)]) } else { joined(&["characters in position ", itoa::Buffer::new().format(i), "-", itoa::Buffer::new().format(i + len - 1)]) };
            raised::<Mini>(Class::Encode, &["'", Mini::codec_name(codec), "' codec can't encode ", &span, ": ", coding(codec, why)])
        }
        Fail::Decode { codec, first, at: i, len, why } => {
            let span = if len == 1 { joined(&["byte 0x", &hex2(first), " in position ", itoa::Buffer::new().format(i)]) } else { joined(&["bytes in position ", itoa::Buffer::new().format(i), "-", itoa::Buffer::new().format(i + len - 1)]) };
            raised::<Mini>(Class::Decode, &["'", Mini::codec_name(codec), "' codec can't decode ", &span, ": ", coding(codec, why)])
        }
        Fail::BadJson { at: i, why } => raised::<Mini>(Class::Value, &["invalid JSON at byte ", itoa::Buffer::new().format(i), ": ", why.words()]),
        Fail::NotJson { v, why } => VmErr::TypeMsg(joined(&[name, "(): '", ty(v), "' ", why.words()])),
        Fail::BadCsv { at: i, why } => raised::<Mini>(Class::Value, &["invalid CSV at byte ", itoa::Buffer::new().format(i), ": ", why.words()]),
        Fail::FieldOf { recv, want } => VmErr::TypeMsg(joined(&["'", name, "' applies to '", want, "' values, not '", ty(recv), "'"])),
        Fail::IntMissing => VmErr::TypeMsg(joined(&[name, "() is missing a positional argument"])),
        Fail::IntArg(v) => VmErr::TypeMsg(joined(&[name, "() argument must be a string, a bytes-like object or a real number, not '", ty(v), "'"])),
        Fail::IntBaseType | Fail::StartType => VmErr::TypeMsg(joined(&[name, "() ", at.param(1), " must be an integer"])),
        Fail::IntBase => raised::<Mini>(Class::Value, &[name, "() ", at.param(1), " must be >= 2 and <= 36, or 0"]),
        Fail::IntExplicitBase => VmErr::TypeMsg(joined(&[name, "() can't convert non-string with explicit ", at.param(1)])),
        Fail::FloatArg(v) => VmErr::TypeMsg(joined(&[name, "() argument must be a string or a real number, not '", ty(v), "'"])),
        Fail::FloatNan => VmErr::Value("cannot convert float NaN to integer"),
        Fail::FloatInf => raise::<Mini>(Class::Overflow, "cannot convert float infinity to integer"),
        Fail::RoundArg => VmErr::TypeMsg(joined(&[name, "() requires a number"])),
        Fail::RoundRange => raise::<Mini>(Class::Overflow, "rounded value too large to represent"),
        Fail::PowArgs => VmErr::TypeMsg(joined(&[name, "() requires numeric operands"])),
        Fail::PowThird => VmErr::TypeMsg(joined(&[name, "() 3rd argument not allowed unless all arguments are integers"])),
        Fail::PowZero => raised::<Mini>(Class::Value, &[name, "() 3rd argument cannot be 0"]),
        Fail::PowInverse => VmErr::Value("base is not invertible for the given modulus"),
        Fail::ExtremeEmpty => raised::<Mini>(Class::Value, &[name, "() iterable argument is empty"]),
        Fail::PairNotIterable => VmErr::Type("object is not iterable"),
        Fail::PairLength { at: i, len } => raised::<Mini>(Class::Value, &["dictionary update sequence element #", itoa::Buffer::new().format(i), " has length ", itoa::Buffer::new().format(len), "; 2 is required"]),
        Fail::RangeArgs => VmErr::TypeMsg(joined(&[name, "() takes 1 to 3 arguments"])),
        Fail::ZeroStep => raised::<Mini>(Class::Value, &[name, "() arg 3 must not be zero"]),
        Fail::IterCallable => VmErr::TypeMsg(joined(&[name, "(v, w): v must be callable"])),
        Fail::SortCount(n) => VmErr::TypeMsg(joined(&[name, "() takes at most 2 arguments (", itoa::Buffer::new().format(n), " given)"])),
        Fail::SortPositional => VmErr::TypeMsg(joined(&[name, "() takes no positional arguments"])),
        Fail::SortKeyword(k) => {
            let hint = edge::util::suggest::did_you_mean(&k, at.keywords.iter().copied());
            VmErr::TypeMsg(joined(&[name, "() got an unexpected keyword argument '", &k, "'", &hint]))
        }
        Fail::ByteOrder => VmErr::Value("byteorder must be 'little' or 'big'"),
        Fail::LengthNegative => VmErr::Value("length argument must be non-negative"),
        Fail::LengthType => VmErr::Type("length must be an integer"),
        Fail::TakeType => VmErr::TypeMsg(joined(&["n must be an integer or ", NIL])),
        Fail::BytesFirst => VmErr::TypeMsg(joined(&[name, "() first arg must be ", kind(KindId::Bytes)])),
        Fail::Spec(why) => spec(why),
        Fail::SpecInvalid { spec, v } => raised::<Mini>(Class::Value, &["Invalid format specifier '", &spec, "' for object of type '", ty(v), "'"]),
        Fail::SpecUnknown { code, v } => raised::<Mini>(Class::Value, &["Unknown format code '", &*(code).encode_utf8(&mut [0u8; 4]), "' for object of type '", ty(v), "'"]),
        Fail::SpecWidth => VmErr::Value("format width exceeds limit"),
        Fail::SpecPrecision => VmErr::Value("format precision exceeds limit"),
        Fail::EndOfInput => raise::<Mini>(Class::End, "EOF when reading a line"),
        Fail::ExitCode(v) => VmErr::TypeMsg(joined(&[name, "() expects an ", kind(KindId::Int), ", not ", ty(v)])),
        Fail::SendText(i) => VmErr::TypeMsg(joined(&[name, "() expects a ", kind(KindId::Str), " at argument ", itoa::Buffer::new().format(i)])),
        Fail::NoScheduler => raised::<Mini>(Class::Runtime, &[name, "() needs an actor scheduler, missing in this runtime"]),
        f => words_bare(f),
    }
}

/* mini's words for failures raised with no running call */
pub fn words_bare(f: Fail) -> VmErr {
    match f {
        Fail::Spec(Spec::CharWide) => raise::<Mini>(Class::Overflow, "Python int too large to convert to C long"),
        Fail::NegativeUnsigned => raise::<Mini>(Class::Overflow, "can't convert negative int to unsigned"),
        Fail::TooBig => raise::<Mini>(Class::Overflow, "int too big to convert"),
        Fail::WideToFloat => raise::<Mini>(Class::Overflow, "int too large to convert to float"),
        Fail::WideDivFloat => raise::<Mini>(Class::Overflow, "integer division result too large for a float"),
        Fail::NegativeShift => VmErr::Value("negative shift count"),
        Fail::ShiftDigits => raise::<Mini>(Class::Overflow, "too many digits in integer"),
        Fail::ZeroNegativePower => raise::<Mini>(Class::ZeroDiv, "zero to a negative power"),
        Fail::TimeRange => VmErr::Value("time out of range"),
        Fail::DurationRange => VmErr::Value("duration out of range"),
        Fail::HandlerArg => VmErr::Type("a handler must name a class or a tuple of classes"),
        Fail::Spec(why) => spec(why),
        Fail::Digits(count) => {
            let limit = Mini::DIGITS.unwrap_or(0);
            match count {
                Some(n) => raised::<Mini>(Class::Value, &["Exceeds the limit (", itoa::Buffer::new().format(limit), " digits) for integer string conversion: value has ", itoa::Buffer::new().format(n), " digits"]),
                None => raised::<Mini>(Class::Value, &["Exceeds the limit (", itoa::Buffer::new().format(limit), " digits) for integer string conversion"]),
            }
        }
        f => VmErr::TypeMsg(alloc::format!("{f:?}")),
    }
}

/* mini's words for why `codec` could not write or read a span */
fn coding(codec: Codec, why: Coding) -> &'static str {
    match why {
        Coding::Unmapped => match codec { Codec::Ascii => "ordinal not in range(128)", Codec::Latin1 => "ordinal not in range(256)", _ => "character maps to <undefined>" },
        Coding::End => "unexpected end of data",
        Coding::Start => "invalid start byte",
        Coding::Continuation => "invalid continuation byte",
        Coding::Truncated => "truncated data",
        Coding::Surrogate => "illegal UTF-16 surrogate",
        Coding::LowFirst => "illegal encoding",
        Coding::SurrogateRange => "code point in surrogate code point range(0xd800, 0xe000)",
        Coding::PastUnicode => "code point not in range(0x110000)",
    }
}

/* the words of a refused format spec */
fn spec(why: Spec) -> VmErr {
    let sep = |b: u8| char::from(b);
    match why {
        Spec::ZeroInText => VmErr::Value("Negative zero coercion (z) not allowed in string format specifier"),
        Spec::ZeroInInt => VmErr::Value("Negative zero coercion (z) not allowed in integer format specifier"),
        Spec::AltWithChar => VmErr::Value("Alternate form (#) not allowed with integer format specifier 'c'"),
        Spec::SignWithChar => VmErr::Value("Sign not allowed with integer format specifier 'c'"),
        Spec::AltInText => VmErr::Value("Alternate form (#) not allowed in string format specifier"),
        Spec::SpaceInText => VmErr::Value("Space not allowed in string format specifier"),
        Spec::SignInText => VmErr::Value("Sign not allowed in string format specifier"),
        Spec::EqInText => VmErr::Value("'=' alignment not allowed in string format specifier"),
        Spec::PrecisionInInt => VmErr::Value("Precision not allowed in integer format specifier"),
        Spec::SepWith(s, t) => raised::<Mini>(Class::Value, &["Cannot specify '", &*(sep(s)).encode_utf8(&mut [0u8; 4]), "' with '", &*(sep(t)).encode_utf8(&mut [0u8; 4]), "'."]),
        Spec::SepTwice(s) => raised::<Mini>(Class::Value, &["Cannot specify '", &*(sep(s)).encode_utf8(&mut [0u8; 4]), "' with '", &*(sep(s)).encode_utf8(&mut [0u8; 4]), "'."]),
        Spec::SepBoth => VmErr::Value("Cannot specify both ',' and '_'."),
        Spec::BadWidth => VmErr::Value("invalid width in format spec"),
        Spec::BadPrecision => VmErr::Value("invalid precision in format spec"),
        Spec::MissingPrecision => VmErr::Value("missing precision in format spec"),
        Spec::BadType => VmErr::Value("invalid type in format spec"),
        Spec::NeedsInt => VmErr::Value("format spec requires an integer"),
        Spec::NeedsNumber => VmErr::Value("format spec requires a number"),
        Spec::CharNeedsInt => VmErr::Value("'c' format spec requires an integer"),
        Spec::CharRange => raise::<Mini>(Class::Overflow, "%c arg not in range(0x110000)"),
        Spec::CharWide => raise::<Mini>(Class::Overflow, "int too large for a character"),
        Spec::CharInvalid => VmErr::Value("'c' format spec arg not a valid char"),
    }
}

/* the repr of `v` as mini renders it */
fn repr(cx: &mut Cx<'_, '_>, v: Val) -> Result<String, VmErr> { Mini::show(cx, v, Mode::Repr) }

/* a byte as two lowercase hex digits */
fn hex2(b: u8) -> String {
    let digits = b"0123456789abcdef";
    let mut out = String::with_capacity(2);
    out.push(char::from(digits[(b >> 4) as usize]));
    out.push(char::from(digits[(b & 15) as usize]));
    out
}

/* a character a codec failure quotes as its shortest hex escape */
fn escaped(c: char) -> String {
    let n = c as u32;
    if n < 0x100 { alloc::format!("'\\x{n:02x}'") } else if n < 0x10000 { alloc::format!("'\\u{n:04x}'") } else { alloc::format!("'\\U{n:08x}'") }
}

// methods whose count errors word the parameters past their positional ones
const TAKES: &[&str] = &["replace", "hex", "sort"];

/* the words of a call that misses a native's signature */
pub fn arity(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr {
    VmErr::TypeMsg(match owner {
        Some(ty) => method_words(n, ty, m),
        None => function_words(n, m),
    })
}

fn function_words(n: &Native, m: Misfit<'_>) -> String {
    let name = n.name;
    let (lo, hi) = (n.sig.min as i64, if n.sig.max == VAR { i64::MAX } else { n.sig.max as i64 });
    match m {
        Misfit::Count(got) => {
            let got = got as i64;
            if lo == hi && lo == 0 { return s!(str name, "() takes no arguments (", int got, " given)"); }
            if lo == hi && lo == 1 { return s!(str name, "() takes exactly one argument (", int got, " given)"); }
            if lo == hi { return s!(str name, " expected ", int lo, " arguments, got ", int got); }
            let (word, bound) = if got < lo { ("least", lo) } else { ("most", hi) };
            s!(str name, " expected at ", str word, " ", int bound, " argument", str if bound == 1 { "" } else { "s" }, ", got ", int got)
        }
        Misfit::Unexpected(_) if !n.sig.keywords() => s!(str name, "() takes no keyword arguments"),
        Misfit::Unexpected(k) if !n.sig.kw.is_empty() => s!(str name, "() got an unexpected keyword argument '", str k, "'"),
        Misfit::Unexpected(k) => s!("'", str k, "' is an invalid keyword argument for ", str name, "()"),
        Misfit::Twice(k) => s!(str name, "() got multiple values for argument '", str k, "'"),
        Misfit::Missing(_) => s!(str name, "() is missing a positional argument"),
    }
}

fn method_words(n: &Native, ty: &str, m: Misfit<'_>) -> String {
    let name = n.name;
    let named = n.sig.names.iter().any(|p| !p.is_empty()) || !n.sig.kw.is_empty();
    match m {
        Misfit::Count(got) => {
            let (min, max, got) = (n.sig.min as i64, n.sig.max as i64, got as i64);
            let args = |k: i64| if k == 1 { "argument" } else { "arguments" };
            if min == max && min <= 1 {
                return s!(str ty, ".", str name, str if min == 0 { "() takes no arguments (" } else { "() takes exactly one argument (" }, int got, " given)");
            }
            if TAKES.contains(&name) || named {
                return match n.sig.names.get(got as usize).filter(|p| !p.is_empty() && got < min) {
                    Some(p) => s!(str name, "() missing required argument '", str p, "' (pos ", int got + 1, ")"),
                    None if got < min => s!(str name, "() takes at least ", int min, " positional ", str args(min), " (", int got, " given)"),
                    None => s!(str name, "() takes at most ", int max, " ", str args(max), " (", int got, " given)"),
                };
            }
            let (word, k) = if min == max { ("", min) } else if got < min { ("at least ", min) } else { ("at most ", max) };
            s!(str name, " expected ", str word, int k, " ", str args(k), ", got ", int got)
        }
        Misfit::Unexpected(_) if !n.sig.keywords() => s!(str ty, ".", str name, "() takes no keyword arguments"),
        Misfit::Unexpected(k) => {
            let hint = edge::util::suggest::did_you_mean(k, n.sig.names.iter().chain(n.sig.kw).copied().filter(|p| !p.is_empty()));
            s!(str name, "() got an unexpected keyword argument '", str k, "'", str &hint)
        }
        Misfit::Twice(k) => s!("argument for ", str ty, ".", str name, "() given by name ('", str k, "') and position"),
        Misfit::Missing(i) => match n.sig.names.get(i).filter(|p| !p.is_empty()) {
            Some(p) => s!(str name, "() missing required argument '", str p, "' (pos ", int i as i64 + 1, ")"),
            None => s!(str name, "() is missing a positional argument"),
        },
    }
}
