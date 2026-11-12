use edge::embed::{Limits, VM};
use edge::lang::{Native, Natives, Runtime};
use lib::data::lines::Lines;
use lib::data::{csv, json, lines};
use rig::Rig;
use rig::natives::{ARRAY, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// the data kernels under names of this test beside the rig's own tables
const DATA: &[Native] = &lib::table![Rig;
    "lines" => lines::lines,
    "json_read" => json::read,
    "json_write" => json::write,
    "csv_read" => csv::read("line", "sep"),
    "csv_write" => csv::write("values", "sep"),
];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, TYPES, rig::natives::MATCH, rig::natives::DATE, DATA],
    methods: [
        Str => [STR],
        Bytes => [BUFFER],
        ByteArray => [ARRAY],
        List => [LIST, LIST_SORT],
        Tuple => [TUPLE],
        Dict => [DICT],
        Set => [SET, SET_MANY],
        Int => [INT],
        Float => [FLOAT],
        Range => [RANGE],
    ],
    types: rig::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Over>();

// the rig's language over these natives
struct Over;
rig::over!(Over, &NATIVES);

/* what `src` printed and its error after it when one ended the run */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Rig>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

// each call's repr or its error as the class then the words
const TRY: &str = "fn t(f) {\n    try {\n        print(repr(f()))\n    } catch Error as e {\n        print(text(type(e)) + \":\", e)\n    }\n}\n";

fn tried(calls: &str) -> String { run(&(String::from(TRY) + calls)) }

fn lines_of(parts: &[&[u8]]) -> Vec<Vec<u8>> { parts.iter().map(|p| p.to_vec()).collect() }

#[test]
fn a_carry_cuts_chunks_into_lines_with_a_crlf_split_anywhere() {
    let mut l = Lines::new();
    assert!(l.feed(b"one\r").is_empty());
    assert_eq!(l.feed(b"\ntwo\n\nthr"), lines_of(&[b"one", b"two", b""]));
    assert_eq!(l.feed(b"ee\r\nfour\rfive\n"), lines_of(&[b"three", b"four\rfive"]));
    assert!(l.feed(b"").is_empty() && l.feed(b"tail\r").is_empty());
    assert_eq!(l.finish(), Some(b"tail\r".to_vec()));
    assert_eq!(Lines::default().finish(), None);
    let src = b"a\r\nbb\n\r\nccc\r\n\r\nd";
    let want = lines_of(&[b"a", b"bb", b"", b"ccc", b""]);
    for i in 0..=src.len() {
        for j in i..=src.len() {
            let mut l = Lines::new();
            let mut got = l.feed(&src[..i]);
            got.extend(l.feed(&src[i..j]));
            got.extend(l.feed(&src[j..]));
            assert_eq!(got, want, "cut at {i} and {j}");
            assert_eq!(l.finish(), Some(b"d".to_vec()));
        }
    }
    let mut l = Lines::new();
    let got: Vec<Vec<u8>> = src.iter().flat_map(|b| l.feed(&[*b])).collect();
    assert_eq!(got, want);
}

#[test]
fn gh_archive_chunks_read_as_events() {
    let src = concat!(
        "rest = b\"\"\n",
        "chunks = [b\"{\\\"id\\\":\\\"1\\\",\\\"type\\\":\\\"WatchEvent\\\",\\\"actor\\\":{\\\"login\\\":\\\"a[bot]\\\"},\\\"repo\\\":{\\\"name\\\":\\\"o/r\\\"},\\\"created_at\\\":\\\"2026-10-01T15:04:05Z\\\"}\\r\",\n",
        "    b\"\\n{\\\"id\\\":\\\"2\\\",\\\"type\\\":\\\"PushEv\", b\"ent\\\",\\\"actor\\\":{\\\"login\\\":\\\"b\\\"},\\\"repo\\\":{\\\"name\\\":\\\"p/q\\\"},\\\"created_at\\\":\\\"2026-10-01T16:00:00Z\\\"}\\r\\n\"]\n",
        "for chunk in chunks {\n",
        "    got = lines(chunk, rest)\n",
        "    rest = got[1]\n",
        "    for line in got[0] {\n",
        "        ev = json_read(line)\n",
        "        print(ev[\"id\"], ev[\"type\"], ev[\"actor\"][\"login\"], ev[\"repo\"][\"name\"], ev[\"created_at\"])\n",
        "    }\n}\n",
        "print(repr(rest), list(json_read(\"{{\\\"id\\\":\\\"1\\\",\\\"type\\\":\\\"WatchEvent\\\",\\\"actor\\\":{{\\\"login\\\":\\\"a[bot]\\\"}},\\\"repo\\\":{{\\\"name\\\":\\\"o/r\\\"}},\\\"created_at\\\":\\\"2026-10-01T15:04:05Z\\\"}}\")))\n",
    );
    let want = "1 WatchEvent a[bot] o/r 2026-10-01T15:04:05Z\n2 PushEvent b p/q 2026-10-01T16:00:00Z\nb\"\" [\"id\", \"type\", \"actor\", \"repo\", \"created_at\"]\n";
    assert_eq!(run(src), want);
}

#[test]
fn lines_keep_the_kind_of_their_chunk() {
    let got = tried(concat!(
        "t(|| lines(\"a\\nb\\r\\nc\", \"\"))\nt(|| lines(\"x\\r\", \"w\"))\nt(|| lines(\"\\n\", \"wx\\r\"))\nt(|| lines(b\"\\n\\n\"))\n",
        "t(|| lines(bytearray(b\"a\\nb\"), b\"z\"))\nt(|| lines(b\"a\", bytearray(b\"q\")))\nt(|| lines(\"hé\\nllo\"))\n",
        "t(|| lines(5))\nt(|| lines(b\"a\", \"b\"))\nt(|| lines(\"a\", b\"b\"))\nt(|| lines(\"a\", nil))\n",
    ));
    let want = concat!(
        "([\"a\", \"b\"], \"c\")\n",
        "([], \"wx\\r\")\n",
        "([\"wx\"], \"\")\n",
        "([b\"\", b\"\"], b\"\")\n",
        "([b\"za\"], b\"b\")\n",
        "([], b\"qa\")\n",
        "([\"hé\"], \"llo\")\n",
        "TypeError: lines takes text or bytes, not int\n",
        "TypeError: lines takes bytes after bytes, not text\n",
        "TypeError: lines takes text after text, not bytes\n",
        "TypeError: lines takes text after text, not nil\n",
    );
    assert_eq!(got, want);
    let src = concat!(
        "src = \"a\\r\\nbb\\n\\r\\nccc\\r\\n\"\nwant = lines(src)[0]\nok = true\n",
        "for i in range(len(src) + 1) {\n    first = lines(src[:i])\n    second = lines(src[i:], first[1])\n    if first[0] + second[0] != want or second[1] != \"\" {\n        ok = false\n    }\n}\nprint(want, ok)\n",
    );
    assert_eq!(run(src), "[\"a\", \"bb\", \"\", \"ccc\"] true\n");
}

#[test]
fn json_reads_every_kind_of_value() {
    let got = tried(concat!(
        "t(|| json_read(\" {{\\\"a\\\" : [1, -2, 3.5, -0.25e2, 1E3, 0, -0, true, false, null], \\\"b\\\": {{}}, \\\"c\\\": [ ], \\\"d\\\": \\\"x\\\"}} \"))\n",
        "t(|| json_read(\"{{\\\"k\\\": 1, \\\"j\\\": 2, \\\"k\\\": 3}}\"))\n",
        "t(|| json_read(b\"[12345678901234567890, -170141183460469231731687303715884105728, 1234567890123456789012345678901234567890123]\"))\n",
        "t(|| json_read(\"[99999999999999999999999999999999999999]\")[0] + 1)\nt(|| json_read(\"[1e400, -1e400, 5e-324, 0.1, -0.0]\"))\n",
        "t(|| len(text(json_read(\"2\" * 10000))))\nt(|| len(json_read(\"[\" * 512 + \"]\" * 512)))\n",
    ));
    let want = concat!(
        "{a = [1, -2, 3.5, -25.0, 1000.0, 0, 0, true, false, nil], b = {}, c = [], d = \"x\"}\n",
        "{k = 3, j = 2}\n",
        "[12345678901234567890, -170141183460469231731687303715884105728, 1234567890123456789012345678901234567890123]\n",
        "100000000000000000000000000000000000000\n",
        "[inf, -inf, 5e-324, 0.1, -0.0]\n",
        "10000\n",
        "1\n",
    );
    assert_eq!(got, want);
    let src = r#"s = json_read("\"q\\\"b\\\\s\\/f\\bf\\fn\\nr\\rt\\tu\\u00e9\\u4E2D\\ud83d\\ude00e\"")
print(s == "q\"b\\s/f\x08f\x0cn\nr\rt\tu\u{e9}\u{4e2d}\u{1f600}e", len(s), json_read("\"plain\""), json_read("\"\u{e9}\\u00e9\""))
"#;
    assert_eq!(run(src), "true 21 plain \u{e9}\u{e9}\n");
}

#[test]
fn json_failures_name_the_byte_and_the_reason() {
    let got = tried(concat!(
        "t(|| json_read(\"{{\\\"a\\\":1,}}\"))\nt(|| json_read(\"[1 2]\"))\nt(|| json_read(\"{{\\\"a\\\" 1}}\"))\nt(|| json_read(\"{{\\\"a\\\":1 \\\"b\\\"}}\"))\n",
        "t(|| json_read(\"\\\"\\\\ud800\\\"\"))\nt(|| json_read(\"\\\"\\\\udc00x\\\"\"))\nt(|| json_read(\"\\\"\\\\ud83d\\\\u0041\\\"\"))\n",
        "t(|| json_read(\"\\\"\\\\x\\\"\"))\nt(|| json_read(\"\\\"\\\\u12g4\\\"\"))\nt(|| json_read(\"01\"))\nt(|| json_read(\"-\"))\n",
        "t(|| json_read(\"1.\"))\nt(|| json_read(\"1e+\"))\nt(|| json_read(\".5\"))\nt(|| json_read(\"\"))\nt(|| json_read(\"   \"))\n",
        "t(|| json_read(\"[1] x\"))\nt(|| json_read(\"tru\"))\nt(|| json_read(\"\\\"a\\tb\\\"\"))\nt(|| json_read(\"\\\"abc\"))\n",
        "t(|| json_read(b\"\\\"\\xff\\\"\"))\nt(|| json_read(5))\nt(|| json_read(\"[\" * 513 + \"]\" * 513))\nt(|| json_read(\"1\" * 10001))\n",
        "t(|| json_read(\"{{1: 2}}\"))\nt(|| json_read(\"[1,]\"))\n",
    ));
    let want = concat!(
        "ValueError: invalid JSON at byte 7: expected a key\n",
        "ValueError: invalid JSON at byte 3: expected a comma or a closing bracket\n",
        "ValueError: invalid JSON at byte 5: expected a colon\n",
        "ValueError: invalid JSON at byte 7: expected a comma or a closing brace\n",
        "ValueError: invalid JSON at byte 1: lone surrogate\n",
        "ValueError: invalid JSON at byte 1: lone surrogate\n",
        "ValueError: invalid JSON at byte 1: lone surrogate\n",
        "ValueError: invalid JSON at byte 1: invalid escape\n",
        "ValueError: invalid JSON at byte 1: invalid unicode escape\n",
        "ValueError: invalid JSON at byte 0: a number has a leading zero\n",
        "ValueError: invalid JSON at byte 1: invalid number\n",
        "ValueError: invalid JSON at byte 2: invalid number\n",
        "ValueError: invalid JSON at byte 3: invalid number\n",
        "ValueError: invalid JSON at byte 0: unexpected character\n",
        "ValueError: invalid JSON at byte 0: unexpected end\n",
        "ValueError: invalid JSON at byte 3: unexpected end\n",
        "ValueError: invalid JSON at byte 4: extra text after the value\n",
        "ValueError: invalid JSON at byte 0: invalid literal\n",
        "ValueError: invalid JSON at byte 2: control character in a string\n",
        "ValueError: invalid JSON at byte 4: unterminated string\n",
        "ValueError: invalid JSON at byte 1: invalid utf-8\n",
        "TypeError: json_read takes text or bytes, not int\n",
        "ValueError: invalid JSON at byte 512: nested past 512 levels\n",
        "ValueError: the number has 10001 digits, past the limit of 10000 for its text\n",
        "ValueError: invalid JSON at byte 1: expected a key\n",
        "ValueError: invalid JSON at byte 3: unexpected character\n",
    );
    assert_eq!(got, want);
}

#[test]
fn json_writes_compact_text_that_reads_back() {
    let src = concat!(
        "print(json_write({a = [1, 2.5, \"x\\\"y\\\\\\n\\t\\u{e9}\\u{1}\"], b = nil, c = true, d = (1, false), e = 10 ** 30, f = 1e16,\n",
        "    g = 1.0, h = -0.0, i = 1e-7, j = {}, k = [], l = 2 ** 60, m = -(10 ** 50)}))\n",
        "x = {a = [1, 2.5, \"x\\\"y\\\\\\n\\t\\u{e9}\\u{1}\\u{1f600}\"], b = nil, c = [true, {d = 1e16, e = 1e-7, f = 10 ** 40}], g = -0.0}\n",
        "print(json_read(json_write(x)) == x, json_write(\"\\u{2028}\"), json_write(0.1), json_write(123456789.0))\n",
        "s = [1]\nprint(json_write([s, s, {a = s}]))\n",
    );
    let want = concat!(
        "{\"a\":[1,2.5,\"x\\\"y\\\\\\n\\té\\u0001\"],\"b\":null,\"c\":true,\"d\":[1,false],\"e\":1000000000000000000000000000000,\"f\":1e16,\"g\":1.0,\"h\":-0.0,\"i\":1e-7,\"j\":{},\"k\":[],\"l\":1152921504606846976,\"m\":-100000000000000000000000000000000000000000000000000}\n",
        "true \" \" 0.1 123456789.0\n",
        "[[1],[1],{\"a\":[1]}]\n",
    );
    assert_eq!(run(src), want);
    let got = tried(concat!(
        "t(|| json_write(record([(1, 2)])))\nt(|| json_write(set([1, 2])))\nt(|| json_write([b\"x\"]))\nt(|| json_write(1e400))\n",
        "t(|| json_write(|| 1))\nxs = [1]\nxs.append(xs)\nt(|| json_write(xs))\nd = {}\nd[\"me\"] = [d]\nt(|| json_write(d))\n",
        "x = []\nfor i in range(511) {\n    x = [x]\n}\nt(|| len(json_write(x)))\nt(|| json_write([x]))\n",
    ));
    let want = concat!(
        "TypeError: int cannot be an object key so no JSON text holds it\n",
        "TypeError: set has no JSON form so no JSON text holds it\n",
        "TypeError: bytes has no JSON form so no JSON text holds it\n",
        "TypeError: float value is not finite so no JSON text holds it\n",
        "TypeError: fn has no JSON form so no JSON text holds it\n",
        "TypeError: list holds itself so no JSON text holds it\n",
        "TypeError: record holds itself so no JSON text holds it\n",
        "1024\n",
        "TypeError: list nests past 512 levels so no JSON text holds it\n",
    );
    assert_eq!(got, want);
}

#[test]
fn json_write_pays_for_shared_parts_each_time_they_show() {
    // a hundred levels each holding one child twice write two to the hundredth values
    let doubled = |depth: u32| format!("x = [1]\nfor i in range({depth}) {{\n    x = [x, x]\n}}\nprint(len(json_write(x)))\n");
    let small = || Limits { ops: 2_000_000, memory: 64 << 20 };
    let (chunk, _) = edge::code::compile_as::<Rig>(&doubled(100));
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, small());
    assert!(matches!(vm.run(), Err(edge::value::VmErr::Budget)), "the walk ends on the budget and not on the memory");
    assert!(vm.memory_peak() < 32 << 20, "{} bytes", vm.memory_peak());
    // a smaller one writes in full with every copy of the shared child
    let (chunk, _) = edge::code::compile_as::<Rig>(&doubled(12));
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, small());
    vm.run().expect("a write within the budget");
    assert_eq!(vm.output_text(), "24573\n");
}

#[test]
fn csv_quotes_fields_both_ways() {
    let got = tried(concat!(
        "t(|| csv_read(\"a,\\\"b,c\\\",\\\"d \\\"\\\"e\\\"\\\"\\\",,f\"))\nt(|| csv_read(\"x;\\\"y;z\\\";\", \";\"))\nt(|| csv_read(\"\\\"multi\\nline\\\",2\\r\\n\"))\n",
        "t(|| csv_read(\"a,b\\n\"))\nt(|| csv_read(\"\"))\nt(|| csv_read(\"\\\"\\\"\"))\nt(|| csv_read(\",\"))\nt(|| csv_read(\"a\\\"b,\\\"c\\\"d\"))\n",
        "t(|| csv_read(\"é\\t\\\"è\\\"\", \"\\t\"))\nt(|| csv_read(line=\"p,q\", sep=\",\"))\n",
        "t(|| csv_read(\"a,\\\"open\"))\nt(|| csv_read(\"a\", \";;\"))\nt(|| csv_read(\"a\", \"\\\"\"))\nt(|| csv_read(\"a\", 1))\nt(|| csv_read(5))\n",
        "t(|| csv_write([\"a\", \"b,c\", \"d \\\"e\\\"\", \"\", 1, 2.5, nil, true, \"x\\ny\", \"cr\\r\"]))\nt(|| csv_write((\"x;y\", \"plain\"), \";\"))\n",
        "t(|| csv_write([\"\"]))\nt(|| csv_write([]))\nt(|| csv_write([\"\", \"\"]))\nt(|| csv_write(\"abc\"))\nt(|| csv_write([\"a\"], \"\"))\n",
    ));
    let want = concat!(
        "[\"a\", \"b,c\", \"d \\\"e\\\"\", \"\", \"f\"]\n",
        "[\"x\", \"y;z\", \"\"]\n",
        "[\"multi\\nline\", \"2\"]\n",
        "[\"a\", \"b\"]\n",
        "[]\n",
        "[\"\"]\n",
        "[\"\", \"\"]\n",
        "[\"a\\\"b\", \"cd\"]\n",
        "[\"é\", \"è\"]\n",
        "[\"p\", \"q\"]\n",
        "ValueError: invalid CSV at byte 2: a quoted field never closes\n",
        "TypeError: csv_read takes a separator of one character other than a quote or a line break\n",
        "TypeError: csv_read takes a separator of one character other than a quote or a line break\n",
        "TypeError: csv_read takes text as argument 2, not int\n",
        "TypeError: csv_read takes text as argument 1, not int\n",
        "\"a,\\\"b,c\\\",\\\"d \\\"\\\"e\\\"\\\"\\\",,1,2.5,nil,true,\\\"x\\ny\\\",\\\"cr\\r\\\"\"\n",
        "\"\\\"x;y\\\";plain\"\n",
        "\"\\\"\\\"\"\n",
        "\"\"\n",
        "\",\"\n",
        "TypeError: csv_write takes list or tuple, not text\n",
        "TypeError: csv_write takes a separator of one character other than a quote or a line break\n",
    );
    assert_eq!(got, want);
    let src = concat!(
        "rows = [[\"a\", \"b,c\", \"d \\\"e\\\"\", \"\", \"x\\r\\ny\"], [\"\"], [], [\"\", \"\"], [\"\\\"\", \",\", \";\"], [\"\\u{e9} \\u{1f600}\", \" lead\", \"trail \"]]\n",
        "print(all(csv_read(csv_write(r)) == r for r in rows), all(csv_read(csv_write(r, \";\"), \";\") == r for r in rows))\n",
    );
    assert_eq!(run(src), "true true\n");
}

// a runtime that words the reasons of JSON and CSV failures its own way
struct Terse;

impl lib::fmt::render::Style for Terse {}

impl edge::lang::Language for Terse {
    const VERSION: u32 = 1;
    const POLICY: edge::lang::Policy = <Rig as edge::lang::Language>::POLICY;
    const NATIVES: &'static edge::lang::Natives = &TERSE_NATIVES;
    const NAMES: &'static edge::lang::Names = &rig::NAMES;
    const FRONT: edge::lang::Front = rig::FRONT;
    const DESCRIPTOR: &'static edge::lang::Descriptor = &rig::DESCRIPTOR;
    const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&rig::lang::WIDE);
    const BUILT: edge::value::foreign::Built = <rig::Rig as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, mode: edge::lang::Mode) -> Result<String, edge::value::VmErr> { lib::fmt::render::hook::<Self>(cx, v, mode) }
}

impl lib::Lang for Terse {
    const TREE: &'static lib::lang::Tree = &rig::types::TREE;

    fn class(c: lib::Class) -> &'static str { <Rig as lib::Lang>::class(c) }

    fn words(cx: &mut edge::lang::Cx<'_, '_>, at: lib::fail::At, f: lib::fail::Fail) -> edge::value::VmErr {
        use lib::fail::{Csv, Fail, Json, raise};
        match f {
            Fail::BadJson { why: Json::Key, .. } => raise::<Terse>(lib::Class::Value, "a name in quotes goes there"),
            Fail::BadJson { at, why } => raise::<Terse>(lib::Class::Value, &format!("{why} {at} bytes in")),
            Fail::BadCsv { why: Csv::Unclosed, .. } => raise::<Terse>(lib::Class::Value, "a quote stays open"),
            f => lib::fail::english::<Terse>(cx, at, f),
        }
    }
}

const TERSE: &[Native] = &lib::table![Terse;
    "json_read" => json::read,
    "csv_read" => csv::read("line", "sep"),
];

static TERSE_NATIVES: Natives = edge::natives! {
    language: Terse,
    fns: [TERSE],
    methods: [],
    types: &[],
};

static TERSE_RUNTIME: Runtime = Runtime::of::<Terse>();

/* the rig's spelling over the terse natives */
struct Tersely;

impl edge::lexer::Spelling for Tersely {
    const SYNTAX: edge::lexer::Syntax = rig::RIG;
    const NATIVES: &'static Natives = &TERSE_NATIVES;
}

/* the words of the error `src` ends in under the terse runtime */
fn terse(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Tersely>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&TERSE_RUNTIME, &chunk, Limits::sandbox());
    vm.run().err().map(|e| e.to_string()).unwrap_or_default()
}

#[test]
fn a_json_or_csv_failure_carries_its_reason_for_the_runtime_to_word() {
    assert_eq!(terse("json_read(\"{{\\\"a\\\":1,}}\")"), "ValueError: a name in quotes goes there");
    assert_eq!(terse("json_read(\"[1 2]\")"), "ValueError: expected a comma or a closing bracket 3 bytes in");
    assert_eq!(terse("csv_read(\"a,\\\"b\")"), "ValueError: a quote stays open");
    assert_eq!(lib::fail::Json::LeadingZero.to_string(), "a number has a leading zero");
    assert_eq!(lib::fail::ToJson::Cycle.words(), "holds itself");
}
