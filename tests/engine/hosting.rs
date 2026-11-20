mod test {
    use interpreter::lang::{Args, Cx, ErrKind, Native, Natives, Ret, Runtime, Sig};
    use interpreter::value::HeapPool;
    use interpreter::{Diagnostic, Limits, Lines, NativeBinding, Val, VmErr, VM};

    use crate::common::TestResolver;

    const OWN: &[Native] = &[Native::new("tick", Sig::exact(0), tick)];

    static NATIVES: Natives = rig::with!(Kept, OWN);

    static RUNTIME: Runtime = Runtime::of::<Kept>();

    // the rig's language with one native that counts on the data its host kept
    struct Kept;
    rig::over!(Kept, &NATIVES);

    // what a host keeps on one machine for the natives of its runtime
    struct Ticks(i64);

    /* one more tick on the machine's own counter */
    fn tick(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
        let ticks = cx.heap_mut().data_mut::<Ticks>().ok_or(VmErr::new(ErrKind::Runtime, "no ticks kept"))?;
        ticks.0 += 1;
        let n = ticks.0;
        Ok(Val::int(n).into())
    }

    /* the same count a module binding makes through the heap */
    fn counted(heap: &mut HeapPool, _: &[Val], _: Option<Val>) -> Result<Val, VmErr> {
        let ticks = heap.data_mut::<Ticks>().ok_or(VmErr::new(ErrKind::Runtime, "no ticks kept"))?;
        ticks.0 += 10;
        Ok(Val::int(ticks.0))
    }

    #[test]
    fn a_front_compiles_and_folds_in_one_call() {
        let src = "x = 2 * 3\nprint(x)\n";
        let chunk = rig::FRONT.compile(src).expect("compiles");
        let (by_hand, errors) = interpreter::compile(&rig::FRONT, src);
        assert!(errors.is_empty());
        assert!(crate::common::differ(&chunk, &by_hand, "", &|n| n.to_string()).is_none(), "the one call folds as compile does");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox());
        vm.run().expect("runs");
        assert_eq!(vm.output_text(), "6\n");
        assert!(rig::FRONT.compile("x = (\n").is_err_and(|errors| !errors.is_empty()), "an open paren is an error");
    }

    #[test]
    fn a_diagnostic_names_its_line_and_column() {
        let src = "x = 1\ny = 2\nprint(x +)\n";
        let Err(errors) = rig::FRONT.compile(src) else { panic!("a missing operand is an error") };
        let (line, col) = errors[0].line_col(src);
        assert_eq!(line, 3);
        let rendered = errors[0].render(src, Some("main.rig"));
        assert!(rendered.contains(&format!("main.rig:{line}:{col}")), "{rendered}");
        let lines = Lines::new("ab\ncd\n");
        assert_eq!((lines.line_col(0), lines.line_col(3), lines.line_col(4), lines.line_col(99)), ((1, 1), (2, 1), (2, 2), (3, 1)));
        assert_eq!(Diagnostic::new(4, 5, "here").line_col("ab\ncd\n"), (2, 2));
    }

    #[test]
    fn a_run_error_names_its_line_and_column() {
        let src = "x = 1\nfn f() {\n    return 1 / 0\n}\nf()\n";
        let chunk = rig::FRONT.compile(src).expect("compiles");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox());
        assert!(vm.error_line_col().is_none());
        vm.run().expect_err("divides by zero");
        let (line, _) = vm.error_line_col().expect("a position");
        assert_eq!(line, 3);
        assert!(vm.error_path().is_none(), "the error rose in the entry program");
    }

    #[test]
    fn a_native_and_a_binding_reach_the_data_their_machine_keeps() {
        let src = "use m\nprint(tick(), tick(), m.counted(), tick())\n";
        let resolver = TestResolver::new().with_native("m", vec![NativeBinding::from_fn("counted", counted)]).with_alias("m", "m");
        let chunk = rig::FRONT.compile_with(src, Box::new(resolver)).expect("compiles");
        let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        vm.heap_mut().set_data(Ticks(0));
        vm.run().expect("runs");
        assert_eq!(vm.output_text(), "1 2 12 13\n");
        assert_eq!(vm.heap().data::<Ticks>().map(|t| t.0), Some(13));
        assert!(vm.heap().data::<u8>().is_none(), "another type finds nothing");
        assert_eq!(vm.heap_mut().take_data::<Ticks>().map(|t| t.0), Some(13));
        assert!(vm.heap().data::<Ticks>().is_none());
        // a machine with nothing kept raises in the native
        let mut bare = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        assert!(bare.run().is_err());
    }

    #[test]
    fn a_name_of_an_error_kind_reads_by_its_kind() {
        const TYPE: &str = rig::NAMES.error(ErrKind::Type);
        assert_eq!(TYPE, "TypeError");
        assert_eq!(interpreter::lang::Names::PLAIN.error(ErrKind::ZeroDiv), "ZeroDiv");
    }

    #[test]
    fn an_error_is_built_and_read_by_its_kind_and_never_by_a_variant() {
        let e = VmErr::new(ErrKind::Value, "bad input");
        assert_eq!(e.kind(), Some(ErrKind::Value));
        assert!(!e.is_limit() && e.paused().is_none() && !e.is_deferred());
        assert!(e.to_string().ends_with("bad input"), "{e}");
        assert_eq!(VmErr::new(ErrKind::Type, "no").kind(), Some(ErrKind::Type));
        assert_eq!(VmErr::raised("Oops: no").kind(), None, "a raised class is the program's and no engine kind");
        assert!(VmErr::deferred().is_deferred());
        // a run past its budget ends on a limit and a run waiting on an event pauses
        let chunk = rig::FRONT.compile("for i in range(100000) {\n    x = i\n}\n").expect("compiles");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox().ops(1000));
        assert!(vm.run().is_err_and(|e| e.is_limit() && e.paused().is_none()));
        let chunk = rig::FRONT.compile("x = receive()\n").expect("compiles");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox());
        assert!(vm.run().is_err_and(|e| e.paused() == Some(interpreter::SchedulerStatus::PendingEvent) && !e.is_limit()));
    }

    #[test]
    fn a_host_reads_and_builds_heap_values_through_the_heap() {
        let chunk = rig::FRONT.compile("x = 1\n").expect("compiles");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox());
        let heap = vm.heap_mut();
        let s = heap.new_str(String::from("hi")).expect("a str");
        let l = heap.new_list(vec![Val::int(1), s]).expect("a list");
        let t = heap.new_tuple(vec![Val::none()]).expect("a tuple");
        assert_eq!(heap.text(s), Some("hi"));
        assert_eq!((heap.text(l), heap.text(Val::int(3))), (None, None));
        assert_eq!(heap.items(l), Some(vec![Val::int(1), s]));
        assert_eq!(heap.items(t), Some(vec![Val::none()]));
        assert_eq!(heap.items(s), None, "a str is no list or tuple");
        // a heap value reads back the index it was built from as a foreign one reads its parts
        assert!(s.is_heap() && Val::heap(s.as_heap()) == s);
        let f = Val::foreign(3, 9);
        assert!(f.is_foreign() && !f.is_heap() && f.as_foreign() == (3, 9) && !s.is_foreign());
    }

    #[test]
    fn a_resolved_module_is_built_by_its_constructors() {
        let resolver = TestResolver::new().with_code("m", "x = 2\n").with_alias("m", "m").with_native("h", vec![NativeBinding::from_fn("n", |_, _, _| Ok(Val::int(40)))]).with_alias("h", "h");
        let chunk = rig::FRONT.compile_with("use m\nuse h\nprint(m.x + h.n())\n", Box::new(resolver)).expect("compiles");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox());
        vm.run().expect("runs");
        assert_eq!(vm.output_text(), "42\n");
        assert!(matches!(interpreter::Resolved::code("x = 1\n", "m").consts(Vec::new()), interpreter::Resolved::Code { .. }), "a code module takes no constants");
    }

    #[test]
    fn a_snapshot_error_shows_its_words() {
        let chunk = rig::FRONT.compile("x = 1\n").expect("compiles");
        let mut vm = VM::with_runtime(&rig::RUNTIME, &chunk, Limits::sandbox());
        let Err(e) = interpreter::snapshot::restore(&mut vm, b"not a blob") else { panic!("a short blob is refused") };
        assert!(e.to_string().contains("snapshot"), "{e}");
        assert!(interpreter::snapshot::entry_of(b"").is_err() && interpreter::snapshot::limits_of(b"").is_err());
    }
}
