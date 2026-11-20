pub(crate) mod cmd;
// the docs convention a package ships its pages under checked at build time
#[cfg(feature = "cli-docs")]
pub(crate) mod docs;
// the local files a package holds read in chunks as an opt-in host module
#[cfg(feature = "cli-files")]
pub(crate) mod files;
pub(crate) mod host;
// the core a composition fills with a runtime and the components it wires
pub(crate) mod kit;
// what each declared version resolves to written by `interpreter lock` and read by every run
pub(crate) mod lock;
pub(crate) mod manifest;
// requests and sockets to the hosts a grant names as an opt-in host module
#[cfg(feature = "cli-net")]
#[path = "../../examples/net/net.rs"]
pub(crate) mod net;
pub(crate) mod pack;
// the grants each manifest writes held from the root down as an opt-in component
#[cfg(feature = "cli-perms")]
pub(crate) mod perms;
// plain terminal output without colors
pub(crate) mod ui;
// embedded browser host assets shared by a packed dist and a headless run
pub(crate) mod web;

pub use host::Access;
pub use kit::{Command, Field, Kit, Runtime};
pub use lock::LockFile;
#[cfg(test)]
use kit::Descriptor;

/* the lox names the kit's own tests run under read from its module in this build of the engine types */
#[cfg(test)]
pub(crate) static LOX: std::sync::LazyLock<&'static Descriptor> = std::sync::LazyLock::new(|| host::describe(kit::tests::WASM).expect("the lox module names itself"));
