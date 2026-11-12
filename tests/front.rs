#[cfg(test)]
mod test {
    use crate::common::{boot, TestResolver};
    use edge::code::{build, compile, Builder, Diagnostic, OpCode, Parsed, SSAChunk, Unit, Value};
    use edge::embed::Limits;
    use edge::lang::Front;
    use edge::deps::walk::{Carry, Fetched, Loaded, Step, Walk};
    use edge::deps::{ImportSpec, Resolver};
    use edge::lexer::{Dot, Shapes, Spelling, Syntax};
    // the builder and the parse it is held to both read the fixed core
    use crate::words::Legacy as Rig;
    use std::rc::Rc;

    // lines led by a word with what follows read through the rig's table
    // the fixed core read in indentation which a builder's chunks compare against
    const LEGACY: Front = Front::spelled::<crate::words::Legacy>();

    const STEPS: Front = Front { extension: "steps", parse: steps, imports, syntax: None, policy: rig::FRONT.policy };

    #[derive(Clone, Copy)]
    struct Line { depth: usize, start: usize, end: usize }

    // every line holding text with its depth by two spaces
    fn lines(src: &str) -> Vec<Line> {
        let (mut out, mut at) = (Vec::new(), 0);
        for text in src.split_inclusive('\n') {
            let body = text.trim_end_matches(['\n', '\r']);
            let lead = body.len() - body.trim_start_matches(' ').len();
            if lead < body.len() { out.push(Line { depth: lead / 2, start: at + lead, end: at + body.len() }); }
            at += text.len();
        }
        out
    }

    fn steps(src: &str, unit: Unit) -> Parsed {
        let lines = lines(src);
        build::<Rig>(src, unit, |b| block(b, src, &lines, &mut 0, 0))
    }

    // the rig's table reading the name after a dot as a key as records want
    struct Keys;
    impl Spelling for Keys {
        const SYNTAX: Syntax = Syntax { name: "keys", shapes: Shapes { dot: Dot::Key, ..Shapes::CORE }, ..crate::words::LEGACY };
        const NATIVES: &'static edge::lang::Natives = &rig::NATIVES;
    }

    fn keyed(src: &str, unit: Unit) -> Parsed {
        let lines = lines(src);
        build::<Keys>(src, unit, |b| block(b, src, &lines, &mut 0, 0))
    }

    fn imports(src: &str) -> Vec<ImportSpec> {
        let spec = |s: &str| if s.starts_with("./") { ImportSpec::Relative(s.into()) } else { ImportSpec::Bare(s.into()) };
        lines(src).iter().filter_map(|l| src[l.start..l.end].strip_prefix("import ")).map(spec).collect()
    }

    // the lines at `depth` from `at` each built as its first word says
    fn block<S: Spelling>(b: &mut Builder<'_, '_, S>, src: &str, lines: &[Line], at: &mut usize, depth: usize) {
        while let Some(&line) = lines.get(*at).filter(|l| l.depth == depth) {
            *at += 1;
            let text = &src[line.start..line.end];
            let (word, rest) = text.split_once(' ').unwrap_or((text, ""));
            let tail = |skip: usize| line.end - rest.len() + skip..line.end;
            b.at(line.start);
            match word {
                "fn" => {
                    let open = rest.find('(').expect("a parameter list");
                    let params: Vec<&str> = rest[open + 1..rest.len() - 1].split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
                    let (from, mut end) = (*at, *at);
                    b.function(&rest[..open], &params, |b| {
                        let mut i = from;
                        block(b, src, lines, &mut i, depth + 1);
                        end = i;
                    });
                    *at = end;
                }
                "let" => {
                    let name = rest.split_once(" = ").expect("a binding").0;
                    b.expr(tail(name.len() + 3)).store(name);
                }
                "each" | "over" => {
                    let name = rest.split_once(" in ").expect("a loop").0;
                    let mut i = *at;
                    b.expr(tail(name.len() + 4)).for_each(&[name], |b| {
                        if word == "over" { b.subject(name, &rig::NATIVES); }
                        block(b, src, lines, &mut i, depth + 1);
                        b.no_subject();
                    });
                    *at = i;
                }
                "if" => {
                    let arms = b.arms().arm(|b| { b.expr(tail(0)); }, |b| block(b, src, lines, at, depth + 1));
                    if lines.get(*at).is_some_and(|l| l.depth == depth && &src[l.start..l.end] == "else") {
                        *at += 1;
                        arms.other(|b| block(b, src, lines, at, depth + 1));
                    }
                }
                "return" => { b.expr(tail(0)).ret(); }
                "raise" => { b.expr(tail(0)).throw(); }
                "import" => {
                    let alias = rest.rsplit('/').next().and_then(|f| f.split('.').next()).unwrap_or(rest);
                    b.import(rest, alias);
                }
                _ => { b.stmts(line.start..line.end); }
            }
        }
    }

    // the chunk and messages a source compiles to through `front` with imports from `resolver`
    fn compiled(front: &Front, src: &str, resolver: Box<dyn Resolver>) -> (SSAChunk, Vec<String>) {
        match front.program(src, resolver) {
            Ok(chunk) => (folded(chunk), Vec::new()),
            Err(errors) => (SSAChunk::default(), errors.into_iter().map(|d| d.msg).collect()),
        }
    }

    fn folded(chunk: Rc<SSAChunk>) -> SSAChunk {
        let mut chunk = Rc::unwrap_or_clone(chunk);
        edge::code::constant_fold(&mut chunk, rig::FRONT.policy);
        chunk
    }

    fn same(steps_src: &str, mini_src: &str) {
        let (ours, theirs) = (compiled(&STEPS, steps_src, Box::new(edge::deps::NoopResolver)), compiled(&LEGACY, mini_src, Box::new(edge::deps::NoopResolver)));
        assert!(ours.1.is_empty() && theirs.1.is_empty(), "{:?} and {:?}", ours.1, theirs.1);
        if let Err(e) = crate::common::same_program(&theirs, &ours) { panic!("{steps_src:?} against {mini_src:?}: {e}"); }
    }

    // what a chunk prints when it runs to its end
    fn output(chunk: &SSAChunk) -> Vec<String> {
        let mut vm = boot(chunk, Limits::sandbox());
        vm.run().unwrap_or_else(|e| panic!("{}", vm.render_error(&e, &chunk.source, Some("main.steps"))));
        vm.output.clone()
    }

    const TOTAL: &str = "fn total(xs)\n  let sum = 0\n  each x in xs\n    if x > 2\n      let sum = sum + x\n    else\n      print(x)\n  return sum\nlet ys = [1, 2, 3, 4]\nprint(total(ys))\n";
    const TOTAL_MINI: &str = "fn total(xs):\n    sum = 0\n    for x in xs:\n        if x > 2:\n            sum = sum + x\n        else:\n            print(x)\n    return sum\nys = [1, 2, 3, 4]\nprint(total(ys))\n";

    // a front end of its own builds the chunk rig parses and runs it
    #[test]
    fn a_front_end_builds_what_mini_parses_and_runs_it() {
        same(TOTAL, TOTAL_MINI);
        let (chunk, errors) = compile(&STEPS, TOTAL);
        assert!(errors.is_empty());
        assert_eq!(output(&chunk), ["1", "2", "7"]);
        // an else chain and a call left bare build as the parser writes them
        same("let a = 3\nif a > 5\n  print(1)\nelse\n  if a > 2\n    print(2)\nprint(a)\n", "a = 3\nif a > 5:\n    print(1)\nelse:\n    if a > 2:\n        print(2)\nprint(a)\n");
    }

    // free names read the subject's fields while bindings and the runtime's natives keep theirs
    #[test]
    fn a_subject_lends_its_fields_to_free_names() {
        let modules = |kind: &str| [("a", "push", 3), ("b", "watch", 1), ("c", "push", 5)].into_iter().fold(TestResolver::new(), |r, (m, k, n)| {
            let src = if kind == "steps" { format!("let kind = \"{k}\"\nlet size = {n}\n") } else { format!("kind = \"{k}\"\nsize = {n}\n") };
            r.with_code(m, &src).with_alias(m, m)
        });
        let src = "import a\nimport b\nimport c\nlet total = 0\nover it in [a, b, c]\n  if kind == \"push\"\n    let total = total + size\n  let f = abs\nprint(total)\n";
        let mini_src = "import a\nimport b\nimport c\ntotal = 0\nfor it in [a, b, c]:\n    if it.kind == \"push\":\n        total = total + it.size\n    f = abs\nprint(total)\n";
        let (ours, theirs) = (compiled(&STEPS, src, Box::new(modules("steps"))), compiled(&LEGACY, mini_src, Box::new(modules("rig"))));
        assert!(ours.1.is_empty() && theirs.1.is_empty(), "{:?} and {:?}", ours.1, theirs.1);
        if let Err(e) = crate::common::same_program(&theirs, &ours) { panic!("{e}"); }
        assert_eq!(output(&ours.0), ["8"]);
        // an import parses through the front end of the source that imports it
        let looped = || Box::new(TestResolver::new().with_code("a", "each n in [3]\n  let size = n\n").with_alias("a", "a"));
        assert!(compiled(&STEPS, "import a\n", looped()).1.is_empty());
        assert!(compiled(&LEGACY, "import a\n", looped()).1.iter().any(|e| e.starts_with("module 'a' parse error")));
    }

    // a table reading keys at dots reads the subject's fields as keys of records
    #[test]
    fn a_subject_reads_record_keys_through_a_keyed_table() {
        let records = "[{\"kind\": \"push\", \"size\": 3}, {\"kind\": \"watch\", \"size\": 1}, {\"kind\": \"push\", \"size\": 5}]";
        let src = format!("let total = 0\nover it in {records}\n  if kind == \"push\"\n    let total = total + size\nprint(total)\n");
        let parsed = format!("total = 0\nfor it in {records}:\n    if it.kind == \"push\":\n        total = total + it.size\nprint(total)\n");
        let keyed = Front { parse: keyed, ..STEPS };
        let (ours, theirs) = (compiled(&keyed, &src, Box::new(edge::deps::NoopResolver)), compiled(&Front::spelled::<Keys>(), &parsed, Box::new(edge::deps::NoopResolver)));
        assert!(ours.1.is_empty() && theirs.1.is_empty(), "{:?} and {:?}", ours.1, theirs.1);
        if let Err(e) = crate::common::same_program(&theirs, &ours) { panic!("{e}"); }
        assert!(ours.0.instructions.iter().any(|i| i.opcode == OpCode::GetItem) && !ours.0.instructions.iter().any(|i| i.opcode == OpCode::LoadAttr));
        assert_eq!(output(&ours.0), ["8"]);
    }

    // diagnostics and tracebacks point into the front end's own file
    #[test]
    fn errors_point_into_the_front_ends_file() {
        let src = "let a = 1\nlet b = a +\n";
        let (_, errors) = compile(&STEPS, src);
        let shown = Diagnostic::render_all(&errors, src, Some("main.steps"));
        assert!(errors.len() == 1 && shown.contains("main.steps:2:") && shown.contains("let b = a +"), "{shown}");
        let src = "fn half(n)\n  return n ~/ 0\nlet x = 1\nprint(half(x))\n";
        let (chunk, errors) = compile(&STEPS, src);
        assert!(errors.is_empty());
        let mut vm = boot(&chunk, Limits::sandbox());
        let e = vm.run().expect_err("a division by zero");
        let shown = vm.render_error(&e, src, Some("main.steps"));
        let at = |line: &str| shown.lines().any(|l| l.contains(&format!("main.steps:{line}:")));
        assert!(at("2") && at("4") && !at("1"), "{shown}");
        // what follows an expression and a return outside any function report where they sit
        let errors = |f: &dyn Fn(&mut Builder<'_, '_, Rig>)| build::<Rig>("x y\n", unit(), |b| f(b)).err().expect("an error").into_iter().map(|d| (d.start, d.msg)).collect::<Vec<_>>();
        assert_eq!(errors(&|b| { b.expr(0..3); }), [(2, "expected the end of the expression, got 'y'".to_string())]);
        assert_eq!(errors(&|b| { b.at(2).constant(Value::None).ret(); }), [(2, "'return' outside function".to_string())]);
        assert_eq!(errors(&|b| { b.at(1).constant(Value::Int(1)).constant(Value::Int(2)).binary(OpCode::Jump); }), [(1, "a builder applies only a binary operator".to_string())]);
        assert_eq!(build::<Rig>("é", unit(), |b| { b.expr(0..1); }).err().expect("a split character").len(), 1);
    }

    fn unit() -> Unit { STEPS.unit(Box::new(edge::deps::NoopResolver)) }

    // each step of the builder's own vocabulary builds what the same rig line parses to
    #[test]
    fn builder_steps_build_what_their_lines_parse_to() {
        let src = "y > 5\nValueError(\"bad\")\n";
        let built = build::<Rig>(src, unit(), |b| {
            b.at(0).constant(Value::Int(1)).constant(Value::Int(2)).list(2).store("xs");
            b.at(0).constant(Value::Str("k".into())).load("xs").map(1).store("m");
            b.at(0).lambda(&["v"], |b| { b.load("v").constant(Value::Int(1)).binary(OpCode::Add); }).store("f");
            b.at(0).load("f").load("m").constant(Value::Str("k".into())).item().constant(Value::Int(0)).item().call(1, 0).store("y");
            b.at(0).load("m").field("get").constant(Value::Str("k".into())).call(1, 0).store("z");
            b.at(0).constant(Value::Bool(true)).store("t");
            b.at(0).constant(Value::None).store("n");
            b.at(0).arms().arm(|b| { b.expr(0..5); }, |b| { b.at(6).expr(6..23).throw(); });
            b.at(0).stmts(0..5);
        });
        let chunk = folded(built.unwrap_or_else(|e| panic!("{:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>())));
        let parsed = compiled(&LEGACY, "xs = [1, 2]\nm = {\"k\": xs}\nf = lambda v: v + 1\ny = f(m[\"k\"][0])\nz = m.get(\"k\")\nt = true\nn = nil\nif y > 5:\n    raise ValueError(\"bad\")\ny > 5\n", Box::new(edge::deps::NoopResolver));
        if let Err(e) = crate::common::same_program(&parsed, &(chunk, Vec::new())) { panic!("{e}"); }
    }

    // a chain of arms builds what an else if chain parses to
    #[test]
    fn arms_chain_as_else_if_does() {
        let src = "a > 1\nprint(1)\na > 0\nprint(2)\nprint(3)\n";
        let built = build::<Rig>(src, unit(), |b| {
            b.at(0).constant(Value::Int(2)).store("a");
            b.at(0).arms().arm(|b| { b.expr(0..5); }, |b| { b.stmts(6..14); }).arm(|b| { b.expr(15..20); }, |b| { b.stmts(21..29); }).other(|b| { b.stmts(30..38); });
        });
        let chunk = folded(built.ok().expect("no errors"));
        let parsed = compiled(&LEGACY, "a = 2\nif a > 1:\n    print(1)\nelse if a > 0:\n    print(2)\nelse:\n    print(3)\n", Box::new(edge::deps::NoopResolver));
        if let Err(e) = crate::common::same_program(&parsed, &(chunk.clone(), Vec::new())) { panic!("{e}"); }
        assert_eq!(output(&chunk), ["1"]);
    }

    // the walk scans sources through the front end and matches its whole suffix
    #[test]
    fn the_walk_follows_a_front_ends_imports_and_suffix() {
        let front = Front { extension: "flow.steps", ..STEPS };
        let files = [("./b.flow.steps", b"import ./c.flow.steps\n".to_vec()), ("./c.flow.steps", b"\0asm".to_vec())];
        let mut walk = Walk::new(&front, "import ./b.flow.steps\n", "", Vec::new(), Carry::default());
        let (mut code, mut plugins) = (Vec::new(), Vec::new());
        let bare = |spec: &str| spec.trim_start_matches("./").to_string();
        loop {
            match walk.step() {
                Step::Fetch(spec) => walk.fetched(files.iter().find(|(f, _)| bare(f) == bare(&spec)).map_or(Fetched::Missing(None), |(_, bytes)| Fetched::Bytes(bytes.clone()))),
                Step::Code { spec, .. } => code.push(bare(&spec)),
                Step::Plugin { spec, .. } => { plugins.push(spec); walk.loaded(Loaded::Ok); }
                Step::System(_) => walk.served(Vec::new()),
                Step::Done(failures) => { assert!(failures.is_empty(), "{failures:?}"); break; }
                Step::Refuse { .. } | Step::Manifest { .. } => {}
            }
        }
        assert_eq!((code, plugins), (vec!["b.flow.steps".to_string(), "c.flow.steps".to_string()], Vec::<String>::new()));
    }

    // built lambdas read the names around them in place so many build in linear time
    #[test]
    #[cfg_attr(miri, ignore)]
    fn many_built_lambdas_build_in_linear_time() {
        let start = std::time::Instant::now();
        let built = build::<Rig>("x\n", unit(), |b| {
            for i in 0..30_000 { b.at(0).lambda(&["v"], |b| { b.load("v"); }).store(&format!("f{i}")); }
        });
        let spent = start.elapsed();
        assert!(built.is_ok() && spent < std::time::Duration::from_secs(2), "{spent:?}");
    }

    // a local a built lambda shadows or loops over reads as before afterwards
    #[test]
    fn a_built_lambda_gives_back_the_names_it_shadows() {
        let built = build::<Rig>("x\n", unit(), |b| {
            b.at(0).function("run", &[], |b| {
                b.at(0).constant(Value::Int(1)).store("x");
                b.at(0).lambda(&["x"], |b| { b.load("x").constant(Value::Int(1)).binary(OpCode::Add); }).store("f");
                b.at(0).lambda(&[], |b| { b.load("range").constant(Value::Int(3)).call(1, 0).for_each(&["x"], |_| {}).load("x"); }).store("g");
                b.at(0).load("x").load("f").constant(Value::Int(5)).call(1, 0).load("g").call(0, 0).list(3).ret();
            });
            b.at(0).load("print").load("run").call(0, 0).call(1, 0).pop();
        });
        assert_eq!(output(&folded(built.ok().expect("no errors"))), ["[1, 6, 2]"]);
    }
    /* a closure per loop pass over `k` with a fresh cell each pass when `fresh` */
    fn passes(b: &mut Builder<'_, '_, Rig>, fresh: bool) {
        b.at(0).constant(Value::Int(0)).store("n").list(0).store("fs");
        b.while_loop(|b| { b.load("n").constant(Value::Int(3)).binary(OpCode::Lt); }, |b| {
            if fresh { b.fresh("k"); }
            b.load("n").store("k");
            b.load("fs").field("append").closure("get", &[], |b| { b.load("k").ret(); }).call(1, 0).pop();
            b.load("n").constant(Value::Int(1)).binary(OpCode::Add).store("n");
        });
        for i in 0..3 { b.load("fs").constant(Value::Int(i)).item().call(0, 0); }
        b.list(3).ret();
    }

    // a fresh cell gives each pass's closure its own variable and a plain store shares one
    #[test]
    fn a_fresh_cell_keeps_each_pass_its_own_variable() {
        let built = build::<Rig>("x\n", unit(), |b| {
            b.at(0).function("fresh", &[], |b| passes(b, true));
            b.at(0).function("shared", &[], |b| passes(b, false));
            b.at(0).load("print").load("fresh").call(0, 0).load("shared").call(0, 0).call(2, 0).pop();
        });
        assert_eq!(output(&folded(built.ok().expect("no errors"))), ["[0, 1, 2] [2, 2, 2]"]);
    }

    // fields lazy tests unary ops and stack moves a front end lays by hand
    #[test]
    fn builder_pieces_lay_fields_classes_and_short_circuits() {
        let built = build::<Rig>("x\n", unit(), |b| {
            // a method of a class reads the field a store left on its instance
            b.at(0).class("Box", 0, |b| { b.function("get", &["self"], |b| { b.load("self").field("v").ret(); }); }).store("Box");
            b.at(0).load("Box").call(0, 0).store("o");
            // a store keeps its value on top as an assignment expression does
            b.at(0).load("o").constant(Value::Int(7)).dup().rot3().swap().set_field("v").store("kept");
            b.at(0).constant(Value::Bool(false)).lazy(true, |b| { b.constant(Value::Int(1)); }).store("a");
            b.at(0).constant(Value::Int(0)).lazy(false, |b| { b.constant(Value::Int(2)); }).store("c");
            b.at(0).constant(Value::Int(5)).unary(OpCode::Minus).store("m");
            b.at(0).constant(Value::Bool(true)).unary(OpCode::Not).store("t");
            b.at(0).load("print").load("kept").load("o").field("get").call(0, 0).load("a").load("c").load("m").load("t").call(6, 0).pop();
        });
        assert_eq!(output(&folded(built.ok().expect("no errors"))), ["7 7 false 2 -5 false"]);
    }

    // a nonlocal writes the body around it and a misused op refuses to build
    #[test]
    fn a_nonlocal_writes_the_body_around_it() {
        let built = build::<Rig>("x\n", unit(), |b| {
            b.at(0).function("count", &[], |b| {
                b.constant(Value::Int(0)).store("n");
                b.closure("bump", &[], |b| { b.nonlocal("n").load("n").constant(Value::Int(1)).binary(OpCode::Add).store("n").constant(Value::None).ret(); }).store("bump");
                b.load("bump").call(0, 0).pop().load("bump").call(0, 0).pop().load("n").ret();
            });
            b.at(0).load("print").load("count").call(0, 0).call(1, 0).pop();
        });
        assert_eq!(output(&folded(built.ok().expect("no errors"))), ["2"]);
        let refused = build::<Rig>("x\n", unit(), |b| { b.constant(Value::Int(1)).unary(OpCode::Add); });
        assert!(refused.is_err_and(|e| e[0].msg.contains("unary")));
    }
    // the engine's rules let classes hold fields share bound methods and order texts
    #[test]
    fn the_engine_rules_keep_fields_bound_methods_and_order_open() {
        let built = build::<Rig>("x\n", unit(), |b| {
            b.at(0).class("Box", 0, |b| { b.function("get", &["self"], |b| { b.constant(Value::None).ret(); }); }).store("Box");
            b.at(0).load("Box").constant(Value::Int(1)).set_field("tag");
            b.at(0).load("Box").call(0, 0).store("o");
            b.at(0).load("print").load("Box").field("tag").load("o").field("get").load("o").field("get").binary(OpCode::Is);
            b.constant(Value::Str("a".into())).constant(Value::Str("b".into())).binary(OpCode::Lt).call(3, 0).pop();
        });
        assert_eq!(output(&folded(built.ok().expect("no errors"))), ["1 true true"]);
    }
}
