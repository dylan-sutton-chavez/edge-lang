use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use super::VM;
use crate::value::*;
use super::act::Dest;
use super::lower::Code;
use crate::code::Ins;
use super::exc::{is_stop, is_exit, handler_at};
use super::sections::section_at;
use crate::r#unsafe::{CodePtr, ChunkPtr};
use crate::lang::Language;

/* the loop of language `L` over the chunk at `pool` for a runtime's own loop */
pub(crate) fn driven<L: Language>(vm: &mut VM<'_>, chunk: &SSAChunk, pool: usize) -> Result<Val, VmErr> { vm.drive::<L>(chunk, pool) }

/* the one loop every language keeping the engine's loop rules shares */
pub(crate) fn engine_drive(vm: &mut VM<'_>, chunk: &SSAChunk, pool: usize) -> Result<Val, VmErr> { vm.drive::<crate::lang::Bare>(chunk, pool) }

/* the frame an instruction loop began with that the frames it opens return to last */
pub(crate) struct Floor {
    pub(crate) depth: usize,
    pub(crate) code: CodePtr,
    pub(crate) chunk: ChunkPtr,
    // the pool whose window the floor runs in
    pub(crate) pool: usize,
    // the lengths of the exception unwind and spread-delta stacks it began at
    pub(crate) bases: [usize; 3],
}

/* what a step of the loop leaves it to do with its frames */
enum Turn {
    Enter,
    // a class body handed to the loop which runs its chunk as a floor
    Floor(usize),
    Lower,
    Grow,
    Yield(Val),
    Return(Val),
    Raise(VmErr),
}

/* flags resume points in sections as lowered indices that code with sections keeps for good */
pub(crate) const RESUME_RAW: usize = 1 << 30;

/* the `VM` method `chunk_at` over the functions alone */
#[inline]
pub(crate) fn chunk_in<'c, 'a: 'c>(functions: &[&'a (Vec<alloc::string::String>, SSAChunk, u16, u16)], code: &Code, chunk: &'c SSAChunk, rip: usize) -> &'c SSAChunk {
    let Some(block) = code.block.as_deref() else { return chunk };
    // the code's own instructions lie before every section and the sections lie in order
    if block.sections.first().is_none_or(|s| rip < s.start as usize) { return chunk; }
    let k = block.sections.partition_point(|s| s.start as usize <= rip);
    match k.checked_sub(1).map(|k| &block.sections[k]).filter(|s| rip < s.end as usize) {
        Some(s) => &functions[s.fi].1,
        // heads and stubs laid past the end for a section's op read its function's chunk
        None if block.moved => match block.holding(rip) { Some((s, _)) => &functions[s.fi].1, None => chunk },
        None => chunk,
    }
}

impl<'a> VM<'a> {

    /* runs `chunk` on `frame` copied into its window and copies the window back after */
    pub(crate) fn exec(&mut self, chunk: &SSAChunk, frame: &mut Vec<Val>) -> Result<Val, VmErr> { self.exec_kept(chunk, frame, true) }

    /* `exec` copying the window back into `frame` only when `back` or the run suspended */
    fn exec_kept(&mut self, chunk: &SSAChunk, frame: &mut Vec<Val>, back: bool) -> Result<Val, VmErr> {
        let pool = self.pool_of(chunk);
        self.exec_pooled(chunk, pool, frame, back)
    }

    /* `exec_kept` in `pool` which is the pool of `chunk` */
    pub(crate) fn exec_pooled(&mut self, chunk: &SSAChunk, pool: usize, frame: &mut Vec<Val>, back: bool) -> Result<Val, VmErr> {
        // a resumed coroutine whose window still holds its state runs in it as it lies
        let co = core::mem::replace(&mut self.resuming, Val::undef());
        let co = if co.is_undef() { co } else { match self.heap.try_get(co) { Some(HeapObj::Coroutine(c)) if matches!(c.body, crate::value::BodyRef::Fn(fi) if self.fn_pool[fi] == pool) => co, _ => Val::undef() } };
        let kept = self.resumes_in(pool, self.pools[pool].active as usize, co);
        // a coroutine a loop frame left in another level's window takes its state back
        if !kept && !co.is_undef() { self.evict_into(co, frame); }
        let fresh = !kept && self.pools[pool].active == 0 && self.pools[pool].w0.base == super::window::NO_WINDOW;
        let base = if fresh { self.first_window(pool, frame, chunk.names.len()) } else { self.open_window(pool, frame.len().max(chunk.names.len()), chunk.names.len(), false) };
        if !kept && !fresh {
            self.regs[base..base + frame.len()].copy_from_slice(frame);
            // a frame of another length never came from this window so constants get laid again
            if frame.len() != self.pools[pool].wlen {
                self.pools[pool].wcode = 0;
                self.note_window(pool);
            }
        }
        let result = self.exec_in(chunk, base, pool);
        frame.clear();
        // a suspended first-level coroutine owns the window with its state until another run takes it
        if !co.is_undef() && self.yielded && result.is_ok() {
            let level = self.pools[pool].active as usize - 1;
            self.close_window(pool);
            self.own(pool, level, co);
            return result;
        }
        if back || self.yielded {
            let (at, len) = (self.pools[pool].window, self.pools[pool].wlen);
            frame.extend_from_slice(&self.regs[at..at + len]);
        }
        self.close_window(pool);
        result
    }

    /* the index of `chunk`'s pool made empty on its first run */
    pub(crate) fn pool_of(&mut self, chunk: &SSAChunk) -> usize {
        let at = self.body_at(chunk);
        if self.bodies[at].pool != super::NO_BODY { return self.bodies[at].pool as usize; }
        self.pools.push(Default::default());
        self.bodies[at].pool = (self.pools.len() - 1) as u32;
        self.pools.len() - 1
    }

    /* runs the frame at `base` whose registers top the register stack */
    pub(crate) fn exec_in(&mut self, chunk: &SSAChunk, base: usize, pool: usize) -> Result<Val, VmErr> {
        // loops nest on the host stack so one bound caps them whatever started them
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let outer = core::mem::replace(&mut self.pool_now, pool);
        // the ops of a loop a host call nests deliver their jobs as their own
        let to = core::mem::replace(&mut self.job_to, Dest::Top);
        // a nested floor over rows leaves the frame below its own top
        let top = self.top;
        self.base = base;
        self.depth += 1;
        #[cfg(any(debug_assertions, feature = "coverage"))]
        self.loops.open(self.hosted != 0);
        let result = if self.own_loop { (self.rt.drive)(self, chunk, pool) } else { self.drive::<crate::lang::Bare>(chunk, pool) };
        #[cfg(any(debug_assertions, feature = "coverage"))]
        self.loops.close();
        self.depth -= 1;
        (self.job_to, self.top) = (to, top);
        self.run_in(outer);
        result
    }

    /* lays missing `code` in `pool`'s window out of line as only frame changes call it */
    #[inline(never)]
    pub(crate) fn lay(&mut self, chunk: &SSAChunk, code: &Code, pool: usize) {
        // code over rows lays its rows alone once debug builds no longer check its depth
        if !self.laid(code, pool) { if code.rows != 0 && !cfg!(debug_assertions) { self.lay_rows(code, pool) } else { self.grow_frame(chunk, code, pool) } }
    }

    /* `set_ret` `deliver` `top_linked` and `top` out of line for loop paths ending jobs and generators */
    #[inline(never)]
    fn set_ret_apart(&mut self, ret: usize, dest: u16) { self.set_ret(ret, dest) }

    #[inline(never)]
    fn deliver_apart(&mut self, v: Val, ret: usize, dest: u16) -> usize { self.deliver(v, ret, dest) }

    #[inline(never)]
    fn deliver_to_apart(&mut self, v: Val, ret: usize, to: Dest) -> usize { self.deliver_to(v, ret, to) }

    #[inline(never)]
    fn top_linked_apart(&self) -> bool { self.top_linked() }

    #[inline(never)]
    fn top_apart(&self) -> Option<super::act::Act> { self.top() }

    /* `run_of` out of line so the loop that takes it so often stays small */
    #[inline(never)]
    fn load_run(&mut self, floor: &Floor) -> (CodePtr, ChunkPtr, usize, usize) { self.run_of(floor) }

    /* the top frame's code chunk pool and exception base made current or the `floor`'s */
    #[inline(always)]
    fn run_of(&mut self, floor: &Floor) -> (CodePtr, ChunkPtr, usize, usize) {
        if self.acts > floor.depth {
            let (code, fi, top, b) = self.top_run();
            self.run_in(top);
            (code, ChunkPtr::of(&self.functions[fi].1), top, b.exc)
        } else {
            self.run_in(floor.pool);
            (floor.code, floor.chunk, floor.pool, floor.bases[0])
        }
    }

    /* where the running frame's code or the floor's takes a value at `ret` */
    #[cold]
    #[inline(never)]
    fn dest_below(&self, ret: usize, floor: &Floor) -> u16 {
        let code = if self.acts > floor.depth { self.top_run().0 } else { floor.code };
        super::act::dest_of(code.get().ins.get(ret))
    }

    /* the unwind and spread-delta stack lengths the running frame began at or the loop's `floor`'s */
    #[inline(never)]
    fn run_bases(&self, floor: &Floor) -> (usize, usize) {
        if self.acts > floor.depth { let b = self.top_run().3; (b.unwind, b.delta) } else { (floor.bases[1], floor.bases[2]) }
    }

    /* drives the register loop over the chunk's code taking each turn it leaves */
    fn drive<L: Language>(&mut self, chunk: &SSAChunk, pool: usize) -> Result<Val, VmErr> {
        // a coroutine resume pushed its handlers first so the handler search starts below them
        let mut exc_base = self.pending_exec_exc_base.take().unwrap_or(self.exception_stack.len());
        let outer_safe = core::mem::replace(&mut self.frame_safe, core::mem::take(&mut self.pending_exec_safe));
        // cleanup reasons belong to this frame's finally bodies so leftovers drop when it returns
        let mut unwind_base = self.unwind_stack.len();
        // drop spread-delta frames leaked by an aborted arg list on unwind
        let mut delta_base = self.pending.delta_save.len();
        // a class body this loop runs takes its chunk as the floor until it ends
        let (mut pool, floors0) = (pool, self.floors.len());

        let (mut floor_code, mut once) = self.frame_code(chunk, pool);
        // constants become values on the chunk's first run with strings in the live heap
        if self.pools[pool].consts.is_none() {
            match super::cache::const_vals(chunk, &mut self.heap) {
                Ok(consts) => self.pools[pool].consts = Some(consts),
                Err(e) => { self.frame_safe = outer_safe; return Err(e); }
            }
        }
        // the floor starts at its first row or takes back the top it parked with
        let parked = core::mem::take(&mut self.park_top);
        if parked as usize > self.pools[pool].wlen.max(floor_code.frame) { self.frame_safe = outer_safe; return Err(cold_runtime("a parked depth past the rows of its frame")); }
        self.top = if floor_code.rows != 0 { parked.max(u32::from(floor_code.rows)) } else { parked };
        if self.resume_ip == 0 { self.lay(chunk, &floor_code, pool); }
        // opened frames sit above `floor` and the innermost runs or else the loop's first frame
        let (mut floor, mut floor_chunk) = (self.acts, ChunkPtr::of(chunk));
        let (mut cur, mut at, mut now, mut exc) = (CodePtr::of(&floor_code), floor_chunk, pool, exc_base);
        // where the loop's frames return to last made once since only a lowering changes it
        let mut floor_at = Floor { depth: floor, code: CodePtr::of(&floor_code), chunk: floor_chunk, pool, bases: [exc_base, unwind_base, delta_base] };
        // takes the top frame as running or the loop's own once none is left
        macro_rules! load {
            () => {
                (cur, at, now, exc) = self.load_run(&floor_at);
            };
            // inlined where frames enter and return and the scheduler switches coroutines on every step
            (hot) => {
                (cur, at, now, exc) = self.run_of(&floor_at);
            };
        }
        let mut ip = match core::mem::take(&mut self.resume_ip) { 0 => cur.get().lowered(0), at => self.resume_at(chunk, cur.get(), pool, at, None) };
        // a coroutine waiting in helpers resumes in the innermost and each return opens the next
        if core::mem::take(&mut self.chain.next) { ip = self.chain_next(self.chain.coro, ip, super::act::dest_of(cur.get().ins.get(ip)), None); load!(); }
        // one-shot raise at the coroutine's park point for a cancellation or error delivered while parked
        let mut raised = self.resume_raise.take();
        // the call a raise came back from where it meets the frame that made it
        let mut raised_at = None;
        // a callee that suspended suspends its caller too at the instruction after the call
        let mut parking = false;
        // trades this loop's floor for the one `f` keeps
        macro_rules! swap_floor {
            ($f:expr) => {
                let f = &mut $f;
                if let Some(code) = &mut f.code { core::mem::swap(&mut floor_code, code); }
                core::mem::swap(&mut once, &mut f.once);
                core::mem::swap(&mut floor_chunk, &mut f.chunk);
                core::mem::swap(&mut pool, &mut f.pool);
                core::mem::swap(&mut floor, &mut f.depth);
                core::mem::swap(&mut self.frame_safe, &mut f.safe);
                let b = [exc_base, unwind_base, delta_base];
                [exc_base, unwind_base, delta_base] = f.bases;
                f.bases = b;
            };
        }
        // the next turn ends the floor's run with `r` suspended when `y` at its call
        let mut ending: Option<(Result<Val, VmErr>, bool)> = None;
        macro_rules! floor_end {
            ($r:expr, $y:expr) => {
                (ending, parking) = (Some(($r, $y)), true);
                continue;
            };
        }
        // opens the last of `floors` returning to `ip` or raising at `rip` when it cannot
        macro_rules! floor_begin {
            ($rip:expr) => {
                let rip = $rip;
                match self.floor_open() {
                    Ok(()) => {
                        if let Some(f) = self.floors.last_mut() {
                            (f.ip, f.rip, f.top) = (ip, rip, self.top);
                            swap_floor!(*f);
                            floor_at = Floor { depth: floor, code: CodePtr::of(&floor_code), chunk: floor_chunk, pool, bases: [exc_base, unwind_base, delta_base] };
                            load!();
                            ip = cur.get().lowered(0);
                            // a floor over rows starts at its first row
                            self.top = floor_code.rows as u32;
                        }
                    }
                    Err(e) => if let Err(e) = self.class_end(Err(e), false) { (raised, raised_at) = (Some(e), Some(rip)); },
                }
            };
        }
        // where the loop goes as `$went` says once frame `f` of a `Calls` job ended
        macro_rules! calls {
            ($went:expr, $f:expr) => {{
                let f = $f;
                match $went {
                    Ok(None) => {
                        (self.entered, self.yielded) = (false, false);
                        self.set_ret_apart(f.ret, f.dest);
                        load!();
                        let (code_ref, chunk): (&Code, &SSAChunk) = (cur.get(), at.get());
                        self.lay(chunk, code_ref, now);
                        ip = code_ref.lowered(0);
                    }
                    // a delegation's step runs the waits it ended as a `Then` job's generator does
                    Ok(Some(_)) if let Some((base, step)) = self.then_out.take() => {
                        let went = self.then_end(base as usize, step, f.ret, super::act::word_of(f.dest));
                        load!();
                        let (code_ref, chunk): (&Code, &SSAChunk) = (cur.get(), at.get());
                        self.lay(chunk, code_ref, now);
                        match went { Ok(i) => ip = i, Err((at, e)) => { raised = Some(e); raised_at = Some(at); ip = at; } }
                    }
                    Ok(Some(v)) => { load!(); ip = self.deliver_apart(v, f.ret, f.dest); }
                    Err(e) => { load!(); raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                }
            }};
        }
        macro_rules! after {
            ($drive:expr, $ret:expr, $dest:expr) => {{
                let ret = $ret;
                let drive = $drive;
                // the frame that made the builtin's job comes in view before its result lands
                let done = matches!(drive, Ok(super::jobs::Drive::Done(..)));
                if done { load!(); }
                let next = match drive { Ok(d) => Ok(self.after_drive(d, ret, $dest)), Err(e) => Err(e) };
                if !done { load!(); }
                match next { Ok(i) => ip = i, Err(e) => { raised = Some(e); raised_at = Some(ret - 1); ip = ret - 1; } }
                parking |= self.floor_req;
            }};
        }
        macro_rules! prefetched {
            ($ret:expr, $drive:expr) => {{
                let ret = $ret;
                if !$drive { ip = ret - 1; continue; }
                after!(self.gather_drive(), ret, super::act::NO_DEST);
                continue;
            }};
        }
        let result = loop {
            let turn = {
                // a suspension or an error the turn before left to pass on or raise here
                let mut carried = if parking {
                    if let Some((r, y)) = ending.take() {
                        let Some(f) = self.floors.last_mut() else { break Err(cold_runtime("a floor ended with none set aside")) };
                        swap_floor!(*f);
                        let (back, rip, dest, top) = (f.ip, f.rip, f.dest, f.top);
                        floor_at = Floor { depth: floor, code: CodePtr::of(&floor_code), chunk: floor_chunk, pool, bases: [exc_base, unwind_base, delta_base] };
                        self.floor_close();
                        load!();
                        // the frame that opened the floor takes back its top
                        self.top = top;
                        (ip, parking, raised_at) = (back, false, None);
                        if y { (self.yielded, self.resume_ip, self.resume_layout) = (false, 0, 0); }
                        self.entered = false;
                        match self.class_end(r, y) {
                            // the register form that made the class names where it goes
                            Ok(v) if dest != super::act::NO_DEST => self.regs[self.base + dest as usize] = v,
                            Ok(v) => self.push(v),
                            Err(e) => (raised, raised_at) = (Some(e), Some(rip)),
                        }
                        continue;
                    }
                    Some(Ok(None))
                } else { raised.take().map(Err) };
                let (mut code, mut chunk): (&Code, &SSAChunk) = (cur.get(), at.get());
                let mut exc_base = exc;
                let mut n = code.ins.len();
                loop {
                    let mut rip = ip;
                    let step = if let Some(step) = carried.take() {
                        if step.is_err() && let Some(r) = raised_at.take() { rip = r; }
                        // a floor a builtin's step asked for last turn opens now rising from its call
                        if self.floor_req { rip = ip.saturating_sub(1); }
                        step
                    } else {
                        // a call or return the register loop ran in place can leave another frame running
                        if ip < n && self.run_frames::<L>(code, chunk, &mut ip, &floor_at) {
                            load!(hot);
                            (code, chunk) = (cur.get(), at.get());
                            (n, exc_base) = (code.ins.len(), exc);
                        }
                        rip = ip;
                        // a call may fail or park with a builtin's floor rising from the op before
                        if let Some(e) = self.reg_error.take() { Err(e) } else if self.yielded {
                            if self.floor_req { rip = ip.saturating_sub(1); }
                            Ok(None)
                        } else if ip >= n {
                            Ok(Some(Val::none()))
                        } else {
                            let ins = code.ins[ip];
                            ip += 1;
                            // the ops the register loop leaves are turns of this driver
                            let named = self.chunk_at(code, chunk, rip);
                            self.turn_op(ins, rip, code, named, exc_base, &mut ip)
                        }
                    };
                    // an instruction pays for its native work whether the register loop ran it or not
                    let step = if self.heap.has_work() && let Err(e) = self.pay_work() { Err(e) } else { step };
                    match step {
                        Ok(None) if !self.yielded => continue,
                        // a floor a builtin asked for opens no frame whatever its step took it for
                        Ok(None) if self.floor_req => { (self.floor_req, self.yielded, self.entered, parking) = (false, false, false, false); break Turn::Floor(rip) }
                        Ok(None) if core::mem::take(&mut self.entered) || core::mem::take(&mut self.thrown) => { self.yielded = false; break Turn::Enter }
                        // a head that asked its code to grow runs again in the grown code
                        Ok(None) if self.growing.is_some() => { self.yielded = false; break Turn::Grow }
                        // a hot loop hands its frame over at the loop head
                        Ok(None) if self.tier_up => { self.yielded = false; self.tier_up = false; break Turn::Lower }
                        Ok(None) => { parking = false; break Turn::Yield(self.yield_out(code, ip)) }
                        Ok(Some(v)) => {
                            self.exception_stack.truncate(exc_base);
                            if !self.unwind_stack.is_empty() { self.unwind_stack.truncate(self.run_bases(&floor_at).0); }
                            break Turn::Return(v)
                        }
                        // an op's handler handed its call to the loop and the call's frame runs next
                        Err(e) => {
                            // a step that failed after asking for a floor closes the floor first
                            let e = if self.floor_req { match self.floor_dropped(e) { Err(e) => e, Ok(_) => continue } } else { e };
                            if matches!(e, VmErr::Redo) { (self.yielded, self.entered) = (false, false); break Turn::Enter }
                            if code.block.is_some() { self.section_error(code, chunk, rip); }
                            match self.catch(code, chunk, &mut ip, exc_base, self.run_bases(&floor_at).1, e, rip) {
                                Ok(()) => continue,
                                Err(e) => break Turn::Raise(e),
                            }
                        }
                    }
                }
            };
            match turn {
                Turn::Floor(rip) => { floor_begin!(rip); }
                Turn::Enter => {
                    // a generator frame returns to the `for` stepping it and resumes where it last yielded
                    let generator = self.top().is_some_and(|f| !f.generator.is_undef());
                    // an op method's frame carries on at its op which its opening noted
                    if let Some(f) = self.top() && !generator && !f.dunder { self.set_ret(ip, f.dest); }
                    // a generator a builtin runs returns past the builtin's call
                    if generator && self.top_gather() { self.gather_ret(ip); }
                    load!(hot);
                    let (code_ref, chunk): (&Code, &SSAChunk) = (cur.get(), at.get());
                    if !self.laid(code_ref, now) { self.grow_frame(chunk, code_ref, now); }
                    ip = if generator { self.gen_entry } else { code_ref.lowered(0) };
                    // a generator's top is the one its park kept which its resume set
                    if !generator { self.top = code_ref.rows as u32; }
                    // a generator thrown into raises at its park point as any resume would
                    if generator && self.resume_raise.is_some() { (raised, raised_at) = (self.resume_raise.take(), None); }
                    // a generator waiting inside helpers opens them over its frame and the innermost carries on
                    if generator && self.chain.next { self.chain.next = false; ip = self.chain_next(self.chain.coro, ip, super::act::dest_of(code_ref.ins.get(ip)), None); load!(); }
                }
                Turn::Grow => {
                    let (chunk, pool, old): (&SSAChunk, usize, &Code) = (at.get(), now, cur.get());
                    let Some(&(asked, head, kind, func)) = self.growing.take().as_deref() else { continue };
                    // the frame goes on in newer same-lineage code keeping its places or grows its own
                    let lineage = |c: &Code| c.block.as_deref().map_or(c.layout, |b| b.lineage);
                    let newest = self.pools[pool].code.clone().filter(|c| lineage(c) == lineage(old) && c.ins.len() >= old.ins.len());
                    let next = match newest {
                        Some(c) if CodePtr::of(&c) != cur => Some(c),
                        Some(_) if asked == cur => self.grown_code(chunk, pool, head, kind, func),
                        _ => None,
                    };
                    if let Some(next) = next {
                        if self.acts > floor {
                            let h = self.head_at(self.top_pool) + 4;
                            self.regs[h] = CodePtr::of(&next).word();
                            if self.top_job() { self.refresh_again(CodePtr::of(&next)); }
                            self.unflat_top(&next);
                        } else {
                            (floor_code, once) = (next, false);
                            floor_at.code = CodePtr::of(&floor_code);
                        }
                        load!();
                        let code_ref: &Code = cur.get();
                        self.lay(chunk, code_ref, now);
                        // grown code over registers reruns the asking op with no row in use
                        if code_ref.rows == 0 { self.top = 0; }
                    }
                }
                Turn::Lower => {
                    // a hot unlowered loop resumes in lowered code at its head in the running frame
                    let (chunk, pool): (&SSAChunk, usize) = (at.get(), now);
                    // what the old code kept in rows at the loop head
                    let was_rows = { let old: &Code = cur.get(); old.rows };
                    let old_rows = (self.top as usize).saturating_sub(was_rows as usize);
                    // the code that ran unlowered tells the lowering what its calls found
                    self.hint = Some(floor_code.clone()).filter(|c| CodePtr::of(c) == cur);
                    // code laid again resumes at the compiler instruction it stood at in the new code
                    let relaying = core::mem::take(&mut self.relaying);
                    // a frame alone on the old code needs no heads kept so it lowers whole
                    if relaying { let old: &Code = cur.get(); ip = old.orig(ip) as usize; }
                    self.pools[pool].whole = relaying && self.pools[pool].active <= 1;
                    // a module's floor never runs what it ran before this loop again
                    self.lower_from = if self.acts == floor && !relaying { ip } else { 0 };
                    let lowered = self.lowered_code(chunk, pool, true);
                    self.lower_from = 0;
                    self.hint = None;
                    // a frame on first code moves to its lowered code as one laid again does
                    if self.acts > floor {
                        let at = self.head_at(self.top_pool) + 4;
                        self.regs[at] = CodePtr::of(&lowered).word();
                        // a job's callback runs the new code again too
                        if self.top_job() { self.refresh_again(CodePtr::of(&lowered)); }
                        self.unflat_top(&lowered);
                    }
                    // a call's code is its pool's which the lowering just made
                    match self.acts > floor {
                        true => {}
                        false => {
                            if once { self.pools[pool].heat += floor_code.hot.get(); }
                            (floor_code, once) = (lowered, false);
                            floor_at.code = CodePtr::of(&floor_code);
                        }
                    }
                    load!();
                    let code_ref: &Code = cur.get();
                    // the rows lie where the new code reads them so lowered code keeps no top
                    debug_assert!(was_rows == 0 || code_ref.rows != 0 || ip & RESUME_RAW != 0 || code_ref.entry_rows(ip) == old_rows, "a loop head of {:?} lowered over other rows", chunk.qualname);
                    if code_ref.rows == 0 { self.top = 0; }
                    self.lay(chunk, code_ref, now);
                    ip = code_ref.lowered(ip);
                    // the loops over generators around the head go on over what the run made
                    if code_ref.block.as_deref().is_some_and(|b| !b.loops.is_empty()) { self.enter_loops(code_ref, ip); }
                    // a loop with its generator laid as a section adopts the run's generator and state
                    if let Some(next) = self.adopt_gen(code_ref, ip) { ip = next; }
                }
                Turn::Return(v) => {
                    if self.acts == floor {
                        if self.floors.len() > floors0 { floor_end!(Ok(v), false); }
                        break Ok(v);
                    }
                    // a key a builtin over a generator called hands its result to the builtin
                    if self.top_job() && let Some(&super::jobs::Job::Fold(d)) = self.jobs.last() && let Some(f) = self.top_apart() {
                        // a builtin stepping a container itself goes to its next fold or its result
                        if d.is_native() {
                            self.jobs.pop();
                            let out = match self.fold_take(d, v) {
                                Ok(()) => self.fold_drive(),
                                Err(e) => { self.jobs.pop(); Err(e) }
                            };
                            if let Ok(super::jobs::Drive::Fold(_, func, d2, args, n)) = out && func.0 == f.callee.0 {
                                self.jobs.push(super::jobs::Job::Fold(d2));
                                let bases = f.cold != super::act::FLAT;
                                if n == 2 { self.rearm2(f.pool, f.fi, f.callee, f.levels, bases, (args[0], args[1])); } else { self.rearm(f.pool, f.fi, f.callee, f.levels, bases, args[0]); }
                                load!();
                                ip = cur.get().lowered(0);
                                continue;
                            }
                            // with the frame's own job gone a stand-in is what leaving it pops
                            self.jobs.push(super::jobs::Job::Stand);
                            self.leave_frame();
                            after!(self.drive_on(out), f.ret, f.dest);
                            continue;
                        }
                        self.leave_frame();
                        match self.fold_take(d, v) {
                            Ok(()) => { load!(); ip = f.ret; }
                            Err(e) => {
                                // the builtin failed so the generator stays suspended where it yielded
                                let ret = self.yield_gen(d.resume());
                                load!();
                                self.jobs.pop();
                                raised = Some(e); raised_at = Some(ret - 1); ip = ret - 1;
                            }
                        }
                        continue;
                    }
                    // a generator that returns before yielding gives `next` its default or ends the iteration
                    if self.top_gather() && let Some(&super::jobs::Job::NextGen(d, to)) = self.jobs.last() && let Some(f) = self.top_apart() {
                        self.finish_gen();
                        load!();
                        match self.next_end(d, v) {
                            Ok(r) => ip = self.deliver_to_apart(r, f.ret, to),
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // an ended generator a builtin iterator's step met acts as a yield of its end
                    if self.top_gather() && let Some(f) = self.top_apart() && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() && let super::jobs::Takes::Prefetch(drive) = j.takes {
                        self.finish_gen();
                        load!();
                        prefetched!(f.ret, drive);
                    }
                    // a generator whose items a builtin gathers ends the list the builtin then runs on
                    if self.top_gather() && let Some(f) = self.top_apart() && let Some(super::jobs::Job::Gather(j)) = self.jobs.last()
                        && matches!(j.takes, super::jobs::Takes::Retry(..) | super::jobs::Takes::Hold) {
                        let j = self.take_gather();
                        self.finish_gen();
                        load!();
                        // the op that met the generator runs again with its items in its place
                        if let super::jobs::Takes::Retry(slot, _) = j.takes {
                            match self.retried(slot, j.items) {
                                Ok(()) => ip = f.ret - 1,
                                Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                            }
                            continue;
                        }
                        let to = j.to;
                        match self.collected(j) {
                            Ok(Some(r)) => ip = self.deliver_to_apart(r, f.ret, to),
                            // a builtin running user code opened a frame or floor so the loop parks
                            Ok(None) => (ip, parking) = (f.ret, true),
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // a returned generator ends its builtin whose job leaves a stand-in the frame's end drops
                    if self.top_gather() && let Some(f) = self.top_apart() && let Some(job @ super::jobs::Job::Gather(_)) = self.jobs.last_mut()
                        && let super::jobs::Job::Gather(mut j) = core::mem::replace(job, super::jobs::Job::Map { before: Val::undef(), ctor: false }) {
                        self.finish_gen();
                        let end = self.gather_end(&mut j, v);
                        if self.entered && matches!(end, Ok(r) if r.is_undef()) { after!(Ok(super::jobs::Drive::Opened), f.ret, super::act::NO_DEST); continue; }
                        // the frame below comes in view before its result lands and again past opened waits
                        let then = self.then_out.is_some();
                        load!();
                        match self.job_out(end, f.ret, j.to) {
                            Ok(i) => { if then { load!(); } ip = i; }
                            Err((at, e)) => { if then { load!(); } raised = Some(e); raised_at = Some(at); ip = at; }
                        }
                        continue;
                    }
                    // a returned generator ends the `for` stepping it and a `yield from` takes its value
                    if self.top().is_some_and(|f| !f.generator.is_undef()) {
                        let at = self.finish_gen();
                        load!();
                        self.yield_from_value = v;
                        self.iter_stack.pop();
                        ip = cur.get().ins.get(at).map_or(at, |ins| ins.a as usize);
                        continue;
                    }
                    // a builtin's callback hands what it returned to the builtin which may call it again
                    if self.top_job() && let Some(f) = self.top_apart() {
                        // a builtin calling in frames takes what this one gave and makes its next call
                        if matches!(self.jobs.last(), Some(super::jobs::Job::Calls(..))) {
                            let call = self.calls_step(Some(Ok(v)));
                            // the same lowered function called again reruns in this frame as a builtin's callback does
                            if let Ok(super::jobs::Call::Frame(g, args)) = &call && g.0 == f.callee.0 && !self.pools[f.pool].cold {
                                let again = match *super::jobs::call_args(args) {
                                    [x] => { self.rearm(f.pool, f.fi, f.callee, f.levels, f.cold != super::act::FLAT, x); true }
                                    [x, y] => { self.rearm2(f.pool, f.fi, f.callee, f.levels, f.cold != super::act::FLAT, (x, y)); true }
                                    _ => false,
                                };
                                if again {
                                    load!();
                                    ip = cur.get().lowered(0);
                                    continue;
                                }
                            }
                            self.calls_keep();
                            self.leave_frame();
                            calls!(self.calls_from(call, true), f);
                            continue;
                        }
                        match self.job_take(v) {
                            Ok(super::jobs::Step::Again(arg)) => { self.rearm(f.pool, f.fi, f.callee, f.levels, f.cold != super::act::FLAT, arg); load!(); ip = cur.get().lowered(0); }
                            Ok(super::jobs::Step::Again2(x, y)) => { self.rearm2(f.pool, f.fi, f.callee, f.levels, f.cold != super::act::FLAT, (x, y)); load!(); ip = cur.get().lowered(0); }
                            // a step a builtin iterator's call made steps the builtin's job on
                            Ok(super::jobs::Step::Drive) => { self.leave_frame(); after!(self.gather_drive(), f.ret, super::act::NO_DEST); }
                            step => {
                                // errors rise from the builtin's call or the `for` a `map` or `filter` returns to
                                let at = f.ret - !matches!(self.jobs.last(), Some(super::jobs::Job::Map { .. } | super::jobs::Job::Filter { .. })) as usize;
                                let to = super::act::job_dest(self.gather_to(), f.dest);
                                self.leave_frame();
                                load!();
                                match step {
                                    Ok(step) => ip = self.job_done(step, cur.get(), f.ret, to),
                                    Err(e) => { raised = Some(e); raised_at = Some(at); ip = at; }
                                }
                            }
                        }
                        continue;
                    }
                    if self.top_linked() {
                        // a coroutine's helper hands its return to the helper kept under it which then opens
                        if let Some((co, ret)) = self.chain_top() {
                            let (_, _, v, _) = self.return_frame(v);
                            let dest = self.dest_below(ret, &floor_at);
                            let at = self.chain_next(co, ret, dest, Some(v));
                            load!();
                            ip = at;
                            continue;
                        }
                        if let Err(e) = self.unlink_top() { raised = Some(e); continue; }
                    }
                    let (ret, dest, v, dunder) = self.return_frame(v);
                    load!(hot);
                    ip = match dunder { Some(func) => self.dunder_back(v, ret, dest, func), None => self.deliver(v, ret, dest) };
                }
                Turn::Yield(v) => {
                    if self.acts == floor {
                        if self.floors.len() > floors0 { floor_end!(Ok(Val::none()), true); }
                        self.park_top = self.parked_top(&floor_code, self.resume_ip);
                        break Ok(v);
                    }
                    if self.top().is_some_and(|f| !f.generator.is_undef()) {
                        self.yielded = false;
                        // a generator a `for` steps parks where it waits down to the first non-generator frame
                        if self.waits() && !self.top_gather() {
                            loop {
                                let step = self.park_gen((floor, pool, CodePtr::of(&floor_code)));
                                let stepping = if self.acts > floor { self.top_run().0 } else { CodePtr::of(&floor_code) };
                                self.resume_ip = stepping.get().resume(step);
                                if !(self.acts > floor && self.top().is_some_and(|f| !f.generator.is_undef())) { break; }
                            }
                            self.yielded = true;
                            if self.acts == floor { self.run_in(pool); self.park_top = self.parked_top(&floor_code, self.resume_ip); break Ok(Val::none()); }
                            ip = self.park_frame();
                            load!();
                            parking = true;
                            continue;
                        }
                        // a generator a builtin steps only hands it values so waiting there is an error
                        if self.waits() { raised = Some(self.suspended_in_generator(self.top_apart().map(|f| f.fi))); parking = false; continue; }
                        let resume = core::mem::take(&mut self.resume_ip);
                        // a generator runs past each yield its builtin takes but stays suspended there for `next`
                        if self.top_gather() && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() && let super::jobs::Takes::Prefetch(drive) = j.takes {
                            let g = self.head(self.head_at(self.top_pool) + 2);
                            let ret = self.yield_gen(resume);
                            self.jobs.pop();
                            if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) { c.pending = v; }
                            load!();
                            parking = false;
                            prefetched!(ret, drive);
                        }
                        if self.top_gather() && let Some(&super::jobs::Job::NextGen(_, to)) = self.jobs.last() {
                            let ret = self.yield_gen(resume);
                            load!();
                            self.jobs.pop();
                            ip = self.deliver_to_apart(v, ret, to);
                            parking = false;
                            continue;
                        }
                        if self.top_gather() {
                            // a key the builtin calls on the value runs as a frame over the generator's
                            if let Some((fi, func, fold, args, n)) = self.fold_of(v, resume) {
                                if self.budget == 0 { raised = Some(VmErr::Budget); parking = false; continue; }
                                self.open_fold(fi, func, fold, args, n);
                                let back = if resume & RESUME_RAW != 0 { resume & !RESUME_RAW } else { cur.get().lowered(resume) };
                                self.set_ret_apart(back, super::act::NO_DEST);
                                load!();
                                let (code_ref, chunk): (&Code, &SSAChunk) = (cur.get(), at.get());
                                self.lay(chunk, code_ref, now);
                                ip = code_ref.lowered(0);
                                parking = false;
                                continue;
                            }
                            match self.gather_take(v) {
                                Ok(None) => {
                                    ip = if resume & RESUME_RAW != 0 { resume & !RESUME_RAW } else { cur.get().lowered(resume) };
                                    parking = false;
                                    continue;
                                }
                                // a result or an error leaves the generator suspended where it yielded
                                step => {
                                    let ret = self.yield_gen(resume);
                                    let (to, then) = (self.gather_to(), self.then_out.is_some());
                                    self.jobs.pop();
                                    load!();
                                    match self.job_out(step.map(|r| r.unwrap_or(Val::none())), ret, to) {
                                        Ok(i) => { if then { load!(); } ip = i; }
                                        Err((at, e)) => { if then { load!(); } raised = Some(e); raised_at = Some(at); ip = at; }
                                    }
                                    parking = false;
                                    continue;
                                }
                            }
                        }
                        let at = self.yield_gen(resume);
                        load!();
                        self.hand_to_for(cur.get(), at, v);
                        ip = at + 1;
                        parking = false;
                        continue;
                    }
                    // a builtin's callback hands its result to the builtin which cannot wait with it
                    if self.top().is_some_and(|f| f.job || self.acts - 1 > floor && self.mapped(&f) || self.acts - 1 == floor && f.dest != super::act::NO_DEST
                        && floor_code.ins.get(f.ret).is_some_and(|i| i.op == OpCode::ForIterR) && matches!(self.iter_stack.last(), Some(IterFrame::Map { .. }))) {
                        self.yielded = false;
                        raised = Some(self.suspended_in(" a callback a builtin runs"));
                        parking = false;
                        continue;
                    }
                    // an op whose call this is has nothing to take its result on resume
                    if self.top().is_some_and(|f| f.dunder) { self.yielded = false; raised = Some(self.suspended_in(" a call an op makes")); parking = false; continue; }
                    if self.top_linked_apart() && let Err(e) = self.unlink_top() { raised = Some(e); parking = false; continue; }
                    ip = self.park_frame();
                    load!();
                    parking = true;
                }
                Turn::Raise(e) => {
                    // an error leaves handler-free flat plain frames at once to rise at the call below
                    if self.acts > floor && !matches!(e, VmErr::HostYield(_) | VmErr::Redo) && e.missing_name().is_none() && !is_stop(&e, self.natives.names()) && let Some(at) = self.unwind_plain(&floor_at) {
                        load!();
                        (ip, raised, raised_at) = (at + 1, Some(e), Some(at));
                        continue;
                    }
                    let Some(f) = self.top().filter(|_| self.acts > floor) else {
                        if self.floors.len() > floors0 { floor_end!(Err(e), false); }
                        break Err(e)
                    };
                    let (fi, call_ip, generator) = (f.fi, f.call_ip, f.generator);
                    // what a delegation's generator frame raises goes on through the waits it ended
                    let e = if f.job && !generator.is_undef() {
                        match self.job_raised(e) {
                            Ok(Ok(at)) => { load!(); ip = at; continue; }
                            Ok(Err((at, e))) => { load!(); (raised, raised_at, ip) = (Some(e), Some(at), at); continue; }
                            Err(e) => e,
                        }
                    } else { e };
                    // a builtin calling in frames takes its frame's error keeping its job past the frame
                    if f.job && matches!(self.jobs.last(), Some(super::jobs::Job::Calls(..))) {
                        self.calls_keep();
                        self.leave_frame();
                        calls!(self.calls_go(Some(Err(e)), true), f);
                        continue;
                    }
                    // an end of iteration out of a `map` or `filter` function ends its `for`
                    let stop = is_stop(&e, self.natives.names()) && generator.is_undef();
                    let before = match self.jobs.last() {
                        Some(&super::jobs::Job::Map { before, .. } | &super::jobs::Job::Filter { before, .. }) if stop && f.job => Some((before, f.ret)),
                        _ => None,
                    };
                    // an error out of a builtin's key leaves the generator suspended where it yielded
                    if f.job && let Some(&super::jobs::Job::Fold(d)) = self.jobs.last() {
                        self.leave_frame();
                        // a builtin stepping a container itself ends with the error which rises from its call
                        if d.is_native() {
                            self.jobs.pop();
                            load!();
                            (raised, raised_at, ip) = (Some(e), Some(f.ret - 1), f.ret - 1);
                            continue;
                        }
                        let ret = self.yield_gen(d.resume());
                        load!();
                        self.jobs.pop();
                        (raised, raised_at, ip) = (Some(e), Some(ret - 1), ret - 1);
                        continue;
                    }
                    // an end of iteration out of a builtin iterator's step call ends that step only
                    if stop && f.job && let Some(drive) = self.feed_stopped() {
                        self.leave_frame();
                        if drive { after!(self.gather_drive(), f.ret, super::act::NO_DEST); } else { load!(); ip = f.ret; }
                        continue;
                    }
                    // an end of iteration out of a job's `map` or `filter` call ends the builtin
                    if stop && f.job && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() {
                        let before = Some(j.before).filter(|b| !b.is_undef());
                        let mut j = self.take_gather();
                        self.leave_frame();
                        load!();
                        (self.error_byte_pos, _) = (None, self.swallow_stop(before));
                        self.call_stack.clear();
                        match self.gather_end(&mut j, Val::none()) {
                            Ok(r) if r.is_undef() && self.entered => after!(Ok(super::jobs::Drive::Opened), f.ret, f.dest),
                            Ok(r) => ip = self.deliver_to_apart(r, f.ret, super::act::job_dest(j.to, f.dest)),
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // the close signal or an end of iteration out of the closed generator gives nil
                    if f.job && !generator.is_undef() && (is_stop(&e, self.natives.names()) || is_exit(&e, self.natives.names())) && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() && j.takes == super::jobs::Takes::Close {
                        let (before, to) = (Some(j.before).filter(|b| !b.is_undef()), j.to);
                        self.finish_gen();
                        load!();
                        (self.error_byte_pos, _) = (None, self.swallow_stop(before));
                        self.call_stack.clear();
                        // a generator's record holds no dest so the value goes where its job names
                        ip = self.deliver_to_apart(Val::none(), f.ret, to);
                        continue;
                    }
                    // a coroutine helper's error rises at the call in the helper kept under it
                    if self.top_linked() && let Some((co, ret)) = self.chain_top() {
                        self.leave_frame();
                        let dest = self.dest_below(ret, &floor_at);
                        let at = self.chain_raise(co, ret, dest, fi);
                        load!();
                        (ip, raised, raised_at) = (at, Some(e), None);
                        continue;
                    }
                    ip = f.ret;
                    let e = match self.top_linked().then(|| self.unlink_top()) { Some(Err(le)) => le, _ => e };
                    // a generator that raised stays finished and the error meets its `for`
                    if generator.is_undef() { self.leave_frame(); } else { self.finish_gen(); }
                    load!();
                    let before = before.or_else(|| (stop && !f.job && cur.get().ins.get(f.ret).is_some_and(|i| i.op == OpCode::ForIterR)
                        && matches!(self.iter_stack.last(), Some(IterFrame::Map { .. }))).then_some((Val::undef(), f.ret)));
                    if let Some((before, at)) = before {
                        (self.error_byte_pos, self.yield_from_value) = (None, self.swallow_stop(Some(before).filter(|v| !v.is_undef())));
                        self.call_stack.clear();
                        self.iter_stack.pop();
                        ip = cur.get().ins.get(at).map_or(at, |ins| ins.a as usize);
                        continue;
                    }
                    let (code_ref, caller): (&Code, &SSAChunk) = (cur.get(), at.get());
                    // a site call kept the caller's raw index and the compiler's is found only here
                    let (call_ip, raw) = match call_ip {
                        Some(o) if o & super::act::RAW_IP != 0 => { let i = (o & !super::act::RAW_IP) as usize; (Some(code_ref.orig(i)), Some(i)) }
                        other => (other, None),
                    };
                    // a call inside a section was made in the function the section was laid from
                    match raw.and_then(|i| code_ref.block.as_deref().and_then(|b| b.holding(i)).map(|(s, i)| (i, s))) {
                        Some((i, s)) => { let o = s.orig.get(i - s.start as usize).copied(); self.note_frame(fi, o, &self.functions[s.fi].1) }
                        None => self.note_frame(fi, call_ip, caller),
                    }
                    // an op handler's call that failed gives the op its error as it runs again
                    if f.dunder && f.dest == super::act::REDO { ip = self.redo_back(f.callee, f.ret, Err(e)); continue; }
                    raised = Some(e);
                    raised_at = Some(raw.unwrap_or_else(|| call_ip.map_or(ip, |o| code_ref.lowered(o as usize))));
                }
            }
        };
        // a run of text an error broke out of the loop with ends with it
        while self.floors.len() > floors0 && let Some(f) = self.floors.last_mut() {
            // a floor the loop never took has no window to close
            if f.code.is_some() {
                swap_floor!(*f);
                self.floor_close();
            }
            let _ = self.class_end(Err(cold_runtime("a class body ended by an error")), false);
        }
        if once { self.pools[pool].heat += floor_code.hot.get(); }
        self.frame_safe = outer_safe;
        result
    }

    /* runs a return finally end or await the register loop leaves to its driver */
    #[inline(never)]
    fn turn_op(&mut self, ins: Ins, rip: usize, code: &Code, chunk: &SSAChunk, exc_base: usize, ip: &mut usize) -> Result<Option<Val>, VmErr> {
        match ins.op {
            OpCode::ReturnR => {
                let result = self.reg(chunk, ins.b)?;
                // run any enclosing finally cleanup before the value leaves the frame
                if self.exception_stack.len() > exc_base && let Some(h) = self.next_cleanup_handler(exc_base) {
                    self.unwind_stack.push(Unwind::Return(result));
                    *ip = handler_at(code, h);
                    return Ok(None);
                }
                Ok(Some(result))
            }
            // code over rows returns its top row past the finally bodies around it
            OpCode::ReturnValue => self.rows_return(code, exc_base, ip),
            // a `finally` of a section ends its return in the section's return
            OpCode::EndFinally if matches!(self.unwind_stack.last(), Some(Unwind::Return(_))) && section_at(code, rip).is_some() => { self.end_section_finally(code, rip, ip); Ok(None) }
            // a finally body ends by popping its reason and resuming the exit it carried
            OpCode::EndFinally => match self.unwind_stack.pop() {
                None | Some(Unwind::Normal) => Ok(None),
                Some(Unwind::Return(v)) => match self.next_cleanup_handler(exc_base) {
                    // a try begins on an empty stack so its finally starts at the first row
                    Some(h) => { self.unwind_stack.push(Unwind::Return(v)); (self.top, *ip) = (u32::from(code.rows), handler_at(code, h)); Ok(None) }
                    None => Ok(Some(v)),
                },
                Some(Unwind::Goto { target, remaining }) => {
                    if remaining > 0 && let Some(h) = self.next_cleanup_handler(exc_base) {
                        self.unwind_stack.push(Unwind::Goto { target, remaining: remaining - 1 });
                        (self.top, *ip) = (u32::from(code.rows), handler_at(code, h));
                    } else {
                        *ip = code.lowered(target);
                    }
                    Ok(None)
                }
                Some(Unwind::Reraise(e, at, exc)) => {
                    self.error_byte_pos = at;
                    self.pending.exc_val = Some(exc);
                    Err(e)
                }
            },
            // a break or continue across finally blocks whose Jump after completes the transfer
            OpCode::UnwindFinally => {
                let target = code.resume(*ip);
                if ins.a > 0 && let Some(h) = self.next_cleanup_handler(exc_base) {
                    self.unwind_stack.push(Unwind::Goto { target, remaining: ins.a - 1 });
                    (self.top, *ip) = (u32::from(code.rows), handler_at(code, h));
                }
                Ok(None)
            }
            // sends the top into the awaitable below yielding up to rerun the step on resume
            OpCode::Await => {
                let sent = self.pop()?;
                let at = self.under(1)?;
                match self.await_on(at, sent, chunk)? {
                    Some(crate::vm::coro::Step::Yield(v)) => {
                        self.push(v);
                        self.yielded = true;
                        *ip -= 1;
                    }
                    Some(crate::vm::coro::Step::Return(v)) => self.set_slot(at, v),
                    // the frame it opened comes back to this op
                    None => *ip -= 1,
                }
                Ok(None)
            }
            _ => Err(cold_runtime("an op the register loop left its driver")),
        }
    }

    /* opens the last floor asked for like a nested run and undoes it on error */
    #[inline(never)]
    fn floor_open(&mut self) -> Result<(), VmErr> {
        let Some(f) = self.floors.last_mut() else { return Err(cold_runtime("a floor opened with none asked for")) };
        let (chunk, mut slots, cost) = (f.chunk, core::mem::take(&mut f.slots), f.cost);
        let chunk: &SSAChunk = chunk.get();
        let (pool, names) = (self.pool_of(chunk), chunk.names.len());
        let fresh = self.pools[pool].active == 0 && self.pools[pool].w0.base == super::window::NO_WINDOW;
        let base = if fresh { self.first_window(pool, &slots, names) } else { self.open_window(pool, slots.len().max(names), names, false) };
        if !fresh {
            self.regs[base..base + slots.len()].copy_from_slice(&slots);
            // a frame of another length never came from this window so constants get laid again
            if slots.len() != self.pools[pool].wlen {
                self.pools[pool].wcode = 0;
                self.note_window(pool);
            }
        }
        self.run_in(pool);
        // it nests as deep as its own run would
        self.depth += cost;
        let (code, once) = self.frame_code(chunk, pool);
        if self.pools[pool].consts.is_none() {
            match super::cache::const_vals(chunk, &mut self.heap) {
                Ok(consts) => self.pools[pool].consts = Some(consts),
                Err(e) => {
                    self.depth -= cost;
                    self.close_window(pool);
                    return Err(e);
                }
            }
        }
        if !self.laid(&code, pool) { self.grow_frame(chunk, &code, pool); }
        let bases = [self.exception_stack.len(), self.unwind_stack.len(), self.pending.delta_save.len()];
        let depth = self.acts;
        // slots keep their room for a class body's members which its end reads back
        slots.clear();
        if let Some(f) = self.floors.last_mut() { (f.code, f.once, f.pool, f.depth, f.bases, f.slots) = (Some(code), once, pool, depth, bases, slots); }
        Ok(())
    }

    /* the error of a step that failed after asking for a floor that never opens */
    #[cold]
    #[inline(never)]
    fn floor_dropped(&mut self, e: VmErr) -> Result<Option<Val>, VmErr> {
        (self.floor_req, self.yielded) = (false, false);
        self.class_end(Err(e), false).map(|_| None)
    }

    /* ends the last of `floors` closing its window and giving back its depth */
    fn floor_close(&mut self) {
        let Some(f) = self.floors.last() else { return };
        let (heat, pool, cost) = (if f.once { f.code.as_ref().map_or(0, |c| c.hot.get()) } else { 0 }, f.pool, f.cost);
        // a class body's slots are its members which its end reads
        if matches!(f.run, crate::vm::frame::FloorRun::Class(_)) && let Some(f) = self.floors.last_mut() {
            let (at, names) = (self.pools[pool].window, f.chunk.get().names.len());
            f.slots.extend_from_slice(&self.regs[at..at + names]);
        }
        self.pools[pool].heat += heat;
        self.depth -= cost;
        self.close_window(pool);
    }

    /* runs register forms from `ip` through plain calls over `floor` true when frames switched */
    #[inline(never)]
    fn run_frames<L: Language>(&mut self, code: &Code, chunk: &SSAChunk, ip: &mut usize, floor: &Floor) -> bool {
        let (mut code, mut chunk, mut switched) = (code, chunk, false);
        loop {
            switched |= self.run_regs::<L>(&mut code, &mut chunk, ip, floor);
            let i = *ip;
            if self.entered && !self.floor_req && self.reg_error.is_none() && !self.chain.next {
                // a call it let in runs next in the frame it opened
                (self.entered, self.yielded) = (false, false);
                // a result the next instruction would store goes straight to its register
                let dest = super::act::dest_of(code.ins.get(i));
                // a generator a builtin runs carries on where it last yielded
                let stepping = self.top_gather();
                // no frame opened leaves the loop to its driver
                let Some((c, k)) = self.regs_enter(i, dest) else { return switched };
                // the frame just opened runs code its pool keeps and a chunk the VM keeps
                (code, chunk) = (c.get(), k.get());
                *ip = if stepping { self.gen_entry } else { code.lowered(0) };
                // a generator's resume set the top its park kept
                if !stepping { self.top = code.rows as u32; }
            } else if self.acts > floor.depth && !self.yielded && let Some(&ins) = code.ins.get(i) && matches!(ins.op, OpCode::ReturnR | OpCode::ReturnValue) && let Some((ret, c, k)) = self.regs_return(ins, floor) {
                (code, chunk) = (c.get(), k.get());
                *ip = ret;
            } else if i >= code.ins.len() && self.acts > floor.depth && !self.yielded && let Some((ret, c, k)) = self.regs_end(code, floor) {
                (code, chunk) = (c.get(), k.get());
                *ip = ret;
            } else {
                return switched;
            }
            switched = true;
        }
    }

    // leaves the top flat plain frames each noted in the traceback giving the call below
    #[cold]
    #[inline(never)]
    fn unwind_plain(&mut self, floor: &Floor) -> Option<usize> {
        let plain = |vm: &Self| vm.acts > floor.depth && { let h = vm.head_at(vm.top_pool); vm.head(h).as_int() >> 40 == 0 && CodePtr::from_word(vm.head(h + 4)).get().block.is_none() };
        if !plain(self) { return None; }
        let mut at = 0;
        while plain(self) {
            let (pool, h) = (self.top_pool, self.head_at(self.top_pool));
            let (ret, below) = ((self.head(h).as_int() & 0xFF_FFFF) as usize, self.head(h + 1).as_int());
            let (fi, under) = ((below >> 24) as usize, (below & 0xFF_FFFF) as usize);
            (self.top_pool, self.acts, self.top) = (under, self.acts - 1, super::act::top_of(self.head(h + 3)));
            self.close_window(pool);
            let (c, k) = if self.acts > floor.depth {
                let h = self.head_at(under);
                (CodePtr::from_word(self.head(h + 4)), ChunkPtr::of(&self.functions[(self.head(h + 1).as_int() >> 24) as usize].1))
            } else { (floor.code, floor.chunk) };
            let (caller, k) = (c.get(), k.get());
            at = ret - 1;
            // a call inside a section was made in the function the section was laid from
            match caller.block.as_deref().and_then(|b| b.holding(at)) {
                Some((s, j)) => { let o = s.orig.get(j - s.start as usize).copied(); self.note_frame(fi, o, &self.functions[s.fi].1); }
                None => self.note_frame(fi, Some(caller.orig(at)), k),
            }
        }
        Some(at)
    }

    /* whether running frame `f` is a `map` call returning to the `for` stepping it below */
    #[inline(never)]
    fn mapped(&self, f: &super::act::Act) -> bool {
        let below = (self.head(self.head_at(self.top_pool) + 1).as_int() & 0xFF_FFFF) as usize;
        let code = CodePtr::from_word(self.head(self.head_at(below) + 4));
        f.dest != super::act::NO_DEST && code.get().ins.get(f.ret).is_some_and(|i| i.op == OpCode::ForIterR) && matches!(self.iter_stack.last(), Some(IterFrame::Map { .. }))
    }

    /* makes current the frame a register loop call just opened giving its code and chunk */
    #[inline(always)]
    fn regs_enter(&mut self, ret: usize, dest: u16) -> Option<(CodePtr, ChunkPtr)> {
        if self.acts == 0 { return None }
        // the record before the window holds the frame's code and function which are read alone
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        self.set_ret_at(at, ret, dest);
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        let (code, chunk) = (CodePtr::from_word(self.head(at + 4)), ChunkPtr::of(&self.functions[fi].1));
        self.run_in(pool);
        // the frame just pushed holds its code and the VM keeps its chunk
        let (c, k): (&Code, &SSAChunk) = (code.get(), chunk.get());
        if !self.laid(c, pool) { self.grow_frame(k, c, pool); }
        Some((code, chunk))
    }

    /* ends the innermost frame on a plain return or `None` when it needs its handler */
    #[inline(always)]
    fn regs_return(&mut self, ins: Ins, floor: &Floor) -> Option<(usize, CodePtr, ChunkPtr)> {
        if self.top_job() { if self.top_gather() { return None; } return self.job_return(ins, floor); }
        let f = self.top().filter(|f| f.generator.is_undef())?;
        if self.top_linked() { return None; }
        // code over rows returns its top row and None with no rows
        let v = match ins.op {
            OpCode::ReturnR => self.regs[self.pools[f.pool].window + ins.b as usize],
            _ => self.rows_value(f.code.get()),
        };
        // a return under a handler runs its cleanup first which its driver does
        let b = self.bases_of(&f);
        if v.is_undef() || self.exception_stack.len() > b.exc { return None; }
        if self.unwind_stack.len() > b.unwind { self.unwind_stack.truncate(b.unwind); }
        let (ret, dest, v, dunder) = self.return_frame(v);
        let (code, chunk) = match self.top() {
            Some(f) if self.acts > floor.depth => { let (pool, at) = (f.pool, (f.code, ChunkPtr::of(&self.functions[f.fi].1))); self.run_in(pool); at }
            _ => { self.run_in(floor.pool); (floor.code, floor.chunk) }
        };
        let next = match dunder { Some(func) => self.dunder_back(v, ret, dest, func), None => self.deliver(v, ret, dest) };
        Some((next, code, chunk))
    }

    /* ends the innermost frame running past its last instruction as a return of none */
    #[inline(never)]
    fn regs_end(&mut self, code: &Code, floor: &Floor) -> Option<(usize, CodePtr, ChunkPtr)> {
        // a plain frame hands back None like a register return and others return over rows
        if let Some((c, k, next)) = self.leave_site(Val::none(), floor.code, floor.chunk, floor.pool, floor.depth) { return Some((next, c, k)); }
        let f = self.top().filter(|f| f.generator.is_undef() && !f.job)?;
        if self.exception_stack.len() > self.bases_of(&f).exc { return None; }
        // code falling off its end holds no row so it returns None
        let top = core::mem::replace(&mut self.top, u32::from(code.rows));
        let back = self.regs_return(Ins { op: OpCode::ReturnValue, x: 0, a: 0, b: 0, c: 0 }, floor);
        if back.is_none() { self.top = top; }
        back
    }

    /* the chunk `rip` of `code` reads names and constants from with sections reading their function's */
    #[inline]
    pub(crate) fn chunk_at<'c>(&self, code: &Code, chunk: &'c SSAChunk, rip: usize) -> &'c SSAChunk where 'a: 'c { chunk_in(&self.functions, code, chunk, rip) }
}

/* the bases and member flags and name a class body floor keeps until it ends */
pub(crate) struct ClassRun {
    pub(crate) bases: Vec<Val>,
    pub(crate) member: Vec<bool>,
    pub(crate) name: String,
}

/* what a floor runs which is a class body */
pub(crate) enum FloorRun { Class(ClassRun) }

/* the floor a class body set aside which the loop takes back once it ends */
pub(crate) struct Floored {
    pub(crate) code: Option<Rc<crate::vm::lower::Code>>,
    pub(crate) once: bool,
    pub(crate) chunk: ChunkPtr,
    pub(crate) pool: usize,
    pub(crate) depth: usize,
    pub(crate) bases: [usize; 3],
    pub(crate) safe: bool,
    pub(crate) ip: usize,
    pub(crate) rip: usize,
    // the top the frame that opened it takes back as it ends
    pub(crate) top: u32,
    // the register a register form wants the class in or `NO_DEST` to push it
    pub(crate) dest: u16,
    pub(crate) slots: Vec<Val>,
    pub(crate) cost: usize,
    pub(crate) run: FloorRun,
}

impl Floored {

    /* a floor of `chunk` starting from `slots` that runs `run` counting `cost` toward the depth */
    pub(crate) fn new(chunk: ChunkPtr, slots: Vec<Val>, cost: usize, run: FloorRun, dest: u16) -> Self {
        Self { code: None, once: false, chunk, pool: 0, depth: 0, bases: [0; 3], safe: false, ip: 0, rip: 0, top: 0, dest, slots, cost, run }
    }

    /* every value it holds which a collection keeps */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        for &v in &self.slots { f(v); }
        match &self.run {
            FloorRun::Class(c) => for &v in &c.bases { f(v) },
        }
    }
}

impl<'a> VM<'a> {

    /* whether the running frame is a helper a coroutine waits inside with more under it */
    #[inline(always)]
    pub(crate) fn top_linked(&self) -> bool {
        use crate::vm::act::{GEN, TASK};
        !self.chain.chains.is_empty() && self.acts != 0 && self.head(self.head_at(self.top_pool)).as_int() >> 40 & (GEN | TASK) == TASK
    }

    /* clears the task mark of the ending frame so a helper parks without its coroutine */
    #[cold]
    #[inline(never)]
    pub(crate) fn unlink_top(&mut self) -> Result<(), VmErr> {
        if self.chain_top().is_some() { return Ok(()); }
        let h = self.head_at(self.top_pool);
        self.regs[h] = Val::int(self.regs[h].as_int() & !(crate::vm::act::TASK << 40));
        Ok(())
    }
}
