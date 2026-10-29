use edge::code::compile_as;
use edge::embed::{Limits, VM};
use edge::lang::{Act, Args, Body, Callee, Cmp, Cx, Descriptor, EngineError, ErrKind, Fit, Front, Gen, KindId, Language, Misfit, Mode, Names, Native, Natives, Op, Policy, Ret, Runtime, Sig, Use};
use edge::lexer::{Spelling, Syntax};
use edge::value::{HeapPool, Val, VmErr};
use edge::value::foreign::Kind;
use edge::value::foreign::{BinOp, UnOp};
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DATE, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};
use mini::{Mini, NAMES};

// the sandbox's memory limit meters what a run allocates as edge.wasm does
edge::allocator!();

// one native past mini's that words an engine error as mini's language does
const PROBE: &[Native] = &[Native::new("probe", Sig::var(1), probe)];

static NATIVES: Natives = edge::natives! {
    language: Probe,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, DATE, PROBE],
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

static RUNTIME: Runtime = Runtime::of::<Probe>();

/* mini's table over these natives so a program calls the probe as a builtin */
struct Probe;

impl Spelling for Probe {
    const SYNTAX: Syntax = mini::MINI;
    const NATIVES: &'static Natives = &NATIVES;
}

// mini's language over these natives
impl Language for Probe {
    const VERSION: u32 = 1;
    const POLICY: Policy = Mini::POLICY;
    const NATIVES: &'static Natives = &NATIVES;
    const NAMES: &'static Names = &NAMES;
    const FRONT: Front = Front::spelled::<Probe>();
    const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
    const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
    const KINDS: &'static [&'static Kind] = <Mini as Language>::KINDS;
    const BUILT: edge::value::foreign::Built = <mini::Mini as edge::lang::Language>::BUILT;
    type Ops = ();

    fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

    fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

    fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

    fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

    fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
}

/* the engine error the first argument names built over the values after it */
fn error(cx: &Cx<'_, '_>, a: &Args<'_>) -> EngineError {
    use EngineError as E;
    let v = |i: usize| a.pos.get(i).copied().unwrap_or(Val::none());
    let text = |i: usize| String::from(cx.text(v(i)).unwrap_or(""));
    let call = |fit: Fit| E::Arity { callee: Callee::Function { name: String::from("f"), normal: 1, defaults: 0 }, fit };
    match text(0).as_str() {
        "add" => E::Unsupported { op: Op::Bin(BinOp::Add), a: v(1), b: Some(v(2)) },
        "mul" => E::Unsupported { op: Op::Bin(BinOp::Mul), a: v(1), b: Some(v(2)) },
        "floor" => E::Unsupported { op: Op::Bin(BinOp::FloorDiv), a: v(1), b: Some(v(2)) },
        "lt" => E::Unsupported { op: Op::Cmp(Cmp::Lt), a: v(1), b: Some(v(2)) },
        "neg" => E::Unsupported { op: Op::Unary(UnOp::Neg), a: v(1), b: None },
        "in" => E::Unsupported { op: Op::In, a: v(1), b: Some(v(2)) },
        "iterate" => E::Cannot { act: Act::Iterate, recv: Some(v(1)) },
        "call" => E::Cannot { act: Act::Call, recv: Some(v(1)) },
        "subscript" => E::Cannot { act: Act::Subscript, recv: Some(v(1)) },
        "assign" => E::Cannot { act: Act::AssignItem, recv: Some(v(1)) },
        "contain" => E::Cannot { act: Act::Contain, recv: Some(v(1)) },
        "size" => E::Cannot { act: Act::Size, recv: Some(v(1)) },
        "key" => E::Cannot { act: Act::Hash { at: Use::Key, inner: KindId::List }, recv: Some(v(1)) },
        "index" => E::BadIndex { recv: v(1), index: v(2) },
        "range" => E::OutOfRange { recv: v(1), store: false },
        "missing" => E::MissingKey(v(1)),
        "undefined" => E::Undefined(text(1)),
        "unbound" => E::Unbound { name: Some(text(1)), free: false },
        "attribute" => E::NoAttribute { recv: v(1), name: text(2), store: false },
        "zero" => E::ZeroDiv,
        "few" => E::Unpack { want: 3, got: Some(2), more: false, rest: false },
        "many" => E::Unpack { want: 2, got: Some(3), more: true, rest: false },
        "count" => call(Fit::Count(2)),
        "unexpected" => call(Fit::Unexpected { key: String::from("b"), near: None }),
        "absent" => call(Fit::Missing { at: 0, names: vec![String::from("a")], keyword: false }),
        "deep" => E::Recursion,
        "store" => E::NoAttribute { recv: v(1), name: text(2), store: true },
        "rest" => E::Unpack { want: 2, got: Some(1), more: false, rest: true },
        "fresh" => E::Generator(Gen::FreshSend(Body::Generator)),
        "step" => E::ZeroStep { range: false },
        "put" => E::OutOfRange { recv: v(1), store: true },
        "raise" => E::Cannot { act: Act::Raise { throw: false }, recv: Some(v(1)) },
        "spread" => E::Cannot { act: Act::Map { spread: true }, recv: Some(v(1)) },
        _ => E::Internal("no such probe"),
    }
}

/* the class and words of the error `error` builds as an uncaught run prints them */
fn probe(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let e = error(cx, &a);
    let class = NAMES.error(e.kind().unwrap_or(ErrKind::Runtime));
    let words = <Mini as Language>::message(cx, &e);
    Ok(cx.new_str(format!("{class}: {words}"))?.into())
}

/* what `src` printed or the error that ended it */
fn ran(src: &str) -> String {
    let (chunk, errors) = compile_as::<Probe>(src);
    assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    match vm.run() {
        Ok(_) => vm.output_text().trim_end().to_string(),
        Err(e) => vm.error_text(&e),
    }
}

// each program's engine error beside the probe that words it over the same values
const SAME: &[(&str, &str)] = &[
    ("x = 1 + 'a'\n", "print(probe('add', 1, 'a'))\n"),
    ("x = [1] + 'a'\n", "print(probe('add', [1], 'a'))\n"),
    ("x = [1] * 'a'\n", "print(probe('mul', [1], 'a'))\n"),
    ("x = 1 < 'a'\n", "print(probe('lt', 1, 'a'))\n"),
    ("x = -'a'\n", "print(probe('neg', 'a'))\n"),
    ("x = 1 in 'a'\n", "print(probe('in', 1, 'a'))\n"),
    ("for x in 5:\n    pass\n", "print(probe('iterate', 5))\n"),
    ("x = 5\nx()\n", "print(probe('call', 5))\n"),
    ("x = 5\ny = x[0]\n", "print(probe('subscript', 5))\n"),
    ("t = (1,)\nt[0] = 2\n", "print(probe('assign', (1,)))\n"),
    ("x = 1 in 5\n", "print(probe('contain', 5))\n"),
    ("x = len(5)\n", "print(probe('size', 5))\n"),
    ("x = {[1]: 2}\n", "print(probe('key', [1]))\n"),
    ("x = [1]['a']\n", "print(probe('index', [1], 'a'))\n"),
    ("x = [1][5]\n", "print(probe('range', [1]))\n"),
    ("x = {}['k']\n", "print(probe('missing', 'k'))\n"),
    ("print(nope)\n", "print(probe('undefined', 'nope'))\n"),
    ("fn f():\n    x = x + 1\nf()\n", "print(probe('unbound', 'x'))\n"),
    ("x = (1).foo\n", "print(probe('attribute', 1, 'foo'))\n"),
    ("x = 1 / 0\n", "print(probe('zero'))\n"),
    ("a, b, c = [1, 2]\n", "print(probe('few'))\n"),
    ("a, b = [1, 2, 3]\n", "print(probe('many'))\n"),
    ("fn f(a):\n    pass\nf(1, 2)\n", "print(probe('count'))\n"),
    ("fn f(a):\n    pass\nf(b=1)\n", "print(probe('unexpected'))\n"),
    ("fn f(a):\n    pass\nf()\n", "print(probe('absent'))\n"),
    ("fn f(n):\n    return f(n + 1)\nf(0)\n", "print(probe('deep'))\n"),
    ("x = 1\nx.y = 2\n", "print(probe('store', 1, 'y'))\n"),
    ("class A:\n    pass\nx = A.b\n", "class A:\n    pass\nprint(probe('attribute', A, 'b'))\n"),
    ("x = (1,)[5]\n", "print(probe('range', (1,)))\n"),
    ("x = 'a'[5]\n", "print(probe('range', 'a'))\n"),
    ("x = 'a'['b']\n", "print(probe('index', 'a', 'b'))\n"),
    ("x = [1]\nx[5] = 2\n", "print(probe('put', [1]))\n"),
    ("a, *b, c = [1]\n", "print(probe('rest'))\n"),
    ("fn g():\n    yield 1\nx = g()\nx.send(1)\n", "print(probe('fresh'))\n"),
    ("x = [1][::0]\n", "print(probe('step'))\n"),
    ("raise 5\n", "print(probe('raise', 5))\n"),
    ("fn f(**k):\n    pass\nf(**5)\n", "print(probe('spread', 5))\n"),
];

#[test]
fn the_loop_raises_each_engine_error_in_minis_words() {
    for &(src, probe) in SAME {
        let words = ran(probe);
        assert!(!words.starts_with(NAMES.error(ErrKind::Runtime)), "{src:?} probes {words:?}");
        assert_eq!(words, ran(src), "{src:?}");
    }
}

// an operator reads as mini spells it in what the loop raises
#[test]
fn an_operator_reads_as_the_runtime_spells_it() {
    assert_eq!(ran("print(probe('floor', 1, 'a'))\n"), "TypeError: unsupported operand type(s) for ~/: 'int' and 'str'");
    assert_eq!(ran("x = 1 ~/ 'a'\n"), ran("print(probe('floor', 1, 'a'))\n"));
}
