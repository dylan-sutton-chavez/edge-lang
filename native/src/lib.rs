// The VM's handlers, frames and entry, kept as the symbols the glue calls. A `.wasm` has no std, so its imports are only
// the two its runtime calls, fd_write and proc_exit.
#![cfg_attr(target_arch = "wasm32", no_std)]

pub use compiler::native::*;

// Every allocation counts toward the program's memory limit, as compiler.wasm meters it.
#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);
#[cfg(target_arch = "wasm32")]
#[global_allocator]
static METERED: compiler::value::meter::Metered<dlmalloc::GlobalDlmalloc> = compiler::value::meter::Metered(dlmalloc::GlobalDlmalloc);

// A panic is an internal error, the program ending as an abort would.
#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { compiler::native::exit(101) }

// Nothing unwinds under panic=abort, yet musl's compiler builtins name the personality routine, so on Linux it exists and never runs.
#[cfg(all(panic = "abort", target_os = "linux"))]
#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() { std::process::abort() }
