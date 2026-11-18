use interpreter::embed::Limits;
use interpreter::lang::{Native, Natives, Runtime};
use interpreter::lib::bytes::{Fixed, Mutable};
use interpreter::lib::lang::Centre;
use interpreter::lib::num::hash::Scheme;
use interpreter::lib::text::cut::Unicode;
use interpreter::lib::uni::Decimal;
use rig::Rig;
use crate::common::ran;

// a runtime of the rig's names keeping every choice lib leaves at its default
struct Plain;

impl interpreter::lib::fmt::render::Style for Plain {}

impl interpreter::lang::Language for Plain {
    const VERSION: u32 = 1;
    const POLICY: interpreter::lang::Policy = <Rig as interpreter::lang::Language>::POLICY;
    const NATIVES: &'static Natives = &PLAIN_NATIVES;
    const NAMES: &'static interpreter::lang::Names = &rig::NAMES;
    const FRONT: interpreter::lang::Front = rig::FRONT;
    const DESCRIPTOR: &'static interpreter::lang::Descriptor = &rig::DESCRIPTOR;
    const WIDE: Option<&'static interpreter::value::foreign::Kind> = Some(&rig::lang::WIDE);
    const BUILT: interpreter::value::foreign::Built = <rig::Rig as interpreter::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut interpreter::lang::Cx<'_, '_>, v: interpreter::value::Val, mode: interpreter::lang::Mode) -> Result<String, interpreter::value::VmErr> { interpreter::lib::fmt::render::hook::<Self>(cx, v, mode) }

    fn format(cx: &mut interpreter::lang::Cx<'_, '_>, v: interpreter::value::Val, spec: &str) -> Result<String, interpreter::value::VmErr> { interpreter::lib::fmt::spec::hook::<Self>(cx, v, spec) }

    fn handles(heap: &interpreter::value::HeapPool, exc: interpreter::value::Val, class: interpreter::value::Val) -> Result<bool, interpreter::value::VmErr> { interpreter::lib::types::match_exc::<Self>(heap, exc, class) }
}

impl interpreter::lib::Lang for Plain {
    const TREE: &'static interpreter::lib::lang::Tree = &rig::types::TREE;

    fn class(c: interpreter::lib::Class) -> &'static str { <Rig as interpreter::lib::Lang>::class(c) }
}

/* numbers modulo two to 40 less one with nothing reserved */
struct Forty;

impl Scheme for Forty {
    const BITS: u32 = 40;
    const NIL: i64 = 7;
    const INF: i64 = 11;
    const RESERVED: Option<(i64, i64)> = None;
    const RESERVED_TUPLE: i64 = 0;
    const SEED: u64 = 1;
    const LANE: (u64, u64) = (3, 5);
    const ROTATE: u32 = 7;
    const LENGTH: u64 = 0;
}

const PLAIN_FNS: &[Native] = &interpreter::lib::table![Plain;
    "print" => interpreter::lib::io::print,
    "repr" => interpreter::lib::fmt::render::repr_of,
    "type" => interpreter::lib::types::type_of,
    "len" => interpreter::lib::seq::len,
    "list" => interpreter::lib::seq::list,
    "int" => interpreter::lib::num::int::new[Decimal]("", "base"),
    "float" => interpreter::lib::num::float::new[Decimal],
    "bytes" => interpreter::lib::bytes::new[Fixed]("source", "encoding", "errors"),
    "bytearray" => interpreter::lib::bytes::new[Mutable]("source", "encoding", "errors"),
    "hash" => interpreter::lib::num::hash::hash[Forty],
    "iter" => interpreter::lib::iter::iter,
    "next" => interpreter::lib::iter::next,
];

const PLAIN_STR: &[Native] = &interpreter::lib::table![Plain;
    "center" => interpreter::lib::text::pad::center,
    "strip" => interpreter::lib::text::pad::trim,
    "split" => interpreter::lib::text::cut::split("sep", "maxsplit"),
    "isspace" => interpreter::lib::text::class::space,
    "splitlines" => interpreter::lib::text::cut::lines[Unicode]("keepends"),
    "expandtabs" => interpreter::lib::text::pad::tabs("tabsize"),
];

const PLAIN_BYTES: &[Native] = &interpreter::lib::table![Plain;
    "center" => interpreter::lib::bytes::methods::center[Fixed],
    "decode" => interpreter::lib::bytes::codec::decode[Fixed]("encoding", "errors"),
];

const PLAIN_ARRAY: &[Native] = &interpreter::lib::table![Plain; "take_bytes" => interpreter::lib::bytes::methods::take];

const PLAIN_INT: &[Native] = &interpreter::lib::table![Plain; "to_bytes" => interpreter::lib::num::int::to_bytes("length", "byteorder", "signed")];

const PLAIN_GEN: &[Native] = &interpreter::lib::table![Plain; "throw" => interpreter::lib::iter::throw];

static PLAIN_NATIVES: Natives = interpreter::natives! {
    language: Plain,
    fns: [PLAIN_FNS],
    methods: [Str => [PLAIN_STR], Bytes => [PLAIN_BYTES], ByteArray => [PLAIN_ARRAY], Int => [PLAIN_INT], Coro => [PLAIN_GEN]],
    types: rig::types::NAMES,
};

static PLAIN_RUNTIME: Runtime = Runtime::of::<Plain>();

/* the rig's spelling over the plain natives */
struct Plainly;

impl interpreter::lexer::Spelling for Plainly {
    const SYNTAX: interpreter::lexer::Syntax = rig::RIG;
    const NATIVES: &'static Natives = &PLAIN_NATIVES;
}

/* what `src` gives under the plain runtime */
fn with_plain(src: &str) -> String { ran::<Plainly>(&PLAIN_RUNTIME, src, Limits::sandbox()) }

/* what `src` gives under rig */
fn with_rig(src: &str) -> String { ran::<Rig>(&rig::RUNTIME, src, Limits::sandbox()) }

#[test]
fn an_odd_pad_splits_as_the_runtime_centres() {
    let src = "print(\"ab\".center(5, \"*\"), \"abc\".center(6, \"-\"), b\"ab\".center(5, b\"*\").decode())\n";
    assert_eq!(with_plain(src), "*ab** -abc-- *ab**\n");
    assert_eq!(with_rig(src), "**ab* -abc-- **ab*\n");
    let split = |c: Centre| [c.before(3, 5), c.before(3, 4), c.before(2, 5)];
    assert_eq!([split(Centre::After), split(Centre::Before), split(Centre::ByWidth)], [[1, 1, 1], [2, 2, 1], [2, 1, 1]]);
}

#[test]
fn number_text_takes_the_separators_and_prefixes_its_runtime_names() {
    let src = "print(int(\"010\", 0), int(\"0x1f\", 0), int(\" 7 \"), float(\"2.5\"))\n";
    assert_eq!(with_plain(src), "10 31 7 2.5\n");
    assert!(with_rig(src).ends_with("ValueError: \"010\" spells no int"), "{}", with_rig(src));
    for (src, want) in [("int(\"1_000\")", "1000\n"), ("int(\"0x1f\", 16)", "31\n"), ("int(\"0x_1f\", 0)", "31\n"), ("float(\"1_0.5\")", "10.5\n")] {
        let src = format!("print({src})\n");
        assert!(with_plain(&src).starts_with("ValueError: \""), "{src} {}", with_plain(&src));
        assert_eq!(with_rig(&src), want, "{src}");
    }
}

#[test]
fn the_information_separators_are_white_space_where_the_runtime_says() {
    let src = "print(len(\"\\u{1c}a\\u{1c}\".strip()), len(\"a\\u{1c}b\".split()), \"\\u{1c}\".isspace(), \" \\t\".isspace(), int(\"\\u{1f}7 \"))\n";
    assert_eq!(with_rig(src), "1 2 true true 7\n");
    assert!(with_plain(src).starts_with("ValueError: "), "{}", with_plain(src));
    assert_eq!(with_plain("print(len(\"\\u{1c}a\\u{1c}\".strip()), len(\"a\\u{1c}b\".split()), \"\\u{1c}\".isspace(), \" \\t\".isspace())\n"), "3 1 false true\n");
}

#[test]
fn sizes_past_a_machine_int_fail_only_where_the_runtime_holds_them_in_one() {
    let cases = [
        ("print(\"ab\".expandtabs(2**40))", "ab\n", "OverflowError: expandtabs takes a tab size too large to hold"),
        ("print(\"a\\tb\".expandtabs(2**40))", "MemoryError", "OverflowError: expandtabs takes a tab size too large to hold"),
        ("print(len(bytes(10**30)))", "MemoryError", "OverflowError: int too large for an index"),
        ("print(len(bytes(-10**30)))", "ValueError: bytes takes a count that is not negative", "OverflowError: int too large for an index"),
        ("print(bytearray(b\"abc\").take_bytes(10**30))", "IndexError: cannot take 9223372036854775807 bytes of 3", "IndexError: int too large for an index"),
    ];
    for (src, want_plain, want_rig) in cases {
        let src = format!("{src}\n");
        assert!(with_plain(&src).starts_with(want_plain), "{src} {}", with_plain(&src));
        assert!(with_rig(&src).starts_with(want_rig), "{src} {}", with_rig(&src));
    }
}

#[test]
fn a_spec_reads_the_parts_its_runtime_takes() {
    for (spec, want) in [("1234567:,", "1,234,567"), ("0.25:.1%", "25.0%"), ("-0.0:z.1f", "0.0"), ("5:n", "5"), ("-5:=4", "-  5")] {
        let src = format!("print(\"{{{spec}}}\")\n");
        assert!(with_plain(&src).starts_with("ValueError: "), "{spec} {}", with_plain(&src));
        assert_eq!(with_rig(&src), format!("{want}\n"), "{spec}");
    }
    let both = "print(\"{-5:05}\", \"{5:>3}\", \"{2.5:^7.2f}\", \"{255:#x}\")\n";
    assert_eq!(with_plain(both), "-0005   5  2.50   0xff\n");
    assert_eq!(with_rig(both), with_plain(both));
}

#[test]
fn throw_and_iter_take_a_second_argument_only_where_the_runtime_does() {
    let calls = "n = [0]\nfn f() {\n    n[0] += 1\n    return n[0]\n}\nprint(list(iter(f, 3)))\n";
    assert_eq!(with_rig(calls), "[1, 2]\n");
    assert!(with_plain(calls).starts_with("TypeError: "), "{}", with_plain(calls));
}

#[test]
fn byte_orders_are_the_words_the_runtime_gives_them() {
    assert_eq!(with_plain("print(list((258).to_bytes(2)))\n"), "[1, 2]\n");
    assert_eq!(with_plain("print((258).to_bytes(2, \"big\"))\n"), "ValueError: the call names no byte order the runtime knows");
    assert_eq!(with_rig("print(list((258).to_bytes(2, \"little\")), list((258).to_bytes(2, \"big\")))\n"), "[2, 1] [1, 2]\n");
}

#[test]
fn unicode_breaks_leave_out_the_separators_the_rig_breaks_at() {
    let src = "print(len(\"a\\u{1c}b\\u{1d}c\\nd\".splitlines()))\n";
    assert_eq!(with_plain(src), "2\n");
    assert_eq!(with_rig(src), "4\n");
}

#[test]
fn a_scheme_hashes_equal_numbers_of_any_width_alike() {
    let src = "print(hash(2**40), hash(2**45), hash(2.0**45), hash(2**100) == hash(2.0**100), hash(2**200), hash(2.0**200), hash(-1), hash(nil), hash(float(\"inf\")))\n";
    assert_eq!(with_plain(src), "1 32 32 true 1 1 -1 7 11\n");
}
