#[cfg(feature = "std")]
// the native host reads the files and arguments it was handed
#[allow(clippy::disallowed_methods)]
mod script;
#[cfg(feature = "std")]
pub use script::{Script, main};

/* installs the meter over the system allocator or a named one as the global allocator */
#[macro_export]
macro_rules! allocator {
    () => { $crate::allocator!(::std::alloc::System = ::std::alloc::System); };
    ($inner:ty = $value:expr) => {
        #[global_allocator]
        static METERED_ALLOCATOR: $crate::__private::Metered<$inner> = $crate::__private::Metered($value);
    };
}
