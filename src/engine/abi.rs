// the tags ops and error kinds the engine its hosts and plugins share
mod wire;
pub use wire::*;

// the handle table and classifiers a host answers plugins with
mod handles;
pub use handles::*;

// heap values as wire trees and back for a host that runs the vm in its own process
pub(crate) mod values;

/* the host bridge behind the six plugin imports in the wasm runtime */
#[cfg(feature = "exports")]
pub(crate) mod bridge;
