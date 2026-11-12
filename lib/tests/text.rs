use edge::embed::{Limits, VM};
use edge::lang::{Args, Cx, Mode, Native, Natives, Ret, Runtime, Sig};
use rig::Rig;
use rig::natives::{ARRAY, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

/* the render hook in each of its modes as a str */
fn hook(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mode = match a.pos[1].as_int() { 0 => Mode::Str, 1 => Mode::Repr, _ => Mode::Ascii };
    let text = lib::fmt::render::hook::<Rig>(cx, a.pos[0], mode)?;
    Ok(cx.new_str(text)?.into())
}

const HOOK: &[Native] = &[Native::new("hook", Sig::exact(2), hook)];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, TYPES, rig::natives::MATCH, rig::natives::DATE, HOOK],
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
    let (chunk, errors) = edge::code::compile_as::<rig::Rig>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

// each call's repr or its error in the words the corpus reads
const TRY: &str = "fn t(f) {\n    try {\n        print(repr(f()))\n    } catch Error as e {\n        print(text(type(e)) + \":\", e)\n    }\n}\n";

fn tried(calls: &str) -> String { run(&(String::from(TRY) + calls)) }

#[test]
fn methods_answer_as_the_engine_did() {
    let got = tried(concat!(
        "t(|| \"a,b,,c\".split(\",\"))\nt(|| \"a b  c\".split(maxsplit=1))\nt(|| \"  a b  c  \".rsplit(nil, 1))\n",
        "t(|| \"x\".split(\"\"))\nt(|| \"x\".split(1))\nt(|| \"x\".split(\",\", \"y\"))\nt(|| \"héllo\".find(\"l\", -2))\n",
        "t(|| \"abcabc\".count(\"bc\", 1, -1))\nt(|| \"abc\".index(\"z\"))\nt(|| \"abc\".startswith((\"x\", \"a\")))\n",
        "t(|| \"abc\".startswith((\"x\", 1)))\nt(|| \"abc\".endswith(5))\nt(|| \"-\".join((text(i) for i in range(3))))\n",
        "t(|| \",\".join([\"a\", 2]))\nt(|| \",\".join(5))\nt(|| \"ab\".center(7, \"*\"))\nt(|| \"ab\".center(5, \"xy\"))\n",
        "t(|| \"-7\".zfill(5))\nt(|| \"ab\".zfill(2 ** 70))\nt(|| \"a\\tb\".expandtabs(2 ** 40))\nt(|| \"x\".strip(nil))\n",
        "t(|| \"ǆungla ß\".title())\nt(|| \"Straße\".casefold())\nt(|| \"a\\r\\nb\\nc\".splitlines(true))\n",
        "t(|| \"k=v=w\".rpartition(\"=\"))\nt(|| \"añb\".encode(\"ascii\", \"replace\"))\nt(|| \"a\".encode(1))\nt(|| text.upper(5))\n",
        "t(|| \"aXbX\".replace(\"X\", \"yy\", 1))\nt(|| \"ab\".replace(\"\", \"-\"))\nt(|| \"ab\".replace(\"a\", \"b\", \"c\"))\n",
    ));
    let want = concat!(
        "[\"a\", \"b\", \"\", \"c\"]\n",
        "[\"a\", \"b  c\"]\n",
        "[\"  a b\", \"c\"]\n",
        "ValueError: split takes a separator that is not empty\n",
        "TypeError: split takes text or nil as its separator, not int\n",
        "TypeError: split takes int, not text\n",
        "3\n",
        "1\n",
        "ValueError: index found no match\n",
        "true\n",
        "TypeError: each affix of startswith must be text, not int\n",
        "TypeError: endswith takes text, not int\n",
        "\"0-1-2\"\n",
        "TypeError: item 1 of join is int, not text\n",
        "TypeError: join takes the items to join\n",
        "\"***ab**\"\n",
        "TypeError: center takes one character as its fill\n",
        "\"-0007\"\n",
        "OverflowError: int too large for an index\n",
        "OverflowError: expandtabs takes a tab size too large to hold\n",
        "\"x\"\n",
        "\"ǅungla Ss\"\n",
        "\"strasse\"\n",
        "[\"a\\r\\n\", \"b\\n\", \"c\"]\n",
        "(\"k=v\", \"=\", \"w\")\n",
        "b\"a?b\"\n",
        "TypeError: encode takes text as encoding, not int\n",
        "TypeError: upper of text takes no receiver of kind int\n",
        "\"ayybX\"\n",
        "\"-a-b-\"\n",
        "TypeError: replace takes int, not text\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_misfit_call_reads_as_the_engine_words_it() {
    let got = tried(concat!(
        "t(|| \"x\".upper(1))\nt(|| \"x\".upper(y=1))\nt(|| \"x\".split(sepp=\",\"))\nt(|| \"x\".split(\",\", sep=\",\"))\n",
        "t(|| \"x\".replace(\"a\"))\nt(|| \"x\".find())\nt(|| \"x\".join())\nt(|| \"x\".split(1, 2, 3))\nt(|| \"x\".center(1, 2, 3))\n",
    ));
    let want = concat!(
        "TypeError: text.upper() takes 0 arguments, not 1\n",
        "TypeError: text.upper() takes no keyword y\n",
        "TypeError: text.split() takes no keyword sepp\n",
        "TypeError: text.split() takes sep only once\n",
        "TypeError: text.replace() takes 2 to 3 arguments, not 1\n",
        "TypeError: text.find() takes 1 to 3 arguments, not 0\n",
        "TypeError: text.join() takes 1 argument, not 0\n",
        "TypeError: text.split() takes 0 to 2 arguments, not 3\n",
        "TypeError: text.center() takes 1 to 2 arguments, not 3\n",
    );
    assert_eq!(got, want);
}

#[test]
fn render_shows_every_kind_as_the_engine_did() {
    let got = run(concat!(
        "print(0.1 + 0.2, 1.0e+16, 1.0e-05, -0.0, float(\"inf\"), float(\"nan\"), 2 ** 100, 1.5e+300 * 10000000000.0)\n",
        "print([1, \"a\", (2,), record([(3, [4.5])]), set(), set([nil])], (1,), (), [[], ()])\n",
        "a = []\na.append(a)\nd = {}\nd[\"k\"] = d\nprint(a, d, [d, a])\n",
        "print(b\"it's\", bytearray(b\"\\x00\\n\"), range(1, 10, 2), range(0, 10), range(10**20))\n",
        "print(ValueError(\"x\"), [KeyError(\"k\")], IOError(2, \"nope\", \"f\"), Error(1, 2), Error(), Stop(5))\n",
        "print(\"\\0é\\u{2028}\", [\"\\0é\\u{2028}\", \"it's\", \"q\\\"\", \"\\\\\"], len, text.upper)\n",
        "print(hook(1.0, 0), hook(\"é\", 1), hook([\"é\"], 2), hook(ValueError(\"x\"), 1), hook(2 ** 64, 2))\n",
        "n = [1]\nfor _ in range(120) {\n    n = [n]\n}\nprint(text(n).count(\"[\"), text(n)[-110:-100])\n",
    ));
    let want = concat!(
        "0.30000000000000004 1e16 0.00001 -0.0 inf NaN 1267650600228229401496703205376 inf\n",
        "[1, \"a\", (2,), {3 = [4.5]}, set(), {nil}] (1,) () [[], ()]\n",
        "[[..]] {k = {..}} [{k = {..}}, [[..]]]\n",
        "b\"it's\" b\"\\x00\\n\" 1..10 by 2 0..10 0..100000000000000000000\n",
        "x [KeyError(\"k\")] (2, \"nope\", \"f\") (1, 2)  5\n",
        "\u{0}é\u{2028} [\"\\u{0}é\\u{2028}\", \"it's\", \"q\\\"\", \"\\\\\"] fn len fn text::upper\n",
        "1.0 \"é\" [\"\\u{e9}\"] ValueError(\"x\") 18446744073709551616\n",
        "101 [[[[[[[..]\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_whitespace_split_keeps_the_rest_whole_past_its_cuts() {
    let got = run(concat!(
        "print(\"  a b  c  \".split(nil, 1), \"  a b  c  \".rsplit(nil, 1))\n",
        "print(\"a b\\u{1c}c\".split(nil, 5), \"a b\\u{1c}c\".rsplit(nil, 5))\n",
        "print(\"   \".split(nil, 0), \" x \".rsplit(nil, 0))\n",
    ));
    assert_eq!(got, "[\"a\", \"b  c  \"] [\"  a b\", \"c\"]\n[\"a\", \"b\", \"c\"] [\"a\", \"b\", \"c\"]\n[] [\" x\"]\n");
}

#[test]
fn a_replace_names_no_text_when_nothing_matches() {
    let got = run("x = \"ab\"\nprint(\"abab\".replace(\"b\", \"xy\"), x.replace(\"\", \"-\"), x.replace(\"zz\", \"-\") == x, x.replace(\"z\", \"-\") == x)\n");
    assert_eq!(got, "axyaxy -a-b- true true\n");
}

#[test]
fn tabs_expand_to_the_next_stop() {
    let got = tried("t(|| \"a\\tbc\\td\\n\\te\".expandtabs(4))\nt(|| \"\\t\".expandtabs(0))\nt(|| \"\\t\\t\".expandtabs(2 ** 30))\n");
    assert_eq!(got, "\"a   bc  d\\n    e\"\n\"\"\nMemoryError: memory limit\n");
}
