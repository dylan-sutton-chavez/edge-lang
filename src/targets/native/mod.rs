#[cfg(feature = "std")]
mod script;
#[cfg(feature = "std")]
pub use script::{Script, main, print};

/* installs the meter over the system allocator or a named one as the global allocator */
#[macro_export]
macro_rules! allocator {
    () => { $crate::allocator!(::std::alloc::System = ::std::alloc::System); };
    ($inner:ty = $value:expr) => {
        #[global_allocator]
        static EDGE_ALLOCATOR: $crate::value::meter::Metered<$inner> = $crate::value::meter::Metered($value);
    };
}
