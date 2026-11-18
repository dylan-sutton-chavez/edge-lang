#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use interpreter::lang::{Descriptor, ErrKind, Front, Natives, Runtime};

mod codecs;
pub mod lang;
mod language;
mod names;
pub mod natives;
pub mod types;

pub use names::NAMES;
// the text type `over!` names in the crate it expands in
#[doc(hidden)]
pub use alloc::string::String as __String;

/* the rig's natives as `$lang` takes them with the fns `$f` and the method rows `$kind` past its own */
#[macro_export]
macro_rules! with {
    ($lang:ty $(, $f:expr)* $(; $($kind:ident => [$($m:expr),*]),*)?) => {
        ::interpreter::natives! {
            language: $lang,
            fns: [$crate::natives::IO, $crate::natives::ITER, $crate::natives::NEXT, $crate::natives::NUM, $crate::natives::FOLDS, $crate::natives::CONVERT, $crate::natives::BYTES, $crate::natives::SORT, $crate::natives::TYPES, $crate::natives::MATCH, $crate::natives::DATE $(, $f)*],
            methods: [
                Str => [$crate::natives::STR],
                Bytes => [$crate::natives::BUFFER],
                ByteArray => [$crate::natives::ARRAY],
                List => [$crate::natives::LIST, $crate::natives::LIST_SORT],
                Tuple => [$crate::natives::TUPLE],
                Dict => [$crate::natives::DICT],
                Set => [$crate::natives::SET, $crate::natives::SET_MANY],
                Int => [$crate::natives::INT],
                Float => [$crate::natives::FLOAT],
                Range => [$crate::natives::RANGE],
                $($($kind => [$($m),*],)*)?
            ],
            types: $crate::types::NAMES,
        }
    };
}

pub static NATIVES: Natives = with!(Rig);

pub static RUNTIME: Runtime = Runtime::of::<Rig>();

// rig reads every source through its own token table and grammar
pub const FRONT: Front = Front::spelled::<Rig>();

mod syntax;
pub use syntax::{Rig, RIG, INTERP, LADDER, NUMBERS, STRINGS};

// the names the CLI gives this runtime's files and errors
pub const DESCRIPTOR: Descriptor = Descriptor {
    language: "rig",
    extension: RIG.extension,
    entries: &["main.rig", "app.rig", "index.rig"],
    test_suffix: "_test.rig",
    scaffold: "print(\"hello from interpreter\")\n",
    example_fence: "rig",
    manifest_fence: "rig-manifest",
    type_error: names::ERRORS[ErrKind::Type as usize],
    value_error: names::ERRORS[ErrKind::Value as usize],
    runtime_error: names::ERRORS[ErrKind::Runtime as usize],
    denied_error: names::ERRORS[ErrKind::Permission as usize],
    failed_error: "IOError",
    ..Descriptor::NONE
};

#[cfg(all(target_arch = "wasm32", feature = "web"))]
interpreter::web_exports!(RUNTIME);
