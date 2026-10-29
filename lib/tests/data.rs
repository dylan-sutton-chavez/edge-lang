use edge::embed::{Limits, VM};
use edge::lang::{Native, Natives, Runtime};
use lib::data::lines::Lines;
use lib::data::{csv, json, lines};
use mini::Mini;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// the data kernels under names of this test beside mini's own tables
const DATA: &[Native] = &lib::table![Mini;
    "lines" => lines::lines,
    "json_read" => json::read,
    "json_write" => json::write,
    "csv_read" => csv::read("line", "sep"),
    "csv_write" => csv::write("values", "sep"),
];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, DATA],
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
        Coro => [GEN],
    ],
    types: mini::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Over>();

// mini's language over these natives
struct Over;
mini::over!(Over, &NATIVES);

/* what `src` printed and its error after it when one ended the run */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Mini>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

// each call's repr or its error as the class then the words
const TRY: &str = "fn t(f):\n    try:\n        print(repr(f()))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\n";

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
        "rest = b''\n",
        "chunks = [b'{\"id\":\"1\",\"type\":\"WatchEvent\",\"actor\":{\"login\":\"a[bot]\"},\"repo\":{\"name\":\"o/r\"},\"created_at\":\"2026-10-01T15:04:05Z\"}\\r',\n",
        "    b'\\n{\"id\":\"2\",\"type\":\"PushEv', b'ent\",\"actor\":{\"login\":\"b\"},\"repo\":{\"name\":\"p/q\"},\"created_at\":\"2026-10-01T16:00:00Z\"}\\r\\n']\n",
        "for chunk in chunks:\n",
        "    got = lines(chunk, rest)\n",
        "    rest = got[1]\n",
        "    for line in got[0]:\n",
        "        ev = json_read(line)\n",
        "        print(ev['id'], ev['type'], ev['actor']['login'], ev['repo']['name'], ev['created_at'])\n",
        "print(repr(rest), list(json_read('{\"id\":\"1\",\"type\":\"WatchEvent\",\"actor\":{\"login\":\"a[bot]\"},\"repo\":{\"name\":\"o/r\"},\"created_at\":\"2026-10-01T15:04:05Z\"}')))\n",
    );
    let want = "1 WatchEvent a[bot] o/r 2026-10-01T15:04:05Z\n2 PushEvent b p/q 2026-10-01T16:00:00Z\nb'' ['id', 'type', 'actor', 'repo', 'created_at']\n";
    assert_eq!(run(src), want);
}

#[test]
fn lines_keep_the_kind_of_their_chunk() {
    let got = tried(concat!(
        "t(lambda: lines('a\\nb\\r\\nc', ''))\nt(lambda: lines('x\\r', 'w'))\nt(lambda: lines('\\n', 'wx\\r'))\nt(lambda: lines(b'\\n\\n'))\n",
        "t(lambda: lines(bytearray(b'a\\nb'), b'z'))\nt(lambda: lines(b'a', bytearray(b'q')))\nt(lambda: lines('h\\u00e9\\nllo'))\n",
        "t(lambda: lines(5))\nt(lambda: lines(b'a', 'b'))\nt(lambda: lines('a', b'b'))\nt(lambda: lines('a', nil))\n",
    ));
    let want = concat!(
        "(['a', 'b'], 'c')\n([], 'wx\\r')\n(['wx'], '')\n([b'', b''], b'')\n",
        "([bytearray(b'za')], bytearray(b'b'))\n([], b'qa')\n(['h\u{e9}'], 'llo')\n",
        "TypeError: lines() argument 1 must be str or bytes-like, not int\n",
        "TypeError: lines() argument 2 must be bytes-like as argument 1 is, not str\n",
        "TypeError: lines() argument 2 must be str as argument 1 is, not bytes\n",
        "TypeError: lines() argument 2 must be str as argument 1 is, not NoneType\n",
    );
    assert_eq!(got, want);
    let src = concat!(
        "src = 'a\\r\\nbb\\n\\r\\nccc\\r\\n'\nwant = lines(src)[0]\nok = true\n",
        "for i in range(len(src) + 1):\n    first = lines(src[:i])\n    second = lines(src[i:], first[1])\n",
        "    if first[0] + second[0] != want || second[1] != '':\n        ok = false\nprint(want, ok)\n",
    );
    assert_eq!(run(src), "['a', 'bb', '', 'ccc'] True\n");
}

#[test]
fn json_reads_every_kind_of_value() {
    let got = tried(concat!(
        "t(lambda: json_read(' {\"a\" : [1, -2, 3.5, -0.25e2, 1E3, 0, -0, true, false, null], \"b\": {}, \"c\": [ ], \"d\": \"x\"} '))\n",
        "t(lambda: json_read('{\"k\": 1, \"j\": 2, \"k\": 3}'))\n",
        "t(lambda: json_read(b'[12345678901234567890, -170141183460469231731687303715884105728, 1234567890123456789012345678901234567890123]'))\n",
        "t(lambda: json_read('[99999999999999999999999999999999999999]')[0] + 1)\nt(lambda: json_read('[1e400, -1e400, 5e-324, 0.1, -0.0]'))\n",
        "t(lambda: len(str(json_read('2' * 4300))))\nt(lambda: len(json_read('[' * 512 + ']' * 512)))\n",
    ));
    let want = concat!(
        "{'a': [1, -2, 3.5, -25.0, 1000.0, 0, 0, True, False, None], 'b': {}, 'c': [], 'd': 'x'}\n{'k': 3, 'j': 2}\n",
        "[12345678901234567890, -170141183460469231731687303715884105728, 1234567890123456789012345678901234567890123]\n",
        "100000000000000000000000000000000000000\n[inf, -inf, 5e-324, 0.1, -0.0]\n4300\n1\n",
    );
    assert_eq!(got, want);
    let src = r#"s = json_read('"q\\"b\\\\s\\/f\\bf\\fn\\nr\\rt\\tu\\u00e9\\u4E2D\\ud83d\\ude00e"')
print(s == 'q"b\\s/f\bf\fn\nr\rt\tu\u00e9\u4e2d\U0001f600e', len(s), json_read('"plain"'), json_read('"\u00e9\\u00e9"'))
"#;
    assert_eq!(run(src), "True 21 plain \u{e9}\u{e9}\n");
}

#[test]
fn json_failures_name_the_byte_and_the_reason() {
    let got = tried(concat!(
        "t(lambda: json_read('{\"a\":1,}'))\nt(lambda: json_read('[1 2]'))\nt(lambda: json_read('{\"a\" 1}'))\nt(lambda: json_read('{\"a\":1 \"b\"}'))\n",
        "t(lambda: json_read('\"\\\\ud800\"'))\nt(lambda: json_read('\"\\\\udc00x\"'))\nt(lambda: json_read('\"\\\\ud83d\\\\u0041\"'))\n",
        "t(lambda: json_read('\"\\\\x\"'))\nt(lambda: json_read('\"\\\\u12g4\"'))\nt(lambda: json_read('01'))\nt(lambda: json_read('-'))\n",
        "t(lambda: json_read('1.'))\nt(lambda: json_read('1e+'))\nt(lambda: json_read('.5'))\nt(lambda: json_read(''))\nt(lambda: json_read('   '))\n",
        "t(lambda: json_read('[1] x'))\nt(lambda: json_read('tru'))\nt(lambda: json_read('\"a\\tb\"'))\nt(lambda: json_read('\"abc'))\n",
        "t(lambda: json_read(b'\"\\xff\"'))\nt(lambda: json_read(5))\nt(lambda: json_read('[' * 513 + ']' * 513))\nt(lambda: json_read('1' * 4301))\n",
        "t(lambda: json_read('{1: 2}'))\nt(lambda: json_read('[1,]'))\n",
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
        "TypeError: json_read() argument must be str or bytes-like, not int\n",
        "ValueError: invalid JSON at byte 512: nested past 512 levels\n",
        "ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits\n",
        "ValueError: invalid JSON at byte 1: expected a key\n",
        "ValueError: invalid JSON at byte 3: unexpected character\n",
    );
    assert_eq!(got, want);
}

#[test]
fn json_writes_compact_text_that_reads_back() {
    let src = concat!(
        "print(json_write({'a': [1, 2.5, 'x\"y\\\\\\n\\t\\u00e9\\x01'], 'b': nil, 'c': true, 'd': (1, false), 'e': 10 ** 30, 'f': 1e16,\n",
        "    'g': 1.0, 'h': -0.0, 'i': 1e-7, 'j': {}, 'k': [], 'l': 2 ** 60, 'm': -(10 ** 50)}))\n",
        "x = {'a': [1, 2.5, 'x\"y\\\\\\n\\t\\u00e9\\x01\\U0001f600'], 'b': nil, 'c': [true, {'d': 1e16, 'e': 1e-7, 'f': 10 ** 40}], 'g': -0.0}\n",
        "print(json_read(json_write(x)) == x, json_write('\\u2028'), json_write(0.1), json_write(123456789.0))\n",
        "s = [1]\nprint(json_write([s, s, {'a': s}]))\n",
    );
    let want = concat!(
        "{\"a\":[1,2.5,\"x\\\"y\\\\\\n\\t\u{e9}\\u0001\"],\"b\":null,\"c\":true,\"d\":[1,false],\"e\":1000000000000000000000000000000,\"f\":1e+16,",
        "\"g\":1.0,\"h\":-0.0,\"i\":1e-07,\"j\":{},\"k\":[],\"l\":1152921504606846976,\"m\":-100000000000000000000000000000000000000000000000000}\n",
        "True \"\u{2028}\" 0.1 123456789.0\n",
        "[[1],[1],{\"a\":[1]}]\n",
    );
    assert_eq!(run(src), want);
    let got = tried(concat!(
        "t(lambda: json_write({1: 2}))\nt(lambda: json_write({1, 2}))\nt(lambda: json_write([b'x']))\nt(lambda: json_write(1e400))\n",
        "t(lambda: json_write(lambda: 1))\nxs = [1]\nxs.append(xs)\nt(lambda: json_write(xs))\nd = {}\nd['me'] = [d]\nt(lambda: json_write(d))\n",
        "x = []\nfor i in range(511):\n    x = [x]\nt(lambda: len(json_write(x)))\nt(lambda: json_write([x]))\n",
    ));
    let want = concat!(
        "TypeError: json_write(): 'int' cannot be an object key\nTypeError: json_write(): 'set' has no JSON form\n",
        "TypeError: json_write(): 'bytes' has no JSON form\nTypeError: json_write(): 'float' value is not finite\n",
        "TypeError: json_write(): 'function' has no JSON form\nTypeError: json_write(): 'list' holds itself\n",
        "TypeError: json_write(): 'dict' holds itself\n1024\nTypeError: json_write(): 'list' nests past 512 levels\n",
    );
    assert_eq!(got, want);
}

#[test]
fn json_write_pays_for_shared_parts_each_time_they_show() {
    // a hundred levels each holding one child twice write two to the hundredth values
    let doubled = |depth: u32| format!("x = [1]\nfor i in range({depth}):\n    x = [x, x]\nprint(len(json_write(x)))\n");
    let small = || Limits { ops: 2_000_000, memory: 64 << 20 };
    let (chunk, _) = edge::code::compile_as::<Mini>(&doubled(100));
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, small());
    assert!(matches!(vm.run(), Err(edge::value::VmErr::Budget)), "the walk ends on the budget and not on the memory");
    assert!(vm.memory_peak() < 32 << 20, "{} bytes", vm.memory_peak());
    // a smaller one writes in full with every copy of the shared child
    let (chunk, _) = edge::code::compile_as::<Mini>(&doubled(12));
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, small());
    vm.run().expect("a write within the budget");
    assert_eq!(vm.output_text(), "24573\n");
}

#[test]
fn csv_quotes_fields_both_ways() {
    let got = tried(concat!(
        "t(lambda: csv_read('a,\"b,c\",\"d \"\"e\"\"\",,f'))\nt(lambda: csv_read('x;\"y;z\";', ';'))\nt(lambda: csv_read('\"multi\\nline\",2\\r\\n'))\n",
        "t(lambda: csv_read('a,b\\n'))\nt(lambda: csv_read(''))\nt(lambda: csv_read('\"\"'))\nt(lambda: csv_read(','))\nt(lambda: csv_read('a\"b,\"c\"d'))\n",
        "t(lambda: csv_read('\\u00e9\\t\"\\u00e8\"', '\\t'))\nt(lambda: csv_read(line='p,q', sep=','))\n",
        "t(lambda: csv_read('a,\"open'))\nt(lambda: csv_read('a', ';;'))\nt(lambda: csv_read('a', '\"'))\nt(lambda: csv_read('a', 1))\nt(lambda: csv_read(5))\n",
        "t(lambda: csv_write(['a', 'b,c', 'd \"e\"', '', 1, 2.5, nil, true, 'x\\ny', 'cr\\r']))\nt(lambda: csv_write(('x;y', 'plain'), ';'))\n",
        "t(lambda: csv_write(['']))\nt(lambda: csv_write([]))\nt(lambda: csv_write(['', '']))\nt(lambda: csv_write('abc'))\nt(lambda: csv_write(['a'], ''))\n",
    ));
    let want = concat!(
        "['a', 'b,c', 'd \"e\"', '', 'f']\n['x', 'y;z', '']\n['multi\\nline', '2']\n",
        "['a', 'b']\n[]\n['']\n['', '']\n['a\"b', 'cd']\n",
        "['\u{e9}', '\u{e8}']\n['p', 'q']\n",
        "ValueError: invalid CSV at byte 2: a quoted field never closes\n",
        "TypeError: csv_read() separator must be one character other than a quote or a line break\n",
        "TypeError: csv_read() separator must be one character other than a quote or a line break\n",
        "TypeError: csv_read() argument 2 must be str, not int\nTypeError: csv_read() argument 1 must be str, not int\n",
        "'a,\"b,c\",\"d \"\"e\"\"\",,1,2.5,None,True,\"x\\ny\",\"cr\\r\"'\n'\"x;y\";plain'\n",
        "'\"\"'\n''\n','\nTypeError: csv_write() takes a list or a tuple, not str\n",
        "TypeError: csv_write() separator must be one character other than a quote or a line break\n",
    );
    assert_eq!(got, want);
    let src = concat!(
        "rows = [['a', 'b,c', 'd \"e\"', '', 'x\\r\\ny'], [''], [], ['', ''], ['\"', ',', ';'], ['\\u00e9 \\U0001f600', ' lead', 'trail ']]\n",
        "print(all(csv_read(csv_write(r)) == r for r in rows), all(csv_read(csv_write(r, ';'), ';') == r for r in rows))\n",
    );
    assert_eq!(run(src), "True True\n");
}

// a runtime that words the reasons of JSON and CSV failures its own way
struct Terse;

impl lib::fmt::render::Style for Terse {}

impl edge::lang::Language for Terse {
    const VERSION: u32 = 1;
    const POLICY: edge::lang::Policy = <Mini as edge::lang::Language>::POLICY;
    const NATIVES: &'static edge::lang::Natives = &TERSE_NATIVES;
    const NAMES: &'static edge::lang::Names = &mini::NAMES;
    const FRONT: edge::lang::Front = mini::FRONT;
    const DESCRIPTOR: &'static edge::lang::Descriptor = &mini::DESCRIPTOR;
    const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&mini::lang::WIDE);
    const BUILT: edge::value::foreign::Built = <mini::Mini as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, mode: edge::lang::Mode) -> Result<String, edge::value::VmErr> { lib::fmt::render::hook::<Self>(cx, v, mode) }
}

impl lib::Lang for Terse {
    const TREE: &'static lib::lang::Tree = &mini::types::TREE;

    fn class(c: lib::Class) -> &'static str { <Mini as lib::Lang>::class(c) }

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

/* mini's spelling over the terse natives */
struct Tersely;

impl edge::lexer::Spelling for Tersely {
    const SYNTAX: edge::lexer::Syntax = mini::MINI;
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
    assert_eq!(terse("json_read('{\"a\":1,}')"), "ValueError: a name in quotes goes there");
    assert_eq!(terse("json_read('[1 2]')"), "ValueError: expected a comma or a closing bracket 3 bytes in");
    assert_eq!(terse("csv_read('a,\"b')"), "ValueError: a quote stays open");
    assert_eq!(lib::fail::Json::LeadingZero.to_string(), "a number has a leading zero");
    assert_eq!(lib::fail::ToJson::Cycle.words(), "holds itself");
}
