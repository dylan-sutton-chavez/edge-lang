use std::process::ExitCode;

use interpreter::Limits;

// every run meters what it allocates so its memory limit holds
interpreter::allocator!();

fn main() -> ExitCode {
    interpreter::native::main(&interpreter_lox::RUNTIME, Limits::sandbox())
}
