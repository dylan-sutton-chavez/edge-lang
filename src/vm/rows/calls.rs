use crate::lang::{Args, Cx, KindId, Native, Out, kind_of};
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use super::super::VM;
use super::super::act::NO_DEST;
use crate::vm::slow::attr::AttrLookup;
use crate::vm::calls::Opened;
use crate::vm::frame::Floor;
use super::super::lower::Code;
use crate::code::Ins;
use super::super::sites::Site;
use crate::value::*;
use crate::r#unsafe::{CodePtr, ChunkPtr};

/* whether `op` takes packed spreads among its operands */
#[inline]
pub(crate) fn spreads(op: OpCode, operand: u16, chunk: &SSAChunk) -> bool {
    match op {
        OpCode::CallSpread | OpCode::CallExtern => true,
        OpCode::CallNative => crate::syntax::chunk::native_site(operand, &chunk.native_sites).is_some_and(|s| s.spread),
        _ => false,
    }
}

// the most positionals a native method takes straight off its rows
const FEW: usize = 4;

// the most positionals a function or class over rows opens on in place
const OPENS: usize = 8;

impl VM<'_> {
    /* what code over rows returns is its top row or None with no rows */
    #[inline]
    pub(crate) fn rows_value(&self, code: &Code) -> Val {
        let (s0, top) = (code.rows as usize, self.top as usize);
        if top > s0 { self.regs[self.base + top - 1] } else { Val::none() }
    }

    /* opens plain function `fi` on the `k` rows under `top` returning into the callee's row */
    #[inline(never)]
    pub(crate) fn enter_rows(&mut self, i: usize, top: usize, k: usize, fi: usize, callee: Val, code: &Code) -> Option<(CodePtr, ChunkPtr)> {
        let (mut args, at) = ([Val::undef(); 5], self.base + top);
        args[..k].copy_from_slice(&self.regs[at - k..at]);
        // the frame keeps the callee's row as the top its value lands on
        self.top = (top - k - 1) as u32;
        let Some(opened) = self.enter_block(i, code, fi, callee, &args[..k], false) else { self.top = top as u32; return None };
        // unlowered code learns what it calls so a lowering after lays it
        code.cache.set_site(i, Site::Call { callee, fi: fi as u32 });
        self.top = opened.0.get().rows as u32;
        Some(opened)
    }

    /* ends the running frame over rows on a plain return of `v` as `leave_site` does */
    #[inline(never)]
    pub(crate) fn leave_rows(&mut self, v: Val, floor: &Floor) -> Option<(CodePtr, ChunkPtr, usize)> {
        self.leave_site(v, floor.code, floor.chunk, floor.pool, floor.depth)
    }

    // a call over the top rows opens in place or runs through `call_op`
    #[inline(never)]
    pub(crate) fn call_over(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let (n, k) = ((ins.a & 0xFF) as usize, (ins.a & 0xFF) as usize + 2 * (ins.a >> 8) as usize);
        if ins.a >> 8 == 0 && n <= OPENS && let Some(next) = self.open_rows(self.top as usize - k - 1, n, i, code) { return next; }
        self.call_op(ins, i, code, chunk)
    }

    // opens the function or class in row `below` in place or gives none
    fn open_rows(&mut self, below: usize, n: usize, i: usize, code: &Code) -> Option<Option<usize>> {
        let at = self.base + below;
        let callee = self.regs[at];
        if !(matches!(self.heap.try_get(callee), Some(HeapObj::Func { .. })) || self.heap.is_class(callee)) { return None; }
        let mut args = [Val::undef(); OPENS];
        args[..n].copy_from_slice(&self.regs[at + 1..at + 1 + n]);
        self.pending.call_ip = Some(code.orig(i));
        match self.open_stacked(callee, &args[..n], below) {
            Ok(Opened::No) => None,
            Err(e) => { self.reg_error = Some(e); Some(None) }
            Ok(Opened::Frame) => {
                self.learn_call(i, code, callee);
                Some(Some(i + 1))
            }
            // a class with no init leaves its instance in the place of the callee
            Ok(Opened::Made(v)) => {
                self.push(v);
                Some(Some(i + 1))
            }
        }
    }

    // opens `callee` on `args` as the stack form does with the rows cut to `below`
    #[inline(never)]
    fn open_stacked(&mut self, callee: Val, args: &[Val], below: usize) -> Result<Opened, VmErr> { self.open_in_loop(callee, args, Some(below)) }

    // runs call or maker `i` over rows and gives its next index or none
    #[inline(never)]
    pub(crate) fn calls_rows(&mut self, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> { self.calls_at(code.ins[i], i, code, chunk) }

    /* `calls_rows` of `ins` over the rows up to the top */
    #[inline(always)]
    pub(crate) fn calls_at(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        match ins.op {
            // a spread packs into its own operand so the frame it opens keeps nothing
            OpCode::BeginArgs => Some(i + 1),
            OpCode::MakeFunction | OpCode::MakeCoroutine => match self.make_rows(ins, i, code, chunk) {
                Ok(()) => Some(i + 1),
                Err(e) => { self.reg_error = Some(e); None }
            },
            OpCode::CallMethod => self.method_rows(ins, i, code, chunk),
            OpCode::CallExtern | OpCode::CallSpread => {
                self.spread_for(code, ins, i, chunk);
                self.stacked(ins, i, code, chunk)
            }
            // a spread packs its items in place and a lazy source steps as frames
            OpCode::UnpackArgs => {
                let at = self.base + self.top as usize - 1;
                if ins.a & 3 == 1 && matches!(self.heap.try_get(self.regs[at]), Some(HeapObj::Iter(..) | HeapObj::Coroutine(_))) { return self.stacked(ins, i, code, chunk); }
                match self.packed_args(ins.a, self.regs[at]) {
                    Ok(v) => { self.regs[at] = v; Some(i + 1) }
                    Err(e) => { self.reg_error = Some(e); None }
                }
            }
            OpCode::MakeClass => self.stacked(ins, i, code, chunk),
            // a method call's data word only ever runs past its call
            _ => { self.reg_error = Some(cold_runtime("CallMethodArgs reached dispatch unpaired")); None }
        }
    }

    /* the pack a spread of `operand`'s kind makes of `v` or its error */
    pub(crate) fn packed_args(&mut self, operand: u16, v: Val) -> Result<Val, VmErr> {
        if operand & 3 == 2 {
            let flat: alloc::vec::Vec<Val> = self.mapping_to_kw_pairs(v)?.into_iter().flat_map(|(k, v)| [k, v]).collect();
            return self.heap.alloc(HeapObj::Tuple(flat));
        }
        let items = self.extract_iter(v)?;
        self.heap.alloc(HeapObj::Tuple(items))
    }

    /* notes the packed operands of spread call `ins` from its word or its chunk's depths */
    #[inline(never)]
    pub(crate) fn spread_for(&mut self, code: &Code, ins: Ins, i: usize, chunk: &SSAChunk) {
        self.pending.spread = if !code.unchanged() { (ins.x, ins.b as u32 | (ins.c as u32) << 16) } else {
            // the compiler's code finds them walking back or past a join by its depths
            let at = |k: usize| { let w = code.ins[k]; (w.op, if w.op == OpCode::CallMethod && w.x == 1 { w.b } else { w.a }) };
            super::super::depth::spread_back(at, i, chunk).unwrap_or_else(|| {
                let src: alloc::vec::Vec<crate::syntax::chunk::Instruction> = code.ins.iter().map(|w| crate::syntax::chunk::Instruction { opcode: w.op, operand: w.a }).collect();
                super::super::depth::depths(&src, chunk).ok().and_then(|d| super::super::depth::spread_packs(&src, &d, i, chunk)).unwrap_or((0, 0))
            })
        };
    }

    // makes a function from the defaults atop the rows into the row of the first
    fn make_rows(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let n = chunk.functions.get(ins.a as usize).map_or(0, |f| f.2 as usize);
        let at = self.top as usize - n;
        self.pending.call_ip = Some(code.orig(i));
        self.make_function(ins.op, ins.a, Some((at as u16, at as u16)), chunk)?;
        self.top = at as u32 + 1;
        Ok(())
    }

    // a method call over rows with known methods and natives taken in place
    #[inline(always)]
    pub(crate) fn method_rows(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let (pos, kw) = ((ins.b & 0xFF) as usize, (ins.b >> 8) as usize);
        let k = pos + 2 * kw;
        let (top, next) = (self.top as usize, i + 1 + usize::from(ins.x == 1));
        let at = self.base + top - k;
        let (obj, site) = (self.regs[at - 1], code.cache.site(i));
        // a method its site knows for an instance opens in place on the receiver
        if kw == 0 && pos < OPENS && let Some(name) = chunk.names.get(ins.a as usize) && let Some(func) = self.site_method(site, obj, name)
            && let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(func) && self.opens_in_loop(fi, pos + 1) {
            // the receiver and arguments open the method as the stack form passes them
            let mut args = [Val::undef(); OPENS];
            args[..pos + 1].copy_from_slice(&self.regs[at - 1..at + pos]);
            self.pending.call_ip = Some(code.orig(i));
            match self.open_stacked(func, &args[..pos + 1], top - k - 1) {
                Err(e) => { self.reg_error = Some(e); return None; }
                Ok(Opened::Frame) => return Some(next),
                Ok(Opened::Made(v)) => { self.push(v); return Some(next); }
                Ok(Opened::No) => {}
            }
        }
        if kw == 0 && pos <= FEW && let Some(n) = self.method_native(i, ins.a, obj, site, code, chunk) && n.sig.fits(pos) {
            let mut few = [Val::undef(); FEW];
            few[..pos].copy_from_slice(&self.regs[at..at + pos]);
            self.job_ok = false;
            return match (n.run)(&mut Cx::new(self), Args { recv: obj, pos: &few[..pos], kw: &[], named: &[] }) {
                Ok(Out::Val(v)) => {
                    self.regs[at - 1] = v;
                    self.top = (top - k) as u32;
                    self.pay_work().map(|()| next).map_err(|e| self.reg_error = Some(e)).ok()
                }
                Ok(out) => self.method_stepped(out, top - k - 1, next),
                Err(e) => { self.reg_error = Some(e); None }
            };
        }
        self.stacked(ins, i, code, chunk)
    }

    // the native a method call makes on `obj` kept in its site
    #[inline(always)]
    fn method_native(&mut self, i: usize, name: u16, obj: Val, site: Site, code: &Code, chunk: &SSAChunk) -> Option<&'static Native> {
        if let Some((n, _)) = self.site_native(site, obj) { return Some(n); }
        let kind = kind_of(&self.heap, obj);
        // instances classes and modules find their methods as attributes so the stack form looks
        if !matches!(site, Site::Empty) || matches!(kind, KindId::Instance | KindId::Class | KindId::Module) { return None; }
        let name = chunk.names.get(name as usize)?;
        // these kinds meet nothing before their natives and others look up as the stack form
        let (n, owner) = if matches!(kind, KindId::None | KindId::Bool | KindId::Int | KindId::Float | KindId::Str | KindId::Bytes | KindId::ByteArray | KindId::Tuple) {
            self.kind_method(kind, name)?
        } else {
            let Ok(AttrLookup::Native(recv, n, owner)) = self.resolve_attr(obj, name) else { return None };
            if recv.0 != obj.0 { return None; }
            (n, owner)
        };
        code.cache.set_site(i, self.learn_native(obj, n, owner));
        self.pending.call_ip = Some(code.orig(i));
        Some(n)
    }

    // runs the step a native method gave with its value due at row `below`
    #[cold]
    #[inline(never)]
    fn method_stepped(&mut self, out: Out, below: usize, next: usize) -> Option<usize> {
        self.top = below as u32;
        if let Err(e) = self.out_pushed(out, true) { self.reg_error = Some(e); return None; }
        Some(next)
    }

    // runs call or maker `ins` over the rows as its stack form does
    fn stacked(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let ran = match ins.op {
            OpCode::Call => return self.call_op(ins, i, code, chunk),
            // an unlowered pair steps over its second half
            OpCode::CallMethod => return self.exec_call_method(ins.a, ins.b, i, code, chunk).map(|()| i + 1 + usize::from(ins.x == 1)).map_err(|e| self.reg_error = Some(e)).ok(),
            OpCode::MakeClass => self.exec_make_class(ins.a, code.orig(i) as usize + 1, chunk, None, NO_DEST),
            // a generator spread runs as frames its items the op takes as it runs again
            OpCode::UnpackArgs if ins.a & 3 == 1 && self.top_may_gen() => self.retry_gen().and_then(|ran| if ran { Ok(()) } else { self.unpack_args(ins.a, Some(i)) }),
            OpCode::UnpackArgs => self.unpack_args(ins.a, Some(i)),
            // a function the items are spread into runs as a frame of this loop
            OpCode::CallSpread => {
                (self.pending.call_ip, self.pending_exec_safe) = (Some(code.orig(i)), self.frame_safe);
                let ran = self.handle_function(ins.op, ins.a, chunk);
                self.pending_exec_safe = false;
                ran
            }
            // an extern may run its callback as a frame of this loop
            _ => {
                (self.pending.call_ip, self.pending_exec_safe, self.job_ok) = (Some(code.orig(i)), false, true);
                let ran = self.handle_function(ins.op, ins.a, chunk);
                (self.pending_exec_safe, self.job_ok) = (false, false);
                ran
            }
        };
        ran.map(|()| i + 1).map_err(|e| self.reg_error = Some(e)).ok()
    }
}
