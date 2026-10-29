#[cfg(feature = "net")]
// net reaches the kit by its public paths as a module outside the crate would
extern crate self as cli;

pub mod cmd;
// the docs convention a package ships its pages under checked at build time
#[cfg(feature = "docs")]
pub mod docs;
// the local files a package holds read in chunks as an opt-in host module
#[cfg(feature = "files")]
pub mod files;
pub mod host;
// the core a composition fills with a runtime and the components it wires
pub mod kit;
// what each declared version resolves to written by `edge lock` and read by every run
pub mod lock;
pub mod manifest;
// requests and sockets to the hosts a grant names as an opt-in host module
#[cfg(feature = "net")]
#[path = "../../examples/net/net.rs"]
pub mod net;
pub mod pack;
// the grants each manifest writes held from the root down as an opt-in component
#[cfg(feature = "perms")]
pub mod perms;
// plain terminal output without colors
pub mod ui;
// embedded browser host assets shared by a packed dist and a headless run
pub mod web;

pub use host::{Access, Held};
pub use kit::{Command, Descriptor, Field, Kit, Limits, Runtime};
