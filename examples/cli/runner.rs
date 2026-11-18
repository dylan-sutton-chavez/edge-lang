#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use interpreter::cli::cmd::{run, test};
use interpreter::cli::{Kit, Runtime};

// the lox runtime make wasm-cli builds
const WASM: &[u8] = include_bytes!("../../target/wasm32-unknown-unknown/cli/interpreter.wasm");

/* two commands over a runtime the kit compiles on its first load */
fn main() {
    Kit::new("runner", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &interpreter_lox::DESCRIPTOR))
        .command(run::Run)
        .command(test::Test)
        .main()
}
