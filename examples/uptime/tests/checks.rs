use std::path::Path;

// every run meters what it allocates as uptime.wasm does
edge::allocator!();

/* the diagnostics `src` compiles to past the imports nothing here resolves */
fn problems(src: &str) -> Vec<String> {
    let (_, errors) = edge::code::compile(&uptime::FRONT, src);
    errors.into_iter().map(|d| d.msg).filter(|msg| !msg.contains("no resolver configured")).collect()
}

#[test]
fn every_check_compiles_through_the_table() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("checks");
    let mut read = 0;
    for entry in std::fs::read_dir(&dir).expect("examples/uptime/checks reads").flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "up") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("a check reads as text");
        assert_eq!(problems(&src), Vec::<String>::new(), "{}", path.display());
        read += 1;
    }
    assert_eq!(read, 5);
}

// a first store without let fails to compile so a typo never makes a name
#[test]
fn a_store_the_check_never_let_is_refused() {
    assert_eq!(problems("let total = 0\ntotl = 5\n"), ["name 'totl' is not bound, bind it with 'let'"]);
    assert_eq!(problems("let total = 0\ntotal = 5\n"), Vec::<String>::new());
}

/* what a check printed then its error in this runtime's classes and words */
fn ran(src: &str) -> String {
    let (chunk, errors) = edge::code::compile(&uptime::FRONT, src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());
    let mut vm = edge::embed::VM::with_runtime(&uptime::RUNTIME, &chunk, edge::embed::Limits::sandbox());
    let result = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = result { out.push_str(&e.render(&uptime::NAMES)); }
    out
}

// a body that spells no number reads in the check's own words for JSON
#[test]
fn a_number_with_a_leading_zero_is_no_json() {
    assert_eq!(ran("print(json(\"01\"))\n"), "BadData: the answer is not JSON, a number has a leading zero at byte 0");
}

// the kernels' failures read in lib's neutral words with this runtime's kind names
#[test]
fn a_wrong_argument_names_the_native_and_the_kinds() {
    assert_eq!(ran("print(json(5))\n"), "WrongType: json takes text or bytes, not int");
    assert_eq!(ran("exit(\"x\")\n"), "WrongType: exit takes int, not text");
}

// a block a line end opens and no end closes names the end it needs
#[test]
fn a_block_left_open_names_the_end_it_needs() {
    assert_eq!(problems("let pages = [\"github\"]\nfor name in pages\n  print(name)\n"), ["the block this line opens was never closed, close it with 'end'"]);
    assert_eq!(problems("for name in [1] do\n  print(name)\n"), ["'do' was never closed"]);
}

// a text field missing its expression or holding more reports that alone
#[test]
fn a_field_reports_only_what_it_holds_wrong() {
    assert_eq!(problems("let page = 1\ncheck page == 1, \"took {} ms\"\n"), ["expected expression"]);
    assert_eq!(problems("let page = 1\ncheck page == 1, \"took {page page} ms\"\n"), ["expected '}', got 'page'"]);
}

// a call off the signature reads in the engine's words with one argument singular
#[test]
fn a_call_off_the_signature_counts_its_arguments() {
    assert_eq!(ran("print(size())\n"), "WrongType: size() takes 1 argument, not 0");
    assert_eq!(ran("print(size(\"up\", 2))\n"), "WrongType: size() takes 1 argument, not 2");
}

// a refused escape before a wide character spans all of it so its report renders
#[test]
fn a_refused_escape_before_a_wide_character_renders() {
    for src in ["print(\"\\é\")\n", "let x = 1\nprint(\"{x} \\é\")\n", "print(\"\\u{zz}\")\n", "print(\"\\u{}\")\n"] {
        let Err(errs) = uptime::FRONT.program(src, Box::new(edge::deps::NoopResolver)) else { panic!("{src:?} compiled") };
        assert!(errs.iter().all(|e| src.is_char_boundary(e.start) && src.is_char_boundary(e.end)), "{src:?}");
        assert_eq!(errs[0].msg, "invalid escape in string literal", "{src:?}");
        assert!(edge::code::Diagnostic::render_all(&errs, src, Some("check.up")).contains("invalid escape"));
    }
    assert_eq!(ran("print(\"\\u{41}\\u{e9}\")\n"), "Aé\n");
}

// an else if chain of any length compiles in a loop on a small stack
#[test]
fn a_long_else_if_chain_compiles_on_a_small_stack() {
    let chain = |arms: usize| {
        let mut src = String::from("let x = 1\nlet y = false\nif y\n  x = 2\n");
        for _ in 0..arms { src.push_str("else if y\n  x = 2\n"); }
        src + "else\n  print(x)\nend\n"
    };
    let small = |src: String| std::thread::Builder::new().stack_size(1 << 20).spawn(move || match problems(&src).as_slice() {
        [] => ran(&src),
        found => found.join("\n"),
    }).expect("a thread").join().expect("no overflow");
    // past the chunk's cap the chain still parses to its end and reports the size
    assert_eq!(small(chain(300_000)), "program too large: exceeded the 65535 instruction, name, or constant limit");
    assert_eq!(small(chain(8_000)), "1\n");
}

// a line end reads the last token once so blank and comment lines stay linear
#[test]
fn blank_lines_and_comments_lex_in_linear_time() {
    let src = format!("let x = 1 +{}\n  2\nprint(x)\n", "\n# note\n".repeat(100_000));
    let start = std::time::Instant::now();
    assert_eq!(problems(&src), Vec::<String>::new());
    assert!(start.elapsed() < std::time::Duration::from_secs(3), "{:?}", start.elapsed());
    assert_eq!(ran(&src), "3\n");
}

// a decimal literal past the digit limit names the limit and no notation uptime lacks
#[test]
fn a_literal_past_the_digit_limit_names_only_the_limit() {
    let src = format!("print({})\n", "9".repeat(10_001));
    assert_eq!(problems(&src), ["an integer literal of 10001 digits is past the limit of 10000"]);
}
