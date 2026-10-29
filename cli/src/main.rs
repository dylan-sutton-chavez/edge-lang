use cli::cmd::{actors, build, init, pkg, publish, run, serve, test, uninstall};
use cli::{docs, files, net, perms, Kit, Runtime};
use std::sync::Arc;

// the reference runtime built from examples/mini which this binary embeds
const EDGE_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edge.wasm"));
const EDGE_CWASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edge.cwasm"));

/* the edge binary as one composition of the kit over the reference runtime */
fn edge() -> Kit {
    // SAFETY build.rs precompiled edge.cwasm from edge.wasm under the host's own engine config
    let runtime = unsafe { Runtime::precompiled(EDGE_WASM, EDGE_CWASM, &mini::DESCRIPTOR) };
    Kit::new("edge", env!("CARGO_PKG_VERSION"), runtime)
        .title(format!("The Edge Lang developer CLI for {}", mini::DESCRIPTOR.language))
        .site("edgelang.dev")
        .origin("https://cdn.edgelang.dev")
        .registry("https://edgelang.dev")
        .upgrade("curl -fsSL https://cdn.edgelang.dev/cli/install.sh | sh")
        .command(run::Run)
        .command(build::Build)
        .command(actors::Actor)
        .command(serve::Serve)
        .command(test::Test)
        .command(init::Init)
        .command(publish::Publish)
        .command(pkg::Add)
        .command(pkg::Remove)
        .command(pkg::Lock)
        .command(uninstall::Uninstall)
        .fields(publish::FIELDS)
        .field(docs::FIELD)
        .field(perms::FIELD)
        .module(Arc::new(net::Net))
        .module(Arc::new(files::Files))
        .access(Arc::new(perms::Perms))
}

fn main() {
    edge().main()
}

#[cfg(test)]
mod tests {
    use super::*;

    /* an accidental precompile would pass every path check yet fail in the page */
    #[test]
    fn the_embedded_runtime_is_a_wasm_module() {
        assert_eq!(&EDGE_WASM[..4], b"\0asm");
        assert_eq!(edge().runtime().module(), Some(EDGE_WASM));
    }
}
