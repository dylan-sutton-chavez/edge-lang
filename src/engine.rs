#[allow(unsafe_code)] pub(crate) mod r#unsafe;

pub(crate) mod abi;
pub(crate) mod code;
pub(crate) mod deps;
pub(crate) mod lang;
#[cfg(feature = "lib")]
pub(crate) mod lib;
#[cfg(feature = "plugin")]
pub(crate) mod plugin;
pub(crate) mod syntax;
pub(crate) mod targets;
pub(crate) mod util;
pub(crate) mod value;
pub(crate) mod vm;
// a tool beside the engine reads files and the environment the sandbox rule keeps from it
#[cfg(feature = "cli-kit")]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub(crate) mod cli;
