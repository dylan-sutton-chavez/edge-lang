mod common;

// Every run meters what it allocates, as compiler.wasm does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

mod abi;
mod lexer;
mod parser;
mod vm;
mod modules;
mod snapshot;
mod coverage;
mod embed;
mod memory;
