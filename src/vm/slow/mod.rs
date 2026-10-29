use alloc::vec::Vec;
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use crate::syntax::ssa::ssa_strip;
use super::{VM, lower};
use crate::value::*;
use super::sites::Site;
use super::lower::{Code, STORE_NOTES_BUILTIN};
use crate::code::Ins;
use super::scope::Kind;
use crate::vm::calls::Opened;
use super::registers::numeric_binop;
use crate::vm::frame::{Floor, chunk_in};
use crate::vm::exc::RAW_HANDLER;
use crate::vm::sections::{section_at, stub_head};

pub(crate) mod arith;
pub(crate) mod attr;
pub(crate) mod item;
pub(crate) mod build;
pub(crate) mod iter;
pub(crate) mod rules;
impl<'a> VM<'a> {

    /* rare register ops run in place giving false for the slow path */
    #[inline(never)]
    pub(crate) fn reg_apart(&mut self, ins: Ins) -> bool {
        let at = self.base;
        match ins.op {
            OpCode::PowR => match numeric_binop(OpCode::Pow, self.regs[at + ins.b as usize], self.regs[at + ins.c as usize]) { Some(v) => self.regs[at + ins.a as usize] = v, None => return false },
            OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR => return self.accumulate(ins, self.regs[at + ins.b as usize], self.regs[at + ins.c as usize]),
            OpCode::UnpackR => return self.unpack_fast(ins),
            OpCode::UnbindR => self.regs[at + ins.a as usize] = Val::undef(),
            OpCode::WrapCellR => { let Ok(v) = self.heap.alloc(HeapObj::Cell(self.regs[at + ins.a as usize])) else { return false }; self.regs[at + ins.a as usize] = v; }
            // another module's binding read or written by a section laid from that module
            OpCode::LoadGlobalAtR => {
                let Some(scope) = self.scopes.get(ins.c as usize) else { return false };
                let v = match scope.at(ins.b as u32) { v if v.is_undef() => scope.fallback(ins.b as u32), v => v };
                if v.is_undef() { return false; }
                self.regs[at + ins.a as usize] = v;
            }
            OpCode::StoreGlobalAtR if ins.x == 0 => {
                let v = self.regs[at + ins.b as usize];
                if v.is_undef() { return false; }
                let Some(scope) = self.scopes.get_mut(ins.c as usize) else { return false };
                scope.set_at(ins.a as u32, v);
            }
            // a handler's test asks the runtime's matcher and leaves its error to the slow path
            OpCode::MatchExcR => { let Ok(hit) = self.match_exc(self.regs[at + ins.b as usize], self.regs[at + ins.c as usize]) else { return false }; self.regs[at + ins.a as usize] = Val::bool(hit); }
            OpCode::ListToTupleR => { let Ok(t) = self.list_tuple(self.regs[at + ins.b as usize]) else { return false }; self.regs[at + ins.a as usize] = t; }
            _ => return false,
        }
        true
    }

    /* runs a GetIterR or OpR in the loop or false for the handler */
    #[inline(never)]
    pub(crate) fn op_in_loop(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> bool {
        // an unbound source or a class or an adopted generator takes the handler
        let done = match ins.op {
            OpCode::GetIterR if ins.x != lower::ADOPT && !self.regs[self.base + ins.b as usize].is_undef() => self.reg_iter(ins),
            OpCode::OpR if ins.x != OpCode::MakeClass as u8 => self.exec_op(ins, i, chunk_in(&self.functions, code, chunk, i), code),
            _ => return false,
        };
        done.map_err(|e| self.reg_error = Some(e)).is_ok()
    }

    /* a build or format the loop left done by the handler of its stack form */
    #[inline(never)]
    pub(crate) fn form_reg(&mut self, ins: Ins, chunk: &SSAChunk) -> Result<(), VmErr> {
        let v = if ins.op == OpCode::BuildR {
            let items = (ins.b..ins.b + ins.c).map(|r| self.reg(chunk, r)).collect::<Result<Vec<Val>, VmErr>>()?;
            self.build_vals(OpCode::ALL[(ins.x & !lower::TAKEN) as usize], items)?
        } else {
            let v = self.reg(chunk, ins.b)?;
            let spec = if ins.x & 1 != 0 { Some(self.reg(chunk, ins.c)?) } else { None };
            self.format_val(ins.x as u16, v, spec)?
        };
        self.regs[self.base + ins.a as usize] = v;
        Ok(())
    }

    // runs the OpR `ins` over its row writing from `a` once its calls are done
    #[inline(never)]
    fn exec_op(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, code: &Code) -> Result<(), VmErr> {
        let (op, b) = (OpCode::ALL[ins.x as usize], ins.b);
        let v = match op {
            OpCode::MakeFunction | OpCode::MakeCoroutine => {
                // a function made inside a section takes its captures where the section lays their slots
                self.cell_map = section_at(code, rip).map(|s| s.slots.clone()).filter(|m| !m.is_empty());
                return self.make_function(op, ins.c, Some((ins.a, ins.b)), chunk);
            }
            OpCode::MakeClass => return self.exec_make_class(ins.c, code.orig(rip) as usize + 1, chunk, Some(ins.b), ins.a),
            OpCode::LoadModule | OpCode::LoadYieldFrom | OpCode::LoadEllipsis => self.load_value(op, ins.c, chunk)?,
            OpCode::MatchSeq | OpCode::MatchMap => { let v = self.reg(chunk, b)?; self.matches_shape(op, v) }
            OpCode::MatchClass => {
                let (s, c, k) = (self.reg(chunk, b)?, self.reg(chunk, b + 1)?, self.reg(chunk, b + 2)?);
                self.match_class(s, c, k, ins.c as usize)?
            }
            OpCode::DelItem => { let (o, k) = (self.reg(chunk, b)?, self.reg(chunk, b + 1)?); return self.del_item(o, k); }
            OpCode::Cut | OpCode::DelCut | OpCode::StoreCut => {
                // what frames gathered of a lazy source comes back as a store reruns
                let retaken = if op == OpCode::StoreCut { core::mem::replace(&mut self.retake, Val::undef()) } else { Val::undef() };
                let row = self.cut_row(chunk, b, ins.c)?;
                match self.cut_op(op, row, retaken, rip)? { Some(v) => v, None => return Ok(()) }
            }
            OpCode::DelAttr => { let o = self.reg(chunk, b)?; return self.del_attr_at(o, ins.c, chunk, Some((&code.cache, rip))); }
            OpCode::Assert => { let v = self.reg(chunk, b)?; return self.assert_holds(v); }
            OpCode::UnpackEx => return self.unpack_reg(ins, rip, chunk, (ins.c >> 8) as usize, Some((ins.c & 0xFF) as usize)),
            _ => return Err(cold_runtime("not an OpR opcode")),
        };
        self.regs[self.base + ins.a as usize] = v;
        Ok(())
    }

    /* what LoadModule LoadYieldFrom or LoadEllipsis gives */
    #[inline(always)]
    pub(crate) fn load_value(&mut self, op: OpCode, operand: u16, chunk: &SSAChunk) -> Result<Val, VmErr> {
        match op {
            OpCode::LoadModule => {
                let entry = chunk.imports.get(operand as usize).ok_or_else(|| cold_runtime("LoadModule: import index out of range"))?;
                self.module_table.get(&entry.spec).copied().ok_or_else(|| cold_runtime("LoadModule: module not initialised"))
            }
            OpCode::LoadYieldFrom => Ok(self.yield_from_value),
            _ => self.heap.ellipsis(),
        }
    }

    /* runs op `rip` its arm left giving the next index or None for its driver */
    #[inline(never)]
    pub(crate) fn op_apart(&mut self, rip: usize, code: &Code, chunk: &SSAChunk, floor: &Floor) -> Option<usize> {
        let ins = code.ins[rip];
        let ran = match ins.op {
            // the driver takes a return a finally's end and an await as its turns
            OpCode::ReturnR | OpCode::ReturnValue | OpCode::EndFinally | OpCode::UnwindFinally | OpCode::Await => return None,
            OpCode::Jump => self.jump_apart(ins, rip, code, floor),
            // a section call the loop left runs its stub unless a body function enters it
            OpCode::CallSectionR | OpCode::CallGlobalSectionR | OpCode::CallMethodSectionR | OpCode::CallValueSectionR | OpCode::CallCtorSectionR | OpCode::CallMethodGlobalSectionR => Ok(self.section_turned(code, rip)),
            // a section's op indexes names in the chunk of the function it was laid from
            op if op.is_register() || op == OpCode::Yield && ins.x != 0 => { let chunk = self.chunk_at(code, chunk, rip); self.reg_slow(ins, rip, chunk, code) }
            _ => Err(cold_runtime("an op the register loop has no arm for")),
        };
        // an op pays for the native work it did before the next one runs
        match ran.and_then(|next| if self.heap.has_work() { self.pay_work().map(|()| next) } else { Ok(next) }) {
            Ok(next) => Some(next),
            Err(e) => { self.reg_error = Some(e); None }
        }
    }

    /* a jump left on a low budget with back edges taking their safe point */
    #[cold]
    #[inline(never)]
    fn jump_apart(&mut self, ins: Ins, rip: usize, code: &Code, floor: &Floor) -> Result<usize, VmErr> {
        let target = self.checked_jump(ins.a as usize, code.ins.len())?;
        // backward jumps are loop back-edges charged so `while` is bounded like `for`
        if target <= rip && ins.x == 0 { self.back_edge(code, floor, target)?; }
        Ok(target)
    }

    /* register forms whose fast path missed giving where the loop goes on */
    #[inline(never)]
    fn reg_slow(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, code: &Code) -> Result<usize, VmErr> {
        let (n, cache, mut ip) = (code.ins.len(), &code.cache, rip + 1);
        let op = ins.a;
        match ins.op {
            // jumps to or from a head laid past grown code's end are not program steps
            OpCode::JumpR => ip = ins.a as usize,
            // forms with generated arms leave here as the hand written ones beside them do
            crate::code::forms!(arith) | crate::code::forms!(order) | OpCode::PowR | OpCode::EqR | OpCode::NotEqR => self.reg_binop(ins, rip, chunk, cache)?,
            crate::code::forms!(test) | OpCode::JumpUnlessEq | OpCode::JumpUnlessNotEq => if !self.reg_test(ins, rip, chunk, cache)? { ip = self.checked_jump(op as usize, n)?; },
            OpCode::JumpIfFalseR => if self.reg_truthy(chunk, ins.b)? == (ins.x != 0) { ip = self.checked_jump(op as usize, n)?; },
            // a dict subscript the register loop left skips the generic protocol
            OpCode::GetItemR | OpCode::StoreItemR if self.item_fast(ins) => {}
            OpCode::BuildR | OpCode::FormatR => self.form_reg(ins, chunk)?,
            OpCode::LenR | OpCode::MinusR | OpCode::NotR | OpCode::GetItemR | OpCode::InR | OpCode::NotInR | OpCode::IsR | OpCode::IsNotR | crate::code::forms!(bits) => {
                // a length keeps its native in `c` where the others keep a register
                let (b, c) = (self.regs[self.base + ins.b as usize], if ins.op == OpCode::LenR { Val::undef() } else { self.regs[self.base + ins.c as usize] });
                if matches!(ins.op, OpCode::BitAndR | OpCode::BitOrR | OpCode::BitXorR | OpCode::ShrR) && let Some(v) = self.wide_fast(super::lower::stack_form(ins.op), b, c)? {
                    self.regs[self.base + ins.a as usize] = v;
                    return Ok(ip);
                }
                if ins.op == OpCode::LenR { self.pending.call_ip = Some(code.orig(rip)); }
                let v = self.reg_value(ins, rip, chunk, cache)?;
                // a job the op opened writes `a` once it ends
                if !v.is_undef() { self.regs[self.base + ins.a as usize] = v; }
            }
            OpCode::UnpackR => self.unpack_reg(ins, rip, chunk, if ins.x != 0 { 2 } else { ins.c as usize }, None)?,
            OpCode::UnaryR => {
                let v = self.reg(chunk, ins.b)?;
                let r = self.unary_op(if ins.x == OpCode::Pos as u8 { OpCode::Pos } else { OpCode::BitNot }, v);
                self.regs[self.base + ins.a as usize] = match r { Ok(v) => v, Err(e) => self.op_rule(e, rip, 0, chunk, cache)? };
            }
            OpCode::RaiseR => return Err(self.raise_reg(ins, rip, chunk)),
            OpCode::RuntimeR => self.own_reg(ins, rip, chunk, cache)?,
            OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR => {
                let v = self.reg(chunk, ins.c)?;
                let k = if ins.op == OpCode::MapAddR { self.reg(chunk, ins.b)? } else { Val::undef() };
                // an accumulator a register holds or the one atop the stack
                let acc = if ins.x == 1 { self.regs[self.base + ins.a as usize] } else { self.slot(self.under(1)?) };
                let form = match ins.op { OpCode::ListAppendR => OpCode::ListAppend, OpCode::SetAddR => OpCode::SetAdd, _ => OpCode::MapAdd };
                self.accumulated(form, acc, k, v)?;
            }
            crate::code::forms!(move) => self.regs[self.base + ins.a as usize] = self.reg(chunk, ins.b)?,
            OpCode::CallNativeR => {
                let wide = lower::wide(ins);
                let rest = if wide { code.ins[rip + 1] } else { ins };
                let n = (ins.x & lower::ARGC) as usize;
                // a read raises for an unbound local and names it
                for &r in &[ins.b, ins.c, rest.a, rest.b, rest.c][..n] { self.reg(chunk, r)?; }
                match self.native_regs(rip, code, chunk) {
                    Some(next) => ip = next,
                    None => return Err(self.reg_error.take().unwrap_or_else(super::steps::no_native)),
                }
            }
            OpCode::MatchExcR => {
                let (e, c) = (self.reg(chunk, ins.b)?, self.reg(chunk, ins.c)?);
                let hit = self.match_exc(e, c)?;
                self.regs[self.base + ins.a as usize] = Val::bool(hit);
            }
            OpCode::ListToTupleR => {
                let l = self.reg(chunk, ins.b)?;
                let t = self.list_tuple(l)?;
                self.regs[self.base + ins.a as usize] = t;
            }
            OpCode::CallR | OpCode::CallMethodR | OpCode::CallGlobalR => {
                let wide = lower::wide(ins);
                let (regs, count) = lower::call_regs(ins, if wide { code.ins[rip + 1] } else { ins });
                // a global callee is read here so its error names the binding as `LoadGlobalR` would
                let first = if ins.op == OpCode::CallGlobalR { self.global_at(code.module, ins.a as u32)?; 1 } else { 0 };
                // a read raises for an unbound local and names it
                for &r in &regs[first..count] { self.reg(chunk, r)?; }
                match self.call_reg(ins, rip, code, chunk) {
                    Some(next) => ip = next,
                    None => return Err(self.reg_error.take().unwrap_or_else(|| cold_runtime("a register call left no next step"))),
                }
            }
            // a call of `f` with `seq` spread as an unpacked argument list
            OpCode::CallSpreadR => {
                let (f, seq) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                // a spread stub asks for a head laid for `f` which its table then finds
                if code.block.is_some() && self.budget != 0 && !self.redo.keep && matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })) && let Some(h) = stub_head(code, rip)
                    && code.ins[h + 2].x & 72 == 72 && self.asks_growth(code, chunk, h, lower::Grow::Value, f) { return Ok(rip); }
                if let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(f) { cache.set_site(rip, Site::Call { callee: f, fi: fi as u32 }); }
                // a short tuple or list becomes the positionals of a function opening in the loop
                if ins.x != 2 && let Some(args) = self.few_items(seq) {
                    let n = args.1;
                    self.pending.call_ip = Some(code.orig(rip));
                    match self.open_in_loop(f, &args.0[..n], None)? {
                        Opened::Frame => return Ok(ip),
                        Opened::Made(v) => return Ok(self.deliver_at(code, rip + 1, v)),
                        Opened::No => {}
                    }
                    self.pending.call_ip = None;
                }
                // the one operand spreads as positionals or a mapping's pairs as keywords
                let (pos, kw) = if ins.x == 2 {
                    let pairs = self.mapping_to_kw_pairs(seq)?;
                    (Vec::new(), pairs.into_iter().flat_map(|(k, v)| [k, v]).collect::<Vec<Val>>())
                } else {
                    let items = match self.heap.try_get(seq) { Some(HeapObj::Tuple(t)) => t.clone(), _ => self.extract_iter(seq)? };
                    (items, Vec::new())
                };
                (self.pending.call_ip, self.pending_exec_safe, self.inline_call) = (Some(code.orig(rip)), self.frame_safe, self.frame_safe);
                let called = self.delivering(Self::take_dest(code, rip + 1), |vm| vm.call_args(f, &pos, &kw, chunk));
                self.pending_exec_safe = false;
                if let Some(v) = called? { ip = self.deliver_at(code, rip + 1, v); }
            }
            // read by the call before it and never run alone
            OpCode::ArgsR | OpCode::CallValueR => {}
            // a level of recursion through sections raising past the frames' share of memory
            OpCode::TurnR => if !self.turn(&code.ins[ins.a as usize..ins.a as usize + ins.b as usize]) { return Err(cold_depth()); },
            OpCode::UnturnR => ip = rip + self.unturn(&code.ins[rip..]),
            // a section's return the loop gave way on raising for an unbound value
            OpCode::ReturnSectionR | OpCode::ReturnTriedR | OpCode::ReturnBoundR => {
                let (link, v) = (self.regs[self.base + ins.a as usize].as_int() as usize, self.regs[self.base + ins.b as usize]);
                if v.is_undef() { return Err(crate::lang::EngineError::Unbound { name: None, free: false }.into()); }
                // a `finally` of the section runs first and its end returns
                if ins.x != 0 && let Some(s) = section_at(code, rip) && let Some(h) = self.section_cleanup(s) {
                    self.unwind_stack.push(Unwind::Return(v));
                    return Ok(h & !RAW_HANDLER);
                }
                self.regs[self.base + (link & 0xFFFF)] = v;
                ip = link >> 16;
            }
            // after an ordinary call replaced a section call what was pending under it is popped
            OpCode::SectionR => if ins.x != lower::STUB && let under = code.ins[rip - 1].c && under > 0 {
                let v = self.pop()?;
                if ins.x & 2 == 0 { self.regs[self.base + under as usize - 1] = v; }
            },
            OpCode::AppendR => {
                let at = self.base + ins.a as usize;
                let (sv, tv) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                if sv.0 == self.owned_str.0 && self.owned_at == at && self.heap.append_str(sv, tv) { return Ok(ip); }
                self.reg_binop(Ins { op: OpCode::InPlaceAddR, x: 0, a: ins.a, b: ins.a, c: ins.b }, rip, chunk, cache)?;
                // the add's fresh string is this register's alone so later appends grow it in place
                let made = self.regs[at];
                let strs = matches!(self.heap.try_get(sv), Some(HeapObj::Str(_))) && matches!(self.heap.try_get(tv), Some(HeapObj::Str(_)));
                if strs && made.0 != sv.0 && made.0 != tv.0 && self.heap.growable_str(made) { (self.owned_str, self.owned_at) = (made, at); }
            }
            OpCode::OwnResetR => if self.owned_at == self.base + ins.a as usize { self.owned_str = Val::undef(); },
            OpCode::ListPushR => {
                let (l, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                let Some(HeapObj::List(rc)) = self.heap.try_get(l) else { return Err(cold_type("append: receiver is not a list")) };
                rc.borrow_mut().push(v);
            }
            OpCode::StoreItemR => {
                // what frames gathered of a lazy source comes back as the op reruns
                let retaken = core::mem::replace(&mut self.retake, Val::undef());
                let (o, k) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                let v = if retaken.is_undef() { self.reg(chunk, ins.c)? } else { retaken };
                let v = if self.may_job(v) && self.heap.slice_of(k).is_some() && self.steps_user(v) {
                    match self.retake_job(v, rip)? { Some(v) => v, None => return Ok(ip) }
                } else { v };
                self.item_store(o, k, v)?;
            }
            OpCode::GetAttrR => {
                let o = self.reg(chunk, ins.b)?;
                let Some(name) = chunk.names.get(ins.c as usize) else { return Err(VmErr::Runtime("LoadAttr: bad name index")) };
                // what the engine finds is learned and what a rule gives keeps that rule
                let v = self.attr_found(o, name)?;
                let v = if v.is_undef() { self.attr_ruled(o, name, Some((cache, rip)))? } else { cache.set_site(rip, self.learn_loaded(o, name, v)); v };
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::SetAttrR => {
                let (o, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                self.store_attr_at(o, ins.c, v, rip, cache, chunk)?;
            }
            OpCode::LoadGlobalR => self.regs[self.base + ins.a as usize] = self.global_at(code.module, ins.b as u32)?,
            OpCode::LoadGlobalAtR => self.regs[self.base + ins.a as usize] = self.global_at(ins.c as usize, ins.b as u32)?,
            OpCode::UnbindR => self.regs[self.base + ins.a as usize] = Val::undef(),
            OpCode::WrapCellR => { let v = self.heap.alloc(HeapObj::Cell(self.regs[self.base + ins.a as usize]))?; self.regs[self.base + ins.a as usize] = v; }
            OpCode::ExcArgsR => {
                let at = self.base + ins.b as usize;
                let args = self.regs[at..at + ins.c as usize].to_vec();
                self.set_exc_args(self.regs[self.base + ins.a as usize], args)?;
            }
            OpCode::StoreGlobalAtR => {
                let (v, m) = (self.reg(chunk, ins.b)?, ins.c as usize);
                self.scopes[m].set_at(ins.a as u32, v);
                if ins.x & STORE_NOTES_BUILTIN != 0 && let Some(id) = self.scopes[m].names_native(ins.a as u32) { self.rebind_native(id); }
            }
            // a take pops the row a wait or call left leaving lowered code no top
            OpCode::StoreTopR if self.top != 0 => {
                let v = self.pop()?;
                self.top = 0;
                if ins.x == 0 { self.regs[self.base + ins.a as usize] = v; } else { self.scopes[code.module].set_at(ins.a as u32, v); }
            }
            // a value a job handed its register at once leaves nothing to take
            OpCode::StoreTopR => {}
            OpCode::StoreGlobalR => {
                let v = self.reg(chunk, ins.b)?;
                self.scopes[code.module].set_at(ins.a as u32, v);
                if ins.x & STORE_NOTES_BUILTIN != 0 && let Some(id) = self.scopes[code.module].names_native(ins.a as u32) { self.rebind_native(id); }
            }
            OpCode::LoadCellR => {
                let v = self.deref(self.regs[self.base + ins.b as usize]);
                if v.is_undef() { return Err(self.unbound_err(chunk, ins.b as usize)); }
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::StoreCellR => {
                let v = self.reg(chunk, ins.b)?;
                if !self.set_cell(self.regs[self.base + ins.a as usize], v) { self.regs[self.base + ins.a as usize] = v; }
            }
            OpCode::GetIterR => {
                let o = self.reg(chunk, ins.b)?;
                match ins.x {
                    lower::ADOPT => match self.take_gen(code, o, rip) { Some(at) => ip = at, None => self.get_iter(o)? },
                    _ => self.reg_iter(ins)?,
                }
            }
            OpCode::OpR => self.exec_op(ins, rip, chunk, code)?,
            // a generator this `for` steps opens as a frame the driver enters
            OpCode::ForIterR if let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() && let Some((_, _, at)) = self.enter_gen(co, rip) => (self.gen_entry, self.thrown, self.yielded) = (at, true, true),
            OpCode::ForIterR if self.for_job(rip, None).is_some() => (self.entered, self.yielded) = (true, true),
            // a builtin iterator first runs its generator to an item as a loop frame
            OpCode::ForIterR if self.lazy_over() && self.lazy_prefetch(rip, code)? => {}
            OpCode::ForIterR => match self.for_step()? {
                Some(item) if item.is_undef() => ip = rip,
                Some(item) if ins.x == 0 => self.regs[self.base + ins.b as usize] = item,
                Some(item) => self.scopes[code.module].set_at(ins.b as u32, item),
                None => {
                    if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                    ip = op as usize;
                }
            },
            // a register yield no frame takes hands its register to its driver
            OpCode::Yield => { self.reg(chunk, ins.a)?; self.yielded = true; }
            _ => return Err(cold_runtime("not a register form")),
        }
        Ok(ip)
    }

    // a join or delete or declaration the register loop runs raising for what is unbound
    #[inline(never)]
    pub(crate) fn side_op(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (op, chunk) = (ins.a as usize, chunk_in(&self.functions, code, chunk, i));
        match ins.op {
            // the phi tables index the compiler's instructions
            OpCode::Phi => self.exec_phi(ins.a, code.orig(i) as usize, &chunk.phi_map, &chunk.phi_sources),
            // a section's delete of binding `b` of module `c`
            OpCode::Del if ins.x == 1 => {
                let (g, m) = (ins.b as u32, ins.c as usize);
                let name = ssa_strip(&chunk.names[op]);
                if self.scopes[m].at(g).is_undef() { return Err(crate::lang::EngineError::Undefined(name.into()).into()); }
                self.scopes[m].set_at(g, Val::undef());
                self.note_builtin_binding(name);
            }
            OpCode::Del => match code.kinds.get(op) {
                Some(&Kind::Global(g)) => {
                    let name = ssa_strip(&chunk.names[op]);
                    if self.scopes[code.module].at(g).is_undef() { return Err(crate::lang::EngineError::Undefined(name.into()).into()); }
                    self.scopes[code.module].set_at(g, Val::undef());
                    // a builtin name falls back
                    self.note_builtin_binding(name);
                }
                Some(Kind::Cell) => {
                    let cell = self.regs[self.base + op];
                    if self.deref(cell).is_undef() { return Err(self.unbound_err(chunk, op)); }
                    self.set_cell(cell, Val::undef());
                }
                _ => match self.regs.get_mut(self.base + op) {
                    Some(s) if !s.is_undef() => *s = Val::undef(),
                    _ => return Err(self.unbound_err(chunk, op)),
                },
            },
            // declarations do nothing as they run
            _ => {}
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn checked_jump(&mut self, target: usize, limit: usize) -> Result<usize, VmErr> {
        self.charge_step()?;
        if target > limit { return Err(cold_runtime("jump target out of bounds")); }
        Ok(target)
    }

    /* picks the first defined phi source and leaves the slot unbound when neither is */
    pub(crate) fn exec_phi(&mut self, op: u16, rip: usize, phi_map: &[usize], phi_sources: &[(u16, u16)]) {
        // parse recovery can leave a phi indexing past the frame so indexes are checked
        let Some(&(ia, ib)) = phi_map.get(rip).and_then(|&pi| phi_sources.get(pi)) else { return };
        let a = self.regs.get(self.base + ia as usize).copied().unwrap_or_else(Val::undef);
        let val = if !a.is_undef() { a }
        else { self.regs.get(self.base + ib as usize).copied().unwrap_or_else(Val::undef) };
        if let Some(dst) = self.regs.get_mut(self.base + op as usize) { *dst = val; }
    }
}
