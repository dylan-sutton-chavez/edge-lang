#![allow(dead_code)]

// the kit's public api under the crate root names the commands reach for
pub use interpreter_cli::*;

// every command compiled again outside the kit where only its public api resolves
#[path = "../src"]
mod commands {
    pub mod cmd;
}

fn main() {}
