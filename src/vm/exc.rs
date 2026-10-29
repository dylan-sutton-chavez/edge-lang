use alloc::string::String;
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use super::{VM, lower};
use crate::value::*;
use super::lower::Code;
use super::sections::section_at;

/* whether `e` ends an iteration and so the loop stepping what raised it */
pub(crate) fn is_stop(e: &VmErr, names: &crate::lang::Names) -> bool { e.is(crate::lang::ErrKind::Stop, names) }

/* whether `e` is the signal a `close` throws in */
pub(crate) fn is_exit(e: &VmErr, names: &crate::lang::Names) -> bool { e.is(crate::lang::ErrKind::Close, names) }

/* a section's handler names its own code index under this flag and others the compiler's */
pub(crate) const RAW_HANDLER: usize = 1 << 30;

#[inline]
pub(crate) fn handler_at(code: &Code, h: usize) -> usize { if h & RAW_HANDLER != 0 { h & !RAW_HANDLER } else { code.lowered(h) } }

/* another generator loop section headed at `site` with a tail which `s` returned past */
fn tail_of<'c>(code: &'c Code, s: &lower::Section, site: usize) -> Option<&'c lower::Section> {
    if !lower::section_call(code.ins.get(site)?.op) { return None; }
    let g = code.block.as_deref()?.sections.get(code.ins.get(site + 2)?.c as usize)?;
    (g.tail.is_some() && !core::ptr::eq(g, s)).then_some(g)
}

/* the top an Except handler at `to` of `code` takes its exception on */
fn handler_top(code: &Code, to: usize) -> Option<u32> {
    if code.rows != 0 { return Some(u32::from(code.rows)); }
    code.ins.get(to).filter(|t| t.op == OpCode::StoreTopR && t.x == 0).map(|t| u32::from(t.a))
}

impl<'a> VM<'a> {

    /* jumps to this frame's handler for `e` or hands `e` back without one */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn catch(&mut self, code: &Code, chunk: &SSAChunk, ip: &mut usize, exc_base: usize, delta_base: usize, e: VmErr, rip: usize) -> Result<(), VmErr> {
        // a host yield is a control signal no handler of the program catches
        if matches!(e, VmErr::HostYield(_)) { return Err(e); }
        // a missing name is measured against its frame here while that frame still lives
        if let Some(n) = e.missing_name() { self.note_missing_name(n, chunk); }
        // the facts of an engine error take the runtime's words before any handler reads them
        let mut e = self.word(e);
        // the innermost frame wins and is cleared below on swallow so later errors re-anchor
        if self.error_byte_pos.is_none() {
            self.error_byte_pos = chunk.resolve(code.orig(rip));
            // an imported module keeps where its error rose for the line its traceback starts at
            if self.error_inner.is_none() && !alloc::sync::Arc::ptr_eq(&chunk.source, &self.chunk.source) && let Some(p) = self.error_byte_pos {
                self.error_inner = Some((chunk.source.clone(), chunk.path.clone(), p));
            }
        }
        if self.exception_stack.len() <= exc_base { return Err(e); }
        let frame = self.exception_stack.pop().unwrap();
        // a recursed section's handler takes back the registers its level kept for the raising call
        if frame.handler_ip & RAW_HANDLER != 0 && code.block.is_some() && self.turned > frame.turned { self.unwind_levels(code, rip, frame.levels); }
        self.levels.truncate(frame.levels);
        self.turned = frame.turned;
        self.iter_stack.truncate(frame.iter_depth);
        self.pending.delta_save.truncate(delta_base);
        // drop the partial traceback so a later error doesn't inherit stale frames
        self.call_stack.clear();
        // prefer the user-raised instance and synthesize one for native errors
        let exc = if let Some(v) = self.pending.exc_val.take() {
            v
        } else if let Some(kind) = e.kind().filter(|_| !matches!(e, VmErr::Heap)) {
            // the runtime makes what a handler binds of the kind and the words
            let text = match (&mut e, frame.kind) {
                (VmErr::Of(_, m), BlockKind::Except) => core::mem::take(m),
                (e, _) => e.message(self.natives.names()),
            };
            (self.rt.raise)(&mut crate::lang::Cx::new(self), crate::lang::EngineError::Of(kind, text))?
        } else {
            let msg = e.class_name(self.natives.names());
            // a memory error reports the limit itself so its objects skip the soft limit
            let heap_err = matches!(e, VmErr::Heap);
            let alloc = |heap: &mut HeapPool, obj| if heap_err { heap.alloc_emergency(obj) } else { heap.alloc(obj) };
            // an error with no message such as a bare end of iteration has no args
            let text = match (&mut e, frame.kind) {
                // a handler takes the words and only a cleanup keeps them for its reraise
                (VmErr::Of(_, m), BlockKind::Except) => core::mem::take(m),
                (e, _) => e.message(self.natives.names()),
            };
            let args = if text.is_empty() { alloc::vec![] } else { alloc::vec![alloc(&mut self.heap, HeapObj::Str(text))?] };
            if heap_err { self.heap.new_error_emergency(&msg, args)? } else { self.heap.new_error(&msg, args)? }
        };
        // kept so raising the exception again reports where it was first raised
        let at = self.error_byte_pos.take();
        (self.error_inner, self.missed) = (None, None);
        // drop reasons from finally bodies this exception unwinds past
        self.unwind_stack.truncate(frame.unwind_depth);
        self.handling.truncate(frame.handling_depth);
        // the running handler or cleanup handles the exception so a bare `raise` re-raises it
        self.handling.push((exc, at));
        let mut to = handler_at(code, frame.handler_ip);
        match frame.kind {
            // a lowered handler takes it in its SetupExcept's slot past the store resumes use
            BlockKind::Except if frame.slot != 0 && code.ins.get(to).is_some_and(|s| s.op == OpCode::StoreTopR && s.x == 0 && s.a + 1 == frame.slot) => {
                self.regs[self.base + frame.slot as usize - 1] = exc;
                // a row op that raised leaves its row behind and the handler holds no row
                (to, self.top) = (to + 1, 0);
            }
            // rows take it in their first row and lowered code in the handler's take
            BlockKind::Except => if let Some(top) = handler_top(code, to) { self.top = top; self.push(exc); },
            // finally and with blocks run their cleanup then re-raise via `EndFinally`
            BlockKind::Finally => {
                self.top = u32::from(code.rows);
                self.pending.exc_val = Some(exc);
                // the object goes with the reason so a handler inside the cleanup leaves it whole
                self.unwind_stack.push(Unwind::Reraise(e, at, exc));
            }
        }
        *ip = to;
        Ok(())
    }

    /* a section error takes its function's line and a frame where each enclosing section called */
    #[cold]
    #[inline(never)]
    pub(crate) fn section_error(&mut self, code: &Code, chunk: &SSAChunk, rip: usize) {
        let Some(block) = code.block.as_deref() else { return };
        let Some((s, o)) = block.holding(rip) else { return };
        if self.error_byte_pos.is_none() && let Some(&o) = s.orig.get(o - s.start as usize) { self.error_byte_pos = self.functions[s.fi].1.resolve(o); }
        // links name calls three back and a recursion counts once more on its noted frame
        let mut last: Option<(usize, usize, usize, u32)> = None;
        self.walk_levels(code, rip, false, 0, |vm, s, site, outer, _| {
            match outer {
                Some(outer) => {
                    let now = (vm.call_stack.len(), vm.call_stack.last().map_or(0, |f| f.repeats));
                    if last == Some((site, s.fi, now.0, now.1)) && let Some(f) = vm.call_stack.last_mut() { f.repeats += 1; } else {
                        // a head laid past the end for an inner op sits where that op does
                        let at = outer.orig.get(site - outer.start as usize).or_else(|| outer.orig.get(block.origin(site) - outer.start as usize)).copied();
                        vm.note_frame(s.fi, at, &vm.functions[outer.fi].1);
                    }
                    let after = (vm.call_stack.len(), vm.call_stack.last().map_or(0, |f| f.repeats));
                    last = (after != now).then_some((site, s.fi, after.0, after.1));
                }
                // the call outside every section or past the generator whose `yield` takes its result
                None => match tail_of(code, s, site) {
                    Some(g) => {
                        vm.note_frame(s.fi, g.tail.map(|t| t.0), &vm.functions[g.fi].1);
                        vm.note_frame(g.fi, Some(code.orig(site)), chunk);
                    }
                    None => vm.note_frame(s.fi, Some(code.orig(site)), chunk),
                },
            }
            true
        });
    }

    /* a `finally` a section return runs ends in the next one or the section's return */
    #[inline(never)]
    pub(crate) fn end_section_finally(&mut self, code: &Code, rip: usize, ip: &mut usize) {
        let (Some(s), Some(Unwind::Return(v))) = (section_at(code, rip), self.unwind_stack.pop()) else { return };
        match self.section_cleanup(s) {
            Some(h) => { self.unwind_stack.push(Unwind::Return(v)); *ip = h & !RAW_HANDLER; }
            None => {
                let link = self.regs[self.base + s.link as usize].as_int() as usize;
                self.regs[self.base + (link & 0xFFFF)] = v;
                *ip = link >> 16;
            }
        }
    }

    /* restores registers each section recursion level kept down to the handler's `depth` as `section_error` walks */
    #[cold]
    #[inline(never)]
    fn unwind_levels(&mut self, code: &Code, rip: usize, depth: usize) { self.walk_levels(code, rip, true, depth, |_, _, _, _, _| true); }

    /* `next_cleanup_handler` for returning out of `s` over frames its `try`s pushed at this recursion level */
    pub(crate) fn section_cleanup(&mut self, s: &lower::Section) -> Option<usize> {
        let level = self.levels.len();
        let own = |f: &ExceptionFrame| f.handler_ip & RAW_HANDLER != 0 && (s.start as usize..s.end as usize).contains(&(f.handler_ip & !RAW_HANDLER)) && f.levels >= level;
        let base = self.exception_stack.iter().rposition(|f| !own(f)).map_or(0, |k| k + 1);
        self.next_cleanup_handler(base)
    }

    /* pops `Except` frames and the next `Finally` frame giving its handler or `None` at base */
    pub(crate) fn next_cleanup_handler(&mut self, exc_base: usize) -> Option<usize> {
        while self.exception_stack.len() > exc_base {
            let frame = self.exception_stack.pop().unwrap();
            self.levels.truncate(frame.levels);
            self.turned = frame.turned;
            self.iter_stack.truncate(frame.iter_depth);
            // discard reasons from finally bodies skipped while seeking this handler
            self.unwind_stack.truncate(frame.unwind_depth);
            self.handling.truncate(frame.handling_depth);
            if frame.kind == BlockKind::Finally {
                return Some(frame.handler_ip);
            }
        }
        None
    }

    /* the class name of a raised value or the runtime's plain error class for others */
    pub(crate) fn exc_type_name(&self, exc: Val) -> String {
        let plain = || String::from(self.natives.names().error(crate::lang::ErrKind::Error));
        match self.heap.class_of(exc) {
            Some(c) if self.heap.is_class(c) || self.is_error(exc) => self.heap.class_name(c).map_or_else(plain, String::from),
            _ => plain(),
        }
    }
}
