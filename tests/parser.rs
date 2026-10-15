#[cfg(test)]
mod test {

    use compiler::lexer::lex;
    use compiler::parser::{Parser, Value, Diagnostic};

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Case {
        src: String,
        constants: Vec<String>,
        names: Vec<String>,
        instructions: Vec<(String, u16)>,
        #[serde(default)]
        functions: usize,
        #[serde(default)]
        classes: usize,
        #[serde(default)]
        errors: Vec<String>,
    }

    #[test]
    fn test_cases() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("cases/parser.json")).expect("invalid JSON");

        for case in cases {
            let (tokens, lex_errs) = lex(&case.src);
            let mut parser = Parser::new(&case.src, tokens.into_iter());
            for e in lex_errs { parser.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }); }
            let (chunk, diagnostics) = parser.parse();

            let constants: Vec<String> = chunk
                .constants
                .iter()
                .map(|v| match v {
                    Value::Str(s) => s.clone(),
                    Value::Bytes(b) => format!("b{:?}",
                    String::from_utf8_lossy(b).to_string()),
                    Value::Int(i) => i.to_string(),
                    Value::BigInt(b) => format!("{}{}", if b.is_neg() { "-" } else { "" }, b.to_radix(10)),
                    Value::Float(f) => f.to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::None => "None".to_string(),
                })
                .collect();

            let instructions: Vec<(String, u16)> = chunk
                .instructions
                .iter()
                // Debug-format opcodes for snapshot comparison (test-only).
                .map(|i| (format!("{:?}", i.opcode), i.operand))
                .collect();

            assert_eq!(
                constants, case.constants,
                "constants mismatch on: {:?}",
                case.src
            );
            assert_eq!(chunk.names, case.names, "names mismatch on: {:?}", case.src);
            assert_eq!(
                instructions, case.instructions,
                "bytecode mismatch on: {:?}",
                case.src
            );
            assert_eq!(
                chunk.functions.len(),
                case.functions,
                "functions mismatch on: {:?}",
                case.src
            );
            assert_eq!(
                chunk.classes.len(),
                case.classes,
                "classes mismatch on: {:?}",
                case.src
            );

            if !case.errors.is_empty() {
                let actual: Vec<String> = diagnostics.iter().map(|e| e.msg.clone()).collect();
                assert_eq!(actual, case.errors, "errors mismatch on: {:?}", case.src);
            }
        }
    }

    // Overflow past MAX_INSTRUCTIONS + a trailing `with` must error, not panic on a stale jump index.
    #[test]
    #[cfg_attr(miri, ignore = "lexing 160 KB under Miri's borrow tracking takes longer than a job")]
    fn instruction_overflow_does_not_panic() {
        let mut src = String::with_capacity(180_000);
        for _ in 0..40_000 { src.push_str("a=1\n"); }
        src.push_str("with a as b:\n    pass\n");
        let (tokens, _) = lex(&src);
        let (_chunk, diagnostics) = Parser::new(&src, tokens.into_iter()).parse();
        assert!(
            diagnostics.iter().any(|d| d.msg.contains("program too large")),
            "expected 'program too large', got {:?}",
            diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
        );
    }

    // A multibyte identifier past the SSA name buffer must truncate on a codepoint boundary.
    #[test]
    fn long_multibyte_names_stay_valid_utf8() {
        let name = "\u{4E2D}".repeat(43);
        let src = format!("{name} = 1\n{name} = 2\nprint({name})\n");
        let (tokens, _) = lex(&src);
        let (chunk, _) = Parser::new(&src, tokens.into_iter()).parse();
        for n in &chunk.names {
            assert!(core::str::from_utf8(n.as_bytes()).is_ok(), "invalid utf8 in SSA name {n:?}");
        }
    }

    /* The diagnostics `src` compiles to. */
    fn messages(src: &str) -> Vec<String> {
        let (tokens, _) = lex(src);
        Parser::new(src, tokens.into_iter()).parse().1.into_iter().map(|d| d.msg).collect()
    }

    // Every deep nesting must hit the depth guard and error, not overflow the native or WASM stack of the parser.
    #[test]
    fn deep_operator_chains_error_not_overflow() {
        let check = |src: String, label: &str| {
            let got = messages(&src);
            assert!(got.iter().any(|m| m.contains("too deeply nested")), "{label}: expected 'too deeply nested', got {got:?}");
        };
        // Borrow tracking makes lexing quadratic under Miri, so there a chain stops a few times past the guard.
        let scale = if cfg!(miri) { 1 } else { 100 };
        check(format!("x = {}1", "-".repeat(2_000 * scale)), "unary minus");
        check(format!("x = {}1", "+".repeat(2_000 * scale)), "unary plus");
        check(format!("x = {}1", "~".repeat(2_000 * scale)), "bitwise not");
        check(format!("x = {}True", "not ".repeat(1_000 * scale)), "logical not");
        check(format!("x = 2{}", "**2".repeat(1_000 * scale)), "right-assoc pow");
        check(format!("async def f():\n    return {}g()", "await ".repeat(1_000 * scale)), "await");
        check(format!("x = {}1", "0 if c else ".repeat(1_000 * scale)), "chained conditional");
        check(format!("{}a{} = 1", "(".repeat(1_000 * scale), ")".repeat(1_000 * scale)), "nested target");
        check(format!("match x:\n    case {}y{}:\n        pass\n", "[".repeat(1_000 * scale), "]".repeat(1_000 * scale)), "nested pattern");
        // A comparison chain stays flat, so any length compiles.
        assert_eq!(messages(&format!("x = 1{}", " < 1".repeat(100 * scale))), Vec::<String>::new(), "comparison chain");
    }

    // A compound statement never follows the colon of another on its line, however long the chain.
    #[test]
    fn compound_after_colon_errors() {
        let scale = if cfg!(miri) { 1 } else { 100 };
        let chain = format!("{}pass", "if c: ".repeat(1_000 * scale));
        for src in ["if a: if b: pass", "def f(): def g(): pass", "for x in y: if x: pass", "class A: def f(self): pass", "while c: with x: pass", chain.as_str()] {
            assert_eq!(messages(src), ["a compound statement must start its own line"], "{src:?}");
        }
        assert_eq!(messages("if a: x = 1; y = 2\nfor i in r: print(i)\ndef f(n): return n"), Vec::<String>::new());
    }

    // A report shows 20 errors cut to the columns around each, however many a long line holds.
    #[test]
    fn long_reports_stay_small() {
        let src = "$".repeat(if cfg!(miri) { 400 } else { 40_000 });
        let (tokens, lex_errs) = lex(&src);
        let mut parser = Parser::new(&src, tokens.into_iter());
        for e in lex_errs { parser.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }); }
        let errs = parser.parse().1;
        let out = Diagnostic::render_all(&errs, &src, None);
        assert!(out.len() < 4_000 && out.ends_with(&format!("and {} more errors\n", errs.len() - 20)), "{} bytes", out.len());
        let one = Diagnostic { start: 300, end: 301, msg: "x".into() }.render(&src, None);
        let lines: Vec<&str> = one.lines().collect();
        assert_eq!((lines[1], lines[3].len(), lines[4].find('^')), ("  --> <input>:1:301", 4 + 3 + 80 + 3, Some(4 + 3 + 40)), "{one}");
        assert_eq!(messages(&format!("x = 1 {}", "a".repeat(1_000))), [format!("expected newline, got '{}...'", "a".repeat(40))]);
    }

    // Broken statements the fuzzer saw loop forever, each must finish with its diagnostics.
    #[test]
    fn broken_statements_finish() {
        for src in ["(] =')\n}] =')", "x = [2, 3, 4]\nx[:; )= 10\nprint(x)", "[n) = [9)bbbbbb, #]\nx3, #]\n] = [5]\ndef f(bbbbbbb"] {
            assert!(!messages(src).is_empty(), "{src:?}");
        }
    }

    // A comprehension cannot yield, in any of its four forms.
    #[test]
    fn yield_inside_comprehension_errors() {
        for src in ["def g():\n    x = [(yield i) for i in range(3)]", "def g():\n    x = {(yield i) for i in y}", "def g():\n    x = {i: (yield) for i in y}", "def g():\n    x = list((yield i) for i in y)"] {
            assert!(messages(src).iter().any(|m| m == "'yield' inside a comprehension"), "{src:?}");
        }
    }
}
