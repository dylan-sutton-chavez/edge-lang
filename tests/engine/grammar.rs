#[cfg(test)]
mod test {
    use crate::common::boot;
    use interpreter::code::OpCode as O;
    use interpreter::embed::Limits;
    use interpreter::lang::Front;
    use interpreter::lexer::grammar::{Emit, Form, Grammar, Mode, Pattern, Row, Slot, Test};
    use interpreter::lexer::TokenType as T;
    use interpreter::lexer::{index, operators, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Keywords, Lambda, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros};

    const WORDS: &[(&str, TokenType)] = &[("true", T::True), ("false", T::False), ("or", T::Or), ("in", T::In), ("if", T::If), ("else", T::Else), ("match", T::Match), ("fn", T::Fn), ("let", T::Let), ("for", T::For), ("use", T::Import), ("_", T::Underscore)];
    const SYMBOLS: &[(&str, TokenType)] = &[
        ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose), ("{", T::BlockOpen), ("}", T::BlockClose), (":", T::Colon),
        ("=>", T::Arm), ("==", T::EqualEqual), ("<", T::Less), ("=", T::Equal), ("+", T::Plus), ("-", T::Minus), ("/", T::Slash), (",", T::Comma), (".", T::Dot),
    ];
    static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);
    static KEYWORDS: Keywords = index(WORDS);
    const ROWS: &[Row] = &[
        Row { tok: T::Or, left: 4, right: 5, emit: Emit::Lazy(false) }, Row { tok: T::EqualEqual, left: 10, right: 11, emit: Emit::Op(O::Eq) },
        Row { tok: T::Less, left: 10, right: 11, emit: Emit::Op(O::Lt) }, Row { tok: T::In, left: 10, right: 11, emit: Emit::Op(O::In) },
        Row { tok: T::Plus, left: 14, right: 15, emit: Emit::Op(O::Add) }, Row { tok: T::Minus, left: 14, right: 15, emit: Emit::Op(O::Sub) },
        Row { tok: T::Slash, left: 16, right: 17, emit: Emit::Op(O::FloorDiv) },
    ];
    const LIMITS: &[(&str, &str)] = &[("work", "work"), ("wide", "wide")];
    static GRAMMAR: Grammar = Grammar {
        modes: &[Mode { rows: ROWS, words: &[] }],
        forms: &[
            Form { head: "fix", slots: &[Slot::Bind, Slot::Word("="), Slot::Expr(0), Slot::Word("up"), Slot::Word("to"), Slot::Expr(0), Slot::Body(0)], call: "steps" },
            Form { head: "here", slots: &[Slot::At, Slot::Expr(0), Slot::Limits("within", LIMITS)], call: "trio" },
        ],
        patterns: &[Pattern { word: "yes", test: Test::Item(0, true), part: None }],
        lazy: &["any", "pick"],
        ..Grammar::NONE
    };
    const SYNTAX: Syntax = Syntax {
        name: "forms", extension: "forms", words: WORDS, symbols: SYMBOLS, operators: &OPERATORS, keywords: &KEYWORDS, precedence: &[], chained: false,
        blocks: Blocks::Braces, ends: Ends::Semicolon, comments: Comments { line: &["--"], block: None, nested: false },
        shapes: Shapes { cond: Cond::None, lambda: Lambda::None, keyword_arg: T::Equal, splat: false, slices: false, dot: Dot::Key, header_parens: false, comprehensions: true, decorators: false, named: false, binding: Binding::Optional, arms: true, ranges: false, grammar: Some(&GRAMMAR) },
        strings: Strings { quotes: &[Quote { byte: b'"', multiline: false, triple: false }], prefixes: &[], any_case: false, hashes: false, adjacent: false, newlines: false, escapes: Escapes { simple: &[], hex: false, unicode: Unicode::Fixed, octal: false, line_join: false, unknown: Unknown::Refuse, refused: &[] } },
        interp: None,
        numbers: Numbers { radixes: &[], separator: None, zeros: Zeros::Refused, leading_dot: false, trailing_dot: false, exponent: false, suffix: Suffix::Refused },
        continuation: Continuation { join: None, operator: false },
        soft: &[],
        bom: false,
    };
    struct Forms;
    impl Spelling for Forms {
        const SYNTAX: Syntax = SYNTAX;
        const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
    }

    // the same table with no colon so neither `:` nor `while` has a spelling
    const BARE_SYMBOLS: &[(&str, TokenType)] = &[
        ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose), ("{", T::BlockOpen), ("}", T::BlockClose),
        ("=>", T::Arm), ("==", T::EqualEqual), ("<", T::Less), ("=", T::Equal), ("+", T::Plus), ("-", T::Minus), ("/", T::Slash), (",", T::Comma), (".", T::Dot),
    ];
    static BARE_OPERATORS: [[TokenType; 4]; 128] = operators(BARE_SYMBOLS);
    struct Bare;
    impl Spelling for Bare {
        const SYNTAX: Syntax = Syntax { symbols: BARE_SYMBOLS, operators: &BARE_OPERATORS, ..SYNTAX };
        const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
    }

    // a form's call is a function of the script so each test sees what the slots gave
    const CALLS: &str = "fn steps(x, n, f) = if n == 0 or f(x) == x { x } else { steps(f(x), n - 1, f) }\nfn trio(a, b, c) = [a, b, c]\nfn pick(n, g) = g\n";

    // a module the tests import by the name m
    struct One;
    impl interpreter::deps::Resolver for One {
        fn resolve(&mut self, spec: &str) -> Result<interpreter::deps::Resolved, String> {
            if spec == "m" { Ok(interpreter::deps::Resolved::Code { src: "let x = 5\n".into(), canonical: "m".into() }) } else { Err(format!("no module {spec}")) }
        }
        fn child(&self, _: &str) -> Box<dyn interpreter::deps::Resolver> { Box::new(One) }
    }

    fn run(src: &str) -> String {
        const FRONT: Front = Front::spelled::<Forms>();
        let src = format!("{CALLS}{src}");
        let chunk = FRONT.program(&src, Box::new(One)).unwrap_or_else(|e| panic!("{}", interpreter::code::Diagnostic::render_all(&e, &src, None)));
        let mut vm = boot(&chunk, Limits::sandbox());
        match vm.run() { Ok(_) => vm.output_text(), Err(e) => vm.error_text(&e) }
    }

    #[test]
    fn a_bound_name_is_the_parameter_of_the_body_after_it() {
        assert_eq!(run("let _ = print(fix k = 0 up to 64 { if k < 5 { k + 1 } else { k } }, fix k = 0 up to 2 { k + 1 })"), "5 2\n");
    }

    #[test]
    fn a_form_hands_its_head_offset_and_a_bare_limit_counts_one() {
        let src = "let _ = print(here 1 within wide, work 4)\nlet _ = print(here 2 within wide)";
        let at = |k: usize| CALLS.len() + src.match_indices("here").nth(k).map_or(0, |m| m.0);
        assert_eq!(run(src), format!("[{}, 1, [[\"wide\", 1], [\"work\", 4]]]\n[{}, 2, [[\"wide\", 1]]]\n", at(0), at(1)));
    }

    #[test]
    fn a_lazy_name_reads_its_lone_comprehension_item_by_item() {
        assert_eq!(run("let _ = print(any([n == 0 or 1 / (n - n) == 1 for n in [0, 1]]))"), "true\n");
        // a comprehension that is not the whole argument and a name off the list stay lists
        assert!(run("let _ = print(any([n == 0 or 1 / (n - n) == 1 for n in [0, 1]] + []))").contains("DivisionError"));
        assert!(run("let _ = print(pick(1, [n for n in [1]]))").contains("generator"));
        assert!(run("let _ = print(all([n == 1 or 1 / (n - n) == 1 for n in [0, 1]]))").contains("DivisionError"));
    }

    #[test]
    fn a_dot_reads_a_module_s_export_as_a_key() {
        assert_eq!(run("use m\nlet _ = print(m.x)"), "5\n");
    }

    #[test]
    fn a_literal_pattern_takes_the_value_equal_to_it() {
        assert_eq!(run("fn kind(x) = match x { 0 => \"zero\"  \"a\" => \"text\"  _ => \"other\" }\nlet _ = print(kind(0), kind(\"a\"), kind(5))"), "zero text other\n");
        // a word with no part binds the whole subject to the name after it
        assert_eq!(run("fn yes1(x) = match x { yes p => p[1]  _ => 0 }\nlet _ = print(yes1([true, 5]), yes1([false, 5]))"), "5 0\n");
    }

    #[test]
    fn a_table_with_no_colon_or_while_words_its_messages_and_runs() {
        const FRONT: Front = Front::spelled::<Bare>();
        let src = format!("{CALLS}let _ = print(fix k = 0 up to 3 {{ k + 1 }})");
        let chunk = FRONT.program(&src, Box::new(One)).unwrap_or_else(|e| panic!("{}", interpreter::code::Diagnostic::render_all(&e, &src, None)));
        let mut vm = boot(&chunk, Limits::sandbox());
        assert_eq!(vm.run().map(|_| vm.output_text()).ok().as_deref(), Some("3\n"));
    }

    // the table again with modifier words a type item and an arrow for a result
    const DECL_WORDS: &[(&str, TokenType)] = &[("true", T::True), ("false", T::False), ("or", T::Or), ("in", T::In), ("if", T::If), ("else", T::Else), ("match", T::Match), ("fn", T::Fn), ("let", T::Let), ("for", T::For), ("use", T::Import), ("type", T::Type), ("_", T::Underscore)];
    const DECL_SYMBOLS: &[(&str, TokenType)] = &[
        ("(", T::ParenOpen), (")", T::ParenClose), ("[", T::BracketOpen), ("]", T::BracketClose), ("{", T::BraceOpen), ("}", T::BraceClose), ("{", T::BlockOpen), ("}", T::BlockClose), (":", T::Colon),
        ("=>", T::Arm), ("==", T::EqualEqual), ("<", T::Less), ("=", T::Equal), ("+", T::Plus), ("->", T::Returns), ("-", T::Minus), ("/", T::Slash), (",", T::Comma), (".", T::Dot),
    ];
    static DECL_OPERATORS: [[TokenType; 4]; 128] = operators(DECL_SYMBOLS);
    static DECL_KEYWORDS: Keywords = index(DECL_WORDS);
    static DECL_GRAMMAR: Grammar = Grammar { modes: &[Mode { rows: ROWS, words: &[] }], modifiers: &[("pub", None), ("traced", Some("trace"))], ..Grammar::NONE };
    struct Decls;
    impl Spelling for Decls {
        const SYNTAX: Syntax = Syntax { words: DECL_WORDS, symbols: DECL_SYMBOLS, operators: &DECL_OPERATORS, keywords: &DECL_KEYWORDS, shapes: Shapes { grammar: Some(&DECL_GRAMMAR), ..SYNTAX.shapes }, ..SYNTAX };
        const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
    }

    #[test]
    fn a_read_keeps_each_item_with_its_modifiers_and_node_and_each_annotation_with_its_function() {
        use interpreter::code::{Annotated, Annotation, Decl, DeclKind, NodeKind as K};
        let src = "pub traced fn double(x: num) -> num = x + x\ntype Pair = [num, num]\npub let two = 2\nlet _ = { let r = { a = two, b = (two) }\n double(r.a)[0] }\n";
        let (chunk, errors) = interpreter::code::read_as::<Decls>(src);
        assert!(errors.is_empty(), "{}", interpreter::code::Diagnostic::render_all(&errors, src, None));
        let at = |text: &str, k: usize| src.match_indices(text).nth(k).map_or(0, |m| m.0);
        let outline: Vec<_> = chunk.notes().outline.iter().map(|n| (n.kind, &src[n.span.clone()], n.parent)).collect();
        assert_eq!(outline, [
            (K::Function, "pub traced fn double(x: num) -> num = x + x", None),
            (K::Type, "type Pair = [num, num]", None),
            (K::Let, "pub let two = 2", None),
            (K::Let, "let _ = { let r = { a = two, b = (two) }\n double(r.a)[0] }", None),
            (K::Block, "{ let r = { a = two, b = (two) }\n double(r.a)[0] }", Some(3)),
            (K::Let, "let r = { a = two, b = (two) }", Some(4)),
            (K::Record, "{ a = two, b = (two) }", Some(5)),
            (K::Group, "(two)", Some(6)),
            (K::Index, "double(r.a)[0]", Some(4)),
            (K::Call, "double(r.a)", Some(8)),
            (K::Field, "r.a", Some(9)),
        ]);
        let decl = |span, kind, name: &str, modifiers: &[&str], node| Decl { span, kind, name: name.into(), modifiers: modifiers.iter().map(|m| m.to_string()).collect(), node };
        assert_eq!(chunk.notes().decls, [
            decl(0..at("(", 0), DeclKind::Fn, "double", &["pub", "traced"], 0),
            decl(at("type", 0)..at(" =", 1), DeclKind::Type, "Pair", &[], 1),
            decl(at("pub", 1)..at(" =", 2), DeclKind::Let, "two", &["pub"], 2),
            decl(at("let _", 0)..at("let _", 0) + 5, DeclKind::Let, "_", &[], 3),
        ]);
        assert_eq!(chunk.notes().annotations, [
            Annotation { span: at("num", 0)..at("num", 0) + 3, of: Annotated::Param { function: 0, name: "x".into() } },
            Annotation { span: at("num", 1)..at("num", 1) + 3, of: Annotated::Returns { function: 0 } },
        ]);
        // a compile of the same source keeps none of it
        assert_eq!(interpreter::code::compile(&Front::spelled::<Decls>(), src).0.notes(), &interpreter::code::Notes::default());
    }

    // two functions of one name each keep their own annotations as the rig reads them past its grammar kit
    #[test]
    fn an_annotation_names_its_function_by_its_node_so_two_of_one_name_stay_apart() {
        use interpreter::code::{Annotated, Annotation, NodeKind};
        let src = "fn add(a: int, b: [int]) -> int {\n    return a\n}\nfn add(c: str) {\n    return c\n}\ncount: int = 1\n";
        let (chunk, errors) = interpreter::code::read_as::<rig::Rig>(src);
        assert!(errors.is_empty(), "{}", interpreter::code::Diagnostic::render_all(&errors, src, None));
        let notes = chunk.notes();
        let functions: Vec<usize> = (0..notes.outline.len()).filter(|&i| notes.outline[i].kind == NodeKind::Function).collect();
        assert_eq!(functions.iter().map(|&i| &src[notes.outline[i].span.clone()]).collect::<Vec<_>>(), ["fn add(a: int, b: [int]) -> int {\n    return a\n}", "fn add(c: str) {\n    return c\n}"]);
        let span = |text: &str, k: usize| src.match_indices(text).nth(k).map_or(0..0, |m| m.0..m.0 + text.len());
        let param = |function: usize, name: &str| Annotated::Param { function, name: name.into() };
        assert_eq!(notes.annotations, [
            Annotation { span: span("int", 0), of: param(functions[0], "a") },
            Annotation { span: span("[int]", 0), of: param(functions[0], "b") },
            Annotation { span: span("int", 2), of: Annotated::Returns { function: functions[0] } },
            Annotation { span: span("str", 0), of: param(functions[1], "c") },
            Annotation { span: span("int", 3), of: Annotated::Binding { target: "count".into() } },
        ]);
    }

    // the rig's table read by the engine's own operators where no grammar kit takes values
    struct Plain;
    impl Spelling for Plain {
        const SYNTAX: Syntax = Syntax { shapes: Shapes { grammar: None, ..rig::RIG.shapes }, ..rig::RIG };
        const NATIVES: &'static interpreter::lang::Natives = &rig::NATIVES;
    }

    /* the bodies of `chunk` with each instruction's span checked to sit in `src` */
    fn spanned_bodies(chunk: &interpreter::code::SSAChunk, src: &str) -> usize {
        for ip in 0..chunk.instructions.len() as u32 {
            let span = chunk.span(ip).unwrap_or_else(|| panic!("{:?} at {ip} of {} has no span", chunk.instructions[ip as usize].opcode, chunk.qualname));
            assert!(span.start <= span.end && span.end <= src.len(), "{span:?}");
        }
        1 + chunk.functions.iter().map(|f| spanned_bodies(&f.1, src)).sum::<usize>() + chunk.classes.iter().map(|c| spanned_bodies(c, src)).sum::<usize>()
    }

    /* the code of `chunk` and every body under it */
    fn code(chunk: &interpreter::code::SSAChunk) -> String {
        format!("{:?}", chunk.instructions) + &chunk.functions.iter().map(|f| code(&f.1)).chain(chunk.classes.iter().map(code)).collect::<String>()
    }

    /* every instruction of every body has a span and each node sits inside its parent apart from its siblings */
    fn well_kept(chunk: &interpreter::code::SSAChunk, src: &str) -> usize {
        let bodies = spanned_bodies(chunk, src);
        let outline = &chunk.notes().outline;
        for (i, n) in outline.iter().enumerate() {
            if let Some(p) = n.parent {
                assert!(p < i && outline[p].span.start <= n.span.start && n.span.end <= outline[p].span.end, "{:?} {:?} sits outside {:?}", n.kind, &src[n.span.clone()], &src[outline[p].span.clone()]);
            }
            let next = outline[i + 1..].iter().find(|m| m.parent == n.parent);
            assert!(next.is_none_or(|m| n.span.end <= m.span.start), "{:?} {:?} overlaps a sibling", n.kind, &src[n.span.clone()]);
        }
        bodies
    }

    #[test]
    fn every_instruction_of_a_read_has_a_span_and_each_node_sits_in_its_parent_apart_from_its_siblings() {
        let plain = "fn scale(xs, k = 2, *rest) {\n    ys = [x * k for x in xs if x > 0 and not x == 3]\n    pairs = {\"a\": (1, 2), \"b\": -k}\n    ys[0] += pairs[\"a\"][1:]\n    f = |y| y.real + 1\n    try { throw f(ys) } catch e { return e }\n    return f(k=1), {ys[0], 4}, \"{k} of {xs}\"\n}\nfor i, v in scale([1, 2]) {\n    while i < v { i = i + (v ~/ 2) }\n}\n";
        let (chunk, errors) = interpreter::code::read_as::<Plain>(plain);
        assert!(errors.is_empty(), "{}", interpreter::code::Diagnostic::render_all(&errors, plain, None));
        assert!(well_kept(&chunk, plain) >= 4);
        assert_eq!(code(&chunk), code(&interpreter::code::compile_as::<Plain>(plain).0));
        let rig = "fn add(a: int, b: [int]) -> int {\n    return a + b[0]\n}\nxs = [1, (2), (3, 4)]\nif add(1, xs).y {\n    count: int = xs.len()\n}\nf = lift k: k + 1\nwhile xs { xs = xs[1:] }\nm = match xs { [a, _] => a  _ => gather { 1; for x in xs { x } } }\n";
        let (chunk, errors) = interpreter::code::read_as::<rig::Rig>(rig);
        assert!(errors.is_empty(), "{}", interpreter::code::Diagnostic::render_all(&errors, rig, None));
        assert!(well_kept(&chunk, rig) >= 3);
        assert_eq!(code(&chunk), code(&interpreter::code::compile_as::<rig::Rig>(rig).0));
        let decls = "pub traced fn double(x: num) -> num = x + x\nlet _ = { let r = { a = two, b = (two) }\n if r.a == 1 { double(r.a)[0] } else { fn (y) = y + 1 } }\n";
        let (chunk, errors) = interpreter::code::read_as::<Decls>(decls);
        assert!(errors.is_empty(), "{}", interpreter::code::Diagnostic::render_all(&errors, decls, None));
        assert!(well_kept(&chunk, decls) >= 2);
        assert_eq!(code(&chunk), code(&interpreter::code::compile_as::<Decls>(decls).0));
    }
}

