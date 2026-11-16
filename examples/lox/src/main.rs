use std::process::ExitCode;

use interpreter::embed::Limits;

// every run meters what it allocates so its memory limit holds
interpreter::allocator!();

fn main() -> ExitCode {
    interpreter::targets::native::main(&interpreter_lox::RUNTIME, &interpreter_lox::FRONT, Limits::sandbox())
}
