use interpreter_cli::cmd::{actors, build, init, pkg, run, serve, test, uninstall};
use interpreter_cli::{docs, files, manifest, net, perms, Kit, Runtime};
use std::sync::Arc;

// the Lox runtime of examples/lox which this binary embeds
const INTERPRETER_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/interpreter.wasm"));
const INTERPRETER_CWASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/interpreter.cwasm"));

/* the interpreter binary as one composition of the kit over the reference runtime */
fn interpreter() -> Kit {
    // SAFETY build.rs precompiled interpreter.cwasm from interpreter.wasm under the host's own engine config
    let runtime = unsafe { Runtime::precompiled(INTERPRETER_WASM, INTERPRETER_CWASM, &interpreter_lox::DESCRIPTOR) };
    Kit::new("interpreter", env!("CARGO_PKG_VERSION"), runtime)
        .title(format!("The interpreter developer CLI for {}", interpreter_lox::DESCRIPTOR.language))
        .site("github.com/dylan-sutton-chavez/interpreter")
        .upgrade("git pull in your interpreter clone, then make wasm-cli js lucide and cargo install --path cli")
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

#[cfg(test)]
mod tests {
    use super::*;

    /* an accidental precompile would pass every path check yet fail in the page */
    #[test]
    fn the_embedded_runtime_is_a_wasm_module() {
        assert_eq!(&INTERPRETER_WASM[..4], b"\0asm");
        assert_eq!(interpreter().runtime().module(), Some(INTERPRETER_WASM));
    }
}
