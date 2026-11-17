#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// every run meters what it allocates as interpreter.wasm does
interpreter::allocator!();

mod bytes;
mod choices;
mod codec;
mod convert;
mod data;
mod date;
mod iter;
mod map;
mod natives;
mod num;
mod render;
mod set;
mod sort;
mod spec;
mod steps;
mod text;
mod types;
