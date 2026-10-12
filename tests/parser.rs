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
                    Value::LongInt(i) => i.to_string(),
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

    // Deep prefix / right-associative operator chains must hit the expr-depth guard and error, not overflow the parser's native/WASM stack.
    #[test]
    fn deep_operator_chains_error_not_overflow() {
        let check = |src: String, label: &str| {
            let (tokens, _) = lex(&src);
            let (_chunk, diagnostics) = Parser::new(&src, tokens.into_iter()).parse();
            assert!(
                diagnostics.iter().any(|d| d.msg.contains("too deeply nested")),
                "{}: expected 'too deeply nested', got {:?}",
                label,
                diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
            );
        };
        // Borrow tracking makes lexing quadratic under Miri, so there a chain stops a few times past the guard.
        let scale = if cfg!(miri) { 1 } else { 100 };
        check(format!("x = {}1", "-".repeat(2_000 * scale)), "unary minus");
        check(format!("x = {}1", "+".repeat(2_000 * scale)), "unary plus");
        check(format!("x = {}1", "~".repeat(2_000 * scale)), "bitwise not");
        check(format!("x = {}True", "not ".repeat(1_000 * scale)), "logical not");
        check(format!("x = 2{}", "**2".repeat(1_000 * scale)), "right-assoc pow");
        check(format!("async def f():\n    return {}g()", "await ".repeat(1_000 * scale)), "await");
    }
}
