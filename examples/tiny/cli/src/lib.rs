use cli::cmd::{run, test};
use cli::host::HostModule;
use cli::{files, perms, Kit, Limits, Runtime};
use std::sync::Arc;

// the runtime make wasm-tiny builds from examples/tiny
const WASM: &[u8] = include_bytes!("../../../../target/wasm32-unknown-unknown/release/tiny.wasm");

// a flow spends steps by the records it reads so a day of events fits
const STEPS: u64 = 10_000_000_000;

/* run and test with net and files under the grants and errors in tiny's words */
pub fn kit(net: Arc<dyn HostModule>) -> Kit {
    Kit::new("tiny", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &tiny::DESCRIPTOR).words(tiny::report))
        .title("Tiny flows on Edge Lang")
        .limits(Limits { ops: STEPS, ..Limits::sandbox() })
        .command(run::Run)
        .command(test::Test)
        .field(perms::FIELD)
        .access(Arc::new(perms::Perms))
        .module(net)
        .module(Arc::new(files::Files))
}
