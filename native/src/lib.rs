// The VM's handlers, frames and entry, kept as the symbols the glue calls.
pub use compiler::native::*;

// Every allocation counts toward the program's memory limit, as compiler.wasm meters it.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);
