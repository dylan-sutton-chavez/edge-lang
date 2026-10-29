mod common;
mod cases;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

mod abi;
mod vm;
mod modules;
mod snapshot;
mod coverage;
mod embed;
mod memory;
mod nested;
mod tables;
mod syntax;
mod rows;
mod front;
mod names;
mod miri;
mod language;
mod policy;
mod rules;
mod ops;
