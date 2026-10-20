#[cfg(all(debug_assertions, not(feature = "coverage")))]
// No loop nests in Rust, which debug builds stop at, and no frame is generic, so every case of vm.json runs with no generic frame pushed.
#[test]
fn old_path_ceilings() {
    use compiler::lexer::lex;
    use compiler::parser::Parser;
    use compiler::vm::{Limits, VM};
    const CEILINGS: [u64; 1] = [0];
    const NAMES: [&str; 1] = ["generic frames"];
    let cases: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string("tests/cases/vm.json").unwrap()).unwrap();
    let mut total = [0u64; 1];
    for case in &cases {
        // The cases a host drives with events or input run as vm.rs runs them.
        if case.get("events").is_some() || case.get("input").is_some() { continue; }
        let src = case["src"].as_str().unwrap();
        let (tokens, _) = lex(src);
        let (mut chunk, _) = Parser::new(src, tokens.into_iter()).parse();
        compiler::vm::optimizer::constant_fold(&mut chunk);
        let mut vm = VM::with_limits(&chunk, Limits::sandbox());
        let _ = vm.run();
        for (t, n) in total.iter_mut().zip(vm.old_path_counts()) { *t += n; }
    }
    eprintln!("old path over vm.json: {:?} (ceilings {:?})", total, CEILINGS);
    for k in 0..1 {
        assert!(total[k] <= CEILINGS[k], "{} opened {} times over vm.json, past its ceiling of {}: a call into Python from Rust is a section, a job or a frame of the running loop", NAMES[k], total[k], CEILINGS[k]);
    }
}
