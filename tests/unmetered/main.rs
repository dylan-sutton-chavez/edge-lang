use interpreter::{Limits, VM};

// without the metering allocator nothing counts memory so a limit is refused
#[test]
fn a_memory_limit_needs_the_metering_allocator() {
    let (chunk, _) = interpreter::compile(&interpreter_lox::FRONT, "print 1;\n");
    let refused = VM::with_runtime(&interpreter_lox::RUNTIME, &chunk, Limits::sandbox()).run().map(|_| ()).unwrap_err();
    assert!(refused.to_string().contains("interpreter::allocator!()"), "{refused}");
    let mut unlimited = VM::with_runtime(&interpreter_lox::RUNTIME, &chunk, Limits::sandbox().memory(usize::MAX));
    unlimited.run().unwrap();
    assert_eq!(unlimited.output_text(), "1\n");
}
