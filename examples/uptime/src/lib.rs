#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use edge::lang::{Descriptor, Front, Runtime};

mod language;
mod names;
mod natives;
mod syntax;

pub use names::NAMES;
pub use natives::NATIVES;
pub use syntax::{Uptime, UPTIME};

pub static RUNTIME: Runtime = Runtime::of::<Uptime>();

// every source reads through the token table of syntax.rs
pub const FRONT: Front = Front::spelled::<Uptime>();

// the names the CLI gives this runtime's files and errors
pub const DESCRIPTOR: Descriptor = Descriptor {
    language: "uptime",
    extension: UPTIME.extension,
    entries: &["main.up"],
    test_suffix: "_test.up",
    scaffold: "print(\"up\")\n",
    example_fence: "uptime",
    manifest_fence: "uptime-manifest",
    type_error: "WrongType",
    value_error: "BadData",
    runtime_error: "Error",
    denied_error: "Denied",
    failed_error: "Unreachable",
};

#[cfg(all(target_arch = "wasm32", feature = "web"))]
edge::web_exports!(RUNTIME);
