mod test {
    use std::any::Any;
    use std::rc::Rc;

    use edge::code::{compile_as, OpCode, SSAChunk};
    use edge::embed::{Limits, VM};
    use edge::lang::{Args, Cx, Descriptor, Front, Intrinsic, Language, Misfit, Mode, Names, Native, NativeId, Natives, Policy, Ret, Runtime, Sig};
    use edge::lexer::{Spelling, Syntax};
    use edge::value::foreign::{Kind, Snapshot, Transfer};
    use edge::value::{HeapPool, IterFrame, Val, VmErr};

    /* `fns` with native `from` renamed `to` and every other one as it was */
    const fn renamed<const N: usize>(fns: &[Native], from: &str, to: &'static str) -> [Native; N] {
        let mut out = [Native::BLANK; N];
        let mut i = 0;
        while i < N {
            out[i] = if same(fns[i].name, from) { fns[i].named(to) } else { fns[i] };
            i += 1;
        }
        out
    }

    const fn same(a: &str, b: &str) -> bool {
        let (a, b) = (a.as_bytes(), b.as_bytes());
        if a.len() != b.len() { return false; }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] { return false; }
            i += 1;
        }
        true
    }

    // a point boxed as a foreign value whose fields its kind reads
    static POINT: Kind = Kind {
        name: "test.point",
        version: 1,
        type_name: "point",
        family: "point",
        immutable: true,
        repr: |_| Ok(String::from("point")),
        truthy: |_, _| true,
        hash: |_, _| 0,
        eq: |_, _, _| false,
        cmp: None,
        binop: None,
        unop: None,
        to_f64: None,
        to_i128: None,
        from_i128: None,
        from_digits: None,
        trace: None,
        get: Some(point_field),
        snapshot: Snapshot::Reject,
        transfer: Transfer::Deny,
        str: Some(|p| Ok(p.downcast_ref::<(i64, i64)>().map_or(String::new(), |&(x, y)| format!("<{x} {y}>")))),
        len: Some(|_, _| Ok(2)),
        item: Some(point_item),
        cut: Some(|_, _, bounds| Ok(bounds[0])),
        iter: Some(point_iter),
        call: Some(point_call),
    };

    // a pair whose loop steps a sequence
    static PAIR: Kind = Kind { name: "test.pair", type_name: "pair", family: "pair", iter: Some(pair_iter), ..Kind::PLAIN };
    // a second kind under the point's name which registering refuses
    static CLASH: Kind = Kind { name: "test.point", type_name: "point", family: "point", ..Kind::PLAIN };

    fn pair_iter(_: &HeapPool, _: Val) -> Option<IterFrame> { Some(IterFrame::Seq { items: Rc::from([Val::int(1), Val::int(2)]), idx: 0 }) }

    fn pair(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
        let token = cx.heap_mut().register_kind(&PAIR)?;
        Ok(cx.heap_mut().foreign_new(token, edge::value::foreign::Store::Words([0; 3]))?.into())
    }

    fn clash(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
        let token = cx.heap_mut().register_kind(&CLASH)?;
        Ok(cx.heap_mut().foreign_new(token, edge::value::foreign::Store::Words([0; 3]))?.into())
    }

    // the coordinates of point `v` or zeros for another value
    fn point_at(heap: &HeapPool, v: Val) -> (i64, i64) {
        Cx::kind_token(heap, &POINT).and_then(|t| heap.foreign_payload::<(i64, i64)>(t, v)).copied().unwrap_or_default()
    }

    // the coordinate an index reads
    fn point_item(heap: &mut HeapPool, v: Val, i: Val) -> Result<Val, VmErr> {
        let (x, y) = point_at(heap, v);
        Ok(Val::int(if i.as_int() == 0 { x } else { y }))
    }

    // a loop over a point steps from its first coordinate to its second
    fn point_iter(heap: &HeapPool, v: Val) -> Option<IterFrame> {
        let (x, y) = point_at(heap, v);
        Some(IterFrame::Range { cur: x, end: y + 1, step: 1 })
    }

    // a call scales the first coordinate by its one positional
    fn point_call(heap: &mut HeapPool, v: Val, args: &[Val]) -> Result<Option<Val>, VmErr> {
        let (x, _) = point_at(heap, v);
        Ok(args.first().map(|a| Val::int(x * a.as_int())))
    }

    fn point_field(heap: &mut HeapPool, v: Val, name: &str) -> Result<Option<Val>, VmErr> {
        let token = heap.register_kind(&POINT)?;
        let Some(&(x, y)) = heap.foreign_payload::<(i64, i64)>(token, v) else { return Ok(None) };
        Ok(match name { "x" => Some(Val::int(x)), "y" => Some(Val::int(y)), _ => None })
    }

    fn point(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
        let token = cx.heap_mut().register_kind(&POINT)?;
        let payload: Rc<dyn Any> = Rc::new((3i64, 4i64));
        Ok(cx.heap_mut().foreign_box(token, payload)?.into())
    }

    /* the squared distance of a point from the origin and from another point when given */
    fn squared(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
        let token = cx.heap_mut().register_kind(&POINT)?;
        let at = |v: Val| cx.heap().foreign_payload::<(i64, i64)>(token, v).copied();
        let ((x, y), (ox, oy)) = (at(a.recv).unwrap_or_default(), a.get(0).and_then(at).unwrap_or_default());
        Ok(Val::int((x - ox) * (x - ox) + (y - oy) * (y - oy)).into())
    }

    /* `types` without `name` so a runtime can leave one of mini's classes unlisted */
    const fn without<const N: usize>(types: &[&'static str], name: &str) -> [&'static str; N] {
        let mut out = [""; N];
        let (mut i, mut k) = (0, 0);
        while i < types.len() {
            if !same(types[i], name) { out[k] = types[i]; k += 1; }
            i += 1;
        }
        out
    }

    const IO: [Native; mini::natives::IO.len()] = renamed(mini::natives::IO, "print", "println");
    const ITER: [Native; mini::natives::ITER.len()] = renamed(mini::natives::ITER, "len", "size");
    const OWN: &[Native] = &[Native::new("point", Sig::exact(0), point), Native::new("pair", Sig::exact(0), pair), Native::new("clash", Sig::exact(0), clash)];
    const POINT_METHODS: &[Native] = &[Native::new("squared", Sig::range(0, 1), squared)];

    // mini's types but the assert class which its names still give
    const TYPES: [&str; mini::types::NAMES.len() - 1] = without(mini::types::NAMES, "AssertionError");

    static NATIVES: Natives = edge::natives! {
        language: Renamed,
        fns: [&IO, &ITER, mini::natives::NEXT, mini::natives::NUM, mini::natives::FOLDS, mini::natives::CONVERT, mini::natives::BYTES, mini::natives::SORT, mini::natives::ATTR, mini::natives::TYPES, OWN],
        methods: [
            Str => [mini::natives::STR],
            List => [mini::natives::LIST, mini::natives::LIST_SORT],
            Dict => [mini::natives::DICT],
        ],
        foreign: [POINT => [POINT_METHODS]],
        types: &TYPES,
    };

    static RUNTIME: Runtime = Runtime::of::<Renamed>();

    // mini's table read by a runtime that names print and len otherwise
    struct Renamed;
    impl Spelling for Renamed {
        const SYNTAX: Syntax = mini::MINI;
        const NATIVES: &'static Natives = &NATIVES;
    }

    // mini's rules with keys read as fields of a map and tighter limits
    impl Language for Renamed {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy { map_fields: true, recursion_limit: 50, max_int_digits: Some(10), ..mini::Mini::POLICY };
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &mini::NAMES;
        const FRONT: Front = Front::spelled::<Renamed>();
        const DESCRIPTOR: &'static Descriptor = &mini::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = Some(&mini::lang::WIDE);
        const BUILT: edge::value::foreign::Built = <mini::Mini as Language>::BUILT;
        type Ops = ();

        fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { lib::fmt::render::hook::<mini::Mini>(cx, v, mode) }

        fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { lib::fmt::spec::hook::<mini::Mini>(cx, v, spec) }

        fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { lib::types::match_exc::<mini::Mini>(heap, exc, class) }

        fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { mini::words::arity(n, owner, m) }

        fn message(cx: &mut Cx<'_, '_>, e: &edge::lang::EngineError) -> String { <mini::Mini as Language>::message(cx, e) }
    }

    fn compiled(src: &str) -> SSAChunk {
        let (chunk, errors) = compile_as::<Renamed>(src);
        assert!(errors.is_empty(), "{src:?} reports {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        chunk
    }

    /* what `src` prints under the renamed runtime or the error it ends with */
    fn ran(src: &str) -> Result<String, String> {
        let chunk = compiled(src);
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        match vm.run() {
            Ok(_) => Ok(vm.output_text()),
            Err(e) => Err(vm.error_text(&e)),
        }
    }

    #[test]
    fn a_renamed_kernel_keeps_its_native_call() {
        let chunk = compiled("println(size([1, 2]))\n");
        let mut called: Vec<&str> = chunk.instructions.iter().filter(|i| i.opcode == OpCode::CallNative)
            .filter_map(|i| edge::code::native_site(i.operand, &chunk.native_sites)).filter_map(|s| NATIVES.get(NativeId(s.id))).map(|n| n.name).collect();
        called.sort_unstable();
        let calls = chunk.instructions.iter().filter(|i| i.opcode == OpCode::CallNative).count();
        assert!(calls == 2 && called == ["println", "size"], "{called:?}");
        assert_eq!(ran("println(size([1, 2]), size('abc'))\n").as_deref(), Ok("2 3\n"));
        // the old names are no builtins of this runtime
        assert_eq!(ran("print(1)\n"), Err(String::from("NameError: name 'print' is not defined")));
        assert!(ran("println(len([1]))\n").is_err_and(|e| e.contains("'len' is not defined")));
    }

    // every engine operation a native stands for has the kernel mini registers for it
    #[test]
    fn each_intrinsic_binds_a_kernel_of_mini() {
        for k in [Intrinsic::Len, Intrinsic::Range, Intrinsic::Next, Intrinsic::IsA] {
            assert!(mini::NATIVES.intrinsic(k).is_some(), "{k:?} has no kernel");
        }
    }

    #[test]
    fn a_rebound_renamed_builtin_runs_its_binding() {
        assert_eq!(ran("fn println(x):\n    size(x)\nprintln([1])\nsize = lambda x: 7\nx = size([1])\n").as_deref(), Ok(""));
    }

    #[test]
    fn map_fields_read_a_key_only_where_no_attribute_is() {
        assert_eq!(ran("d = {'a': 1, 'keys': 2}\nprintln(d.a, d['keys'], d.get('a'), size(list(d.keys())))\n").as_deref(), Ok("1 2 1 2\n"));
        // a message names the length builtin as this runtime does
        assert!(ran("println(size(5))\n").is_err_and(|e| e.contains("has no size()")));
        assert!(ran("d = {'a': 1}\nprintln(d.b)\n").is_err_and(|e| e.contains("has no attribute 'b'")));
    }

    // a foreign kind's methods come from its own table and no other kind finds them
    #[test]
    fn a_foreign_kind_calls_the_methods_its_table_holds() {
        assert_eq!(ran("p = point()\nprintln(p.squared(), p.squared(p), p.x)\n").as_deref(), Ok("25 0 3\n"));
        assert_eq!(ran("p = point()\nf = p.squared\nt = 0\nfor i in range(300):\n    t += f()\nprintln(t)\n").as_deref(), Ok("7500\n"));
        assert!(ran("x = 10 ** 30\nprintln(x.squared())\n").is_err_and(|e| e.contains("has no attribute 'squared'")));
        assert!(ran("p = point()\nprintln(p.squared(1, 2))\n").is_err_and(|e| e.contains("squared")));
    }

    #[test]
    fn a_foreign_value_shows_the_fields_its_kind_reads() {
        assert_eq!(ran("p = point()\nprintln(p.x + p.y)\n").as_deref(), Ok("7\n"));
        assert!(ran("p = point()\nprintln(p.z)\n").is_err_and(|e| e.contains("'point' object has no attribute 'z'")));
    }

    /* a foreign kind answers the length items cuts loops calls and text its hooks give */
    #[test]
    fn a_foreign_kind_answers_the_hooks_it_sets() {
        let src = "p = point()\nprintln(size(p), p[0], p[1], p[1:2], list(p), p(2), str(p), p)\nfor v in p:\n    println(v)\n";
        assert_eq!(ran(src).as_deref(), Ok("2 3 4 1 [3, 4] 6 <3 4> <3 4>\n3\n4\n"));
        // a kind that answers calls makes its values callable as their calls are
        assert_eq!(ran("p = point()\nprintln(p(2), callable(p), callable(2 ** 100))\n").as_deref(), Ok("6 True False\n"));
        // the ellipsis its syntax spells is a value of the kind it names
        assert_eq!(ran("println(callable(...), ..., ... == ...)\n").as_deref(), Ok("False Ellipsis True\n"));
    }

    #[test]
    fn the_runtime_sets_how_deep_calls_go_and_how_long_a_literal_is() {
        let down = |n: u32| ran(&format!("fn down(n):\n    if n == 0:\n        return 0\n    return down(n - 1)\nprintln(down({n}))\n"));
        assert_eq!(down(40).as_deref(), Ok("0\n"));
        assert!(down(60).is_err_and(|e| e.contains("maximum recursion depth exceeded")));
        let (_, errors) = compile_as::<Renamed>("println(123456789012345678901234)\n");
        assert!(errors.iter().any(|d| d.msg.contains("past the limit of 10")), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
    }

    #[test]
    fn a_missed_name_suggests_the_words_the_runtime_spells() {
        let src = "println(tru)\n";
        let chunk = compiled(src);
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        let e = vm.run().expect_err("tru is no name");
        let shown = vm.render_error(&e, src, Some("main.mini"));
        assert!(shown.contains("Did you mean: 'true'?"), "{shown}");
    }

    // a failed assert raises the class its runtime names though no type lists it
    #[test]
    fn a_failed_assert_raises_the_class_the_runtime_names() {
        assert!(ran("println(AssertionError)\n").is_err_and(|e| e.contains("'AssertionError' is not defined")));
        assert_eq!(ran("assert 1 == 2, 'boom'\n"), Err(String::from("AssertionError: boom")));
        assert_eq!(ran("assert 1 == 2\n"), Err(String::from("AssertionError")));
        assert_eq!(ran("AssertionError = 5\nassert true, 'fine'\nprintln('passed')\n").as_deref(), Ok("passed\n"));
    }

    #[test]
    fn a_foreign_sequence_steps_as_a_tuple_and_a_second_kind_of_a_name_is_refused() {
        assert_eq!(ran("println(type(iter(pair())), list(pair()))\n").as_deref(), Ok("<class 'tuple_iterator'> [1, 2]\n"));
        // a second kind of the point's name fails where it registers
        assert!(ran("p = point()\nq = clash()\n").is_err_and(|e| e.contains("a second kind takes a name another kind holds")), "{:?}", ran("p = point()\nq = clash()\n"));
        assert!(ran("q = clash()\np = point()\n").is_err_and(|e| e.contains("a second kind takes a name another kind holds")));
    }

    #[test]
    fn a_save_sweeps_a_refusing_kind_s_value_nothing_holds() {
        let src = "p = point()\nprintln(p.x)\np = nil\nt = 0\nfor i in range(3000):\n    t += i\nprintln(t)\n";
        let chunk = compiled(src);
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        vm.set_preempt_interval(100);
        assert!(matches!(vm.run(), Err(VmErr::HostYield(_))));
        assert!(edge::embed::save(&vm, src, "").is_err_and(|e| e.contains("test.point")));
        let blob = edge::embed::save_swept(&mut vm, src, "").expect("a value nothing holds is swept");
        let mut back = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        edge::embed::restore(&mut back, &blob).expect("restore");
        while matches!(back.run(), Err(VmErr::HostYield(_))) {}
        assert!(back.output_text().ends_with("4498500\n"), "{:?}", back.output_text());
        // a value the run still holds keeps refusing
        let held = "p = point()\nt = 0\nfor i in range(3000):\n    t += i\nprintln(t, p.x)\n";
        let chunk = compiled(held);
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        vm.set_preempt_interval(100);
        assert!(matches!(vm.run(), Err(VmErr::HostYield(_))));
        assert!(edge::embed::save_swept(&mut vm, held, "").is_err_and(|e| e.contains("test.point")));
    }

    #[test]
    fn errors_name_their_classes_in_the_runtime_s_words() {
        let mini = Names::PLAIN;
        assert_eq!(VmErr::ZeroDiv.render(&mini), "ZeroDiv: division by zero");
        // the engine's own words name no source language which a runtime's message replaces
        assert_eq!(VmErr::Name(String::from("x")).render(&mini), "Name: x is not bound");
        assert_eq!(VmErr::CallDepth.render(&mini), "Recursion: calls nested past the recursion limit");
        assert_eq!(VmErr::ZeroDiv.render(&mini::NAMES), "ZeroDivisionError: division by zero");
        assert!(VmErr::Raised(String::from("StopIteration: 3")).is(edge::lang::ErrKind::Stop, &mini::NAMES));
        assert!(!VmErr::Raised(String::from("StopIterationX")).is(edge::lang::ErrKind::Stop, &mini::NAMES));
    }
}
