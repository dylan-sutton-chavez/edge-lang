use edge::embed::{Limits, VM};
use rig::RUNTIME;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// each call's repr or its error in the words the corpus reads
const TRY: &str = "fn t(f) {\n    try {\n        print(repr(f()))\n    } catch Error as e {\n        print(text(type(e)) + \":\", e)\n    }\n}\nfn g(n) {\n    for i in range(n) {\n        yield i\n    }\n}\nfn bad() {\n    yield 1\n    yield 300\n    print(\"past the bad one\")\n}\n";

/* what the calls printed through the helpers above */
fn tried(calls: &str) -> String {
    let src = String::from(TRY) + calls;
    let (chunk, errors) = edge::code::compile_as::<rig::Rig>(&src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

#[test]
fn constructors_take_every_source() {
    let got = tried(concat!(
        "t(|| bytes(3))\nt(|| bytearray([65, true]))\nt(|| bytes(\"hé\", \"utf-8\"))\nt(|| bytes(source=b\"q\"))\n",
        "t(|| bytes(g(3)))\nt(|| bytearray((x for x in b\"ab\")))\nt(|| bytes(record([(7, 0)])))\nt(|| bytes(-1))\n",
        "t(|| bytes(-2 ** 70))\nt(|| bytes(2 ** 70))\nt(|| bytes(10 ** 12))\nt(|| bytes(\"x\"))\nt(|| bytes(1.5))\n",
        "t(|| bytearray([256]))\nt(|| bytes([256]))\nt(|| bytes([\"a\"]))\nt(|| bytes(7, 3))\nt(|| bytes(\"x\", \"utf-8\", nil))\n",
        "t(|| bytes(errors=\"strict\"))\nt(|| bytes(\"x\", errors=\"strict\"))\nt(|| bytes(5, \"utf-8\"))\nt(|| bytes(b\"x\", errors=\"e\"))\n",
        "t(|| bytes(bad()))\nt(|| bytes_fromhex(\"41 42\"))\n",
    ));
    let want = concat!(
        "b\"\\x00\\x00\\x00\"\n",
        "b\"A\\x01\"\n",
        "b\"h\\xc3\\xa9\"\n",
        "b\"q\"\n",
        "b\"\\x00\\x01\\x02\"\n",
        "b\"ab\"\n",
        "b\"\\x07\"\n",
        "ValueError: bytes takes a count that is not negative\n",
        "OverflowError: int too large for an index\n",
        "OverflowError: int too large for an index\n",
        "MemoryError: memory limit\n",
        "TypeError: bytes needs encoding to convert text\n",
        "TypeError: bytes cannot convert float\n",
        "ValueError: bytearray takes a byte from 0 to 255\n",
        "ValueError: bytes takes bytes from 0 to 255\n",
        "TypeError: bytes takes int, not text\n",
        "TypeError: bytes takes text as encoding, not int\n",
        "TypeError: bytes takes text as errors, not nil\n",
        "TypeError: bytes takes errors only with text to convert\n",
        "TypeError: bytes needs encoding to convert text\n",
        "TypeError: bytes takes encoding only with text to convert\n",
        "TypeError: bytes takes errors only with text to convert\n",
        "ValueError: bytes takes bytes from 0 to 255\n",
        "b\"AB\"\n",
    );
    assert_eq!(got, want);
}

#[test]
fn fromhex_reads_str_and_bytes_skipping_whitespace_between_pairs() {
    let got = tried(concat!(
        "t(|| bytes.fromhex(\" 41\\u{b}42 \"))\nt(|| bytearray.fromhex(b\"4142\"))\nt(|| b\"x\".fromhex(\"43\"))\n",
        "t(|| bytes.fromhex(\"4 1\"))\nt(|| bytes.fromhex(\"4\"))\nt(|| bytes.fromhex(\"4 é\"))\nt(|| bytes.fromhex(5))\n",
    ));
    let want = concat!(
        "b\"AB\"\n",
        "b\"AB\"\n",
        "b\"C\"\n",
        "ValueError: fromhex found no hex digit at position 1\n",
        "ValueError: fromhex takes an even count of hex digits\n",
        "ValueError: fromhex found no hex digit at position 2\n",
        "TypeError: fromhex takes text or bytes, not int\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_bytearray_gets_bytearrays_back_where_bytes_get_bytes() {
    let got = tried(concat!(
        "for k in [bytes, bytearray] {\n    x = k(b\"a,b c\")\n",
        "    print(x.upper(), x.split(b\",\"), x.partition(b\",\"), x.rpartition(b\"z\"), x.replace(b\",\", b\"\"), x.center(7, b\".\"))\n",
        "    print(x.strip(b\"a\"), x.join([b\"1\", b\"2\"]), x.join(bytes([i]) for i in b\"yz\"), x.splitlines(), x.zfill(3), x.removeprefix(b\"z\"))\n",
        "    print(x.decode(), x.hex(\":\", 2), x.find(44), x.count(b\"\"), x.startswith((b\"q\", b\"a\")), x.isalpha(), k.fromhex(\"2c\"))\n}\n",
        "print([text(type(p)) for p in b\"a,b\".partition(bytearray(b\",\"))], bytearray(b\"xy\").take_bytes(1))\n",
    ));
    let want = concat!(
        "b\"A,B C\" [b\"a\", b\"b c\"] (b\"a\", b\",\", b\"b c\") (b\"\", b\"\", b\"a,b c\") b\"ab c\" b\".a,b c.\"\n",
        "b\",b c\" b\"1a,b c2\" b\"ya,b cz\" [b\"a,b c\"] b\"a,b c\" b\"a,b c\"\n",
        "a,b c 61:2c62:2063 1 6 true false b\",\"\n",
        "b\"A,B C\" [b\"a\", b\"b c\"] (b\"a\", b\",\", b\"b c\") (b\"\", b\"\", b\"a,b c\") b\"ab c\" b\".a,b c.\"\n",
        "b\",b c\" b\"1a,b c2\" b\"ya,b cz\" [b\"a,b c\"] b\"a,b c\" b\"a,b c\"\n",
        "a,b c 61:2c62:2063 1 6 true false b\",\"\n",
        "[\"bytes\", \"bytearray\", \"bytes\"] b\"x\"\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_bad_item_stops_the_drain_where_it_stands() {
    let got = tried(concat!(
        "b = bytearray(b\"z\")\nt(|| b.extend(bad()))\nt(|| b.extend((x + 65 for x in range(2))))\nt(|| b.extend(map(|x| x, [66])))\n",
        "t(|| b.extend(\"s\"))\nt(|| b.extend(5))\nprint(b)\nt(|| bytearray(bad()))\nt(|| b\"-\".join((x for x in [b\"a\", 1])))\n",
        "t(|| list(map(bytes, [g(2), g(1)])))\n",
    ));
    let want = concat!(
        "ValueError: extend takes a byte from 0 to 255\n",
        "nil\n",
        "nil\n",
        "TypeError: extend takes bytes or int values, not text\n",
        "TypeError: cannot extend bytearray with int\n",
        "b\"zABB\"\n",
        "ValueError: bytearray takes a byte from 0 to 255\n",
        "TypeError: item 1 of join is int, not bytes\n",
        "[b\"\\x00\\x01\", b\"\\x00\"]\n",
    );
    assert_eq!(got, want);
}

#[test]
fn arguments_convert_as_ints_and_bounds() {
    let got = tried(concat!(
        "x = b\"hello world\"\nt(|| x.find(b\"o\", true))\nt(|| x.find(b\"o\", \"x\"))\nt(|| x.split(nil, true))\nt(|| x.split(maxsplit=nil))\n",
        "t(|| x.replace(b\"l\", b\"L\", count=1))\nt(|| x.replace(b\"l\", b\"L\", 2 ** 70))\nt(|| x.hex(\":\", -2 ** 70))\nt(|| x.count(2 ** 70))\n",
        "t(|| x.strip(nil))\nt(|| x.strip(\"h\"))\nt(|| x.center(13, nil))\nt(|| x.decode(nil))\nt(|| x.startswith((b\"h\", 1)))\n",
        "b = bytearray(b\"abc\")\nt(|| b.pop(true))\nt(|| b.insert(true, 120))\nt(|| b.pop(\"x\"))\nt(|| b.resize(2 ** 70))\n",
        "t(|| b.take_bytes(-5))\nt(|| b.take_bytes(\"x\"))\nt(|| b.take_bytes(2 ** 70))\nprint(b)\n",
    ));
    let want = concat!(
        "4\n",
        "TypeError: the bounds of a slice must be int\n",
        "[b\"hello\", b\"world\"]\n",
        "TypeError: split takes int, not nil\n",
        "b\"heLlo world\"\n",
        "OverflowError: int too large for an index\n",
        "OverflowError: int too large for an index\n",
        "ValueError: count takes a byte from 0 to 255\n",
        "b\"hello world\"\n",
        "TypeError: strip takes bytes, not text\n",
        "TypeError: center takes one byte as its fill, not nil\n",
        "TypeError: decode takes text as encoding, not nil\n",
        "true\n",
        "98\n",
        "nil\n",
        "TypeError: pop takes int, not text\n",
        "OverflowError: int too large for an index\n",
        "IndexError: cannot take -2 bytes of 3\n",
        "TypeError: the count must be int or nil\n",
        "IndexError: int too large for an index\n",
        "b\"axc\"\n",
    );
    assert_eq!(got, want);
}

#[test]
fn the_fast_paths_answer_as_the_full_calls() {
    let mut heap = edge::value::HeapPool::new(1 << 20);
    let fast = |name: &str| rig::natives::BYTES.iter().find(|n| n.name == name).and_then(|n| n.fast1).unwrap_or_else(|| panic!("{name}"));
    let shown = |heap: &edge::value::HeapPool, v: Option<edge::value::Val>| v.and_then(|v| heap.bytes_of(v).map(<[u8]>::to_vec));
    let three = edge::value::Val::int(3);
    let zeros = fast("bytes")(&mut heap, three);
    assert_eq!(shown(&heap, zeros), Some(vec![0, 0, 0]));
    let hex = heap.alloc(edge::value::HeapObj::Str("41 42".to_string())).unwrap_or_else(|e| panic!("{e:?}"));
    let ab = fast("bytes_fromhex")(&mut heap, hex);
    assert_eq!(shown(&heap, ab), Some(vec![0x41, 0x42]));
    let odd = heap.alloc(edge::value::HeapObj::Str("4".to_string())).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(fast("bytes_fromhex")(&mut heap, odd).is_none());
    let list = heap.alloc(edge::value::HeapObj::List(std::rc::Rc::new(core::cell::RefCell::new(vec![three, edge::value::Val::int(300)])))).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(fast("bytearray")(&mut heap, list).is_none());
    assert!(fast("bytes")(&mut heap, edge::value::Val::int(-1)).is_none());
}
