use cli::cmd::{run, test};
use cli::host::HostModule;
use cli::{perms, Kit, Runtime};
use std::sync::Arc;

mod clock;

pub use clock::{wall, Clock};

// the runtime make wasm-uptime builds from examples/uptime
const WASM: &[u8] = include_bytes!("../../../../target/wasm32-unknown-unknown/release/uptime.wasm");

/* run and test with net and the clock held to what edge.json grants */
pub fn kit(net: Arc<dyn HostModule>, clock: Arc<dyn HostModule>) -> Kit {
    Kit::new("uptime", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &uptime::DESCRIPTOR))
        .title("Uptime checks on Edge Lang")
        .command(run::Run)
        .command(test::Test)
        .field(perms::FIELD)
        .access(Arc::new(perms::Perms))
        .module(net)
        .module(clock)
}
