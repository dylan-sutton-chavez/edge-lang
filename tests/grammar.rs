#[cfg(test)]
mod test {
    use crate::common::boot;
    use edge::code::OpCode as O;
    use edge::embed::Limits;
    use edge::lang::Front;
    use edge::lexer::grammar::{Emit, Form, Grammar, Mode, Pattern, Row, Slot, Test};
    use edge::lexer::TokenType as T;
    use edge::lexer::{index, operators, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Keywords, Lambda, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros};

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
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // a form's call is a function of the script so each test sees what the slots gave
    const CALLS: &str = "fn steps(x, n, f) = if n == 0 or f(x) == x { x } else { steps(f(x), n - 1, f) }\nfn trio(a, b, c) = [a, b, c]\nfn pick(n, g) = g\n";

    // a module the tests import by the name m
    struct One;
    impl edge::deps::Resolver for One {
        fn resolve(&mut self, spec: &str) -> Result<edge::deps::Resolved, String> {
            if spec == "m" { Ok(edge::deps::Resolved::Code { src: "let x = 5\n".into(), canonical: "m".into() }) } else { Err(format!("no module {spec}")) }
        }
        fn child(&self, _: &str) -> Box<dyn edge::deps::Resolver> { Box::new(One) }
    }

    fn run(src: &str) -> String {
        const FRONT: Front = Front::spelled::<Forms>();
        let src = format!("{CALLS}{src}");
        let chunk = FRONT.program(&src, Box::new(One)).unwrap_or_else(|e| panic!("{}", edge::code::Diagnostic::render_all(&e, &src, None)));
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
        assert_eq!(run(src), format!("[{}, 1, [['wide', 1], ['work', 4]]]\n[{}, 2, [['wide', 1]]]\n", at(0), at(1)));
    }

    #[test]
    fn a_lazy_name_reads_its_lone_comprehension_item_by_item() {
        assert_eq!(run("let _ = print(any([n == 0 or 1 / (n - n) == 1 for n in [0, 1]]))"), "True\n");
        // a comprehension that is not the whole argument and a name off the list stay lists
        assert!(run("let _ = print(any([n == 0 or 1 / (n - n) == 1 for n in [0, 1]] + []))").contains("ZeroDivisionError"));
        assert!(run("let _ = print(pick(1, [n for n in [1]]))").contains("generator"));
        assert!(run("let _ = print(all([n == 1 or 1 / (n - n) == 1 for n in [0, 1]]))").contains("ZeroDivisionError"));
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
}
