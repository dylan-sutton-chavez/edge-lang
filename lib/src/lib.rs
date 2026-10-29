#![no_std]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

extern crate alloc;

pub mod fail;
pub mod fmt;
pub mod kernel;
pub mod lang;
pub mod uni;

pub use fail::{Class, Fail};
pub use kernel::{Kernel, Site};
pub use lang::Lang;

#[cfg(feature = "bytes")]
pub mod bytes;
#[cfg(feature = "data")]
pub mod data;
#[cfg(feature = "date")]
pub mod date;
#[cfg(feature = "io")]
pub mod io;
#[cfg(feature = "iter")]
pub mod iter;
#[cfg(feature = "map")]
pub mod map;
#[cfg(feature = "num")]
pub mod num;
#[cfg(feature = "reflect")]
pub mod reflect;
#[cfg(feature = "seq")]
pub mod seq;
#[cfg(feature = "set")]
pub mod set;
#[cfg(feature = "text")]
pub mod text;
#[cfg(feature = "types")]
pub mod types;
#[cfg(feature = "bytes")]
pub mod units;

/* a runtime's natives each a kernel built for its entry and named by it */
#[macro_export]
macro_rules! table {
    ($lang:ty; $($name:literal => $($seg:ident)::+ $([$($v:ty),+ $(,)?])? $(($($p:literal),* $(,)?))? $({$($k:literal),* $(,)?})?),* $(,)?) => {
        [$({
            struct Entry;
            impl $crate::Site for Entry {
                type Lang = $lang;
                const NAME: &'static str = $name;
                const PARAMS: &'static [&'static str] = &[$($($p),*)?];
                const KEYWORDS: &'static [&'static str] = &[$($($k),*)?];
            }
            $($seg)::+::<Entry $($(, $v)+)?>()
                .params(<Entry as $crate::Site>::PARAMS)
                .keywords(<Entry as $crate::Site>::KEYWORDS)
                .named($name)
        }),*]
    };
}
