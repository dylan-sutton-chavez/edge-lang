mod test {
    use edge::embed::{Limits, VM};
    use edge::lang::{Cx, Descriptor, EngineError, Falsy, Front, IntDiv, IntMod, Language, Misfit, Mode, NegPow, Names, Native, Natives, Policy, Runtime, ZeroDiv};
    use edge::lexer::{Spelling, Syntax};
    use edge::value::foreign::Kind;
    use edge::value::{HeapPool, Val, VmErr};
    use mini::Mini;
    use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

    // mini's natives under a runtime that states other rules for the loop to fold
    static NATIVES: Natives = edge::natives! {
        language: Rules,
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

    static RUNTIME: Runtime = Runtime::of::<Rules>();

    // mini's names and words with truncated division and only nil and false false
    struct Rules;

    impl Spelling for Rules {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Rules {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy {
            int_div: IntDiv::Trunc,
            int_mod: IntMod::Truncated,
            zero_div: ZeroDiv::Ieee,
            neg_pow: NegPow::Error,
            falsy: Falsy::NIL.or(Falsy::FALSE),
            ..Mini::POLICY
        };
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Rules>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
    }

    /* what `src` prints under the runtime or the error that ends it */
    fn ran(src: &str) -> String {
        let (chunk, errors) = edge::code::compile_as::<Rules>(src);
        assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => vm.output_text().trim_end().to_string(),
            Err(e) => vm.error_text(&e),
        }
    }

    #[test]
    fn a_runtime_that_states_other_rules_runs_a_loop_of_its_own() {
        assert!(!Rules::POLICY.loops_as(&Policy::ENGINE) && !Floors::POLICY.loops_as(&Policy::ENGINE));
        assert!(Mini::POLICY.loops_as(&Policy::ENGINE));
    }

    #[test]
    fn floor_division_and_remainder_truncate_in_constants_frames_and_hot_loops() {
        assert_eq!(ran("print(-7 ~/ 2, -7 % 2, 7 ~/ -2, 7 % -2)\n"), "-3 -1 -3 1");
        assert_eq!(ran("a = -7\nb = 2\nprint(a ~/ b, a % b)\n"), "-3 -1");
        let hot = "fn f(a, b):\n    return a ~/ b, a % b\nlast = nil\nfor i in range(300):\n    last = f(-7 - i % 2, 2)\nprint(last)\n";
        assert_eq!(ran(hot), "(-4, 0)");
        let mixed = "fn f(a, b):\n    return a ~/ b, a % b\nlast = nil\nfor i in range(300):\n    last = f(-9, 4)\nprint(last)\n";
        assert_eq!(ran(mixed), "(-2, -1)");
    }

    // the folder rounds constants by the runtime's rule and leaves no op
    #[test]
    fn the_folder_rounds_constants_by_the_runtime_rule() {
        let (chunk, errors) = edge::code::compile_as::<Rules>("print(-7 ~/ 2, -7 % 2)\n");
        assert!(errors.is_empty());
        let left: Vec<_> = chunk.instructions.iter().map(|i| i.opcode).filter(|&op| matches!(op, edge::code::OpCode::FloorDiv | edge::code::OpCode::Mod)).collect();
        assert!(left.is_empty(), "the folder left {left:?}");
        let folded: Vec<_> = chunk.constants.iter().filter_map(|c| match c { edge::code::Value::Int(i) => Some(*i), _ => None }).collect();
        assert!(folded.contains(&-3) && folded.contains(&-1) && !folded.contains(&-4), "{folded:?}");
        let (engine, _) = edge::code::compile_as::<Mini>("print(-7 ~/ 2, -7 % 2)\n");
        let truncated: Vec<_> = engine.constants.iter().filter_map(|c| match c { edge::code::Value::Int(i) => Some(*i), _ => None }).collect();
        assert!(truncated.contains(&-3) && truncated.contains(&-1) && !truncated.contains(&-4), "{truncated:?}");
        let (floors, _) = edge::code::compile_as::<Floors>("print(-7 ~/ 2, -7 % 2)\n");
        let left: Vec<_> = floors.instructions.iter().map(|i| i.opcode).filter(|&op| matches!(op, edge::code::OpCode::FloorDiv | edge::code::OpCode::Mod)).collect();
        assert!(left.is_empty(), "the folder left {left:?}");
        let floored: Vec<_> = floors.constants.iter().filter_map(|c| match c { edge::code::Value::Int(i) => Some(*i), _ => None }).collect();
        assert!(floored.contains(&-4) && floored.contains(&1), "{floored:?}");
    }

    #[test]
    fn floats_wide_ints_and_lib_take_the_same_remainder_rule() {
        assert_eq!(ran("x = -7.5\nprint(x ~/ 2, x % 2)\n"), "-3.0 -1.5");
        assert_eq!(ran("print(divmod(-7, 2), divmod(7, -2), divmod(-7.5, 2))\n"), "(-3, -1) (-3, 1) (-3.0, -1.5)");
        assert_eq!(ran("x = -(10 ** 20)\nprint(x ~/ 7, x % 7, divmod(x, 7))\n"), "-14285714285714285714 -2 (-14285714285714285714, -2)");
        assert_eq!(ran("print(pow(-3, 3, 5), pow(3, 3, -5), pow(-3, 3, 10 ** 30))\n"), "-2 2 -27");
        // a float quotient a hair under a whole number truncates to that whole
        assert_eq!(ran("print(618437.5595219431 ~/ 47.96571828414838, divmod(-618437.5595219431, 47.96571828414838), -1.0 ~/ 2, 0.0 ~/ -3)\n"), "12893.0 (-12893.0, -15.553684418031565) -0.0 -0.0");
    }

    #[test]
    fn dividing_two_ints_keeps_an_int_and_a_float_divided_by_zero_follows_the_float_standard() {
        assert_eq!(ran("a = 7\nb = 2\nprint(a / b, -a / b, 6 / 3)\n"), "3 -3 2");
        assert_eq!(ran("x = 1.0\nz = 0.0\nprint(x / z, -x / z, z / z)\n"), "inf -inf nan");
        assert_eq!(ran("a = 1\nb = 0\nprint(a / b)\n"), "ZeroDivisionError: division by zero");
    }

    #[test]
    fn an_int_to_a_negative_power_raises() {
        assert_eq!(ran("a = 2\nb = -1\nprint(a ** b)\n"), "ValueError: an int to a negative power");
        assert_eq!(ran("print(2.0 ** -1)\n"), "0.5");
    }

    #[test]
    fn only_nil_and_false_read_as_false() {
        assert_eq!(ran("print(!0, !'', ![], !nil, !false)\n"), "False False False True True");
        assert_eq!(ran("z = 0\nif z:\n    print('zero holds')\nif '':\n    print('empty text holds')\nif nil:\n    print('nil holds')\n"), "zero holds\nempty text holds");
        assert_eq!(ran("print(bool(0), bool([]), any([0, '']), [x for x in [0, 1, nil] if x])\n"), "True True True [0, 1]");
        let hot = "fn f(x):\n    if x:\n        return 1\n    return 2\nt = 0\nfor i in range(300):\n    t += f(i % 2)\nprint(t)\n";
        assert_eq!(ran(hot), "300");
        // lib's walks of lists tuples bytes and ranges read a zero as the runtime does
        assert_eq!(ran("print(any([0]), all([0, 0]), any((0,)), any(b'\\x00'), all(bytearray(2)), any(range(1)), all(range(3)), any(x for x in [0]))\n"), "True True True True True True True True");
    }

    // the engine's own folds over a range read a zero as the runtime does
    #[test]
    fn a_fold_over_a_range_reads_zero_as_the_runtime_does() {
        assert_eq!(ran("print(all(iter(range(0, 3))), any(iter(range(0, 1))))\nf = all\ng = any\nprint(f(range(0, 3)), g(range(0, 1)), all(range(0, 3)))\n"), "True True\nTrue True True");
    }

    // mini's natives under stricter rules for bools and negative indexes
    static STRICT_NATIVES: Natives = edge::natives! {
        language: Strict,
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

    static STRICT: Runtime = Runtime::of::<Strict>();

    struct Strict;

    impl Spelling for Strict {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &STRICT_NATIVES;
    }

    impl Language for Strict {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy { bool_is_int: false, neg_index: false, ..Mini::POLICY };
        const NATIVES: &'static Natives = &STRICT_NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Strict>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
    }

    /* what `src` prints under the stricter rules */
    fn strict(src: &str) -> String {
        let (chunk, errors) = edge::code::compile_as::<Strict>(src);
        assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_runtime(&STRICT, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => vm.output_text().trim_end().to_string(),
            Err(e) => vm.error_text(&e),
        }
    }

    // a bool that is no int finds no int method and orders with bools only
    #[test]
    fn a_bool_that_is_no_int_finds_no_int_method_and_orders_only_against_a_bool() {
        assert_eq!(ran("print(true.bit_length(), (5).bit_length())\n"), "1 3");
        assert_eq!(strict("print((5).bit_length())\n"), "3");
        assert_eq!(strict("print(true.bit_length())\n"), "AttributeError: 'bool' object has no attribute 'bit_length'");
        assert_eq!(strict("print(false < true, true < false, max(false, true))\n"), "True False True");
        assert!(strict("print(true < 2)\n").starts_with("TypeError"), "{}", strict("print(true < 2)\n"));
        assert!(strict("print(true + 1)\n").starts_with("TypeError"), "{}", strict("print(true + 1)\n"));
        assert!(strict("print([1, 2][true])\n").starts_with("TypeError"), "{}", strict("print([1, 2][true])\n"));
    }

    // a negative index reads nothing and a negative slice bound clamps to the start
    #[test]
    fn a_negative_index_counts_from_no_end_where_the_policy_says_so() {
        assert_eq!(ran("xs = [1, 2, 3]\nprint(xs[-1], 'abc'[-1], 'añb'[-1], (1, 2)[-2], range(5)[-1], b'ab'[-1], xs[-2:], xs[:-1])\n"), "3 c b 1 4 98 [2, 3] [1, 2]");
        let refused = [
            "print([1, 2, 3][-1])\n", "print('abc'[-1])\n", "print('añb'[-1])\n", "print((1, 2)[-1])\n", "print(range(5)[-1])\n",
            "print(range(10 ** 20)[-1])\n", "print(b'ab'[-1])\n", "xs = [1, 2]\nxs[-1] = 5\n", "xs = [1, 2]\ndel xs[-1]\n", "b = bytearray(b'ab')\nb[-1] = 1\n",
        ];
        for src in refused { assert!(strict(src).starts_with("IndexError"), "{src:?} gave {}", strict(src)); }
        assert_eq!(strict("xs = [1, 2, 3]\nprint(xs[-2:], xs[:-1], xs[::-1], xs[-1::-1], 'abc'[-2:], range(5)[-2:])\n"), "[1, 2, 3] [] [3, 2, 1] [] abc range(0, 5)");
        assert_eq!(strict("xs = [1, 2, 3]\nxs[-5:1] = [9]\nprint(xs)\n"), "[9, 2, 3]");
        // a hot loop reads in place and refuses a negative index all the same
        let hot = "fn at(xs, i):\n    return xs[i]\nt = 0\nfor i in range(300):\n    t += at([1, 2, 3], i % 3)\nprint(t)\n";
        assert_eq!(strict(hot), "600");
        assert!(strict(&format!("{hot}print(at([1, 2, 3], -1))\n")).starts_with("IndexError"));
        assert_eq!(ran(&format!("{hot}print(at([1, 2, 3], -1))\n")), "600\n3");
    }

    // a bool bounds a cut where bools are ints and is refused elsewhere
    #[test]
    fn a_bool_bounds_a_cut_only_where_the_policy_counts_it_an_int() {
        assert_eq!(ran("print([1, 2, 3][true:], 'héllo'[:true], b'ab'[true:], range(5)[true:])\n"), "[2, 3] h b'b' range(1, 5)");
        let refused = [
            "print([1, 2, 3][true:])\n", "print('héllo'[:false])\n", "print('abc'[false::2])\n", "print(range(5)[true:])\n",
            "xs = [1, 2]\nxs[true:] = [5]\n", "b = bytearray(b'ab')\ndel b[false:]\n",
        ];
        for src in refused { assert!(strict(src).starts_with("TypeError"), "{src:?} gave {}", strict(src)); }
    }

    // mini's natives under the remainder and truth rules of the other family
    static FLOORS_NATIVES: Natives = edge::natives! {
        language: Floors,
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

    static FLOORS: Runtime = Runtime::of::<Floors>();

    // mini flooring its divisions and reading every empty value as false
    struct Floors;

    impl Spelling for Floors {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &FLOORS_NATIVES;
    }

    impl Language for Floors {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy { int_mod: IntMod::Floored, falsy: Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::EMPTY), ..Mini::POLICY };
        const NATIVES: &'static Natives = &FLOORS_NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Floors>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = <Mini as Language>::WIDE;
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { <Mini as Language>::show(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { <Mini as Language>::format(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { <Mini as Language>::handles(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { <Mini as Language>::misfit(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { <Mini as Language>::message(cx, e) }
    }

    /* what `src` prints under the flooring rules */
    fn floored(src: &str) -> String {
        let (chunk, errors) = edge::code::compile_as::<Floors>(src);
        assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_runtime(&FLOORS, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => vm.output_text().trim_end().to_string(),
            Err(e) => vm.error_text(&e),
        }
    }

    // a floored quotient rounds down and its remainder takes the divisor's sign
    #[test]
    fn a_runtime_that_floors_gives_the_remainder_the_divisors_sign_everywhere() {
        assert_eq!(floored("print(-7 ~/ 2, -7 % 2, 7 ~/ -2, 7 % -2)\n"), "-4 1 -4 -1");
        assert_eq!(floored("a = -7\nb = 2\nprint(a ~/ b, a % b)\n"), "-4 1");
        let hot = "fn f(a, b):\n    return a ~/ b, a % b\nlast = nil\nfor i in range(300):\n    last = f(-9, 4)\nprint(last)\n";
        assert_eq!(floored(hot), "(-3, 3)");
        assert_eq!(floored("x = -7.5\nprint(x ~/ 2, x % 2, divmod(-7, 2), divmod(7, -2), divmod(-7.5, 2), divmod(-0.0, 1))\n"), "-4.0 0.5 (-4, 1) (-4, -1) (-4.0, 0.5) (-0.0, 0.0)");
        assert_eq!(floored("x = -(10 ** 20)\nprint(x ~/ 7, x % 7, divmod(x, 7))\n"), "-14285714285714285715 5 (-14285714285714285715, 5)");
        assert_eq!(floored("print(-(2 ** 200) ~/ 7, -(2 ** 200) % 7, divmod(-(2 ** 130), 12345))\n"), "-229562577751284325077423156048737514646028999111827547900197 3 (-110257551047691685204819637887976740, 9476)");
        assert_eq!(floored("print(pow(-3, 3, 5), pow(3, 3, -5), pow(-3, 3, 10 ** 30))\n"), "3 -3 999999999999999999999999999973");
        // a wide range hashes by its length whichever rule rounds
        assert_eq!(floored("print(hash(range(5, 2 ** 64, 2 ** 64)) == hash(range(5, 6)), len(range(-(2 ** 64), 0, 3)))\n"), "True 6148914691236517206");
    }

    // every empty text and container reads as false while NaN reads as true
    #[test]
    fn a_runtime_can_read_every_empty_value_as_false() {
        assert_eq!(floored("print(!'', ![], !{}, !set(), !(), !range(0), !b'', !bytearray(), !float('nan'), !0.0)\n"), "True True True True True True True True False True");
        assert_eq!(floored("print(bool([]), [] || 'none', any([[], '']), [x for x in [0, 1, '', 'a', [], [0], nil] if x])\n"), "False none False [1, 'a', [0]]");
        let hot = "fn f(xs):\n    if xs:\n        return 1\n    return 2\nt = 0\nfor i in range(300):\n    t += f([[], [i]][i % 2])\nprint(t)\n";
        assert_eq!(floored(hot), "450");
    }
}
