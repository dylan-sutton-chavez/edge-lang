mod test {
    use edge::embed::{Limits, VM};
    use edge::lang::{Access, CallRule, CallShape, Cmp, Cx, Descriptor, EngineError, Front, GetRule, Key, KindId, Language, Misfit, Mode, Name, Names, Native, Natives, Op, OpRule, Policy, Runtime};
    use edge::lexer::{Spelling, Syntax};
    use edge::value::foreign::{BinOp, Kind, UnOp};
    use edge::value::{HeapPool, Val, VmErr};
    use mini::Mini;
    use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

    // mini's natives under a runtime whose rules answer what the engine refuses
    static NATIVES: Natives = edge::natives! {
        language: Binds,
        fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES],
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

    static RUNTIME: Runtime = Runtime::of::<Binds>();

    // mini reading map keys as fields with objects answering operators through their class
    struct Binds;

    impl Spelling for Binds {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
    }

    /* the member `name` of class `class` */
    fn member(cx: &Cx<'_, '_>, class: Val, name: &str) -> Option<Val> {
        cx.heap().own_member(class, name)
    }

    /* the id of method `name` of kind `kind` */
    fn method(cx: &Cx<'_, '_>, kind: KindId, name: &str) -> Option<edge::lang::NativeId> {
        cx.natives().method(kind, name).map(|n| n.id)
    }

    impl Language for Binds {
        const VERSION: u32 = 1;
        const POLICY: Policy = Mini::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Binds>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn bind_get(cx: &mut Cx<'_, '_>, recv: Key, name: Name, how: Access) -> GetRule {
            let name = cx.text(name.0).unwrap_or_default();
            match (recv, name, how) {
                // a map's keys stand as its fields for reads stores and deletes
                (Key::Kind(KindId::Dict), _, _) => GetRule::Field,
                (Key::Kind(KindId::Int), "zero", Access::Load) => GetRule::Const(Val::int(0)),
                (Key::Kind(KindId::Int), "width", Access::Method) => method(cx, KindId::Int, "bit_length").map_or(GetRule::Fail, GetRule::Native),
                _ => GetRule::Ask,
            }
        }

        fn bind_call(cx: &mut Cx<'_, '_>, callee: Val, _shape: CallShape) -> CallRule {
            let Some(class) = cx.heap().class_of(callee).filter(|&c| cx.heap().is_class(c)) else { return CallRule::Ask };
            // an instance makes another of its class or runs the call its class holds
            if member(cx, class, "made").is_some() { return CallRule::Construct { meta: class, init: member(cx, class, "__init__"), slots: 1 }; }
            member(cx, class, "call").map_or(CallRule::Fail, |func| CallRule::Bound { func, recv: callee })
        }

        fn bind_op(cx: &mut Cx<'_, '_>, op: Op, a: Key, b: Key) -> OpRule {
            match (op, a, b) {
                (Op::Bin(BinOp::Add), Key::Object(class), _) => member(cx, class, "add").map_or(OpRule::Fail, |func| OpRule::Call { func, swap: false, negate: false }),
                (Op::Bin(BinOp::Add), _, Key::Object(class)) => member(cx, class, "add").map_or(OpRule::Fail, |func| OpRule::Call { func, swap: true, negate: false }),
                (Op::Cmp(Cmp::Lt), Key::Object(class), _) => member(cx, class, "less").map_or(OpRule::Fail, |func| OpRule::Call { func, swap: false, negate: false }),
                (Op::Cmp(Cmp::Ge), Key::Object(class), _) => member(cx, class, "less").map_or(OpRule::Fail, |func| OpRule::Call { func, swap: false, negate: true }),
                // minus a list gives its length through the length native
                (Op::Unary(UnOp::Neg), Key::Kind(KindId::List), _) => cx.natives().function("len").map_or(OpRule::Fail, |n| OpRule::Native(n.id)),
                (Op::Cmp(Cmp::Lt), Key::Kind(KindId::List), Key::Kind(KindId::Str)) => OpRule::Identity,
                _ => OpRule::Kernel,
            }
        }

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
    }

    /* what `src` prints under the runtime or the error that ends it */
    fn ran(src: &str) -> String {
        let (chunk, errors) = edge::code::compile_as::<Binds>(src);
        assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => vm.output_text().trim_end().to_string(),
            Err(e) => vm.error_text(&e),
        }
    }

    #[test]
    fn a_get_rule_reads_writes_and_deletes_a_maps_keys_as_fields() {
        assert_eq!(ran("d = {'x': 1}\nd.y = 2\nprint(d.x, d.y, d)\ndel d.x\nprint(d)\n"), "1 2 {'x': 1, 'y': 2}\n{'y': 2}");
        assert_eq!(ran("d = {}\nprint(d.nope)\n"), "AttributeError: 'dict' object has no attribute 'nope'");
    }

    #[test]
    fn a_get_rule_gives_a_constant_and_binds_a_native_method() {
        assert_eq!(ran("x = 5\nprint(x.zero, x.width())\n"), "0 3");
        assert_eq!(ran("x = 5\nprint(x.other)\n"), "AttributeError: 'int' object has no attribute 'other'");
    }

    #[test]
    fn a_call_rule_binds_an_instance_or_constructs_another() {
        let src = "class Greeter:\n    fn __init__(self, word):\n        self.word = word\n    fn call(self, name):\n        return self.word + ' ' + name\ng = Greeter('hi')\nprint(g('ann'))\n";
        assert_eq!(ran(src), "hi ann");
        let src = "class P:\n    made = true\n    fn __init__(self, n):\n        self.n = n\np = P(1)\nq = p(5)\nprint(p.n, q.n, type(q) == P)\n";
        assert_eq!(ran(src), "1 5 True");
        let hot = "class Greeter:\n    fn __init__(self, word):\n        self.word = word\n    fn call(self, name):\n        return self.word + name\ng = Greeter('a')\nout = ''\nfor i in range(300):\n    out = g(out)\nprint(len(out))\n";
        assert_eq!(ran(hot), "300");
        assert_eq!(ran("class Q:\n    pass\nQ()(1)\n"), "TypeError: 'Q' object is not callable");
    }

    #[test]
    fn an_op_rule_calls_the_class_swapped_or_negated() {
        let src = "class V:\n    fn __init__(self, n):\n        self.n = n\n    fn add(self, other):\n        return self.n + other\n    fn less(self, other):\n        return self.n < other.n\nv = V(2)\nw = V(3)\nprint(v + 3, 3 + v, v < w, v >= w, w >= v)\n";
        assert_eq!(ran(src), "5 5 True False True");
        let hot = "class V:\n    fn __init__(self, n):\n        self.n = n\n    fn add(self, other):\n        return self.n + other\nv = V(1)\nt = 0\nfor i in range(300):\n    t = t + (v + i)\nprint(t)\n";
        assert_eq!(ran(hot), "45150");
        let nested = "class V:\n    fn __init__(self, n):\n        self.n = n\n    fn add(self, other):\n        return self.n + other\nfn f(v, k):\n    return v + k\nv = V(1)\nt = 0\nfor i in range(300):\n    t = t + f(v, i)\ntry:\n    v + 'a'\nexcept TypeError as e:\n    print(t, e)\n";
        assert_eq!(ran(nested), "45150 unsupported operand type(s) for +: 'int' and 'str'");
    }

    #[test]
    fn an_op_rule_runs_a_native_or_identity_and_a_kernel_rule_refuses() {
        assert_eq!(ran("print(-[1, 2, 3], [1] < 'a')\n"), "3 False");
        assert_eq!(ran("class V:\n    pass\nprint(V() * 2)\n"), "TypeError: unsupported operand type(s) for *: 'V' and 'int'");
    }

    // mini binding capitals naming ints wholes and classing objects as value errors
    struct Hooked;

    impl Spelling for Hooked {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Hooked {
        const VERSION: u32 = 1;
        const POLICY: Policy = Mini::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Hooked>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }

        fn raise(cx: &mut Cx<'_, '_>, e: EngineError) -> Result<Val, VmErr> {
            let Some(kind) = e.kind() else { return edge::lang::raised::<Self>(cx, e) };
            let words = <Mini as Language>::message(cx, &e).to_uppercase();
            let words = cx.new_str(words)?;
            cx.heap_mut().new_error(mini::NAMES.error(kind), vec![words])
        }

        fn type_name<'h>(cx: &'h Cx<'_, '_>, v: Val) -> &'h str { if v.is_int() { "whole" } else { cx.plain_type(v) } }

        fn error_kind(cx: &Cx<'_, '_>, exc: Val) -> edge::abi::ErrorKind {
            match cx.heap().class_of(exc) {
                Some(c) if cx.heap().is_class(c) => edge::abi::ErrorKind::Value,
                _ => <Mini as Language>::error_kind(cx, exc),
            }
        }
    }

    static HOOKED: Runtime = Runtime::of::<Hooked>();

    /* what `src` prints under the hooked runtime or the error that ends it */
    fn hooked(src: &str) -> String {
        let (chunk, errors) = edge::code::compile_as::<Hooked>(src);
        assert!(errors.is_empty());
        let mut vm = VM::with_runtime(&HOOKED, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => vm.output_text().trim_end().to_string(),
            Err(e) => vm.error_text(&e),
        }
    }

    #[test]
    fn a_handler_binds_what_the_runtimes_raise_makes() {
        let src = "try:\n    1 / 0\nexcept ZeroDivisionError as e:\n    print(e)\ntry:\n    [][2]\nexcept IndexError as e:\n    print(e)\n";
        assert_eq!(hooked(src), "DIVISION BY ZERO\nLIST INDEX OUT OF RANGE");
        assert_eq!(ran(src), "division by zero\nlist index out of range");
        // what the program raises itself stays its own
        assert_eq!(hooked("try:\n    raise ValueError('mine')\nexcept ValueError as e:\n    print(e)\n"), "mine");
    }

    #[test]
    fn the_engine_names_a_type_as_the_runtimes_type_name_does() {
        assert_eq!(hooked("x = 5\nprint(x.nope)\n"), "AttributeError: 'whole' object has no attribute 'nope'");
        assert_eq!(hooked("print(1 + 'a')\n"), "TypeError: unsupported operand type(s) for +: 'whole' and 'str'");
        assert_eq!(ran("x = 5\nprint(x.nope)\n"), "AttributeError: 'int' object has no attribute 'nope'");
    }

    #[test]
    fn a_plugin_reads_the_error_kind_the_runtime_gives() {
        let src = "class Boom(Exception):\n    pass\nraise Boom('x')\n";
        let kind = |rt: &'static Runtime| {
            let (chunk, _) = edge::code::compile_as::<Hooked>(src);
            let mut vm = VM::with_runtime(rt, &chunk, Limits::sandbox());
            let e = vm.run().expect_err("raises");
            vm.error_kind(&e)
        };
        assert_eq!((kind(&HOOKED), kind(&RUNTIME)), (edge::abi::ErrorKind::Value, edge::abi::ErrorKind::Custom));
    }

    // how often each bind hook of the counting runtime was asked
    static ASKED: [std::sync::atomic::AtomicUsize; 3] = [const { std::sync::atomic::AtomicUsize::new(0) }; 3];

    // the rules of `Binds` counted as the engine asks them
    struct Counted;

    impl Spelling for Counted {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Counted {
        const VERSION: u32 = 1;
        const POLICY: Policy = Mini::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Counted>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn bind_get(cx: &mut Cx<'_, '_>, recv: Key, name: Name, how: Access) -> GetRule {
            ASKED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Binds::bind_get(cx, recv, name, how)
        }

        fn bind_call(cx: &mut Cx<'_, '_>, callee: Val, shape: CallShape) -> CallRule {
            ASKED[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Binds::bind_call(cx, callee, shape)
        }

        fn bind_op(cx: &mut Cx<'_, '_>, op: Op, a: Key, b: Key) -> OpRule {
            ASKED[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Binds::bind_op(cx, op, a, b)
        }

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
    }

    static COUNTED: Runtime = Runtime::of::<Counted>();

    /* what `src` prints under the counting runtime with how often it asked each hook */
    fn counted(src: &str) -> (String, [usize; 3]) {
        // the counts are one runtime's so its runs take turns
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        for a in &ASKED { a.store(0, std::sync::atomic::Ordering::Relaxed); }
        let (chunk, errors) = edge::code::compile_as::<Counted>(src);
        assert!(errors.is_empty());
        let mut vm = VM::with_runtime(&COUNTED, &chunk, Limits::sandbox());
        let out = match vm.run() { Ok(_) => vm.output_text().trim_end().to_string(), Err(e) => vm.error_text(&e) };
        (out, ASKED.each_ref().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)))
    }

    #[test]
    fn a_site_keeps_its_rule_and_asks_the_runtime_once() {
        // a field read a store and a constant three hundred times at their sites
        let (out, asked) = counted("d = {'x': 1}\nt = 0\nfor i in range(300):\n    d.y = i\n    t = t + d.x + d.y + i.zero\nprint(t)\n");
        assert_eq!(out, "45150");
        assert!(asked[0] <= 12, "{asked:?}");
        // an operator a class answers and a call an instance's class answers
        let (out, asked) = counted("class V:\n    fn __init__(self, n):\n        self.n = n\n    fn add(self, other):\n        return self.n + other\nv = V(1)\nt = 0\nfor i in range(300):\n    t = t + (v + i)\nprint(t)\n");
        assert_eq!(out, "45150");
        assert!(asked[2] <= 4, "{asked:?}");
        let (out, asked) = counted("class G:\n    fn __init__(self, w):\n        self.w = w\n    fn call(self, s):\n        return self.w + s\ng = G('a')\nfn f(n):\n    out = ''\n    for i in range(n):\n        out = g(out)\n    return len(out)\nprint(f(300))\n");
        assert_eq!(out, "300");
        assert!(asked[1] <= 4, "{asked:?}");
    }

    #[test]
    fn a_kept_rule_goes_once_its_class_changes() {
        let src = "class V:\n    fn __init__(self, n):\n        self.n = n\n    fn add(self, other):\n        return self.n + other\nfn twice(self, other):\n    return 2 * other\nv = V(0)\nt = 0\nfor i in range(300):\n    if i == 150:\n        V.add = twice\n    t = t + (v + 1)\nprint(t)\n";
        let (out, asked) = counted(src);
        assert_eq!(out, "450");
        assert!(asked[2] >= 2, "{asked:?}");
        // one site meeting two keys answers each by its own rule
        assert_eq!(counted("for x in [{'zero': 7}, 5, {'zero': 8}, 6]:\n    print(x.zero)\n").0, "7\n0\n8\n0");
    }
}
