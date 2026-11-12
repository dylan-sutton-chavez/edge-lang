mod test {
    use edge::embed::{Limits, VM};
    use edge::lang::{Access, CallRule, CallShape, Cmp, Cx, Descriptor, EngineError, Front, GetRule, Key, KindId, Language, Misfit, Mode, Name, Names, Native, Natives, Op, OpRule, Policy, Runtime};
    use edge::lexer::{Spelling, Syntax};
    use edge::value::foreign::{BinOp, Kind, UnOp};
    use edge::value::{HeapPool, Val, VmErr};
    use rig::Rig;
    use rig::natives::{ARRAY, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

    // the rig's natives under a runtime whose rules answer what the engine refuses
    static NATIVES: Natives = edge::natives! {
        language: Binds,
        fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, TYPES],
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

    static RUNTIME: Runtime = Runtime::of::<Binds>();

    // rig reading map keys as fields with objects answering operators through their class
    struct Binds;

    impl Spelling for Binds {
        const SYNTAX: Syntax = rig::RIG;
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
        const POLICY: Policy = Rig::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &rig::NAMES;
        const FRONT: Front = Front::spelled::<Binds>();
        const DESCRIPTOR: &'static Descriptor = &rig::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Rig as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <rig::Rig as Language>::BUILT;
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

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Rig as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Rig as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Rig as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Rig as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Rig as Language>::message(cx, e) }
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
    fn a_get_rule_reads_and_writes_a_maps_keys_as_fields() {
        assert_eq!(ran("d = {x = 1}\nd.y = 2\nprint(d.x, d.y, d)\n"), "1 2 {x = 1, y = 2}");
        assert_eq!(ran("d = {}\nprint(d.nope)\n"), "FieldError: record has no field nope");
    }

    #[test]
    fn a_get_rule_gives_a_constant_and_binds_a_native_method() {
        assert_eq!(ran("x = 5\nprint(x.zero, x.width())\n"), "0 3");
        assert_eq!(ran("x = 5\nprint(x.other)\n"), "FieldError: int has no field other");
    }

    #[test]
    fn an_op_rule_runs_a_native_or_identity_and_a_kernel_rule_refuses() {
        assert_eq!(ran("print(-[1, 2, 3], [1] < \"a\")\n"), "3 false");
    }

    // rig binding capitals naming ints wholes and classing objects as value errors
    struct Hooked;

    impl Spelling for Hooked {
        const SYNTAX: Syntax = rig::RIG;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Hooked {
        const VERSION: u32 = 1;
        const POLICY: Policy = Rig::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &rig::NAMES;
        const FRONT: Front = Front::spelled::<Hooked>();
        const DESCRIPTOR: &'static Descriptor = &rig::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Rig as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <rig::Rig as Language>::BUILT;
        type Ops = ();

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Rig as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Rig as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Rig as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Rig as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Rig as Language>::message(cx, e) }

        fn raise(cx: &mut Cx<'_, '_>, e: EngineError) -> Result<Val, VmErr> {
            let Some(kind) = e.kind() else { return edge::lang::raised::<Self>(cx, e) };
            let words = <Rig as Language>::message(cx, &e).to_uppercase();
            let words = cx.new_str(words)?;
            cx.heap_mut().new_error(rig::NAMES.error(kind), vec![words])
        }

        fn type_name<'h>(cx: &'h Cx<'_, '_>, v: Val) -> &'h str { if v.is_int() { "whole" } else { cx.plain_type(v) } }

        fn error_kind(cx: &Cx<'_, '_>, exc: Val) -> edge::abi::ErrorKind {
            match cx.heap().class_of(exc) {
                Some(c) if cx.heap().is_class(c) => edge::abi::ErrorKind::Value,
                _ => <Rig as Language>::error_kind(cx, exc),
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
        let src = "try {\n    1 / 0\n} catch DivisionError as e {\n    print(e)\n}\ntry {\n    [][2]\n} catch IndexError as e {\n    print(e)\n}\n";
        assert_eq!(hooked(src), "DIVISION BY ZERO\nLIST INDEX OUT OF RANGE");
        assert_eq!(ran(src), "division by zero\nlist index out of range");
        // what the program raises itself stays its own
        assert_eq!(hooked("try {\n    throw ValueError(\"mine\")\n} catch ValueError as e {\n    print(e)\n}\n"), "mine");
    }

    #[test]
    fn the_engine_names_a_type_as_the_runtimes_type_name_does() {
        assert_eq!(hooked("x = 5\nprint(x.nope)\n"), "FieldError: whole has no field nope");
        assert_eq!(hooked("print(1 + \"a\")\n"), "TypeError: + does not apply to whole and text");
        assert_eq!(ran("x = 5\nprint(x.nope)\n"), "FieldError: int has no field nope");
    }

    // how often each bind hook of the counting runtime was asked
    static ASKED: [std::sync::atomic::AtomicUsize; 3] = [const { std::sync::atomic::AtomicUsize::new(0) }; 3];

    // the rules of `Binds` counted as the engine asks them
    struct Counted;

    impl Spelling for Counted {
        const SYNTAX: Syntax = rig::RIG;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Counted {
        const VERSION: u32 = 1;
        const POLICY: Policy = Rig::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &rig::NAMES;
        const FRONT: Front = Front::spelled::<Counted>();
        const DESCRIPTOR: &'static Descriptor = &rig::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Rig as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <rig::Rig as Language>::BUILT;
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

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Rig as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Rig as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Rig as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Rig as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Rig as Language>::message(cx, e) }
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
        let (out, asked) = counted("d = {x = 1}\nt = 0\nfor i in range(300) {\n    d.y = i\n    t = t + d.x + d.y + i.zero\n}\nprint(t)\n");
        assert_eq!(out, "45150");
        assert!(asked[0] <= 12, "{asked:?}");
    }
}
