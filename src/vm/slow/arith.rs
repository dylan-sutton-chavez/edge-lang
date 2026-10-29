use crate::vm::VM;
use crate::value::{Val, VmErr, HeapObj, Host, cold_depth, cold_runtime, eq_checked};
use crate::code::OpCode;
use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;
use crate::value::{BinOp, UnOp};
use crate::lang::{Cmp, EngineError, Op};
use crate::value::*;
use crate::vm::lower;
use crate::code::Ins;
use crate::vm::registers::numeric_binop;
use crate::value::host::{coerce_floats, long_pair};

impl<'a> VM<'a> {

    /* the arithmetic ops exact for ints of any size with Minus unary */
    pub(crate) fn handle_arith(&mut self, op: OpCode, operand: u16) -> Result<(), VmErr> {
        if op == OpCode::Minus {
            let v = self.pop()?;
            let r = self.minus_value(v)?;
            self.push(r);
            return Ok(());
        }
        let (a, b) = self.pop2()?;
        // a job that appends the items of a lazy source hands the list back itself
        if let Some(v) = self.arith_value(op, operand, a, b, true)? { self.push(v); }
        Ok(())
    }

    /* whether `a += b` extends list `a` from a source a job may step */
    #[inline]
    pub(crate) fn lazy_extend(&self, a: Val, b: Val) -> bool {
        matches!(self.heap.try_get(a), Some(HeapObj::List(_))) && self.may_job(b)
    }

    /* `a op b` of arithmetic and none when `jobs` lets a job take the source */
    #[inline(always)]
    pub(crate) fn arith_value(&mut self, op: OpCode, operand: u16, a: Val, b: Val, jobs: bool) -> Result<Option<Val>, VmErr> {
        let inplace = matches!(op, OpCode::InPlaceAdd | OpCode::InPlaceSub) || operand == crate::syntax::parser::INPLACE;
        let op = match op {
            OpCode::InPlaceAdd => {
                // a list reads the iterable's items before it grows so `xs += xs` doubles
                if a.is_heap() && matches!(self.heap.get(a), HeapObj::List(_)) {
                    // a generator or an iterator running user code appends as a job of the loop
                    if jobs && self.may_job(b) && self.extend_job(a, b)? { return Ok(None); }
                    let rhs = self.extract_iter(b)?;
                    if let HeapObj::List(la) = self.heap.get(a) { la.borrow_mut().extend_from_slice(&rhs); }
                    return Ok(Some(a));
                }
                // a bytearray grows by any bytes-like value
                if a.is_heap() && matches!(self.heap.get(a), HeapObj::ByteArray(_)) && let Some(more) = self.heap.bytes_of(b).map(<[u8]>::to_vec) {
                    self.heap.reserve(more.len())?;
                    if let Some(HeapObj::ByteArray(buf)) = self.heap.try_get_mut(a) { buf.extend_from_slice(&more); }
                    return Ok(Some(a));
                }
                OpCode::Add
            }
            // a left set loses the right set's items in place so its aliases see it
            OpCode::InPlaceSub if self.is_set_like(a) && self.is_set_like(b) => return self.set_iop(a, b, OpCode::Sub).map(Some),
            OpCode::InPlaceSub => OpCode::Sub,
            op => op,
        };
        let result = match op {
            OpCode::Add => self.add_vals(a, b),
            OpCode::Sub => self.sub_vals(a, b),
            OpCode::Mul => self.mul_vals(a, b),
            OpCode::Div => self.div_vals(a, b),
            OpCode::Mod => self.exec_mod(a, b),
            OpCode::Pow => self.exec_pow(a, b),
            OpCode::FloorDiv => self.exec_floordiv(a, b),
            _ => return Err(cold_runtime("non-arith opcode in handle_arith")),
        };
        if inplace { result.map_err(inplace_err).map(Some) } else { result.map(Some) }
    }

    /* any binary op as its stack form runs it with wide ints first */
    pub(crate) fn binary_value(&mut self, op: OpCode, operand: u16, a: Val, b: Val) -> Result<Val, VmErr> {
        match op {
            OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr
            | OpCode::InPlaceBitOr | OpCode::InPlaceBitAnd | OpCode::InPlaceBitXor => self.bitwise_value(op, a, b),
            OpCode::In | OpCode::NotIn | OpCode::Is | OpCode::IsNot => self.identity_value(op, a, b).map(Val::bool),
            OpCode::MatMul => Err(self.unsupported(BinOp::MatMul, a, b)),
            _ => {
                if let Some(v) = self.wide_fast(op, a, b)? { return Ok(v); }
                if matches!(op, OpCode::Eq | OpCode::Lt | OpCode::NotEq | OpCode::Gt | OpCode::LtEq | OpCode::GtEq) { return self.compare_value(op, a, b).map(Val::bool); }
                // its callers took any lazy source aside so a value always comes back
                self.arith_value(op, operand, a, b, false).map(|v| v.unwrap_or(a))
            }
        }
    }

    /* `-v` of a number exact for ints of any size */
    #[inline(always)]
    pub(crate) fn minus_value(&mut self, v: Val) -> Result<Val, VmErr> {
        self.unary_of(v, |v| Val::float(-v.as_float()), UnOp::Neg)
    }

    // a unary minus or plus of `v` with `ffl` mapping the float case
    fn unary_of(&mut self, v: Val, ffl: fn(Val) -> Val, op: UnOp) -> Result<Val, VmErr> {
        if v.is_float() { return Ok(ffl(v)); }
        if (v.is_int() || v.is_bool()) && let Some(i) = self.as_i128(v) {
            return self.heap.int(if op == UnOp::Neg { -i } else { i });
        }
        match self.heap.foreign_unop(op, v)? { Some(r) => Ok(r), None => Err(self.bad_operand(crate::lang::Op::Unary(op), v)) }
    }

    // unary plus or invert of `v` where a bool becomes an int
    pub(crate) fn unary_op(&mut self, op: OpCode, v: Val) -> Result<Val, VmErr> {
        if op == OpCode::Pos { return self.unary_of(v, |v| v, UnOp::Pos); }
        if (v.is_int() || v.is_bool()) && let Some(i) = self.as_i128(v) { return self.heap.int(!i); }
        match self.heap.foreign_unop(UnOp::Invert, v)? { Some(r) => Ok(r), None => Err(self.bad_operand(crate::lang::Op::Unary(UnOp::Invert), v)) }
    }

    fn exec_mod(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        self.floor_op(BinOp::Mod, a, b)
    }

    fn exec_floordiv(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        self.floor_op(BinOp::FloorDiv, a, b)
    }

    fn exec_pow(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        self.pow_vals(a, b)
    }

    /* a bitwise op where an augmented form changes a left set or dict */
    #[inline(always)]
    pub(crate) fn bitwise_value(&mut self, op: OpCode, a: Val, b: Val) -> Result<Val, VmErr> {
        let inplace = matches!(op, OpCode::InPlaceBitOr | OpCode::InPlaceBitAnd | OpCode::InPlaceBitXor);
        let op = match op {
            OpCode::InPlaceBitOr => OpCode::BitOr,
            OpCode::InPlaceBitAnd => OpCode::BitAnd,
            OpCode::InPlaceBitXor => OpCode::BitXor,
            other => other,
        };
        if self.is_set_like(a) && self.is_set_like(b) && matches!(op, OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor) {
            return if inplace { self.set_iop(a, b, op) } else { self.set_binop(a, b, op) };
        }
        // dict union merges with the right side winning and the in-place form updates the left
        if op == OpCode::BitOr && let (Some(n), Some(m)) = (self.heap.dict_cell(a).map(|d| d.borrow().len()), self.heap.dict_cell(b).map(|d| d.borrow().len())) {
            // the entries a merge reads pay and its copies are refused past the memory left
            let read = if inplace { m } else { n + m };
            if read > 64 { self.charge_steps(read)?; self.heap.reserve(m.saturating_mul(2 * VAL_BYTES).saturating_add(if inplace { 0 } else { n.saturating_mul(MAP_TABLE) }))?; }
            let (Some(x), Some(y)) = (self.heap.dict_cell(a), self.heap.dict_cell(b)) else { return Err(cold_runtime("a merge lost its maps")) };
            let entries: alloc::vec::Vec<(Val, Val)> = y.borrow().iter().collect();
            // colliding keys stop the merge once the budget is spent
            if inplace {
                let x = x.clone();
                for (k, v) in entries { x.borrow_mut().insert(k, v, &self.heap); self.heap.within_budget()?; }
                return Ok(a);
            }
            let mut merged = x.borrow().clone();
            for (k, v) in entries { merged.insert(k, v, &self.heap); self.heap.within_budget()?; }
            return self.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(merged))));
        }
        // two bools keep a bool for these three where bools count as ints
        if a.is_bool() && b.is_bool() && self.heap.policy.bool_is_int && let Some(v) = match op {
            OpCode::BitAnd => Some(a.as_bool() & b.as_bool()), OpCode::BitOr => Some(a.as_bool() | b.as_bool()), OpCode::BitXor => Some(a.as_bool() ^ b.as_bool()), _ => None,
        } {
            return Ok(Val::bool(v));
        }
        match op {
            OpCode::BitAnd => self.int_bitop(BinOp::And, a, b),
            OpCode::BitOr => self.int_bitop(BinOp::Or, a, b),
            OpCode::BitXor => self.int_bitop(BinOp::Xor, a, b),
            OpCode::Shl => self.int_bitop(BinOp::Shl, a, b),
            OpCode::Shr => self.int_bitop(BinOp::Shr, a, b),
            _ => Err(cold_runtime("non-bitwise opcode in bitwise_value")),
        }
    }

    /* int bitwise ops and shifts exact in i128 and through the int kind past it */
    fn int_bitop(&mut self, op: BinOp, a: Val, b: Val) -> Result<Val, VmErr> {
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) {
            let r = match op {
                BinOp::And => Some(x & y),
                BinOp::Or => Some(x | y),
                BinOp::Xor => Some(x ^ y),
                _ if y < 0 => return Err(crate::lang::EngineError::NegativeShift.into()),
                // bits shifted past the top leave i128 which `checked_shl` alone lets through
                BinOp::Shl => if y < 128 { x.checked_shl(y as u32).filter(|r| r >> y == x) } else { (x == 0).then_some(0) },
                _ => Some(x >> y.min(127)),
            };
            if let Some(r) = r { return self.heap.int(r); }
        }
        self.heap.foreign_binop(op, a, b)?.ok_or_else(|| self.unsupported(op, a, b))
    }

    pub(crate) fn handle_compare(&mut self, op: OpCode) -> Result<(), VmErr> {
        let (a, b) = self.pop2()?;
        let r = self.compare_value(op, a, b)?;
        self.push(Val::bool(r));
        Ok(())
    }

    /* whether comparison `a op b` holds with two sets ordered by inclusion */
    #[inline(always)]
    pub(crate) fn compare_value(&mut self, op: OpCode, a: Val, b: Val) -> Result<bool, VmErr> {
        // two sets order by inclusion which no total order gives
        if self.is_set_like(a) && self.is_set_like(b) { return self.set_compare(a, b, op); }
        Ok(match op {
            OpCode::Eq => self.values_eq(a, b)?,
            OpCode::NotEq => !self.values_eq(a, b)?,
            OpCode::Lt => self.lt_vals(a, b).map_err(|e| self.ordering_err(e, Cmp::Lt, a, b))?,
            OpCode::Gt => self.lt_vals(b, a).map_err(|e| self.ordering_err(e, Cmp::Gt, a, b))?,
            OpCode::LtEq => self.at_most(a, b, Cmp::Le)?,
            OpCode::GtEq => self.at_most(b, a, Cmp::Ge)?,
            _ => return Err(cold_runtime("non-compare opcode in handle_compare")),
        })
    }

    /* whether `x` orders before or equals `y` so a pair with no order answers false */
    #[inline]
    fn at_most(&self, x: Val, y: Val, op: Cmp) -> Result<bool, VmErr> {
        let (a, b) = if op == Cmp::Le { (x, y) } else { (y, x) };
        self.pair_at_most(x, y, 0).map_err(|e| self.ordering_err(e, op, a, b))
    }

    /* a pair's `at_most` where two sequences read once to their first differing pair */
    fn pair_at_most(&self, x: Val, y: Val, depth: usize) -> Result<bool, VmErr> {
        // two inline numbers compare as the register forms do where a NaN orders with nothing
        if let Some(v) = numeric_binop(OpCode::LtEq, x, y) { return Ok(v.as_bool()); }
        match (self.heap.try_get(x), self.heap.try_get(y)) {
            (Some(HeapObj::List(p)), Some(HeapObj::List(q))) => self.seq_order_d(&p.borrow(), &q.borrow(), depth + 1, true, |a, b, d| self.pair_at_most(a, b, d)),
            (Some(HeapObj::Tuple(p)), Some(HeapObj::Tuple(q))) => self.seq_order_d(p, q, depth + 1, true, |a, b, d| self.pair_at_most(a, b, d)),
            // a long pair counts the bytes the order may read as native work
            (Some(HeapObj::Str(p)), Some(HeapObj::Str(q))) => { long_pair(&self.heap, p.len(), q.len()); Ok(p <= q) }
            (Some(HeapObj::Bytes(p) | HeapObj::ByteArray(p)), Some(HeapObj::Bytes(q) | HeapObj::ByteArray(q))) => { long_pair(&self.heap, p.len(), q.len()); Ok(p <= q) }
            _ => Ok(self.lt_vals_d(x, y, depth)? || self.values_eq(x, y)?),
        }
    }

    /* an unsupported ordering `a op b` named by the written operator with operands in order */
    #[cold]
    fn ordering_err(&self, e: VmErr, op: Cmp, a: Val, b: Val) -> VmErr {
        match e {
            VmErr::Engine(f) if matches!(*f, EngineError::Unsupported { op: Op::Cmp(Cmp::Lt), .. }) => EngineError::unsupported(Op::Cmp(op), a, Some(b)),
            e => e,
        }
    }

    /* `is` and `is not` compare tag bits while `in` and `not in` ask `contains_op` */
    pub(crate) fn handle_identity(&mut self, op: OpCode) -> Result<(), VmErr> {
        let (a, b) = self.pop2()?;
        let v = self.lazy_identity(op, a, b)?;
        if !v.is_undef() { self.push(v); }
        Ok(())
    }

    /* identity or membership over a lazy container giving undef once its job opened */
    pub(crate) fn lazy_identity(&mut self, op: OpCode, a: Val, b: Val) -> Result<Val, VmErr> {
        // a tested generator runs as a loop frame until an equal item or its end
        if matches!(op, OpCode::In | OpCode::NotIn) && matches!(self.heap.try_get(b), Some(HeapObj::Coroutine(..)))
            && self.with_roots([a, b], |vm| vm.gather_job(b, crate::vm::jobs::Takes::Contains(a, op == OpCode::NotIn), Vec::new()))? { return Ok(Val::undef()); }
        // a `map` or `filter` whose steps run user code steps as a job too
        if matches!(op, OpCode::In | OpCode::NotIn) && matches!(self.heap.try_get(b), Some(HeapObj::Iter(..)))
            && self.with_roots([a], |vm| vm.contains_job(b, crate::vm::jobs::Takes::Contains(a, op == OpCode::NotIn)))? { return Ok(Val::undef()); }
        self.identity_value(op, a, b).map(Val::bool)
    }

    /* whether `a op b` holds for identity or membership stepping a lazy container here */
    #[inline(always)]
    pub(crate) fn identity_value(&mut self, op: OpCode, a: Val, b: Val) -> Result<bool, VmErr> {
        Ok(match op {
            OpCode::In => self.contains_op(b, a)?,
            OpCode::NotIn => !self.contains_op(b, a)?,
            OpCode::Is => a.0 == b.0,
            OpCode::IsNot => a.0 != b.0,
            _ => return Err(cold_runtime("non-identity opcode in handle_identity")),
        })
    }
}

/* an operator error of `a op= b` naming the augmented operator */
#[cold]
fn inplace_err(e: VmErr) -> VmErr {
    match e {
        VmErr::Engine(mut f) => {
            if let EngineError::Unsupported { op, .. } = &mut *f && let Op::Bin(b) = *op { *op = Op::InPlace(b); }
            VmErr::Engine(f)
        }
        e => e,
    }
}

impl<'a> VM<'a> {

    /* `b op c` the handler answers for strings or mixed or wide numbers else none */
    #[inline(never)]
    pub(crate) fn binop_apart(&mut self, ins: Ins) -> Option<Val> {
        let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
        let op = lower::stack_form(ins.op);
        if b.is_heap() { return if matches!(op, OpCode::Add | OpCode::InPlaceAdd | OpCode::Mul) { self.joined(op == OpCode::Mul, b, c) } else { None }; }
        if let Some(v) = numeric_binop(op, b, c) { return Some(v); }
        // inline ints past their range take a wide int as the handler makes
        if b.is_int() && c.is_int() {
            let (x, y) = (b.as_int() as i128, c.as_int() as i128);
            let r = match op { OpCode::Add | OpCode::InPlaceAdd => x + y, OpCode::Sub | OpCode::InPlaceSub => x - y, OpCode::Mul => x * y, _ => return None };
            return self.heap.int(r).ok();
        }
        self.wide_fast(op, b, c).ok().flatten()
    }

    /* two strings joined or a string repeated an int times and none otherwise */
    #[inline(never)]
    pub(crate) fn joined(&mut self, repeat: bool, x: Val, y: Val) -> Option<Val> {
        if !repeat { return self.concat(x, y); }
        if !matches!(self.heap.try_get(x), Some(HeapObj::Str(_))) || !y.is_int() { return None; }
        self.mul_vals(x, y).ok()
    }

    /* two texts end to end paying for the copy as any join does */
    #[inline(always)]
    fn concat(&mut self, x: Val, y: Val) -> Option<Val> {
        let (Some(HeapObj::Str(a)), Some(HeapObj::Str(b))) = (self.heap.try_get(x), self.heap.try_get(y)) else { return None };
        let n = a.len() + b.len();
        // a long text past the memory left goes to the handler which refuses it
        if n > 1 << 16 && self.heap.reserve(n).is_err() { return None; }
        self.charge_steps(n).ok()?;
        let (Some(HeapObj::Str(a)), Some(HeapObj::Str(b))) = (self.heap.try_get(x), self.heap.try_get(y)) else { return None };
        let mut r = String::with_capacity(n);
        r.push_str(a);
        r.push_str(b);
        self.heap.alloc(HeapObj::Str(r)).ok()
    }

    #[inline(never)]
    pub(crate) fn exec_arith_or_compare(&mut self, opcode: OpCode, operand: u16) -> Result<(), VmErr> {
        if let Ok(at) = self.under(2) && let Some(v) = self.wide_fast(opcode, self.slot(at), self.slot(at + 1))? {
            self.cut_to(at);
            self.push(v);
            return Ok(());
        }
        if matches!(opcode, OpCode::Eq | OpCode::Lt | OpCode::NotEq | OpCode::Gt | OpCode::LtEq | OpCode::GtEq) { self.handle_compare(opcode) } else { self.handle_arith(opcode, operand) }
    }

    /* fast `a op b` of two i128 ints one boxed or None to fall back */
    #[inline]
    pub(crate) fn wide_fast(&mut self, op: OpCode, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        if !(a.is_foreign() || b.is_foreign()) { return Ok(None); }
        let int = |v: Val| if v.is_int() { Some(v.as_int() as i128) } else { self.heap.foreign.wide_value(v) };
        let (Some(x), Some(y)) = (int(a), int(b)) else { return Ok(None) };
        let r = match op {
            OpCode::Add | OpCode::InPlaceAdd => x.checked_add(y),
            OpCode::Sub | OpCode::InPlaceSub => x.checked_sub(y),
            OpCode::Mul => x.checked_mul(y),
            OpCode::FloorDiv if y != 0 => self.natives.policy.int_divmod(x, y).map(|(q, _)| q),
            OpCode::Mod if y != 0 => self.natives.policy.int_divmod(x, y).map(|(_, r)| r),
            OpCode::BitAnd => Some(x & y),
            OpCode::BitOr => Some(x | y),
            OpCode::BitXor => Some(x ^ y),
            OpCode::Shr if y >= 0 => Some(x >> y.min(127)),
            OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::LtEq | OpCode::Gt | OpCode::GtEq => {
                return Ok(Some(Val::bool(match op { OpCode::Eq => x == y, OpCode::NotEq => x != y, OpCode::Lt => x < y, OpCode::LtEq => x <= y, OpCode::Gt => x > y, _ => x >= y })));
            }
            _ => None,
        };
        r.map(|r| self.heap.int(r)).transpose()
    }

    /* `a == b` by content where a pair past the depth cap raises */
    #[inline]
    pub(crate) fn values_eq(&self, a: Val, b: Val) -> Result<bool, VmErr> {
        eq_checked(a, b, &self.heap, 0).ok_or_else(cold_depth)
    }

    /* member equality as `x is e or x == e` inlined into per-item walks */
    #[inline]
    pub(crate) fn member_eq(&self, a: Val, b: Val) -> Result<bool, VmErr> {
        if a.0 == b.0 { return Ok(true); }
        self.values_eq(a, b)
    }

    /* the item counts of two sets */
    fn set_sizes(&self, a: Val, b: Val) -> Result<(usize, usize), VmErr> {
        let size = |v: Val| match self.heap.try_get(v) { Some(HeapObj::Set(s)) => Some(s.borrow().len()), _ => None };
        let (Some(n), Some(m)) = (size(a), size(b)) else { return Err(cold_runtime("a set operation on non-set operands")) };
        Ok((n, m))
    }

    /* true for `set` operands */
    pub(crate) fn is_set_like(&self, v: Val) -> bool {
        v.is_heap() && matches!(self.heap.get(v), HeapObj::Set(_))
    }

    // set op items by content membership so the alloc dedups equal values of distinct handles
    fn set_binop_items(&mut self, a: Val, b: Val, op: OpCode) -> Result<Vec<Val>, VmErr> {
        let (n, m) = self.set_sizes(a, b)?;
        // an operator pays for the items it reads and its build refuses a long result
        let read = match op { OpCode::BitXor => n + m, OpCode::BitAnd => n.min(m), _ => n };
        if read > 64 { self.charge_steps(read)?; }
        let (Some(HeapObj::Set(x)), Some(HeapObj::Set(y))) = (self.heap.try_get(a), self.heap.try_get(b)) else { return Err(cold_runtime("a set operation on non-set operands")) };
        let (sa, sb) = (x.borrow(), y.borrow());
        let heap = &self.heap;
        let mut out = Vec::new();
        // the items of `from` that `other` holds or lacks as `keep` asks
        let mut pick = |from: &ValSet, other: &ValSet, keep: bool| -> Result<(), VmErr> {
            for (h, v) in from.iter_hashed() {
                if other.slot(h, heap, |k| eq_member(k, v, heap)).is_some() == keep { out.push(v); }
                heap.within_budget()?;
            }
            Ok(())
        };
        match op {
            // the smaller right side finds the left items it shares which keep their left order
            OpCode::BitAnd if m < n => {
                let mut found = Vec::new();
                for (h, v) in sb.iter_hashed() {
                    if let Some(i) = sa.slot(h, heap, |k| eq_member(k, v, heap)) { found.push(i); }
                    heap.within_budget()?;
                }
                found.sort_unstable();
                out.extend(found.into_iter().map(|i| sa.item_at(i)));
            }
            OpCode::BitAnd => pick(&sa, &sb, true)?,
            OpCode::BitXor => { pick(&sa, &sb, false)?; pick(&sb, &sa, false)?; }
            OpCode::Sub => pick(&sa, &sb, false)?,
            _ => return Err(cold_runtime("set_binop with unsupported opcode")),
        }
        Ok(out)
    }

    /* the left set copied with room for the right one whose items go in after */
    fn set_union(&mut self, a: Val, b: Val) -> Result<ValSet, VmErr> {
        let (n, m) = self.set_sizes(a, b)?;
        // both sides pay for their items and the copy is refused past the memory left
        if n + m > 64 { self.charge_steps(n + m)?; self.heap.reserve((n + m).saturating_mul(SET_TABLE))?; }
        let (Some(HeapObj::Set(x)), Some(HeapObj::Set(y))) = (self.heap.try_get(a), self.heap.try_get(b)) else { return Err(cold_runtime("a set operation on non-set operands")) };
        let (mut s, more): (ValSet, Vec<Val>) = (x.borrow().copy_with_room(m), y.borrow().iter().copied().collect());
        s.put_all(&more, &self.heap)?;
        Ok(s)
    }

    // the fresh set a set op of two sets makes
    pub(crate) fn set_binop(&mut self, a: Val, b: Val, op: OpCode) -> Result<Val, VmErr> {
        let s = if op == OpCode::BitOr { self.set_union(a, b)? } else { ValSet::build(&self.set_binop_items(a, b, op)?, &self.heap)? };
        self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))
    }

    // augmented set ops change the left set in place so its aliases see the change
    pub(crate) fn set_iop(&mut self, a: Val, b: Val, op: OpCode) -> Result<Val, VmErr> {
        let (n, m) = self.set_sizes(a, b)?;
        // an intersection walks the left side and a smaller left side serves a difference too
        let left = op == OpCode::BitAnd || op == OpCode::Sub && n < m;
        let read = if left { n } else { m };
        if read > 64 { self.charge_steps(read)?; }
        let (Some(HeapObj::Set(x)), Some(HeapObj::Set(y))) = (self.heap.try_get(a), self.heap.try_get(b)) else { return Err(cold_runtime("a set operation on non-set operands")) };
        let heap = &self.heap;
        // a set taking itself out or toggling itself empties and keeps itself otherwise
        if a.0 == b.0 { if matches!(op, OpCode::Sub | OpCode::BitXor) { x.borrow_mut().clear(); } return Ok(a); }
        // the items to put in take out or toggle are read first holding no borrow
        let items: Vec<Val> = if left {
            let (sa, sb) = (x.borrow(), y.borrow());
            let mut gone = Vec::new();
            for (h, v) in sa.iter_hashed() {
                if sb.slot(h, heap, |k| eq_member(k, v, heap)).is_some() == (op == OpCode::Sub) { gone.push(v); }
                heap.within_budget()?;
            }
            gone
        } else { y.borrow().iter().copied().collect() };
        let mut s = x.borrow_mut();
        match op {
            OpCode::BitOr => s.put_all(&items, heap)?,
            OpCode::BitAnd | OpCode::Sub => for v in items { s.remove(v, heap); heap.within_budget()?; },
            _ => for v in items { if !s.remove(v, heap) { s.insert(v, heap); } heap.within_budget()?; },
        }
        Ok(a)
    }

    /* set comparisons with subset and superset semantics */
    pub(crate) fn set_compare(&mut self, a: Val, b: Val, op: OpCode) -> Result<bool, VmErr> {
        let (Some(HeapObj::Set(x)), Some(HeapObj::Set(y))) = (self.heap.try_get(a), self.heap.try_get(b)) else { return Err(cold_runtime("a set operation on non-set operands")) };
        let (sa, sb) = (x.borrow(), y.borrow());
        let heap = &self.heap;
        let mut read = 0;
        // members are looked up under their stored hash and the walk pays what it reads
        let mut within = |x: &ValSet, y: &ValSet| -> Result<bool, VmErr> {
            for (h, v) in x.iter_hashed() {
                read += 1;
                if y.slot(h, heap, |k| eq_member(k, v, heap)).is_none() { return Ok(false); }
                heap.within_budget()?;
            }
            Ok(true)
        };
        let (n, m) = (sa.len(), sb.len());
        let result = match op {
            OpCode::Eq => n == m && within(&sa, &sb)?,
            OpCode::NotEq => !(n == m && within(&sa, &sb)?),
            OpCode::Lt => n < m && within(&sa, &sb)?,
            OpCode::LtEq => n <= m && within(&sa, &sb)?,
            OpCode::Gt => m < n && within(&sb, &sa)?,
            OpCode::GtEq => m <= n && within(&sb, &sa)?,
            _ => return Err(cold_runtime("set_compare with non-compare opcode")),
        };
        if read > 64 { heap.spend(read); }
        Ok(result)
    }

    pub fn sub_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        if a.is_int() && b.is_int()
            && let Some(r) = a.as_int().checked_sub(b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, &self.heap) { let (af, bf) = p?; return Ok(Val::float(af - bf)); }
        if let (Some(ai), Some(bi)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && let Some(r) = ai.checked_sub(bi) { return self.heap.int(r); }
        if let Some(r) = self.wide_op(BinOp::Sub, a, b)? { return Ok(r); }
        // set difference makes a fresh set of the `a` elements not in `b`
        if self.is_set_like(a) && self.is_set_like(b) { return self.set_binop(a, b, OpCode::Sub); }
        Err(self.unsupported(BinOp::Sub, a, b))
    }

    pub fn mul_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        if a.is_int() && b.is_int()
            && let Some(r) = crate::vm::registers::mul_exact(a.as_int(), b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, &self.heap) { let (af, bf) = p?; return Ok(Val::float(af * bf)); }
        // numeric multiply wins over sequence repetition when either side is a boxed int
        if let (Some(ai), Some(bi)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && let Some(r) = ai.checked_mul(bi) { return self.heap.int(r); }
        if let Some(r) = self.wide_op(BinOp::Mul, a, b)? { return Ok(r); }
        // sequence repetition by an int count where a bool counts as 0 or 1
        let rep_count = |v: Val| if v.is_int() { Some(v.as_int()) } else if v.is_bool() && self.heap.policy.bool_is_int { Some(v.as_bool() as i64) } else { None };
        let (seq_val, count) = if a.is_heap() && let Some(n) = rep_count(b) {
            (a, n)
        } else if b.is_heap() && let Some(n) = rep_count(a) {
            (b, n)
        } else {
            let seq = |v: Val| matches!(self.heap.try_get(v), Some(HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_) | HeapObj::List(_) | HeapObj::Tuple(_)));
            if (seq(a) && b.is_foreign()) || (seq(b) && a.is_foreign()) { return Err(EngineError::TooWide(crate::lang::ErrKind::Overflow).into()); }
            return Err(self.unsupported(BinOp::Mul, a, b));
        };
        let n = count.max(0) as usize;
        // charging the fill up front bounds `[x]*n` by the op budget not just the heap
        let fill_cost = match self.heap.get(seq_val) {
            HeapObj::Str(s) => s.len().checked_mul(n),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b.len().checked_mul(n),
            HeapObj::List(rc) => rc.borrow().len().checked_mul(n),
            HeapObj::Tuple(v) => v.len().checked_mul(n),
            _ => None,
        };
        if let Some(c) = fill_cost && c.saturating_mul(VAL_BYTES) <= self.heap.room() { self.charge_steps(c)?; }
        match self.heap.get(seq_val) {
            HeapObj::Str(s) => {
                let bytes = s.len().checked_mul(n).ok_or_else(cold_overflow)?;
                self.heap.reserve(bytes)?;
                let r = s.repeat(n);
                // the text repeated counts a long result so no reader scans it
                return if r.len() <= SHORT_TEXT { self.heap.alloc(HeapObj::Str(r)) } else { self.heap.alloc_times(r, seq_val, n) };
            }
            HeapObj::List(rc) => {
                let src = rc.borrow().clone();
                let out = self.repeat_seq(&src, n)?;
                return self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(out))));
            }
            HeapObj::Tuple(v) => {
                let src = v.clone();
                let out = self.repeat_seq(&src, n)?;
                return self.heap.alloc(HeapObj::Tuple(out));
            }
            seq @ (HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => {
                let bytes = b.len().checked_mul(n).ok_or_else(cold_overflow)?;
                self.heap.reserve(bytes)?;
                let r = b.repeat(n);
                return self.heap.alloc(if matches!(seq, HeapObj::ByteArray(_)) { HeapObj::ByteArray(r) } else { HeapObj::Bytes(r) });
            }
            _ => {}
        }
        Err(self.unsupported(BinOp::Mul, a, b))
    }

    /* repeats a sequence `n` times with the overflow and heap guards of `seq * n` */
    fn repeat_seq(&self, src: &[Val], n: usize) -> Result<Vec<Val>, VmErr> {
        // an empty source gives empty for any `n` so the n-step loop is skipped
        if src.is_empty() { return Ok(Vec::new()); }
        let cap = src.len().checked_mul(n).ok_or_else(cold_overflow)?;
        self.heap.reserve(cap.saturating_mul(VAL_BYTES))?;
        let mut out = Vec::with_capacity(cap);
        for _ in 0..n { out.extend_from_slice(src); }
        Ok(out)
    }

    pub fn div_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        let policy = self.natives.policy;
        // two ints of a policy that keeps their quotient an int divide as it rounds
        if !matches!(policy.int_div, crate::lang::IntDiv::ToFloat) && let (Some(x), Some(y)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) {
            if y == 0 { return Err(VmErr::ZeroDiv); }
            let q = match policy.int_div { crate::lang::IntDiv::Trunc => x.checked_div(y), _ => int_divmod(x, y).map(|(q, _)| q) };
            return match q { Some(q) => self.heap.int(q), None => Err(cold_int_range()) };
        }
        // boxed ints that fit i128 divide without limbs and round the same way
        if (a.is_foreign() || b.is_foreign()) && let (Some(x), Some(y)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && y != 0 {
            return Ok(Val::float(crate::value::math::div_i128(x, y)));
        }
        if let Some(r) = self.wide_op(BinOp::TrueDiv, a, b)? { return Ok(r); }
        let (Some(av), Some(bv)) = (num_f64(a, &self.heap), num_f64(b, &self.heap)) else { return Err(self.unsupported(BinOp::TrueDiv, a, b)) };
        let (av, bv) = (av?, bv?);
        // an int over an int zero always raises and a float follows `zero_div`
        if bv == 0.0 && (matches!(policy.zero_div, crate::lang::ZeroDiv::Error) || as_i128(a, &self.heap).is_some() && as_i128(b, &self.heap).is_some()) { return Err(VmErr::ZeroDiv); }
        Ok(Val::float(av / bv))
    }

    /* method wrapper around the free `as_i128` for borrow-checker ergonomics */
    #[inline]
    pub(crate) fn as_i128(&self, v: Val) -> Option<i128> {
        as_i128(v, &self.heap)
    }
}
