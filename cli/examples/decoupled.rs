#![allow(dead_code)]

// the kit's public api under the crate root names the commands reach for
pub use cli::*;

// every command compiled again outside the kit where only its public api resolves
#[path = "../src/cmd/mod.rs"]
mod commands;

fn main() {}
