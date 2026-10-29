use cli::cmd::{run, test};
use cli::{Kit, Runtime};

// the module build.rs copies from the reference runtime beside its precompile
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edge.wasm"));

/* two commands over a runtime the kit compiles on its first load */
fn main() {
    Kit::new("runner", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &mini::DESCRIPTOR))
        .command(run::Run)
        .command(test::Test)
        .main()
}
