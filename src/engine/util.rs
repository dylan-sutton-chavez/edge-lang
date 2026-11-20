pub(crate) mod hash;
pub(crate) mod fstr;
pub(crate) mod jesc;
pub(crate) mod probe;
pub(crate) mod search;
pub mod sha256;
pub(crate) mod suggest;

/* leaks memory kept until exit */
pub fn leak<T: ?Sized>(boxed: alloc::boxed::Box<T>) -> &'static mut T {
    alloc::boxed::Box::leak(boxed)
}
