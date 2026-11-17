// the tags ops and error kinds the engine its hosts and plugins share
mod wire;
pub use wire::*;

// the handle table and classifiers a host answers plugins with
mod handles;
pub use handles::*;

// heap values as wire trees and back for a host that runs the vm in its own process
mod values;
pub use values::{error_from_kind, from_wire, to_wire};

/* the host bridge behind the six plugin imports in the wasm runtime */
#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
pub mod bridge;
