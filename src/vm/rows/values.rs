use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::syntax::ssa::ssa_strip;
use super::super::VM;
use super::super::lower::Code;
use crate::code::Ins;
use super::super::scope::Kind;
use crate::value::*;

impl VM<'_> {
    /* runs value op `i` over the rows and gives false with its error kept */
    #[inline(never)]
    pub(crate) fn value_rows(&mut self, i: usize, code: &Code, chunk: &SSAChunk) -> bool {
        let ran = match self.value_op(code.ins[i], i, code, chunk) {
            // the native work an op counted is paid before the next one runs
            Ok(()) if self.heap.has_work() => self.pay_work(),
            ran => ran,
        };
        ran.map_err(|e| self.reg_error = Some(e)).is_ok()
    }

    /* `value_rows` of op `ins` moving the top as its stack form moves the stack */
    fn value_op(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let top = self.top as usize;
        let at = self.base + top;
        match ins.op {
            OpCode::LoadName | OpCode::LoadGlobal | OpCode::LoadEllipsis | OpCode::LoadModule => {
                if top >= code.frame { return Err(cold_runtime("a stack deeper than the rows of its frame")); }
                let v = match ins.op {
                    OpCode::LoadName => self.name_value(ins.a as usize, code, chunk)?,
                    OpCode::LoadGlobal => self.global_value(ins.a as usize, code, chunk)?,
                    op => self.load_value(op, ins.a, chunk)?,
                };
                self.regs[at] = v;
                self.top += 1;
            }
            OpCode::StoreName => {
                self.top -= 1;
                self.store_name(ins.a as usize, self.regs[at - 1], code);
            }
            OpCode::StoreGlobal => {
                self.top -= 1;
                let name = chunk.names.get(ins.a as usize).ok_or_else(|| cold_runtime("StoreGlobal: name index out of bounds"))?;
                self.note_builtin_binding(name);
                self.scopes[code.module].set(name, self.regs[at - 1]);
            }
            OpCode::Del => self.del_name(ins.a as usize, code, chunk)?,
            // the phi tables index the compiler's instructions
            OpCode::Phi => self.phi_rows(ins.a as usize, code.orig(i) as usize, code, chunk),
            OpCode::Assert => {
                self.top -= 1;
                self.assert_holds(self.regs[at - 1])?;
            }
            OpCode::Not => self.regs[at - 1] = Val::bool(!self.truthy(self.regs[at - 1])),
            OpCode::Minus => self.regs[at - 1] = match self.minus_value(self.regs[at - 1]) { Ok(v) => v, Err(e) => self.op_rule(e, i, 1, chunk, &code.cache)? },
            OpCode::Pos | OpCode::BitNot => self.regs[at - 1] = match self.unary_op(ins.op, self.regs[at - 1]) { Ok(v) => v, Err(e) => self.op_rule(e, i, 1, chunk, &code.cache)? },
            // a pattern's length as the engine counts it and a starred display's tuple
            OpCode::Len => { let n = self.builtin_len(self.regs[at - 1])?; self.regs[at - 1] = self.heap.int(n)?; }
            OpCode::ListToTuple => self.regs[at - 1] = self.list_tuple(self.regs[at - 1])?,
            // a handler's test of the exception under the class it names
            OpCode::MatchExc => {
                let hit = self.match_exc(self.regs[at - 2], self.regs[at - 1])?;
                self.regs[at - 2] = Val::bool(hit);
                self.top -= 1;
            }
            OpCode::RuntimeOp => return self.own_rows(ins, i, code, chunk),
            // a test over a lazy container or a list's `+=` from one may step frames
            OpCode::In | OpCode::NotIn if self.may_job(self.regs[at - 1]) => return self.value_aside(ins),
            OpCode::InPlaceAdd if self.lazy_extend(self.regs[at - 2], self.regs[at - 1]) => return self.value_aside(ins),
            op => {
                let v = match self.binary_value(op, ins.a, self.regs[at - 2], self.regs[at - 1]) { Ok(v) => v, Err(e) => self.op_rule(e, i, 2, chunk, &code.cache)? };
                self.regs[at - 2] = v;
                self.top -= 1;
            }
        }
        Ok(())
    }

    /* what LoadName of slot `k` reads or the error an unbound name raises */
    fn name_value(&mut self, k: usize, code: &Code, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let names = code.rows as usize;
        match code.kinds.get(k) {
            Some(&Kind::Global(g)) => self.global_at(code.module, g),
            Some(Kind::Cell) => {
                let v = self.deref(if k < names { self.regs[self.base + k] } else { Val::undef() });
                if v.is_undef() { Err(self.unbound_err(chunk, k)) } else { Ok(v) }
            }
            _ => match self.regs[self.base..self.base + names].get(k).copied().filter(|v| !v.is_undef()) {
                Some(v) => Ok(v),
                None => self.unbound(chunk, k as u16),
            },
        }
    }

    /* what LoadGlobal of name `k` reads from its module binding or its builtin */
    fn global_value(&self, k: usize, code: &Code, chunk: &SSAChunk) -> Result<Val, VmErr> {
        if let Some(&Kind::Global(g)) = code.kinds.get(k) { return self.global_at(code.module, g); }
        let name = chunk.names.get(k).ok_or_else(|| cold_runtime("LoadGlobal: name index out of bounds"))?;
        self.scopes[code.module].get(name).or_else(|| self.global(name)).ok_or_else(|| crate::lang::EngineError::Undefined(name.clone()).into())
    }

    /* binds slot `k` to `v` rebinding a builtin's name and filling a cell */
    fn store_name(&mut self, k: usize, v: Val, code: &Code) {
        match code.kinds.get(k) {
            Some(&Kind::Global(g)) => {
                self.scopes[code.module].set_at(g, v);
                if let Some(id) = self.scopes[code.module].names_native(g) { self.rebind_native(id); }
            }
            Some(Kind::Cell) => if !self.set_cell(self.regs[self.base + k], v) { self.regs[self.base + k] = v; },
            // a slot past the names is one only malformed code writes
            _ => if k < code.rows as usize { self.regs[self.base + k] = v; },
        }
    }

    /* unbinds slot `k` and an unbound one raises as its read would */
    fn del_name(&mut self, k: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        match code.kinds.get(k) {
            Some(&Kind::Global(g)) => {
                let name = ssa_strip(&chunk.names[k]);
                if self.scopes[code.module].at(g).is_undef() { return Err(crate::lang::EngineError::Undefined(name.into()).into()); }
                self.scopes[code.module].set_at(g, Val::undef());
                // a builtin's name falls back to it
                self.note_builtin_binding(name);
            }
            Some(Kind::Cell) => {
                let cell = self.regs[self.base + k];
                if self.deref(cell).is_undef() { return Err(self.unbound_err(chunk, k)); }
                self.set_cell(cell, Val::undef());
            }
            _ => match self.regs[self.base..self.base + code.rows as usize].get_mut(k) {
                Some(s) if !s.is_undef() => *s = Val::undef(),
                _ => return Err(self.unbound_err(chunk, k)),
            },
        }
        Ok(())
    }

    /* binds slot `k` to the first bound source of the phi at compiler instruction `rip` */
    fn phi_rows(&mut self, k: usize, rip: usize, code: &Code, chunk: &SSAChunk) {
        let Some(&(ia, ib)) = chunk.phi_map.get(rip).and_then(|&pi| chunk.phi_sources.get(pi)) else { return };
        let names = &mut self.regs[self.base..self.base + code.rows as usize];
        let read = |names: &[Val], s: u16| names.get(s as usize).copied().unwrap_or(Val::undef());
        let v = match read(names, ia) { a if a.is_undef() => read(names, ib), a => a };
        if let Some(slot) = names.get_mut(k) { *slot = v; }
    }

    /* runs value op `ins` over the rows since a job may step its lazy operand */
    #[cold]
    #[inline(never)]
    fn value_aside(&mut self, ins: Ins) -> Result<(), VmErr> {
        if ins.op == OpCode::InPlaceAdd { self.exec_arith_or_compare(ins.op, ins.a) } else { self.handle_identity(ins.op) }
    }
}
