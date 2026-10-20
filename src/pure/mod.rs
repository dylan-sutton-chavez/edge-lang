pub use crate::value::Host;

#[cfg(feature = "pure")]
mod plain;
#[cfg(feature = "pure")]
pub use plain::{Plain, PlainFn, PlainHost, Raised, call_pure, is_pure};
