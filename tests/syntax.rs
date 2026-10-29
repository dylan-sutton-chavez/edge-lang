#[cfg(test)]
mod test {

    use edge::code::{Parser, SSAChunk};
    use edge::lexer::{index, lex_as, operators, Binding, Blocks, Comments, Cond, Continuation, Ends, Interp, Keywords, Lambda, Literal, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Zeros, RAW};
    use edge::deps::NoopResolver;
    use mini::{Mini, MINI};
    use spell::{Braces, BRACES};
    use spell::{Indent, Words, INDENT, WORDS};

    // the chunk a source compiles to unfolded and every message its lexer and parser report
    fn compiled<S: Spelling>(src: &str) -> (SSAChunk, Vec<String>) {
        let (tokens, lex_errs) = lex_as::<S>(src);
        let (chunk, errs) = Parser::<_, S>::spelled(src, tokens.into_iter(), Box::new(NoopResolver)).parse();
        (chunk, lex_errs.iter().map(|e| e.msg.to_string()).chain(errs.into_iter().map(|d| d.msg)).collect())
    }

    fn kinds<S: Spelling>(src: &str) -> Vec<TokenType> {
        lex_as::<S>(src).0.iter().map(|t| t.kind).filter(|k| !matches!(k, TokenType::Newline | TokenType::LineBreak | TokenType::Eof)).collect()
    }

    // each word and symbol of a table lexes back to its token where context allows
    fn spells_its_tokens<S: Spelling>(table: Syntax) {
        let soft = |k: TokenType| table.is_soft(k);
        // a block word needs a header as a block brace does
        let bound = |k: TokenType| matches!(k, TokenType::BlockOpen | TokenType::BlockClose);
        for &(word, kind) in table.words.iter().filter(|(_, k)| !soft(*k) && !bound(*k)) {
            let mut got = kinds::<S>(word);
            // a words table opens the block of `else` or `try` at the word itself
            if matches!(table.blocks, Blocks::Words { .. }) && got.len() == 2 && got[1] == TokenType::BlockOpen { got.pop(); }
            assert_eq!(got, [kind], "{} word {word:?}", table.name);
        }
        // a block brace needs a header and a field conversion an f-string
        let contextual = |k: TokenType| matches!(k, TokenType::BlockOpen | TokenType::BlockClose) || (k == TokenType::Convert && table.convert_shared());
        for &(symbol, kind) in table.symbols.iter().filter(|(_, k)| !contextual(*k)) {
            assert_eq!(kinds::<S>(symbol).first(), Some(&kind), "{} symbol {symbol:?}", table.name);
        }
    }

    #[test]
    fn tables_spell_their_tokens() {
        spells_its_tokens::<Indent>(INDENT);
        spells_its_tokens::<Braces>(BRACES);
        spells_its_tokens::<Mini>(MINI);
        spells_its_tokens::<Words>(WORDS);
        // the index of each table agrees with its word list
        let words = INDENT.words.iter().chain(MINI.words).chain(WORDS.words).map(|(w, _)| *w).chain(["Def", "de", "defs", "trueish", "nil", "x", "ends", "d"]);
        for w in words.filter(|w| !w.contains(' ')) {
            for table in [INDENT, BRACES, MINI, WORDS] {
                let listed = table.words.iter().find(|(v, _)| *v == w).map(|&(_, k)| k);
                assert_eq!(table.word(w), listed, "{} {w:?}", table.name);
            }
        }
        assert!(!INDENT.braces() && !INDENT.phrases() && INDENT.pairs() && !INDENT.convert_shared());
        assert!(BRACES.braces() && !MINI.braces() && MINI.phrases() && MINI.pairs() && MINI.convert_shared());
        // both tables open phrases only at `else` and pair scan the same three symbols
        let set = |t: [bool; 128]| (0..128).filter(|&i| t[i]).collect::<Vec<_>>();
        for table in [MINI, BRACES] {
            assert_eq!((set(table.heads()), set(table.pair_starts())), (vec![TokenType::Else as usize], vec![b'-' as usize, b'.' as usize, b'~' as usize]));
        }
        assert!(set(INDENT.heads()).is_empty() && set(INDENT.pair_starts()) == [b'-' as usize, b'.' as usize]);
        // the words table opens its two phrases at `is` and `not`
        assert!(!WORDS.braces() && WORDS.delimited() && WORDS.phrases() && WORDS.pairs() && !WORDS.convert_shared());
        assert_eq!(set(WORDS.heads()), [TokenType::Is as usize, TokenType::Not as usize]);
    }

    // an index finds every word of any table whole and nothing else
    #[test]
    fn an_index_finds_whole_words_of_any_length() {
        use edge::lexer::{index, Keywords};
        use TokenType as T;
        const WORDS: &[(&str, TokenType)] = &[
            ("si", T::If), ("sino", T::Else), ("sino si", T::ElseIf), ("en", T::In), ("es", T::Is), ("e", T::And), ("_", T::Underscore),
            ("mientras", T::While), ("finalmente", T::Finally), ("función", T::Fn), ("ñu", T::Empty), ("en", T::Not),
        ];
        static WORDS_INDEX: Keywords = index(WORDS);
        for (w, k) in WORDS.iter().filter(|(w, _)| !w.contains(' ')) {
            let first = WORDS.iter().find(|(v, _)| v == w).map(|&(_, k)| k);
            assert_eq!(WORDS_INDEX.find(w.as_bytes()), first, "{w:?} is {k:?}");
        }
        for name in ["", "s", "sin", "sinos", "sino si", "ei", "eN", "mientra", "mientrass", "finalment", "finalmentes", "funcion", "ñ", "ñ_", "x", "__"] {
            assert_eq!(WORDS_INDEX.find(name.as_bytes()), None, "{name:?}");
        }
    }

    // imports read in a spelling take its suffix and skip its comments
    #[test]
    fn imports_follow_the_spelling() {
        use edge::deps::{scan_imports, ImportSpec::*};
        let src = "from .a import x\nimport b.c, d // import e\n";
        assert_eq!(scan_imports::<Mini>(src), [Relative("./a.mini".into()), Root("b/c.mini".into()), Bare("d".into())]);
        assert_eq!(scan_imports::<Indent>("from .a import x\nimport b.c # import e\n"), [Relative("./a.py".into()), Root("b/c.py".into())]);
        let resolver = crate::common::TestResolver::new().with_code("./a.mini", "x = 1\n");
        let (tokens, _) = lex_as::<Mini>("from .a import x\n");
        let (_, errs) = Parser::<_, Mini>::spelled("from .a import x\n", tokens.into_iter(), Box::new(resolver)).parse();
        assert!(errs.is_empty(), "{:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
    }

    // a run of dots climbs a directory per dot past the first however it lexes
    #[test]
    fn a_run_of_dots_climbs_however_it_lexes() {
        use edge::deps::{scan_imports, ImportSpec::*};
        let src = "from ..a import x\nfrom ...b.c import y\nfrom ......d import z\n";
        assert_eq!(scan_imports::<Mini>(src), [Relative("../a.mini".into()), Relative("../../b/c.mini".into()), Relative("../../../../../d.mini".into())]);
    }

    #[test]
    fn braces_tell_blocks_from_displays() {
        use TokenType::*;
        let cases: &[(&str, &[TokenType])] = &[
            ("if x == {} { y = {1} }", &[If, Name, EqualEqual, BraceOpen, BraceClose, BlockOpen, Name, Equal, BraceOpen, Int, BraceClose, BlockClose]),
            ("x = a if b else {}", &[Name, Equal, Name, If, Name, Else, BraceOpen, BraceClose]),
            ("if a {} else if c {}", &[If, Name, BlockOpen, BlockClose, ElseIf, Name, BlockOpen, BlockClose]),
            ("if a {} else {}", &[If, Name, BlockOpen, BlockClose, Else, BlockOpen, BlockClose]),
            ("case {\"k\": v} {", &[Case, BraceOpen, String, Colon, Name, BraceClose, BlockOpen]),
            ("for x in 7, {\n}", &[For, Name, In, Int, Comma, BlockOpen, BlockClose]),
            ("x = !a && b || c ~/ 2 // note", &[Name, Equal, Not, Name, And, Name, Or, Name, FloorDiv, Int, Comment]),
            ("f\"{x!r} {!x}\"", &[FormatStart, BraceOpen, Name, Convert, Name, BraceClose, FormatText, BraceOpen, Not, Name, BraceClose, FormatEnd]),
            ("fn fn() { fn = nil }", &[Fn, Name, ParenOpen, ParenClose, BlockOpen, Name, Equal, Nil, BlockClose]),
            ("x = {\n  1: 2,\n}", &[Name, Equal, BraceOpen, Int, Colon, Int, Comma, BraceClose]),
            // a doubled symbol with no `=` form leaves the `=` to its own token
            ("a &&= b || c", &[Name, And, Equal, Name, Or, Name]),
            // the lead a trailing comma asks for is the statement's own after every new line
            ("x = 1, {\n}, {\n}\nfor a in b, {\n}", &[Name, Equal, Int, Comma, BraceOpen, BraceClose, Comma, BraceOpen, BraceClose, For, Name, In, Name, Comma, BlockOpen, BlockClose]),
        ];
        for (src, want) in cases {
            assert_eq!(kinds::<Braces>(src), *want, "{src:?}");
        }
    }

    // the brace bits keep every brace past one word and a block above them
    #[test]
    fn braces_count_past_a_word() {
        use TokenType::*;
        const DEEP: usize = 70;
        let src = format!("x = {}{}\nif y {{ z }}\n", "{".repeat(DEEP), "}".repeat(DEEP));
        let want: Vec<TokenType> = [Name, Equal].into_iter().chain([BraceOpen; DEEP]).chain([BraceClose; DEEP]).chain([If, Name, BlockOpen, Name, BlockClose]).collect();
        assert_eq!(kinds::<Braces>(&src), want);
        let src = format!("{}{}if y {{ z }} }}", "{".repeat(DEEP), ")".repeat(DEEP));
        let want: Vec<TokenType> = [BraceOpen; DEEP].into_iter().chain([ParenClose; DEEP]).chain([If, Name, BlockOpen, Name, BlockClose, BraceClose]).collect();
        assert_eq!(kinds::<Braces>(&src), want);
    }

    // a table with no comment marker and a symbol longer than the arrow
    struct Odd;
    const ODD_SYMBOLS: &[(&str, TokenType)] = &[("->>", TokenType::RightShift), ("->", TokenType::Returns), ("-", TokenType::Minus), ("=", TokenType::Equal), (">", TokenType::Greater)];
    static ODD_OPERATORS: [[TokenType; 4]; 128] = edge::lexer::operators(ODD_SYMBOLS);
    impl Spelling for Odd {
        const SYNTAX: Syntax = Syntax { name: "odd", symbols: ODD_SYMBOLS, operators: &ODD_OPERATORS, comments: Comments { line: &[], block: None, nested: false }, ..INDENT };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    #[test]
    fn a_table_lexes_its_longest_symbol_and_may_have_no_comments() {
        use TokenType::*;
        assert_eq!(kinds::<Odd>("a ->> b -> c - d\n"), [Name, RightShift, Name, Returns, Name, Minus, Name]);
        let (tokens, errors) = lex_as::<Odd>("x = 1 # y\n\nz\n");
        assert_eq!(tokens.iter().map(|t| t.kind).filter(|k| !matches!(k, Newline | LineBreak | Eof)).collect::<Vec<_>>(), [Name, Equal, Int, Name, Name]);
        assert_eq!(errors.iter().map(|e| e.msg).collect::<Vec<_>>(), ["unexpected character"]);
    }

    #[test]
    fn braces_report_in_their_own_spelling() {
        let errors = |src: &str| compiled::<Braces>(src).1;
        assert_eq!(errors("if x\n    y\n"), ["expected '{', got 'y'"]);
        assert_eq!(errors("if x: y\n"), ["expected '{', got ':'"]);
        assert_eq!(errors("if x {\n    if y {\n        z\n"), ["'{' was never closed"]);
        assert_eq!(errors("try { a } except { b } except E { c }\n"), ["default 'except' must be last"]);
        assert_eq!(errors("fn f() { return 1 } x = 2\n"), ["expected newline, got 'x'"]);
        assert_eq!(errors("# x\n"), ["unexpected character"]);
        assert_eq!(errors(&"if x {\n".repeat(101)).first().map(String::as_str), Some("block nesting depth exceeds maximum (100)"));
        assert!(errors("fn f() { if x { return } }\nwhile true { break }\n").is_empty());
    }

    // the braces table whose statements end only at semicolons and block ends
    struct Semi;
    impl Spelling for Semi {
        const SYNTAX: Syntax = Syntax { name: "semi", ends: Ends::Semicolon, ..BRACES };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // the words table with explicit block words and comments that do not nest
    struct Flat;
    impl Spelling for Flat {
        const SYNTAX: Syntax = Syntax {
            name: "flat",
            blocks: Blocks::Words { implicit: false },
            comments: Comments { line: &["--"], block: Some(("/*", "*/")), nested: false },
            ..WORDS
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // numbers with a quote between digits and octal zeros and dropped suffixes
    struct Digits;
    const DOUBLE: &[Quote] = &[Quote { byte: b'"', multiline: false, triple: false }];
    impl Spelling for Digits {
        const SYNTAX: Syntax = Syntax {
            name: "digits",
            strings: Strings { quotes: DOUBLE, ..mini::STRINGS },
            numbers: Numbers { radixes: &[(b'x', 16)], separator: Some(b'\''), zeros: Zeros::Octal, leading_dot: false, trailing_dot: false, exponent: false, suffix: Suffix::Ignored },
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // backquoted templates with dollar fields and no specs
    struct Backquote;
    const BACKQUOTES: &[Quote] = &[Quote { byte: b'"', multiline: false, triple: false }, Quote { byte: b'`', multiline: true, triple: false }];
    impl Spelling for Backquote {
        const SYNTAX: Syntax = Syntax {
            name: "backquote",
            strings: Strings { quotes: BACKQUOTES, prefixes: &[("r", RAW)], ..mini::STRINGS },
            interp: Some(Interp { name: "template", quotes: b"`", open: "${", close: b'}', literal: Literal::Backslash, short: None, spec: None, convert: None, echo: None }),
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // fields a backslash and parenthesis open in every double quoted literal
    struct Paren;
    impl Spelling for Paren {
        const SYNTAX: Syntax = Syntax {
            name: "paren",
            strings: Strings { quotes: DOUBLE, prefixes: &[], ..mini::STRINGS },
            interp: Some(Interp { name: "string", quotes: b"\"", open: "\\(", close: b')', literal: Literal::Backslash, short: None, spec: None, convert: None, echo: None }),
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // bare dollar names and braced dollar fields
    struct Dollar;
    impl Spelling for Dollar {
        const SYNTAX: Syntax = Syntax {
            name: "dollar",
            strings: Strings { quotes: DOUBLE, prefixes: &[], ..mini::STRINGS },
            interp: Some(Interp { name: "string template", quotes: b"\"", open: "${", close: b'}', literal: Literal::Backslash, short: Some(b'$'), spec: None, convert: None, echo: None }),
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // three byte symbols for identity and a one word membership test with a flat ladder
    struct Strict;
    const STRICT_SYMBOLS: &[(&str, TokenType)] = &[
        ("===", TokenType::Is), ("!==", TokenType::IsNot), ("**", TokenType::Pow), ("*", TokenType::Star), ("+", TokenType::Plus),
        ("<", TokenType::Less), ("=", TokenType::Equal), ("(", TokenType::ParenOpen), (")", TokenType::ParenClose), (",", TokenType::Comma),
        ("[", TokenType::BracketOpen), ("]", TokenType::BracketClose), (":", TokenType::Colon), ("!", TokenType::Not),
    ];
    static STRICT_OPERATORS: [[TokenType; 4]; 128] = operators(STRICT_SYMBOLS);
    const STRICT_WORDS: &[(&str, TokenType)] = &[("notin", TokenType::NotIn), ("in", TokenType::In), ("if", TokenType::If), ("else", TokenType::Else), ("nil", TokenType::Nil)];
    static STRICT_KEYWORDS: Keywords = index(STRICT_WORDS);
    const STRICT_LADDER: &[(TokenType, u8, u8)] = &[
        (TokenType::Not, 0, 5), (TokenType::Less, 7, 8), (TokenType::Is, 7, 8), (TokenType::IsNot, 7, 8), (TokenType::In, 7, 8), (TokenType::NotIn, 7, 8),
        (TokenType::Star, 17, 18), (TokenType::Plus, 19, 20), (TokenType::Pow, 22, 21),
    ];
    impl Spelling for Strict {
        const SYNTAX: Syntax = Syntax {
            name: "strict",
            words: STRICT_WORDS,
            symbols: STRICT_SYMBOLS,
            operators: &STRICT_OPERATORS,
            keywords: &STRICT_KEYWORDS,
            precedence: STRICT_LADDER,
            chained: false,
            blocks: Blocks::Indent,
            comments: Comments { line: &["#"], block: None, nested: false },
            interp: None,
            strings: Strings { prefixes: &[], ..mini::STRINGS },
            continuation: Continuation { join: None, operator: true },
            soft: &[],
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    fn constants<S: Spelling>(src: &str) -> String {
        let (chunk, errors) = compiled::<S>(src);
        assert!(errors.is_empty(), "{} {src:?} reports {errors:?}", S::SYNTAX.name);
        format!("{:?}", chunk.constants)
    }

    // `src` in table `S` compiles to the program `mini` writes in mini
    fn alike<S: Spelling>(src: &str, mini: &str) {
        let (mut theirs, ours) = (compiled::<S>(src), compiled::<Mini>(mini));
        assert!(theirs.1.is_empty() && ours.1.is_empty(), "{src:?} reports {:?} and {mini:?} reports {:?}", theirs.1, ours.1);
        unpinned(&mut theirs.0);
        if let Err(e) = spell::check::same_program(&ours, &theirs) { panic!("{} {src:?} against {mini:?}: {e}", S::SYNTAX.name); }
    }

    // a range mark calls its native pinned where mini calls it by its name
    fn unpinned(chunk: &mut SSAChunk) {
        for i in chunk.instructions.iter_mut().filter(|i| i.opcode == edge::code::OpCode::CallNative) {
            let Some(site) = edge::code::native_site(i.operand, &chunk.native_sites).filter(|s| s.pinned) else { continue };
            if let Some(plain) = (edge::code::NativeSite { pinned: false, ..site }).in_place() { i.operand = plain; }
        }
        for f in &mut chunk.functions { unpinned(&mut f.1); }
        for c in &mut chunk.classes { unpinned(c); }
    }

    // a range mark builds the runtime's range whatever a module binds its name to
    #[test]
    fn a_range_mark_keeps_its_native_past_a_rebound_name() {
        let src = "range = |*a| [99]\nt = 0\nfor i in 0..3:\n    t += i\nprint(t, range(5), list(1..=2))\n";
        let (chunk, errors) = compiled::<Kit>(src);
        assert!(errors.is_empty(), "{errors:?}");
        let mut vm = crate::common::boot(&chunk, crate::common::corpus_limits());
        vm.run().expect("the program runs");
        assert_eq!(vm.output, ["3 [99] [1, 2]"]);
    }

    // each form compiles to one program in mini and indent and words
    #[test]
    fn forms_compile_alike_in_three_spellings() { super::runner::run("forms.yml") }

    // block words and a header's line end bound the blocks of the words table
    #[test]
    fn words_bound_blocks_with_words_and_line_ends() {
        use TokenType::*;
        let cases: &[(&str, &[TokenType])] = &[
            ("if a then b end", &[If, Name, BlockOpen, Name, BlockClose]),
            ("while a do b end", &[While, Name, BlockOpen, Name, BlockClose]),
            ("if a then b elseif c then d else e end", &[If, Name, BlockOpen, Name, BlockClose, ElseIf, Name, BlockOpen, Name, BlockClose, Else, BlockOpen, Name, BlockClose]),
            ("function f()\n  skip\nend", &[Fn, Name, ParenOpen, ParenClose, BlockOpen, Empty, BlockClose]),
            ("try\n  a\ncatch E\n  b\nend", &[Try, BlockOpen, Name, BlockClose, Catch, Name, BlockOpen, Name, BlockClose]),
            ("match x\n  case 1\n    y\n  end\nend", &[Match, Name, BlockOpen, Case, Int, BlockOpen, Name, BlockClose, BlockClose]),
            ("for x in xs do\n  if x then break end\nend", &[For, Name, In, Name, BlockOpen, If, Name, BlockOpen, Break, BlockClose, BlockClose]),
            // the else of a conditional expression closes no block
            ("x = a if b else c", &[Name, Equal, Name, If, Name, Else, Name]),
            ("if x then y = 1 if a else 2 else y = 3 end", &[If, Name, BlockOpen, Name, Equal, Int, If, Name, Else, Int, BlockClose, Else, BlockOpen, Name, Equal, Int, BlockClose]),
            ("x = (\n  1 if a else 2)", &[Name, Equal, ParenOpen, Int, If, Name, Else, Int, ParenClose]),
            ("x = a is not b and c not in d", &[Name, Equal, Name, IsNot, Name, And, Name, NotIn, Name]),
        ];
        for (src, want) in cases {
            assert_eq!(kinds::<Words>(src), *want, "{src:?}");
        }
        // a table without implicit blocks waits for its block word past the line end
        assert_eq!(kinds::<Flat>("if a\nthen b end"), [If, Name, BlockOpen, Name, BlockClose]);
        assert_eq!(kinds::<Flat>("function f() do skip end"), [Fn, Name, ParenOpen, ParenClose, BlockOpen, Empty, BlockClose]);
        assert_eq!(compiled::<Flat>("function f()\n  skip\nend\n").1, ["expected 'then', got 'skip'"]);
    }

    #[test]
    fn words_report_in_their_own_spelling() {
        let errors = |src: &str| compiled::<Words>(src).1;
        assert_eq!(errors("break\n"), ["'break' outside loop"]);
        assert_eq!(errors("x = await f()\n"), ["'await' outside function"]);
        assert_eq!(errors("outer x\n"), ["outer declaration not allowed at module level"]);
        assert_eq!(errors("throw e from c\n"), ["throw from is not supported"]);
        assert_eq!(errors("try\n  x\ncatch* E\n  y\nend\n"), ["catch* is not supported"]);
        assert_eq!(errors("try\n  a\ncatch\n  b\ncatch E\n  c\nend\n"), ["default 'catch' must be last"]);
        assert_eq!(errors("if x then\n  y\n"), ["'then' was never closed"]);
        assert_eq!(errors("x = 1 end\n"), ["unexpected 'end'"]);
        assert_eq!(errors("function f(x) return x end\n"), ["expected 'then', got 'return'"]);
        assert_eq!(errors("x = \"#{y!z}\"\n"), ["invalid template conversion (expected !r, !s, or !a)"]);
        assert_eq!(errors("x = \"#{y\"\n"), ["unterminated template literal", "template was never closed"]);
        assert_eq!(errors("x = 'a\\qb'\n"), ["invalid escape in string literal"]);
        assert_eq!(errors("x = 1j\n"), ["invalid suffix on numeric literal"]);
        assert_eq!(errors("x = r#\"a\n"), ["unterminated string literal"]);
        assert_eq!(errors("x = /* open\n").first().map(String::as_str), Some("unterminated comment"));
    }

    // a block comment nests only in a table that says so
    #[test]
    fn comments_take_the_markers_of_the_table() {
        use TokenType::*;
        assert_eq!(kinds::<Words>("x = 1 -- note\n/* a /* b */ c */ y = 2"), [Name, Equal, Int, Comment, Comment, Name, Equal, Int]);
        alike::<Words>("x = 1 -- note\n/* a /* b */ c */ y = 2 /* inline */ + 0\n", "x = 1\ny = 2 + 0\n");
        assert_eq!(kinds::<Flat>("x = 1 /* a /* b */ c */ + 2"), [Name, Equal, Int, Comment, Name, Star, Slash, Plus, Int]);
        assert_eq!(compiled::<Flat>("x = 1 /* a /* b */ c */ + 2").1.first().map(|e| e.as_str()), Some("expected newline, got 'c'"));
    }

    #[test]
    fn strings_take_hashes_and_braced_escapes() {
        assert_eq!(constants::<Words>("x = r#\"a\"b\"#"), r#"[Str("a\"b")]"#);
        assert_eq!(constants::<Words>("x = r##\"a\"#b\"##"), r##"[Str("a\"#b")]"##);
        assert_eq!(constants::<Words>("x = '\\u{e9}\\u{1F600}' + \"\\x41\\0\""), r#"[Str("é😀"), Str("A\0")]"#);
        alike::<Words>("x = 'it\\'s' 'a\\tb'\n", "x = 'it\\'s' 'a\\tb'\n");
    }

    // statements end at semicolons and block ends where line ends count for nothing
    #[test]
    fn semicolons_end_statements_where_line_ends_do_not() {
        alike::<Semi>("x = 1 +\n  2;\nif x > 2 {\n  print(x);\n}\nz = [x,\n  x];\n", "x = 1 + 2\nif x > 2:\n  print(x)\nz = [x, x]\n");
        alike::<Semi>("if x { y = 1 }\nz = 2;\nw = 3", "if x: y = 1\nz = 2\nw = 3\n");
        assert_eq!(compiled::<Semi>("x = 1\ny = 2;\n").1, ["expected ';', got 'y'"]);
    }

    #[test]
    fn numbers_read_as_the_table_says() {
        assert_eq!(constants::<Digits>("x = 1'000 + 017 + 0x1F + 10L + 007 + 0"), "[Int(1000), Int(15), Int(31), Int(10), Int(7), Int(0)]");
        // with no exponent the letters after a number are a suffix the table drops
        assert_eq!(constants::<Digits>("x = 1e3"), "[Int(1)]");
        let errors = |src: &str| compiled::<Digits>(src).1;
        assert_eq!(errors("x = .5"), ["expected expression"]);
        assert_eq!(errors("x = 5."), ["expected an attribute name after '.'"]);
        assert_eq!(errors("x = 1'"), ["invalid ''' in numeric literal"]);
        assert_eq!(errors("x = 1''0"), ["consecutive ''' in numeric literal"]);
        assert_eq!(errors("x = 08"), ["invalid digit in integer literal"]);
    }

    // fields open and close with the table's bytes and a short form needs no brackets
    #[test]
    fn interpolation_takes_the_delimiters_of_the_table() {
        alike::<Backquote>("x = `a ${y} b ${z + 1}`", "x = f'a {y} b {z + 1}'");
        alike::<Backquote>("x = `a ${`b ${y}`}`", "x = f'a {f\"b {y}\"}'");
        assert_eq!(constants::<Backquote>("x = `a \\${y} } {`"), r#"[Str("a ${y} } {")]"#);
        assert_eq!(constants::<Backquote>("x = `a\nb` + \"${y}\""), r#"[Str("a\nb"), Str("${y}")]"#);
        assert_eq!(compiled::<Backquote>("x = `a ${y`").1, ["unterminated template literal", "template was never closed"]);
        alike::<Paren>("x = \"a \\(y) b \\(f(z))\"", "x = f\"a {y} b {f(z)}\"");
        assert_eq!(constants::<Paren>("x = \"a \\\\(y) (z)\""), r#"[Str("a \\(y) (z)")]"#);
        alike::<Dollar>("x = \"a $y b ${y + 1} c $\"", "x = f\"a {y} b {y + 1} c $\"");
        alike::<Dollar>("x = \"\\$y $y_z$w\"", "x = f\"$y {y_z}{w}\"");
    }

    // three byte symbols and one word comparisons and a flat ladder from the table
    #[test]
    fn symbols_words_and_ladders_come_from_the_table() {
        use TokenType::*;
        assert_eq!(kinds::<Strict>("a === b !== c notin d ** e"), [Name, Is, Name, IsNot, Name, NotIn, Name, Pow, Name]);
        alike::<Strict>("x = a === b", "x = a is b");
        alike::<Strict>("x = a !== b", "x = a is !b");
        alike::<Strict>("x = a notin b", "x = a ! in b");
        alike::<Strict>("x = !a", "x = !a");
        // a table that chains no comparison reads them left to right
        alike::<Strict>("x = a < b < c", "x = (a < b) < c");
        alike::<Strict>("x = 1 + 2 * 3", "x = (1 + 2) * 3");
        alike::<Strict>("x = 2 ** 3 ** 2", "x = 2 ** (3 ** 2)");
        // a line ending in an operator goes on
        alike::<Strict>("x = 1 +\n  2", "x = 1 + 2");
    }

    // check refuses each table the lexer and parser cannot read one way
    #[test]
    fn check_refuses_tables_it_cannot_read() {
        let refused = |table: Syntax| -> String {
            let e = std::panic::catch_unwind(move || table.check()).expect_err("a refused table");
            e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default()
        };
        assert_eq!(refused(Syntax { ends: Ends::Semicolon, ..WORDS }), "a table that opens blocks at line ends needs line ends to end statements");
        assert_eq!(refused(Syntax { ends: Ends::Semicolon, ..INDENT }), "an indentation table needs line ends to end statements");
        assert_eq!(refused(Syntax { ends: Ends::Newline, ..BRACES }), "a table whose statements end at line ends spells no semicolon");
        assert_eq!(refused(Syntax { blocks: Blocks::Braces, ..INDENT }), "a table that delimits blocks spells their bounds");
        assert_eq!(refused(Syntax { blocks: Blocks::Braces, ..WORDS }), "a braces table bounds blocks with braces");
        assert_eq!(refused(Syntax { soft: &[TokenType::Match], ..WORDS }), "a table that opens blocks at line ends reserves its header words");
        assert_eq!(refused(Syntax { words: &[("if", TokenType::If), ("if", TokenType::While)], ..INDENT }), "two tokens of a table share one text");
        assert_eq!(refused(Syntax { strings: Strings { quotes: &[Quote { byte: b'~', multiline: false, triple: false }], ..mini::STRINGS }, ..BRACES }), "a quote of a table is also a symbol");
        assert_eq!(refused(Syntax { numbers: Numbers { separator: Some(b'"'), ..mini::NUMBERS }, ..BRACES }), "a digit separator of a table is also a quote");
        assert_eq!(refused(Syntax { comments: Comments { line: &["rem"], block: None, nested: false }, ..BRACES }), "a comment marker starts with a byte no name number or quote starts");
        assert_eq!(refused(Syntax { interp: Some(Interp { open: "{{{", ..mini::INTERP }), ..BRACES }), "a field opens with one or two bytes");
        assert_eq!(refused(Syntax { interp: Some(Interp { open: "${", ..mini::INTERP }), ..BRACES }), "a doubled delimiter is one byte");
        assert_eq!(refused(Syntax { interp: None, ..BRACES }), "an interpolating prefix needs the table's interpolation");
    }

    // mini's words with a binding word and without the lambda and case words
    const KIT_WORDS: &[(&str, TokenType)] = &[
        ("false", TokenType::False), ("nil", TokenType::Nil), ("true", TokenType::True), ("as", TokenType::As), ("assert", TokenType::Assert),
        ("async", TokenType::Async), ("await", TokenType::Await), ("break", TokenType::Break), ("class", TokenType::Class), ("continue", TokenType::Continue),
        ("fn", TokenType::Fn), ("del", TokenType::Delete), ("else if", TokenType::ElseIf), ("else", TokenType::Else), ("except", TokenType::Catch),
        ("finally", TokenType::Finally), ("for", TokenType::For), ("from", TokenType::From), ("global", TokenType::Global), ("if", TokenType::If),
        ("import", TokenType::Import), ("in", TokenType::In), ("is", TokenType::Is), ("nonlocal", TokenType::Outer), ("pass", TokenType::Empty),
        ("raise", TokenType::Throw), ("return", TokenType::Return), ("try", TokenType::Try), ("while", TokenType::While), ("with", TokenType::With),
        ("yield", TokenType::Yield), ("match", TokenType::Match), ("type", TokenType::Type), ("_", TokenType::Underscore), ("let", TokenType::Let),
    ];

    // mini's symbols with the marks of arms ranges and question conditionals
    const KIT_SYMBOLS: &[(&str, TokenType)] = &[
        ("(", TokenType::ParenOpen), (")", TokenType::ParenClose), ("[", TokenType::BracketOpen), ("]", TokenType::BracketClose),
        ("{", TokenType::BraceOpen), ("}", TokenType::BraceClose), ("{", TokenType::BlockOpen), ("}", TokenType::BlockClose),
        ("**=", TokenType::PowEqual), ("~/=", TokenType::FloorDivEqual), ("<<=", TokenType::LeftShiftEqual), (">>=", TokenType::RightShiftEqual),
        ("!=", TokenType::NotEqual), ("%=", TokenType::PercentEqual), ("&=", TokenType::BitAndEqual), ("**", TokenType::Pow), ("*=", TokenType::StarEqual),
        ("+=", TokenType::PlusEqual), ("-=", TokenType::MinusEqual), ("->", TokenType::Returns), ("...", TokenType::Ellipsis), ("~/", TokenType::FloorDiv),
        ("/=", TokenType::SlashEqual), (":=", TokenType::ColonEqual), ("<<", TokenType::LeftShift), ("<=", TokenType::LessEqual), ("==", TokenType::EqualEqual),
        (">=", TokenType::GreaterEqual), (">>", TokenType::RightShift), ("@=", TokenType::AtEqual), ("^=", TokenType::BitXorEqual), ("|=", TokenType::BitOrEqual),
        ("&&", TokenType::And), ("||", TokenType::Or), ("!", TokenType::Not), ("!", TokenType::Convert), ("%", TokenType::Percent), ("&", TokenType::BitAnd),
        ("*", TokenType::Star), ("+", TokenType::Plus), ("-", TokenType::Minus), (".", TokenType::Dot), ("/", TokenType::Slash), ("<", TokenType::Less),
        ("=", TokenType::Equal), (">", TokenType::Greater), ("@", TokenType::At), ("^", TokenType::BitXor), ("|", TokenType::BitOr), ("~", TokenType::Tilde),
        (",", TokenType::Comma), (":", TokenType::Colon), (";", TokenType::Semi),
        ("=>", TokenType::Arm), ("..", TokenType::Range), ("..=", TokenType::RangeInclusive), ("?", TokenType::Question),
    ];
    static KIT_OPERATORS: [[TokenType; 4]; 128] = operators(KIT_SYMBOLS);
    static KIT_KEYWORDS: Keywords = index(KIT_WORDS);

    // mini's ladder with ranges below the arithmetic and above the comparisons
    const KIT_LADDER: &[(TokenType, u8, u8)] = &[
        (TokenType::Or, 1, 2), (TokenType::And, 3, 4), (TokenType::Not, 0, 5),
        (TokenType::EqualEqual, 7, 8), (TokenType::NotEqual, 7, 8), (TokenType::Less, 7, 8), (TokenType::Greater, 7, 8), (TokenType::LessEqual, 7, 8),
        (TokenType::GreaterEqual, 7, 8), (TokenType::In, 7, 8), (TokenType::NotIn, 7, 8), (TokenType::Is, 7, 8), (TokenType::IsNot, 7, 8), (TokenType::Not, 7, 8),
        (TokenType::Range, 8, 9), (TokenType::RangeInclusive, 8, 9),
        (TokenType::BitOr, 9, 10), (TokenType::BitXor, 11, 12), (TokenType::BitAnd, 13, 14), (TokenType::LeftShift, 15, 16), (TokenType::RightShift, 15, 16),
        (TokenType::Plus, 17, 18), (TokenType::Minus, 17, 18),
        (TokenType::Star, 19, 20), (TokenType::Slash, 19, 20), (TokenType::Percent, 19, 20), (TokenType::FloorDiv, 19, 20), (TokenType::At, 19, 20),
        (TokenType::Minus, 0, 21), (TokenType::Plus, 0, 21), (TokenType::Tilde, 0, 21), (TokenType::Await, 0, 21),
        (TokenType::Pow, 22, 21),
    ];

    const KIT_SHAPES: Shapes = Shapes { lambda: Lambda::Pipes, binding: Binding::Optional, arms: true, ranges: true, ..Shapes::CORE };

    // a surface reading indentation with a binding word and arms and ranges and closures
    const KIT: Syntax = Syntax {
        name: "kit", extension: "kit", words: KIT_WORDS, symbols: KIT_SYMBOLS, operators: &KIT_OPERATORS, keywords: &KIT_KEYWORDS,
        precedence: KIT_LADDER, blocks: Blocks::Indent, shapes: KIT_SHAPES, ..MINI
    };

    struct Kit;
    impl Spelling for Kit {
        const SYNTAX: Syntax = KIT;
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // the kit in braces where an arm may open a block
    struct Braced;
    impl Spelling for Braced {
        const SYNTAX: Syntax = Syntax { name: "braced", blocks: Blocks::Braces, ..KIT };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // the kit where the first store of a name needs its binding word
    struct Declared;
    impl Spelling for Declared {
        const SYNTAX: Syntax = Syntax { name: "declared", shapes: Shapes { binding: Binding::Required, ..KIT_SHAPES }, ..KIT };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // conditionals that read the test first then a question mark
    struct Questions;
    impl Spelling for Questions {
        const SYNTAX: Syntax = Syntax { name: "questions", shapes: Shapes { cond: Cond::Question, ..Shapes::CORE }, ..KIT };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // mini where a dot reads a key unless a call follows
    struct Keys;
    impl Spelling for Keys {
        const SYNTAX: Syntax = Syntax { name: "keys", shapes: Shapes { dot: edge::lexer::Dot::Key, ..Shapes::CORE }, ..BRACES };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // mini whose headers hold their expressions in parentheses
    struct Parens;
    impl Spelling for Parens {
        const SYNTAX: Syntax = Syntax { name: "parens", shapes: Shapes { header_parens: true, ..Shapes::CORE }, ..BRACES };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // mini whose keyword arguments take a colon
    struct Colons;
    impl Spelling for Colons {
        const SYNTAX: Syntax = Syntax { name: "colons", shapes: Shapes { keyword_arg: TokenType::Colon, ..Shapes::CORE }, ..BRACES };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // mini with every switch of the fixed core off
    struct Bare;
    impl Spelling for Bare {
        const SYNTAX: Syntax = Syntax {
            name: "bare",
            shapes: Shapes { cond: Cond::None, lambda: Lambda::None, splat: false, slices: false, comprehensions: false, decorators: false, named: false, ..Shapes::CORE },
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // each piece compiles where a table switches it on as mini's program does or fails
    #[test]
    fn pieces_read_where_a_table_switches_them_on() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Piece { spelling: String, source: String, #[serde(default)] mini: Option<String>, #[serde(default)] error: Option<String> }
        // the mismatch a piece shows or None when it reads as the case says
        fn read<S: Spelling>(p: &Piece) -> Option<String> {
            match (&p.mini, &p.error) {
                (Some(mini), None) => { alike::<S>(&p.source, mini); None }
                (None, Some(want)) => {
                    let errors = compiled::<S>(&p.source).1;
                    (!errors.iter().any(|e| e.contains(want.as_str()))).then(|| format!("{} {:?} reports {errors:?}, want {want:?}", S::SYNTAX.name, p.source))
                }
                _ => panic!("a piece holds a mini program or an error"),
            }
        }
        let pieces: Vec<Piece> = serde_yaml_ng::from_str(include_str!("cases/syntax/pieces.yml")).expect("pieces");
        assert!(!pieces.is_empty());
        let wrong: Vec<String> = pieces.iter().filter_map(|p| match p.spelling.as_str() {
            "kit" => read::<Kit>(p),
            "braced" => read::<Braced>(p),
            "declared" => read::<Declared>(p),
            "questions" => read::<Questions>(p),
            "keys" => read::<Keys>(p),
            "parens" => read::<Parens>(p),
            "colons" => read::<Colons>(p),
            "bare" => read::<Bare>(p),
            other => panic!("no spelling {other}"),
        }).collect();
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn the_kit_lexes_its_marks() {
        use TokenType::*;
        assert_eq!(kinds::<Kit>("let x = 0..=n => a ? b .. c || |d|"), [Let, Name, Equal, Int, RangeInclusive, Name, Arm, Name, Question, Name, Range, Name, Or, BitOr, Name, BitOr]);
        // a closure default an arm or a question conditional leaves the targets of its statement
        for src in ["f = 0\nf += |a=1| a\n", "f: object = |a=1| a\n", "|a=1| a\n", "d = [0]\nmatch 1:\n    1 => d[0] = 1\n", "xs = [|a, b=2| a for _ in 0..2]\n"] {
            assert!(compiled::<Kit>(src).1.is_empty(), "{src:?} reports {:?}", compiled::<Kit>(src).1);
        }
        assert!(compiled::<Questions>("d = [0]\nif true ? 1 : 0: d[0] = 5\n").1.is_empty());
    }

    // dotted patterns read keys as dots do and a class pattern keeps its attribute
    #[test]
    fn keys_reach_dotted_patterns() {
        use edge::code::{OpCode, Value};
        let src = "match d {\n    case k.a { y = 1 }\n    case m.P(x=1) { y = 2 }\n}\n";
        let ((keys, e), (attrs, f)) = (compiled::<Keys>(src), compiled::<Braces>(src));
        assert!(e.is_empty() && f.is_empty(), "{e:?} {f:?}");
        let count = |c: &SSAChunk, op: OpCode| c.instructions.iter().filter(|i| i.opcode == op).count();
        assert_eq!((count(&keys, OpCode::LoadAttr), count(&attrs, OpCode::LoadAttr)), (1, 2));
        assert_eq!(count(&keys, OpCode::GetItem), count(&attrs, OpCode::GetItem) + 1);
        assert!(keys.constants.contains(&Value::Str("a".into())));
    }

    // check refuses each piece a table switches on without spelling its marks
    #[test]
    fn check_refuses_pieces_without_their_marks() {
        let refused = |table: Syntax| -> String {
            let e = std::panic::catch_unwind(move || table.check()).expect_err("a refused table");
            e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default()
        };
        let on = |shapes: Shapes| Syntax { shapes, ..BRACES };
        assert_eq!(refused(on(Shapes { cond: Cond::Question, ..Shapes::CORE })), "a table of question conditionals spells their marks");
        assert_eq!(refused(Syntax { shapes: Shapes { lambda: Lambda::Pipes, ..Shapes::CORE }, ..Strict::SYNTAX }), "a table of pipe closures spells the bar");
        assert_eq!(refused(on(Shapes { binding: Binding::Optional, ..Shapes::CORE })), "a table that binds with a word spells it");
        assert_eq!(refused(on(Shapes { arms: true, ..Shapes::CORE })), "a table that matches with arms spells their mark");
        assert_eq!(refused(Syntax { symbols: KIT_SYMBOLS, operators: &KIT_OPERATORS, shapes: Shapes { arms: true, ..Shapes::CORE }, ..WORDS }), "a table of block words takes no arms");
        assert_eq!(refused(on(Shapes { ranges: true, ..Shapes::CORE })), "a table that makes ranges spells their mark");
        for table in [KIT, Braced::SYNTAX, Declared::SYNTAX, Questions::SYNTAX, Keys::SYNTAX, Parens::SYNTAX, Colons::SYNTAX, Bare::SYNTAX] { table.check(); }
    }

    // an operand missing before a closer reports alone in every table that spells fields
    #[test]
    fn an_empty_field_reports_only_its_missing_expression() {
        fn alone<S: Spelling>(src: &str) {
            assert_eq!(compiled::<S>(src).1, ["expected expression"], "{} {src:?}", S::SYNTAX.name);
        }
        for src in ["x = f\"{}\"\n", "x = f\"a{}b\"\n", "print(f\"{}\")\n", "x = f\"{x:{}}\"\n", "x = f'{f\"{}\"}'\n", "x = f\"{1 +}\"\n"] {
            alone::<Mini>(src);
        }
        alone::<Indent>("x = f\"{}\"\n");
        alone::<Braces>("if x { y = f\"{}\" }\n");
        alone::<Words>("if x then y = \"#{}\" end\n");
        alone::<Backquote>("x = `${}`\n");
        alone::<Paren>("x = \"\\()\"\n");
        alone::<Dollar>("x = \"${}\"\n");
        // a bracket keeps its closer the same way
        for src in ["x = (1 + )\n", "x = [1, 2 +\n  ]\n", "f(a=)\n", "x = {1: }\n", "x[]\n", "x = [y for y in ]\n"] {
            alone::<Mini>(src);
        }
    }

    // a field holding more than its expression names its close and still closes
    #[test]
    fn a_field_past_its_expression_names_its_close() {
        assert_eq!(compiled::<Mini>("x = f\"{x y}\"\n").1, ["expected '}', got 'y'"]);
        assert_eq!(compiled::<Mini>("x = f\"{x:{y z}} {w}\"\n").1, ["expected '}', got 'z'"]);
        assert_eq!(compiled::<Words>("x = \"#{x y}\"\n").1, ["expected '}', got 'y'"]);
        assert_eq!(compiled::<Paren>("x = \"\\(x y)\"\n").1, ["expected ')', got 'y'"]);
        // a field its line leaves open is one the literal never closed
        assert_eq!(compiled::<Mini>("x = f\"{y\nz = 1\n").1, ["f-string was never closed"]);
    }

    // a block word ending in a wide letter whose last character a report spans whole
    const WIDE_WORDS: &[(&str, TokenType)] = &[("if", TokenType::If), ("hacé", TokenType::BlockOpen), ("fin", TokenType::BlockClose)];
    static WIDE_KEYWORDS: Keywords = index(WIDE_WORDS);
    struct Wide;
    impl Spelling for Wide {
        const SYNTAX: Syntax = Syntax { name: "wide", words: WIDE_WORDS, keywords: &WIDE_KEYWORDS, blocks: Blocks::Words { implicit: false }, soft: &[], ..WORDS };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    // fields that open with a wide character and take a spec after a colon
    struct Section;
    impl Spelling for Section {
        const SYNTAX: Syntax = Syntax {
            name: "section",
            strings: Strings { quotes: DOUBLE, prefixes: &[], ..mini::STRINGS },
            interp: Some(Interp { name: "template", quotes: b"\"", open: "§", close: b'}', literal: Literal::Backslash, short: None, spec: Some(b':'), convert: None, echo: None }),
            ..BRACES
        };
        const NATIVES: &'static edge::lang::Natives = &mini::NATIVES;
    }

    /* every report a source compiles to with its span between characters and rendered whole */
    fn reports<S: Spelling>(src: &str) -> Vec<String> {
        let Err(errs) = edge::lang::Front::spelled::<S>().program(src, Box::new(NoopResolver)) else { return Vec::new() };
        for e in &errs {
            assert!(src.is_char_boundary(e.start) && src.is_char_boundary(e.end), "{} {src:?} reports {:?} at {}..{}", S::SYNTAX.name, e.msg, e.start, e.end);
        }
        let shown = edge::code::Diagnostic::render_all(&errs, src, Some("main"));
        assert!(shown.contains(&errs[0].msg), "{shown}");
        errs.into_iter().map(|e| e.msg).collect()
    }

    // a refused escape before a wide character spans that character and renders whole
    #[test]
    fn a_report_spans_whole_characters() {
        assert_eq!(reports::<Words>("print(\"\\é\")\n"), ["invalid escape in string literal"]);
        assert_eq!(reports::<Words>("x = 1\nprint(\"#{x} \\é\")\n"), ["invalid escape in string literal"]);
        assert_eq!(reports::<Words>("print(\"\\日本\")\n"), ["invalid escape in string literal"]);
        assert_eq!(reports::<Wide>("if x hacé\n  print(x)\n"), ["'hacé' was never closed"]);
        // a field past the cap resumes after its whole opener in text or spec
        let too_deep = |src: String| {
            assert!(lex_as::<Section>(&src).0.iter().all(|t| src.is_char_boundary(t.start) && src.is_char_boundary(t.end)));
            reports::<Section>(&src).iter().any(|m| m == "template nesting depth exceeds maximum (200)")
        };
        assert!(too_deep(format!("print({}{})\n", "\"§".repeat(201), "\"}".repeat(201))));
        assert!(too_deep(format!("x = 1\nprint(\"{}{}\")\n", "§x:".repeat(201), "}".repeat(201))));
        // a span cut inside a character still renders the characters it touches
        let src = "x = 'é'\n";
        let cut = edge::code::Diagnostic { start: 6, end: 7, msg: "cut".into() };
        assert!(cut.render(src, None).contains("x = 'é'"));
        let past = edge::code::Diagnostic { start: 99, end: 3, msg: "past".into() };
        assert!(past.render(src, None).contains("past"));
    }

    // a wide character after corpus backslashes and brackets never cuts a span
    #[test]
    fn spans_end_between_characters_however_the_source_is_cut() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut tried = 0;
        for case in crate::cases::vm_cases(root).iter().step_by(3) {
            let src = case["src"].as_str().unwrap_or("");
            for (at, _) in src.match_indices(['\\', '{', ':', '(']).take(4) {
                let mutated = format!("{}é{}", &src[..at + 1], &src[at + 1..]);
                reports::<Mini>(&mutated);
                reports::<Words>(&mutated);
                tried += 1;
            }
        }
        assert!(tried > 1000, "{tried}");
    }

    // the walk's scan reads an import exactly where the parser starts an import statement
    #[test]
    fn the_scan_reads_imports_the_parser_reads() {
        use edge::deps::{scan_imports, ImportSpec::*};
        for src in ["from . import net\n", "x = f\"{import net}\"\n", "fn g():\n    yield from net()\n", "try:\n    pass\nexcept E as e:\n    raise V from e\n", "x = [import net]\n", "import a.(b\n", "import a.\n"] {
            assert_eq!(scan_imports::<Mini>(src), [], "{src:?}");
        }
        assert_eq!(scan_imports::<Mini>("if x: import y\nimport a as b, c\nfrom ._ import z\n"), [Bare("y".into()), Bare("a".into()), Bare("c".into()), Relative("./_.mini".into())]);
        // what the scan reads the parser resolves under the same spec
        let resolver = crate::common::TestResolver::new().with_code("./_.mini", "z = 1\n");
        let src = "from ._ import z\nprint(z)\n";
        let (_, errs) = Parser::<_, Mini>::spelled(src, lex_as::<Mini>(src).0.into_iter(), Box::new(resolver)).parse();
        assert!(errs.is_empty(), "{:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
    }

    // short fields queue in a loop so any number lex on a small stack
    #[test]
    fn short_fields_lex_in_a_loop() {
        let src = format!("x = \"{}\"\n", "$a ".repeat(100_000));
        let tokens = std::thread::Builder::new().stack_size(1 << 20).spawn(move || kinds::<Dollar>(&src)).expect("a thread").join().expect("no overflow");
        assert_eq!(tokens.iter().filter(|&&k| k == TokenType::Name).count(), 100_001);
        assert_eq!(kinds::<Dollar>("x = \"a $b c $d\"\n"), [TokenType::Name, TokenType::Equal, TokenType::FormatStart, TokenType::FormatText, TokenType::BraceOpen, TokenType::Name, TokenType::BraceClose, TokenType::FormatText, TokenType::BraceOpen, TokenType::Name, TokenType::BraceClose, TokenType::FormatEnd]);
    }

    /* every token's kind and flags as a pass settles them */
    fn settled(tokens: &[edge::lexer::Token]) -> Vec<(TokenType, bool, u8)> { tokens.iter().map(|t| (t.kind, t.comp, t.assigns)).collect() }

    // the forward pass settles each token as the scans back do on every corpus program
    #[test]
    fn the_forward_pass_settles_tokens_as_the_scans_back_do() {
        use edge::lexer::lex_forward;
        fn same<S: Spelling>(src: &str, wrong: &mut Vec<String>) {
            let (back, ahead) = (settled(&lex_as::<S>(src).0), settled(&lex_forward::<S>(src).0));
            if back != ahead && wrong.len() < 20 {
                let at = back.iter().zip(&ahead).position(|(a, b)| a != b).unwrap_or(0);
                wrong.push(format!("{} {src:?} at token {at}: back {:?} ahead {:?}", S::SYNTAX.name, back.get(at), ahead.get(at)));
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut sources: Vec<String> = crate::cases::vm_cases(root).iter().filter_map(|c| c["src"].as_str().map(String::from)).collect();
        let forms: Vec<std::collections::BTreeMap<String, String>> = serde_yaml_ng::from_str(include_str!("cases/syntax/forms.yml")).expect("forms");
        sources.extend(forms.into_iter().flat_map(|f| f.into_values()));
        // a fixed walk of insertions and deletions so the mutants repeat run to run
        const PIECES: &[&str] = &["for ", " in ", "=", ":", ",", "(", ")", "[", "]", "{", "}", "match ", "case ", "type ", "fn ", "lambda ", "|", "=>", "?", "\n", ";", " if ", " else ", "\n    ", "x", "\\\n"];
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        let mutants: Vec<String> = sources.iter().map(|src| {
            let mut chars: Vec<char> = src.chars().collect();
            let at = (next() as usize) % (chars.len() + 1);
            match next() % 3 {
                0 if at < chars.len() => { chars.remove(at); }
                _ => { let piece = PIECES[(next() as usize) % PIECES.len()]; for (k, c) in piece.chars().enumerate() { chars.insert(at + k, c); } }
            }
            chars.into_iter().collect()
        }).collect();
        let mut wrong = Vec::new();
        for src in sources.iter().chain(&mutants) {
            same::<Mini>(src, &mut wrong);
            same::<Indent>(src, &mut wrong);
            same::<Words>(src, &mut wrong);
            same::<Kit>(src, &mut wrong);
            same::<Braced>(src, &mut wrong);
            same::<Questions>(src, &mut wrong);
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}

// each syntax case held in every spelling it gives with its tokens and its chunk
#[cfg(test)]
mod runner {
    use edge::code::{Diagnostic, OpCode, Parser, SSAChunk, Value};
    use edge::deps::NoopResolver;
    use edge::lexer::{lex_as, Spelling};
    use mini::Mini;
    use spell::{Indent, Spell, Words};

    // the files the runner reads while the pieces have a test of their own
    pub const FILES: &[(&str, &str)] = &[
        ("bytecode.yml", include_str!("cases/syntax/bytecode.yml")),
        ("diagnostics.yml", include_str!("cases/syntax/diagnostics.yml")),
        ("forms.yml", include_str!("cases/syntax/forms.yml")),
        ("tokens.yml", include_str!("cases/syntax/tokens.yml")),
    ];

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Case {
        mini: Option<String>,
        // spell makes the indent text from mini's where a case leaves it out
        indent: Option<String>,
        words: Option<String>,
        tokens: Option<Vec<String>>,
        names: Option<Vec<String>>,
        constants: Option<Vec<String>>,
        instructions: Option<Vec<(String, u16)>>,
        functions: Option<usize>,
        classes: Option<usize>,
        // each native call's native positionals and keywords in code order
        natives: Option<Vec<(String, u8, u8)>>,
        errors: Option<Vec<String>>,
        error: Option<String>,
    }

    /* the chunk `src` compiles to with the lexer's reports kept among the parser's */
    fn compiled<S: Spelling>(src: &str) -> (SSAChunk, Vec<String>) {
        let (tokens, lex_errs) = lex_as::<S>(src);
        let mut parser = Parser::<_, S>::spelled(src, tokens.into_iter(), Box::new(NoopResolver));
        for e in lex_errs { parser.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }); }
        let (chunk, diagnostics) = parser.parse();
        (chunk, diagnostics.into_iter().map(|d| d.msg).collect())
    }

    fn kinds<S: Spelling>(src: &str) -> Vec<String> { lex_as::<S>(src).0.iter().map(|t| format!("{:?}", t.kind)).collect() }

    /* the decimal text of `digits` in `base` most significant first */
    fn decimal(digits: &[u8], base: u32) -> String {
        let mut out: Vec<u32> = vec![0];
        for &d in digits {
            let mut carry = d as u32;
            for x in out.iter_mut() { let v = *x * base + carry; (*x, carry) = (v % 10, v / 10); }
            while carry > 0 { out.push(carry % 10); carry /= 10; }
        }
        while out.len() > 1 && out.last() == Some(&0) { out.pop(); }
        out.iter().rev().map(|d| char::from(b'0' + *d as u8)).collect()
    }

    /* a constant as the cases write it */
    fn constant(v: &Value) -> String {
        match v {
            Value::Str(s) => s.clone(),
            Value::Bytes(b) => format!("b{:?}", String::from_utf8_lossy(b).to_string()),
            Value::Int(i) => i.to_string(),
            Value::Wide { neg, base, digits } => format!("{}{}", if *neg { "-" } else { "" }, decimal(digits, u32::from(*base))),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::None => "None".to_string(),
        }
    }

    /* where a chunk parts from a case's fields with a field left out held empty */
    fn chunk_differs(chunk: &SSAChunk, case: &Case) -> Option<String> {
        let constants: Vec<String> = chunk.constants.iter().map(constant).collect();
        // a native call reads as its positionals as the cases write it
        let instructions: Vec<(String, u16)> = chunk.instructions.iter()
            .map(|i| (format!("{:?}", i.opcode), if i.opcode == OpCode::CallNative { edge::code::native_site(i.operand, &chunk.native_sites).map_or(u16::MAX, |s| s.pos as u16) } else { i.operand }))
            .collect();
        let natives: Vec<(String, u8, u8)> = chunk.instructions.iter().filter(|i| i.opcode == OpCode::CallNative)
            .filter_map(|i| edge::code::native_site(i.operand, &chunk.native_sites))
            .map(|s| (mini::NATIVES.get(edge::lang::NativeId(s.id)).map_or("?", |n| n.name).to_string(), s.pos, s.kw)).collect();
        let parts = [
            ("names", format!("{:?}", chunk.names), format!("{:?}", case.names.clone().unwrap_or_default())),
            ("constants", format!("{constants:?}"), format!("{:?}", case.constants.clone().unwrap_or_default())),
            ("instructions", format!("{instructions:?}"), format!("{:?}", case.instructions.clone().unwrap_or_default())),
            ("functions", chunk.functions.len().to_string(), case.functions.unwrap_or(0).to_string()),
            ("classes", chunk.classes.len().to_string(), case.classes.unwrap_or(0).to_string()),
            ("natives", format!("{natives:?}"), format!("{:?}", case.natives.clone().unwrap_or_default())),
        ];
        parts.into_iter().find(|(_, got, want)| got != want).map(|(field, got, want)| format!("{field} {got} against {want}"))
    }

    /* a range mark calls its native pinned where mini calls it by its name */
    fn unpinned(chunk: &mut SSAChunk) {
        for i in chunk.instructions.iter_mut().filter(|i| i.opcode == OpCode::CallNative) {
            let Some(site) = edge::code::native_site(i.operand, &chunk.native_sites).filter(|s| s.pinned) else { continue };
            if let Some(plain) = (edge::code::NativeSite { pinned: false, ..site }).in_place() { i.operand = plain; }
        }
        for f in &mut chunk.functions { unpinned(&mut f.1); }
        for c in &mut chunk.classes { unpinned(c); }
    }

    /* the first way a case's sources part from what it holds */
    fn mismatch(case: &Case) -> Option<String> {
        let indent = match (&case.indent, &case.mini) {
            (Some(text), _) => Some(text.clone()),
            (None, Some(mini)) => match spell::respell(Spell::Mini, Spell::Indent, mini) {
                Ok(text) => Some(text),
                Err(e) => return Some(format!("spell makes no indent text of it, {}", e.why)),
            },
            (None, None) => None,
        };
        let sources: Vec<(&str, &str)> = [("mini", case.mini.as_deref()), ("indent", indent.as_deref()), ("words", case.words.as_deref())]
            .into_iter().filter_map(|(name, src)| src.map(|s| (name, s))).collect();
        if !sources.iter().any(|(n, _)| *n != "words") { return Some("a case holds a mini or an indent text".into()); }
        if let Some(want) = &case.tokens {
            for &(name, src) in &sources {
                let got = match name { "mini" => kinds::<Mini>(src), "indent" => kinds::<Indent>(src), _ => kinds::<Words>(src) };
                if &got != want { return Some(format!("{name} lexes to {got:?}")); }
            }
        }
        let chunked = case.names.is_some() || case.constants.is_some() || case.instructions.is_some();
        // a case of tokens alone holds no program to compile
        if case.tokens.is_some() && !chunked && case.errors.is_none() && case.error.is_none() { return None; }
        let mut first: Option<(&str, (SSAChunk, Vec<String>))> = None;
        for &(name, src) in &sources {
            let mut got = match name { "mini" => compiled::<Mini>(src), "indent" => compiled::<Indent>(src), _ => compiled::<Words>(src) };
            unpinned(&mut got.0);
            if chunked && let Some(why) = chunk_differs(&got.0, case) { return Some(format!("{name} gives {why}")); }
            match (&case.errors, &case.error) {
                (Some(want), _) if &got.1 != want => return Some(format!("{name} reports {:?}", got.1)),
                (_, Some(want)) if !got.1.iter().any(|m| m.contains(want.as_str())) => return Some(format!("{name} reports {:?} and not {want:?}", got.1)),
                (None, None) if !got.1.is_empty() => return Some(format!("{name} reports {:?}", got.1)),
                _ => {}
            }
            match &first {
                Some((other, chunk)) => if let Err(why) = spell::check::same_program(chunk, &got) { return Some(format!("{name} compiles to another program than {other}, {why}")); },
                None => first = Some((name, got)),
            }
        }
        None
    }

    /* runs every case of a file and lists each one that fails */
    pub fn run(file: &str) {
        let &(_, text) = FILES.iter().find(|(f, _)| *f == file).unwrap_or_else(|| panic!("no file {file}"));
        let cases: Vec<Case> = serde_yaml_ng::from_str(text).unwrap_or_else(|e| panic!("{file} {e}"));
        assert!(!cases.is_empty(), "{file} holds no case");
        let wrong: Vec<String> = cases.iter().enumerate().filter_map(|(i, c)| mismatch(c).map(|why| {
            let src = c.mini.as_deref().or(c.indent.as_deref()).unwrap_or("");
            format!("{file} case {i} {src:?}\n   {why}")
        })).collect();
        assert!(wrong.is_empty(), "{} of {} cases fail\n{}", wrong.len(), cases.len(), wrong.join("\n"));
    }

    // a file of cases no test reads would hold its cases to nothing
    #[test]
    fn every_case_file_is_read() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases/syntax");
        let mut found: Vec<String> = std::fs::read_dir(&dir).expect("tests/cases/syntax").map(|e| e.expect("a case file").file_name().to_string_lossy().into_owned()).collect();
        found.sort();
        let mut read: Vec<String> = FILES.iter().map(|(f, _)| f.to_string()).chain(["pieces.yml".to_string()]).collect();
        read.sort();
        assert_eq!(found, read);
    }
}

// the lexer's rules past what a case lists
#[cfg(test)]
mod lexer {
    use edge::lexer::{lex_as, TokenType};
    use spell::Indent;

    // every token case lexes to its tokens in each spelling it holds
    #[test]
    fn token_cases() { super::runner::run("tokens.yml") }

    // a line end a backslash escapes inside any literal is a line of the source
    #[test]
    fn escaped_line_ends_count_as_lines() {
        let lines = |src: &str| lex_as::<Indent>(src).0.iter().filter(|t| t.kind == TokenType::Name).map(|t| t.line).collect::<Vec<_>>();
        assert_eq!(lines("a = 'x\\\ny'\nb = 1\n"), [0, 2]);
        assert_eq!(lines("a = '''x\\\ny'''\nb = 1\n"), [0, 2]);
        assert_eq!(lines("a = b'x\\\ny' + r'x\\\ny'\nb = 1\n"), [0, 3]);
        assert_eq!(lines("a = f'x\\\n{c}'\nb = 1\n"), [0, 1, 2]);
    }

    // the post pass reads each token a bounded number of times however the statements repeat
    #[test]
    fn quadratic_shapes_lex_in_linear_time() {
        use edge::lexer::lex_as;
        let shapes = [
            "a=".repeat(200_000) + "1\n",
            format!("f({}1)\n", "a=1,".repeat(100_000)),
            format!("x = [y {}]\n", "for y in z ".repeat(60_000)),
            format!("x = [{}]\n", "lambda a, b: a for c in d, ".repeat(20_000)),
            "match (;".repeat(100_000),
            format!("if x: {}\n", "y = ".repeat(100_000)),
            format!("x = [\n{}y {}]\n", "\n".repeat(50_000), "for y in z ".repeat(50_000)),
        ];
        let start = std::time::Instant::now();
        for src in &shapes {
            let (tokens, _) = lex_as::<mini::Mini>(src);
            assert!(tokens.len() > 100_000, "{} tokens", tokens.len());
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{:?}", start.elapsed());
    }
}

// the parser's rules past what a case lists
#[cfg(test)]
mod parser {
    use edge::code::{Parser, Diagnostic};
    use edge::lexer::{lex_as, LexError, Token};
    use edge::deps::NoopResolver;
    use spell::Indent;

    // these tests keep the indent table they were written in
    fn lex(src: &str) -> (Vec<Token>, Vec<LexError>) { lex_as::<Indent>(src) }

    fn parser(src: &str, tokens: Vec<Token>) -> Parser<'_, std::vec::IntoIter<Token>, Indent> { Parser::spelled(src, tokens.into_iter(), Box::new(NoopResolver)) }

    // every bytecode case compiles to its chunk in each spelling it holds
    #[test]
    fn bytecode_cases() { super::runner::run("bytecode.yml") }

    // every diagnostic case reports its messages in each spelling it holds
    #[test]
    fn diagnostic_cases() { super::runner::run("diagnostics.yml") }

    // overflowing MAX_INSTRUCTIONS with a trailing `with` errors and never panics on a stale jump
    #[test]
    #[cfg_attr(miri, ignore = "lexing 160 KB under Miri's borrow tracking takes longer than a job")]
    fn instruction_overflow_does_not_panic() {
        let mut src = String::with_capacity(180_000);
        for _ in 0..40_000 { src.push_str("a=1\n"); }
        src.push_str("with a as b:\n    pass\n");
        let (tokens, _) = lex(&src);
        let (_chunk, diagnostics) = parser(&src, tokens).parse();
        assert!(
            diagnostics.iter().any(|d| d.msg.contains("program too large")),
            "expected 'program too large', got {:?}",
            diagnostics.iter().map(|d| &d.msg).collect::<Vec<_>>()
        );
    }

    // a multibyte identifier past the SSA name buffer truncates on a codepoint boundary
    #[test]
    fn long_multibyte_names_stay_valid_utf8() {
        let name = "\u{4E2D}".repeat(43);
        let src = format!("{name} = 1\n{name} = 2\nprint({name})\n");
        let (tokens, _) = lex(&src);
        let (chunk, _) = parser(&src, tokens).parse();
        for n in &chunk.names {
            assert!(core::str::from_utf8(n.as_bytes()).is_ok(), "invalid utf8 in SSA name {n:?}");
        }
    }

    /* the diagnostics `src` compiles to */
    fn messages(src: &str) -> Vec<String> {
        let (tokens, _) = lex(src);
        parser(src, tokens).parse().1.into_iter().map(|d| d.msg).collect()
    }

    // every deep nesting hits the depth guard instead of overflowing the native or wasm stack
    #[test]
    fn deep_operator_chains_error_not_overflow() {
        let check = |src: String, label: &str| {
            let got = messages(&src);
            assert!(got.iter().any(|m| m.contains("too deeply nested")), "{label}: expected 'too deeply nested', got {got:?}");
        };
        // borrow tracking makes lexing quadratic under Miri so there chains stop just past the guard
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
        // a comparison chain stays flat so any length compiles
        assert_eq!(messages(&format!("x = 1{}", " < 1".repeat(100 * scale))), Vec::<String>::new(), "comparison chain");
    }

    // a compound statement never follows the colon of another on its line at any length
    #[test]
    fn compound_after_colon_errors() {
        let scale = if cfg!(miri) { 1 } else { 100 };
        let chain = format!("{}pass", "if c: ".repeat(1_000 * scale));
        for src in ["if a: if b: pass", "def f(): def g(): pass", "for x in y: if x: pass", "class A: def f(self): pass", "while c: with x: pass", chain.as_str()] {
            assert_eq!(messages(src), ["a compound statement must start its own line"], "{src:?}");
        }
        assert_eq!(messages("if a: x = 1; y = 2\nfor i in r: print(i)\ndef f(n): return n"), Vec::<String>::new());
    }

    // a report shows 20 errors cut to their nearby columns whatever a long line holds
    #[test]
    fn long_reports_stay_small() {
        let src = "$".repeat(if cfg!(miri) { 400 } else { 40_000 });
        let (tokens, lex_errs) = lex(&src);
        let mut parser = parser(&src, tokens);
        for e in lex_errs { parser.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }); }
        let errs = parser.parse().1;
        let out = Diagnostic::render_all(&errs, &src, None);
        assert!(out.len() < 4_000 && out.ends_with(&format!("and {} more errors\n", errs.len() - 20)), "{} bytes", out.len());
        let one = Diagnostic { start: 300, end: 301, msg: "x".into() }.render(&src, None);
        let lines: Vec<&str> = one.lines().collect();
        assert_eq!((lines[1], lines[3].len(), lines[4].find('^')), ("  --> <input>:1:301", 4 + 3 + 80 + 3, Some(4 + 3 + 40)), "{one}");
        assert_eq!(messages(&format!("x = 1 {}", "a".repeat(1_000))), [format!("expected newline, got '{}...'", "a".repeat(40))]);
    }

    // broken statements that once looped forever under the fuzzer finish with their diagnostics
    #[test]
    fn broken_statements_finish() {
        for src in ["(] =')\n}] =')", "x = [2, 3, 4]\nx[:; )= 10\nprint(x)", "[n) = [9)bbbbbb, #]\nx3, #]\n] = [5]\ndef f(bbbbbbb"] {
            assert!(!messages(src).is_empty(), "{src:?}");
        }
    }

    // none of the four comprehension forms may yield
    #[test]
    fn yield_inside_comprehension_errors() {
        for src in ["def g():\n    x = [(yield i) for i in range(3)]", "def g():\n    x = {(yield i) for i in y}", "def g():\n    x = {i: (yield) for i in y}", "def g():\n    x = list((yield i) for i in y)"] {
            assert!(messages(src).iter().any(|m| m == "'yield' inside a comprehension"), "{src:?}");
        }
    }

    // lambdas read the names around them in place so many compile in linear time
    #[test]
    #[cfg_attr(miri, ignore)]
    fn many_lambdas_compile_in_linear_time() {
        let src: String = (0..30_000).map(|i| format!("f{i} = lambda x: x + {i}\n")).collect();
        let start = std::time::Instant::now();
        let (chunk, errors) = crate::common::compile(&src);
        let spent = start.elapsed();
        assert!(errors.is_empty() && chunk.functions.len() == 30_000, "{:?}", errors.first().map(|e| &e.msg));
        assert!(spent < std::time::Duration::from_secs(2), "{spent:?}");
    }

    // an attribute name stays in its body whatever other operand matches its slot
    #[test]
    fn attribute_names_stay_in_their_body() {
        let (chunk, errors) = crate::common::compile("f = lambda o: o.foo(1)\nfn outer(o):\n    fn inner(p):\n        return p.bar(2)\n    return inner(o)\nclass P:\n    fn m(self):\n        return self.baz(3)\n");
        assert!(errors.is_empty(), "{:?}", errors.first().map(|e| &e.msg));
        let outer = &chunk.functions.iter().find(|f| f.1.qualname == "outer").expect("outer").1;
        let class = chunk.classes.first().expect("P");
        for (names, attr) in [(&chunk.names, "foo"), (&outer.names, "bar"), (&class.names, "baz")] {
            assert!(!names.iter().any(|n| n == attr), "{attr} in {names:?}");
        }
    }

    // an else if chain of any length compiles in a loop on a small stack
    #[test]
    #[cfg_attr(miri, ignore = "a chain long enough to matter takes Miri too long")]
    fn a_long_else_if_chain_compiles_on_a_small_stack() {
        let chain = |arms: usize, test: &str| {
            let mut src = format!("x = 1\ny = false\nif {test}:\n    pass\n");
            for _ in 0..arms { src.push_str(&format!("else if {test}:\n    pass\n")); }
            src + "else:\n    print('last')\n"
        };
        let small = |src: String| std::thread::Builder::new().stack_size(1 << 20).spawn(move || {
            let (chunk, errors) = crate::common::compile(&src);
            if !errors.is_empty() { return errors.into_iter().map(|e| e.msg).collect(); }
            let mut vm = crate::common::boot(&chunk, edge::value::Limits::sandbox());
            vm.run().expect("the chain runs");
            vm.output.clone()
        }).expect("a thread").join().expect("no overflow");
        // past the chunk's cap the chain still parses to its end and reports the size
        assert_eq!(small(chain(400_000, "x == 0")), ["program too large: exceeded the 65535 instruction, name, or constant limit"]);
        assert_eq!(small(chain(10_000, "y")), ["last"]);
    }

    // a compile keeps what the lexer reported beside what the parser did
    #[test]
    fn a_compile_keeps_the_lexers_reports() {
        let (_, errors) = crate::common::compile("x = 'abc\nprint(x)\n");
        assert_eq!(errors.first().map(|e| e.msg.as_str()), Some("unterminated string literal"));
        let (_, errors) = crate::common::compile("x = 0x\n");
        assert_eq!(errors.first().map(|e| e.msg.as_str()), Some("missing digits in numeric literal"));
    }
}
