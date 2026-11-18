#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[allow(dead_code)]
mod suite;

use interpreter::cli::cmd::run;
use interpreter::cli::{Kit, Runtime};
use suite::{suite, Bin};

// the native engine meters what a run holds through this allocator
interpreter::allocator!();

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "INTERPRETER_AS_NATIVE";

/* the coverage cases on a run of lox built into this binary with the coverage feature */
fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        Kit::new("interpreter", env!("CARGO_PKG_VERSION"), Runtime::native(&interpreter_lox::RUNTIME)).command(run::Run).main()
    }
    let exe = std::env::current_exe().expect("the test binary runs from a path");
    suite(Bin { path: exe.to_str().expect("the test binary has a UTF-8 path"), env: &[(AS_CLI, "1")], plugins: true }, include_str!("coverage.json"), &[&[]]);
}
