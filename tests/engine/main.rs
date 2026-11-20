#![allow(unsafe_code, clippy::disallowed_methods, clippy::disallowed_types)]

mod common;
mod cases;

// every run meters what it allocates as interpreter.wasm does
interpreter::allocator!();

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
mod language;
mod policy;
mod rules;
mod ops;
mod grammar;
mod words;
mod threads;
// the paths railsbit and its runtimes name stay public
mod downstream;
// the user macros make api does not see keep their paths while plugin/example keeps their input
mod macros;
// what a host reads of a run and keeps on a machine for its natives
mod hosting;
