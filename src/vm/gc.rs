use alloc::{rc::Rc, vec::Vec};

use super::VM;
use super::lower;
use crate::value::*;

impl<'a> VM<'a> {

    /* collects and raises a memory error when still past the memory limit or instance budget */
    #[cold]
    #[inline(never)]
    pub(crate) fn collect_point(&mut self) -> Result<(), VmErr> {
        self.collect();
        self.heap.meter().tell();
        if self.heap.over() { Err(cold_heap()) } else { Ok(()) }
    }

    /* marks every reachable root then sweeps and marking a non-heap value does nothing */
    pub(crate) fn collect(&mut self) {
        for &v in &self.levels { self.heap.mark(v); }
        for &v in &self.temp_roots { self.heap.mark(v); }
        for &v in self.event_queue.slots() { self.heap.mark(v); }
        // the handled exception and any pending finally return value outlive their stack slots
        if let Some(v) = self.pending.exc_val { self.heap.mark(v); }
        for t in &self.conts { let (crate::vm::coro::Cont::After(g, _) | crate::vm::coro::Cont::Exit(g, ..) | crate::vm::coro::Cont::Close(g, _)) = t.cont; self.heap.mark(g); self.heap.mark(t.before); if let crate::vm::coro::Cont::Exit(_, _, e) = t.cont { self.heap.mark(e); } }
        if let Some((_, Ok(crate::vm::coro::Step::Yield(v) | crate::vm::coro::Step::Return(v)))) = self.then_out { self.heap.mark(v); }
        if let Some(super::hints::Missed::Attr(v, _)) = &self.missed { self.heap.mark(*v); }
        self.redo.mark(&mut self.heap);
        self.heap.mark(self.yield_from_value);
        self.heap.mark(self.retake);
        for &(it, v) in &self.pend { self.heap.mark(it); self.heap.mark(v); }
        for (g, items) in &self.held { self.heap.mark(*g); for &v in items { self.heap.mark(v); } }
        for &(v, _) in &self.handling { self.heap.mark(v); }
        for u in &self.unwind_stack { if let Unwind::Return(v) | Unwind::Reraise(_, _, v) = u { self.heap.mark(*v); } }
        // the module body suspended to the host keeps its saved state across resumes
        if let Some(root) = &self.root {
            self.heap.mark(root.coro);
            if let CoroState::Raising(_, Some(exc)) = &root.state { self.heap.mark(*exc); }
        }
        // unheld windows keep nothing alive as the chunk's next run lays their constants again
        self.evict_all();
        self.clear_idle_windows();
        for p in &self.pools {
            for w in core::iter::once(&p.w0).chain(p.wins.iter()).take(p.active as usize).filter(|w| w.base != super::window::NO_WINDOW) {
                for &v in &self.regs[w.base - super::act::HEAD..w.base + w.len] { self.heap.mark(v); }
            }
        }
        for &v in self.instances.iter().chain(&self.learned).chain(&self.grown) { self.heap.mark(v); }
        #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
        crate::abi::bridge::mark_handles(self as *const Self as *const u8, &mut self.heap);
        for &v in self.builtins.values() { self.heap.mark(v); }
        for scope in &self.scopes { for (_, v) in scope.iter() { self.heap.mark(v); } }
        // a class body's methods may still close over the cells around it
        for cells in &self.class_cells { for &(_, c) in cells { self.heap.mark(c); } }
        // `from x import` leaves the module unnamed yet its functions read globals through this table
        for &v in self.module_table.values() { self.heap.mark(v); }
        // shapes name their keys by value which a sweep must never hand to another string
        for edges in &self.shapes { for &(k, _) in edges { self.heap.mark(k); } }
        let heap = &mut self.heap; // split borrow so closures take &mut heap while iterating other fields
        for frame in &self.iter_stack { frame.for_each_val(&mut |v| heap.mark(v)); }
        for job in &self.jobs { job.for_each_val(&mut |v| heap.mark(v)); }
        for f in &self.floors { f.for_each_val(&mut |v| heap.mark(v)); }
        for sf in &self.pending_sync_frames { sf.for_each_val(&mut |v| heap.mark(v)); }
        for pool in &self.pools { for &v in pool.consts.iter().flatten() { self.heap.mark(v); } }
        self.release_retired();
        for code in self.pools.iter().filter_map(|p| p.code.as_deref()).chain(self.retired.iter().map(|c| &**c)) { mark_code(&mut self.heap, code); }
        self.heap.sweep();
        // an unlowered frame holds its sites unmarked so sites keyed on the epoch learn again
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
    }

    /* drops replaced codes held by no frame or job or parked run or other reference */
    fn release_retired(&mut self) {
        if self.retired.is_empty() { return; }
        // frames name their code in the window record and suspended section runs by layout
        let mut used: Vec<usize> = Vec::new();
        for p in &self.pools {
            let levels = core::iter::once(&p.w0).chain(p.wins.iter()).take(p.active as usize).map(|w| w.base);
            for base in levels.chain((p.active != 0).then_some(p.window)).filter(|&b| b != super::window::NO_WINDOW && b >= super::act::HEAD) {
                used.push(self.regs[base - super::act::HEAD + 4].word());
            }
        }
        used.extend(self.jobs.iter().filter_map(|j| j.again_code()).map(|c| c.addr() as usize));
        used.sort_unstable();
        let held = |c: &Rc<lower::Code>| Rc::strong_count(c) > 1 || used.binary_search(&(Rc::as_ptr(c) as usize)).is_ok();
        if self.retired.iter().all(held) { return; }
        let mut layouts: Vec<u32> = alloc::vec![self.resume_layout];
        layouts.extend(self.pending_sync_frames.iter().map(|s| s.layout));
        for obj in self.heap.snapshot_objs().flatten() {
            if let HeapObj::Coroutine(c) = obj { layouts.push(c.layout); layouts.extend(c.syncs.iter().map(|s| s.layout)); }
        }
        self.retired.retain(|c| held(c) || layouts.contains(&c.layout));
    }
}

/* marks every value a code checks by identity in its sites and its block */
fn mark_code(heap: &mut crate::value::HeapPool, code: &lower::Code) {
    for v in code.cache.site_roots() { heap.mark(v); }
    let Some(b) = code.block.as_deref() else { return };
    for &v in b.extra.iter() { heap.mark(v); }
    // a defaults section's key is no value and its function is marked with its section
    if let Some(g) = b.grown.get() { for &(_, v) in g.pending.borrow().iter() { heap.mark(v); } }
    for &(v, k) in b.alt_tab.iter() { if k == u32::MAX || k & lower::DEFAULTS == 0 { heap.mark(v); } }
    for c in b.by_class.iter() { heap.mark(c.get().0); }
    for (_, c) in b.callees.iter() {
        for v in [c.value, c.ctor().map_or(Val::undef(), |t| t.2)] { heap.mark(v); }
        for &v in c.cells.iter().map(|c| &c.1).chain(c.defaults.iter()).chain(c.consts.iter()) { heap.mark(v); }
        if let Some(site) = c.site() { for v in site.roots() { heap.mark(v); } }
    }
    for s in b.sections.iter() {
        heap.mark(s.value.get());
        if let Some((_, _, init)) = &s.ctor { heap.mark(*init); }
        for &v in s.defaults.iter() { heap.mark(v); }
        heap.mark(s.guard.get().0);
    }
}
