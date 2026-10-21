// The VM's handlers, frames and entry, kept as the symbols the glue calls.
pub use compiler::native::*;

// Every allocation counts toward the program's memory limit, as compiler.wasm meters it.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

// Nothing unwinds under panic=abort, yet musl's compiler builtins name the personality routine, so it exists and never runs.
#[cfg(panic = "abort")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() { std::process::abort() }
