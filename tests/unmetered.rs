use edge::embed::{Limits, VM};

// without the metering allocator nothing counts memory so a limit is refused
#[test]
fn a_memory_limit_needs_the_metering_allocator() {
    let (chunk, _) = edge::code::compile(&lox::FRONT, "print 1;\n");
    let refused = VM::with_runtime(&lox::RUNTIME, &chunk, Limits::sandbox()).run().map(|_| ()).unwrap_err();
    assert!(refused.to_string().contains("edge::allocator!()"), "{refused}");
    let mut unlimited = VM::with_runtime(&lox::RUNTIME, &chunk, Limits { memory: usize::MAX, ..Limits::sandbox() });
    unlimited.run().unwrap();
    assert_eq!(unlimited.output_text(), "1\n");
}
