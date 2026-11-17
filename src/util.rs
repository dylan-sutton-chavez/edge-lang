pub(crate) mod hash;
pub(crate) mod fstr;
pub(crate) mod jesc;
pub(crate) mod probe;
pub mod search;
pub mod sha256;
pub mod suggest;

/* leaks memory kept until exit as a static root Miri never reports */
pub fn leak<T: ?Sized>(boxed: alloc::boxed::Box<T>) -> &'static mut T {
    let leaked = alloc::boxed::Box::leak(boxed);
    #[cfg(miri)]
    crate::r#unsafe::miri_root(leaked);
    leaked
}
