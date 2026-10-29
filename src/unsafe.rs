#![deny(unsafe_op_in_unsafe_fn)]

// only this file holds unsafe code and a SAFETY line argues each region
use core::alloc::{GlobalAlloc, Layout};
use crate::value::meter::{Meter, Metered, charge};
use crate::value::pool::{KEEP, Pooled, class, class_layout};
use crate::code::Ins;
use crate::value::Val;
use crate::vm::lower::Code;
use crate::syntax::chunk::SSAChunk;

// SAFETY alloc keeps each block's payer in a tag before it as a_run_gives_back_every_byte_once_its_vm_drops checks
const TAG: usize = core::mem::size_of::<&'static Meter>();

/* the inner block for `size` bytes at `align` behind the tag and where they start */
#[inline(always)]
fn tagged(size: usize, align: usize) -> Option<(Layout, usize)> {
    let head = align.max(TAG);
    // SAFETY head is a power of two and the rounded size fits an isize
    (size + head <= isize::MAX as usize + 1 - head).then(|| (unsafe { Layout::from_size_align_unchecked(size + head, head) }, head))
}

/* charges the new inner block at `base` to the payer named in its tag */
#[cfg(not(miri))]
#[inline]
unsafe fn place(base: *mut u8, inner: Layout, head: usize) -> *mut u8 {
    if base.is_null() { return base; }
    let m = charge(inner.size());
    unsafe { let p = base.add(head); p.sub(TAG).cast::<&'static Meter>().write(m); p }
}

// SAFETY each block gives the bytes past its tag and returns whole to its allocator
#[cfg(not(miri))]
unsafe impl<A: GlobalAlloc> GlobalAlloc for Metered<A> {
    #[inline] unsafe fn alloc(&self, l: Layout) -> *mut u8 { tagged(l.size(), l.align()).map_or(core::ptr::null_mut(), |(inner, head)| unsafe { place(self.0.alloc(inner), inner, head) }) }
    #[inline] unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 { tagged(l.size(), l.align()).map_or(core::ptr::null_mut(), |(inner, head)| unsafe { place(self.0.alloc_zeroed(inner), inner, head) }) }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let head = l.align().max(TAG);
        unsafe { p.sub(TAG).cast::<&'static Meter>().read().shrank(l.size() + head); self.0.dealloc(p.sub(head), Layout::from_size_align_unchecked(l.size() + head, head)) }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let Some((grown, head)) = tagged(size, l.align()) else { return core::ptr::null_mut() };
        unsafe {
            let owner = p.sub(TAG).cast::<&'static Meter>().read();
            let base = self.0.realloc(p.sub(head), Layout::from_size_align_unchecked(l.size() + head, head), grown.size());
            if base.is_null() { return base; }
            // the current payer takes a grown block so a shared buffer charges whoever grew it
            owner.shrank(l.size() + head);
            let q = base.add(head);
            q.sub(TAG).cast::<&'static Meter>().write(charge(grown.size()));
            q
        }
    }
}

#[cfg(miri)]
use crate::value::meter::{Owners, disown, own};

// SAFETY each call passes the caller's arguments on as they came
#[cfg(miri)]
unsafe impl<A: GlobalAlloc> GlobalAlloc for Metered<A> {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let Some((inner, _)) = tagged(l.size(), l.align()) else { return core::ptr::null_mut() };
        let p = unsafe { self.0.alloc(l) };
        if !p.is_null() { own(&self.0, p.addr(), charge(inner.size())); }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { disown(p.addr()).shrank(l.size() + l.align().max(TAG)); unsafe { self.0.dealloc(p, l) } }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let Some((grown, head)) = tagged(size, l.align()) else { return core::ptr::null_mut() };
        let owner = disown(p.addr());
        let q = unsafe { self.0.realloc(p, l, size) };
        if q.is_null() { own(&self.0, p.addr(), owner); return q; }
        owner.shrank(l.size() + head);
        own(&self.0, q.addr(), charge(grown.size()));
        q
    }
}

/* runs `f` on the owner table under a spin lock that Miri's test threads share */
#[cfg(miri)]
pub(crate) fn owners<R>(f: impl FnOnce(&mut Owners) -> R) -> R {
    use core::sync::atomic::{AtomicBool, Ordering::{Acquire, Release}};
    static LOCK: AtomicBool = AtomicBool::new(false);
    static mut OWNERS: Owners = Owners { slots: &mut [], len: 0 };
    while LOCK.swap(true, Acquire) { core::hint::spin_loop(); }
    // SAFETY the lock stays held until f returns so no other reference exists
    let r = f(unsafe { &mut *&raw mut OWNERS });
    LOCK.store(false, Release);
    r
}

/* `cap` empty owner slots from `inner` never freed so they outlive every block */
#[cfg(miri)]
pub(crate) fn table(inner: &impl GlobalAlloc, cap: usize) -> &'static mut [(usize, Option<&'static Meter>)] {
    let cap = cap.max(1);
    let l = Layout::array::<(usize, Option<&'static Meter>)>(cap).expect("owner table size");
    // SAFETY l is nonzero and its zeroed block holds cap empty slots nothing frees
    let t = unsafe { core::slice::from_raw_parts_mut(core::ptr::NonNull::new(inner.alloc_zeroed(l)).expect("owner table out of memory").as_ptr().cast(), cap) };
    miri_root(t);
    t
}

// SAFETY wasm without threads runs one call at a time and edge.wasm installs it there
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<A: Sync + GlobalAlloc> Sync for Pooled<A> {}

// SAFETY a kept block of class c fits each layout of c as pooled_blocks_come_back_with_their_bytes checks
unsafe impl<A: GlobalAlloc> GlobalAlloc for Pooled<A> {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY calls never overlap and a kept block's first word links the next one
        unsafe {
            let Some(c) = class(l) else { return self.inner.alloc(l) };
            let (head, kept) = &mut *self.lists.get();
            let p = head[c];
            if p.is_null() { return self.inner.alloc(class_layout(c)); }
            head[c] = p.cast::<*mut u8>().read();
            kept[c] -= 1;
            p
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        unsafe {
            if class(l).is_none() { return self.inner.alloc_zeroed(l); }
            let p = self.alloc(l);
            if !p.is_null() { p.write_bytes(0, l.size()); }
            p
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe {
            let Some(c) = class(l) else { return self.inner.dealloc(p, l) };
            let (head, kept) = &mut *self.lists.get();
            if kept[c] == KEEP { return self.inner.dealloc(p, class_layout(c)); }
            p.cast::<*mut u8>().write(head[c]);
            head[c] = p;
            kept[c] += 1;
        }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        // SAFETY the caller keeps the rounded size a valid layout and p live with l
        unsafe {
            let grown = Layout::from_size_align_unchecked(size, l.align());
            match (class(l), class(grown)) {
                // the block already has room for any size of its class
                (Some(a), Some(b)) if a == b => p,
                (None, None) => self.inner.realloc(p, l, size),
                _ => { let q = self.alloc(grown); if !q.is_null() { core::ptr::copy_nonoverlapping(p, q, l.size().min(size)); self.dealloc(p, l); } q }
            }
        }
    }
}

/* a pool gives what its classes keep back to the inner allocator */
impl<A: GlobalAlloc> Drop for Pooled<A> {
    fn drop(&mut self) {
        let (head, kept) = self.lists.get_mut();
        for c in 0..head.len() {
            for _ in 0..kept[c] { let p = head[c]; unsafe { head[c] = p.cast::<*mut u8>().read(); self.inner.dealloc(p, class_layout(c)) } }
        }
    }
}

// SAFETY Miri marks only the allocation r starts which its leak check over test_cases reads
#[cfg(miri)]
pub(crate) fn miri_root<T: ?Sized>(r: &T) {
    unsafe extern "Rust" { fn miri_static_root(ptr: *const ()); }
    if core::mem::size_of_val(r) != 0 { unsafe { miri_static_root(core::ptr::from_ref(r).cast()) } }
}

// SAFETY text callers pass whole characters which debug builds check across test_cases

/* `bytes` as the text its caller made of whole characters alone */
#[inline(always)]
pub(crate) fn whole(bytes: &[u8]) -> &str {
    debug_assert!(core::str::from_utf8(bytes).is_ok(), "a text cut inside a character");
    unsafe { core::str::from_utf8_unchecked(bytes) }
}

// SAFETY a cold box drops once in drop or into_inner as test_cases checks under Miri

/* a box whose value drops in a call of its own keeping its holders small */
pub struct Cold<T>(core::mem::ManuallyDrop<alloc::boxed::Box<T>>);

impl<T> Cold<T> {
    #[inline(always)]
    pub fn new(v: T) -> Self { Self(core::mem::ManuallyDrop::new(alloc::boxed::Box::new(v))) }

    /* the value it holds with its box freed and the holder forgotten */
    #[inline(always)]
    pub fn into_inner(self) -> T {
        let mut me = core::mem::ManuallyDrop::new(self);
        *unsafe { core::mem::ManuallyDrop::take(&mut me.0) }
    }
}

impl<T> Drop for Cold<T> {
    #[inline(never)]
    fn drop(&mut self) { unsafe { core::mem::ManuallyDrop::drop(&mut self.0) } }
}

impl<T> core::ops::Deref for Cold<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T { &self.0 }
}

impl<T> core::ops::DerefMut for Cold<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T { &mut self.0 }
}

// SAFETY the payer slot only holds pointers made from a static meter as hosts_suite checks
#[cfg(not(feature = "std"))]
pub(crate) struct Payer(core::sync::atomic::AtomicPtr<Meter>);

#[cfg(not(feature = "std"))]
impl Payer {
    pub(crate) const fn new(m: &'static Meter) -> Self { Self(core::sync::atomic::AtomicPtr::new(core::ptr::from_ref(m).cast_mut())) }
    #[inline(always)] pub(crate) fn get(&self) -> &'static Meter { unsafe { &*self.0.load(core::sync::atomic::Ordering::Relaxed) } }
    #[inline(always)] pub(crate) fn swap(&self, m: &'static Meter) -> &'static Meter { unsafe { &*self.0.swap(core::ptr::from_ref(m).cast_mut(), core::sync::atomic::Ordering::Relaxed) } }
}

// SAFETY wasm statics see one thread and one loan at once as plugin_suite checks
#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
pub(crate) struct Single<T> { value: core::cell::UnsafeCell<T>, #[cfg(debug_assertions)] lent: core::cell::Cell<bool> }

#[cfg(all(target_arch = "wasm32", feature = "runtime", not(target_feature = "atomics")))]
unsafe impl<T> Sync for Single<T> {}

#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
impl<T> Single<T> {
    pub(crate) const fn new(value: T) -> Self { Self { value: core::cell::UnsafeCell::new(value), #[cfg(debug_assertions)] lent: core::cell::Cell::new(false) } }

    /* lends the value to `f` and a debug build panics on a second loan */
    #[inline(always)]
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        #[cfg(debug_assertions)]
        assert!(!self.lent.replace(true), "a static was lent while its last loan was out");
        let r = f(unsafe { &mut *self.value.get() });
        #[cfg(debug_assertions)]
        self.lent.set(false);
        r
    }
}

// SAFETY the published vm outlives the loan and its run waits inside the host call
#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
#[inline(always)]
pub(crate) fn vm_mut<'x>(vm: core::ptr::NonNull<crate::vm::VM<'static>>) -> &'x mut crate::vm::VM<'static> { unsafe { &mut *vm.as_ptr() } }

// SAFETY buffers either side lends stay aligned and valid for one call as plugin_suite checks
#[cfg(all(target_arch = "wasm32", feature = "runtime"))]
pub(crate) mod web {
    use crate::abi::bridge;
    use crate::targets::web::{entry, exports, package, walk, Entry};
    use alloc::{boxed::Box, vec::Vec};

    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        fn host_print(ptr: *const u8, len: usize);
        // the call id brings a deferred result back to the coroutine waiting on it
        fn host_call_native(id: u32, call_id: u32, argv_ptr: *const u32, argc: u32, out: *mut u32) -> i32;
        // host-cached bytes for the spec checked against the sha-256 a non-null hash names
        fn host_fetch_bytes(spec_ptr: *const u8, spec_len: u32, hash_ptr: *const u8, out_len: *mut u32) -> *mut u8;
        // 0 when taken and 2 when refused with an error stashed and else not taken
        fn host_send(group_ptr: *const u8, group_len: u32, body_ptr: *const u8, body_len: u32) -> i32;
    }

    // SAFETY only web_exports defines this symbol so its type is the one declared here
    unsafe extern "Rust" { safe static EDGE_RUNTIME: Entry; }
    #[inline(always)] pub(crate) fn entry() -> &'static Entry { &EDGE_RUNTIME }

    pub(crate) fn print(s: &str) { unsafe { host_print(s.as_ptr(), s.len()) } }
    pub(crate) fn send(group: &str, body: &str) -> i32 { unsafe { host_send(group.as_ptr(), group.len() as u32, body.as_ptr(), body.len() as u32) } }
    #[inline(always)] pub(crate) fn call_native(id: u32, call_id: u32, argv: &[u32], out: &mut u32) -> i32 { unsafe { host_call_native(id, call_id, argv.as_ptr(), argv.len() as u32, out) } }

    /* the bytes the host holds for `spec` copied out of its wasm_alloc buffer */
    #[inline(always)]
    pub(crate) fn fetch(spec: &str, pin: Option<&[u8; 32]>) -> Option<Vec<u8>> {
        let mut len = 0u32;
        let p = unsafe { host_fetch_bytes(spec.as_ptr(), spec.len() as u32, pin.map_or(core::ptr::null(), |h| h.as_ptr()), &mut len) };
        if p.is_null() { return None; }
        let bytes = slice(p, len).to_vec();
        free(p, len);
        Some(bytes)
    }

    #[inline(always)] fn slice<'a, T>(p: *const T, n: u32) -> &'a [T] { if p.is_null() { &[] } else { unsafe { core::slice::from_raw_parts(p, n as usize) } } }
    #[inline(always)] fn text<'a>(p: *const u8, n: u32) -> &'a str { core::str::from_utf8(slice(p, n)).unwrap_or("") }
    #[inline(always)] fn word(p: *mut u32, spare: &mut u32) -> &mut u32 { unsafe { p.as_mut() }.unwrap_or(spare) }
    #[inline(always)] fn room<'a>(p: *mut u8, n: u32) -> &'a mut [u8] { if p.is_null() { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(p, n as usize) } } }
    // SAFETY the host gives back once a buffer wasm_alloc made of exactly `n` bytes
    fn free(p: *mut u8, n: u32) { if !p.is_null() && n != 0 { drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(p, n as usize)) }) } }

    // each export is a C symbol of its name which exports.ts holds to the ABI
    macro_rules! shims { ($($(#[$m:meta])* fn $name:ident($($p:ident: $t:ty),*) $(-> $r:ty)? $body:block)*) => { $($(#[$m])* #[unsafe(no_mangle)] pub extern "C" fn $name($($p: $t),*) $(-> $r)? $body)* }; }

    shims! {
        fn wasm_alloc(size: u32) -> *mut u8 { Box::into_raw(alloc::vec![0u8; size as usize].into_boxed_slice()).cast() }
        fn wasm_free(p: *mut u8, size: u32) { free(p, size) }
        fn set_input(p: *const u8, n: u32) { exports::set_input(slice(p, n)) }
        fn out_ptr() -> *const u8 { exports::out_ptr() }
        fn out_len() -> u32 { exports::out_len() }
        fn register_native_module(sp: *const u8, sn: u32, np: *const u8, nn: u32, base_id: u32) { exports::register_native_module(text(sp, sn), text(np, nn), base_id) }
        fn register_module_error(sp: *const u8, sn: u32, mp: *const u8, mn: u32) { exports::register_module_error(text(sp, sn), text(mp, mn)) }
        fn reset_modules() { exports::reset_modules() }
        fn set_entry(p: *const u8, n: u32) { exports::set_entry(text(p, n)) }
        fn set_limits(memory: u64, ops: u64) { exports::set_limits(memory, ops) }
        fn run_start(p: *const u8, n: u32) -> u32 { exports::run_start(slice(p, n)) }
        fn repl_eval(p: *const u8, n: u32) -> u32 { exports::repl_eval(slice(p, n)) }
        fn restore_state(p: *const u8, n: u32) -> u32 { exports::restore_state(slice(p, n)) }
        fn run_resume() -> u32 { exports::run_resume() }
        fn run_push_event(p: *const u8, n: u32) -> i32 { exports::run_push_event(slice(p, n)) }
        fn set_host_result_by_id(id: u32, handle: u32) -> i32 { exports::set_host_result_by_id(id, handle) }
        fn set_host_error_by_id(id: u32, kind: u32, msg_handle: u32) -> i32 { exports::set_host_error_by_id(id, kind, msg_handle) }
        fn set_preempt_interval(n: u32) { exports::set_preempt_interval(n) }
        fn save_state() -> i64 { exports::save_state() }
        fn state_globals() -> u32 { exports::state_globals() }
        fn state_stack() -> u32 { exports::state_stack() }
        fn vm_create() -> u32 { exports::vm_create() }
        fn vm_select(id: u32) -> i32 { exports::vm_select(id) }
        fn vm_drop(id: u32) -> i32 { exports::vm_drop(id) }
        fn memory_peak() -> u64 { exports::memory_peak() }
        fn send_package() -> u32 { exports::send_package() }
        fn runtime_descriptor() -> u32 { entry::runtime_descriptor() }
        fn walk_start(sp: *const u8, sn: u32, yp: *const u8, yn: u32) -> u32 { walk::walk_start(text(sp, sn), slice(yp, yn)) }
        fn walk_fetched(p: *const u8, n: u32, kind: u32) -> u32 { walk::walk_fetched(slice(p, n), kind) }
        fn walk_plugin(kind: u32, p: *const u8, n: u32) -> u32 { walk::walk_plugin(kind, text(p, n)) }
        fn walk_plugin_bytes() -> u32 { walk::walk_plugin_bytes() }
        fn walk_served(p: *const u8, n: u32) -> u32 { walk::walk_served(slice(p, n)) }
        fn walk_known(p: *const u8, n: u32) -> u32 { walk::walk_known(slice(p, n)) }
        fn walk_carry(p: *const u8, n: u32) { walk::walk_carry(slice(p, n)) }
        fn manifest_check(mp: *const u8, mn: u32, lp: *const u8, ln: u32, yp: *const u8, yn: u32) -> u32 { package::manifest_check(slice(mp, mn), slice(lp, ln), slice(yp, yn)) }
        fn manifest_fields(p: *const u8, n: u32) -> u32 { package::manifest_fields(slice(p, n)) }
        fn bundle_index(p: *const u8, n: u32) -> u32 { package::bundle_index(slice(p, n)) }
        #[cfg(feature = "perms")] fn grants_check(sp: *const u8, sn: u32, mp: *const u8, mn: u32, tp: *const u8, tn: u32) -> u32 { crate::targets::web::perms::grants_check(slice(sp, sn), slice(mp, mn), slice(tp, tn)) }
        #[cfg(feature = "perms")] fn grants_scopes(p: *const u8, n: u32) -> u32 { crate::targets::web::perms::grants_scopes(slice(p, n)) }
        #[cfg(feature = "perms")] fn grants_held(cp: *const u8, cn: u32, pp: *const u8, pn: u32, mp: *const u8, mn: u32) -> u32 { crate::targets::web::perms::grants_held(slice(cp, cn), text(pp, pn), text(mp, mn)) }
        fn host_edge_op(op: u32, recv: u32, np: *const u8, nn: u32, ap: *const u32, an: u32, out: *mut u32) -> i32 { bridge::host_edge_op(op, recv, text(np, nn), slice(ap, an), word(out, &mut 0)) }
        fn host_edge_encode(tag: u32, p: *const u8, n: u32) -> u32 { bridge::host_edge_encode(tag, slice(p, n)) }
        fn host_edge_decode(h: u32, out_tag: *mut u32, dst: *mut u8, dst_max: u32) -> i32 { bridge::host_edge_decode(h, word(out_tag, &mut 0), room(dst, dst_max)) }
        fn host_edge_release(h: u32) { bridge::host_edge_release(h) }
        fn host_edge_throw(kind: u32, p: *const u8, n: u32) { bridge::host_edge_throw(kind, text(p, n)) }
        fn host_edge_take_error(out_kind: *mut u32, dst: *mut u8, dst_max: u32) -> i32 { bridge::host_edge_take_error(word(out_kind, &mut 0), room(dst, dst_max)) }
    }

    /* registers runtime `$rt` which carries its front end and descriptor as the one edge.wasm boots */
    #[macro_export]
    macro_rules! web_exports {
        ($rt:path) => {
            #[unsafe(no_mangle)]
            static EDGE_RUNTIME: $crate::targets::web::Entry = $crate::targets::web::Entry::new(&$rt);
        };
    }
}

// SAFETY nothing frees code chunks or constants a frame or record names as test_cases checks

// SAFETY the caller keeps rc in a slot or table outliving the borrow
#[inline(always)] pub(crate) fn pinned<'x, T>(rc: &alloc::rc::Rc<T>) -> &'x T { unsafe { &*alloc::rc::Rc::as_ptr(rc) } }

/* code a pool or record or the retired list keeps made only from a reference */
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodePtr(*const Code);

impl CodePtr {
    /* a pointer naming no code never read for a job with nothing to rerun */
    pub(crate) const NONE: Self = Self(core::ptr::null());
    #[inline(always)] pub(crate) fn of(rc: &alloc::rc::Rc<Code>) -> Self { Self(alloc::rc::Rc::as_ptr(rc)) }
    /* the code a frame runs which its pool or record already holds */
    #[inline(always)] pub(crate) fn of_code(code: &Code) -> Self { Self(code) }
    /* the code itself which release_retired frees only once nothing names it */
    #[inline(always)] pub(crate) fn get<'x>(self) -> &'x Code { unsafe { &*self.0 } }
    /* the record word that names this code exposing its provenance for `from_word` */
    #[inline(always)] pub(crate) fn word(self) -> Val {
        let at = self.0.expose_provenance();
        debug_assert!(at as u64 >> 48 == 0, "a code address past the payload of a word");
        Val::int(at as i64)
    }
    /* the code a record word names which only `word` writes */
    #[inline(always)] pub(crate) fn from_word(w: Val) -> Self { Self(core::ptr::with_exposed_provenance(w.word())) }
    /* the address alone which compares and hashes without a read */
    #[inline(always)] pub(crate) fn addr(self) -> *const Code { self.0 }
}

/* a chunk the vm or a compiled set keeps made only from a reference */
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkPtr(*const SSAChunk);

impl ChunkPtr {
    /* a pointer naming no chunk and never read like the code one */
    pub(crate) const NONE: Self = Self(core::ptr::null());
    #[inline(always)] pub(crate) fn of(chunk: &SSAChunk) -> Self { Self(chunk) }
    /* the chunk itself which outlives its vm or sits pinned in its compiled set */
    #[inline(always)] pub(crate) fn get<'x>(self) -> &'x SSAChunk { unsafe { &*self.0 } }
}

/* callees a block keeps past the lowering that lent their names and literals */
#[inline(always)]
pub(crate) fn kept_callees(v: alloc::vec::Vec<(u16, crate::vm::lower::Callee<'_>)>) -> alloc::vec::Vec<(u16, crate::vm::lower::Callee<'static>)> {
    unsafe { core::mem::transmute::<alloc::vec::Vec<(u16, crate::vm::lower::Callee<'_>)>, alloc::vec::Vec<(u16, crate::vm::lower::Callee<'static>)>>(v) }
}

/* a pool's constants that rows code reads by index counted in debug builds */
#[derive(Clone, Copy)]
pub(crate) struct Consts { at: *const Val, #[cfg(debug_assertions)] len: usize }

impl Consts {
    pub(crate) const NONE: Self = Self { at: core::ptr::null(), #[cfg(debug_assertions)] len: 0 };
    #[inline(always)] pub(crate) fn of(vals: &[Val]) -> Self { Self { at: vals.as_ptr(), #[cfg(debug_assertions)] len: vals.len() } }
    /* constant `k` which a parser operand of the chunk names below the pool's count */
    #[inline(always)] pub(crate) fn get(self, k: usize) -> Val {
        #[cfg(debug_assertions)]
        debug_assert!(k < self.len, "a constant past its pool");
        unsafe { *self.at.add(k) }
    }
}

// SAFETY sealed code and windows name only registers inside the frame as test_cases checks

/* register `k` of `r` which sealed code names only below its frame */
#[inline(always)]
pub(crate) fn reg(r: &[Val], k: usize) -> Val {
    debug_assert!(k < r.len(), "a register past its frame");
    unsafe { *r.get_unchecked(k) }
}

/* writes register `k` of `r` as `reg` reads it */
#[inline(always)]
pub(crate) fn put(r: &mut [Val], k: usize, v: Val) {
    debug_assert!(k < r.len(), "a register past its frame");
    unsafe { *r.get_unchecked_mut(k) = v }
}

/* a call's record before window `base` its args from there and undef up to `names` */
#[inline(always)]
pub(crate) fn open<const H: usize>(regs: &mut [Val], base: usize, head: Option<&[Val; H]>, args: &[Val], names: usize) {
    debug_assert!(base >= H && base + names.max(args.len()) <= regs.len(), "a window past its registers");
    unsafe {
        let r = regs.as_mut_ptr().add(base);
        if let Some(h) = head { core::ptr::copy_nonoverlapping(h.as_ptr(), r.sub(H), H); }
        core::ptr::copy_nonoverlapping(args.as_ptr(), r, args.len());
        for k in args.len()..names { *r.add(k) = Val::undef(); }
    }
}

/* undef in registers `from` up to `to` of a window laid that long */
#[inline(always)]
pub(crate) fn unbind(regs: &mut [Val], from: usize, to: usize) {
    debug_assert!(from >= to || to <= regs.len(), "a window past its registers");
    for k in from..to { unsafe { *regs.get_unchecked_mut(k) = Val::undef() } }
}

/* the `H` words of the record at `at` lent to be written in place */
#[inline(always)]
pub(crate) fn record_mut<const H: usize>(regs: &mut [Val], at: usize) -> &mut [Val; H] {
    debug_assert!(at + H <= regs.len(), "a record past its registers");
    unsafe { &mut *regs.as_mut_ptr().add(at).cast::<[Val; H]>() }
}

/* the `H` words of the record that starts at `at` */
#[inline(always)]
pub(crate) fn record<const H: usize>(regs: &[Val], at: usize) -> &[Val; H] {
    debug_assert!(at + H <= regs.len(), "a record past its registers");
    unsafe { &*regs.as_ptr().add(at).cast::<[Val; H]>() }
}

// SAFETY the cursor stays on sealed code or the sentinel past it as test_cases checks

/* the word `k` past `p` which every jump and step keeps in code */
#[inline(always)]
pub(crate) fn past(p: *const Ins, k: usize) -> *const Ins { unsafe { p.add(k) } }

/* the index in `prog` of the word `p` lies on */
#[inline(always)]
pub(crate) fn index_in(prog: *const [Ins], p: *const Ins) -> usize { unsafe { p.offset_from(prog.cast::<Ins>()) as usize } }

/* the word at `p` lent so fields read alone and a copy takes it whole */
#[inline(always)]
pub(crate) fn word<'x>(p: *const Ins) -> &'x Ins { unsafe { &*p } }

/* what `f` picks of the word at `p` in a volatile load of its own */
#[inline(always)]
pub(crate) fn read<T: Copy>(p: *const Ins, f: impl FnOnce(&Ins) -> &T) -> T { unsafe { core::ptr::read_volatile(f(&*p)) } }

/* `n` words of `prog` from `k` which a TurnR or UnturnR names */
#[inline(always)]
pub(crate) fn words<'x>(prog: *const [Ins], k: usize, n: usize) -> &'x [Ins] {
    debug_assert!(k + n <= prog.len(), "words past the end of their code");
    unsafe { (&*prog).get_unchecked(k..k + n) }
}

// SAFETY tables never shrink and options are set before code reads them as test_cases checks

/* item `i` of a table that holds every index the vm made for it */
#[inline(always)]
pub(crate) fn get_in<T>(s: &[T], i: usize) -> &T {
    debug_assert!(i < s.len(), "an index past its table");
    unsafe { s.get_unchecked(i) }
}

/* `get_in` lent mutably */
#[inline(always)]
pub(crate) fn get_in_mut<T>(s: &mut [T], i: usize) -> &mut T {
    debug_assert!(i < s.len(), "an index past its table");
    unsafe { s.get_unchecked_mut(i) }
}

/* the value of an option the vm set before any code that reads it runs */
#[inline(always)]
pub(crate) fn known<T>(o: Option<T>) -> T {
    debug_assert!(o.is_some(), "an option read before it was set");
    unsafe { o.unwrap_unchecked() }
}

// SAFETY lifo reads stay below each length and writes inside reserved capacity as test_cases checks

/* room for pushes on `levels` its reserve made counted against the pushes made */
pub(crate) struct Room<'v> { levels: &'v mut alloc::vec::Vec<Val>, top: *mut Val, at: usize, end: usize }

impl<'v> Room<'v> {
    #[inline(always)]
    pub(crate) fn reserve(levels: &'v mut alloc::vec::Vec<Val>, n: usize) -> Self {
        levels.reserve(n);
        let (top, at) = (levels.as_mut_ptr(), levels.len());
        Self { levels, top, at, end: at + n }
    }
    #[inline(always)]
    pub(crate) fn push(&mut self, v: Val) {
        debug_assert!(self.at < self.end, "a level past the room its turn reserved");
        unsafe { self.top.add(self.at).write(v) }
        self.at += 1;
    }
    /* the levels grown over every value pushed */
    #[inline(always)]
    pub(crate) fn close(self) { unsafe { self.levels.set_len(self.at) } }
}

/* a lone level of `k` registers and its link pushed in room its span reserved */
#[inline(always)]
pub(crate) fn turn_lone(levels: &mut alloc::vec::Vec<Val>, len: usize, k: u8, (b, c, link): (Val, Val, Val)) {
    debug_assert!(len == levels.len() && levels.capacity() >= len + 3, "a lone level past the room of its span");
    unsafe {
        let top = levels.as_mut_ptr().add(len);
        let mut at = 0;
        if k != 0 { top.write(b); at = 1; }
        if k == 2 { top.add(1).write(c); at = 2; }
        top.add(at).write(link);
        levels.set_len(len + at + 1);
    }
}

/* the link then `k` registers of a lone level taken back into `set` */
#[inline(always)]
pub(crate) fn untake_lone(levels: &mut alloc::vec::Vec<Val>, len: usize, k: u8, mut set: impl FnMut(u8, Val)) {
    debug_assert!(len == levels.len() && len >= 3, "a lone level taken from fewer than three values");
    unsafe {
        let (top, mut sp) = (levels.as_ptr(), len - 1);
        set(0, *top.add(sp));
        if k == 2 { sp -= 1; set(2, *top.add(sp)); }
        if k != 0 { sp -= 1; set(1, *top.add(sp)); }
        levels.set_len(sp);
    }
}

/* `v` grown to `to` with undef written straight into room its capacity holds */
#[inline(always)]
pub(crate) fn fill_to(v: &mut alloc::vec::Vec<Val>, to: usize) {
    let len = v.len();
    if to < len || to > v.capacity() { return v.resize(to, Val::undef()); }
    unsafe {
        let at = v.as_mut_ptr();
        for k in len..to { at.add(k).write(Val::undef()); }
        v.set_len(to);
    }
}

/* the last item of `from` moved onto `to` and copied once where it lands */
#[inline]
pub(crate) fn move_last<T>(from: &mut alloc::vec::Vec<T>, to: &mut alloc::vec::Vec<T>) {
    let Some(n) = from.len().checked_sub(1) else { return };
    to.reserve(1);
    unsafe {
        core::ptr::copy_nonoverlapping(from.as_ptr().add(n), to.as_mut_ptr().add(to.len()), 1);
        from.set_len(n);
        to.set_len(to.len() + 1);
    }
}

// SAFETY nothing else borrows a cell while these readers hold it as test_cases checks

/* the value of `c` for a reader that copies out before anything writes it */
#[inline(always)]
pub(crate) fn peek<T>(c: &core::cell::RefCell<T>) -> Option<&T> { unsafe { c.try_borrow_unguarded() }.ok() }

/* a builtin iterator's next item stepped with no flag as no user code runs */
#[inline(always)]
pub(crate) fn step_ready(frame: &core::cell::RefCell<crate::value::IterFrame>, heap: &crate::value::HeapPool) -> Option<Val> {
    unsafe { &mut *frame.as_ptr() }.next_ready(heap)
}

/* `f` on the value of `c` which `f` never sets */
#[inline(always)]
pub(crate) fn lent<T, R>(c: &core::cell::Cell<T>, f: impl FnOnce(&T) -> R) -> R { f(unsafe { &*c.as_ptr() }) }
