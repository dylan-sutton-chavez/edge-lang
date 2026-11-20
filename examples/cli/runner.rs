#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use interpreter::cli::cmd::{Run, Test};
use interpreter::cli::{Engine, Kit};

// the lox runtime make wasm-cli builds
const WASM: &[u8] = include_bytes!("../../target/wasm32-unknown-unknown/cli/interpreter.wasm");

/* two commands over a runtime the kit compiles on its first load */
fn main() {
    Kit::new("runner", env!("CARGO_PKG_VERSION"), Engine::wasm(WASM, interpreter_lox::RUNTIME.descriptor()))
        .command(Run)
        .command(Test)
        .main()
}
