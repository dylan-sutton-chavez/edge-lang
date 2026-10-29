use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use super::super::VM;
use super::super::lower::Code;
use crate::code::Ins;
use crate::value::*;

impl VM<'_> {
    /* container op `i` run over rows past its arm's inline path or false on error */
    #[inline(never)]
    pub(crate) fn held_rows(&mut self, i: usize, code: &Code, chunk: &SSAChunk) -> bool { self.held_at(code.ins[i], i, code, chunk) }

    /* `held_rows` of `ins` over the rows to the top */
    #[inline(always)]
    pub(crate) fn held_at(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> bool {
        let ran = match self.held_op(ins, i, code, chunk) {
            // the native work an op counted is paid before the next one runs
            Ok(()) if self.heap.has_work() => self.pay_work(),
            ran => ran,
        };
        ran.map_err(|e| self.reg_error = Some(e)).is_ok()
    }

    /* `held_rows` of `ins` whose value replaces the operands it takes off the top rows */
    fn held_op(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (op, top) = (ins.a, self.top as usize);
        let at = self.base + top;
        let (k, v) = match ins.op {
            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildString | OpCode::BuildSet | OpCode::BuildDict | OpCode::BuildSlice => {
                let k = op as usize * if ins.op == OpCode::BuildDict { 2 } else { 1 };
                let items = self.regs[at - k..at].to_vec();
                (k, Some(self.build_vals(ins.op, items)?))
            }
            OpCode::FormatValue => {
                let k = 1 + (op & 1) as usize;
                let spec = (k == 2).then(|| self.regs[at - 1]);
                (k, Some(self.format_val(op, self.regs[at - k], spec)?))
            }
            // the accumulator under the operands stays in its row
            OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd => return self.handle_comprehension(ins.op),
            OpCode::GetItem => return self.get_item(),
            // a slice taking a generator or a lazy iterator gathers it as frames first
            OpCode::StoreItem if self.may_job(self.regs[at - 1]) && self.heap.slice_of(self.regs[at - 2]).is_some() => return self.held_aside(ins, i),
            OpCode::StoreItem => return self.store_item(),
            OpCode::DelItem => { self.del_item(self.regs[at - 2], self.regs[at - 1])?; (2, None) }
            // a slice read or delete passes its bounds with no slice built
            OpCode::Cut | OpCode::DelCut => {
                let row = self.cut_row(chunk, top as u16 - op, op)?;
                (op as usize, self.cut_op(ins.op, row, Val::undef(), i)?)
            }
            OpCode::LoadAttr => {
                let obj = self.regs[at - 1];
                let name = chunk.names.get(op as usize).ok_or(VmErr::Runtime("LoadAttr: bad name index"))?;
                let v = self.attr_found(obj, name)?;
                let v = if v.is_undef() { self.attr_ruled(obj, name, Some((&code.cache, i)))? } else {
                    if !self.yielded { code.cache.set_site(i, self.learn_get(obj, name)); }
                    v
                };
                (1, Some(v))
            }
            OpCode::StoreAttr => { self.store_attr_at(self.regs[at - 2], op, self.regs[at - 1], i, &code.cache, chunk)?; (2, None) }
            OpCode::DelAttr => { self.del_attr_at(self.regs[at - 1], op, chunk, Some((&code.cache, i)))?; (1, None) }
            OpCode::MatchSeq | OpCode::MatchMap => (1, Some(self.matches_shape(ins.op, self.regs[at - 1]))),
            OpCode::MatchClass => (3, Some(self.match_class(self.regs[at - 3], self.regs[at - 2], self.regs[at - 1], op as usize)?)),
            OpCode::ListExtend | OpCode::SetUpdate | OpCode::DictUpdate | OpCode::UnpackSequence | OpCode::UnpackEx => return self.held_aside(ins, i),
            _ => return Err(cold_runtime("not a container op over rows")),
        };
        let top = top - k;
        match v {
            Some(v) => { self.regs[self.base + top] = v; self.top = (top + 1) as u32; }
            None => self.top = top as u32,
        }
        Ok(())
    }

    /* native work an op counted paid now as the loop's step would pay it */
    #[inline]
    fn paid(&mut self) -> bool {
        if self.heap.has_work() && let Err(e) = self.pay_work() { self.reg_error = Some(e); return false; }
        true
    }

    /* runs `ins` over the rows since its lazy operands step as frames */
    #[cold]
    #[inline(never)]
    fn held_aside(&mut self, ins: Ins, i: usize) -> Result<(), VmErr> {
        match ins.op {
            OpCode::SetUpdate | OpCode::ListExtend if self.top_may_gen() && self.retry_gen()? => Ok(()),
            OpCode::ListExtend | OpCode::SetUpdate | OpCode::DictUpdate => self.handle_spread_merge(ins.op, i),
            OpCode::StoreItem if self.slice_job(i)? => Ok(()),
            OpCode::StoreItem => self.store_item(),
            op => self.handle_container(op, ins.a, i),
        }
    }
}

impl<'a> VM<'a> {
    /* subscript `i` its register arm missed done in place or false for the handler */
    #[inline(never)]
    pub(crate) fn item_missed<'c>(&mut self, i: usize, code: &Code, chunk: &'c SSAChunk) -> bool where 'a: 'c {
        let ins = code.ins[i];
        if self.item_fast(ins) { return self.paid(); }
        // a replay or a value frames gathered runs in the handler
        if self.redo.keep || !self.retake.is_undef() { return false; }
        let chunk = self.chunk_at(code, chunk, i);
        let done = if ins.op == OpCode::GetItemR {
            self.reg_value(ins, i, chunk, &code.cache).map(|v| if !v.is_undef() { self.regs[self.base + ins.a as usize] = v; })
        } else {
            let at = self.base;
            let (o, k, v) = (self.regs[at + ins.a as usize], self.regs[at + ins.b as usize], self.regs[at + ins.c as usize]);
            // an unbound operand or a slice taking a lazy value leaves for the handler
            if o.is_undef() || k.is_undef() || v.is_undef() || self.may_job(v) && self.heap.slice_of(k).is_some() { return false; }
            self.item_store(o, k, v)
        };
        match done {
            Ok(()) => self.paid(),
            Err(e) => { self.reg_error = Some(e); false }
        }
    }

    /* register field `i` its arm left formatted in place by its stack form's handler */
    #[inline(never)]
    pub(crate) fn form_missed<'c>(&mut self, i: usize, code: &Code, chunk: &'c SSAChunk) -> bool where 'a: 'c {
        let (ins, chunk) = (code.ins[i], self.chunk_at(code, chunk, i));
        match self.form_reg(ins, chunk) {
            Ok(()) => self.paid(),
            Err(e) => { self.reg_error = Some(e); false }
        }
    }

    /* attribute read `i` its site missed made in place and learned as its handler learns */
    #[inline(never)]
    pub(crate) fn attr_missed<'c>(&mut self, i: usize, code: &Code, chunk: &'c SSAChunk) -> bool where 'a: 'c {
        let (ins, chunk) = (code.ins[i], self.chunk_at(code, chunk, i));
        let o = self.regs[self.base + ins.b as usize];
        // an unbound receiver or a bad name leaves for the handler which raises
        let Some(name) = chunk.names.get(ins.c as usize).filter(|_| !o.is_undef()) else { return false };
        let v = match self.attr_found(o, name) {
            Ok(v) if v.is_undef() => match self.attr_ruled(o, name, Some((&code.cache, i))) { Ok(v) => v, Err(e) => { self.reg_error = Some(e); return false; } },
            Ok(v) => { code.cache.set_site(i, self.learn_loaded(o, name, v)); v }
            Err(e) => { self.reg_error = Some(e); return false; }
        };
        self.regs[self.base + ins.a as usize] = v;
        self.paid()
    }
}
