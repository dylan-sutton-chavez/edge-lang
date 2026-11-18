pub mod cmd;
// the docs convention a package ships its pages under checked at build time
#[cfg(feature = "cli-docs")]
pub mod docs;
// the local files a package holds read in chunks as an opt-in host module
#[cfg(feature = "cli-files")]
pub mod files;
pub mod host;
// the core a composition fills with a runtime and the components it wires
pub mod kit;
// what each declared version resolves to written by `interpreter lock` and read by every run
pub mod lock;
pub mod manifest;
// requests and sockets to the hosts a grant names as an opt-in host module
#[cfg(feature = "cli-net")]
#[path = "../examples/net/net.rs"]
pub mod net;
pub mod pack;
// the grants each manifest writes held from the root down as an opt-in component
#[cfg(feature = "cli-perms")]
pub mod perms;
// plain terminal output without colors
pub mod ui;
// embedded browser host assets shared by a packed dist and a headless run
pub mod web;

pub use host::{Access, Held};
pub use kit::{Command, Descriptor, Field, Kit, Limits, Runtime};

/* the lox names the kit's own tests run under in this build of the engine types */
#[cfg(test)]
pub(crate) fn lox() -> Descriptor {
    let d = &interpreter_lox::DESCRIPTOR;
    Descriptor {
        language: d.language,
        extension: d.extension,
        entries: d.entries,
        test_suffix: d.test_suffix,
        scaffold: d.scaffold,
        example_fence: d.example_fence,
        manifest_fence: d.manifest_fence,
        type_error: d.type_error,
        value_error: d.value_error,
        runtime_error: d.runtime_error,
        denied_error: d.denied_error,
        failed_error: d.failed_error,
        statuses: d.statuses,
        // a test links the engine twice so the words lox gives belong to the other build
        diagnosed: None,
    }
}

#[cfg(test)]
pub(crate) static LOX: std::sync::LazyLock<Descriptor> = std::sync::LazyLock::new(lox);
