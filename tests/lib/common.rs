use interpreter::{Limits, VM};
use interpreter::lang::Runtime;
use interpreter::lexer::Spelling;

// each call's repr or its error as the class then the words
pub(crate) const TRY: &str = "fn t(f) {\n    try {\n        print(repr(f()))\n    } catch Error as e {\n        print(text(type(e)) + \":\", e)\n    }\n}\n";

/* what `src` read as `S` printed under `runtime` and then the words of the error it ended in */
pub(crate) fn ran<S: Spelling>(runtime: &'static Runtime, src: &str, limits: Limits) -> String {
    let (chunk, errors) = interpreter::compile_as::<S>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(runtime, &chunk, limits);
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}
