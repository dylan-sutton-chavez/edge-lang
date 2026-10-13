use compiler::lexer::lex;
use compiler::parser::Parser;
use compiler::vm::{Limits, VM};

// Without the metering allocator nothing counts the memory limit, so a run refuses it and runs without one.
#[test]
fn a_memory_limit_needs_the_metering_allocator() {
    let src = "print(1)\n";
    let (tokens, _) = lex(src);
    let (chunk, _) = Parser::new(src, tokens.into_iter()).parse();
    let refused = VM::with_limits(&chunk, Limits::sandbox()).run().map(|_| ()).unwrap_err();
    assert!(refused.to_string().contains("Metered"), "{refused}");
    let mut unlimited = VM::with_limits(&chunk, Limits { memory: usize::MAX, ..Limits::sandbox() });
    unlimited.run().unwrap();
    assert_eq!(unlimited.output_text(), "1\n");
}
