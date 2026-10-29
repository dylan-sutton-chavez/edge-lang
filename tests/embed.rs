mod test {
    use edge::value::HeapObj;
use edge::embed::Limits;

    use crate::common::{lex, parser, TestResolver};

    const SRC: &str = "print('booted')\nfn greet(name):\n    print('hi', name)\n    return [name, len(name)]\n";

    // the site embeds the VM through exactly these calls so none reads as dead
    #[test]
    fn an_embedder_runs_a_program_calls_into_it_and_reads_what_it_printed() {
        let (tokens, _) = lex(SRC);
        let (chunk, errors) = parser(SRC, tokens.into_iter(), Box::new(TestResolver::new())).parse();
        assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = crate::common::boot(&chunk, Limits::sandbox());
        vm.bind_chunk_externs().unwrap();
        vm.run().unwrap();
        let booted = vm.output_text();
        assert_eq!(booted, "booted\n");

        // a heap argument goes in and the result and the call output come back
        let name = vm.heap_mut().alloc(HeapObj::Str("ada".into())).unwrap();
        let result = vm.call_export("greet", &[name]).unwrap();
        assert!(matches!(vm.heap().get(result), HeapObj::List(_)));
        assert_eq!(vm.display(result), "['ada', 3]");
        assert_eq!(&vm.output_text()[booted.len()..], "hi ada\n");

        // a failing call renders a traceback naming the function it failed in
        let err = vm.call_export("greet", &[edge::value::Val::int(7)]).unwrap_err();
        let traceback = vm.render_error(&err, SRC, None);
        assert!(traceback.contains("TypeError") && traceback.contains("greet"), "{traceback}");
    }

    // the empty runtime refuses a chunk mini compiled before any of it runs
    #[test]
    fn a_machine_refuses_a_chunk_another_runtime_compiled() {
        let wide = "print(list(range(9223372036854775805, 9223372036854775807, 5)))\n";
        let turns = "fn s(n):\n    return 0 if n == 0 else s(n - 1) + 9223372036854775000\nfor i in range(70):\n    r = s(i % 5)\nprint(r)\n";
        for src in [wide, turns, "print(len([1, 2]))\n"] {
            let (chunk, errors) = crate::common::compile(src);
            assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
            let mut vm = edge::embed::VM::with_limits(&chunk, Limits::sandbox());
            let e = vm.run().expect_err("the empty runtime runs no chunk mini compiled");
            assert!(e.to_string().ends_with("a chunk compiled for mini cannot run on the empty runtime"), "{e}");
            assert!(vm.output.is_empty());
            assert!(vm.call_export("s", &[]).is_err_and(|e| e.to_string().ends_with("cannot run on the empty runtime")));
            let mut own = crate::common::boot(&chunk, Limits::sandbox());
            own.run().expect("mini runs the chunk it compiled");
            assert!(!own.output.is_empty());
        }
    }
}
