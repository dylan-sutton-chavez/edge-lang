/* The tables the engine makes public for tools that generate rules from it, checked against what the engine does. */
mod test {
    use std::collections::HashSet;
    use compiler::parser::{OpCode, BUILTIN_TYPES};
    use compiler::vm::{ALL_METHODS, exc_parents, lookup_method};

    #[test]
    fn every_opcode_is_listed_once_at_its_byte() {
        let names: HashSet<String> = OpCode::ALL.iter().map(|op| format!("{op:?}")).collect();
        assert_eq!(names.len(), OpCode::ALL.len());
        for (i, &op) in OpCode::ALL.iter().enumerate() { assert_eq!(op as usize, i, "{op:?}"); }
        assert_eq!(OpCode::ALL.last(), Some(&OpCode::OwnResetR));
    }

    #[test]
    fn every_method_is_unique_and_found_by_its_lookup() {
        let mut seen = HashSet::new();
        for m in ALL_METHODS {
            assert!(seen.insert((m.ty, m.name)), "{}.{} listed twice", m.ty, m.name);
            let id = lookup_method(m.ty, m.name).unwrap_or_else(|| panic!("{}.{} not found", m.ty, m.name));
            assert_eq!(id.name(), m.name);
        }
    }

    #[test]
    fn a_method_says_whether_it_changes_its_receiver() {
        let mutating = |ty: &str, name: &str| ALL_METHODS.iter().find(|m| m.ty == ty && m.name == name).unwrap_or_else(|| panic!("{ty}.{name}")).mutating;
        for (ty, name) in [("list", "append"), ("list", "sort"), ("list", "pop"), ("dict", "update"), ("set", "add"), ("bytearray", "extend"), ("generator", "send")] {
            assert!(mutating(ty, name), "{ty}.{name}");
        }
        for (ty, name) in [("list", "count"), ("list", "index"), ("list", "copy"), ("str", "upper"), ("dict", "get"), ("set", "union"), ("bytes", "replace"), ("frozenset", "copy")] {
            assert!(!mutating(ty, name), "{ty}.{name}");
        }
    }

    #[test]
    fn every_exception_reaches_base_exception_without_a_cycle() {
        let exceptions: Vec<&str> = BUILTIN_TYPES.iter().copied().filter(|n| *n == "BaseException" || !exc_parents(n).is_empty()).collect();
        assert!(exceptions.len() > 60, "{exceptions:?}");
        // Every path up ends at BaseException within as many steps as there are classes, which no cycle does.
        fn walk(n: &str, depth: usize, limit: usize) {
            assert!(depth <= limit, "{n} loops");
            let parents = exc_parents(n);
            assert!(n == "BaseException" || !parents.is_empty(), "stops at {n}");
            for p in parents { walk(p, depth + 1, limit); }
        }
        for name in &exceptions { walk(name, 0, exceptions.len()); }
        assert_eq!(exc_parents("ExceptionGroup"), ["BaseExceptionGroup", "Exception"]);
        assert!(exc_parents("int").is_empty());
    }

    #[test]
    fn the_exception_tree_is_what_issubclass_answers() {
        let src = "print(issubclass(TimeoutError, OSError), issubclass(KeyError, LookupError), issubclass(ExceptionGroup, BaseExceptionGroup))";
        let (tokens, _) = compiler::lexer::lex(src);
        let (chunk, errors) = compiler::parser::Parser::new(src, tokens.into_iter()).parse();
        assert!(errors.is_empty());
        let mut vm = compiler::vm::VM::with_limits(&chunk, compiler::vm::Limits::sandbox());
        vm.run().unwrap();
        assert_eq!(vm.output_text(), "True True True\n");
        for (child, parent) in [("TimeoutError", "OSError"), ("KeyError", "LookupError")] { assert!(exc_parents(child).contains(&parent)); }
    }

    #[test]
    fn a_function_scope_and_parameters_are_those_the_vm_resolved() {
        use compiler::parser::Parser;
        use compiler::vm::{Kind, ParamKind, VM};
        let src = "g = 1\ndef f(a, *r, k=2, **m):\n    x = a\n    def h():\n        return x\n    return g\n";
        let (tokens, _) = compiler::lexer::lex(src);
        let (chunk, errors) = Parser::new(src, tokens.into_iter()).parse();
        assert!(errors.is_empty());
        let vm = VM::new(&chunk);
        let body = &chunk.functions[0].1;
        let (fi, scope) = vm.scope_of(body).expect("f is indexed");
        let kind = |name: &str| scope.kinds[body.names.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name}"))];
        assert_eq!(kind("a_0"), Kind::Local);
        assert!(matches!(kind("g_0"), Kind::Global(_)));
        assert!(scope.cellvars.iter().any(|&s| body.names[s].starts_with('x')));
        let kinds: Vec<ParamKind> = vm.params_of(fi).iter().map(|p| p.0).collect();
        assert_eq!(kinds, [ParamKind::Normal, ParamKind::Star, ParamKind::KwOnly, ParamKind::DoubleStar]);
        assert!(vm.scope_of(&chunk).is_none());
    }
}
