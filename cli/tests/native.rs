mod common;
mod suite;

use cli::cmd::{publish, run, test};
use cli::{docs, files, net, perms, Kit, Runtime};
use std::sync::Arc;
use suite::{net, suite, Bin};

// the native engine meters what a run holds through this allocator
edge::allocator!();

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "EDGE_AS_NATIVE";

/* the edge composition's run and test over mini built into this binary with no wasm between */
fn kit() -> Kit {
    Kit::new("edge", env!("CARGO_PKG_VERSION"), Runtime::native(&mini::RUNTIME))
        .title(format!("The Edge Lang developer CLI for {} run natively", mini::DESCRIPTOR.language))
        .command(run::Run)
        .command(test::Test)
        .fields(publish::FIELDS)
        .field(docs::FIELD)
        .field(perms::FIELD)
        .module(Arc::new(net::Net))
        .module(Arc::new(files::Files))
        .access(Arc::new(perms::Perms))
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
