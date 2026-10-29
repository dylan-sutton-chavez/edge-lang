#![cfg_attr(not(feature = "std"), no_std)]
// unsafe code lives in r#unsafe alone as the lint gate checks
#![deny(unsafe_code)]

extern crate alloc;

#[allow(unsafe_code)] pub(crate) mod r#unsafe;

pub mod abi {
    // the wire format keeps its file at src/abi.rs
    #[path = "../abi.rs"]
    mod wire;
    pub use wire::*;

    // heap values as wire trees and back for a host that runs the vm in its own process
    mod values;
    pub use values::{error_from_kind, from_wire, to_wire};

    /* the host bridge behind the six plugin imports in the wasm runtime */
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    pub mod bridge;
}

/* the wasm runtime edge.wasm builds and what a native binary boots a runtime with */
pub mod targets;

/* internal engine helpers apart from the pipeline code and from any runtime library */
pub mod util;

/* the NaN-boxed values and the mark-and-sweep heap under both the frontend and the VM */
pub mod value;

/* the natives a runtime gives the engine and the steps they ask of it */
pub mod lang;

// public only through the internals feature for the engine's own tests and tools
macro_rules! internal {
    ($($(#[$a:meta])* $m:ident;)*) => { $(
        $(#[$a])* #[cfg(feature = "internals")] pub mod $m;
        $(#[$a])* #[cfg(not(feature = "internals"))] pub(crate) mod $m;
    )* };
}

internal! {
    syntax;
}
pub use syntax::lexer;
internal! {
    vm;
}
pub mod deps;

// source to bytecode and the facts of compiled code read without running it
pub mod code;

// a VM to embed and run and snapshot with host modules and natives
pub mod embed {
    pub use crate::vm::VM;
    pub use crate::vm::snapshot::{SnapErr, save, save_swept, restore, source_of, entry_of, runtime_of, saved_under, limits_of, inspect_globals, inspect_stack, RUNTIME_AT, OTHER_RUNTIME};
    pub use crate::value::{Limits, SchedulerStatus};
    pub use crate::deps::{NativeBinding, Resolved, Resolver};
    #[cfg(feature = "std")]
    pub use crate::vm::calling::{calling, refuse, Calling};

    use alloc::vec::Vec;
    use crate::value::{Val, VmErr};

    /* calls `name` or `kind.method` on plain values and gives None once a frame opened */
    pub fn call_native(vm: &mut VM<'_>, name: &str, recv: Val, args: &[Val], kw: &[(&str, Val)]) -> Result<Option<Val>, VmErr> {
        let _paying = vm.paying()?;
        let (kind, method) = name.split_once('.').unwrap_or(("", name));
        let found = match (kind.is_empty(), vm.natives.names().kind_named(kind)) {
            (true, _) => vm.natives.find(name).map(|n| (n, None)),
            (false, Some(k)) => vm.kind_method(k, method).map(|(n, owner)| (n, Some(owner))),
            (false, None) => vm.natives.foreign_method(kind, method).map(|n| (n, Some(crate::lang::KindId::Foreign))),
        };
        let Some((native, owner)) = found else { return Err(VmErr::Runtime("a call names a native this runtime does not give")) };
        // a receiver goes first to check its kind and a type binds itself if asked
        let (recv, pos): (Val, Vec<Val>) = if !recv.is_undef() {
            (Val::undef(), core::iter::once(recv).chain(args.iter().copied()).collect())
        } else if owner.is_some() && native.on_class() {
            (vm.heap.type_object(kind)?, args.to_vec())
        } else {
            (recv, args.to_vec())
        };
        let mut flat = Vec::with_capacity(kw.len() * 2);
        for &(k, v) in kw { flat.extend([vm.heap.intern_str(k)?, v]); }
        let held = core::iter::once(recv).chain(pos.iter().copied()).chain(flat.iter().copied());
        let ran = vm.with_roots(held, |vm| {
            vm.job_ok = true;
            vm.native_value_call(recv, native, owner, &pos, &flat)
        });
        ran.map_err(|e| vm.word(e))
    }
}

// unstable internals for the engine's own tests and tools
#[cfg(feature = "internals")]
pub mod internals {
    pub use crate::{syntax, vm};
    pub use crate::code::fold;
}
