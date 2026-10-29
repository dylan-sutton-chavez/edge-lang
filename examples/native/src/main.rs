use std::process::ExitCode;

use edge::embed::Limits;

// every run meters what it allocates so the sandbox's memory limit holds
edge::allocator!();

/* runs the mini file its first argument names with stdin as the program's input */
fn main() -> ExitCode {
    edge::targets::native::main(&mini::RUNTIME, &mini::FRONT, Limits::sandbox())
}
