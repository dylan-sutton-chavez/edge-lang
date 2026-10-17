use super::VM;
use super::types::*;

impl<'a> VM<'a> {

    /* Collects, and past the memory limit or the instance budget after it, the program raises MemoryError. */
    #[cold]
    #[inline(never)]
    pub(crate) fn collect_point(&mut self) -> Result<(), VmErr> {
        self.collect();
        self.heap.meter().tell();
        if self.heap.over() { Err(cold_heap()) } else { Ok(()) }
    }

    /* Mark all reachable roots then sweep, non-heap Vals are no-op to mark. */
    pub(crate) fn collect(&mut self) {
        for &v in &self.stack { self.heap.mark(v); }
        for &v in &self.with_stack { self.heap.mark(v); }
        for &v in &self.temp_roots { self.heap.mark(v); }
        for &v in &self.yields { self.heap.mark(v); }
        for &v in &self.event_queue { self.heap.mark(v); }
        // The handled exception and any pending finally return value outlive their stack slots.
        if let Some(v) = self.pending.exc_val { self.heap.mark(v); }
        if let Some(super::hints::Missed::Attr(v, _)) = &self.missed { self.heap.mark(*v); }
        self.heap.mark(self.yield_from_value);
        for &(v, _) in &self.handling { self.heap.mark(v); }
        for u in &self.unwind_stack { if let Unwind::Return(v) = u { self.heap.mark(*v); } }
        // The module body suspended to the host keeps its saved state across resumes.
        if let Some(root) = &self.root {
            self.heap.mark(root.coro);
            if let CoroState::Raising(_, Some(exc)) = &root.state { self.heap.mark(*exc); }
        }
        // Every running frame lives in the register stack, the innermost on top.
        for &v in &self.regs { self.heap.mark(v); }
        #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
        crate::bridge::mark_handles(self as *const Self as *const u8, &mut self.heap);
        for &v in &self.template_roots { self.heap.mark(v); }
        for &v in self.builtins.values() { self.heap.mark(v); }
        for scope in &self.scopes { for (_, v) in scope.iter() { self.heap.mark(v); } }
        for &(_, d) in &self.globals_dicts { self.heap.mark(d); }
        for l in &self.linked { self.heap.mark(l.dict); }
        self.heap.mark(self.builtins_module);
        for f in &self.frames { self.heap.mark(f.callee); self.heap.mark(f.instance); }
        // A class body's methods may still close over the cells around it.
        for cells in &self.class_cells { for &(_, c) in cells { self.heap.mark(c); } }
        // A `from x import` binds no name to the module, yet its functions still read their globals through this table.
        for &v in self.module_table.values() { self.heap.mark(v); }
        // A shape names its keys by value, which a sweep must never hand to another string.
        for edges in &self.shapes { for &(k, _) in edges { self.heap.mark(k); } }
        let heap = &mut self.heap; // split borrow, lets closures take &mut heap while iterating other fields
        for frame in &self.iter_stack { frame.for_each_val(&mut |v| heap.mark(v)); }
        for sf in &self.pending_sync_frames { sf.for_each_val(&mut |v| heap.mark(v)); }
        for pool in &self.pools { for &v in pool.consts.iter().flatten() { self.heap.mark(v); } }
        // Lowered code keeps its sites in its pool, an unlowered frame keeps its own until it ends.
        for code in self.pools.iter().filter_map(|p| p.code.as_deref()) {
            for v in code.cache.site_roots() { self.heap.mark(v); }
        }
        self.heap.sweep();
        self.prune_key_reps();
        // An unlowered frame holds its sites unmarked, so every site keyed on the epoch learns again.
        self.class_epoch = self.class_epoch.wrapping_add(1);
    }
}
