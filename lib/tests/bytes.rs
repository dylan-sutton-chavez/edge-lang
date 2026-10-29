use edge::embed::{Limits, VM};
use mini::RUNTIME;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// each call's repr or its error in the words the corpus reads
const TRY: &str = "fn t(f):\n    try:\n        print(repr(f()))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\nfn g(n):\n    for i in range(n):\n        yield i\nfn bad():\n    yield 1\n    yield 300\n    print('past the bad one')\n";

/* what the calls printed through the helpers above */
fn tried(calls: &str) -> String {
    let src = String::from(TRY) + calls;
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(&src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

#[test]
fn constructors_take_every_source() {
    let got = tried(concat!(
        "t(lambda: bytes(3))\nt(lambda: bytearray([65, true]))\nt(lambda: bytes('h\\u00e9', 'utf-8'))\nt(lambda: bytes(source=b'q'))\n",
        "t(lambda: bytes(g(3)))\nt(lambda: bytearray(x for x in b'ab'))\nt(lambda: bytes({7: 0}))\nt(lambda: bytes(-1))\n",
        "t(lambda: bytes(-2**70))\nt(lambda: bytes(2**70))\nt(lambda: bytes(10**12))\nt(lambda: bytes('x'))\nt(lambda: bytes(1.5))\n",
        "t(lambda: bytearray([256]))\nt(lambda: bytes([256]))\nt(lambda: bytes(['a']))\nt(lambda: bytes(7, 3))\nt(lambda: bytes('x', 'utf-8', nil))\n",
        "t(lambda: bytes(errors='strict'))\nt(lambda: bytes('x', errors='strict'))\nt(lambda: bytes(5, 'utf-8'))\nt(lambda: bytes(b'x', errors='e'))\n",
        "t(lambda: bytes(bad()))\nt(lambda: bytes_fromhex('41 42'))\n",
    ));
    let want = concat!(
        "b'\\x00\\x00\\x00'\nbytearray(b'A\\x01')\nb'h\\xc3\\xa9'\nb'q'\n",
        "b'\\x00\\x01\\x02'\nbytearray(b'ab')\nb'\\x07'\nValueError: negative count\n",
        "OverflowError: cannot fit 'int' into an index-sized integer\nOverflowError: cannot fit 'int' into an index-sized integer\nMemoryError: memory limit\n",
        "TypeError: string argument without an encoding\nTypeError: cannot convert 'float' object to bytes\n",
        "ValueError: byte must be in range(0, 256)\nValueError: bytes must be in range(0, 256)\nTypeError: 'str' object cannot be interpreted as an integer\n",
        "TypeError: bytes() argument 'encoding' must be str, not int\nTypeError: bytes() argument 'errors' must be str, not None\n",
        "TypeError: errors without a string argument\nTypeError: string argument without an encoding\nTypeError: encoding without a string argument\n",
        "TypeError: errors without a string argument\nValueError: bytes must be in range(0, 256)\nb'AB'\n",
    );
    assert_eq!(got, want);
}

#[test]
fn fromhex_reads_str_and_bytes_skipping_whitespace_between_pairs() {
    let got = tried(concat!(
        "t(lambda: bytes.fromhex(' 41\\x0b42 '))\nt(lambda: bytearray.fromhex(b'4142'))\nt(lambda: b'x'.fromhex('43'))\n",
        "t(lambda: bytes.fromhex('4 1'))\nt(lambda: bytes.fromhex('4'))\nt(lambda: bytes.fromhex('4 \\u00e9'))\nt(lambda: bytes.fromhex(5))\n",
    ));
    let want = concat!(
        "b'AB'\nbytearray(b'AB')\nb'C'\n",
        "ValueError: non-hexadecimal number found in fromhex() arg at position 1\n",
        "ValueError: fromhex() arg must contain an even number of hexadecimal digits\n",
        "ValueError: non-hexadecimal number found in fromhex() arg at position 2\n",
        "TypeError: fromhex() argument must be str or bytes-like, not int\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_bytearray_gets_bytearrays_back_where_bytes_get_bytes() {
    let got = tried(concat!(
        "for k in [bytes, bytearray]:\n    x = k(b'a,b c')\n",
        "    print(x.upper(), x.split(b','), x.partition(b','), x.rpartition(b'z'), x.replace(b',', b''), x.center(7, b'.'))\n",
        "    print(x.strip(b'a'), x.join([b'1', b'2']), x.join(bytes([i]) for i in b'yz'), x.splitlines(), x.zfill(3), x.removeprefix(b'z'))\n",
        "    print(x.decode(), x.hex(':', 2), x.find(44), x.count(b''), x.startswith((b'q', b'a')), x.isalpha(), k.fromhex('2c'))\n",
        "print([type(p).__name__ for p in b'a,b'.partition(bytearray(b','))], bytearray(b'xy').take_bytes(1))\n",
    ));
    let want = concat!(
        "b'A,B C' [b'a', b'b c'] (b'a', b',', b'b c') (b'', b'', b'a,b c') b'ab c' b'.a,b c.'\n",
        "b',b c' b'1a,b c2' b'ya,b cz' [b'a,b c'] b'a,b c' b'a,b c'\n",
        "a,b c 61:2c62:2063 1 6 True False b','\n",
        "bytearray(b'A,B C') [bytearray(b'a'), bytearray(b'b c')] (bytearray(b'a'), bytearray(b','), bytearray(b'b c')) (bytearray(b''), bytearray(b''), bytearray(b'a,b c')) bytearray(b'ab c') bytearray(b'.a,b c.')\n",
        "bytearray(b',b c') bytearray(b'1a,b c2') bytearray(b'ya,b cz') [bytearray(b'a,b c')] bytearray(b'a,b c') bytearray(b'a,b c')\n",
        "a,b c 61:2c62:2063 1 6 True False bytearray(b',')\n",
        "['bytes', 'bytearray', 'bytes'] b'x'\n",
    );
    assert_eq!(got, want);
}

#[test]
fn a_bad_item_stops_the_drain_where_it_stands() {
    let got = tried(concat!(
        "b = bytearray(b'z')\nt(lambda: b.extend(bad()))\nt(lambda: b.extend(x + 65 for x in range(2)))\nt(lambda: b.extend(map(lambda x: x, [66])))\n",
        "t(lambda: b.extend('s'))\nt(lambda: b.extend(5))\nprint(b)\nt(lambda: bytearray(bad()))\nt(lambda: b'-'.join(x for x in [b'a', 1]))\n",
        "t(lambda: list(map(bytes, [g(2), g(1)])))\n",
    ));
    let want = concat!(
        "ValueError: byte must be in range(0, 256)\nNone\nNone\n",
        "TypeError: expected iterable of integers; got: 'str'\nTypeError: can't extend bytearray with int\nbytearray(b'zABB')\n",
        "ValueError: byte must be in range(0, 256)\nTypeError: sequence item 1: expected a bytes-like object, int found\n",
        "[b'\\x00\\x01', b'\\x00']\n",
    );
    assert_eq!(got, want);
}

#[test]
fn arguments_convert_as_ints_and_bounds() {
    let got = tried(concat!(
        "x = b'hello world'\nt(lambda: x.find(b'o', true))\nt(lambda: x.find(b'o', 'x'))\nt(lambda: x.split(nil, true))\nt(lambda: x.split(maxsplit=nil))\n",
        "t(lambda: x.replace(b'l', b'L', count=1))\nt(lambda: x.replace(b'l', b'L', 2**70))\nt(lambda: x.hex(':', -2**70))\nt(lambda: x.count(2**70))\n",
        "t(lambda: x.strip(nil))\nt(lambda: x.strip('h'))\nt(lambda: x.center(13, nil))\nt(lambda: x.decode(nil))\nt(lambda: x.startswith((b'h', 1)))\n",
        "b = bytearray(b'abc')\nt(lambda: b.pop(true))\nt(lambda: b.insert(true, 120))\nt(lambda: b.pop('x'))\nt(lambda: b.resize(2**70))\n",
        "t(lambda: b.take_bytes(-5))\nt(lambda: b.take_bytes('x'))\nt(lambda: b.take_bytes(2**70))\nprint(b)\n",
    ));
    let want = concat!(
        "4\nTypeError: slice indices must be integers or have an __index__ method\n[b'hello', b'world']\nTypeError: 'NoneType' object cannot be interpreted as an integer\n",
        "b'heLlo world'\nOverflowError: Python int too large to convert to C ssize_t\nOverflowError: Python int too large to convert to C ssize_t\nValueError: byte must be in range(0, 256)\n",
        "b'hello world'\nTypeError: a bytes-like object is required, not 'str'\nTypeError: center() argument 2 must be a byte string of length 1, not None\n",
        "TypeError: decode() argument 'encoding' must be str, not None\nTrue\n",
        "98\nNone\nTypeError: 'str' object cannot be interpreted as an integer\nOverflowError: Python int too large to convert to C ssize_t\n",
        "IndexError: can't take -2 bytes outside size 3\nTypeError: n must be an integer or None\nIndexError: cannot fit 'int' into an index-sized integer\nbytearray(b'axc')\n",
    );
    assert_eq!(got, want);
}

#[test]
fn the_fast_paths_answer_as_the_full_calls() {
    let mut heap = edge::value::HeapPool::new(1 << 20);
    let fast = |name: &str| mini::natives::BYTES.iter().find(|n| n.name == name).and_then(|n| n.fast1).unwrap_or_else(|| panic!("{name}"));
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
