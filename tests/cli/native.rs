#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[allow(dead_code)]
mod suite;

use interpreter::cli::cmd::{Run, Test};
use interpreter::cli::host::{Files, Net};
use interpreter::cli::{Engine, Kit, Perms, DOCS_FIELD, MANIFEST_FIELDS};
use std::sync::Arc;
use suite::{net, suite, Bin};

// the native engine meters what a run holds through this allocator
interpreter::allocator!();

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "INTERPRETER_AS_NATIVE";

/* the interpreter composition's run and test over lox built into this binary */
fn kit() -> Kit {
    Kit::new("interpreter", env!("CARGO_PKG_VERSION"), Engine::native(&interpreter_lox::RUNTIME))
        .title(format!("The interpreter developer CLI for {} run natively", interpreter_lox::RUNTIME.name()))
        .command(Run)
        .command(Test)
        .fields(MANIFEST_FIELDS)
        .field(DOCS_FIELD)
        .module(Arc::new(Net))
        .module(Arc::new(Files))
        .access(Arc::new(Perms))
}

/* the cases both hosts run alike and the net cases run again on the native engine */
fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit().main()
    }
    let exe = std::env::current_exe().expect("the test binary runs from a path");
    let bin = Bin { path: exe.to_str().expect("the test binary has a UTF-8 path"), env: &[(AS_CLI, "1")], plugins: false };
    suite(bin, include_str!("hosts.json"), &[&[]]);
    net(bin, &[&[]]);
    // a runtime run natively loads no wasm plugin and these cases hold how it refuses one
    suite(Bin { plugins: true, ..bin }, include_str!("native.json"), &[&[]]);
}
