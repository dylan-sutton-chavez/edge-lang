use std::cell::RefCell;
use std::rc::Rc;

use edge::embed::{Limits, VM};
use edge::value::{DictMap, HeapObj, HeapPool, Val};
use mini::RUNTIME;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// each call's type and repr or its error then the generators the calls take
const TRY: &str = concat!(
    "fn t(f):\n    try:\n        r = f()\n        print(type(r).__name__, repr(r))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\n",
    "fn g(n):\n    for i in range(n):\n        yield i\nfn pairs():\n    yield ('a', 1)\n    yield ['b', 2]\nfn kv(k):\n    yield k\n    yield len(k)\n",
    "fn boom(k):\n    yield k\n    raise KeyError('inner')\nasync fn co():\n    return 1\n",
);

/* what `src` printed and then the words of the error it ended in */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits { ops: 10_000_000, memory: 64 << 20 });
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

fn tried(calls: &str) -> String { run(&(String::from(TRY) + calls)) }

#[test]
fn text_and_numbers_read_as_the_engine_did() {
    let got = tried(concat!(
        "t(lambda: (str(), str(-12), str(2**70), str(0.1 + 0.2), str(true), str(nil), str([1, 'a']), str(b'x'), str(object=5)))\n",
        "t(lambda: (str(b'h\\xc3\\xa9', 'utf-8'), str(b'a\\xffb', 'utf-8', 'replace'), str(b'a\\xffb', errors='ignore'), str(encoding='utf-8')))\n",
        "t(lambda: (str(b'\\xffa', 'utf-8')))\n",
        "t(lambda: (str(5, 'utf-8')))\n",
        "t(lambda: (str(b'x', 5)))\n",
        "t(lambda: (str(b'x', 'utf-8', nil)))\n",
        "t(lambda: (str(1, 2, 3, 4)))\n",
        "t(lambda: (str(x=1)))\n",
        "t(lambda: (int(), int(-7.9), int(true), int(1e30), int(-2.0**127), int('  -12 '), int('1_000'), int('٣٤'), int(b' 42 '), int(2**70)))\n",
        "t(lambda: (int('ff', 16), int('0x_ff', 16), int('0b101', 0), int('z', 36), int(b'7f', 16), int('ff', base=16), int('0', false)))\n",
        "t(lambda: (int(float('nan'))))\n",
        "t(lambda: (int(float('-inf'))))\n",
        "t(lambda: (int('12a')))\n",
        "t(lambda: (int('010', 0)))\n",
        "t(lambda: (int(b'\\xff')))\n",
        "t(lambda: (int([1])))\n",
        "t(lambda: (int('1', true)))\n",
        "t(lambda: (int('7', 2**70)))\n",
        "t(lambda: (int(5, 1)))\n",
        "t(lambda: (int(5, 2)))\n",
        "t(lambda: (int('1', 1.5)))\n",
        "t(lambda: (int(x='5')))\n",
        "t(lambda: (int(base=16)))\n",
        "t(lambda: (int(1, 2, 3)))\n",
        "t(lambda: (float(), float(7), float(true), float(2**100), float(' 1.5\\n'), float('-Infinity'), float('1_000.5'), float('١.٥'), float(b'2.5'), float('2_5e1_0')))\n",
        "t(lambda: (float('1_.5')))\n",
        "t(lambda: (float('i_nf')))\n",
        "t(lambda: (float(b'\\xff')))\n",
        "t(lambda: (float(2**2000)))\n",
        "t(lambda: (float([1])))\n",
        "t(lambda: (float(x=1)))\n",
        "t(lambda: (bool(), bool(0.0), bool('a'), bool([]), bool(range(2, 1)), bool(len)))\n",
    ));
    let want = concat!(
        "tuple ('', '-12', '1180591620717411303424', '0.30000000000000004', 'True', 'None', \"[1, 'a']\", \"b'x'\", '5')\n",
        "tuple ('hé', 'a�b', 'ab', '')\n",
        "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 0: invalid start byte\n",
        "TypeError: decoding to str: need a bytes-like object, int found\n",
        "TypeError: str() argument 'encoding' must be str, not int\n",
        "TypeError: str() argument 'errors' must be str, not None\n",
        "TypeError: str expected at most 3 arguments, got 4\n",
        "TypeError: 'x' is an invalid keyword argument for str()\n",
        "tuple (0, -7, 1, 1000000000000000019884624838656, -170141183460469231731687303715884105728, -12, 1000, 34, 42, 1180591620717411303424)\n",
        "tuple (255, 255, 5, 35, 127, 255, 0)\n",
        "ValueError: cannot convert float NaN to integer\n",
        "OverflowError: cannot convert float infinity to integer\n",
        "ValueError: invalid literal for int() with base 10: '12a'\n",
        "ValueError: invalid literal for int() with base 0: '010'\n",
        "ValueError: invalid literal for int() with base 10: b'\\xff'\n",
        "TypeError: int() argument must be a string, a bytes-like object or a real number, not 'list'\n",
        "ValueError: int() base must be >= 2 and <= 36, or 0\n",
        "ValueError: int() base must be >= 2 and <= 36, or 0\n",
        "ValueError: int() base must be >= 2 and <= 36, or 0\n",
        "TypeError: int() can't convert non-string with explicit base\n",
        "TypeError: int() base must be an integer\n",
        "TypeError: 'x' is an invalid keyword argument for int()\n",
        "TypeError: int() is missing a positional argument\n",
        "TypeError: int expected at most 2 arguments, got 3\n",
        "tuple (0.0, 7.0, 1.0, 1.2676506002282294e+30, 1.5, -inf, 1000.5, 1.5, 2.5, 250000000000.0)\n",
        "ValueError: could not convert string to float: '1_.5'\n",
        "ValueError: could not convert string to float: 'i_nf'\n",
        "ValueError: could not convert string to float: b'\\xff'\n",
        "OverflowError: int too large to convert to float\n",
        "TypeError: float() argument must be a string or a real number, not 'list'\n",
        "TypeError: float() takes no keyword arguments\n",
        "tuple (False, False, True, True, True, True)\n",
    );
    assert_eq!(got, want);
}

#[test]
fn containers_take_every_iterable_and_drain_generators() {
    let got = tried(concat!(
        "t(lambda: (list(), list((1, 2)), list(range(5, 0, -2)), list(range(10**12, 0)), list('hé'), list(b'ab'), list({1: 2, 3: 4}), list(range(2**47 - 1, 2**47 + 1))))\n",
        "t(lambda: (tuple(), tuple([1]), tuple('ab'), tuple(range(0, -10**12)), tuple({5})))\n",
        "t(lambda: (set(), set([1, 1, 2]), set('aba'), set(range(3)), set({1: 2}), set([1.0, 1, true]), set([(1, 2), (1, 2)])))\n",
        "t(lambda: (list(g(3)), tuple(g(2)), set(x % 2 for x in g(5)), list(map(lambda x: -x, g(2))), list(zip(g(2), 'ab'))))\n",
        "t(lambda: (list(5)))\n",
        "t(lambda: (tuple(nil)))\n",
        "t(lambda: (set([[1]])))\n",
        "t(lambda: (set([(1, [2])])))\n",
        "t(lambda: (list(1, 2)))\n",
        "t(lambda: (list(iterable=[1])))\n",
        "t(lambda: (list(co())))\n",
        "t(lambda: (set(co())))\n",
        "t(lambda: (dict(), dict(a=1), dict({'a': 1}, a=2, b=3), dict([('a', 1), ['b', 2]], c=3), dict(zip('ab', [1, 2])), dict(['ab'])))\n",
        "t(lambda: (dict(pairs(), z=0), dict([kv('x'), ('y', 1), kv('w')]), dict((kv(c) for c in 'pq'), r=3), dict(iter([('k', 'v')]))))\n",
        "t(lambda: (dict([('a', 1), kv('a'), (c for c in 'abc')])))\n",
        "t(lambda: (dict([kv('q')])))\n",
        "t(lambda: (dict([1])))\n",
        "t(lambda: (dict([([1], 2)])))\n",
        "t(lambda: (dict(5)))\n",
        "t(lambda: (dict({1: 2}, {3: 4})))\n",
        "t(lambda: (dict([boom('a')])))\n",
        "t(lambda: (dict(co())))\n",
    ));
    let want = concat!(
        "tuple ([], [1, 2], [5, 3, 1], [], ['h', 'é'], [97, 98], [1, 3], [140737488355327, 140737488355328])\n",
        "tuple ((), (1,), ('a', 'b'), (), (5,))\n",
        "tuple (set(), {1, 2}, {'a', 'b'}, {0, 1, 2}, {1}, {1.0}, {(1, 2)})\n",
        "tuple ([0, 1, 2], (0, 1), {0, 1}, [0, -1], [(0, 'a'), (1, 'b')])\n",
        "TypeError: 'int' object is not iterable\n",
        "TypeError: 'NoneType' object is not iterable\n",
        "TypeError: cannot use 'list' as a set element (unhashable type: 'list')\n",
        "TypeError: cannot use 'tuple' as a set element (unhashable type: 'list')\n",
        "TypeError: list expected at most 1 argument, got 2\n",
        "TypeError: list() takes no keyword arguments\n",
        "TypeError: 'coroutine' object is not iterable\n",
        "TypeError: 'coroutine' object is not iterable\n",
        "tuple ({}, {'a': 1}, {'a': 2, 'b': 3}, {'a': 1, 'b': 2, 'c': 3}, {'a': 1, 'b': 2}, {'a': 'b'})\n",
        "tuple ({'a': 1, 'b': 2, 'z': 0}, {'x': 1, 'y': 1, 'w': 1}, {'p': 1, 'q': 1, 'r': 3}, {'k': 'v'})\n",
        "ValueError: dictionary update sequence element #2 has length 3; 2 is required\n",
        "dict {'q': 1}\n",
        "TypeError: object is not iterable\n",
        "TypeError: cannot use 'list' as a dict key (unhashable type: 'list')\n",
        "TypeError: 'int' object is not iterable\n",
        "TypeError: dict expected at most 1 argument, got 2\n",
        "KeyError: 'inner'\n",
        "TypeError: 'coroutine' object is not iterable\n",
    );
    assert_eq!(got, want);
}

#[test]
fn characters_radixes_reprs_and_hashes() {
    let got = tried(concat!(
        "t(lambda: (chr(65), chr(233), chr(0x1F600), chr(true), ord('a'), ord('日'), ord(b'z'), ord(bytearray(b'\\x00'))))\n",
        "t(lambda: (chr(0x110000)))\n",
        "t(lambda: (chr(1.5)))\n",
        "t(lambda: (ord('ab')))\n",
        "t(lambda: (ord(b'')))\n",
        "t(lambda: (ord(5)))\n",
        "t(lambda: (bin(5), bin(-5), bin(0), bin(true), oct(8), oct(-2**64), hex(255), hex(-2**63), hex(2**70)))\n",
        "t(lambda: (hex(1.5)))\n",
        "t(lambda: (repr(5), repr(1.5), repr(nil), repr('a\\nb'), repr(\"it's\"), repr([1, 'x']), repr(b'x'), ascii('é'), ascii(['日']), ascii(7)))\n",
        "t(lambda: (callable(len), callable(str), callable(lambda: 1), callable(5), callable(nil), callable('x'), callable(print)))\n",
        "t(lambda: (hash(5), hash(-1), hash(true), hash(1.5), hash(-1.5), hash(-0.0), hash(float('inf')), hash(1e100), hash(2**61), hash(-(2**61)), hash(10**20)))\n",
        "t(lambda: (hash(()), hash((1, 2)), hash((1, (2, 3))), hash(nil), hash((nil,)), hash(range(0)), hash(range(3)), hash(range(5, 6)), hash(range(-5, 2**64, 3))))\n",
        "t(lambda: (hash(range(5, 2**64, 2**64)) == hash(range(5, 6)), hash(range(2**70)) == hash(range(2**70)), hash(range(2**64, 5)) == hash(range(0)), hash('ab') == hash('ab')))\n",
        "t(lambda: (hash([1])))\n",
        "t(lambda: (hash((1, [2]))))\n",
        "t(lambda: (hash(bytearray(b'x'))))\n",
    ));
    let want = concat!(
        "tuple ('A', 'é', '😀', '\\x01', 97, 26085, 122, 0)\n",
        "ValueError: chr() arg not in range(0x110000)\n",
        "TypeError: 'float' object cannot be interpreted as an integer\n",
        "TypeError: ord() expected a character, but string of length 2 found\n",
        "TypeError: ord() expected a character, but string of length 0 found\n",
        "TypeError: ord() expected string of length 1, but int found\n",
        "tuple ('0b101', '-0b101', '0b0', '0b1', '0o10', '-0o2000000000000000000000', '0xff', '-0x8000000000000000', '0x400000000000000000')\n",
        "TypeError: 'float' object cannot be interpreted as an integer\n",
        "tuple ('5', '1.5', 'None', \"'a\\\\nb'\", '\"it\\'s\"', \"[1, 'x']\", \"b'x'\", \"'\\\\xe9'\", \"['\\\\u65e5']\", '7')\n",
        "tuple (True, True, True, False, False, False, True)\n",
        "tuple (5, -2, 1, 1152921504606846977, -1152921504606846977, 0, 314159, 1822893315824342674, 1, -2, 848750603811160107)\n",
        "tuple (5740354900026072187, -3550055125485641917, 7267574591690527098, 4238894112, 9181102132670838864, 2676694398852732306, -8338477496398685190, -7048428680432378585, -1699638561505372368)\n",
        "tuple (True, True, True, True)\n",
        "TypeError: unhashable type: 'list'\n",
        "TypeError: unhashable type: 'list'\n",
        "TypeError: unhashable type: 'bytearray'\n",
    );
    assert_eq!(got, want);
}

#[test]
fn generators_convert_in_loops_and_frames() {
    let got = run(concat!(
        "fn g(n):\n    for i in range(n):\n        yield i\n",
        "t = 0\nfor i in range(300):\n    t += len(list(g(20))) + len(tuple(g(3))) + len(set(g(4))) + len(dict((k, k) for k in g(5)))\nprint(t)\n",
        "fn f(n):\n    return list(g(n)), dict(zip(g(n), g(n)))\nprint(f(3))\nx = (1, 2)\ns = 'text'\nprint(tuple(x) is x, str(s) is s, list(x) is x)\n",
        "it = g(2)\nprint(list(it), list(it), tuple(it), set(it), dict(it, k=1))\nit = g(3)\nnext(it)\nit.close()\nprint(list(it))\n",
    ));
    assert_eq!(got, "9600\n([0, 1, 2], {0: 0, 1: 1, 2: 2})\nTrue True False\n[0, 1] [] () set() {'k': 1}\n[]\n");
}

/* `name`'s fast path on `x` where none hands the value to the full call */
fn fast(heap: &mut HeapPool, name: &str, x: Val) -> Option<Val> {
    let f = mini::natives::CONVERT.iter().find(|n| n.name == name).and_then(|n| n.fast1)?;
    f(heap, x)
}

fn text(heap: &HeapPool, v: Option<Val>) -> String {
    match v.and_then(|v| heap.try_get(v)) { Some(HeapObj::Str(s)) => s.clone(), _ => String::from("none") }
}

#[test]
fn the_fast_paths_take_plain_values_and_leave_the_rest() {
    assert!(mini::natives::CONVERT.iter().all(|n| n.fast1.is_some()));
    let mut heap = HeapPool::new(1 << 24);
    let put = |heap: &mut HeapPool, obj: HeapObj| match heap.alloc(obj) { Ok(v) => v, Err(e) => panic!("{e:?}") };
    let (digits, spaced, accent, real) = (put(&mut heap, HeapObj::Str("12".into())), put(&mut heap, HeapObj::Str(" 1.5".into())), put(&mut heap, HeapObj::Str("\u{e9}".into())), put(&mut heap, HeapObj::Str("1.5".into())));
    let byte = put(&mut heap, HeapObj::Bytes(vec![7]));
    let list = put(&mut heap, HeapObj::List(Rc::new(RefCell::new(vec![Val::int(1), Val::int(2)]))));
    let nested = put(&mut heap, HeapObj::List(Rc::new(RefCell::new(vec![list]))));
    let tuple = put(&mut heap, HeapObj::Tuple(vec![Val::int(1)]));
    let dict = put(&mut heap, HeapObj::Dict(Rc::new(RefCell::new(DictMap::new()))));
    let int = |v: Option<Val>| v.filter(Val::is_int).map(|v| v.as_int());

    let shown: Vec<String> = [Val::int(-42), Val::bool(true), Val::none(), Val::float(0.5)].iter().map(|&x| { let v = fast(&mut heap, "str", x); text(&heap, v) }).collect();
    assert_eq!(shown, ["-42", "True", "None", "0.5"]);
    assert_eq!(fast(&mut heap, "str", accent).map(|v| v.raw()), Some(accent.raw()));
    assert!(fast(&mut heap, "str", list).is_none() && fast(&mut heap, "repr", accent).is_none() && fast(&mut heap, "ascii", list).is_none());
    let r = fast(&mut heap, "repr", Val::none());
    assert_eq!(text(&heap, r), "None");

    assert_eq!(int(fast(&mut heap, "int", Val::float(-7.9))), Some(-7));
    assert_eq!(int(fast(&mut heap, "int", digits)), Some(12));
    assert!(fast(&mut heap, "int", spaced).is_none() && fast(&mut heap, "int", Val::float(1e300)).is_none());
    assert_eq!(fast(&mut heap, "float", real).map(|v| v.as_float()), Some(1.5));
    assert!(fast(&mut heap, "float", spaced).is_none());
    assert!(fast(&mut heap, "bool", list).is_some_and(|v| v.as_bool()) && fast(&mut heap, "bool", Val::int(0)).is_some_and(|v| !v.as_bool()));

    assert_eq!(fast(&mut heap, "tuple", tuple).map(|v| v.raw()), Some(tuple.raw()));
    assert!(matches!(fast(&mut heap, "list", tuple).and_then(|v| heap.try_get(v)), Some(HeapObj::List(l)) if l.borrow().len() == 1));
    assert!(matches!(fast(&mut heap, "set", list).and_then(|v| heap.try_get(v)), Some(HeapObj::Set(s)) if s.borrow().len() == 2));
    assert!(fast(&mut heap, "set", nested).is_none() && fast(&mut heap, "list", digits).is_none() && fast(&mut heap, "dict", list).is_none());
    assert!(fast(&mut heap, "dict", dict).is_some_and(|v| v.raw() != dict.raw()));

    let c = fast(&mut heap, "chr", Val::int(233));
    assert_eq!(text(&heap, c), "\u{e9}");
    assert!(fast(&mut heap, "chr", Val::int(-1)).is_none() && fast(&mut heap, "chr", Val::float(1.0)).is_none());
    assert_eq!((int(fast(&mut heap, "ord", accent)), int(fast(&mut heap, "ord", byte))), (Some(233), Some(7)));
    assert!(fast(&mut heap, "ord", digits).is_none());
    let radixes: Vec<String> = ["bin", "oct", "hex"].iter().map(|&n| { let v = fast(&mut heap, n, Val::int(-255)); text(&heap, v) }).collect();
    assert_eq!(radixes, ["-0b11111111", "-0o377", "-0xff"]);

    assert_eq!((int(fast(&mut heap, "hash", Val::int(-1))), int(fast(&mut heap, "hash", Val::none()))), (Some(-2), Some(0xFCA8_6420)));
    assert!(fast(&mut heap, "hash", list).is_none() && fast(&mut heap, "callable", list).is_none());
    assert!(fast(&mut heap, "callable", Val::int(5)).is_some_and(|v| !v.as_bool()));
}
