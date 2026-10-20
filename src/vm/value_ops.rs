use crate::s;

use super::types::*;
use super::types::host::coerce_floats;
use crate::parser::types::OpCode;

use alloc::{string::String, vec::Vec, rc::Rc};
use core::cell::RefCell;









use super::VM;

impl<'a> VM<'a> {
    #[inline] pub fn truthy(&self, v: Val) -> bool { self.heap.truthy(v) }

    /* Clone the element set out of a `set` or `frozenset`, None for any other type. */
    pub(crate) fn clone_set_items(&self, v: Val) -> Option<ValSet> {
        if !v.is_heap() { return None; }
        match self.heap.get(v) {
            HeapObj::Set(s) => Some(s.borrow().clone()),
            HeapObj::FrozenSet(s) => Some((**s).clone()),
            _ => None,
        }
    }

    /* True for `set` or `frozenset` operands. */
    pub(crate) fn is_set_like(&self, v: Val) -> bool {
        v.is_heap() && matches!(self.heap.get(v), HeapObj::Set(_) | HeapObj::FrozenSet(_))
    }

    /* True for a keys or items view, which takes part in set operators. */
    pub(crate) fn is_set_view(&self, v: Val) -> bool {
        matches!(self.heap.try_get(v), Some(HeapObj::DictView(_, View::Keys | View::Items)))
    }

    /* Both operands as new sets when one is a keys or items view, `any` admitting any iterable. */
    pub(crate) fn view_set_operands(&mut self, a: Val, b: Val, any: bool, chunk: &crate::parser::SSAChunk) -> Result<Option<(Val, Val)>, VmErr> {
        let (va, vb) = (self.is_set_view(a), self.is_set_view(b));
        if !(va || vb) || !(any || (va || self.is_set_like(a)) && (vb || self.is_set_like(b))) { return Ok(None); }
        self.with_roots([a, b, Val::none()], |vm| {
            let x = vm.as_fresh_set(a, chunk)?;
            if let Some(slot) = vm.temp_roots.last_mut() { *slot = x; }
            let y = vm.as_fresh_set(b, chunk)?;
            Ok(Some((x, y)))
        })
    }

    fn as_fresh_set(&mut self, v: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        let items = match self.heap.try_get(v) {
            Some(&HeapObj::DictView(d, kind)) => self.view_items(d, kind)?,
            _ => self.iterable_items(v, chunk)?,
        };
        let s = self.valset_of(&items)?;
        self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))
    }


    /* Set binops (|, &, ^, -) over set/frozenset, result frozen iff `a` is frozen. */
    // Union/intersection/symmetric-diff/difference items, content membership lets alloc dedup distinct-handle equals.
    fn set_binop_items(&self, a: Val, b: Val, op: OpCode) -> Result<Vec<Val>, VmErr> {
        let (sa, sb) = match (self.clone_set_items(a), self.clone_set_items(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(cold_runtime("set_binop on non-set operands")),
        };
        Ok(match op {
            OpCode::BitOr => sa.iter().chain(sb.iter()).copied().collect(),
            OpCode::BitAnd => sa.iter().filter(|&&v| sb.contains(v, &self.heap)).copied().collect(),
            OpCode::BitXor => sa.iter().filter(|&&v| !sb.contains(v, &self.heap))
                .chain(sb.iter().filter(|&&v| !sa.contains(v, &self.heap))).copied().collect(),
            OpCode::Sub => sa.iter().filter(|&&v| !sb.contains(v, &self.heap)).copied().collect(),
            _ => return Err(cold_runtime("set_binop with unsupported opcode")),
        })
    }

    pub(crate) fn set_binop_and_push(&mut self, a: Val, b: Val, op: OpCode) -> Result<(), VmErr> {
        let items = self.set_binop_items(a, b, op)?;
        let frozen = matches!(self.heap.get(a), HeapObj::FrozenSet(_));
        let v = self.alloc_set_result(items, frozen)?;
        self.push(v); Ok(())
    }

    // Augmented set ops rewrite the left set in place (identity preserved), frozenset rebinds.
    pub(crate) fn set_iop_and_push(&mut self, a: Val, b: Val, op: OpCode) -> Result<(), VmErr> {
        if !matches!(self.heap.get(a), HeapObj::Set(_)) {
            return self.set_binop_and_push(a, b, op);
        }
        let items = self.set_binop_items(a, b, op)?;
        let s = ValSet::from_vals(&items, &self.heap);
        if let HeapObj::Set(rc) = self.heap.get(a) { *rc.borrow_mut() = s; }
        self.push(a); Ok(())
    }

    /* Set comparisons with subset/superset semantics over set/frozenset. */
    pub(crate) fn set_compare_and_push(&mut self, a: Val, b: Val, op: OpCode) -> Result<(), VmErr> {
        let (sa, sb) = match (self.clone_set_items(a), self.clone_set_items(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(cold_runtime("set_compare on non-set operands")),
        };
        // Content-based so distinct-handle equal elements (tuples, long strings) compare correctly.
        let eq = eq_set(&sa, &sb, |a, b| eq_member(a, b, &self.heap));
        // Each member is looked up under its stored hash, so a subset test stays linear.
        let subset = |x: &ValSet, y: &ValSet| x.len() <= y.len() && x.iter().all(|&v| y.contains(v, &self.heap));
        let result = match op {
            OpCode::Eq => eq,
            OpCode::NotEq => !eq,
            OpCode::Lt => subset(&sa, &sb) && !eq,
            OpCode::LtEq => subset(&sa, &sb),
            OpCode::Gt => subset(&sb, &sa) && !eq,
            OpCode::GtEq => subset(&sb, &sa),
            _ => return Err(cold_runtime("set_compare with non-compare opcode")),
        };
        self.push(Val::bool(result));
        Ok(())
    }

    #[inline(never)]
    pub fn type_name(&self, v: Val) -> &str {
        self.heap.type_name_by(v, &|c| match c.body {
            BodyRef::Fn(fi) if !self.is_async_fn(fi) => "generator",
            BodyRef::Fn(fi) if self.functions[fi].1.is_generator => "async_generator",
            _ => "coroutine",
        })
    }


    // Builtin str and repr with no dunder run.
    pub fn display(&self, v: Val) -> String { Host::display(self, v) }
    pub fn repr(&self, v: Val) -> String { Host::repr(self, v) }

    // Text of what only the VM makes.
    pub(crate) fn display_vm(&self, v: Val, seen: &mut Vec<u32>) -> String {
        match self.heap.get(v) {
            &HeapObj::FnCode(fi) => {
                let name = self.function_names.get(fi).map_or("<lambda>", |n| if n.is_empty() { "<lambda>" } else { n });
                let body = self.functions.get(fi).map(|f| &f.1);
                s!("<code object ", str name, str &at(v), ", file \"", str body.map_or("", |b| b.path.as_str()), "\", line ", int body.map_or(0, |b| b.first_line) as i64, ">")
            }
            &HeapObj::Func(i, ..) => s!("<function ", str self.fn_qualname(i), str &at(v), ">"),
            // User classes live in `__main__`, Python qualifies the repr with the module.
            HeapObj::Class(name, _, _) => crate::s!("<class '__main__.", str self.class_qualnames.get(&v.0).unwrap_or(name), "'>"),
            HeapObj::Instance(cls, _) => {
                // An exception reads as its message.
                if let Some(base) = self.exc_base(*cls) {
                    // An exception inside its own args reads as `E(...)` there, the way repr does.
                    let slot = self.exc_args_slot(v);
                    if slot.is_some_and(|s| seen.contains(&s)) { return s!(str self.type_name(v), "(...)"); }
                    seen.extend(slot);
                    let text = self.exc_text(base, &self.exc_args(v), seen);
                    if slot.is_some() { seen.pop(); }
                    return text;
                }
                let name = match self.heap.try_get(*cls) {
                    Some(HeapObj::Class(name, ..)) => s!("__main__.", str self.class_qualnames.get(&cls.0).unwrap_or(name)),
                    Some(HeapObj::Type(name)) => name.clone(),
                    _ => "object".into(),
                };
                s!("<", str &name, " object", str &at(v), ">")
            }
            &HeapObj::BoundUserMethod(recv, func, _) => {
                let name = match self.heap.try_get(func) { Some(&HeapObj::Func(i, ..)) => self.fn_qualname(i), _ => "?" };
                s!("<bound method ", str name, " of ", str &self.repr_d(recv, seen), ">")
            }
            HeapObj::Coroutine(c) => {
                let name = match c.body { BodyRef::Fn(fi) => self.fn_qualname(fi), BodyRef::Module => "<module>" };
                s!("<", str self.type_name(v), " object ", str name, str &at(v), ">")
            }
            HeapObj::GenericAlias(origin, args) => {
                let mut o = self.alias_arg(*origin, seen);
                o.push('[');
                match self.heap.get(*args) {
                    HeapObj::Tuple(t) if !t.is_empty() => for (i, &a) in t.iter().enumerate() {
                        if i > 0 { o.push_str(", "); }
                        o.push_str(&self.alias_arg(a, seen));
                    },
                    _ => o.push_str("()"),
                }
                o.push(']');
                o
            }
            HeapObj::Union(args) => {
                let mut o = String::new();
                if let HeapObj::Tuple(t) = self.heap.get(*args) {
                    for (i, &m) in t.iter().enumerate() {
                        if i > 0 { o.push_str(" | "); }
                        // A union stores `None` as its type and spells it back as `None`.
                        let none = m.is_heap() && matches!(self.heap.get(m), HeapObj::Type(n) if n == "NoneType");
                        o.push_str(&if none { "None".into() } else { self.alias_arg(m, seen) });
                    }
                }
                o
            }
            _ => s!("<", str self.type_name(v), " object", str &at(v), ">"),
        }
    }

    /* An exception instance reads as its constructor call, `E('x')`. */
    pub(crate) fn repr_vm(&self, v: Val, seen: &mut Vec<u32>) -> Option<String> {
        let &HeapObj::Instance(cls, _) = self.heap.get(v) else { return None };
        self.exc_base(cls)?;
        let slot = self.exc_args_slot(v);
        if slot.is_some_and(|s| seen.contains(&s)) { return Some(s!(str self.type_name(v), "(...)")); }
        seen.extend(slot);
        let mut o = s!(cap: 32; str self.type_name(v), "(");
        self.append_reprs(&mut o, self.exc_args(v).iter(), seen);
        o.push(')');
        if slot.is_some() { seen.pop(); }
        Some(o)
    }

    /* The heap slot of an exception's `args` tuple, the key cycle checks use. */
    fn exc_args_slot(&self, inst: Val) -> Option<u32> {
        let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(inst) else { return None };
        let args = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == "args")).map(|(_, v)| v)?;
        matches!(self.heap.try_get(args), Some(HeapObj::Tuple(_))).then(|| args.as_heap())
    }

    /* The `args` an exception instance holds, empty for anything else. */
    pub(crate) fn exc_args(&self, inst: Val) -> Vec<Val> {
        let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(inst) else { return Vec::new() };
        let args = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == "args")).map(|(_, v)| v);
        match args.and_then(|a| self.heap.try_get(a)) { Some(HeapObj::Tuple(t)) => t.clone(), _ => Vec::new() }
    }

    /* Whether function `fi` is an `async def`. */
    #[inline]
    pub(crate) fn is_async_fn(&self, fi: usize) -> bool { self.is_async.get(fi).copied().unwrap_or(false) }

    /* Whether `v` is a coroutine or an async generator, which only `async for` and `await` step. */
    pub(crate) fn is_async_object(&self, v: Val) -> bool {
        matches!(self.heap.try_get(v), Some(HeapObj::Coroutine(c)) if !matches!(c.body, BodyRef::Fn(fi) if !self.is_async_fn(fi)))
    }








    /* The qualified name of function `fi`, `<lambda>` for an anonymous one. */
    fn fn_qualname(&self, fi: usize) -> &str {
        match self.function_qualnames.get(fi) { Some(q) if !q.is_empty() => q, _ => match self.function_names.get(fi) { Some(n) if !n.is_empty() => n, _ => "<lambda>" } }
    }




    /* An alias argument as Python spells it, a type by its name and anything else by repr. */
    fn alias_arg(&self, v: Val, seen: &mut Vec<u32>) -> String {
        if v.is_heap() {
            match self.heap.get(v) {
                HeapObj::Type(n) => return n.clone(),
                HeapObj::TypeVar(n) => return n.trim_start_matches('*').into(),
                HeapObj::TypeAlias(q, _) => return s!("__main__.", str q),
                HeapObj::Class(n, ..) => return s!("__main__.", str self.class_qualnames.get(&v.0).unwrap_or(n)),
                HeapObj::Ellipsis => return "...".into(),
                _ => {}
            }
        }
        self.repr_d(v, seen)
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
        // Set / frozenset difference makes a fresh set of `a` elements not in `b`.
        if let (Some(sa), Some(sb)) = (self.clone_set_items(a), self.clone_set_items(b)) {
            let items: Vec<Val> = sa.iter().filter(|&&v| !sb.contains(v, &self.heap)).copied().collect();
            let frozen = matches!(self.heap.get(a), HeapObj::FrozenSet(_));
            return self.alloc_set_result(items, frozen);
        }
        Err(self.unsupported("-", a, b))
    }

    pub fn mul_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        if a.is_int() && b.is_int()
            && let Some(r) = super::registers::mul_exact(a.as_int(), b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, &self.heap) { let (af, bf) = p?; return Ok(Val::float(af * bf)); }
        // Numeric multiply wins over sequence repetition when either side is a boxed int.
        if let (Some(ai), Some(bi)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && let Some(r) = ai.checked_mul(bi) { return self.heap.int(r); }
        if let Some(r) = self.wide_op(BinOp::Mul, a, b)? { return Ok(r); }
        // Sequence repetition, str/bytes/list/tuple * int, bool counts as 0/1 (count clamped to i64).
        let rep_count = |v: Val| if v.is_int() { Some(v.as_int()) } else if v.is_bool() { Some(v.as_bool() as i64) } else { None };
        let (seq_val, count) = if a.is_heap() && let Some(n) = rep_count(b) {
            (a, n)
        } else if b.is_heap() && let Some(n) = rep_count(a) {
            (b, n)
        } else {
            let seq = |v: Val| matches!(self.heap.try_get(v), Some(HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_) | HeapObj::List(_) | HeapObj::Tuple(_)));
            if (seq(a) && b.is_foreign()) || (seq(b) && a.is_foreign()) { return Err(VmErr::Raised(String::from("OverflowError: cannot fit 'int' into an index-sized integer"))); }
            let other = if seq(a) { b } else if seq(b) { a } else { return Err(self.unsupported("*", a, b)) };
            return Err(VmErr::TypeMsg(s!("can't multiply sequence by non-int of type '", str self.type_name(other), "'")));
        };
        let n = count.max(0) as usize;
        // Charge the fill up front so repeated `[x]*n` is bounded by the op budget, not just heap.
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
                return self.heap.alloc(HeapObj::Str(r));
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
        Err(self.unsupported("*", a, b))
    }

    /* Repeat a sequence `n` times with the same budget/overflow/heap-limit guards as `seq * n`. */
    fn repeat_seq(&self, src: &[Val], n: usize) -> Result<Vec<Val>, VmErr> {
        // Empty source means result is empty for any n, so skip the n-iteration loop.
        if src.is_empty() { return Ok(Vec::new()); }
        let cap = src.len().checked_mul(n).ok_or_else(cold_overflow)?;
        self.heap.reserve(cap.saturating_mul(VAL_BYTES))?;
        let mut out = Vec::with_capacity(cap);
        for _ in 0..n { out.extend_from_slice(src); }
        Ok(out)
    }

    pub fn div_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        // Boxed ints that fit i128 divide without limbs, rounded the same way.
        if (a.is_foreign() || b.is_foreign()) && let (Some(x), Some(y)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && y != 0 {
            return Ok(Val::float(crate::frontend::bigint::BigInt::div_i128(x, y)));
        }
        if let Some(r) = self.wide_op(BinOp::TrueDiv, a, b)? { return Ok(r); }
        let (Some(av), Some(bv)) = (num_f64(a, &self.heap), num_f64(b, &self.heap)) else { return Err(self.unsupported("/", a, b)) };
        let (av, bv) = (av?, bv?);
        if bv == 0.0 { return Err(VmErr::ZeroDiv); }
        Ok(Val::float(av / bv))
    }


    /* Method wrapper around the free `as_i128` for borrow-checker ergonomics. */
    #[inline]
    pub(crate) fn as_i128(&self, v: Val) -> Option<i128> {
        as_i128(v, &self.heap)
    }

}


pub(crate) use super::types::render::{MAX_REPR_LEN, RENDER_DEPTH_MAX, at, push_repr_str};
