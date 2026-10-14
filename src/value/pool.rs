use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const STEP: usize = 8;
const CLASSES: usize = 32;
// The most blocks a class keeps, the rest go back to the inner allocator.
const KEEP: u16 = 256;

/* Freed small blocks kept by size class, reused before the inner allocator is asked again, for one thread only. */
pub struct Pooled<A> {
    inner: A,
    lists: UnsafeCell<([*mut u8; CLASSES], [u16; CLASSES])>,
}

// Only a wasm module without threads may share it, compiler.wasm installs it there.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<A: Sync> Sync for Pooled<A> {}

impl<A> Pooled<A> {
    pub const fn new(inner: A) -> Self { Self { inner, lists: UnsafeCell::new(([core::ptr::null_mut(); CLASSES], [0; CLASSES])) } }
}

// The bytes dlmalloc puts in front of a block, so a class of 8k + 4 bytes fills its chunk.
const FRONT: usize = 4;

/* The class of a block small enough to keep, the size every block of that class takes. */
#[inline(always)]
fn class(l: Layout) -> Option<usize> {
    if l.align() > STEP || l.size() > STEP * CLASSES + FRONT { None } else { Some(l.size().saturating_sub(FRONT + 1) / STEP) }
}

#[inline(always)]
fn class_layout(c: usize) -> Layout {
    unsafe { Layout::from_size_align_unchecked((c + 1) * STEP + FRONT, STEP) }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for Pooled<A> {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let Some(c) = class(l) else { return unsafe { self.inner.alloc(l) } };
        let (head, kept) = unsafe { &mut *self.lists.get() };
        let p = head[c];
        if p.is_null() { return unsafe { self.inner.alloc(class_layout(c)) }; }
        head[c] = unsafe { p.cast::<*mut u8>().read() };
        kept[c] -= 1;
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if class(l).is_none() { return unsafe { self.inner.alloc_zeroed(l) }; }
        let p = unsafe { self.alloc(l) };
        if !p.is_null() { unsafe { p.write_bytes(0, l.size()) }; }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let Some(c) = class(l) else { return unsafe { self.inner.dealloc(p, l) } };
        let (head, kept) = unsafe { &mut *self.lists.get() };
        if kept[c] == KEEP { return unsafe { self.inner.dealloc(p, class_layout(c)) }; }
        unsafe { p.cast::<*mut u8>().write(head[c]) };
        head[c] = p;
        kept[c] += 1;
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let grown = unsafe { Layout::from_size_align_unchecked(size, l.align()) };
        match (class(l), class(grown)) {
            // The block already has room for any size of its class.
            (Some(a), Some(b)) if a == b => p,
            (None, None) => unsafe { self.inner.realloc(p, l, size) },
            _ => unsafe {
                let q = self.alloc(grown);
                if !q.is_null() {
                    core::ptr::copy_nonoverlapping(p, q, l.size().min(size));
                    self.dealloc(p, l);
                }
                q
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::System;

    // Blocks of every class come back, move across classes and keep what they held.
    #[test]
    fn blocks_are_reused_and_keep_their_bytes() {
        let pool = Pooled::new(System);
        unsafe {
            let mut live = std::vec::Vec::new();
            for round in 0..3 {
                for size in (1..=300).step_by(7) {
                    let l = Layout::from_size_align(size, 8).unwrap();
                    let p = pool.alloc(l);
                    assert!(!p.is_null());
                    p.write_bytes(round as u8, size);
                    live.push((p, l));
                }
                for (p, l) in live.drain(..).rev() {
                    let q = pool.realloc(p, l, l.size() * 2);
                    assert!((0..l.size()).all(|i| *q.add(i) == round as u8));
                    pool.dealloc(q, Layout::from_size_align(l.size() * 2, 8).unwrap());
                }
            }
            let z = pool.alloc_zeroed(Layout::from_size_align(40, 8).unwrap());
            assert!((0..40).all(|i| *z.add(i) == 0));
            pool.dealloc(z, Layout::from_size_align(40, 8).unwrap());
            // Whatever the classes keep goes back, so Miri sees no leak.
            let (head, kept) = &mut *pool.lists.get();
            for c in 0..CLASSES {
                while !head[c].is_null() {
                    let p = head[c];
                    head[c] = p.cast::<*mut u8>().read();
                    kept[c] -= 1;
                    System.dealloc(p, class_layout(c));
                }
            }
        }
    }
}
