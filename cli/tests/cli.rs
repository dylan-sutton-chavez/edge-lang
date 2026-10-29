mod common;
mod suite;

use suite::{net, suite, Bin};

const EDGE: Bin = Bin { path: env!("CARGO_BIN_EXE_edge"), env: &[], plugins: true };

/// JSON-driven CLI suite, each case in `cli.json` is one tempdir plus one spawn of the `edge` binary.
#[test]
fn cli_suite() {
    suite(EDGE, include_str!("cli.json"), &[&[]]);
}

/// third party wasm plugins built from `plugin/example` run the way a project declares them
#[test]
fn plugin_suite() {
    suite(EDGE, include_str!("plugins.json"), &[&[]]);
}

/// Programs both hosts must run alike, each case natively and again in the browser, which needs a Chrome.
#[test]
fn hosts_suite() {
    suite(EDGE, include_str!("hosts.json"), &[&[], &["--web"]]);
}

/// the net cases on both hosts against one loopback fixture which needs a Chrome
#[test]
fn net_suite() {
    net(EDGE, &[&[], &["--web"]]);
}
