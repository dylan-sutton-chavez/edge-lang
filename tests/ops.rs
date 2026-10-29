mod test {
    use edge::code::OpCode;
    use edge::embed::{Limits, VM};
    use edge::lang::{Args, Cx, Descriptor, EngineError, ErrKind, Front, Language, Misfit, Mode, Names, Native, Natives, Op, OpSet, OpTable, Policy, Ret, Runtime, Sig};
    use edge::lexer::{Spelling, Syntax};
    use edge::value::foreign::Kind;
    use edge::value::{HeapPool, Val, VmErr};
    use mini::Mini;
    use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

    // clamp of three ints answered in the loop and other numbers by its slow path
    edge::ops! {
        table Clamps: Debug;
        own {
        //  name   pops  fuse     fast        slow        fold
            Clamp  3     "clamp"  clamp_fast  clamp_slow  clamp_fold;
        }
    }

    /* three inline ints clamped or none for the slow path */
    fn ints(args: &[Val]) -> Option<Val> {
        let &[x, lo, hi] = args else { return None };
        (x.is_int() && lo.is_int() && hi.is_int() && lo.as_int() <= hi.as_int()).then(|| Val::int(x.as_int().clamp(lo.as_int(), hi.as_int())))
    }

    /* the loop's path which reads no heap */
    fn clamp_fast(_heap: &mut HeapPool, args: &[Val]) -> Option<Val> { ints(args) }

    /* constant ints clamped as the program compiles */
    fn clamp_fold(args: &[Val]) -> Option<Val> { ints(args) }

    // how often this thread ran the slow path which the fast path should spare
    std::thread_local! { static SLOW: core::cell::Cell<usize> = const { core::cell::Cell::new(0) }; }

    /* any ints or floats clamped and other kinds refused through the runtime's rules */
    fn clamp_slow(cx: &mut Cx<'_, '_>, args: &[Val]) -> Result<Val, VmErr> {
        SLOW.with(|n| n.set(n.get() + 1));
        let &[x, lo, hi] = args else { return Err(VmErr::Runtime("clamp takes three operands")) };
        let bounds = || VmErr::Of(ErrKind::Value, String::from("clamp's low bound lies past its high bound"));
        if let (Some(a), Some(l), Some(h)) = (cx.int(x), cx.int(lo), cx.int(hi)) {
            if l > h { return Err(bounds()); }
            return cx.new_int(a.clamp(l, h));
        }
        let all = [x, lo, hi];
        if let Some(k) = all.iter().position(|&v| cx.float(v).is_none()) { return Err(EngineError::Unsupported { op: Op::Runtime(Clamps::Clamp as u8), a: all[k], b: Some(all[usize::from(k == 0)]) }.into()); }
        let num = |v: Val| cx.float(v).unwrap_or(Ok(f64::NAN));
        let (a, l, h) = (num(x)?, num(lo)?, num(hi)?);
        // a bound that is no number orders past nothing
        if l.is_nan() || h.is_nan() || l > h { return Err(bounds()); }
        Ok(Val::float(a.clamp(l, h)))
    }

    /* the native whose plain calls of three arguments run the op */
    fn clamp_native(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { clamp_slow(cx, a.pos).map(Into::into) }

    const CLAMP: &[Native] = &[Native::new("clamp", Sig::exact(3), clamp_native)];

    // mini's natives with the clamp the op fuses
    static NATIVES: Natives = edge::natives! {
        language: Clamped,
        fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, CLAMP],
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

    static RUNTIME: Runtime = Runtime::of::<Clamped>();

    // mini with an opcode of its own
    struct Clamped;

    impl Spelling for Clamped {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
        const OPS: &'static OpSet = <Clamps as OpTable>::SET;
    }

    impl Language for Clamped {
        const VERSION: u32 = 1;
        const POLICY: Policy = Mini::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Clamped>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = Clamps;

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
    }

    /* what `src` prints under the runtime or the error that ends it */
    fn ran(src: &str) -> String {
        let (chunk, errors) = edge::code::compile_as::<Clamped>(src);
        assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => vm.output_text().trim_end().to_string(),
            Err(e) => vm.error_text(&e),
        }
    }

    /* the ops `src` compiles to in its module body */
    fn ops_of(src: &str) -> Vec<(OpCode, u16)> {
        let (chunk, errors) = edge::code::compile_as::<Clamped>(src);
        assert!(errors.is_empty());
        chunk.instructions.iter().map(|i| (i.opcode, i.operand)).collect()
    }

    #[test]
    fn a_runtime_declares_its_ops_and_the_engine_reads_the_table() {
        assert_eq!(Clamps::ALL, [Clamps::Clamp]);
        assert_eq!((<Clamps as OpTable>::COUNT, Clamps::Clamp.operand()), (1, 3 << 8));
        assert_eq!(RUNTIME.ops.ops[0].fuse, Some("clamp"));
        assert_ne!(<Clamps as OpTable>::HASH, 0);
        // a table of no ops is empty and the identity tells the runtimes apart
        assert_ne!(RUNTIME.identity, mini::RUNTIME.identity);
        assert_eq!(<() as OpTable>::SET.ops.len(), 0);
    }

    #[test]
    fn a_plain_call_of_the_fused_native_compiles_to_the_op() {
        let ops = ops_of("x = 5\ny = clamp(x, 0, 3)\n");
        assert!(ops.contains(&(OpCode::RuntimeOp, Clamps::Clamp.operand())), "{ops:?}");
        // another count of arguments or a keyword calls the native
        for src in ["x = 5\ny = clamp(x, 0)\n", "x = 5\ny = clamp(x, 0, hi=3)\n"] {
            assert!(!ops_of(src).iter().any(|o| o.0 == OpCode::RuntimeOp), "{src:?}");
        }
        // a name the program binds is its own function and no op
        assert!(!ops_of("fn clamp(a, b, c):\n    return a\ny = clamp(1, 2, 3)\n").iter().any(|o| o.0 == OpCode::RuntimeOp));
    }

    #[test]
    fn constant_operands_fold_as_the_table_says() {
        let ops = ops_of("y = clamp(9, 0, 3)\n");
        assert!(!ops.iter().any(|o| o.0 == OpCode::RuntimeOp), "{ops:?}");
        assert_eq!(ran("print(clamp(9, 0, 3), clamp(-4, 0, 3), clamp(1, 0, 3))\n"), "3 0 1");
        // what the rule refuses stays the op and runs
        assert!(ops_of("y = clamp(9, 3, 0)\n").iter().any(|o| o.0 == OpCode::RuntimeOp));
        assert_eq!(ran("y = clamp(9, 3, 0)\n"), "ValueError: clamp's low bound lies past its high bound");
    }

    #[test]
    fn the_loop_runs_the_op_over_rows_and_lowered_registers() {
        assert_eq!(ran("x = 5\nprint(clamp(x, 0, 3), clamp(x - 9, 0, 3), clamp(x, 0, 9))\n"), "3 0 5");
        // a hot loop and a function body lower the op to its register form
        let src = "fn f(n):\n    t = 0\n    for i in range(n):\n        t = t + clamp(i, 10, 20)\n    return t\nprint(f(300), f(300))\nt = 0\nfor i in range(300):\n    t = t + clamp(i, 10, 20)\nprint(t)\n";
        assert_eq!(ran(src), "5845 5845\n5845");
    }

    #[test]
    fn the_slow_path_takes_what_the_fast_path_declines() {
        assert_eq!(ran("x = 2.5\nprint(clamp(x, 0, 1), clamp(x, 0.5, 9), clamp(true, 0, 9))\n"), "1.0 2.5 1");
        let wide = "x = 10 ** 30\nprint(clamp(x, 0, 7), clamp(-x, 0, 7))\n";
        assert_eq!(ran(wide), "7 0");
        assert_eq!(ran("x = 'a'\nclamp(x, 0, 1)\n"), "TypeError: unsupported operand type(s) for clamp: 'str' and 'int'");
        assert_eq!(ran("fn f(x):\n    return clamp(x, 0, 1)\nprint(f(3), f(2.5))\n"), "1 1.0");
        assert_eq!(ran("fn f(x):\n    return clamp(x, 0, 1)\nfor v in [3, 2.5, 'b']:\n    f(v)\n"), "TypeError: unsupported operand type(s) for clamp: 'str' and 'int'");
        // an unbound operand raises as its read would
        assert!(ran("fn f():\n    clamp(y, 0, 1)\n    y = 1\nf()\n").starts_with("UnboundLocalError"));
    }

    #[test]
    fn a_rebound_name_runs_its_binding() {
        let src = "fn swap():\n    global clamp\n    clamp = lambda a, b, c: a + b + c\nx = 5\nprint(clamp(x, 0, 3))\nswap()\nprint(clamp(x, 0, 3))\n";
        assert_eq!(ran(src), "3\n8");
        let hot = "fn f(n):\n    t = 0\n    for i in range(n):\n        t = t + clamp(i, 0, 1)\n    return t\nprint(f(100))\nfn g(a, b, c):\n    return 2\nfn swap():\n    global clamp\n    clamp = g\nswap()\nprint(f(100))\n";
        assert_eq!(ran(hot), "99\n200");
    }

    #[test]
    fn another_rebound_name_leaves_the_op_on_its_fast_path() {
        let src = "fn sum(xs):\n    return 0\nfn f(n):\n    t = 0\n    for i in range(n):\n        t = t + clamp(i, 10, 20)\n    return t\nprint(f(300), f(300))\nt = 0\nfor i in range(300):\n    t = t + clamp(i, 10, 20)\nprint(t, sum([1]))\n";
        let before = SLOW.with(|n| n.get());
        assert_eq!(ran(src), "5845 5845\n5845 0");
        // every operand was an int in order so no op left its fast path
        assert_eq!(SLOW.with(|n| n.get()), before);
    }

    #[test]
    fn a_module_binding_of_the_fused_name_is_called_as_a_function() {
        let own = "fn clamp(x, lo, hi):\n    return x + lo + hi\nfn f(n):\n    t = 0\n    for i in range(n):\n        t = t + clamp(i, 1, 2)\n    return t\nprint(f(100), f(100))\n";
        assert_eq!(ran(own), "5250 5250");
        // a new binding is called and once deleted the op's own rule answers again
        let swapped = "fn clamp(x, lo, hi):\n    return 1\nfn f(n):\n    t = 0\n    for i in range(n):\n        t = t + clamp(i, 0, 5)\n    return t\nprint(f(100))\nfn swap():\n    global clamp\n    clamp = lambda x, lo, hi: 2\nswap()\nprint(f(100))\ndel clamp\nprint(f(100))\n";
        assert_eq!(ran(swapped), "100\n200\n485");
        // its own function raising inside the loop raises as that function
        let raised = "fn clamp(x, lo, hi):\n    if x == 70:\n        raise ValueError('mine')\n    return x\nfn f(n):\n    t = 0\n    for i in range(n):\n        t = t + clamp(i, 0, 5)\n    return t\nf(100)\n";
        assert_eq!(ran(raised), "ValueError: mine");
    }

    #[test]
    fn a_helper_holding_the_op_runs_it_in_hot_loops() {
        let plain = "fn bar(x):\n    return clamp(x, 10, 20)\nfn f(n):\n    t = 0\n    for i in range(n):\n        t = t + bar(i)\n    return t\nprint(f(300), f(300))\n";
        assert_eq!(ran(plain), "5845 5845");
        // operands its fast path declines take the slow path from inside the helper
        let slow = "fn bar(x):\n    return clamp(x, 0, 7)\nt = 0\nfor k in range(100):\n    t = t + bar(10 ** 30) + bar(2.5) + bar(true)\nprint(t)\n";
        assert_eq!(ran(slow), "1050.0");
        let wrong = "fn bar(x):\n    return clamp(x, 0, 1)\nt = 0\nfor v in [3, 2.5, -1] * 30 + ['b']:\n    t = t + bar(v)\n";
        assert_eq!(ran(wrong), "TypeError: unsupported operand type(s) for clamp: 'str' and 'int'");
        let bounds = "fn bar(x, lo):\n    return clamp(x, lo, 4)\nt = 0\nfor i in range(100):\n    t = t + bar(i, i ~/ 50 * 9)\n";
        assert_eq!(ran(bounds), "ValueError: clamp's low bound lies past its high bound");
        // a recursion and a generator holding the op keep their values across their levels
        let walk = "fn walk(n):\n    if n == 0:\n        return 0\n    return clamp(n, 0, 5) + walk(n - 1)\nt = 0\nfor k in range(100):\n    t = t + walk(10)\nprint(t)\n";
        assert_eq!(ran(walk), "4000");
        let stepped = "fn clamped(xs):\n    for x in xs:\n        yield clamp(x, 0, 9)\nt = 0\nfor k in range(50):\n    for v in clamped([-3, 4, 12]):\n        t = t + v\nprint(t)\n";
        assert_eq!(ran(stepped), "650");
    }

    // the same runtime with no opcodes of its own
    struct Unclamped;

    impl Spelling for Unclamped {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Unclamped {
        const VERSION: u32 = 1;
        const POLICY: Policy = Mini::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Unclamped>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();
    }

    static UNCLAMPED: Runtime = Runtime::of::<Unclamped>();

    #[test]
    fn a_blob_of_another_table_is_refused_before_its_source_is_read() {
        let src = "x = 5\ny = clamp(x, 0, 3)\nreceive()\nprint(y)\n";
        let (chunk, _) = edge::code::compile_as::<Clamped>(src);
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(_))));
        let blob = edge::embed::save(&vm, src, "").expect("saves");
        // the runtimes differ only by their tables so the identity alone refuses it
        assert_ne!(RUNTIME.identity, UNCLAMPED.identity);
        assert!(edge::embed::saved_under(&blob, &RUNTIME) && !edge::embed::saved_under(&blob, &UNCLAMPED));
        let (plain, _) = edge::code::compile_as::<Unclamped>(src);
        let mut other = VM::with_runtime(&UNCLAMPED, &plain, Limits::sandbox());
        assert_eq!(edge::embed::restore(&mut other, &blob).err().as_deref(), Some(edge::embed::OTHER_RUNTIME));
        let mut same = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        assert!(edge::embed::restore(&mut same, &blob).is_ok());
    }

    #[test]
    fn a_constant_call_runs_a_binding_the_program_makes() {
        let before = "fn swap():\n    global clamp\n    clamp = lambda a, b, c: 1000\nswap()\nx = 1\nprint(clamp(x, 2, 3), clamp(1, 2, 3))\n";
        assert_eq!(ran(before), "1000 1000");
        // a store written past the call still keeps the call from folding
        let after = "fn f():\n    return clamp(1, 2, 3)\nfn swap():\n    global clamp\n    clamp = lambda a, b, c: 1000\nprint(f())\nswap()\nprint(f())\n";
        assert_eq!(ran(after), "2\n1000");
        assert!(ops_of("y = clamp(9, 0, 3)\nfn g():\n    global clamp\n    clamp = 1\n").iter().any(|o| o.0 == OpCode::RuntimeOp));
    }

    /* what `src` prints driven with `events` pushed one at each event wait */
    fn ran_with(src: &str, events: &[&str]) -> String {
        let (chunk, errors) = edge::code::compile_as::<Clamped>(src);
        assert!(errors.is_empty());
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        let mut events = events.iter();
        loop {
            match vm.run() {
                Ok(_) => return vm.output_text().trim_end().to_string(),
                Err(VmErr::HostYield(_)) => match events.next() { Some(e) => vm.push_event(e).expect("push_event"), None => return String::from("parked") },
                Err(e) => return format!("{}{}", vm.output_text(), vm.error_text(&e)),
            }
        }
    }

    #[test]
    fn a_binding_the_op_calls_cannot_wait() {
        let src = "fn bar(x):\n    return clamp(x, 0, 5)\nfn f(n):\n    t = 0\n    for i in range(n):\n        t = t + bar(i)\n    return t\nprint(f(200))\nfn clamp(x, lo, hi):\n    if x == 150:\n        print('wait', receive())\n    return x * 2\nprint(f(200))\n";
        assert_eq!(ran_with(src, &["go"]), "985\nRuntimeError: receive() and host calls cannot wait inside a call an op makes");
        // the same wait inside the binding of a native's name parks and resumes
        let native = src.replace("clamp", "min").replace("min(x, 0, 5)", "min(x, 5)").replace("fn min(x, lo, hi)", "fn min(x, hi)");
        assert_eq!(ran_with(&native, &["go"]), "985\nwait go\n39800");
    }
}
