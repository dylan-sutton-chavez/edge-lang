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
    pub mod hash;
    pub mod fstr;
    pub mod jesc;
    pub mod sha256;
    pub mod uni;
    pub mod suggest;

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

pub mod lexer;
pub mod parser;
/* Post-SSA passes, run between parse and boot, touches no VM state. */
pub mod optimizer;
pub mod vm;
/* The kinds Edge Python registers on the engine, the wide ints first. */
pub mod frontend;
pub mod modules;
