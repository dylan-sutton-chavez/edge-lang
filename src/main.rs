#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use interpreter::cli::cmd::{Actor, Add, Build, Init, Lock, Remove, Run, Serve, Test, Uninstall};
use interpreter::cli::host::{Files, Net};
use interpreter::cli::{Kit, Perms, DOCS_FIELD, MANIFEST_FIELDS};
use std::sync::Arc;

/* the interpreter binary as one composition of the kit over the runtime each project declares */
fn interpreter() -> Kit {
    Kit::per_project("interpreter", env!("CARGO_PKG_VERSION"))
        .site("github.com/dylan-sutton-chavez/interpreter")
        .upgrade("cargo install interpreter --features cli")
        .command(Run)
        .command(Build)
        .command(Actor)
        .command(Serve)
        .command(Test)
        .command(Init)
        .command(Add)
        .command(Remove)
        .command(Lock)
        .command(Uninstall)
        .fields(MANIFEST_FIELDS)
        .field(DOCS_FIELD)
        .module(Arc::new(Net))
        .module(Arc::new(Files))
        .access(Arc::new(Perms))
}

fn main() {
    interpreter().main()
}
