use std::process::ExitCode;

use edge::embed::Limits;

// every run meters what it allocates so its memory limit holds
edge::allocator!();

fn main() -> ExitCode {
    edge::targets::native::main(&lox::RUNTIME, &lox::FRONT, Limits::sandbox())
}
