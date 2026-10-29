mod test {
    use std::collections::HashSet;
    use edge::code::OpCode;

    #[test]
    fn every_opcode_is_listed_once_at_its_byte() {
        let names: HashSet<String> = OpCode::ALL.iter().map(|op| format!("{op:?}")).collect();
        assert_eq!(names.len(), OpCode::ALL.len());
        for (i, &op) in OpCode::ALL.iter().enumerate() { assert_eq!(op as usize, i, "{op:?}"); }
        assert_eq!(OpCode::ALL.last(), Some(&OpCode::OwnResetR));
    }

    #[test]
    fn a_function_scope_and_parameters_are_those_the_vm_resolved() {
        use edge::code::{Kind, ParamKind};
        use edge::embed::VM;
        let src = "g = 1\nfn f(a, *r, k=2, **m):\n    x = a\n    fn h():\n        return x\n    return g\n";
        let (chunk, errors) = crate::common::parse(src);
        assert!(errors.is_empty());
        let vm = VM::new(&chunk);
        let body = &chunk.functions[0].1;
        let (fi, scope) = vm.scope_of(body).expect("f is indexed");
        let kind = |name: &str| scope.kinds[body.names.iter().position(|n| n == name).unwrap_or_else(|| panic!("{name}"))];
        assert_eq!(kind("a_0"), Kind::Local);
        assert!(matches!(kind("g_0"), Kind::Global(_)));
        assert!(scope.cellvars.iter().any(|&s| body.names[s].starts_with('x')));
        let kinds: Vec<ParamKind> = vm.params_of(fi).iter().map(|p| p.0).collect();
        assert_eq!(kinds, [ParamKind::Normal, ParamKind::Star, ParamKind::KwOnly, ParamKind::KwRest]);
        assert!(vm.scope_of(&chunk).is_none());
    }

    /* the bindings of the slots of `body` named `name` leaving out slots no variable takes */
    fn bound(p: &edge::code::Program, body: &edge::code::SSAChunk, name: &str) -> Vec<edge::code::Binding> {
        use edge::code::Binding;
        let (_, b) = p.bindings_of(body).expect("a body of the program");
        let mut all: Vec<Binding> = body.names.iter().enumerate().filter(|(_, n)| edge::code::ssa_strip(n) == name).map(|(i, _)| b[i]).collect();
        all.dedup();
        if all.iter().any(|b| *b != Binding::Local) { all.retain(|b| *b != Binding::Local); }
        all
    }

    #[test]
    fn module_and_class_bodies_bind_as_the_vm_resolves_them() {
        use edge::code::{Binding::*, BodyKind, Program};
        use crate::common::compile;
        let global = |b: &[edge::code::Binding]| matches!(b, [Global(_)]);
        // a module binds into its globals and a function reads them
        let (c, e) = compile("x = 1\nfn f(): return x\nprint(len([x]))\n");
        assert!(e.is_empty());
        let p = Program::new(&c);
        assert_eq!(p.bindings_of(&c).map(|b| b.0), Some(BodyKind::Module));
        assert!(global(&bound(&p, &c, "x")) && global(&bound(&p, &c, "f")));
        assert_eq!(p.bindings_of(&c.functions[0].1).map(|b| b.0), Some(BodyKind::Function(0)));
        assert_eq!(bound(&p, &c.functions[0].1, "x"), bound(&p, &c, "x"));
        // class body names are members and a method reads past them to the module
        let (c, _) = compile("y = 2\nclass A:\n    a = y\n    fn m(self): return a\n");
        let p = Program::new(&c);
        let a = &c.classes[0];
        assert_eq!(p.bindings_of(a).map(|b| b.0), Some(BodyKind::Class));
        assert_eq!((bound(&p, a, "a"), bound(&p, a, "m")), (vec![Member], vec![Member]));
        assert_eq!(bound(&p, a, "y"), bound(&p, &c, "y"));
        assert!(global(&bound(&p, &a.functions[0].1, "a")));
        // a class in a function borrows plain locals and reads cells its methods share too
        let (c, _) = compile("fn f():\n    c = 1\n    p = 2\n    fn g(): return c\n    class B:\n        u = c\n        v = p\n        fn m(self): return c\n    return B\n");
        let p = Program::new(&c);
        let (f, b) = (&c.functions[0].1, &c.functions[0].1.classes[0]);
        assert_eq!((bound(&p, f, "c"), bound(&p, f, "p")), (vec![Cell], vec![Local]));
        assert_eq!((bound(&p, b, "c"), bound(&p, b, "p"), bound(&p, b, "u")), (vec![Cell], vec![Lent], vec![Member]));
        let (_, m) = p.scope_of(&b.functions[0].1).expect("m is a function");
        assert_eq!(m.freevars.iter().map(|v| v.2.as_str()).collect::<Vec<_>>(), ["c"]);
        // `global` in a class body writes the module and inner classes see no plain locals
        let (c, _) = compile("fn f():\n    w = 1\n    class D:\n        global q\n        q = 1\n        class I:\n            t = w\n    return D\n");
        let p = Program::new(&c);
        let d = &c.functions[0].1.classes[0];
        assert!(global(&bound(&p, d, "q")) && global(&bound(&p, &d.classes[0], "w")));
        // a comprehension in a class body sees no members except the iterable it is handed
        let (c, _) = compile("class E:\n    xs = [1, 2]\n    ys = [i * 2 for i in xs]\n    zs = [xs for i in range(2)]\n");
        let p = Program::new(&c);
        let e = &c.classes[0];
        assert_eq!((bound(&p, e, "xs"), bound(&p, e, "ys")), (vec![Member], vec![Member]));
        assert_eq!(bound(&p, &e.functions[0].1, "i"), vec![Local]);
        assert!(global(&bound(&p, &e.functions[1].1, "xs")));
    }

    use edge::code::{Class, Fast, NativeSite, Operator, SSAChunk};

    #[test]
    fn every_op_of_the_table_sits_at_its_byte_with_its_columns() {
        assert_eq!(OpCode::INFO.len(), OpCode::EVERY.len());
        assert_eq!(OpCode::EVERY[..OpCode::ALL.len()], OpCode::ALL);
        for (i, &op) in OpCode::EVERY.iter().enumerate() {
            let info = op.info();
            assert_eq!(op as usize, i, "{op:?}");
            assert_eq!(info.name, format!("{op:?}"));
            let class = if i < OpCode::Move as usize { Class::Stack } else if i < OpCode::ALL.len() { Class::Register } else { Class::Internal };
            assert_eq!(info.class, class, "{op:?}");
            // the predicates the parser and the folder call are the columns as patterns
            assert_eq!(op.is_register(), class != Class::Stack, "{op:?}");
            assert_eq!(op.is_jump(), info.operand == edge::code::Operand::Jump, "{op:?}");
            assert_eq!(op.names_slot(), info.operand == edge::code::Operand::Slot, "{op:?}");
            assert_eq!(op.names_attr(), info.operand == edge::code::Operand::Attr, "{op:?}");
            assert_eq!(op.is_binary(), info.operator == Operator::Binary, "{op:?}");
            assert_eq!(op.folds_binary(), info.operator == Operator::Binary && info.fold, "{op:?}");
            // a register form falls back to the op whose form or branch it is
            if let Some(stack) = info.stack { assert!(stack.info().form == Some(op) || stack.info().branch == Some(op), "{op:?}"); }
            // the columns a run reads alone are the row's own
            assert_eq!((op.form(), op.branch(), op.operator(), op.edge(), op.rows()), (info.form, info.branch, info.operator, info.edge, info.rows), "{op:?}");
            // a generated arm runs a register form and only a move has no stack op
            assert!(info.fast == Fast::None || class == Class::Register && (info.fast == Fast::Move) == info.stack.is_none(), "{op:?}");
            let compares = info.stack.is_some_and(|s| s.info().branch.is_some());
            assert!(!matches!(info.fast, Fast::Order | Fast::Test) || compares, "{op:?}");
        }
    }

    #[test]
    fn the_hash_covers_every_column_in_order() {
        fn fnv(h: u64, bytes: &[u8]) -> u64 { bytes.iter().fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01B3)) }
        let byte = |op: Option<OpCode>| op.map_or(u8::MAX, |op| op as u8);
        let h = OpCode::INFO.iter().fold(0xCBF2_9CE4_8422_2325, |h, e| {
            let h = fnv(fnv(h, &[e.name.len() as u8]), e.name.as_bytes());
            fnv(h, &[e.class as u8, e.pops.code(), e.pushes.code(), e.operand as u8, e.arm as u8, e.flow as u8, e.edge as u8, e.spill as u8,
                e.operator as u8, u8::from(e.fold), byte(e.form), byte(e.branch), byte(e.stack), e.rows as u8, e.fast as u8])
        });
        assert_eq!(h, OpCode::HASH);
    }

    /* chunks whose functions classes and native sites give every count of an effect its cases */
    fn chunks() -> Vec<SSAChunk> {
        let mut c = SSAChunk::default();
        for d in [0, 1, 3, 255] { c.functions.push((Vec::new(), SSAChunk::default(), d, 0)); }
        for bases in [0, 1, 2, 7, edge::code::SPREAD_BASES] {
            let mut k = SSAChunk::default();
            k.bases = bases;
            c.classes.push(k);
        }
        for (pos, kw, void) in [(0, 0, false), (1, 0, true), (3, 1, false), (7, 2, true), (255, 255, false)] {
            c.native_sites.push(NativeSite { id: 5, pos, kw, spread: kw > 1, pinned: false, fast: false, void });
        }
        vec![SSAChunk::default(), c]
    }

    #[test]
    fn each_effect_and_predicate_is_what_its_columns_say() {
        for chunk in chunks() {
            for &op in &OpCode::EVERY {
                // a dict's pairs double in sixteen bits so past half they overflow
                let top = if op == OpCode::BuildDict { 0x7FFF } else { u16::MAX };
                for operand in 0..=top { assert_eq!(op.effect(operand, &chunk), op.info().effect(operand, &chunk), "{op:?} {operand}"); }
                // what the operand alone sets is what an empty chunk lets the columns count
                if chunk.functions.is_empty() { for operand in 0..=top { assert!(op.plain_effect(operand).is_none_or(|e| Some(e) == op.info().effect(operand, &chunk)), "{op:?} {operand}"); } }
                // an OpR row takes what its op takes when no chunk sets the count
                if chunk.functions.is_empty() && op.form() == Some(OpCode::OpR) { for operand in 0..=top { assert_eq!(op.plain_effect(operand), op.info().effect(operand, &chunk), "{op:?} {operand}"); } }
            }
        }
        for &op in &OpCode::EVERY {
            // the predicates the lowering asks per instruction are the columns as patterns
            assert_eq!(op.ends_flow(), !op.info().falls(), "{op:?}");
            assert_eq!(op.unstacked(), !op.is_register() && op.info().fixed() == Some((0, 0)), "{op:?}");
        }
    }

    #[test]
    fn a_runtime_declares_a_table_of_its_own_with_the_same_macro() {
        use super::toy::Toy;
        assert_eq!(Toy::ALL, [Toy::Push, Toy::Clamp, Toy::Plus, Toy::PlusR]);
        assert!(!Toy::EVERY.iter().any(|op| op.is_jump() || op.names_slot() || op.names_attr() || op.folds_binary()));
        assert!(Toy::Plus.is_binary() && Toy::PlusR.is_register() && !Toy::Plus.is_register());
        assert_eq!(Toy::Clamp.effect(0, &SSAChunk::default()), Some((3, 1)));
        assert_eq!(Toy::PlusR.info().stack, Some(Toy::Plus));
        assert_ne!(Toy::HASH, OpCode::HASH);
    }
}

// a table a runtime of its own declares with the engine's macro
mod toy {
    edge::ops! {
        table Toy: Debug;
        stack {
            Push   0  1  constant  values  -  -  -  -       -  -      -;
            Clamp  3  1  none      values  -  -  -  -       -  -      -;
            Plus   2  1  flags     values  -  -  -  binary  -  PlusR  -;
        }
        register {
            PlusR  Plus  -  -;
        }
        internal {}
    }
}
