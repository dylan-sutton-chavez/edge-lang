mod test {
    use interpreter::value::HeapObj;
    use interpreter::{Limits, VM};

    const SRC: &str = "print \"booted\";\nfun greet(name) {\n  print \"hi \" + name;\n  return name + \"!\";\n}\n";

    // an embedder boots a program then calls into it through the public API alone
    #[test]
    fn an_embedder_runs_a_program_calls_into_it_and_reads_what_it_printed() {
        let (chunk, errors) = interpreter::compile(&interpreter_lox::FRONT, SRC);
        assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_runtime(&interpreter_lox::RUNTIME, &chunk, Limits::sandbox());
        vm.run().unwrap();
        let booted = vm.output_text();
        assert_eq!(booted, "booted\n");

        // a heap argument goes in and the result and the call output come back
        let name = vm.heap_mut().alloc(HeapObj::Str("ada".into())).unwrap();
        let result = vm.call_export("greet", &[name]).unwrap();
        assert!(matches!(vm.heap().get(result), HeapObj::Str(_)));
        assert_eq!(vm.display(result), "ada!");
        assert_eq!(&vm.output_text()[booted.len()..], "hi ada\n");

        // a failing call is reported in the runtime's words at the line inside the function
        let err = vm.call_export("greet", &[interpreter::Val::int(7)]).unwrap_err();
        let report = vm.report_error(&err, SRC, None);
        assert_eq!(report, "Operands must be two numbers or two strings.\n[line 3] in script", "{report}");
    }

    // the empty runtime refuses a chunk rig compiled before any of it runs
    #[test]
    fn a_machine_refuses_a_chunk_another_runtime_compiled() {
        let wide = "print(list(range(9223372036854775805, 9223372036854775807, 5)))\n";
        let turns = "fn s(n) {\n    return if n == 0 { 0 } else { s(n - 1) + 9223372036854775000 }\n}\nfor i in range(70) {\n    r = s(i % 5)\n}\nprint(r)\n";
        for src in [wide, turns, "print(len([1, 2]))\n"] {
            let (chunk, errors) = crate::common::compile(src);
            assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
            let mut vm = VM::new(&chunk);
            let e = vm.run().expect_err("the empty runtime runs no chunk rig compiled");
            assert!(e.to_string().ends_with("a chunk compiled for rig cannot run on the empty runtime"), "{e}");
            assert!(vm.output.is_empty());
            assert!(vm.call_export("s", &[]).is_err_and(|e| e.to_string().ends_with("cannot run on the empty runtime")));
            let mut own = crate::common::boot(&chunk, Limits::sandbox());
            own.run().expect("rig runs the chunk it compiled");
            assert!(!own.output.is_empty());
        }
        let (lox, _) = interpreter::compile(&interpreter_lox::FRONT, "print 1;\n");
        let refused = VM::new(&lox).run().expect_err("the empty runtime runs no chunk Lox compiled");
        assert!(refused.to_string().ends_with("a chunk compiled for lox cannot run on the empty runtime"), "{refused}");
    }
}
