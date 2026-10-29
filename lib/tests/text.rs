use edge::embed::{Limits, VM};
use edge::lang::{Args, Cx, Mode, Native, Natives, Ret, Runtime, Sig};
use mini::Mini;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

/* the render hook in each of its modes as a str */
fn hook(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mode = match a.pos[1].as_int() { 0 => Mode::Str, 1 => Mode::Repr, _ => Mode::Ascii };
    let text = lib::fmt::render::hook::<Mini>(cx, a.pos[0], mode)?;
    Ok(cx.new_str(text)?.into())
}

const HOOK: &[Native] = &[Native::new("hook", Sig::exact(2), hook)];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, HOOK],
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
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

// each call's repr or its error in the words the corpus reads
const TRY: &str = "fn t(f):\n    try:\n        print(repr(f()))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\n";

fn tried(calls: &str) -> String { run(&(String::from(TRY) + calls)) }

#[test]
fn methods_answer_as_the_engine_did() {
    let got = tried(concat!(
        "t(lambda: 'a,b,,c'.split(','))\nt(lambda: 'a b  c'.split(maxsplit=1))\nt(lambda: '  a b  c  '.rsplit(nil, 1))\n",
        "t(lambda: 'x'.split(''))\nt(lambda: 'x'.split(1))\nt(lambda: 'x'.split(',', 'y'))\nt(lambda: 'h\\u00e9llo'.find('l', -2))\n",
        "t(lambda: 'abcabc'.count('bc', 1, -1))\nt(lambda: 'abc'.index('z'))\nt(lambda: 'abc'.startswith(('x', 'a')))\n",
        "t(lambda: 'abc'.startswith(('x', 1)))\nt(lambda: 'abc'.endswith(5))\nt(lambda: '-'.join(str(i) for i in range(3)))\n",
        "t(lambda: ','.join(['a', 2]))\nt(lambda: ','.join(5))\nt(lambda: 'ab'.center(7, '*'))\nt(lambda: 'ab'.center(5, 'xy'))\n",
        "t(lambda: '-7'.zfill(5))\nt(lambda: 'ab'.zfill(2**70))\nt(lambda: 'a\\tb'.expandtabs(2**40))\nt(lambda: 'x'.strip(nil))\n",
        "t(lambda: '\\u01c6ungla \\u00df'.title())\nt(lambda: 'Stra\\u00dfe'.casefold())\nt(lambda: 'a\\r\\nb\\nc'.splitlines(true))\n",
        "t(lambda: 'k=v=w'.rpartition('='))\nt(lambda: 'a\\u00f1b'.encode('ascii', 'replace'))\nt(lambda: 'a'.encode(1))\nt(lambda: str.upper(5))\n",
        "t(lambda: 'aXbX'.replace('X', 'yy', 1))\nt(lambda: 'ab'.replace('', '-'))\nt(lambda: 'ab'.replace('a', 'b', 'c'))\n",
    ));
    let want = concat!(
        "['a', 'b', '', 'c']\n['a', 'b  c']\n['  a b', 'c']\nValueError: empty separator\nTypeError: must be str or None, not int\n",
        "TypeError: 'str' object cannot be interpreted as an integer\n3\n1\nValueError: substring not found\nTrue\n",
        "TypeError: tuple for startswith must only contain str, not int\nTypeError: endswith first arg must be str or a tuple of str, not int\n'0-1-2'\n",
        "TypeError: sequence item 1: expected str instance, int found\nTypeError: can only join an iterable\n'***ab**'\n",
        "TypeError: The fill character must be exactly one character long\n'-0007'\nOverflowError: Python int too large to convert to C ssize_t\n",
        "OverflowError: Python int too large to convert to C int\n'x'\n'ǅungla Ss'\n'strasse'\n",
        "['a\\r\\n', 'b\\n', 'c']\n('k=v', '=', 'w')\nb'a?b'\nTypeError: encode() argument 'encoding' must be str, not int\n",
        "TypeError: descriptor 'upper' for 'str' objects doesn't apply to a 'int' object\n'ayybX'\n'-a-b-'\nTypeError: 'str' object cannot be interpreted as an integer\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_misfit_call_reads_as_the_engine_words_it() {
    let got = tried(concat!(
        "t(lambda: 'x'.upper(1))\nt(lambda: 'x'.upper(y=1))\nt(lambda: 'x'.split(sepp=','))\nt(lambda: 'x'.split(',', sep=','))\n",
        "t(lambda: 'x'.replace('a'))\nt(lambda: 'x'.find())\nt(lambda: 'x'.join())\nt(lambda: 'x'.split(1, 2, 3))\nt(lambda: 'x'.center(1, 2, 3))\n",
    ));
    let want = concat!(
        "TypeError: str.upper() takes no arguments (1 given)\nTypeError: str.upper() takes no keyword arguments\n",
        "TypeError: split() got an unexpected keyword argument 'sepp'. Did you mean 'sep'?\n",
        "TypeError: argument for str.split() given by name ('sep') and position\n",
        "TypeError: replace() takes at least 2 positional arguments (1 given)\nTypeError: find expected at least 1 argument, got 0\n",
        "TypeError: str.join() takes exactly one argument (0 given)\nTypeError: split() takes at most 2 arguments (3 given)\n",
        "TypeError: center expected at most 2 arguments, got 3\n",
    );
    assert_eq!(got, want);
}

#[test]
fn render_shows_every_kind_as_the_engine_did() {
    let got = run(concat!(
        "print(0.1 + 0.2, 1e16, 1e-5, -0.0, float('inf'), float('nan'), 2**100, 1.5e300 * 1e10)\n",
        "print([1, 'a', (2,), {3: [4.5]}, set(), {nil}], (1,), (), [[], ()])\n",
        "a = []\na.append(a)\nd = {}\nd['k'] = d\nprint(a, d, [d, a])\n",
        "print(b\"it's\", bytearray(b'\\x00\\n'), range(1, 10, 2), range(0, 10), range(10**20), ...)\n",
        "print(ValueError('x'), [KeyError('k')], OSError(2, 'nope', 'f'), Exception(1, 2), Exception(), StopIteration(5))\n",
        "class E(ValueError):\n    pass\nprint(E('boom'), [E('in')], [E()], E(1, 2))\n",
        "print('\\x00\\u00e9\\u2028', ['\\x00\\u00e9\\u2028', \"it's\", 'q\"', '\\\\'], len, str.upper)\n",
        "print(hook(1.0, 0), hook('\\u00e9', 1), hook(['\\u00e9'], 2), hook(E('x'), 1), hook(2**64, 2))\n",
        "n = [1]\nfor _ in range(120):\n    n = [n]\nprint(str(n).count('['), str(n)[-110:-100])\n",
    ));
    let want = concat!(
        "0.30000000000000004 1e+16 1e-05 -0.0 inf nan 1267650600228229401496703205376 inf\n",
        "[1, 'a', (2,), {3: [4.5]}, set(), {None}] (1,) () [[], ()]\n",
        "[[...]] {'k': {...}} [{'k': {...}}, [[...]]]\n",
        "b\"it's\" bytearray(b'\\x00\\n') range(1, 10, 2) range(0, 10) range(0, 100000000000000000000) Ellipsis\n",
        "x [KeyError('k')] [Errno 2] nope: 'f' (1, 2)  5\n",
        "boom [E('in')] [E()] (1, 2)\n",
        "\u{0}é\u{2028} ['\\x00é\\u2028', \"it's\", 'q\"', '\\\\'] <built-in function len> <method 'upper' of 'str' objects>\n",
        "1.0 'é' ['\\xe9'] E('x') 18446744073709551616\n",
        "101 [[[[[[...]\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_whitespace_split_keeps_the_rest_whole_past_its_cuts() {
    let got = run(concat!(
        "print('  a b  c  '.split(nil, 1), '  a b  c  '.rsplit(nil, 1))\n",
        "print('a\\u2003b\\x1cc'.split(nil, 5), 'a\\u2003b\\x1cc'.rsplit(nil, 5))\n",
        "print('   '.split(nil, 0), ' x '.rsplit(nil, 0))\n",
    ));
    assert_eq!(got, "['a', 'b  c  '] ['  a b', 'c']\n['a', 'b', 'c'] ['a', 'b', 'c']\n[] [' x']\n");
}

#[test]
fn a_replace_names_no_text_when_nothing_matches() {
    let got = run("x = 'ab'\nprint('abab'.replace('b', 'xy'), x.replace('', '-'), x.replace('zz', '-') is x, x.replace('z', '-') is x)\n");
    assert_eq!(got, "axyaxy -a-b- True True\n");
}

#[test]
fn tabs_expand_to_the_next_stop() {
    let got = tried("t(lambda: 'a\\tbc\\td\\n\\te'.expandtabs(4))\nt(lambda: '\\t'.expandtabs(0))\nt(lambda: '\\t\\t'.expandtabs(2**30))\n");
    assert_eq!(got, "'a   bc  d\\n    e'\n''\nMemoryError: memory limit\n");
}
