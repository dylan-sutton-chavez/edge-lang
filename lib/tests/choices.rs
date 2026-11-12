use edge::embed::{Limits, VM};
use edge::lang::{Native, Natives, Runtime};
use lib::bytes::{Fixed, Mutable};
use lib::lang::Centre;
use lib::num::hash::Scheme;
use lib::text::cut::Unicode;
use lib::uni::Decimal;
use rig::Rig;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// a runtime of the rig's names keeping every choice lib leaves at its default
struct Plain;

impl lib::fmt::render::Style for Plain {}

impl edge::lang::Language for Plain {
    const VERSION: u32 = 1;
    const POLICY: edge::lang::Policy = <Rig as edge::lang::Language>::POLICY;
    const NATIVES: &'static Natives = &PLAIN_NATIVES;
    const NAMES: &'static edge::lang::Names = &rig::NAMES;
    const FRONT: edge::lang::Front = rig::FRONT;
    const DESCRIPTOR: &'static edge::lang::Descriptor = &rig::DESCRIPTOR;
    const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&rig::lang::WIDE);
    const BUILT: edge::value::foreign::Built = <rig::Rig as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, mode: edge::lang::Mode) -> Result<String, edge::value::VmErr> { lib::fmt::render::hook::<Self>(cx, v, mode) }

    fn format(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, spec: &str) -> Result<String, edge::value::VmErr> { lib::fmt::spec::hook::<Self>(cx, v, spec) }

    fn handles(heap: &edge::value::HeapPool, exc: edge::value::Val, class: edge::value::Val) -> Result<bool, edge::value::VmErr> { lib::types::match_exc::<Self>(heap, exc, class) }
}

impl lib::Lang for Plain {
    const TREE: &'static lib::lang::Tree = &rig::types::TREE;

    fn class(c: lib::Class) -> &'static str { <Rig as lib::Lang>::class(c) }
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

const PLAIN_FNS: &[Native] = &lib::table![Plain;
    "print" => lib::io::print,
    "repr" => lib::fmt::render::repr_of,
    "type" => lib::types::type_of,
    "len" => lib::seq::len,
    "list" => lib::seq::list,
    "int" => lib::num::int::new[Decimal]("", "base"),
    "float" => lib::num::float::new[Decimal],
    "bytes" => lib::bytes::new[Fixed]("source", "encoding", "errors"),
    "bytearray" => lib::bytes::new[Mutable]("source", "encoding", "errors"),
    "hash" => lib::num::hash::hash[Forty],
    "iter" => lib::iter::iter,
    "next" => lib::iter::next,
];

const PLAIN_STR: &[Native] = &lib::table![Plain;
    "center" => lib::text::pad::center,
    "strip" => lib::text::pad::trim,
    "split" => lib::text::cut::split("sep", "maxsplit"),
    "isspace" => lib::text::class::space,
    "splitlines" => lib::text::cut::lines[Unicode]("keepends"),
    "expandtabs" => lib::text::pad::tabs("tabsize"),
];

const PLAIN_BYTES: &[Native] = &lib::table![Plain;
    "center" => lib::bytes::methods::center[Fixed],
    "decode" => lib::bytes::codec::decode[Fixed]("encoding", "errors"),
];

const PLAIN_ARRAY: &[Native] = &lib::table![Plain; "take_bytes" => lib::bytes::methods::take];

const PLAIN_INT: &[Native] = &lib::table![Plain; "to_bytes" => lib::num::int::to_bytes("length", "byteorder", "signed")];

const PLAIN_GEN: &[Native] = &lib::table![Plain; "throw" => lib::iter::throw];

static PLAIN_NATIVES: Natives = edge::natives! {
    language: Plain,
    fns: [PLAIN_FNS],
    methods: [Str => [PLAIN_STR], Bytes => [PLAIN_BYTES], ByteArray => [PLAIN_ARRAY], Int => [PLAIN_INT], Coro => [PLAIN_GEN]],
    types: rig::types::NAMES,
};

static PLAIN_RUNTIME: Runtime = Runtime::of::<Plain>();

/* the rig's spelling over the plain natives */
struct Plainly;

impl edge::lexer::Spelling for Plainly {
    const SYNTAX: edge::lexer::Syntax = rig::RIG;
    const NATIVES: &'static Natives = &PLAIN_NATIVES;
}

/* what `src` printed and the words of the error it ended in under `runtime` */
fn ran(runtime: &'static Runtime, chunk: &edge::code::SSAChunk) -> String {
    let mut vm = VM::with_runtime(runtime, chunk, Limits::sandbox());
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

/* what `src` gives under the plain runtime */
fn with_plain(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Plainly>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    ran(&PLAIN_RUNTIME, &chunk)
}

/* what `src` gives under rig */
fn with_mini(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Rig>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    ran(&rig::RUNTIME, &chunk)
}

#[test]
fn an_odd_pad_splits_as_the_runtime_centres() {
    let src = "print(\"ab\".center(5, \"*\"), \"abc\".center(6, \"-\"), b\"ab\".center(5, b\"*\").decode())\n";
    assert_eq!(with_plain(src), "*ab** -abc-- *ab**\n");
    assert_eq!(with_mini(src), "**ab* -abc-- **ab*\n");
    let split = |c: Centre| [c.before(3, 5), c.before(3, 4), c.before(2, 5)];
    assert_eq!([split(Centre::After), split(Centre::Before), split(Centre::ByWidth)], [[1, 1, 1], [2, 2, 1], [2, 1, 1]]);
}

#[test]
fn number_text_takes_the_separators_and_prefixes_its_runtime_names() {
    let src = "print(int(\"010\", 0), int(\"0x1f\", 0), int(\" 7 \"), float(\"2.5\"))\n";
    assert_eq!(with_plain(src), "10 31 7 2.5\n");
    assert!(with_mini(src).ends_with("ValueError: \"010\" spells no int"), "{}", with_mini(src));
    for (src, want) in [("int(\"1_000\")", "1000\n"), ("int(\"0x1f\", 16)", "31\n"), ("int(\"0x_1f\", 0)", "31\n"), ("float(\"1_0.5\")", "10.5\n")] {
        let src = format!("print({src})\n");
        assert!(with_plain(&src).starts_with("ValueError: \""), "{src} {}", with_plain(&src));
        assert_eq!(with_mini(&src), want, "{src}");
    }
}

#[test]
fn the_information_separators_are_white_space_where_the_runtime_says() {
    let src = "print(len(\"\\u{1c}a\\u{1c}\".strip()), len(\"a\\u{1c}b\".split()), \"\\u{1c}\".isspace(), \" \\t\".isspace(), int(\"\\u{1f}7 \"))\n";
    assert_eq!(with_mini(src), "1 2 true true 7\n");
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
    for (src, want_plain, want_mini) in cases {
        let src = format!("{src}\n");
        assert!(with_plain(&src).starts_with(want_plain), "{src} {}", with_plain(&src));
        assert!(with_mini(&src).starts_with(want_mini), "{src} {}", with_mini(&src));
    }
}

#[test]
fn a_spec_reads_the_parts_its_runtime_takes() {
    for (spec, want) in [("1234567:,", "1,234,567"), ("0.25:.1%", "25.0%"), ("-0.0:z.1f", "0.0"), ("5:n", "5"), ("-5:=4", "-  5")] {
        let src = format!("print(\"{{{spec}}}\")\n");
        assert!(with_plain(&src).starts_with("ValueError: "), "{spec} {}", with_plain(&src));
        assert_eq!(with_mini(&src), format!("{want}\n"), "{spec}");
    }
    let both = "print(\"{-5:05}\", \"{5:>3}\", \"{2.5:^7.2f}\", \"{255:#x}\")\n";
    assert_eq!(with_plain(both), "-0005   5  2.50   0xff\n");
    assert_eq!(with_mini(both), with_plain(both));
}

#[test]
fn throw_and_iter_take_a_second_argument_only_where_the_runtime_does() {
    let calls = "n = [0]\nfn f() {\n    n[0] += 1\n    return n[0]\n}\nprint(list(iter(f, 3)))\n";
    assert_eq!(with_mini(calls), "[1, 2]\n");
    assert!(with_plain(calls).starts_with("TypeError: "), "{}", with_plain(calls));
}

#[test]
fn byte_orders_are_the_words_the_runtime_gives_them() {
    assert_eq!(with_plain("print(list((258).to_bytes(2)))\n"), "[1, 2]\n");
    assert_eq!(with_plain("print((258).to_bytes(2, \"big\"))\n"), "ValueError: the call names no byte order the runtime knows");
    assert_eq!(with_mini("print(list((258).to_bytes(2, \"little\")), list((258).to_bytes(2, \"big\")))\n"), "[2, 1] [1, 2]\n");
}

#[test]
fn unicode_breaks_leave_out_the_separators_mini_breaks_at() {
    let src = "print(len(\"a\\u{1c}b\\u{1d}c\\nd\".splitlines()))\n";
    assert_eq!(with_plain(src), "2\n");
    assert_eq!(with_mini(src), "4\n");
}

#[test]
fn a_scheme_hashes_equal_numbers_of_any_width_alike() {
    let src = "print(hash(2**40), hash(2**45), hash(2.0**45), hash(2**100) == hash(2.0**100), hash(2**200), hash(2.0**200), hash(-1), hash(nil), hash(float(\"inf\")))\n";
    assert_eq!(with_plain(src), "1 32 32 true 1 1 -1 7 11\n");
}
