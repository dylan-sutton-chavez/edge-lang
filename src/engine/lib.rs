#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub(crate) mod fail;
pub(crate) mod fmt;
pub(crate) mod kernel;
pub(crate) mod lang;
pub(crate) mod uni;

pub use kernel::{Kernel, Site};
pub use lang::Lang;

#[cfg(feature = "bytes")]
pub(crate) mod bytes;
#[cfg(feature = "data")]
pub(crate) mod data;
#[cfg(feature = "date")]
pub(crate) mod date;
#[cfg(feature = "io")]
pub(crate) mod io;
#[cfg(feature = "iter")]
pub(crate) mod iter;
#[cfg(feature = "map")]
pub(crate) mod map;
#[cfg(feature = "num")]
pub(crate) mod num;
#[cfg(feature = "types")]
pub(crate) mod reflect;
#[cfg(feature = "seq")]
pub(crate) mod seq;
#[cfg(feature = "set")]
pub(crate) mod set;
#[cfg(feature = "text")]
pub(crate) mod text;
pub(crate) mod types;
#[cfg(feature = "bytes")]
pub(crate) mod units;

/* the `Language` methods a runtime takes from lib as associated items of its impl */
#[doc(hidden)]
#[macro_export]
macro_rules! __lib_hooks {
    ($lang:ty) => { $crate::__lib_hooks!($lang; show, format, handles); };
    ($lang:ty; $($hook:ident),+ $(,)?) => { $($crate::__lib_hooks!(@ $lang, $hook);)+ };
    (@ $lang:ty, show) => {
        fn show(cx: &mut $crate::lang::Cx<'_, '_>, v: $crate::Val, mode: $crate::lang::Show) -> ::core::result::Result<$crate::__private::String, $crate::VmErr> { $crate::lib::fmt::show::<$lang>(cx, v, mode) }
    };
    (@ $lang:ty, format) => {
        fn format(cx: &mut $crate::lang::Cx<'_, '_>, v: $crate::Val, spec: &str) -> ::core::result::Result<$crate::__private::String, $crate::VmErr> { $crate::lib::fmt::format::<$lang>(cx, v, spec) }
    };
    (@ $lang:ty, handles) => {
        fn handles(heap: &$crate::value::HeapPool, exc: $crate::Val, class: $crate::Val) -> ::core::result::Result<bool, $crate::VmErr> { $crate::lib::types::handles::<$lang>(heap, exc, class) }
    };
}

/* a runtime's natives each a kernel built for its entry and named by it */
#[doc(hidden)]
#[macro_export]
macro_rules! __lib_table {
    ($lang:ty; $($name:literal => $($seg:ident)::+ $([$($v:ty),+ $(,)?])? $(($($p:literal),* $(,)?))? $({$($k:literal),* $(,)?})?),* $(,)?) => {
        [$({
            struct Entry;
            impl $crate::lib::Site for Entry {
                type Lang = $lang;
                const NAME: &'static str = $name;
                const PARAMS: &'static [&'static str] = &[$($($p),*)?];
                const KEYWORDS: &'static [&'static str] = &[$($($k),*)?];
            }
            $crate::__private::registered($($seg)::+::<Entry $($(, $v)+)?>(), $name, <Entry as $crate::lib::Site>::PARAMS, <Entry as $crate::lib::Site>::KEYWORDS)
        }),*]
    };
}
