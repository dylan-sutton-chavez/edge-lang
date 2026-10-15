use alloc::{rc::Rc, vec, vec::Vec};

use crate::parser::{OpCode, SSAChunk, ssa_strip};

use super::VM;
use super::cache::OpcodeCache;
use super::lower::{self, Code, Facts, Ins};
use super::scope::Kind;
use super::types::*;

/* The number a register holds as a float, its int widened. */
#[inline(always)]
fn as_f64(v: Val) -> f64 { if v.is_int() { v.as_int() as f64 } else { v.as_float() } }

#[inline(always)]
fn numeric(v: Val) -> bool { v.is_int() || v.is_float() }

impl<'a> VM<'a> {
    /* The code a frame of `chunk` runs, and whether the pool never keeps it. */
    #[inline]
    pub(crate) fn frame_code(&mut self, chunk: &SSAChunk, pool: usize) -> (Rc<Code>, bool) {
        match &self.pools[pool].code {
            Some(code) => (code.clone(), false),
            None => self.first_code(chunk, pool),
        }
    }

    /* The code a chunk without lowered code runs, unlowered until a loop heats. */
    fn first_code(&mut self, chunk: &SSAChunk, pool: usize) -> (Rc<Code>, bool) {
        let names = chunk.names.len();
        if cfg!(feature = "coverage") {
            let (kinds, module) = self.chunk_kinds(chunk);
            let code = Rc::new(lower::identity(&chunk.instructions, names, kinds, module));
            self.pools[pool].code = Some(code.clone());
            return (code, false);
        }
        // A first call, or a cold resume, runs unlowered until a loop heats.
        let p = &mut self.pools[pool];
        let cold = if self.resume_ip == 0 { !core::mem::replace(&mut p.ran, true) } else { p.heat < lower::HOT_LOOP };
        if cold {
            p.heat += 1;
            let (kinds, module) = self.chunk_kinds(chunk);
            return (Rc::new(lower::identity(&lower::fuse_method_calls(chunk), names, kinds, module)), true);
        }
        (self.lowered_code(chunk, pool), false)
    }

    /* The chunk's lowered code, made once and shared by every frame after. */
    pub(crate) fn lowered_code(&mut self, chunk: &SSAChunk, pool: usize) -> Rc<Code> {
        if let Some(code) = &self.pools[pool].code { return code.clone(); }
        let names = chunk.names.len();
        let mut facts = self.lowering_facts(chunk);
        let src = lower::fuse_method_calls(chunk);
        let code = lower::lower(&src, chunk, &facts).unwrap_or_else(|| {
            facts.regs = false;
            lower::lower(&src, chunk, &facts).unwrap_or_else(|| lower::identity(&chunk.instructions, names, facts.kinds.clone(), facts.module))
        });
        let code = Rc::new(code);
        self.pools[pool].code = Some(code.clone());
        code
    }

    /* What each slot of a chunk's frame is, and the module it binds into. */
    fn chunk_kinds(&mut self, chunk: &SSAChunk) -> (Rc<[Kind]>, usize) {
        let module = self.chunk_module_id(chunk);
        if let Some(&fi) = self.body_to_fi.get(&(chunk as *const SSAChunk)) { return (self.fn_scope[fi].kinds.clone(), module); }
        // A class body keeps its namespace in slots, its members come from them.
        if self.class_chunks.contains(&(chunk as *const SSAChunk)) { return (vec![Kind::Local; chunk.names.len()].into(), module); }
        // Only the names code reads or binds as variables take a module binding.
        let mut var = vec![false; chunk.names.len()];
        for ins in &chunk.instructions {
            if matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi | OpCode::LoadGlobal | OpCode::StoreGlobal)
                && let Some(v) = var.get_mut(ins.operand as usize) { *v = true; }
        }
        let kinds = chunk.names.iter().zip(var).map(|(name, var)| {
            let bare = ssa_strip(name);
            if !var || bare.starts_with('#') { Kind::Local } else { Kind::Global(self.scopes[module].id(bare)) }
        }).collect();
        (kinds, module)
    }

    /* What a chunk's lowering may assume about its frame's slots. */
    fn lowering_facts(&mut self, chunk: &SSAChunk) -> Facts {
        let (kinds, module) = self.chunk_kinds(chunk);
        let mut bound = vec![false; chunk.names.len()];
        let fi = self.body_to_fi.get(&(chunk as *const SSAChunk)).copied();
        if let Some(fi) = fi { for &(_, s) in &self.param_slots[fi] { if let Some(b) = bound.get_mut(s) { *b = true; } } }
        let regs = !self.class_chunks.contains(&(chunk as *const SSAChunk));
        Facts { regs, bound, kinds, module }
    }

    /* Grows a frame to its code's length, and a body's template with it. */
    #[cold]
    #[inline(never)]
    pub(crate) fn grow_frame(&mut self, chunk: &SSAChunk, code: &Code, pool: usize) {
        let names = chunk.names.len();
        let consts = self.pools[pool].consts.as_deref().unwrap_or(&[]);
        // The running frame is the top of the register stack, so it grows in place.
        let grow = |regs: &mut Vec<Val>, at: usize| {
            regs.resize(at + names, Val::undef());
            regs.extend_from_slice(consts);
            regs.extend_from_slice(&[Val::none(), Val::bool(true), Val::bool(false)]);
            regs.resize(at + code.frame.max(regs.len() - at), Val::undef());
        };
        grow(&mut self.regs, self.base);
        if let Some(&fi) = self.body_to_fi.get(&(chunk as *const SSAChunk))
            && let Some(template) = self.slot_templates.get_mut(fi)
            && template.len() < code.frame
        {
            grow(template, 0);
            // The template now holds constants a collection must keep.
            self.template_roots.extend(consts.iter().filter(|v| v.is_heap()));
        }
    }

    /* The value register `r` holds, a name read the way LoadName reads it. */
    #[inline(always)]
    pub(crate) fn reg(&self, chunk: &SSAChunk, r: u16) -> Result<Val, VmErr> {
        let v = self.regs[self.base + r as usize];
        if v.is_undef() { self.unbound(chunk, r) } else { Ok(v) }
    }

    /* What reading unbound slot `r` gives, a function local failing and any other name falling back to the builtins. */
    #[cold]
    #[inline(never)]
    pub(crate) fn unbound(&self, chunk: &SSAChunk, r: u16) -> Result<Val, VmErr> {
        if self.body_to_fi.contains_key(&(chunk as *const SSAChunk)) { return Err(self.unbound_err(chunk, r as usize)); }
        let name = chunk.names.get(r as usize).map(|n| ssa_strip(n)).unwrap_or_default();
        if self.scopes[self.chunk_module_id(chunk)].closed { return Err(VmErr::Name(name.into())); }
        self.builtin_binding(name).ok_or_else(|| VmErr::Name(name.into()))
    }

    /* The error unbound slot `s` raises, a function telling its own variables from captured ones. */
    #[cold]
    #[inline(never)]
    pub(crate) fn unbound_err(&self, chunk: &SSAChunk, s: usize) -> VmErr {
        let name = chunk.names.get(s).map(|n| ssa_strip(n)).unwrap_or_default();
        let Some(&fi) = self.body_to_fi.get(&(chunk as *const SSAChunk)) else { return VmErr::Name(name.into()) };
        if self.fn_scope[fi].freevars.iter().any(|f| f.2 == name) {
            return VmErr::Raised(crate::s!("NameError: cannot access free variable '", str name, "' where it is not associated with a value in enclosing scope"));
        }
        VmErr::Raised(crate::s!("UnboundLocalError: cannot access local variable '", str name, "' where it is not associated with a value"))
    }

    /* `a = b op c` on numbers inline, anything else through the stack opcode. */
    #[inline(always)]
    pub(crate) fn reg_binop(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.regs[self.base + ins.a as usize] = match numeric_binop(lower::stack_form(ins.op), self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) {
            Some(v) => v,
            None => {
                let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                match self.wide_fast(lower::stack_form(ins.op), b, c)? { Some(v) => v, None => self.reg_stack(ins, rip, cache, chunk)? }
            }
        };
        Ok(())
    }

    /* Whether `b op c` holds, numbers compared inline. */
    #[inline(always)]
    pub(crate) fn reg_test(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if let Some(v) = numeric_binop(lower::stack_form(ins.op), self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return Ok(v.as_bool()); }
        self.reg_test_slow(ins, rip, cache, chunk)
    }

    /* The comparison the inline numbers did not settle, wide ints first and then the stack opcode, kept out of the loop. */
    #[inline(never)]
    fn reg_test_slow(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
        if let Some(v) = self.wide_fast(lower::stack_form(ins.op), b, c)? && v.is_bool() { return Ok(v.as_bool()); }
        let r = self.reg_stack(ins, rip, cache, chunk)?;
        self.truthy_op(r, chunk)
    }

    /* A register form run as its stack opcode, its result taken back. */
    #[inline(never)]
    pub(crate) fn reg_stack(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let op = lower::stack_form(ins.op);
        let x = self.reg(chunk, ins.b)?;
        self.push(x);
        if !matches!(op, OpCode::Minus | OpCode::Not | OpCode::CallLen) { let y = self.reg(chunk, ins.c)?; self.push(y); }
        let augmented = ins.x & lower::AUGMENTED != 0;
        match op {
            OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr => {
                let op = match (op, augmented) {
                    (OpCode::BitAnd, true) => OpCode::InPlaceBitAnd,
                    (OpCode::BitOr, true) => OpCode::InPlaceBitOr,
                    (OpCode::BitXor, true) => OpCode::InPlaceBitXor,
                    (op, _) => op,
                };
                self.handle_bitwise(op, (ins.x & !lower::AUGMENTED) as u16, chunk)?;
            }
            OpCode::In | OpCode::NotIn | OpCode::Is | OpCode::IsNot => self.handle_identity(op, chunk)?,
            OpCode::Not => self.handle_logic(op, chunk)?,
            OpCode::GetItem => self.get_item_op(rip, chunk, cache)?,
            OpCode::CallLen => self.handle_function(op, 1, chunk)?,
            _ => self.exec_arith_or_compare(op, ins.x as u16, rip, cache, chunk)?,
        }
        self.pop()
    }

    /* Whether register `r` is true, a user `__bool__` or `__len__` included. */
    #[inline(always)]
    pub(crate) fn reg_truthy(&mut self, chunk: &SSAChunk, r: u16) -> Result<bool, VmErr> {
        let v = self.regs[self.base + r as usize];
        if v.is_bool() { return Ok(v.as_bool()); }
        if v.is_int() { return Ok(v.as_int() != 0); }
        let v = self.reg(chunk, r)?;
        self.truthy_op(v, chunk)
    }

    /* Pushes the `ins.x` registers an instruction names, deepest first. */
    #[inline(always)]
    pub(crate) fn push_regs(&mut self, ins: Ins, chunk: &SSAChunk) -> Result<(), VmErr> {
        for &r in [ins.a, ins.b, ins.c].iter().take(ins.x as usize) {
            let v = self.reg(chunk, r)?;
            self.push(v);
        }
        Ok(())
    }
}

/* The int math register forms keep inline, None when the handler must answer. */
pub(crate) fn int_add(a: i64, b: i64) -> Option<Val> { Val::int_checked(a + b) }
pub(crate) fn int_sub(a: i64, b: i64) -> Option<Val> { Val::int_checked(a - b) }
pub(crate) fn int_mul(a: i64, b: i64) -> Option<Val> { mul_exact(a, b).and_then(Val::int_checked) }
pub(crate) fn int_mod(a: i64, b: i64) -> Option<Val> {
    if b == 0 { return None; }
    let r = a % b;
    Some(Val::int(if r != 0 && (r < 0) != (b < 0) { r + b } else { r }))
}

/* `a * b` when exact, 32-bit operands skipping wasm's 128-bit overflow routine. */
#[inline(always)]
pub(crate) fn mul_exact(a: i64, b: i64) -> Option<i64> {
    if a as i32 as i64 == a && b as i32 as i64 == b { Some(a * b) } else { a.checked_mul(b) }
}
pub(crate) fn int_div(a: i64, b: i64) -> Option<Val> { (b != 0).then(|| Val::float(a as f64 / b as f64)) }
pub(crate) fn int_floordiv(a: i64, b: i64) -> Option<Val> {
    if b == 0 { return None; }
    let (q, r) = (a / b, a % b);
    Some(Val::int(if r != 0 && (r < 0) != (b < 0) { q - 1 } else { q }))
}
pub(crate) fn float_div(a: f64, b: f64) -> Option<Val> { (b != 0.0).then(|| Val::float(a / b)) }

/* A bitwise op on two inline ints, None when a shift overflows them. */
#[inline(always)]
pub(crate) fn int_bits(op: OpCode, a: i64, b: i64) -> Option<Val> {
    match op {
        OpCode::BitAndR => Some(Val::int(a & b)),
        OpCode::BitOrR => Some(Val::int(a | b)),
        OpCode::BitXorR => Some(Val::int(a ^ b)),
        OpCode::ShlR if (0..48).contains(&b) => i64::try_from((a as i128) << b).ok().and_then(Val::int_checked),
        OpCode::ShrR if b >= 0 => Some(Val::int(a >> b.min(63))),
        _ => None,
    }
}

/* `a ** b` for small ints, None for a negative exponent or an overflow. */
fn int_pow(a: i64, b: i64) -> Option<Val> {
    if !(0..64).contains(&b) { return None; }
    let (mut result, mut base, mut e) = (1i64, a, b);
    while e > 0 {
        if e & 1 == 1 { result = mul_exact(result, base)?; }
        e >>= 1;
        if e > 0 { base = mul_exact(base, base)?; }
    }
    Val::int_checked(result)
}

/* `a ** b` for floats, None for a zero base under a negative power, a complex result or an overflow. */
fn float_pow(a: f64, b: f64) -> Option<Val> {
    if a == 0.0 && b < 0.0 || crate::value::math::complex_power(a, b) { return None; }
    crate::value::math::fpowf(a, b).map(Val::float)
}

/* `x op y` for two numbers as the register forms compute it, else None. */
#[inline(always)]
pub(crate) fn numeric_binop(op: OpCode, x: Val, y: Val) -> Option<Val> {
    if !numeric(x) || !numeric(y) { return None; }
    let ints = x.is_int() && y.is_int();
    let (a, b, f, g) = (x.as_int(), y.as_int(), as_f64(x), as_f64(y));
    let test = |i: bool, fl: bool| Some(Val::bool(if ints { i } else { fl }));
    match op {
        OpCode::Add | OpCode::InPlaceAdd => if ints { int_add(a, b) } else { Some(Val::float(f + g)) },
        OpCode::Sub | OpCode::InPlaceSub => if ints { int_sub(a, b) } else { Some(Val::float(f - g)) },
        OpCode::Mul => if ints { int_mul(a, b) } else { Some(Val::float(f * g)) },
        OpCode::Div => if ints { int_div(a, b) } else { float_div(f, g) },
        OpCode::Pow => if ints { int_pow(a, b) } else { float_pow(f, g) },
        OpCode::Mod if ints => int_mod(a, b),
        OpCode::FloorDiv if ints => int_floordiv(a, b),
        OpCode::Eq => test(a == b, f == g),
        OpCode::NotEq => test(a != b, f != g),
        OpCode::Lt => test(a < b, f < g),
        OpCode::LtEq => test(a <= b, f <= g),
        OpCode::Gt => test(a > b, f > g),
        OpCode::GtEq => test(a >= b, f >= g),
        _ => None,
    }
}

