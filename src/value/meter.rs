#![deny(unsafe_code)]

use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

// a meter reports to its instance per this much growth and when a run ends
const STEP: usize = 1 << 20;

/* what one interpreter holds through the allocator and the part its instance has counted */
pub struct Meter {
    held: AtomicUsize,
    // the most it held before it last went down so growing costs no peak check
    peak: AtomicUsize,
    // growth past it tells the instance so each block pays one compare
    mark: AtomicUsize,
    told: AtomicUsize,
    limit: usize,
    // the limit or zero once its growth took the instance past its budget
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

    /* past its limit or grown while its instance is over the budget */
    #[inline]
    pub fn over(&self) -> bool { self.held() > self.cap.load(Relaxed) }

    /* what it may still take before its instance reaches the budget */
    pub fn instance_room(&self) -> usize {
        budget().saturating_sub(total().wrapping_add(self.held().saturating_sub(self.told.load(Relaxed))))
    }

    /* updates the instance total and squeezes the cap when its growth passed the budget */
    pub(crate) fn tell(&self) {
        let held = self.held();
        let told = self.told.swap(held, Relaxed);
        let total = add_total(held.wrapping_sub(told));
        self.mark.store(held.saturating_add(STEP), Relaxed);
        if total <= budget() { self.cap.store(self.limit, Relaxed); } else if held > told { self.cap.store(0, Relaxed); }
    }

    /* takes what it held out of the instance total once its program is gone */
    pub(crate) fn retire(&self) {
        add_total(0usize.wrapping_sub(self.told.swap(0, Relaxed)));
        self.mark.store(usize::MAX, Relaxed);
    }

    #[inline]
    pub(crate) fn grew(&self, n: usize) {
        let held = self.held.fetch_add(n, Relaxed) + n;
        if held > self.mark.load(Relaxed) { self.tell(); }
    }

    #[inline]
    pub(crate) fn shrank(&self, n: usize) {
        let held = self.held.fetch_sub(n, Relaxed);
        if held > self.peak.load(Relaxed) { self.peak.store(held, Relaxed); }
    }
}

// pays for what is allocated outside every run and stays out of the instance budget
static HOST: Meter = Meter { mark: AtomicUsize::new(usize::MAX), ..Meter::new(usize::MAX) };

// the instance is the wasm module or a std thread whose programs share one budget
#[cfg(feature = "std")]
std::thread_local! {
    static PAYER: core::cell::Cell<Option<&'static Meter>> = const { core::cell::Cell::new(None) };
    static TOTAL: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static BUDGET: core::cell::Cell<usize> = const { core::cell::Cell::new(isize::MAX as usize) };
}

#[cfg(not(feature = "std"))]
static PAYER: crate::r#unsafe::Payer = crate::r#unsafe::Payer::new(&HOST);

#[cfg(not(feature = "std"))]
static TOTAL: AtomicUsize = AtomicUsize::new(0);

// the most its programs may hold together and half the address space by default
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

/* the most this instance's programs hold before the growing one raises a memory error */
pub fn set_budget(bytes: usize) {
    let bytes = bytes.min(isize::MAX as usize);
    #[cfg(feature = "std")]
    BUDGET.with(|b| b.set(bytes));
    #[cfg(not(feature = "std"))]
    BUDGET.store(bytes, Relaxed);
}

#[inline]
fn swap_payer(m: &'static Meter) -> &'static Meter {
    #[cfg(feature = "std")]
    { PAYER.replace(Some(m)).unwrap_or(&HOST) }
    #[cfg(not(feature = "std"))]
    { PAYER.swap(m) }
}

#[inline]
fn payer() -> &'static Meter {
    #[cfg(feature = "std")]
    { PAYER.get().unwrap_or(&HOST) }
    #[cfg(not(feature = "std"))]
    { PAYER.get() }
}

/* charges `n` new bytes to the payer and gives that meter for the block's tag */
#[inline]
pub(crate) fn charge(n: usize) -> &'static Meter {
    let m = payer();
    m.grew(n);
    m
}

/* charges this thread's memory to `m` until dropped then reports `m` and restores the payer */
pub struct Paying(&'static Meter, &'static Meter, core::marker::PhantomData<*const ()>);

impl Paying {
    #[inline]
    pub fn to(m: &'static Meter) -> Self { Self(swap_payer(m), m, core::marker::PhantomData) }
}

impl Drop for Paying {
    #[inline]
    fn drop(&mut self) {
        swap_payer(self.0);
        self.1.tell();
    }
}

/* whether `Metered` is the global allocator as one block paid to a probe meter shows */
pub fn installed() -> bool {
    use core::sync::atomic::AtomicU8;
    static PROBE: Meter = Meter::new(usize::MAX);
    // zero while unknown then one when metered and two when not
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

/* each block carries the meter it was charged to and its free credits that meter */
pub struct Metered<A>(pub A);

/* under Miri an address table holds the meter so Stacked Borrows lets each block go */
#[cfg(miri)]
pub(crate) struct Owners { pub(crate) slots: &'static mut [(usize, Option<&'static Meter>)], pub(crate) len: usize }

#[cfg(miri)]
impl Owners {
    fn home(&self, addr: usize) -> usize { (addr.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 7) & (self.slots.len() - 1) }

    /* the slot of `addr` or the empty slot where it would go */
    fn find(&self, addr: usize) -> usize {
        let mut i = self.home(addr);
        while self.slots[i].0 != addr && self.slots[i].0 != 0 { i = (i + 1) & (self.slots.len() - 1); }
        i
    }

    /* records `m` for `addr` in slots from `inner` which keep the table off the meter */
    fn put(&mut self, inner: &impl core::alloc::GlobalAlloc, addr: usize, m: &'static Meter) {
        if (self.len + 1) * 2 > self.slots.len() {
            let grown = crate::r#unsafe::table(inner, (self.slots.len() * 2).max(1024));
            let old = core::mem::replace(&mut self.slots, grown);
            self.len = 0;
            for &(a, m) in old.iter() { if let Some(m) = m { self.put(inner, a, m); } }
        }
        let i = self.find(addr);
        if self.slots[i].0 == 0 { self.len += 1; }
        self.slots[i] = (addr, Some(m));
    }

    /* removes `addr` shifting its run back so lookups still stop at an empty slot */
    fn take(&mut self, addr: usize) -> &'static Meter {
        let mut i = self.find(addr);
        let (key, m) = self.slots[i];
        let m = m.filter(|_| key == addr).expect("a block the meter never handed out");
        self.len -= 1;
        let mut j = i;
        loop {
            self.slots[i] = (0, None);
            loop {
                j = (j + 1) & (self.slots.len() - 1);
                let a = self.slots[j].0;
                if a == 0 { return m; }
                let home = self.home(a);
                // an entry stays when its home lies cyclically after i and at most at j
                if if i <= j { i < home && home <= j } else { i < home || home <= j } { continue; }
                self.slots[i] = self.slots[j];
                i = j;
                break;
            }
        }
    }
}

/* records that the block at `addr` from `inner` was charged to `m` */
#[cfg(miri)]
pub(crate) fn own(inner: &impl core::alloc::GlobalAlloc, addr: usize, m: &'static Meter) { crate::r#unsafe::owners(|o| o.put(inner, addr, m)) }

/* the meter the block at `addr` was charged to which the table then forgets */
#[cfg(miri)]
pub(crate) fn disown(addr: usize) -> &'static Meter { crate::r#unsafe::owners(|o| o.take(addr)) }
