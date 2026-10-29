pub mod native;

#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
pub mod web;
