#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use interpreter::cli::cmd::{actors, build, init, pkg, run, serve, test, uninstall};
use interpreter::cli::{docs, files, manifest, net, perms, Kit};
use std::sync::Arc;

/* the interpreter binary as one composition of the kit over the runtime each project declares */
fn interpreter() -> Kit {
    Kit::per_project("interpreter", env!("CARGO_PKG_VERSION"))
        .site("github.com/dylan-sutton-chavez/interpreter")
        .upgrade("cargo install interpreter --features cli")
        .command(run::Run)
        .command(build::Build)
        .command(actors::Actor)
        .command(serve::Serve)
        .command(test::Test)
        .command(init::Init)
        .command(pkg::Add)
        .command(pkg::Remove)
        .command(pkg::Lock)
        .command(uninstall::Uninstall)
        .fields(manifest::FIELDS)
        .field(docs::FIELD)
        .field(perms::FIELD)
        .module(Arc::new(net::Net))
        .module(Arc::new(files::Files))
        .access(Arc::new(perms::Perms))
}

fn main() {
    interpreter().main()
}
