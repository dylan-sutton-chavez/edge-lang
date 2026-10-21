#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod abi;

/* Host bridge behind the six plugin imports, part of the wasm `runtime`. */
#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
pub mod bridge;

#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
pub mod wasm;

/* Internal compiler helpers (not Edge Python stdlib), separate from pipeline code. */
pub mod util {
    pub(crate) mod hash;
    pub(crate) mod fstr;
    pub(crate) mod jesc;
    pub mod sha256;
    pub(crate) mod uni;
    pub(crate) mod suggest;

    /* `Box::leak` for memory kept until exit, which Miri then holds as a static root instead of reporting it. */
    pub fn leak<T: ?Sized>(boxed: alloc::boxed::Box<T>) -> &'static mut T {
        let leaked = alloc::boxed::Box::leak(boxed);
        // A zero-sized value owns no allocation to root.
        #[cfg(miri)]
        if core::mem::size_of_val(leaked) != 0 {
            unsafe extern "Rust" {
                fn miri_static_root(ptr: *const ());
            }
            unsafe { miri_static_root(core::ptr::from_ref(leaked).cast()) };
        }
        leaked
    }
}

/* NaN-boxed values and the mark-and-sweep heap, the layer both the frontend and the VM build on. */
pub mod value;

// Public only through the internals feature for the engine's own tests and tools.
macro_rules! internal {
    ($($(#[$a:meta])* $m:ident;)*) => { $(
        $(#[$a])* #[cfg(feature = "internals")] pub mod $m;
        $(#[$a])* #[cfg(not(feature = "internals"))] pub(crate) mod $m;
    )* };
}

pub mod lexer;
internal! {
    parser;
    /* Post-SSA passes, run between parse and boot, touches no VM state. */
    optimizer;
    vm;
    /* The kinds Edge Python registers on the engine, the wide ints first. */
    frontend;
}
pub mod pure;
pub mod modules;

// Source to bytecode and the facts of compiled code read without running it.
pub mod code {
    pub use crate::parser::{
        Parser, SSAChunk, Instruction, OpCode, Value, ImportKind, ImportEntry, Diagnostic, Lines, SsaName, ssa_strip, param_base_name, unpack_call,
        COMP_ARG, SPREAD_BASES, effect, INPLACE, ITER_VALUE, ITER_AS_IS, ITER_ASYNC, YIELD_FROM, SPREAD_ARGS, KEYWORDS,
    };
    pub use crate::optimizer::constant_fold;
    pub use crate::vm::{Binding, BodyKind, FnScope, Kind, ParamKind};

    // Lexes, parses and folds `src` into a chunk and its errors.
    pub fn compile(src: &str) -> (SSAChunk, alloc::vec::Vec<Diagnostic>) {
        let (tokens, _) = crate::lexer::lex(src);
        let (mut chunk, errors) = Parser::new(src, tokens.into_iter()).parse();
        constant_fold(&mut chunk);
        (chunk, errors)
    }

    // Each body's scope and parameters resolved as the VM resolves them without running.
    pub struct Program<'c> { vm: crate::vm::VM<'c>, bodies: crate::util::hash::FxHashMap<*const SSAChunk, (BodyKind, alloc::vec::Vec<Binding>)> }

    impl<'c> Program<'c> {
        pub fn new(chunk: &'c SSAChunk) -> Self {
            let mut vm = crate::vm::VM::new(chunk);
            let bodies = vm.body_bindings(chunk);
            Self { vm, bodies }
        }
        // None for the module body.
        pub fn scope_of(&self, body: &SSAChunk) -> Option<(usize, &FnScope)> { self.vm.scope_of(body) }
        // Which body `body` is and the binding of each of its names, a module's, a class's or a function's, None for a chunk of another program.
        pub fn bindings_of(&self, body: &SSAChunk) -> Option<(BodyKind, &[Binding])> { self.bodies.get(&(body as *const SSAChunk)).map(|(k, b)| (*k, b.as_slice())) }
        // Kind and slot of each parameter in order.
        pub fn params_of(&self, fi: usize) -> &[(ParamKind, usize)] { self.vm.params_of(fi) }
    }
}

// Builtin types, methods and functions with arity and purity.
pub mod types {
    pub use crate::parser::{BUILTIN_TYPES, CLASS_DUNDERS, builtin_type, class_dunder};
    pub use crate::vm::{ALL_METHODS, BuiltinMethodId, MethodDesc, MethodKind, lookup_method, exc_parents};
    pub use crate::value::NativeFnId;
}

// A VM to embed, run and snapshot with host modules and natives.
pub mod embed {
    pub use crate::vm::VM;
    pub use crate::vm::snapshot::{SnapErr, save, restore, source_of, limits_of, inspect_globals, inspect_stack};
    pub use crate::value::{Limits, SchedulerStatus};
    pub use crate::modules::{NativeBinding, Resolved, Resolver};
}

// The opcode handlers and runtime `edge build` links native programs against.
#[cfg(feature = "native")]
pub mod native {
    pub use crate::vm::native::*;
}

// Unstable internals for the engine's own tests and tools.
#[cfg(feature = "internals")]
pub mod internals {
    pub use crate::{frontend, optimizer, parser, vm};
}
