/* The tables the engine makes public for tools that generate rules from it, checked against what the engine does. */
mod test {
    use std::collections::HashSet;
    use compiler::code::OpCode;
    use compiler::types::{ALL_METHODS, BUILTIN_TYPES, CLASS_DUNDERS, NativeFnId, class_dunder, exc_parents, lookup_method};

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
        let (chunk, errors) = compiler::code::Parser::new(src, tokens.into_iter()).parse();
        assert!(errors.is_empty());
        let mut vm = compiler::embed::VM::with_limits(&chunk, compiler::embed::Limits::sandbox());
        vm.run().unwrap();
        assert_eq!(vm.output_text(), "True True True\n");
        for (child, parent) in [("TimeoutError", "OSError"), ("KeyError", "LookupError")] { assert!(exc_parents(child).contains(&parent)); }
    }

    #[test]
    fn a_function_scope_and_parameters_are_those_the_vm_resolved() {
        use compiler::code::Parser;
        use compiler::code::{Kind, ParamKind};
        use compiler::embed::VM;
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

    #[test]
    fn every_method_is_pure_or_says_why_not() {
        let off: Vec<String> = ALL_METHODS.iter().filter(|m| m.pure == m.excluded().is_some() || m.pure && m.mutating).map(|m| format!("{}.{}", m.ty, m.name)).collect();
        assert!(off.is_empty(), "neither pure nor excluded with a reason: {}", off.join(" "));
        let off: Vec<&str> = NativeFnId::ALL.iter().filter(|f| f.pure() == f.excluded().is_some()).map(|f| f.name()).collect();
        assert!(off.is_empty(), "builtins neither pure nor excluded with a reason: {}", off.join(" "));
    }

    #[test]
    fn a_class_body_binds_every_listed_dunder_and_no_other() {
        let compiles = |name: &str| {
            let src = format!("class A:\n    def {name}(self): pass\n");
            let (tokens, _) = compiler::lexer::lex(&src);
            compiler::code::Parser::new(&src, tokens.into_iter()).parse().1.is_empty()
        };
        let mut seen = HashSet::new();
        for &name in CLASS_DUNDERS {
            assert!(seen.insert(name), "{name} listed twice");
            assert!(class_dunder(name) && compiles(name), "{name}");
        }
        for name in ["__set__", "__delete__", "__set_name__", "__slots__"] { assert!(CLASS_DUNDERS.contains(&name), "{name}"); }
        for name in ["__del__", "__sizeof__", "__bytes__"] { assert!(!class_dunder(name) && !compiles(name), "{name}"); }
    }

    /* The bindings of the slots of `body` named `name`, the slots no variable takes left out. */
    fn bound(p: &compiler::code::Program, body: &compiler::code::SSAChunk, name: &str) -> Vec<compiler::code::Binding> {
        use compiler::code::Binding;
        let (_, b) = p.bindings_of(body).expect("a body of the program");
        let mut all: Vec<Binding> = body.names.iter().enumerate().filter(|(_, n)| compiler::code::ssa_strip(n) == name).map(|(i, _)| b[i]).collect();
        all.dedup();
        if all.iter().any(|b| *b != Binding::Local) { all.retain(|b| *b != Binding::Local); }
        all
    }

    #[test]
    fn module_and_class_bodies_bind_as_the_vm_resolves_them() {
        use compiler::code::{Binding::*, BodyKind, Program, compile};
        let global = |b: &[compiler::code::Binding]| matches!(b, [Global(_)]);
        // A module binds into its globals, a function reads them.
        let (c, e) = compile("x = 1\ndef f(): return x\nprint(len([x]))\n");
        assert!(e.is_empty());
        let p = Program::new(&c);
        assert_eq!(p.bindings_of(&c).map(|b| b.0), Some(BodyKind::Module));
        assert!(global(&bound(&p, &c, "x")) && global(&bound(&p, &c, "f")));
        assert_eq!(p.bindings_of(&c.functions[0].1).map(|b| b.0), Some(BodyKind::Function(0)));
        assert_eq!(bound(&p, &c.functions[0].1, "x"), bound(&p, &c, "x"));
        // A class body's names are its members, a method reads past them to the module.
        let (c, _) = compile("y = 2\nclass A:\n    a = y\n    def m(self): return a\n");
        let p = Program::new(&c);
        let a = &c.classes[0];
        assert_eq!(p.bindings_of(a).map(|b| b.0), Some(BodyKind::Class));
        assert_eq!((bound(&p, a, "a"), bound(&p, a, "m")), (vec![Member], vec![Member]));
        assert_eq!(bound(&p, a, "y"), bound(&p, &c, "y"));
        assert!(global(&bound(&p, &a.functions[0].1, "a")));
        // A class inside a function reads the function's cells and borrows its plain locals, its methods share the cells.
        let (c, _) = compile("def f():\n    c = 1\n    p = 2\n    def g(): return c\n    class B:\n        u = c\n        v = p\n        def m(self): return c\n    return B\n");
        let p = Program::new(&c);
        let (f, b) = (&c.functions[0].1, &c.functions[0].1.classes[0]);
        assert_eq!((bound(&p, f, "c"), bound(&p, f, "p")), (vec![Cell], vec![Local]));
        assert_eq!((bound(&p, b, "c"), bound(&p, b, "p"), bound(&p, b, "u")), (vec![Cell], vec![Lent], vec![Member]));
        let (_, m) = p.scope_of(&b.functions[0].1).expect("m is a function");
        assert_eq!(m.freevars.iter().map(|v| v.2.as_str()).collect::<Vec<_>>(), ["c"]);
        // A class body's `global` writes the module, and a class inside a class reads none of the function's plain locals.
        let (c, _) = compile("def f():\n    w = 1\n    class D:\n        global q\n        q = 1\n        class I:\n            t = w\n    return D\n");
        let p = Program::new(&c);
        let d = &c.functions[0].1.classes[0];
        assert!(global(&bound(&p, d, "q")) && global(&bound(&p, &d.classes[0], "w")));
        // A comprehension in a class body is a function its members are not visible to, past the iterable it is handed.
        let (c, _) = compile("class E:\n    xs = [1, 2]\n    ys = [i * 2 for i in xs]\n    zs = [xs for i in range(2)]\n");
        let p = Program::new(&c);
        let e = &c.classes[0];
        assert_eq!((bound(&p, e, "xs"), bound(&p, e, "ys")), (vec![Member], vec![Member]));
        assert_eq!(bound(&p, &e.functions[0].1, "i"), vec![Local]);
        assert!(global(&bound(&p, &e.functions[1].1, "xs")));
    }
}
