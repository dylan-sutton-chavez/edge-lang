#![deny(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const STEP: usize = 8;
const CLASSES: usize = 32;
// the most blocks a class keeps before the rest go back to the inner allocator
pub(crate) const KEEP: u16 = 256;

/* freed small blocks of one thread kept by class and reused before the inner allocator */
pub struct Pooled<A: GlobalAlloc> {
    pub(crate) inner: A,
    pub(crate) lists: UnsafeCell<([*mut u8; CLASSES], [u16; CLASSES])>,
}

impl<A: GlobalAlloc> Pooled<A> {
    pub const fn new(inner: A) -> Self { Self { inner, lists: UnsafeCell::new(([core::ptr::null_mut(); CLASSES], [0; CLASSES])) } }
}

// bytes dlmalloc puts before a block so classes of 8k plus 4 fill whole chunks
const FRONT: usize = 4;

/* the class of a block small enough to keep or none for a larger one */
#[inline(always)]
pub(crate) fn class(l: Layout) -> Option<usize> {
    if l.align() > STEP || l.size() > STEP * CLASSES + FRONT { None } else { Some(l.size().saturating_sub(FRONT + 1) / STEP) }
}

/* the layout every block of class `c` takes which holds each layout of that class */
#[inline(always)]
pub(crate) fn class_layout(c: usize) -> Layout {
    Layout::from_size_align((c + 1) * STEP + FRONT, STEP).expect("a class below CLASSES")
}
