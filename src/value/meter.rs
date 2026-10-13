use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

// A meter tells its instance what it holds once per this much growth, and whenever its run ends.
const STEP: usize = 1 << 20;

/* What one interpreter holds through the allocator, the most it held, and what its instance counts for it. */
pub struct Meter {
    held: AtomicUsize,
    // The most it held before it last went down, so growing costs no peak check.
    peak: AtomicUsize,
    // Growth past it tells the instance, so each block pays one compare.
    mark: AtomicUsize,
    told: AtomicUsize,
    limit: usize,
    // The limit, or zero after its growth took the instance past its budget.
    cap: AtomicUsize,
}

impl Meter {
    pub(crate) const fn new(limit: usize) -> Self {
        Self { held: AtomicUsize::new(0), peak: AtomicUsize::new(0), mark: AtomicUsize::new(STEP), told: AtomicUsize::new(0), limit, cap: AtomicUsize::new(limit) }
    }

    #[inline]
    pub fn held(&self) -> usize { self.held.load(Relaxed) }

    #[inline]
    pub fn peak(&self) -> usize { self.peak.load(Relaxed).max(self.held()) }

    /* Whether it holds more than its limit, or grew while its instance is past the budget. */
    #[inline]
    pub fn over(&self) -> bool { self.held() > self.cap.load(Relaxed) }

    /* What it may still take before its instance reaches the budget. */
    pub fn instance_room(&self) -> usize {
        budget().saturating_sub(total().wrapping_add(self.held().saturating_sub(self.told.load(Relaxed))))
    }

    /* Brings the instance total to what it holds now, squeezing it if its growth passed the budget. */
    pub(crate) fn tell(&self) {
        let held = self.held();
        let told = self.told.swap(held, Relaxed);
        let total = add_total(held.wrapping_sub(told));
        self.mark.store(held.saturating_add(STEP), Relaxed);
        if total <= budget() { self.cap.store(self.limit, Relaxed); } else if held > told { self.cap.store(0, Relaxed); }
    }

    /* Takes what it held out of the instance total, once its program is gone. */
    pub(crate) fn retire(&self) {
        add_total(0usize.wrapping_sub(self.told.swap(0, Relaxed)));
        self.mark.store(usize::MAX, Relaxed);
    }

    #[inline]
    fn grew(&self, n: usize) {
        let held = self.held.fetch_add(n, Relaxed) + n;
        if held > self.mark.load(Relaxed) { self.tell(); }
    }

    #[inline]
    fn shrank(&self, n: usize) {
        let held = self.held.fetch_sub(n, Relaxed);
        if held > self.peak.load(Relaxed) { self.peak.store(held, Relaxed); }
    }
}

// Pays for what is allocated outside every interpreter run.
static HOST: Meter = Meter::new(usize::MAX);

// An instance is the wasm module, or the thread on a std host, and its programs share one budget.
#[cfg(feature = "std")]
std::thread_local! {
    static PAYER: core::cell::Cell<*const Meter> = const { core::cell::Cell::new(core::ptr::null()) };
    static TOTAL: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static BUDGET: core::cell::Cell<usize> = const { core::cell::Cell::new(isize::MAX as usize) };
}

#[cfg(not(feature = "std"))]
static PAYER: core::sync::atomic::AtomicPtr<Meter> = core::sync::atomic::AtomicPtr::new(&HOST as *const Meter as *mut Meter);

#[cfg(not(feature = "std"))]
static TOTAL: AtomicUsize = AtomicUsize::new(0);

// Half the address space by default, the most its programs may hold together.
#[cfg(not(feature = "std"))]
static BUDGET: AtomicUsize = AtomicUsize::new(isize::MAX as usize);

#[inline]
fn total() -> usize {
    #[cfg(feature = "std")]
    { TOTAL.with(|t| t.get()) }
    #[cfg(not(feature = "std"))]
    { TOTAL.load(Relaxed) }
}

fn add_total(delta: usize) -> usize {
    #[cfg(feature = "std")]
    { TOTAL.with(|t| { let total = t.get().wrapping_add(delta); t.set(total); total }) }
    #[cfg(not(feature = "std"))]
    { TOTAL.fetch_add(delta, Relaxed).wrapping_add(delta) }
}

#[inline]
fn budget() -> usize {
    #[cfg(feature = "std")]
    { BUDGET.with(|b| b.get()) }
    #[cfg(not(feature = "std"))]
    { BUDGET.load(Relaxed) }
}

/* Bounds what the programs of this instance hold together, past it the one that grows raises MemoryError. */
pub fn set_budget(bytes: usize) {
    let bytes = bytes.min(isize::MAX as usize);
    #[cfg(feature = "std")]
    BUDGET.with(|b| b.set(bytes));
    #[cfg(not(feature = "std"))]
    BUDGET.store(bytes, Relaxed);
}

#[inline]
fn swap_payer(m: *const Meter) -> *const Meter {
    #[cfg(feature = "std")]
    { PAYER.with(|p| p.replace(m)) }
    #[cfg(not(feature = "std"))]
    { PAYER.swap(m as *mut Meter, Relaxed) }
}

#[inline]
fn payer() -> &'static Meter {
    // Every meter is leaked, so a payer outlives each block it paid for.
    #[cfg(feature = "std")]
    { unsafe { PAYER.with(|p| p.get()).as_ref() }.unwrap_or(&HOST) }
    #[cfg(not(feature = "std"))]
    { unsafe { &*PAYER.load(Relaxed) } }
}

/* Charges new memory to `m` until it drops, then tells the instance what `m` holds and restores the payer before it. */
pub struct Paying(*const Meter, &'static Meter);

impl Paying {
    #[inline]
    pub fn to(m: &'static Meter) -> Self { Self(swap_payer(m), m) }
}

impl Drop for Paying {
    #[inline]
    fn drop(&mut self) {
        swap_payer(self.0);
        self.1.tell();
    }
}

/* Whether the global allocator is `Metered`, asked once by paying a block to a meter of its own. */
pub fn installed() -> bool {
    use core::sync::atomic::AtomicU8;
    static PROBE: Meter = Meter::new(usize::MAX);
    // Unknown, metered or not, in that order.
    static SEEN: AtomicU8 = AtomicU8::new(0);
    match SEEN.load(Relaxed) {
        0 => {
            let seen = {
                let _paying = Paying::to(&PROBE);
                let block = core::hint::black_box(alloc::boxed::Box::new(0u64));
                let seen = PROBE.held() > 0;
                drop(block);
                seen
            };
            SEEN.store(if seen { 1 } else { 2 }, Relaxed);
            seen
        }
        known => known == 1,
    }
}

/* Wraps an allocator so each block carries the meter it was charged to, and its free credits that meter. */
pub struct Metered<A>(pub A);

const TAG: usize = core::mem::size_of::<*const Meter>();

/* The block the inner allocator hands out for `l`, the tag in front keeping the alignment of `l`. */
#[inline(always)]
fn tagged(l: Layout) -> Option<(Layout, usize)> {
    let head = l.align().max(TAG);
    if l.size() > isize::MAX as usize - head { return None; }
    // A power of two and a size that stays under isize::MAX once rounded.
    Some((unsafe { Layout::from_size_align_unchecked(l.size() + head, head) }, head))
}

/* The block a live `l` came from, whose size passed `tagged` when it was made. */
#[inline(always)]
fn block(l: Layout) -> (Layout, usize) {
    let head = l.align().max(TAG);
    (unsafe { Layout::from_size_align_unchecked(l.size() + head, head) }, head)
}

impl<A: GlobalAlloc> Metered<A> {
    #[inline]
    unsafe fn place(&self, base: *mut u8, inner: Layout, head: usize) -> *mut u8 {
        if base.is_null() { return base; }
        let m = payer();
        m.grew(inner.size());
        unsafe {
            let p = base.add(head);
            p.sub(TAG).cast::<*const Meter>().write(m);
            p
        }
    }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for Metered<A> {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let Some((inner, head)) = tagged(l) else { return core::ptr::null_mut() };
        unsafe { self.place(self.0.alloc(inner), inner, head) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let Some((inner, head)) = tagged(l) else { return core::ptr::null_mut() };
        unsafe { self.place(self.0.alloc_zeroed(inner), inner, head) }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let (inner, head) = block(l);
        unsafe {
            (*p.sub(TAG).cast::<*const Meter>().read()).shrank(inner.size());
            self.0.dealloc(p.sub(head), inner);
        }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let (inner, head) = block(l);
        let Some(grown) = size.checked_add(head) else { return core::ptr::null_mut() };
        unsafe {
            let owner = p.sub(TAG).cast::<*const Meter>().read();
            let base = self.0.realloc(p.sub(head), inner, grown);
            if base.is_null() { return base; }
            // A grown block goes to the current payer, so a shared buffer charges who made it grow.
            (*owner).shrank(inner.size());
            let m = payer();
            m.grew(grown);
            let q = base.add(head);
            q.sub(TAG).cast::<*const Meter>().write(m);
            q
        }
    }
}
