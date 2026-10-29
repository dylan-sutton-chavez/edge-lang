use edge::embed::{Limits, VM};
use edge::lang::{Args, Cx, Native, Natives, Ret, Runtime, Sig};
use edge::value::foreign::{BinOp, Snapshot, UnOp};
use edge::value::{DictMap, HeapPool, Val, hash_val_with_heap};
use lib::Site;
use lib::fail::{Fail, fail};
use lib::num::big::BigInt;
use lib::num::int::Parse;
use lib::uni::Decimal;
use mini::Mini;
use mini::lang::WIDE;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

/* a heap whose wide ints this kind boxes as a runtime installing it would */
fn heap() -> HeapPool {
    let mut h = HeapPool::new(1 << 26);
    h.set_wide_int(&WIDE);
    h
}

fn op(h: &mut HeapPool, op: BinOp, a: Val, b: Val) -> Val {
    match h.foreign_binop(op, a, b) {
        Ok(Some(v)) => v,
        r => panic!("{op:?} gave {:?}", r.map(|v| v.map(|v| v.raw()))),
    }
}

fn repr(h: &HeapPool, v: Val) -> String {
    if v.is_int() { return format!("{}", v.as_int()); }
    match h.foreign_repr(v) { Some(Ok(s)) => s, r => panic!("no repr {:?}", r.map(|r| r.map_err(|e| format!("{e}")))) }
}

#[test]
fn bigint_round_trips_its_text_bytes_and_floats() {
    let digits: Vec<u8> = "1275966880838951957140048049196824912750462188239772872474131448732708150300526818376846344705".bytes().map(|b| b - b'0').collect();
    let b = BigInt::from_digits(false, &digits, 10);
    let bytes: Vec<u8> = (1..40).collect();
    assert_eq!(BigInt::from_bytes_le(&bytes, true), b);
    assert_eq!(b.to_radix(10).len(), digits.len());
    let neg = BigInt::from_i128(-1).mul(&BigInt::from_i128(1).shl(100));
    assert_eq!(neg.to_bytes(16, true).map(|mut v| { v.reverse(); v.iter().map(|x| format!("{x:02x}")).collect::<String>() }), Some(String::from("fffffff0000000000000000000000000")));
    assert_eq!(BigInt::from_i128(1).shl(200).to_f64(), Some(1.6069380442589903e60));
    assert_eq!(BigInt::from_f64(1.6069380442589903e60), BigInt::from_i128(1).shl(200));
    let mut out = Vec::new();
    neg.encode(&mut out);
    assert_eq!(BigInt::decode(&out), Some(neg));
    let three = BigInt::from_i128(3).pow(150);
    assert_eq!((three.bit_length(), three.bit_count()), (238, 119));
    let (q, r) = three.divmod(&BigInt::from_i128(7).pow(40)).unwrap_or_default();
    assert_eq!((q.to_radix(10), r.to_radix(10)), (String::from("58112105022393747904914796275992515767"), String::from("1063019302470221748900469164424482")));
}

#[test]
fn the_kind_computes_past_i128_and_narrows_back() {
    let mut h = heap();
    let big = op(&mut h, BinOp::Pow, Val::int(2), Val::int(200));
    let half = op(&mut h, BinOp::Pow, Val::int(2), Val::int(127));
    let sq = op(&mut h, BinOp::Mul, half, half);
    assert_eq!(repr(&h, sq), "28948022309329048855892746252171976963317496166410141009864396001978282409984");
    let up = op(&mut h, BinOp::Add, big, Val::int(1));
    let one = op(&mut h, BinOp::Sub, up, big);
    assert!(one.is_int() && one.as_int() == 1);
    let low = match h.foreign_unop(UnOp::Neg, big) { Ok(Some(v)) => v, _ => panic!("neg") };
    let q = op(&mut h, BinOp::FloorDiv, low, Val::int(7));
    let r = op(&mut h, BinOp::Mod, low, Val::int(7));
    assert_eq!((repr(&h, q), repr(&h, r)), (String::from("-229562577751284325077423156048737514646028999111827547900196"), String::from("-4")));
    let mask = op(&mut h, BinOp::Add, half, Val::int(0));
    let and = op(&mut h, BinOp::And, big, mask);
    let or = op(&mut h, BinOp::Or, low, Val::int(3));
    assert_eq!((repr(&h, and), repr(&h, or)), (String::from("0"), String::from("-1606938044258990275541962092341162602522202993782792835301373")));
    let inv = match h.foreign_unop(UnOp::Invert, big) { Ok(Some(v)) => v, _ => panic!("invert") };
    assert_eq!(repr(&h, inv), "-1606938044258990275541962092341162602522202993782792835301377");
    let shifted = op(&mut h, BinOp::Shr, big, Val::int(199));
    assert!(shifted.is_int() && shifted.as_int() == 2);
    let tiny = op(&mut h, BinOp::Pow, big, Val::int(-1));
    assert!(tiny.is_float() && tiny.as_float() == 6.223015277861142e-61);
    let ratio = op(&mut h, BinOp::TrueDiv, big, half);
    assert!(ratio.is_float() && ratio.as_float() == 9.44473296573929e21);
}

#[test]
fn the_kind_orders_hashes_and_saves_as_the_engine_did() {
    let mut h = heap();
    let big = op(&mut h, BinOp::Pow, Val::int(2), Val::int(200));
    let part = op(&mut h, BinOp::Pow, Val::int(2), Val::int(100));
    let again = op(&mut h, BinOp::Mul, part, part);
    let boxed = op(&mut h, BinOp::Pow, Val::int(2), Val::int(64));
    let low = match h.foreign_unop(UnOp::Neg, big) { Ok(Some(v)) => v, _ => panic!("neg") };
    assert_eq!(h.foreign_eq(big, again), Some(true));
    assert_eq!(h.foreign_cmp(big, boxed), Some(core::cmp::Ordering::Greater));
    assert_eq!(h.foreign_cmp(boxed, big), Some(core::cmp::Ordering::Less));
    assert_eq!(h.foreign_cmp(low, Val::int(0)), Some(core::cmp::Ordering::Less));
    assert_eq!(h.foreign_cmp(big, Val::float(1.6069380442589903e60)), Some(core::cmp::Ordering::Equal));
    assert_eq!(h.foreign_cmp(big, Val::float(f64::INFINITY)), Some(core::cmp::Ordering::Less));
    assert_eq!(h.foreign_eq(boxed, Val::float(18446744073709551616.0)), Some(true));
    assert_eq!((h.foreign_hash(big), h.foreign_hash(low), h.foreign_hash(boxed)), (Some(131072), Some(-131072i64 as u64), Some(8)));
    assert!(matches!(h.foreign_f64(big), Some(Ok(f)) if f == 1.6069380442589903e60));
    let Some(token) = Cx::kind_token(&h, &WIDE) else { panic!("the kind registered") };
    let Some(held) = h.foreign_payload::<BigInt>(token, big).cloned() else { panic!("limbs") };
    let Snapshot::Serialize { save, load } = WIDE.snapshot else { panic!("saved") };
    let mut out = Vec::new();
    save(&held, &mut out);
    assert_eq!(load(&out).and_then(|p| p.downcast_ref::<BigInt>().cloned()), Some(held));
    let huge = op(&mut h, BinOp::Pow, Val::int(10), Val::int(5000));
    assert!(matches!(h.foreign_repr(huge), Some(Err(e)) if format!("{e}").ends_with("Exceeds the limit (4300 digits) for integer string conversion")));
}

// the native whose name and params the raw parse failures are worded with
struct Int;

impl Site for Int {
    type Lang = Mini;
    const NAME: &'static str = "int";
    const PARAMS: &'static [&'static str] = &["", "base"];
}

/* the raw parse of `text` in `base` with the base checked as int checks it */
fn parse(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(text) = cx.text(a.pos[0]).map(String::from) else { return Ok(edge::value::Val::none().into()) };
    let base = a.pos[1].as_int();
    if base != 0 && !(2..=36).contains(&base) { return Err(fail::<Int>(cx, Fail::IntBase)); }
    match lib::num::int::parse::<Mini, Decimal>(cx, &text, base) {
        Ok(v) => Ok(v.into()),
        Err(Parse::Literal) => Err(fail::<Int>(cx, Fail::IntLiteral { base, v: a.pos[0] })),
        Err(Parse::Fail(e)) => Err(e),
    }
}

const PARSE: &[Native] = &[Native::new("parse", Sig::exact(2), parse)];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, PARSE],
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

// each call's repr or its error as the corpus reads them
const TRY: &str = "fn t(f):\n    try:\n        print(repr(f()))\n    except Exception as e:\n        print(type(e).__name__ + ':', e)\n";

fn tried(calls: &str) -> String {
    let src = String::from(TRY) + calls;
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(&src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

#[test]
fn int_text_parses_in_every_base_with_its_errors() {
    let got = tried(concat!(
        "t(lambda: parse('42', 10))\nt(lambda: parse(' -0x1F ', 0))\nt(lambda: parse('0b101', 0))\nt(lambda: parse('1_000_000', 10))\n",
        "t(lambda: parse('\u{663}\u{664}\u{665}', 10))\nt(lambda: parse('9' * 30, 10))\nt(lambda: parse('f' * 40, 16))\nt(lambda: parse('0_0', 0))\n",
        "t(lambda: parse('--5', 10))\nt(lambda: parse('0x-5', 16))\nt(lambda: parse('010', 0))\nt(lambda: parse('1__0', 10))\nt(lambda: parse('7', 37))\n",
        "t(lambda: parse('0x', 16))\nt(lambda: parse('1' * 4301, 10))\nt(lambda: len(str(parse('f' * 5000, 16))) > 0)\n",
    ));
    let want = concat!(
        "42\n-31\n5\n1000000\n345\n999999999999999999999999999999\n1461501637330902918203684832716283019655932542975\n0\n",
        "ValueError: invalid literal for int() with base 10: '--5'\nValueError: invalid literal for int() with base 16: '0x-5'\n",
        "ValueError: invalid literal for int() with base 0: '010'\nValueError: invalid literal for int() with base 10: '1__0'\n",
        "ValueError: int() base must be >= 2 and <= 36, or 0\nValueError: invalid literal for int() with base 16: '0x'\n",
        "ValueError: Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits\n",
        "ValueError: Exceeds the limit (4300 digits) for integer string conversion\n",
    );
    assert_eq!(got, want);
}

#[test]
fn the_held_folds_answer_as_the_engine_does() {
    let got = tried(concat!(
        "fn g(n):\n    for i in range(n):\n        yield (i * 7) % 5\n",
        "t(lambda: (sum([1, 2, 3]), sum(range(101)), sum(g(10)), sum([0.1] * 10), sum([1, 0.5]), sum([2**62] * 4)))\n",
        "t(lambda: (sum(i * 0.1 for i in range(10)), sum([], true), sum([1, 2], 10), sum([[1], [2]], []), sum(range(2**62, 2**62 + 3))))\n",
        "t(lambda: sum(['a'], ''))\nt(lambda: sum([1, 'a']))\n",
        "t(lambda: (min([3, 1, 2]), max(range(10)), max(g(10)), min('hello'), max(3, 9, 2), max([1, 2.5, 2]), min(x * 1.5 for x in range(4))))\n",
        "t(lambda: (max(['aaa', 'b'], key=len), min(g(6), key=lambda v: -v), max([], default=7), min(range(0), default=nil), max(1, 2, key=lambda v: -v)))\n",
        "t(lambda: max([]))\nt(lambda: max(1, 2, default=0))\nt(lambda: max([1, 'a']))\n",
        "t(lambda: (max([2**100, 3]), min(range(-5, 5)), max(range(10, 0, -3)), max([float('nan'), 1.0])))\n",
    ));
    let want = concat!(
        "(6, 5050, 20, 1.0, 1.5, 18446744073709551616)\n(4.5, True, 13, [1, 2], 13835058055282163715)\n",
        "TypeError: sum() can't sum strings [use ''.join(seq) instead]\nTypeError: unsupported operand type(s) for +: 'int' and 'str'\n",
        "(1, 9, 4, 'e', 9, 2.5, 0.0)\n('aaa', 4, 7, None, 1)\nValueError: max() iterable argument is empty\n",
        "TypeError: Cannot specify a default for max() with multiple positional arguments\n",
        "TypeError: '>' not supported between instances of 'str' and 'int'\n(1267650600228229401496703205376, -5, 10, nan)\n",
    );
    assert_eq!(got, want);
}

#[test]
fn natives_cross_into_wide_ints_and_back() {
    let got = tried(concat!(
        "t(lambda: abs(-140737488355328))\nt(lambda: round(10**30 + 5 * 10**9, -10))\nt(lambda: round(12345678901234567890123, -20))\n",
        "t(lambda: pow(2**100, 2**70, 10**30 + 7))\nt(lambda: (3**150).bit_length())\nt(lambda: int.from_bytes(bytes(range(1, 40)), 'little', signed=true))\n",
        "t(lambda: (-(2**100)).to_bytes(16, 'big', signed=true).hex())\nt(lambda: sum([2**62] * 4))\nt(lambda: max(2**100, 3, -2**100))\n",
        "t(lambda: divmod(-(2**100), 7))\nt(lambda: round(1e300))\n",
    ));
    let want = concat!(
        "140737488355328\n1000000000000000000000000000000\n12300000000000000000000\n588308192633340810703587294064\n238\n",
        "1275966880838951957140048049196824912750462188239772872474131448732708150300526818376846344705\n'fffffff0000000000000000000000000'\n",
        "18446744073709551616\n1267650600228229401496703205376\n(-181092942889747057356671886482, -2)\n",
        "1000000000000000052504760255204420248704468581108159154915854115511802457988908195786371375080447864043704443832883878176942523235360430575644792184786706982848387200926575803737830233794788090059368953234970799945081119038967640880074652742780142494579258788820056842838115669472196386865459400540160\n",
    );
    assert_eq!(got, want);
}

// a runtime of mini's words that sets no limit on the digits of decimal text
struct Unbounded;

impl lib::fmt::render::Style for Unbounded {}

impl edge::lang::Language for Unbounded {
    const VERSION: u32 = 1;
    const POLICY: edge::lang::Policy = edge::lang::Policy { max_int_digits: None, ..<Mini as edge::lang::Language>::POLICY };
    const NATIVES: &'static edge::lang::Natives = &UNBOUNDED_NATIVES;
    const NAMES: &'static edge::lang::Names = &mini::NAMES;
    // its own spelling folds constants under the rules it states
    const FRONT: edge::lang::Front = edge::lang::Front::spelled::<Lifted>();
    const DESCRIPTOR: &'static edge::lang::Descriptor = &mini::DESCRIPTOR;
    const WIDE: Option<&'static edge::value::foreign::Kind> = Some(&UNBOUNDED_WIDE);
    const BUILT: edge::value::foreign::Built = <mini::Mini as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut edge::lang::Cx<'_, '_>, v: edge::value::Val, mode: edge::lang::Mode) -> Result<String, edge::value::VmErr> { lib::fmt::render::hook::<Self>(cx, v, mode) }
}

impl lib::Lang for Unbounded {
    const TREE: &'static lib::lang::Tree = &mini::types::TREE;

    fn class(c: lib::Class) -> &'static str { <Mini as lib::Lang>::class(c) }
}

static UNBOUNDED_WIDE: edge::value::foreign::Kind = lib::num::big::kind::<Unbounded>("int");

const UNBOUNDED: &[Native] = &lib::table![Unbounded;
    "print" => lib::io::print,
    "len" => lib::seq::len,
    "str" => lib::text::text,
];

static UNBOUNDED_NATIVES: Natives = edge::natives! {
    language: Unbounded,
    fns: [UNBOUNDED],
    methods: [],
    types: &[],
};

static UNBOUNDED_RUNTIME: Runtime = Runtime::of::<Unbounded>();

/* mini's spelling over the unbounded natives so the calls fuse to that table */
struct Lifted;

impl edge::lexer::Spelling for Lifted {
    const SYNTAX: edge::lexer::Syntax = mini::MINI;
    const NATIVES: &'static Natives = &UNBOUNDED_NATIVES;
}

// with no digit limit a wide int's text pays for its digits first
#[test]
fn decimal_text_with_no_limit_runs_out_of_budget_before_converting() {
    let (chunk, errors) = edge::code::compile_as::<Lifted>("print(len(str(7 ** 200000)))\n");
    assert!(errors.is_empty());
    let mut vm = VM::with_runtime(&UNBOUNDED_RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    assert!(ran.is_err_and(|e| e.to_string().contains("budget exceeded")), "{}", vm.output_text());
    let (chunk, _) = edge::code::compile_as::<Lifted>("print(len(str(7 ** 2000)))\n");
    let mut vm = VM::with_runtime(&UNBOUNDED_RUNTIME, &chunk, Limits::sandbox());
    assert!(vm.run().is_ok());
    assert_eq!(vm.output_text(), "1691\n");
}

// a key lib boxes and its equal engine number hash alike in one table
#[test]
fn wide_keys_and_engine_numbers_hash_alike_in_one_table() {
    let mut h = heap();
    let mut keys = Vec::new();
    for e in [48, 61, 62, 100, 127, 128, 200, 1000] {
        let k = op(&mut h, BinOp::Pow, Val::int(2), Val::int(e));
        let n = op(&mut h, BinOp::Sub, Val::int(0), k);
        let f = 2f64.powi(e as i32);
        keys.extend([(k, Val::float(f)), (n, Val::float(-f))]);
    }
    let mut table = DictMap::new();
    for (i, &(k, _)) in keys.iter().enumerate() { table.insert(k, Val::int(i as i64), &h); }
    for (i, &(k, f)) in keys.iter().enumerate() {
        assert!(k.is_foreign(), "{} is boxed", repr(&h, k));
        assert_eq!(hash_val_with_heap(k, &h), hash_val_with_heap(f, &h), "{}", repr(&h, k));
        assert_eq!(table.get(&f, &h).map(Val::as_int), Some(i as i64), "{}", repr(&h, k));
    }
    // past the prime a wide key hashes as the engine's own int of its residue
    for (e, plus, residue) in [(61, 5, 6), (122, 0, 1), (130, 1, 257)] {
        let p = op(&mut h, BinOp::Pow, Val::int(2), Val::int(e));
        let k = op(&mut h, BinOp::Add, p, Val::int(plus));
        let n = op(&mut h, BinOp::Sub, Val::int(0), k);
        assert_eq!(hash_val_with_heap(k, &h), hash_val_with_heap(Val::int(residue), &h), "{}", repr(&h, k));
        assert_eq!(hash_val_with_heap(n, &h), hash_val_with_heap(Val::int(-residue), &h), "{}", repr(&h, n));
    }
}
