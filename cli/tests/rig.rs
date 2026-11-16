#[allow(dead_code)]
mod suite;

use interpreter_cli::cmd::{run, test};
use interpreter_cli::{docs, files, manifest, net, perms, Kit, Runtime};
use std::sync::Arc;
use suite::{net_cases, Bin};

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "INTERPRETER_AS_RIG";

// the rig make wasm-rig builds with the exports interpreter.wasm has
const WASM: &[u8] = include_bytes!("../../target/wasm32-unknown-unknown/release/rig.wasm");

/* the interpreter composition over the rig so plugin and net calls take lists and keywords */
fn kit() -> Kit {
    Kit::new("interpreter", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &rig::DESCRIPTOR))
        .command(run::Run)
        .command(test::Test)
        .fields(manifest::FIELDS)
        .field(docs::FIELD)
        .field(perms::FIELD)
        .module(Arc::new(net::Net))
        .module(Arc::new(files::Files))
        .access(Arc::new(perms::Perms))
}

fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit().main()
    }
    let exe = std::env::current_exe().expect("the test binary runs from a path");
    let bin = Bin { path: exe.to_str().expect("the test binary has a UTF-8 path"), env: &[(AS_CLI, "1")], plugins: true };
    net_cases(bin, include_str!("rig.json"), &[&[]]);
}
