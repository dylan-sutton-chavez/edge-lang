#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

#[allow(dead_code)]
mod suite;

use interpreter::cli::cmd::{Run, Test};
use interpreter::cli::host::{Files, Net};
use interpreter::cli::{Engine, Kit, Perms, DOCS_FIELD, MANIFEST_FIELDS};
use std::sync::Arc;
use suite::{net_cases, Bin};

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "INTERPRETER_AS_RIG";

// the rig make wasm-rig builds with the exports interpreter.wasm has
const WASM: &[u8] = include_bytes!("../../target/wasm32-unknown-unknown/release/rig.wasm");

/* the interpreter composition over the rig so plugin and net calls take lists and keywords */
fn kit() -> Kit {
    Kit::new("interpreter", env!("CARGO_PKG_VERSION"), Engine::wasm(WASM, rig::RUNTIME.descriptor()))
        .command(Run)
        .command(Test)
        .fields(MANIFEST_FIELDS)
        .field(DOCS_FIELD)
        .module(Arc::new(Net))
        .module(Arc::new(Files))
        .access(Arc::new(Perms))
}

fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit().main()
    }
    let exe = std::env::current_exe().expect("the test binary runs from a path");
    let bin = Bin { path: exe.to_str().expect("the test binary has a UTF-8 path"), env: &[(AS_CLI, "1")], plugins: true };
    net_cases(bin, include_str!("rig.json"), &[&[]]);
}
