use super::VM;
use crate::value::Val;
use super::act::HEAD;
use crate::r#unsafe::{get_in, get_in_mut, unbind};

// a pool whose chunk is not running has no window in view
pub(crate) const NO_WINDOW: usize = usize::MAX;

/* a level's window with `code` its constants fit and `owner` the generator parked in it */
#[derive(Clone, Copy)]
pub(crate) struct Win { pub base: usize, pub len: usize, pub code: usize, pub owner: Val }

// a level no run reached
#[inline(always)]
fn no_win() -> Win { Win { base: NO_WINDOW, len: 0, code: 0, owner: Val::undef() } }

impl super::cache::CachePool {
    /* the window of level `level` which a run there laid out */
    #[inline(always)]
    pub(crate) fn level(&self, level: usize) -> Win {
        if level == 0 { return self.w0; }
        // every level a run reached has its window
        *get_in(&self.wins, level - 1)
    }

    /* the window of level `level` as it lies if a run reached it */
    #[inline(always)]
    pub(crate) fn win(&self, level: usize) -> Option<Win> {
        if level == 0 { Some(self.w0) } else { self.wins.get(level - 1).copied() }
    }

    /* the window of level `level` if a run reached it */
    #[inline(always)]
    pub(crate) fn level_mut(&mut self, level: usize) -> Option<&mut Win> {
        if level == 0 { Some(&mut self.w0) } else { self.wins.get_mut(level - 1) }
    }
}

impl VM<'_> {
    /* opens a run of `pool`'s chunk in its level's window of at least `len` registers */
    #[inline(always)]
    pub(crate) fn open_call(&mut self, pool: usize, len: usize, names: usize, args: usize, call: bool) -> usize {
        let p = get_in_mut(&mut self.pools, pool);
        let level = p.active as usize;
        let w = if level == 0 { p.w0 } else { p.wins.get(level - 1).copied().unwrap_or_else(no_win) };
        if !w.owner.is_undef() { self.evict(pool, level); }
        let p = get_in_mut(&mut self.pools, pool);
        (p.window, p.wlen, p.wcode) = (w.base, w.len, w.code);
        p.active += 1;
        if call && p.calls < super::lower::TIER { p.calls += 1; }
        let base = if p.window != NO_WINDOW && p.wlen >= len { p.window }
            else if level > 0 && p.window == NO_WINDOW && p.level(level - 1).len >= len { self.new_level(pool) }
            else { self.place_window(pool, len) };
        if call { unbind(&mut self.regs, base + args, base + names); }
        base
    }

    #[inline(always)]
    pub(crate) fn open_window(&mut self, pool: usize, len: usize, names: usize, call: bool) -> usize { self.open_call(pool, len, names, 0, call) }

    /* ends `pool`'s innermost run and brings the run it interrupted back in view */
    #[inline(always)]
    pub(crate) fn close_window(&mut self, pool: usize) {
        let p = get_in_mut(&mut self.pools, pool);
        p.active -= 1;
        if p.active == 0 { return; }
        let w = p.level(p.active as usize - 1);
        (p.window, p.wlen, p.wcode) = (w.base, w.len, w.code);
        // the running chunk may be this one which is back in the run it interrupted
        if pool == self.pool_now { self.base = w.base; }
    }

    /* moves `pool`'s running level to `len` registers past every other window and gives its base */
    #[cold]
    #[inline(never)]
    pub(crate) fn place_window(&mut self, pool: usize, len: usize) -> usize {
        let len = self.pools[pool].code.as_ref().map_or(len, |c| len.max(c.frame));
        let base = self.regs.len() + HEAD;
        // the registers grow by half while small then by a quarter so little goes unused
        let cap = self.regs.capacity();
        if base + len > cap { self.regs.reserve_exact((base + len).max(if cap <= 256 { cap + cap / 2 } else { cap + cap / 4 }) - self.regs.len()); }
        self.regs.resize(base + len, Val::undef());
        let p = &mut self.pools[pool];
        if p.window != NO_WINDOW {
            let keep = p.wlen.min(len);
            self.regs.copy_within(p.window - HEAD..p.window + keep, base - HEAD);
            // the old place keeps nothing alive
            self.regs[p.window - HEAD..p.window + p.wlen].fill(Val::undef());
        }
        let p = &mut self.pools[pool];
        // a zero code has the next `grow_frame` lay the constants again
        (p.window, p.wlen, p.wcode) = (base, len, 0);
        self.note_window(pool);
        if self.owned_at >= base { self.owned_str = Val::undef(); }
        base
    }

    /* opens `pool`'s first run in a window made from `frame` and at least `names` long */
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

    /* gives `pool`'s newly reached running level a window laid out like the level below's */
    #[cold]
    #[inline(never)]
    fn new_level(&mut self, pool: usize) -> usize {
        let p = &self.pools[pool];
        let w = p.level(p.active as usize - 2);
        let at = self.regs.len();
        // a new level grows the registers as a placed window does
        let cap = self.regs.capacity();
        if at + HEAD + w.len > cap { self.regs.reserve_exact((at + HEAD + w.len).max(if cap <= 256 { cap + cap / 2 } else { cap + cap / 4 }) - at); }
        self.regs.extend_from_within(w.base - HEAD..w.base + w.len);
        let p = &mut self.pools[pool];
        (p.window, p.wlen, p.wcode) = (at + HEAD, w.len, w.code);
        self.note_window(pool);
        at + HEAD
    }

    /* keeps the window in view of `pool` as its running level's */
    #[inline]
    pub(crate) fn note_window(&mut self, pool: usize) {
        let p = &mut self.pools[pool];
        let (level, w) = (p.active as usize - 1, Win { base: p.window, len: p.wlen, code: p.wcode, owner: Val::undef() });
        match p.level_mut(level) { Some(at) => *at = Win { owner: at.owner, ..w }, None => p.wins.push(w) }
    }

    /* makes `pool`'s chunk the one running in its window as it lies now */
    #[inline(always)]
    pub(crate) fn run_in(&mut self, pool: usize) {
        self.pool_now = pool;
        self.base = get_in(&self.pools, pool).window;
    }

    /* copies out the whole window of `pool` for a run that suspends */
    pub(crate) fn window_slots(&self, pool: usize) -> alloc::vec::Vec<Val> { self.window_prefix(pool, usize::MAX) }

    /* copies out the first `live` registers of the window of `pool` */
    pub(crate) fn window_prefix(&self, pool: usize, live: usize) -> alloc::vec::Vec<Val> {
        let p = &self.pools[pool];
        self.regs[p.window..p.window + p.wlen.min(live)].to_vec()
    }

    /* frees level `level` of `pool` for another run by giving its generator its state back */
    #[cold]
    #[inline(never)]
    pub(crate) fn evict(&mut self, pool: usize, level: usize) {
        let Some(w) = self.pools[pool].level_mut(level) else { return };
        let (co, base, len, laid) = (core::mem::replace(&mut w.owner, Val::undef()), w.base, w.len, w.code);
        if co.is_undef() { return; }
        self.owners -= 1;
        let code = self.pools[pool].code.as_deref();
        if let Some(crate::value::HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            // a saved window keeps only the registers its resume reads unless helpers wait inside it
            let live = match code { Some(code) if !c.pinned() && c.syncs.is_empty() => live_len(code, c.ip, c.top, len), _ => len };
            c.slots.clear();
            c.slots.extend_from_slice(&self.regs[base..base + live]);
            if !c.pinned() { c.layout = crate::value::SAVED | laid as u32; }
        }
    }

    /* gives every generator a window holds its state back */
    pub(crate) fn evict_all(&mut self) {
        if self.owners == 0 { return; }
        for pool in 0..self.pools.len() { self.evict_pool(pool); }
    }

    /* gives the generator `co` its state back if a window holds it */
    pub(crate) fn evict_coro(&mut self, co: Val) {
        if self.owners == 0 { return; }
        for pool in 0..self.pools.len() { self.evict_in(pool, co); }
    }

    /* `evict_coro` within the windows of `pool` */
    fn evict_in(&mut self, pool: usize, co: Val) {
        for level in 0..=self.pools[pool].wins.len() {
            if self.pools[pool].level_mut(level).is_some_and(|w| w.owner.0 == co.0) { return self.evict(pool, level); }
        }
    }

    /* gives each generator parked in `pool` its state back to resume wherever its frame opens */
    pub(crate) fn evict_pool(&mut self, pool: usize) {
        if self.owners == 0 { return; }
        for level in 0..=self.pools[pool].wins.len() {
            // a level no generator owns has nothing to give back
            if self.pools[pool].win(level).is_some_and(|w| !w.owner.is_undef()) { self.evict(pool, level); }
        }
    }

    /* `evict_coro` for `co`'s resume that took saved slots as `frame` and takes the window's instead */
    #[cold]
    #[inline(never)]
    pub(crate) fn evict_into(&mut self, co: Val, frame: &mut alloc::vec::Vec<Val>) {
        if self.owners == 0 { return; }
        self.evict_coro(co);
        if frame.is_empty() && let Some(crate::value::HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { core::mem::swap(frame, &mut c.slots); }
    }

    /* what register `at` of generator `g` holds in its own window or saved slots */
    pub(crate) fn gen_slot_val(&self, g: Val, at: usize) -> Option<Val> {
        let Some(crate::value::HeapObj::Coroutine(c)) = self.heap.try_get(g) else { return None };
        let crate::value::BodyRef::Fn(fi) = c.body else { return None };
        let pool = self.fn_pool[fi];
        match (0..=self.pools[pool].wins.len()).find_map(|level| self.pools[pool].win(level).filter(|w| w.owner.0 == g.0)) {
            Some(w) => self.regs.get(w.base + at).copied(),
            None => c.slots.get(at).copied(),
        }
    }

    /* register `at` of generator `g` in the window it owns or its saved slots */
    pub(crate) fn gen_slot(&mut self, g: Val, at: usize) -> Option<&mut Val> {
        let Some(crate::value::HeapObj::Coroutine(c)) = self.heap.try_get(g) else { return None };
        let crate::value::BodyRef::Fn(fi) = c.body else { return None };
        let pool = self.fn_pool[fi];
        let owned = (0..=self.pools[pool].wins.len()).find_map(|level| self.pools[pool].win(level).filter(|w| w.owner.0 == g.0));
        match owned {
            Some(w) => self.regs.get_mut(w.base + at),
            None => match self.heap.try_get_mut(g) { Some(crate::value::HeapObj::Coroutine(c)) => c.slots.get_mut(at), _ => None },
        }
    }

    /* makes `co` the generator whose state level `level` of `pool`'s windows holds */
    #[inline]
    pub(crate) fn own(&mut self, pool: usize, level: usize, co: Val) {
        if let Some(w) = self.pools[pool].level_mut(level) { w.owner = co; self.owners += 1; }
    }

    /* whether level `level` of `pool` holds `co`'s state and releases it as `co` resumes there */
    #[inline]
    pub(crate) fn resumes_in(&mut self, pool: usize, level: usize, co: Val) -> bool {
        let Some(w) = self.pools[pool].level_mut(level).filter(|w| w.owner.0 == co.0 && !co.is_undef()) else { return false };
        w.owner = Val::undef();
        self.owners -= 1;
        true
    }

    /* clears idle windows so they keep nothing alive and their next run lays constants again */
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

/* the registers a window `len` long parked at `ip` of `code` with `top` resumes from */
pub(crate) fn live_len(code: &super::lower::Code, ip: usize, top: u16, len: usize) -> usize {
    // a raw park or one at the start or in sectioned code keeps every register
    if ip & crate::vm::frame::RESUME_RAW != 0 || ip == 0 || code.block.is_some() { return len; }
    let live = match (code.rows, code.slots) {
        // code no rows hold keeps its names
        (0, 0) => code.kinds.len(),
        // code over rows keeps them to its top
        (rows, _) if rows != 0 => if top != 0 { top as usize } else { len },
        // lowered code keeps its names and resume rows and the row a wait took
        (_, s0) => (s0 as usize + code.entry_rows(ip)).max(top as usize),
    };
    live.min(len)
}
