use edge::embed::{Limits, VM};
use lib::Lang;
use mini::{Mini, RUNTIME};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// each call's repr or its error in the words the corpus reads
const TRY: &str = "fn t(f):\n    try:\n        print(repr(f()))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\n";

/* what `src` printed and then the words of the error it ended in */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Mini>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

fn tried(calls: &str) -> String { run(&(String::from(TRY) + calls)) }

#[test]
fn names_normalize_before_the_lookup() {
    for name in ["UTF 8", "utf--8", " utf-8", "U8", "latin 1", "ISO_8859-1:1987", "windows_1252", "utf-16LE", "csASCII", "unicodebigunmarked"] {
        assert!(<Mini as Lang>::codec(name).is_some(), "{name}");
    }
    for name in ["utf16le", "utf32be", "utf.8", "u-8", "cp-1252", "L_1"] {
        assert!(<Mini as Lang>::codec(name).is_none(), "{name}");
        assert_eq!(run(&format!("'a'.encode({name:?})")), "LookupError: unknown encoding: ".to_string() + name);
    }
}

#[test]
fn cp1252_reads_nul_and_refuses_only_its_holes() {
    let got = tried(concat!(
        "t(lambda: [ord(c) for c in b'a\\x00\\x80'.decode('cp1252')])\n",
        "t(lambda: b'\\x81'.decode('cp1252'))\n",
        "t(lambda: list('\\u0152\\u0153'.encode('cp1252')))\n",
    ));
    let want = concat!(
        "[97, 0, 8364]\n",
        "UnicodeDecodeError: 'charmap' codec can't decode byte 0x81 in position 0: character maps to <undefined>\n",
        "[140, 156]\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_handler_is_looked_up_only_once_a_span_fails() {
    let got = tried(concat!(
        "t(lambda: [ord(c) for c in b'ab'.decode('utf-16-le', 'bogus')])\n",
        "t(lambda: b'ab'.decode('cp1252', 'bogus'))\n",
        "t(lambda: b'\\x81'.decode('cp1252', 'bogus'))\n",
        "t(lambda: b'\\xff'.decode('utf-8', 'xmlcharrefreplace'))\n",
        "t(lambda: 'a\\xe9\\xfc'.encode('ascii', 'surrogateescape'))\n",
    ));
    let want = concat!(
        "[25185]\n",
        "'ab'\n",
        "LookupError: unknown error handler name 'bogus'\n",
        "TypeError: don't know how to handle UnicodeDecodeError in error callback\n",
        "UnicodeEncodeError: 'ascii' codec can't encode characters in position 1-2: ordinal not in range(128)\n",
    );
    assert_eq!(got, want);
}

// a runtime of mini's classes and codecs that keeps lib's own words for every failure
struct Neutral;

impl lib::fmt::render::Style for Neutral {}

impl edge::lang::Language for Neutral {
    const VERSION: u32 = 1;
    const POLICY: edge::lang::Policy = <Mini as edge::lang::Language>::POLICY;
    const NATIVES: &'static edge::lang::Natives = &NEUTRAL_NATIVES;
    const NAMES: &'static edge::lang::Names = &mini::NAMES;
    const FRONT: edge::lang::Front = mini::FRONT;
    const DESCRIPTOR: &'static edge::lang::Descriptor = &mini::DESCRIPTOR;
    const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&mini::lang::WIDE);
    const BUILT: edge::value::foreign::Built = <mini::Mini as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, mode: edge::lang::Mode) -> Result<String, edge::value::VmErr> { lib::fmt::render::hook::<Self>(cx, v, mode) }
}

impl Lang for Neutral {
    const TREE: &'static lib::lang::Tree = &mini::types::TREE;

    fn class(c: lib::Class) -> &'static str { <Mini as Lang>::class(c) }

    fn codec(name: &str) -> Option<lib::lang::Codec> { <Mini as Lang>::codec(name) }
}

const NEUTRAL_IO: &[edge::lang::Native] = &lib::table![Neutral; "print" => lib::io::print];
const NEUTRAL_STR: &[edge::lang::Native] = &lib::table![Neutral; "encode" => lib::bytes::codec::encode("encoding", "errors")];
const NEUTRAL_BYTES: &[edge::lang::Native] = &lib::table![Neutral; "decode" => lib::bytes::codec::decode[lib::bytes::Fixed]("encoding", "errors")];

static NEUTRAL_NATIVES: edge::lang::Natives = edge::natives! {
    language: Neutral,
    fns: [NEUTRAL_IO],
    methods: [Str => [NEUTRAL_STR], Bytes => [NEUTRAL_BYTES]],
    types: &[],
};

static NEUTRAL_RUNTIME: edge::lang::Runtime = edge::lang::Runtime::of::<Neutral>();

/* mini's spelling over the neutral natives */
struct Plainly;

impl edge::lexer::Spelling for Plainly {
    const SYNTAX: edge::lexer::Syntax = mini::MINI;
    const NATIVES: &'static edge::lang::Natives = &NEUTRAL_NATIVES;
}

/* the words of the error `src` ends in under the neutral runtime */
fn neutral(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Plainly>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&NEUTRAL_RUNTIME, &chunk, Limits::sandbox());
    vm.run().err().map(|e| e.to_string()).unwrap_or_default()
}

#[test]
fn a_codec_failure_carries_its_reason_for_the_runtime_to_word() {
    assert_eq!(neutral("'caf\\u00e9'.encode('ascii')"), "UnicodeEncodeError: us-ascii cannot encode U+00E9 at character 3, which the codec maps to nothing");
    assert_eq!(neutral("b'\\xff'.decode('utf-8')"), "UnicodeDecodeError: utf-8 cannot decode byte 0xff at 0, which starts no character");
    assert_eq!(neutral("b'\\xe2\\x82'.decode('utf-8')"), "UnicodeDecodeError: utf-8 cannot decode bytes 0 to 1, where the data ends inside a character");
    assert_eq!(neutral("b'\\x00\\xdc'.decode('utf-16-le')"), "UnicodeDecodeError: utf-16le cannot decode bytes 0 to 1, a low surrogate with no high surrogate before it");
    // mini words the same reasons as its corpus reads them
    let got = tried("t(lambda: 'caf\\u00e9'.encode('ascii'))\nt(lambda: b'\\xff'.decode('utf-8'))\nt(lambda: b'\\x00\\xdc'.decode('utf-16-le'))\n");
    let want = concat!(
        "UnicodeEncodeError: 'ascii' codec can't encode character '\\xe9' in position 3: ordinal not in range(128)\n",
        "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte\n",
        "UnicodeDecodeError: 'utf-16-le' codec can't decode bytes in position 0-1: illegal encoding\n",
    );
    assert_eq!(got, want);
}
