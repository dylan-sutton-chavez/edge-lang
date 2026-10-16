/* The VM calls a Rust embedder builds on, the site runs programs through exactly these, so none reads as dead here. */
mod test {
    use compiler::lexer::lex;
    use compiler::parser::Parser;
    use compiler::vm::{HeapObj, Limits, VM};

    use crate::common::TestResolver;

    const SRC: &str = "print('booted')\ndef greet(name):\n    print('hi', name)\n    return [name, len(name)]\n";

    #[test]
    fn an_embedder_runs_a_program_calls_into_it_and_reads_what_it_printed() {
        let (tokens, _) = lex(SRC);
        let (chunk, errors) = Parser::with_resolver(SRC, tokens.into_iter(), Box::new(TestResolver::new())).parse();
        assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
        let mut vm = VM::with_limits(&chunk, Limits::sandbox());
        vm.bind_chunk_externs().unwrap();
        vm.run().unwrap();
        let booted = vm.output_text();
        assert_eq!(booted, "booted\n");

        // An argument built on the heap, the result read back, and only what the call printed past the boot.
        let name = vm.heap_mut().alloc(HeapObj::Str("ada".into())).unwrap();
        let result = vm.call_export("greet", &[name]).unwrap();
        assert!(matches!(vm.heap().get(result), HeapObj::List(_)));
        assert_eq!(vm.display(result), "['ada', 3]");
        assert_eq!(&vm.output_text()[booted.len()..], "hi ada\n");

        // A failing call renders a traceback that names the function it failed in.
        let err = vm.call_export("greet", &[compiler::vm::Val::int(7)]).unwrap_err();
        let traceback = vm.render_error(&err, SRC, None);
        assert!(traceback.contains("TypeError") && traceback.contains("greet"), "{traceback}");
    }
}
