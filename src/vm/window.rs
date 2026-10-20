use super::VM;
use super::types::Val;
use super::act::HEAD;

// A pool whose chunk is not running has no window in view.
pub(crate) const NO_WINDOW: usize = usize::MAX;

/* One level's window, its start, length, the code its constants are laid for (0 for none) and the generator whose state it holds past a yield (undef for none). */
#[derive(Clone, Copy)]
pub(crate) struct Win { pub base: usize, pub len: usize, pub code: usize, pub owner: Val }

// A level no run reached.
#[inline(always)]
fn no_win() -> Win { Win { base: NO_WINDOW, len: 0, code: 0, owner: Val::undef() } }

impl super::cache::CachePool {
    /* The window of level `level`, which a run there laid out. */
    #[inline(always)]
    pub(crate) fn level(&self, level: usize) -> Win {
        if level == 0 { return self.w0; }
        debug_assert!(level <= self.wins.len());
        // SAFETY every level a run reached has its window.
        unsafe { *self.wins.get_unchecked(level - 1) }
    }

    /* The window of level `level`, if a run reached it, as it lies. */
    #[inline(always)]
    pub(crate) fn win(&self, level: usize) -> Option<Win> {
        if level == 0 { Some(self.w0) } else { self.wins.get(level - 1).copied() }
    }

    /* The window of level `level`, if a run reached it. */
    #[inline(always)]
    pub(crate) fn level_mut(&mut self, level: usize) -> Option<&mut Win> {
        if level == 0 { Some(&mut self.w0) } else { self.wins.get_mut(level - 1) }
    }
}

impl VM<'_> {
    /* Opens a run of `pool`'s chunk in its level's window of at least `len` registers, its first `names` the names, unbound for a call but its first `args`, and gives the base. */
    #[inline(always)]
    pub(crate) fn open_call(&mut self, pool: usize, len: usize, names: usize, args: usize, call: bool) -> usize {
        debug_assert!(pool < self.pools.len());
        // SAFETY a pool a call opens is one of `pools`.
        let p = unsafe { self.pools.get_unchecked_mut(pool) };
        let level = p.active as usize;
        let w = if level == 0 { p.w0 } else { p.wins.get(level - 1).copied().unwrap_or_else(no_win) };
        if !w.owner.is_undef() { self.evict(pool, level); }
        let p = unsafe { self.pools.get_unchecked_mut(pool) };
        (p.window, p.wlen, p.wcode) = (w.base, w.len, w.code);
        p.active += 1;
        if call && p.calls < super::lower::TIER { p.calls += 1; }
        let base = if p.window != NO_WINDOW && p.wlen >= len { p.window }
            else if level > 0 && p.window == NO_WINDOW && p.level(level - 1).len >= len { self.new_level(pool) }
            else { self.place_window(pool, len) };
        debug_assert!(base + names <= self.regs.len());
        // SAFETY the window holds its names.
        if call { for k in args..names { unsafe { *self.regs.get_unchecked_mut(base + k) = Val::undef(); } } }
        base
    }

    #[inline(always)]
    pub(crate) fn open_window(&mut self, pool: usize, len: usize, names: usize, call: bool) -> usize { self.open_call(pool, len, names, 0, call) }

    /* Ends the innermost run of `pool`'s chunk, the run it interrupted, if any, in view again and running if its chunk runs. */
    #[inline(always)]
    pub(crate) fn close_window(&mut self, pool: usize) {
        debug_assert!(pool < self.pools.len());
        // SAFETY a pool a run ends in is one of `pools`.
        let p = unsafe { self.pools.get_unchecked_mut(pool) };
        p.active -= 1;
        if p.active == 0 { return; }
        let w = p.level(p.active as usize - 1);
        (p.window, p.wlen, p.wcode) = (w.base, w.len, w.code);
        // The chunk running may be the one whose run ended, back in the run it interrupted.
        if pool == self.pool_now { self.base = w.base; }
    }

    /* Gives the running level of `pool` a window of `len` registers past every other, its values moved with it, and the new base, its constants laid again by the next `grow_frame`. */
    #[cold]
    #[inline(never)]
    pub(crate) fn place_window(&mut self, pool: usize, len: usize) -> usize {
        let len = self.pools[pool].code.as_ref().map_or(len, |c| len.max(c.frame));
        let base = self.regs.len() + HEAD;
        // Past a fresh VM's registers they grow by half once, so a small program holds little more than it uses, then by powers of two, so a deep one copies rarely.
        let cap = self.regs.capacity();
        if base + len > cap { self.regs.reserve_exact((base + len).max(if cap <= 256 { cap + cap / 2 } else { 2 * cap.next_power_of_two() }) - self.regs.len()); }
        self.regs.resize(base + len, Val::undef());
        let p = &mut self.pools[pool];
        if p.window != NO_WINDOW {
            let keep = p.wlen.min(len);
            self.regs.copy_within(p.window - HEAD..p.window + keep, base - HEAD);
            // The old place keeps nothing alive.
            self.regs[p.window - HEAD..p.window + p.wlen].fill(Val::undef());
        }
        let p = &mut self.pools[pool];
        (p.window, p.wlen, p.wcode) = (base, len, 0);
        self.note_window(pool);
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        base
    }

    /* Opens the first run of `pool`'s chunk, its window made from `frame`, at least `names` long. */
    #[cold]
    #[inline(never)]
    pub(crate) fn first_window(&mut self, pool: usize, frame: &[Val], names: usize) -> usize {
        let base = self.regs.len() + HEAD;
        let len = frame.len().max(names);
        self.regs.reserve(HEAD + len);
        self.regs.extend_from_slice(&[Val::undef(); HEAD]);
        self.regs.extend_from_slice(frame);
        self.regs.resize(base + len, Val::undef());
        let p = &mut self.pools[pool];
        (p.window, p.wlen, p.wcode, p.active) = (base, len, 0, 1);
        p.w0 = Win { base, len, code: 0, owner: Val::undef() };
        base
    }

    /* Gives the running level of `pool`, reached for the first time, a window laid out as the level below's is. */
    #[cold]
    #[inline(never)]
    fn new_level(&mut self, pool: usize) -> usize {
        let p = &self.pools[pool];
        let w = p.level(p.active as usize - 2);
        let at = self.regs.len();
        self.regs.extend_from_within(w.base - HEAD..w.base + w.len);
        let p = &mut self.pools[pool];
        (p.window, p.wlen, p.wcode) = (at + HEAD, w.len, w.code);
        self.note_window(pool);
        at + HEAD
    }

    /* Keeps the window in view of `pool` as its running level's. */
    #[inline]
    pub(crate) fn note_window(&mut self, pool: usize) {
        let p = &mut self.pools[pool];
        let (level, w) = (p.active as usize - 1, Win { base: p.window, len: p.wlen, code: p.wcode, owner: Val::undef() });
        match p.level_mut(level) { Some(at) => *at = Win { owner: at.owner, ..w }, None => p.wins.push(w) }
    }

    /* Makes `pool`'s chunk the one running, in its window as it lies now. */
    #[inline(always)]
    pub(crate) fn run_in(&mut self, pool: usize) {
        debug_assert!(pool < self.pools.len());
        self.pool_now = pool;
        // SAFETY a pool code runs in is one of `pools`.
        self.base = unsafe { self.pools.get_unchecked(pool) }.window;
    }

    /* Copies the window of `pool` out, as long as it is, for a run that suspends. */
    pub(crate) fn window_slots(&self, pool: usize) -> alloc::vec::Vec<Val> {
        let p = &self.pools[pool];
        self.regs[p.window..p.window + p.wlen].to_vec()
    }

    /* Gives the generator whose state level `level` of `pool`'s windows holds its state back, so another run may take it. */
    #[cold]
    #[inline(never)]
    pub(crate) fn evict(&mut self, pool: usize, level: usize) {
        let Some(w) = self.pools[pool].level_mut(level) else { return };
        let (co, base, len) = (core::mem::replace(&mut w.owner, Val::undef()), w.base, w.len);
        if co.is_undef() { return; }
        self.owners -= 1;
        if let Some(super::types::HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            c.slots.clear();
            c.slots.extend_from_slice(&self.regs[base..base + len]);
        }
    }

    /* Gives every generator a window holds its state back. */
    pub(crate) fn evict_all(&mut self) {
        if self.owners == 0 { return; }
        for pool in 0..self.pools.len() { for level in 0..=self.pools[pool].wins.len() { self.evict(pool, level); } }
    }

    /* Gives the generator `co` its state back if a window holds it. */
    pub(crate) fn evict_coro(&mut self, co: Val) {
        if self.owners == 0 { return; }
        for pool in 0..self.pools.len() { self.evict_in(pool, co); }
    }

    /* `evict_coro` within the windows of `pool`. */
    fn evict_in(&mut self, pool: usize, co: Val) {
        for level in 0..=self.pools[pool].wins.len() {
            if self.pools[pool].level_mut(level).is_some_and(|w| w.owner.0 == co.0) { return self.evict(pool, level); }
        }
    }

    /* Gives every generator a window of `pool` holds its state back, so each resumes from its own wherever its frame opens. */
    pub(crate) fn evict_pool(&mut self, pool: usize) {
        if self.owners == 0 { return; }
        for level in 0..=self.pools[pool].wins.len() { self.evict(pool, level); }
    }

    /* `evict_coro` for a resume of `co` that took its saved slots as `frame` already, which takes those of the window instead. */
    #[cold]
    #[inline(never)]
    pub(crate) fn evict_into(&mut self, co: Val, frame: &mut alloc::vec::Vec<Val>) {
        if self.owners == 0 { return; }
        self.evict_coro(co);
        if frame.is_empty() && let Some(super::types::HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { core::mem::swap(frame, &mut c.slots); }
    }

    /* Makes `co` the generator level `level` of `pool`'s windows holds the state of. */
    #[inline]
    pub(crate) fn own(&mut self, pool: usize, level: usize, co: Val) {
        if let Some(w) = self.pools[pool].level_mut(level) { w.owner = co; self.owners += 1; }
    }

    /* Whether level `level` of `pool`'s windows holds the state of `co`, taking it back from it as it resumes there. */
    #[inline]
    pub(crate) fn resumes_in(&mut self, pool: usize, level: usize, co: Val) -> bool {
        let Some(w) = self.pools[pool].level_mut(level).filter(|w| w.owner.0 == co.0 && !co.is_undef()) else { return false };
        w.owner = Val::undef();
        self.owners -= 1;
        true
    }

    /* Clears every window no run holds, so it keeps nothing alive, its constants laid again when it next runs. */
    pub(crate) fn clear_idle_windows(&mut self) {
        for p in self.pools.iter_mut() {
            let idle = core::iter::once(&mut p.w0).chain(p.wins.iter_mut()).skip(p.active as usize).filter(|w| w.base != NO_WINDOW);
            for w in idle {
                self.regs[w.base - HEAD..w.base + w.len].fill(Val::undef());
                w.code = 0;
            }
        }
    }
}
