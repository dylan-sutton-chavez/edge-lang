use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::syntax::parser::ITER_VALUE;
use super::super::VM;
use crate::vm::exc::handler_at;
use super::super::jobs::Job;
use super::super::lower::Code;
use crate::value::*;
use crate::r#unsafe::{CodePtr, ChunkPtr};

// the code a yield carries on in or the index the loop leaves at
type Went = Result<(CodePtr, ChunkPtr, usize), usize>;

// whether a step of `f` opens no frame and leaves no value on the stack
fn plain(f: Option<&IterFrame>) -> bool {
    matches!(f, Some(IterFrame::Seq { .. } | IterFrame::List { .. } | IterFrame::Range { .. } | IterFrame::BigRange { .. }
        | IterFrame::Watched { .. } | IterFrame::DictWalk { .. } | IterFrame::ListRev { .. } | IterFrame::Octets { .. } | IterFrame::Chars { .. }))
}

// whether a try over rows holds no row
pub(crate) fn tried(s0: usize, sp: usize, top: u32) -> bool { sp == 0 || sp == usize::MAX && top as usize == s0 }

impl VM<'_> {
    // ends a step run apart paying its native work as the loop does
    #[inline]
    fn stepped<T>(&mut self, ran: Result<T, VmErr>) -> Result<T, VmErr> {
        if self.heap.has_work() && let Err(e) = self.pay_work() { return Err(e); }
        ran
    }

    // a test past a plain truth or the budget run as its stack form tests
    #[cold]
    #[inline(never)]
    pub(crate) fn test_rows(&mut self, op: OpCode, to: usize, n: usize) -> Result<bool, VmErr> {
        let t = self.truthy(self.regs[self.base + self.top as usize - 1]);
        let jump = t == (op == OpCode::JumpIfTrueOrPop);
        if op == OpCode::JumpIfFalse || !jump { self.top -= 1; }
        // only the plain jump checks its target and pays a step for it
        let ran = if jump && op == OpCode::JumpIfFalse { self.checked_jump(to, n).map(|_| jump) } else if jump && to > n { Err(cold_runtime("jump target out of bounds")) } else { Ok(jump) };
        self.stepped(ran)
    }

    // the iterator a loop or a comprehension takes from the top row
    #[inline(never)]
    pub(crate) fn iter_rows(&mut self, op: u16) -> bool {
        let o = self.regs[self.base + self.top as usize - 1];
        let ran = if op == ITER_VALUE {
            self.iter_value(o).map(|it| { let at = self.base + self.top as usize - 1; self.regs[at] = it; })
        } else {
            self.get_iter(o).map(|()| self.top -= 1)
        };
        self.stepped(ran).map_err(|e| self.reg_error = Some(e)).is_ok()
    }

    // a loop step past the inline one going on at its index or leaving there
    #[inline(never)]
    pub(crate) fn for_rows(&mut self, i: usize, code: &Code, n: usize) -> Result<usize, usize> {
        let to = code.ins[i].a as usize;
        // dict and text walks and a ready builtin iterator step here while budget lasts
        if self.budget > 0 && !self.heap.needs_gc() {
            let item = match self.iter_stack.last_mut() {
                Some(f @ (IterFrame::DictWalk { .. } | IterFrame::Chars { .. })) => f.next_item(&mut self.heap).transpose(),
                Some(&mut IterFrame::UserDefined(it)) => self.ready_next(it),
                _ => None,
            };
            match item {
                Some(Ok(v)) => { self.budget -= 1; self.push(v); return Ok(i + 1); }
                Some(Err(e)) => { self.budget -= 1; self.reg_error = Some(e); return Err(i); }
                None => {}
            }
        }
        if !plain(self.iter_stack.last()) { return self.for_aside(i, code, n); }
        // a plain iterator's end and a step past the budget take the stack form's step
        let ran = match self.for_step() {
            Ok(Some(v)) => { self.push(v); Ok(i + 1) }
            Ok(None) if to > n => Err(cold_runtime("jump target out of bounds")),
            Ok(None) => Ok(to),
            Err(e) => Err(e),
        };
        self.stepped(ran).map_err(|e| { self.reg_error = Some(e); i })
    }

    // a loop step that may open frames runs as its stack form over the rows
    #[cold]
    #[inline(never)]
    fn for_aside(&mut self, i: usize, code: &Code, n: usize) -> Result<usize, usize> {
        let mut ip = i + 1;
        let ran = self.for_stack(i, code, n, &mut ip);
        // a frame it opened takes the loop's turn so a generator resumes where it yielded
        if self.entered { (self.entered, self.thrown) = (false, true); }
        let ran = if self.yielded { ran } else { self.stepped(ran) };
        match ran {
            Err(e) => { self.reg_error = Some(e); Err(i) }
            Ok(()) if self.yielded => Err(ip),
            Ok(()) => Ok(ip),
        }
    }

    // the step of the `for` at `i` in its stack form over the rows
    fn for_stack(&mut self, i: usize, code: &Code, n: usize, ip: &mut usize) -> Result<(), VmErr> {
        let co = match self.iter_stack.last() { Some(&IterFrame::Coroutine(co)) => Some(co), _ => None };
        match co.and_then(|co| self.enter_gen(co, i)) {
            Some((_, _, at)) => (self.gen_entry, self.entered, self.yielded) = (at, true, true),
            None if self.for_job(i, None).is_some() => (self.entered, self.yielded) = (true, true),
            None if self.lazy_over() && self.lazy_prefetch(i, code)? => {}
            None => self.exec_for_iter(code.ins[i].a, ip, n)?,
        }
        Ok(())
    }

    // a finally that fell through ends at once and other reasons wait for the loop
    #[inline(never)]
    pub(crate) fn end_rows(&mut self, i: usize, code: &Code) -> bool {
        if code.ins[i].op == OpCode::EndFinally && matches!(self.unwind_stack.last(), None | Some(Unwind::Normal)) { self.unwind_stack.pop(); return true; }
        false
    }

    // a raise run as its stack form raises with the error kept for the loop
    #[cold]
    #[inline(never)]
    pub(crate) fn raise_rows(&mut self, i: usize, code: &Code, chunk: &SSAChunk) {
        let e = self.raise_stacked(code.ins[i].a, i, chunk);
        self.reg_error = Some(if self.heap.has_work() && let Err(paid) = self.pay_work() { paid } else { e });
    }

    // a yield over rows hands its value on and parks its generator or runs on
    #[inline(never)]
    pub(crate) fn yield_rows(&mut self, i: usize, code: &Code, (to, at, pool, depth): (CodePtr, ChunkPtr, usize, usize)) -> Went {
        let top = self.top as usize;
        let v = self.regs[self.base + top - 1];
        let pops = code.ins.get(i + 1).is_some_and(|n| n.op == OpCode::PopTop);
        let (resume, framed) = (i + 1 + pops as usize, self.acts > depth);
        // a builtin running the generator takes the value in place while it can
        if framed && self.top_gather() && self.gen_yields() {
            if !v.is_undef() && self.gather_fast(v) {
                self.placed(top, pops);
                return Ok((CodePtr::NONE, ChunkPtr::NONE, resume));
            }
            if !v.is_undef() && matches!(self.jobs.last(), Some(Job::NextGen(..))) && self.budget != 0 {
                self.placed(top, pops);
                return self.gather_leave(v, (code.resume(resume), self.kept_top(code)), to, at, pool, depth).ok_or(resume);
            }
        } else if framed && self.gen_yields_to_for() {
            self.placed(top, pops);
            return self.yield_site(v, (code.resume(resume), self.kept_top(code)), to, at, pool, depth).ok_or(resume);
        }
        // any other yield takes the loop's own turn with its rows left in its window
        self.yielded = true;
        Err(i + 1)
    }

    // the value a yield over rows hands out its row taking the value sent
    #[inline(never)]
    pub(crate) fn rows_yielded(&mut self, drops: bool) -> Val {
        let top = self.top as usize;
        let v = self.regs[self.base + top - 1];
        self.placed(top, drops);
        v
    }

    // the top a generator over rows parks with and none while it holds no row
    #[inline]
    fn kept_top(&self, code: &Code) -> u16 { if self.top > u32::from(code.rows) { self.top as u16 } else { 0 } }

    // a yield's rows with its value taken and the sent value due in its place
    #[inline]
    fn placed(&mut self, top: usize, pops: bool) {
        if pops { self.top -= 1; } else { self.regs[self.base + top - 1] = Val::none(); }
    }

    // the awaitable an await steps in the top row with its first sent value above
    #[inline(never)]
    pub(crate) fn awaitable_rows(&mut self, code: &Code) -> bool {
        let top = self.top as usize;
        if top >= code.frame { self.reg_error = Some(cold_runtime("a stack deeper than the rows of its frame")); return false; }
        let ran = self.awaitable_of(self.regs[self.base + top - 1]).map(|it| {
            let at = self.base + top;
            (self.regs[at - 1], self.regs[at]) = (it, Val::none());
            self.top += 1;
        });
        self.stepped(ran).map_err(|e| self.reg_error = Some(e)).is_ok()
    }

    // the value a delegation returned pushed onto the rows
    #[inline(never)]
    pub(crate) fn delegated_rows(&mut self, code: &Code) -> bool {
        let top = self.top as usize;
        if top >= code.frame { self.reg_error = Some(cold_runtime("a stack deeper than the rows of its frame")); return false; }
        self.regs[self.base + top] = self.yield_from_value;
        self.top += 1;
        true
    }

    // a return the loop left over rows running the finally bodies around it first
    #[inline(never)]
    pub(crate) fn rows_return(&mut self, code: &Code, exc_base: usize, ip: &mut usize) -> Result<Option<Val>, VmErr> {
        let v = self.rows_value(code);
        if self.exception_stack.len() > exc_base && let Some(h) = self.next_cleanup_handler(exc_base) {
            // a try begins on an empty stack so its finally starts at the first row
            (self.top, *ip) = (code.rows as u32, handler_at(code, h));
            self.unwind_stack.push(Unwind::Return(v));
            return Ok(None);
        }
        Ok(Some(v))
    }
}
