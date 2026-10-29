use edge::embed::{Limits, VM};

mod common;

// without the metering allocator nothing counts memory so a limit is refused
#[test]
fn a_memory_limit_needs_the_metering_allocator() {
    let src = "print(1)\n";
    let (chunk, _) = common::parse(src);
    let refused = VM::with_runtime(&mini::RUNTIME, &chunk, Limits::sandbox()).run().map(|_| ()).unwrap_err();
    assert!(refused.to_string().contains("edge::allocator!()"), "{refused}");
    let mut unlimited = VM::with_runtime(&mini::RUNTIME, &chunk, Limits { memory: usize::MAX, ..Limits::sandbox() });
    unlimited.run().unwrap();
    assert_eq!(unlimited.output_text(), "1\n");
}
