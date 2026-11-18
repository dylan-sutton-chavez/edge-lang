#![allow(dead_code, clippy::disallowed_methods, clippy::disallowed_types)]

// the public api of the crate under the root names the commands reach for
pub use interpreter::*;

// every command compiled again outside the kit where only its public api resolves
#[path = "../../src/cli"]
mod commands {
    pub mod cmd;
}

fn main() {}
