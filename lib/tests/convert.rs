use std::cell::RefCell;
use std::rc::Rc;

use edge::embed::{Limits, VM};
use edge::value::{DictMap, HeapObj, HeapPool, Val};
use mini::RUNTIME;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// each call's type and repr or its error then the generators the calls take
const TRY: &str = concat!(
    "fn t(f) {\n    try {\n        r = f()\n        print(text(type(r)), repr(r))\n    } catch Error as e {\n        print(text(type(e)) + \":\", e)\n    }\n}\n",
    "fn g(n) {\n    for i in range(n) {\n        yield i\n    }\n}\nfn pairs() {\n    yield (\"a\", 1)\n    yield [\"b\", 2]\n}\nfn kv(k) {\n    yield k\n    yield len(k)\n}\n",
    "fn boom(k) {\n    yield k\n    throw KeyError(\"inner\")\n}\n",
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
        "t(|| (text(), text(-12), text(2 ** 70), text(0.1 + 0.2), text(true), text(nil), text([1, \"a\"]), text(b\"x\"), text(object=5)))\n",
        "t(|| (text(b\"h\\xc3\\xa9\", \"utf-8\"), text(b\"a\\xffb\", \"utf-8\", \"replace\"), text(b\"a\\xffb\", errors=\"ignore\"), text(encoding=\"utf-8\")))\n",
        "t(|| text(b\"\\xffa\", \"utf-8\"))\n",
        "t(|| text(5, \"utf-8\"))\n",
        "t(|| text(b\"x\", 5))\n",
        "t(|| text(b\"x\", \"utf-8\", nil))\n",
        "t(|| text(1, 2, 3, 4))\n",
        "t(|| text(x=1))\n",
        "t(|| (int(), int(-7.9), int(true), int(1.0e+30), int(-2.0 ** 127), int(\"  -12 \"), int(\"1_000\"), int(\"٣٤\"), int(b\" 42 \"), int(2 ** 70)))\n",
        "t(|| (int(\"ff\", 16), int(\"0x_ff\", 16), int(\"0b101\", 0), int(\"z\", 36), int(b\"7f\", 16), int(\"ff\", base=16), int(\"0\", false)))\n",
        "t(|| int(float(\"nan\")))\n",
        "t(|| int(float(\"-inf\")))\n",
        "t(|| int(\"12a\"))\n",
        "t(|| int(\"010\", 0))\n",
        "t(|| int(b\"\\xff\"))\n",
        "t(|| int([1]))\n",
        "t(|| int(\"1\", true))\n",
        "t(|| int(\"7\", 2 ** 70))\n",
        "t(|| int(5, 1))\n",
        "t(|| int(5, 2))\n",
        "t(|| int(\"1\", 1.5))\n",
        "t(|| int(x=\"5\"))\n",
        "t(|| int(base=16))\n",
        "t(|| int(1, 2, 3))\n",
        "t(|| (float(), float(7), float(true), float(2 ** 100), float(\" 1.5\\n\"), float(\"-Infinity\"), float(\"1_000.5\"), float(\"١.٥\"), float(b\"2.5\"), float(\"2_5e1_0\")))\n",
        "t(|| float(\"1_.5\"))\n",
        "t(|| float(\"i_nf\"))\n",
        "t(|| float(b\"\\xff\"))\n",
        "t(|| float(2 ** 2000))\n",
        "t(|| float([1]))\n",
        "t(|| float(x=1))\n",
        "t(|| (bool(), bool(0.0), bool(\"a\"), bool([]), bool(range(2, 1)), bool(len)))\n",
    ));
    let want = concat!(
        "tuple (\"\", \"-12\", \"1180591620717411303424\", \"0.30000000000000004\", \"true\", \"nil\", \"[1, \\\"a\\\"]\", \"b\\\"x\\\"\", \"5\")\n",
        "tuple (\"hé\", \"a�b\", \"ab\", \"\")\n",
        "ValueError: utf-8 cannot decode byte 0xff at 0, which starts no character\n",
        "TypeError: text decodes bytes, not int\n",
        "TypeError: text takes text as encoding, not int\n",
        "TypeError: text takes text as errors, not nil\n",
        "TypeError: text() takes 0 to 3 arguments, not 4\n",
        "TypeError: text() takes no keyword x\n",
        "tuple (0, -7, 1, 1000000000000000019884624838656, -170141183460469231731687303715884105728, -12, 1000, 34, 42, 1180591620717411303424)\n",
        "tuple (255, 255, 5, 35, 127, 255, 0)\n",
        "ValueError: cannot convert NaN to int\n",
        "OverflowError: cannot convert infinity to int\n",
        "ValueError: \"12a\" spells no int in base 10\n",
        "ValueError: \"010\" spells no int\n",
        "ValueError: b\"\\xff\" spells no int in base 10\n",
        "TypeError: int takes text, bytes or a number, not list\n",
        "ValueError: base of int must be 0 or from 2 to 36\n",
        "ValueError: base of int must be 0 or from 2 to 36\n",
        "ValueError: base of int must be 0 or from 2 to 36\n",
        "TypeError: int takes base only with text or bytes\n",
        "TypeError: int takes int as base\n",
        "TypeError: int() takes no keyword x\n",
        "TypeError: int takes a value to convert\n",
        "TypeError: int() takes 0 to 2 arguments, not 3\n",
        "tuple (0.0, 7.0, 1.0, 1.2676506002282294e30, 1.5, -inf, 1000.5, 1.5, 2.5, 250000000000.0)\n",
        "ValueError: \"1_.5\" spells no float\n",
        "ValueError: \"i_nf\" spells no float\n",
        "ValueError: b\"\\xff\" spells no float\n",
        "OverflowError: the number is too large for floating point\n",
        "TypeError: float takes text or a number, not list\n",
        "TypeError: float() takes no keyword x\n",
        "tuple (false, false, true, true, true, true)\n",
    );
    assert_eq!(got, want);
}

#[test]
fn containers_take_every_iterable_and_drain_generators() {
    let got = tried(concat!(
        "t(|| (list(), list((1, 2)), list(range(5, 0, -2)), list(range(10 ** 12, 0)), list(\"hé\"), list(b\"ab\"), list(record([(1, 2), (3, 4)])), list(range(2 ** 47 - 1, 2 ** 47 + 1))))\n",
        "t(|| (tuple(), tuple([1]), tuple(\"ab\"), tuple(range(0, -10 ** 12)), tuple(set([5]))))\n",
        "t(|| (set(), set([1, 1, 2]), set(\"aba\"), set(range(3)), set(record([(1, 2)])), set([1.0, 1, true]), set([(1, 2), (1, 2)])))\n",
        "t(|| (list(g(3)), tuple(g(2)), set((x % 2 for x in g(5))), list(map(|x| -x, g(2))), list(zip(g(2), \"ab\"))))\n",
        "t(|| list(5))\n",
        "t(|| tuple(nil))\n",
        "t(|| set([[1]]))\n",
        "t(|| set([(1, [2])]))\n",
        "t(|| list(1, 2))\n",
        "t(|| list(iterable=[1]))\n",
        "t(|| (record(), record(a=1), record({a = 1}, a=2, b=3), record([(\"a\", 1), [\"b\", 2]], c=3), record(zip(\"ab\", [1, 2])), record([\"ab\"])))\n",
        "t(|| (record(pairs(), z=0), record([kv(\"x\"), (\"y\", 1), kv(\"w\")]), record((kv(c) for c in \"pq\"), r=3), record(iter([(\"k\", \"v\")]))))\n",
        "t(|| record([(\"a\", 1), kv(\"a\"), (c for c in \"abc\")]))\n",
        "t(|| record([kv(\"q\")]))\n",
        "t(|| record([1]))\n",
        "t(|| record([([1], 2)]))\n",
        "t(|| record(5))\n",
        "t(|| record(record([(1, 2)]), record([(3, 4)])))\n",
        "t(|| record([boom(\"a\")]))\n",
    ));
    let want = concat!(
        "tuple ([], [1, 2], [5, 3, 1], [], [\"h\", \"é\"], [97, 98], [1, 3], [140737488355327, 140737488355328])\n",
        "tuple ((), (1,), (\"a\", \"b\"), (), (5,))\n",
        "tuple (set(), {1, 2}, {\"a\", \"b\"}, {0, 1, 2}, {1}, {1.0}, {(1, 2)})\n",
        "tuple ([0, 1, 2], (0, 1), {0, 1}, [0, -1], [(0, \"a\"), (1, \"b\")])\n",
        "TypeError: a value of kind int cannot be iterated\n",
        "TypeError: a value of kind nil cannot be iterated\n",
        "TypeError: a value of kind list cannot stand as a key\n",
        "TypeError: a value of kind tuple cannot stand as a key\n",
        "TypeError: list() takes 0 to 1 arguments, not 2\n",
        "TypeError: list() takes no keyword iterable\n",
        "tuple ({}, {a = 1}, {a = 2, b = 3}, {a = 1, b = 2, c = 3}, {a = 1, b = 2}, {a = \"b\"})\n",
        "tuple ({a = 1, b = 2, z = 0}, {x = 1, y = 1, w = 1}, {p = 1, q = 1, r = 3}, {k = \"v\"})\n",
        "ValueError: entry 2 holds 3 items where a pair holds 2\n",
        "record {q = 1}\n",
        "TypeError: each entry must be a pair of a key and a value\n",
        "TypeError: a value of kind list cannot stand as a key\n",
        "TypeError: a value of kind int cannot be iterated\n",
        "TypeError: record() takes 0 to 1 arguments, not 2\n",
        "KeyError: inner\n",
    );
    assert_eq!(got, want);
}

#[test]
fn characters_radixes_reprs_and_hashes() {
    let got = tried(concat!(
        "t(|| (chr(65), chr(233), chr(0x1F600), chr(true), ord(\"a\"), ord(\"日\"), ord(b\"z\"), ord(bytearray(b\"\\x00\"))))\n",
        "t(|| chr(0x110000))\n",
        "t(|| chr(1.5))\n",
        "t(|| ord(\"ab\"))\n",
        "t(|| ord(b\"\"))\n",
        "t(|| ord(5))\n",
        "t(|| (bin(5), bin(-5), bin(0), bin(true), oct(8), oct(-2 ** 64), hex(255), hex(-2 ** 63), hex(2 ** 70)))\n",
        "t(|| hex(1.5))\n",
        "t(|| (repr(5), repr(1.5), repr(nil), repr(\"a\\nb\"), repr(\"it's\"), repr([1, \"x\"]), repr(b\"x\")))\n",
        "t(|| (callable(len), callable(text), callable(|| 1), callable(5), callable(nil), callable(\"x\"), callable(print)))\n",
    ));
    let want = concat!(
        "tuple (\"A\", \"é\", \"😀\", \"\\u{1}\", 97, 26085, 122, 0)\n",
        "ValueError: chr takes a code point from 0 to 0x10FFFF\n",
        "TypeError: chr takes int, not float\n",
        "TypeError: ord takes one character, not 2\n",
        "TypeError: ord takes one character, not 0\n",
        "TypeError: ord takes one character, not int\n",
        "tuple (\"0b101\", \"-0b101\", \"0b0\", \"0b1\", \"0o10\", \"-0o2000000000000000000000\", \"0xff\", \"-0x8000000000000000\", \"0x400000000000000000\")\n",
        "TypeError: hex takes int, not float\n",
        "tuple (\"5\", \"1.5\", \"nil\", \"\\\"a\\\\nb\\\"\", \"\\\"it's\\\"\", \"[1, \\\"x\\\"]\", \"b\\\"x\\\"\")\n",
        "tuple (true, true, true, false, false, false, true)\n",
    );
    assert_eq!(got, want);
}

#[test]
fn generators_convert_in_loops_and_frames() {
    let got = run(concat!(
        "fn g(n) {\n    for i in range(n) {\n        yield i\n    }\n}\n",
        "t = 0\nfor i in range(300) {\n    t += len(list(g(20))) + len(tuple(g(3))) + len(set(g(4))) + len(record(((k, k) for k in g(5))))\n}\nprint(t)\n",
        "fn f(n) = (list(g(n)), record(zip(g(n), g(n))))\nprint(f(3))\n",
        "it = g(2)\nprint(list(it), list(it), tuple(it), set(it), record(it, k=1))\n",
    ));
    assert_eq!(got, "9600\n([0, 1, 2], {0 = 0, 1 = 1, 2 = 2})\n[0, 1] [] () set() {k = 1}\n");
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

    let shown: Vec<String> = [Val::int(-42), Val::bool(true), Val::none(), Val::float(0.5)].iter().map(|&x| { let v = fast(&mut heap, "text", x); text(&heap, v) }).collect();
    assert_eq!(shown, ["-42", "true", "nil", "0.5"]);
    assert_eq!(fast(&mut heap, "text", accent).map(|v| v.raw()), Some(accent.raw()));
    assert!(fast(&mut heap, "text", list).is_none() && fast(&mut heap, "repr", accent).is_none());
    let r = fast(&mut heap, "repr", Val::none());
    assert_eq!(text(&heap, r), "nil");

    assert_eq!(int(fast(&mut heap, "int", Val::float(-7.9))), Some(-7));
    assert_eq!(int(fast(&mut heap, "int", digits)), Some(12));
    assert!(fast(&mut heap, "int", spaced).is_none() && fast(&mut heap, "int", Val::float(1e300)).is_none());
    assert_eq!(fast(&mut heap, "float", real).map(|v| v.as_float()), Some(1.5));
    assert!(fast(&mut heap, "float", spaced).is_none());
    assert!(fast(&mut heap, "bool", list).is_some_and(|v| v.as_bool()) && fast(&mut heap, "bool", Val::int(0)).is_some_and(|v| !v.as_bool()));

    assert_eq!(fast(&mut heap, "tuple", tuple).map(|v| v.raw()), Some(tuple.raw()));
    assert!(matches!(fast(&mut heap, "list", tuple).and_then(|v| heap.try_get(v)), Some(HeapObj::List(l)) if l.borrow().len() == 1));
    assert!(matches!(fast(&mut heap, "set", list).and_then(|v| heap.try_get(v)), Some(HeapObj::Set(s)) if s.borrow().len() == 2));
    assert!(fast(&mut heap, "set", nested).is_none() && fast(&mut heap, "list", digits).is_none() && fast(&mut heap, "record", list).is_none());
    assert!(fast(&mut heap, "record", dict).is_some_and(|v| v.raw() != dict.raw()));

    let c = fast(&mut heap, "chr", Val::int(233));
    assert_eq!(text(&heap, c), "\u{e9}");
    assert!(fast(&mut heap, "chr", Val::int(-1)).is_none() && fast(&mut heap, "chr", Val::float(1.0)).is_none());
    assert_eq!((int(fast(&mut heap, "ord", accent)), int(fast(&mut heap, "ord", byte))), (Some(233), Some(7)));
    assert!(fast(&mut heap, "ord", digits).is_none());
    let radixes: Vec<String> = ["bin", "oct", "hex"].iter().map(|&n| { let v = fast(&mut heap, n, Val::int(-255)); text(&heap, v) }).collect();
    assert_eq!(radixes, ["-0b11111111", "-0o377", "-0xff"]);

    assert!(fast(&mut heap, "callable", list).is_none());
    assert!(fast(&mut heap, "callable", Val::int(5)).is_some_and(|v| !v.as_bool()));
}
