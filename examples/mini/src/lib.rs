#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use edge::lang::{Descriptor, ErrKind, Front, Natives, Runtime};

mod codecs;
pub mod lang;
mod language;
mod names;
pub mod natives;
mod style;
pub mod types;
pub mod words;

pub use names::NAMES;
// the text type `over!` names in the crate it expands in
#[doc(hidden)]
pub use alloc::string::String as __String;
use natives::*;

pub static NATIVES: Natives = edge::natives! {
    language: Mini,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, DATE],
    methods: [
        Str => [STR],
        Bytes => [BUFFER],
        ByteArray => [ARRAY],
        List => [LIST, LIST_SORT],
        Tuple => [TUPLE],
        Dict => [DICT],
        Set => [SET, SET_MANY],
        Int => [INT],
        Float => [FLOAT],
        Range => [RANGE],
        Coro => [GEN],
    ],
    types: types::NAMES,
};

pub static RUNTIME: Runtime = Runtime::of::<Mini>();

// mini reads every source through its own token table
pub const FRONT: Front = Front::spelled::<Mini>();

mod syntax;
pub use syntax::{Mini, MINI, INTERP, LADDER, NUMBERS, STRINGS};

// the names the CLI gives this runtime's files and errors
pub const DESCRIPTOR: Descriptor = Descriptor {
    language: "mini",
    extension: MINI.extension,
    entries: &["main.mini", "app.mini", "index.mini"],
    test_suffix: "_test.mini",
    scaffold: "print(\"hello from edge\")\n",
    example_fence: "edge",
    manifest_fence: "edge-manifest",
    type_error: names::ERRORS[ErrKind::Type as usize],
    value_error: names::ERRORS[ErrKind::Value as usize],
    runtime_error: names::ERRORS[ErrKind::Runtime as usize],
    denied_error: names::ERRORS[ErrKind::Permission as usize],
    failed_error: "OSError",
};

#[cfg(all(target_arch = "wasm32", feature = "web"))]
edge::web_exports!(RUNTIME);
