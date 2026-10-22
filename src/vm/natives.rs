use alloc::{string::{String, ToString}, vec::Vec};
use super::types::*;
use super::jobs::{Check, Folded, Gather, Takes, Total, take_plain};
use super::keys::{PairScan, scan_pairs};
use super::VM;
use crate::parser::{OpCode, SSAChunk};
use alloc::rc::Rc;
use crate::s;
use core::cell::RefCell;
use super::globals::numeric::{float_arg, float_to_int, int_arg, parse_int_radix, round_float_tens};

// Arguments and user code hooks a builtin function reads past the host.
pub(crate) trait Frames: Host + Sized {
    type Code: ?Sized;
    fn pop(&mut self) -> Result<Val, VmErr>;
    fn pop_n(&mut self, n: usize) -> Result<Vec<Val>, VmErr>;
    // Whether this call may open a frame of the loop.
    fn take_job_ok(&mut self) -> bool { false }
    fn check_job(&mut self, _o: Val, _check: Check) -> bool { false }
    fn render_job(&mut self, _o: Val, _print: bool, _chunk: &Self::Code) -> Result<bool, VmErr> { Ok(false) }
    fn str_job(&mut self, _o: Val, _print: bool) -> bool { false }
    fn try_call_dunder(&mut self, _recv: Val, _name: &str, _args: &[Val], _chunk: &Self::Code) -> Result<Option<Val>, VmErr> { Ok(None) }
    fn lookup_class_member(&self, _cls: Val, _name: &str) -> Option<(Val, Val)> { None }
    // The result a call left.
    fn last(&self) -> Option<Val>;
    fn stack(&mut self) -> &mut Vec<Val>;
    fn is_async_object(&self, _v: Val) -> bool { false }
    // Pops positional and flat keyword arguments.
    fn parse_call_args(&mut self, operand: u16) -> Result<(Vec<Val>, Vec<Val>), VmErr> {
        let mut all = self.pop_n((operand & 0xFF) as usize + 2 * ((operand >> 8) & 0xFF) as usize)?;
        let kw = all.split_off((operand & 0xFF) as usize);
        Ok((all, kw))
    }
    // Whether stepping `o` may run user code.
    fn may_job(&self, o: Val) -> bool { o.is_heap() && matches!(self.heap().get(o), HeapObj::Coroutine(_) | HeapObj::Instance(..) | HeapObj::Iter(..)) }
    fn items_job(&mut self, _o: Val, _takes: Takes, _chunk: &Self::Code) -> Result<bool, VmErr> { Ok(false) }
    fn gather_gen(&mut self, _o: Val, _takes: Takes, _extra: &[Val]) -> Result<bool, VmErr> { Ok(false) }
    // The key `k` bound to a function of the loop when it steps a generator among `items` in Rust, None when it steps none.
    fn held_key(&mut self, _k: Val, _items: &[Val]) -> Result<Option<Val>, VmErr> { Ok(None) }
    // Gathers the iterable at stack index `base` of builtin `id` by a job of the loop, whether the job took the call.
    fn gather_call(&mut self, _id: NativeFnId, _op: u16, _base: usize, _chunk: &Self::Code) -> Result<bool, VmErr> { Ok(false) }
    fn iter_to_vec_op(&mut self, _obj: Val, _chunk: &Self::Code) -> Result<Option<Vec<Val>>, VmErr> { Ok(None) }
    // A fold of plain items counts this down per item.
    fn budget_mut(&mut self) -> &mut u64;
    // Split borrow for a loop reading the heap and counting the budget.
    fn heap_budget(&mut self) -> (&HeapPool, &mut u64);
    fn charge_step(&mut self) -> Result<(), VmErr> { let n = 1 + self.heap().take_work(); self.charge(n) }
    // Only the VM continues a fold as a job.
    fn fold_run(&mut self, o: Val, takes: Takes, extra: &[Val], _frames: bool, chunk: &Self::Code) -> Result<(), VmErr> {
        let mut rest = None;
        self.fold_ready(o, takes, extra, chunk, &mut rest)?;
        match rest { None => Ok(()), Some(_) => Err(cold_runtime("a fold that runs user code")) }
    }
    fn try_binary_dunder(&mut self, _op: OpCode, _a: Val, _b: Val, _inplace: bool, _chunk: &Self::Code) -> Result<Option<Val>, VmErr> { Ok(None) }
    fn try_compare_dunder(&mut self, _op: OpCode, _a: Val, _b: Val, _chunk: &Self::Code) -> Result<Option<Val>, VmErr> { Ok(None) }
    // Sequences holding instances walk through their dunders.
    fn values_lt(&mut self, a: Val, b: Val, _chunk: &Self::Code) -> Result<bool, VmErr> { self.lt_vals(a, b) }
    fn exec_call(&mut self, _operand: u16, _chunk: &Self::Code) -> Result<(), VmErr> { Err(cold_runtime("a call of user code")) }
    fn call_rows(&mut self, _f: Val, _cols: &[Vec<Val>], _n: usize, _chunk: &Self::Code) -> Result<Vec<Val>, VmErr> { Err(cold_runtime("a call of user code")) }
    // Builtin iterators step as the VM steps them.
    fn next_of(&mut self, it: Val, chunk: &Self::Code) -> Result<Option<Val>, VmErr> {
        let Some(HeapObj::Iter(frame, _)) = self.heap().try_get(it) else { return Err(VmErr::TypeMsg(s!("'", str self.type_name(it), "' object is not an iterator"))) };
        let frame = frame.clone();
        let state = frame.borrow().clone();
        match state {
            IterFrame::Zip { its, strict } => self.zip_step(&its, strict, chunk),
            IterFrame::Enumerate { it, n } => self.enumerate_step(Some(&frame), it, n, chunk),
            _ => frame.borrow_mut().next_item(self.heap_mut()),
        }
    }
    fn merge_job(&mut self, items: Vec<Val>, _reverse: bool, _into: Val) -> Result<(), Vec<Val>> { Err(items) }
    fn complex_job(&mut self, _v: Val) -> Result<(), VmErr> { Err(cold_runtime("a call of user code")) }
    fn keys_floor(&mut self, _key: Val, _items: Vec<Val>, _is_min: bool) -> Result<(), VmErr> { Err(cold_runtime("a call of user code")) }
    // Containers render with the __repr__ of instances they hold.
    fn render_now(&mut self, v: Val, _chunk: &Self::Code, seen: &mut Vec<u32>) -> Result<String, VmErr> { Ok(self.repr_d(v, seen)) }
}

impl<'a> Frames for VM<'a> {
    type Code = SSAChunk;
    #[inline(always)] fn pop(&mut self) -> Result<Val, VmErr> { VM::pop(self) }
    #[inline(always)] fn pop_n(&mut self, n: usize) -> Result<Vec<Val>, VmErr> { VM::pop_n(self, n) }
    #[inline(always)] fn take_job_ok(&mut self) -> bool { core::mem::take(&mut self.job_ok) }
    #[inline] fn check_job(&mut self, o: Val, check: Check) -> bool { VM::check_job(self, o, check) }
    #[inline] fn render_job(&mut self, o: Val, print: bool, chunk: &SSAChunk) -> Result<bool, VmErr> { VM::render_job(self, o, print, chunk) }
    #[inline] fn str_job(&mut self, o: Val, print: bool) -> bool { VM::str_job(self, o, print) }
    #[inline] fn try_call_dunder(&mut self, recv: Val, name: &str, args: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> { VM::try_call_dunder(self, recv, name, args, chunk) }
    #[inline] fn lookup_class_member(&self, cls: Val, name: &str) -> Option<(Val, Val)> { VM::lookup_class_member(self, cls, name) }
    #[inline] fn last(&self) -> Option<Val> { self.stack.last().copied() }
    #[inline(always)] fn stack(&mut self) -> &mut Vec<Val> { &mut self.stack }
    #[inline] fn is_async_object(&self, v: Val) -> bool { VM::is_async_object(self, v) }
    #[inline] fn parse_call_args(&mut self, operand: u16) -> Result<(Vec<Val>, Vec<Val>), VmErr> { VM::parse_call_args(self, operand) }
    #[inline] fn may_job(&self, o: Val) -> bool { VM::may_job(self, o) }
    #[inline] fn items_job(&mut self, o: Val, takes: Takes, chunk: &SSAChunk) -> Result<bool, VmErr> { VM::items_job(self, o, takes, chunk) }
    #[inline] fn gather_gen(&mut self, o: Val, takes: Takes, extra: &[Val]) -> Result<bool, VmErr> { VM::gather_gen(self, o, takes, extra) }
    fn held_key(&mut self, k: Val, items: &[Val]) -> Result<Option<Val>, VmErr> {
        if !self.steps_whole(k) { return Ok(None); }
        let leaf = |vm: &Self, vs: &[Val]| vs.iter().any(|&v| vm.steps_py(v));
        let held = match items[..] {
            [o] => match self.heap.try_get(o) { Some(HeapObj::List(rc)) => { let l = rc.borrow().clone(); leaf(self, &l) }, Some(HeapObj::Tuple(t)) => leaf(self, t), _ => true },
            _ => leaf(self, items),
        };
        if !held { return Ok(None); }
        Ok(Some(self.through_loop(k, 1)?.unwrap_or(k)))
    }
    fn gather_call(&mut self, id: NativeFnId, op: u16, base: usize, chunk: &SSAChunk) -> Result<bool, VmErr> {
        self.job_ok = true;
        if self.gather_arg(id, op, base, chunk)? { return Ok(true); }
        self.job_ok = false;
        Ok(false)
    }
    #[inline] fn iter_to_vec_op(&mut self, obj: Val, chunk: &SSAChunk) -> Result<Option<Vec<Val>>, VmErr> { VM::iter_to_vec_op(self, obj, chunk) }
    #[inline(always)] fn budget_mut(&mut self) -> &mut u64 { &mut self.budget }
    #[inline(always)] fn heap_budget(&mut self) -> (&HeapPool, &mut u64) { (&self.heap, &mut self.budget) }
    #[inline(always)] fn charge_step(&mut self) -> Result<(), VmErr> { VM::charge_step(self) }
    #[inline] fn fold_run(&mut self, o: Val, takes: Takes, extra: &[Val], frames: bool, chunk: &SSAChunk) -> Result<(), VmErr> { VM::fold_run(self, o, takes, extra, frames, chunk) }
    #[inline] fn try_binary_dunder(&mut self, op: OpCode, a: Val, b: Val, inplace: bool, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> { VM::try_binary_dunder(self, op, a, b, inplace, chunk) }
    #[inline] fn try_compare_dunder(&mut self, op: OpCode, a: Val, b: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> { VM::try_compare_dunder(self, op, a, b, chunk) }
    #[inline] fn values_lt(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> { VM::values_lt(self, a, b, chunk) }
    #[inline] fn exec_call(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> { VM::exec_call(self, operand, chunk) }
    #[inline] fn call_rows(&mut self, f: Val, cols: &[Vec<Val>], n: usize, chunk: &SSAChunk) -> Result<Vec<Val>, VmErr> { VM::call_rows(self, f, cols, n, chunk) }
    #[inline] fn next_of(&mut self, it: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> { VM::next_of(self, it, chunk) }
    #[inline] fn merge_job(&mut self, items: Vec<Val>, reverse: bool, into: Val) -> Result<(), Vec<Val>> { VM::merge_job(self, items, reverse, into) }
    fn complex_job(&mut self, v: Val) -> Result<(), VmErr> {
        if let Some(r) = self.calls_run(alloc::boxed::Box::new(super::globals::numeric::ComplexOf { v, hook: 0 }), true)? { self.push(r); }
        Ok(())
    }
    #[inline] fn keys_floor(&mut self, key: Val, items: Vec<Val>, is_min: bool) -> Result<(), VmErr> { VM::keys_floor(self, key, items, is_min) }
    #[inline] fn render_now(&mut self, v: Val, chunk: &SSAChunk, seen: &mut Vec<u32>) -> Result<String, VmErr> { VM::render_now(self, v, chunk, seen) }
}

// Builtin functions with one body for the VM and call_pure.
pub(crate) trait Builtins: Frames {
    fn call_abs(&mut self, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        let o = self.pop()?;
        // An instance's `__abs__` runs as a frame of the loop.
        if ok && self.is_instance(o) && self.check_job(o, crate::vm::jobs::Check::Abs) { return Ok(()); }
        // Honor a user `__abs__` and pass any return type through.
        if let Some(r) = self.try_call_dunder(o, "__abs__", &[], chunk)? {
            self.push(r);
            return Ok(());
        }
        let v = if o.is_float() {
            Val::float(o.as_float().abs())
        } else if o.is_int() || o.is_bool() {
            Val::int(self.as_i128(o).unwrap_or(0).abs() as i64)
        } else if let Some(r) = self.heap_mut().foreign_unop(UnOp::Abs, o)? {
            r
        } else {
            return Err(self.bad_operand("abs()", o));
        };
        self.push(v); Ok(())
    }

    #[inline(never)]
    fn call_int(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        // Two-arg form `int(string, base)` parses the string in the given radix.
        if argc == 2 {
            let base_v = self.pop()?;
            let o = self.pop()?;
            if !base_v.is_int() { return Err(cold_type("int() base must be an integer")); }
            let base = base_v.as_int();
            let s = match self.heap().try_get(o) {
                Some(HeapObj::Str(s)) => s.clone(),
                Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => core::str::from_utf8(b).map_err(|_| self.invalid_int(cold_value("invalid literal"), o, base))?.into(),
                _ => return Err(cold_type("int() can't convert non-string with explicit base")),
            };
            let v = parse_int_radix(self.heap_mut(), &s, base).map_err(|e| self.invalid_int(e, o, base))?;
            self.push(v); return Ok(());
        }
        if argc == 0 { self.push(Val::int(0)); return Ok(()); }
        if argc > 2 { for _ in 0..argc { let _ = self.pop(); } return Err(cold_type("int() takes at most 2 arguments")); }
        let o = self.pop()?;
        let o = self.plain_number(o, "__int__", chunk)?;
        // An int passes through unchanged.
        if o.is_int() || crate::frontend::int::is_big(self.heap(), o) { self.push(o); return Ok(()); }
        let v = if o.is_float() { float_to_int(self.heap_mut(), o.as_float(), ftrunc)? }
            else if o.is_bool() { Val::int(o.as_bool() as i64) }
            else if o.is_heap() && let HeapObj::Str(s) = self.heap().get(o) {
                let s = s.clone();
                parse_int_radix(self.heap_mut(), &s, 10).map_err(|e| self.invalid_int(e, o, 10))?
            }
            else if let Some(b) = self.heap().bytes_of(o) {
                let text = core::str::from_utf8(b).map(String::from).map_err(|_| cold_value("invalid literal"));
                text.and_then(|t| parse_int_radix(self.heap_mut(), &t, 10)).map_err(|e| self.invalid_int(e, o, 10))?
            }
            else if o.is_heap() && matches!(self.heap().get(o), HeapObj::Instance(..)) {
                // A user `__int__` runs as a frame of the loop, its int checked as it returns.
                if ok && self.check_job(o, crate::vm::jobs::Check::Int(o)) { return Ok(()); }
                // Honor a user `__int__` method.
                match self.try_call_dunder(o, "__int__", &[], chunk)? {
                    Some(r) if r.is_bool() => Val::int(r.as_bool() as i64),
                    Some(r) if r.is_int() || crate::frontend::int::is_big(self.heap(), r) => r,
                    Some(_) => return Err(cold_type("__int__ returned non-int")),
                    None => return Err(int_arg(self, o)),
                }
            }
            else { return Err(int_arg(self, o)); };
        self.push(v); Ok(())
    }

    /* The number in `o`, a value of a class over int or float, through the class `hook` when it defines one. */
    fn plain_number(&mut self, o: Val, hook: &str, chunk: &Self::Code) -> Result<Val, VmErr> {
        let Some(&HeapObj::Boxed(n)) = self.heap().try_get(o) else { return Ok(o) };
        Ok(self.try_call_dunder(o, hook, &[], chunk)?.map_or(n, |r| crate::value::unboxed(r, self.heap())))
    }

    /* An invalid literal error that quotes `o` and names `base` like Python, any other error as it is. */
    fn invalid_int(&self, e: VmErr, o: Val, base: i64) -> VmErr {
        match e {
            VmErr::Value(m) if m.contains("invalid literal") => VmErr::Raised(crate::s!("ValueError: invalid literal for int() with base ", int base, ": ", str &self.repr(o))),
            e => e,
        }
    }

    /* Converts int or parseable string to floating point. */
    #[inline(never)]
    fn call_float(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        if argc == 0 { self.push(Val::float(0.0)); return Ok(()); } // `float()` is 0.0.
        let o = self.pop()?;
        let o = self.plain_number(o, "__float__", chunk)?;
        // A float comes back as the same object.
        if o.is_float() { self.push(o); return Ok(()); }
        let text = match self.heap().try_get(o) {
            Some(HeapObj::Str(s)) => Some(s.clone()),
            _ => self.heap().bytes_of(o).map(|b| String::from_utf8_lossy(b).into_owned()),
        };
        let f = if let Some(f) = num_f64(o, self.heap()) { f? }
            else if let Some(s) = text {
                let t = crate::util::uni::number_text(&s);
                let t = t.as_ref();
                // Accept `_` digit separators, strip then parse.
                let cleaned = if t.contains('_') {
                    if t.starts_with('_') || t.ends_with('_') || t.contains("__") { return Err(self.no_float(o)); }
                    t.chars().filter(|&c| c != '_').collect::<alloc::string::String>()
                } else { alloc::string::String::from(t) };
                cleaned.parse().map_err(|_| self.no_float(o))?
            }
            else if o.is_heap() && matches!(self.heap().get(o), HeapObj::Instance(..)) {
                // A user `__float__` runs as a frame of the loop, its float checked as it returns.
                if ok && self.check_job(o, crate::vm::jobs::Check::Float(o)) { return Ok(()); }
                // Honor a user `__float__` and fall back to `__index__` like CPython.
                match self.try_call_dunder(o, "__float__", &[], chunk)? {
                    Some(r) if r.is_float() => { self.push(r); return Ok(()); }
                    Some(r) => return Err(self.returned_wrong(o, "__float__", "a float", r)),
                    None => match self.try_call_dunder(o, "__index__", &[], chunk)? {
                        Some(r) if r.is_int() || r.is_bool() || crate::frontend::int::is_big(self.heap(), r) => num_f64(r, self.heap()).unwrap_or(Ok(0.0))?,
                        Some(r) => return Err(self.returned_wrong(o, "__index__", "an int", r)),
                        None => return Err(float_arg(self, o)),
                    },
                }
            }
            else { return Err(float_arg(self, o)); };
        self.push(Val::float(f)); Ok(())
    }

    /* `complex(real, imag)` from numbers or one string, a value whose class defines `__complex__` asked first. */
    fn call_complex(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        use crate::frontend::complex::{Complex, parse, parts, val};
        let ok = self.take_job_ok();
        let args = self.pop_n(argc as usize)?;
        // Called where a frame may open, the dunder that converts an instance runs as a frame of the loop.
        if ok && argc == 1 && let Some(&a) = args.first() && self.is_instance(a) { return self.complex_job(a); }
        let num = |vm: &mut Self, v: Val, which: &str| -> Result<Complex, VmErr> {
            if let Some(z) = parts(vm.heap(), crate::value::unboxed(v, vm.heap())) { return Ok(z); }
            for hook in ["__complex__", "__float__", "__index__"] {
                if let Some(r) = vm.try_call_dunder(v, hook, &[], chunk)? && let Some(z) = parts(vm.heap(), r) { return Ok(z); }
            }
            Err(VmErr::TypeMsg(match which {
                "" => crate::s!("complex() argument must be a string or a number, not ", str vm.type_name(v)),
                part => crate::s!("complex() argument '", str part, "' must be a real number, not ", str vm.type_name(v)),
            }))
        };
        let text = |vm: &Self, v: Val| match vm.heap().try_get(v) { Some(HeapObj::Str(s)) => Some(s.clone()), _ => None };
        let z = match args.as_slice() {
            [] => Complex { re: 0.0, im: 0.0 },
            [s] if text(self, *s).is_some() => parse(&text(self, *s).unwrap_or_default()).ok_or_else(|| cold_value("complex() arg is a malformed string"))?,
            [a] => num(self, *a, "")?,
            [a, b] => {
                let (x, y) = (num(self, *a, "real")?, num(self, *b, "imag")?);
                Complex { re: x.re - y.im, im: x.im + y.re }
            }
            _ => return Err(cold_type("complex() takes at most 2 arguments")),
        };
        let v = val(self.heap_mut(), z.re, z.im)?;
        self.push(v);
        Ok(())
    }

    fn no_float(&self, o: Val) -> VmErr {
        VmErr::Raised(crate::s!("ValueError: could not convert string to float: ", str &self.repr(o)))
    }

    fn call_chr(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        if !(o.is_int() || o.is_bool() || crate::frontend::int::is_big(self.heap(), o)) { return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(o), "' object cannot be interpreted as an integer"))); }
        let c = self.as_i128(o).and_then(|i| u32::try_from(i).ok()).and_then(char::from_u32).ok_or_else(|| cold_value("chr() arg not in range(0x110000)"))?;
        let mut s = String::with_capacity(4);
        s.push(c);
        self.alloc_and_push_str(s)
    }

    fn call_ord(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        if o.is_heap()
            && let HeapObj::Str(s) = self.heap().get(o) {
                let mut cs = s.chars();
                if let (Some(c), None) = (cs.next(), cs.next()) {
                    self.push(Val::int(c as i64)); return Ok(());
                }
        }
        // ord() of a one-byte bytes returns the byte value.
        if let Some(b) = self.heap().bytes_of(o) && b.len() == 1 {
            let v = b[0] as i64;
            self.push(Val::int(v)); return Ok(());
        }
        let found = match self.heap().try_get(o) {
            Some(HeapObj::Str(s)) => s.chars().count(),
            _ => match self.heap().bytes_of(o) { Some(b) => b.len(), None => return Err(VmErr::TypeMsg(crate::s!("ord() expected string of length 1, but ", str self.type_name(o), " found"))) },
        };
        Err(VmErr::TypeMsg(crate::s!("ord() expected a character, but string of length ", int found as i64, " found")))
    }

    #[inline(never)]
    fn call_round(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        let v = match (args.first().copied(), args.get(1).copied()) {
            (Some(o), Some(n)) if o.is_float() && n.is_int() => {
                let x = o.as_float();
                let nd = n.as_int();
                if !x.is_finite() {
                    Val::float(x)
                } else if nd >= 0 {
                    // Correctly-rounded decimal (round-half-even on the true value), avoids the double-rounding `x*10^n` would introduce (e.g. round(2.675, 2) -> 2.67).
                    let s = alloc::format!("{:.*}", (nd as usize).min(323), x);
                    Val::float(s.parse().unwrap_or(x))
                } else {
                    let r = round_float_tens(x, nd.unsigned_abs().min(309) as usize);
                    if r.is_infinite() { return Err(VmErr::Raised(String::from("OverflowError: rounded value too large to represent"))); }
                    Val::float(r)
                }
            }
            (Some(o), None) if o.is_float() => {
                // 1-arg round returns an int, promote via int_to_val so large results don't wrap.
                float_to_int(self.heap_mut(), o.as_float(), fround)?
            }
            // Ints/bools round to themselves, negative ndigits round to tens/hundreds.
            (Some(o), n) if o.is_bool() || o.is_int() || crate::frontend::int::is_big(self.heap(), o) => {
                let o = if o.is_bool() { Val::int(o.as_bool() as i64) } else { o };
                match n {
                    // An inline int and at most 18 dropped digits stay in i64, half to even.
                    Some(n) if o.is_int() && (-18..0).contains(&n.as_int()) => {
                        let (x, f) = (o.as_int(), 10i64.pow(n.as_int().unsigned_abs() as u32));
                        let (q, r) = (x.div_euclid(f), x.rem_euclid(f));
                        self.heap_mut().int(((q + (2 * r > f || (2 * r == f && q % 2 != 0)) as i64) * f) as i128)?
                    }
                    Some(n) if n.is_int() && n.as_int() < 0 => crate::frontend::int::round_tens(self.heap_mut(), o, n.as_int().unsigned_abs()).unwrap_or(Ok(o))?,
                    _ => o,
                }
            }
            _ => return Err(cold_type("round() requires a number")),
        };
        self.push(v); Ok(())
    }

    #[inline(never)]
    fn call_range(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        // A bound past i64 makes a wide range.
        let (s, e, st) = match args[..] { [e] => (Val::int(0), e, Val::int(1)), [s, e] => (s, e, Val::int(1)), [s, e, st] => (s, e, st), _ => return Err(cold_type("range() takes 1 to 3 arguments")) };
        if let Some(r) = crate::frontend::int::new_range(self.heap_mut(), s, e, st) { let r = r?; self.push(r); return Ok(()); }
        // Accept any integer, bools included, that fits the i64 range bounds.
        // The first bound that is no integer names its type.
        if let Some(&bad) = args.iter().find(|&&a| self.as_i128(a).is_none() || a.is_float()) {
            return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(bad), "' object cannot be interpreted as an integer")));
        }
        let gi = |i: Option<i128>| -> Result<i64, VmErr> {
            match i {
                Some(n) if (i64::MIN as i128..=i64::MAX as i128).contains(&n) => Ok(n as i64),
                Some(_) => Err(cold_overflow()),
                None => Err(cold_type("range() arguments must be integers")),
            }
        };
        let (s, e, st) = match args.len() {
            1 => (0, gi(self.as_i128(args[0]))?, 1),
            2 => (gi(self.as_i128(args[0]))?, gi(self.as_i128(args[1]))?, 1),
            3 => (gi(self.as_i128(args[0]))?, gi(self.as_i128(args[1]))?, gi(self.as_i128(args[2]))?),
            _ => return Err(cold_type("range() takes 1 to 3 arguments")),
        };
        if st == 0 { return Err(cold_value("range() arg 3 must not be zero")); }
        let val = self.heap_mut().alloc(HeapObj::Range(s, e, st))?;
        self.push(val); Ok(())
    }

    fn call_bin(&mut self) -> Result<(), VmErr> { self.call_radix(2,  "0b") }

    fn call_oct(&mut self) -> Result<(), VmErr> { self.call_radix(8,  "0o") }

    fn call_hex(&mut self) -> Result<(), VmErr> { self.call_radix(16, "0x") }

    /* Push int as "<prefix><digits>" with optional sign. */
    fn call_radix(&mut self, radix: u32, prefix: &str) -> Result<(), VmErr> {
        let o = self.pop()?;
        let Some(d) = crate::frontend::int::radix_digits(self.heap(), o, radix) else { return Err(cold_type("integer required")); };
        let (neg, digits) = d?;
        self.alloc_and_push_str(crate::s!(str if neg { "-" } else { "" }, str prefix, str &digits))
    }

    /* `str(v)` semantics, instance `__str__` wins, then `__repr__`, else the built-in display. */
    fn display_op(&mut self, v: Val, chunk: &Self::Code) -> Result<String, VmErr> {
        if self.instance_class(v).is_some() {
            if let Some(r) = self.try_call_dunder(v, "__str__", &[], chunk)? {
                return self.require_str(r, "__str__");
            }
            if let Some(r) = self.try_call_dunder(v, "__repr__", &[], chunk)? {
                return self.require_str(r, "__repr__");
            }
        }
        // Containers render their elements with repr, dispatching user __repr__ on instances.
        if self.is_container_val(v) { return self.repr_op(v, chunk); }
        let s = match self.heap().foreign_repr(v) { Some(r) => r?, None => self.display(v) };
        // Render is O(size). Charge it so reprinting growing data can't outrun the budget.
        self.charge(s.len())?;
        Ok(s)
    }

    /* `repr(v)` semantics, instance `__repr__` wins, otherwise the built-in repr (which adds quotes for strings, etc.). */
    fn repr_op(&mut self, v: Val, chunk: &Self::Code) -> Result<String, VmErr> {
        let s = self.repr_deep(v, chunk, &mut Vec::new())?;
        self.charge(s.len())?;
        Ok(s)
    }

    /* Container-aware repr dispatches `__repr__` on nested instances. Elements always use repr, with `seen` tracking heap ids for cycle detection. */
    fn repr_deep(&mut self, v: Val, chunk: &Self::Code, seen: &mut Vec<u32>) -> Result<String, VmErr> {
        if let Some(r) = self.heap().foreign_repr(v) { return r; }
        if !v.is_heap() { return Ok(self.repr(v)); }
        if self.heap().classed(v).is_some() && let Some(r) = self.try_call_dunder(v, "__repr__", &[], chunk)? {
            return self.require_str(r, "__repr__");
        }
        if !self.is_container_val(v) {
            if matches!(self.heap().get(v), HeapObj::Instance(..))
                && let Some(r) = self.try_call_dunder(v, "__repr__", &[], chunk)? {
                return self.require_str(r, "__repr__");
            }
            // The outer path goes along, so an exception inside its own args stops there.
            return Ok(self.repr_d(v, seen));
        }
        let id = v.as_heap();
        if seen.contains(&id) {
            return Ok(match self.heap().get(v) {
                HeapObj::Dict(_) | HeapObj::Set(_) => "{...}".into(),
                HeapObj::FrozenDict(_) => "frozendict({...})".into(),
                HeapObj::Tuple(_) => "(...)".into(),
                HeapObj::FrozenSet(_) => "frozenset({...})".into(),
                _ => "[...]".into(),
            });
        }
        if seen.len() > crate::vm::value_ops::RENDER_DEPTH_MAX { return Ok("...".into()); }
        self.render_now(v, chunk, seen)
    }

    fn is_container_val(&self, v: Val) -> bool {
        v.is_heap() && matches!(
            self.heap().get(v),
            HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_)
        )
    }

    fn require_str(&self, v: Val, name: &str) -> Result<String, VmErr> {
        if v.is_heap() && let HeapObj::Str(s) = self.heap().get(v) { return Ok(s.clone()); }
        Err(VmErr::TypeMsg(crate::s!("'", str name, "' did not return a string")))
    }

    /* `format(v, spec)`, an instance `__format__(spec)` wins, an empty spec is `str(v)` and the rest runs the spec engine. */
    fn format_op(&mut self, v: Val, spec: &str, chunk: &Self::Code) -> Result<String, VmErr> {
        if self.instance_class(v).is_some() {
            let spec_val = self.heap_mut().alloc(HeapObj::Str(spec.to_string()))?;
            if let Some(r) = self.try_call_dunder(v, "__format__", &[spec_val], chunk)? {
                if !matches!(self.heap().try_get(r), Some(HeapObj::Str(_))) { return Err(VmErr::TypeMsg(crate::s!("__format__ must return a str, not ", str self.type_name(r)))); }
                return self.require_str(r, "__format__");
            }
        }
        let v = crate::value::unboxed(v, self.heap());
        if spec.is_empty() { return self.display_op(v, chunk); }
        if let Some(z) = crate::frontend::complex::parts(self.heap(), v).filter(|_| crate::frontend::complex::is_complex(self.heap(), v)) {
            return crate::vm::format_spec::format_complex(z.re, z.im, spec, self.heap());
        }
        // Only numbers and strings read a spec, `f"{x!s:>9}"` pads anything else.
        let own = v.is_int() || v.is_float() || v.is_bool() || crate::frontend::int::is_big(self.heap(), v) || matches!(self.heap().try_get(v), Some(HeapObj::Str(_)));
        if !own { return Err(VmErr::TypeMsg(crate::s!("unsupported format string passed to ", str &self.type_repr_name(v), ".__format__"))); }
        crate::vm::format_spec::format_typed(v, spec, self.type_name(v), self.heap())
    }

    /* Python `bool()` semantics, try `__bool__`, then `__len__` (0 = False), else default True for instances. Pass-through for built-in types. */
    fn truthy_op(&mut self, v: Val, chunk: &Self::Code) -> Result<bool, VmErr> {
        if self.instance_class(v).is_none() {
            let t = self.truthy(v);
            if t && v.is_heap() && self.heap().is_not_implemented(v) { return Err(cold_type("NotImplemented should not be used in a boolean context")); }
            return Ok(t);
        }
        if let Some(r) = self.try_call_dunder(v, "__bool__", &[], chunk)? {
            if !matches!(r, x if x.is_bool()) {
                return Err(VmErr::TypeMsg(crate::s!("__bool__ should return bool, returned ", str self.type_name(r))));
            }
            return Ok(r.as_bool());
        }
        if let Some(r) = self.try_call_dunder(v, "__len__", &[], chunk)? {
            return self.len_to_bool(r);
        }
        Ok(self.truthy(v))
    }

    fn call_len(&mut self, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        let o = self.pop()?;
        // An instance's `__len__` runs as a frame of the loop.
        if ok && self.is_instance(o) && self.check_job(o, crate::vm::jobs::Check::Len) { return Ok(()); }
        // instance `__len__` takes precedence over built-in length rules.
        if let Some(r) = self.try_call_dunder(o, "__len__", &[], chunk)? {
            let n = if r.is_int() { r.as_int() as i128 }
            else if let Some(i) = crate::vm::types::as_i128(r, self.heap()) { i }
            else { return Err(cold_type("__len__ must return int")); };
            if n < 0 { return Err(cold_value("__len__() should return >= 0")); }
            let v = self.int_to_val(Some(n))?;
            self.push(v);
            return Ok(());
        }
        let n = self.builtin_len(o)?;
        let v = self.int_to_val(Some(n))?;
        self.push(v); Ok(())
    }

    fn call_str(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        if argc == 0 { return self.alloc_and_push_str(alloc::string::String::new()); }
        if argc >= 2 {
            // `str(bytes, encoding[, errors])` decodes, mirroring `bytes.decode`.
            let args = self.pop_n(argc as usize)?;
            for (&a, name) in args[1..].iter().zip(["encoding", "errors"]) {
                if !matches!(self.heap().try_get(a), Some(HeapObj::Str(_))) {
                    return Err(VmErr::TypeMsg(crate::s!("str() argument '", str name, "' must be str, not ", str self.type_name(a))));
                }
            }
            if self.heap().bytes_of(args[0]).is_some() {
                return crate::vm::methods::bytes::decode(self, args[0], &args[1..]);
            }
            return Err(VmErr::TypeMsg(crate::s!("decoding to str: need a bytes-like object, ", str self.type_name(args[0]), " found")));
        }
        let o = self.pop()?;
        // A container writes its items with repr, as `repr` does, and an instance its `__str__` or `__repr__`, each run as frames of the loop.
        if ok && o.is_heap() && self.render_job(o, false, chunk)? { return Ok(()); }
        if ok && self.is_instance(o) && self.str_job(o, false) { return Ok(()); }
        let s = self.display_op(o, chunk)?;
        self.alloc_and_push_str(s)
    }

    fn call_bool(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        if argc == 0 { self.push(Val::bool(false)); return Ok(()); }
        let o = self.pop()?;
        // An instance's `__bool__`, or past it its `__len__`, runs as a frame of the loop.
        if ok && self.is_instance(o) {
            let class = self.instance_class(o).unwrap_or(Val::undef());
            let has_bool = self.lookup_class_member(class, "__bool__").is_some();
            if if has_bool { self.check_job(o, crate::vm::jobs::Check::Bool) } else { self.check_job(o, crate::vm::jobs::Check::LenTruth) } { return Ok(()); }
        }
        let t = self.truthy_op(o, chunk)?;
        self.push(Val::bool(t));
        Ok(())
    }

    fn call_repr(&mut self, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        let o = self.pop()?;
        // A container of instances whose `__repr__` runs as frames of the loop writes there, as an instance's own does.
        if ok && o.is_heap() && self.render_job(o, false, chunk)? { return Ok(()); }
        if ok && self.is_instance(o) && self.check_job(o, crate::vm::jobs::Check::Str(true)) { return Ok(()); }
        let s = self.repr_op(o, chunk)?;
        self.alloc_and_push_str(s)
    }

    /* `ascii(x)`, the repr of `x` with every non-ASCII character escaped. */
    fn call_ascii(&mut self, chunk: &Self::Code) -> Result<(), VmErr> {
        let o = self.pop()?;
        let s = self.repr_op(o, chunk)?;
        self.alloc_and_push_str(crate::vm::format_spec::ascii_escape(&s))
    }

    fn call_callable(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        let result = self.is_callable(o);
        self.push(Val::bool(result));
        Ok(())
    }

    /* Whether `o` can be called, an instance or a value a class made only when its class defines `__call__`. */
    fn is_callable(&self, o: Val) -> bool {
        if let Some(cls) = self.instance_class(o) { return self.lookup_class_member(cls, "__call__").is_some(); }
        matches!(self.heap().try_get(o), Some(HeapObj::Func(..) | HeapObj::BoundMethod(..) | HeapObj::Type(_) | HeapObj::NativeFn(_)
            | HeapObj::Class(..) | HeapObj::BoundUserMethod(..) | HeapObj::Extern(_) | HeapObj::PropertyPart(..)
            | HeapObj::StaticMethod(_) | HeapObj::GenericAlias(..)))
    }

    /* The name `type(x)` reports, a user instance's own class, an exception's concrete class, else the builtin type name. */
    fn type_repr_name(&self, o: Val) -> alloc::string::String {
        if o.is_heap() {
            match self.heap().get(o) {
                // Exception instances report their concrete class (e.g. `ZeroDivisionError`).
                HeapObj::ExcInstance(n, ..) => return n.clone(),
                _ => if let Some(cls) = self.instance_class(o) && let HeapObj::Class(n, _, _) = self.heap().get(cls) {
                    return n.clone();
                },
            }
        }
        self.type_name(o).to_string() // interned, so shares the `set`/`int` singleton
    }

    // `format(value [, spec])`.
    fn call_format(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        if op != 1 && op != 2 {
            return Err(cold_type("format() takes 1 or 2 arguments"));
        }
        let spec_val = if op == 2 { Some(self.pop()?) } else { None };
        let val = self.pop()?;
        let spec = match spec_val.map(|sv| self.heap().try_get(sv)) {
            None => String::new(),
            Some(Some(HeapObj::Str(s))) => s.clone(),
            Some(_) => return Err(VmErr::TypeMsg(crate::s!("format() argument 2 must be str, not ", str self.type_name(spec_val.unwrap_or_else(Val::none))))),
        };
        let result = self.format_op(val, &spec, chunk)?;
        self.alloc_and_push_str(result)
    }

    // `slice(stop)` | `slice(start, stop)` | `slice(start, stop, step)`, builtin, usable as a sequence index.
    fn call_slice(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let (start, stop, step) = match args.as_slice() {
            [stop] => (Val::none(), *stop, Val::none()),
            [start, stop] => (*start, *stop, Val::none()),
            [start, stop, step] => (*start, *stop, *step),
            _ => return Err(cold_type("slice() takes 1 to 3 arguments")),
        };
        let v = self.heap_mut().alloc(HeapObj::Slice(start, stop, step))?;
        self.push(v); Ok(())
    }

    /* `bytes_fromhex(s)`, the `bytes.fromhex` classmethod as a function. */
    fn call_bytes_fromhex(&mut self) -> Result<(), VmErr> {
        let s = self.pop()?;
        crate::vm::methods::bytes::fromhex(self, Val::none(), &[s])
    }

    /* `int_from_bytes(b, byteorder)`, the `int.from_bytes` classmethod as a function. */
    fn call_int_from_bytes(&mut self) -> Result<(), VmErr> {
        let order = self.pop()?;
        let b = self.pop()?;
        if !matches!(self.heap().try_get(b), Some(HeapObj::Bytes(_))) { return Err(cold_type("int_from_bytes() first arg must be bytes")); }
        crate::vm::methods::numeric::from_bytes(self, Val::none(), &[b, order])
    }

    // `int_to_bytes(n, length, byteorder)`, the `int.to_bytes` method as a function.
    fn call_int_to_bytes(&mut self) -> Result<(), VmErr> {
        let args = self.pop_n(3)?;
        crate::vm::methods::numeric::to_bytes(self, args[0], &args[1..])
    }

    /* Coerce a `__len__` / `__length_hint__` return value to bool semantics and reject negatives. */
    fn len_to_bool(&self, v: Val) -> Result<bool, VmErr> {
        let n = if v.is_int() { v.as_int() as i128 }
        else if let Some(i) = crate::vm::types::as_i128(v, self.heap()) { i }
        else { return Err(cold_type("__len__ must return int")); };
        if n < 0 { return Err(cold_value("__len__() should return >= 0")); }
        Ok(n != 0)
    }

    // Materialise an iterable to a list, strings -> chars, ranges eager, coroutines drained.
    fn call_list(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        if argc == 0 { return self.alloc_and_push_list(Vec::new()); } // `list()` is the empty list.
        let o = self.pop()?;
        // A generator, or a user iterator's `__next__`, called where a frame may open runs as a frame of the loop, its items gathered as they come.
        if ok && self.may_job(o) && self.items_job(o, crate::vm::jobs::Takes::All(OpCode::CallList, 1), chunk)? { return Ok(()); }
        // user-defined iterable wins over the built-in dispatch.
        if let Some(items) = self.iter_to_vec_op(o, chunk)? {
            return self.alloc_and_push_list(items);
        }
        let items = self.extract_iter(o)?;
        self.alloc_and_push_list(items)
    }

    fn call_tuple(&mut self, argc: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        if argc == 0 { return self.alloc_and_push_tuple(Vec::new()); } // `tuple()` is the empty tuple.
        let o = self.pop()?;
        if ok && self.may_job(o) && self.items_job(o, crate::vm::jobs::Takes::All(OpCode::CallTuple, 1), chunk)? { return Ok(()); }
        if let Some(items) = self.iter_to_vec_op(o, chunk)? {
            return self.alloc_and_push_tuple(items);
        }
        let items = self.extract_iter(o)?;
        self.alloc_and_push_tuple(items)
    }

    // Operand packs kw<<8 | pos, keep counts distinct.
    fn call_dict(&mut self, op: u16) -> Result<(), VmErr> {
        let pos = (op & 0xFF) as usize;
        let kw = (op >> 8) as usize;
        if pos > 1 {
            return Err(cold_type("dict expected at most 1 argument"));
        }
        // Keyword pairs sit above the positional source.
        let ok = self.take_job_ok();
        let kw_flat = self.pop_n(kw * 2)?;
        let src = if pos == 1 { Some(self.pop()?) } else { None };
        // A generator called where a frame may open runs as a frame of the loop, its pairs gathered as it yields them.
        if ok && kw == 0 && let Some(s) = src && s.is_heap() && self.gather_gen(s, crate::vm::jobs::Takes::All(crate::parser::OpCode::CallDict, op), &[])? { return Ok(()); }
        // A dict source copies with the hashes it stored.
        if let Some(rc) = src.and_then(|s| self.heap().dict_cell(s)) && kw_flat.is_empty() {
            let dm = rc.borrow().clone();
            return self.alloc_and_push_dict(dm);
        }
        let mut pairs = match src { Some(s) => self.pairs_of(s)?, None => Vec::new() };
        pairs.extend(kw_flat.chunks(2).map(|p| (p[0], p[1])));
        let dm = self.dictmap_of(pairs)?;
        self.alloc_and_push_dict(dm)
    }

    /* `frozendict()` as `dict()` builds it, a frozendict alone given back as itself. */
    fn call_frozendict(&mut self, op: u16) -> Result<(), VmErr> {
        if op == 1 && let Some(top) = self.last() && matches!(self.heap().try_get(top), Some(HeapObj::FrozenDict(_))) { return Ok(()); }
        self.call_dict(op)?;
        let made = self.last().ok_or_else(|| cold_runtime("stack underflow"))?;
        if let Some(slot) = self.heap_mut().try_get_mut(made) && let HeapObj::Dict(rc) = slot { *slot = HeapObj::FrozenDict(rc.clone()); }
        Ok(())
    }

    fn call_set(&mut self, op: u16) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        let src = if op == 0 { None } else { Some(self.pop()?) };
        if ok && let Some(s) = src && s.is_heap() && self.gather_gen(s, crate::vm::jobs::Takes::All(crate::parser::OpCode::CallSet, 1), &[])? { return Ok(()); }
        let s = self.set_source(src)?;
        let val = self.heap_mut().alloc(HeapObj::Set(Rc::new(RefCell::new(s))))?;
        self.push(val);
        Ok(())
    }

    /* The items `set(src)` or `frozenset(src)` holds, a set source copies the hashes it stored. */
    fn set_source(&mut self, src: Option<Val>) -> Result<ValSet, VmErr> {
        let Some(src) = src else { return Ok(ValSet::new()) };
        match self.heap().try_get(src) {
            Some(HeapObj::Set(rc)) => return Ok(rc.borrow().clone()),
            Some(HeapObj::FrozenSet(rc)) => return Ok((**rc).clone()),
            _ => {}
        }
        let items = self.extract_iter(src)?;
        self.valset_of(&items)
    }

    /* `frozenset()` | `frozenset(iter)`, construct an immutable, hashable set from an iterable. Without args returns the empty frozenset. */
    fn call_frozenset(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        if args.len() > 1 { return Err(cold_type("frozenset() takes 0 or 1 argument")); }
        let s = self.set_source(args.first().copied())?;
        let v = self.heap_mut().alloc(HeapObj::FrozenSet(Rc::new(s)))?;
        self.push(v); Ok(())
    }

    /* `bytes()`, empty, `n` zero bytes, an iterable of ints in range(256), a bytes-like value or `(str, encoding, errors)`. */
    #[inline(never)]
    fn call_bytes(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let buf = self.octets_of(&args, "bytes")?;
        let v = self.heap_mut().alloc(HeapObj::Bytes(buf))?;
        self.push(v); Ok(())
    }

    /* `bytearray(...)`, the arguments `bytes()` takes, a mutable copy. */
    fn call_bytearray(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let buf = self.octets_of(&args, "bytearray")?;
        let v = self.heap_mut().alloc(HeapObj::ByteArray(buf))?;
        self.push(v); Ok(())
    }

    /* The octets the arguments of `bytes()` or `bytearray()` name. */
    fn octets_of(&mut self, args: &[Val], what: &str) -> Result<Vec<u8>, VmErr> {
        Ok(match *args {
            [] => Vec::new(),
            [a] if a.is_int() || a.is_bool() || crate::frontend::int::is_big(self.heap(), a) => {
                let n = self.as_i128(a).unwrap_or(i128::MAX);
                if n < 0 { return Err(cold_value("negative count")); }
                // Length is user-controlled, cap it against the memory left so a huge count errors instead of aborting.
                // Past a 64-bit ssize_t it overflows as CPython's does, short of that no address space holds it.
                let n = isize::try_from(n).map_err(|_| if n > i64::MAX as i128 { cold_overflow() } else { cold_heap() })? as usize;
                self.heap().reserve(n)?;
                alloc::vec![0u8; n]
            }
            [a] if self.heap().bytes_of(a).is_some() => self.heap().bytes_of(a).unwrap_or_default().to_vec(),
            [a] if matches!(self.heap().try_get(a), Some(HeapObj::Str(_))) => return Err(VmErr::TypeMsg(crate::s!("string argument without an encoding"))),
            [a] => {
                let items = self.extract_iter(a).map_err(|_| VmErr::TypeMsg(crate::s!("cannot convert '", str self.type_name(a), "' object to ", str what)))?;
                // `bytes` names its range in the plural, `bytearray` in the singular.
                items.into_iter().map(|v| self.byte_value(v).map_err(|e| if what == "bytes" && matches!(e, VmErr::Value(_)) { cold_value("bytes must be in range(0, 256)") } else { e })).collect::<Result<Vec<u8>, VmErr>>()?
            }
            [text, ref rest @ ..] if rest.len() <= 2 => {
                for (&a, name) in rest.iter().zip(["encoding", "errors"]) {
                    if !matches!(self.heap().try_get(a), Some(HeapObj::Str(_))) {
                        return Err(VmErr::TypeMsg(crate::s!(str what, "() argument '", str name, "' must be str, not ", str self.type_name(a))));
                    }
                }
                let Some(HeapObj::Str(t)) = self.heap().try_get(text) else { return Err(cold_type("encoding without a string argument")) };
                let (encoding, errors) = crate::vm::methods::string::codec_args(self, rest)?;
                crate::vm::codecs::encode(t, encoding, errors)?
            }
            _ => return Err(VmErr::TypeMsg(crate::s!(str what, "() takes at most 3 arguments"))),
        })
    }

    /* Entries of a mapping, or of an iterable of two-item iterables, as `dict()` takes them. */
    fn pairs_of(&mut self, src: Val) -> Result<Vec<(Val, Val)>, VmErr> {
        if let Some(rc) = self.heap().dict_cell(src) { self.heap().spend(rc.borrow().len()); return Ok(rc.borrow().iter().collect()); }
        let mut pairs = Vec::new();
        for item in self.extract_iter(src)? {
            let item = self.extract_iter(item).map_err(|e| match e {
                VmErr::TypeMsg(_) => VmErr::TypeMsg("object is not iterable".into()),
                e => e,
            })?;
            let [k, v] = item[..] else {
                return Err(VmErr::Raised(crate::s!("ValueError: dictionary update sequence element #", int pairs.len() as i64, " has length ", int item.len() as i64, "; 2 is required")));
            };
            self.require_key(k)?;
            self.keyed(k)?;
            pairs.push((k, v));
        }
        Ok(pairs)
    }

    /* An iterator over `o`, the one `iter(o)` gives. */
    fn iter_of(&mut self, o: Val, chunk: &Self::Code) -> Result<Val, VmErr> {
        let ascii = self.heap_mut().str_is_ascii(o);
        if self.heap().classed(o).is_some() && let Some(it) = self.try_call_dunder(o, "__iter__", &[], chunk)? { return Ok(it); }
        let name = match self.heap().try_get(o) {
            Some(HeapObj::Coroutine(..)) if self.is_async_object(o) => return Err(self.not_iterable(o)),
            Some(HeapObj::Iter(..) | HeapObj::Coroutine(..)) => return Ok(o),
            Some(HeapObj::Instance(..)) => return self.try_call_dunder(o, "__iter__", &[], chunk)?.ok_or_else(|| self.not_iterable(o)),
            Some(HeapObj::List(rc)) => { let rc = rc.clone(); return self.alloc_iterator(IterFrame::List { rc, idx: 0 }, "list_iterator"); }
            Some(&HeapObj::Range(cur, end, step)) => return self.alloc_iterator(IterFrame::Range { cur, end, step }, "range_iterator"),
            Some(&HeapObj::BigRange(cur, end, step)) => return self.alloc_iterator(IterFrame::BigRange { cur, end, step }, "range_iterator"),
            Some(HeapObj::Dict(_) | HeapObj::FrozenDict(_)) => return self.alloc_iterator(self.dict_walk(o, View::Keys), "dict_keyiterator"),
            Some(&HeapObj::DictView(d, kind)) => {
                let name = match kind { View::Keys => "dict_keyiterator", View::Values => "dict_valueiterator", View::Items => "dict_itemiterator" };
                return self.alloc_iterator(self.dict_walk(d, kind), name);
            }
            Some(HeapObj::Tuple(_)) => "tuple_iterator",
            Some(HeapObj::Str(_)) => if ascii { "str_ascii_iterator" } else { "str_iterator" },
            Some(HeapObj::Set(_) | HeapObj::FrozenSet(_)) => "set_iterator",
            Some(HeapObj::Bytes(_)) => "bytes_iterator",
            Some(HeapObj::ByteArray(_)) => "bytearray_iterator",
            Some(HeapObj::MemoryView(_)) => "memory_iterator",
            _ => return Err(self.not_iterable(o)),
        };
        let items = self.extract_iter(o)?;
        let frame = self.watched(o, items);
        self.alloc_iterator(frame, name)
    }

    /* Turns the stack values from `at` into iterators in place, each rooted while the next is made. */
    fn iters_in_place(&mut self, at: usize, chunk: &Self::Code) -> Result<(), VmErr> {
        for i in at..self.stack().len() {
            let v = self.stack()[i];
            let it = self.iter_of(v, chunk)?;
            self.stack()[i] = it;
        }
        Ok(())
    }

    fn call_enumerate(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let (positional, kw_flat) = self.parse_call_args(op)?;
        if positional.is_empty() || positional.len() > 2 {
            return Err(cold_type("enumerate() takes 1 or 2 positional arguments"));
        }
        // `start` is positional (`enumerate(xs, 5)`) or keyword (`enumerate(xs, start=5)`), default 0.
        let mut start = if positional.len() == 2 { positional[1] } else { Val::int(0) };
        for pair in kw_flat.as_chunks::<2>().0 {
            match self.kw_name(pair[0]) {
                Some("start") => start = pair[1],
                _ => return Err(cold_type("enumerate() got an unexpected keyword argument")),
            }
        }
        if self.as_i128(start).is_none() { return Err(cold_type("enumerate() start must be an integer")); }
        let it = self.with_roots([positional[0], start], |vm| vm.iter_of(positional[0], chunk))?;
        self.push_iterator(IterFrame::Enumerate { it, n: start }, "enumerate")
    }

    /* Pairs the items of N iterables into tuples as they are asked for, stopping at the shortest. */
    fn call_zip(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        // `strict=` is the one keyword, its pair above the iterables.
        let (pos, kw) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let mut strict = false;
        for pair in self.pop_n(2 * kw)?.chunks(2) {
            if self.kw_name(pair[0]) != Some("strict") {
                return Err(VmErr::TypeMsg(s!("zip() got an unexpected keyword argument '", str self.kw_name(pair[0]).unwrap_or(""), "'")));
            }
            strict = self.truthy(pair[1]);
        }
        let at = self.stack().len().checked_sub(pos).ok_or_else(|| cold_runtime("stack underflow"))?;
        self.iters_in_place(at, chunk)?;
        let its = self.pop_n(pos)?;
        self.push_iterator(IterFrame::Zip { its: its.into(), strict }, "zip")
    }

    /* `reversed(seq)`, a list read live from its end, a range stepped backwards, other sequences read up front. */
    #[inline(never)]
    fn call_reversed(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        let name = match self.heap().try_get(o) {
            Some(HeapObj::List(rc)) => {
                let (rc, idx) = (rc.clone(), rc.borrow().len());
                return self.push_iterator(IterFrame::ListRev { rc, idx }, "list_reverseiterator");
            }
            Some(&HeapObj::Range(start, end, step)) => {
                let n = crate::vm::eq::range_len(start, end, step);
                let (last, stop) = ((start as i128) + (n - 1) * step as i128, start as i128 - step as i128);
                // A range whose backward bounds leave i64 reads its items up front.
                if let (true, Some(back), Ok(cur), Ok(end)) = (n > 0, step.checked_neg(), i64::try_from(last), i64::try_from(stop)) {
                    return self.push_iterator(IterFrame::Range { cur, end, step: back }, "range_iterator");
                }
                "range_iterator"
            }
            Some(HeapObj::BigRange(..)) => {
                let (cur, end, step) = crate::frontend::int::range_reversed(self.heap_mut(), o).unwrap_or(Err(VmErr::Overflow))?;
                return self.push_iterator(IterFrame::BigRange { cur, end, step }, "range_iterator");
            }
            Some(HeapObj::Str(_) | HeapObj::Tuple(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_)) => "reversed",
            Some(HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::DictView(_, View::Keys)) => "dict_reversekeyiterator",
            Some(HeapObj::DictView(_, View::Values)) => "dict_reversevalueiterator",
            Some(HeapObj::DictView(_, View::Items)) => "dict_reverseitemiterator",
            _ => return Err(VmErr::TypeMsg(s!("'", str self.type_name(o), "' object is not reversible"))),
        };
        let mut items = self.extract_iter(o)?;
        items.reverse();
        let frame = self.watched(o, items);
        self.push_iterator(frame, name)
    }

    fn call_divmod(&mut self) -> Result<(), VmErr> {
        let b = self.pop()?;
        let a = self.pop()?;
        let q = self.floor_op(BinOp::FloorDiv, a, b, "divmod()")?;
        let r = self.floor_op(BinOp::Mod, a, b, "divmod()")?;
        self.alloc_and_push_tuple(alloc::vec![q, r])
    }

    #[inline(never)]
    fn call_pow(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        match args.len() {
            2 => {
                let r = self.pow_vals(args[0], args[1], "pow() requires numeric operands")?;
                self.push(r);
                Ok(())
            }
            3 => {
                // A modulus under 2^63 keeps every product in i128, so small operands skip the boxed ints.
                if let (Some(b), Some(e), Some(m)) = (self.as_i128(args[0]), self.as_i128(args[1]), self.as_i128(args[2]))
                    && m != 0 && m.unsigned_abs() < 1 << 63 {
                    let (am, mut acc, mut b, mut e) = (m.abs(), 1i128, b, e);
                    b = b.rem_euclid(am);
                    // A negative exponent raises the inverse of the base, which needs it coprime to the modulus.
                    if e < 0 {
                        let (mut r, mut r1, mut t, mut t1) = (am, b, 0i128, 1i128);
                        while r1 != 0 { let q = r / r1; (r, r1, t, t1) = (r1, r - q * r1, t1, t - q * t1); }
                        if r != 1 { return Err(cold_value("base is not invertible for the given modulus")); }
                        (b, e) = (t.rem_euclid(am), -e);
                    }
                    while e > 0 { if e & 1 == 1 { acc = acc * b % am; } b = b * b % am; e >>= 1; }
                    let r = acc % am;
                    let r = self.heap_mut().int(if m < 0 && r != 0 { r - am } else { r })?;
                    self.push(r);
                    return Ok(());
                }
                let r = crate::frontend::int::pow_mod(self.heap_mut(), args[0], args[1], args[2])
                    .ok_or_else(|| cold_type("pow() 3rd argument not allowed unless all arguments are integers"))??;
                self.push(r);
                Ok(())
            }
            _ => Err(cold_type("pow() takes 2 or 3 arguments")),
        }
    }

    // Folds plain items here and leaves the rest of the fold in `rest`.
    fn fold_ready(&mut self, o: Val, takes: Takes, extra: &[Val], chunk: &Self::Code, rest: &mut Option<Folded>) -> Result<(), VmErr> {
        // An iterator steps as asked, so the fold stops at the item that decides it, and a container is read through its cursor.
        let lazy = matches!(self.heap().try_get(o), Some(HeapObj::Iter(..) | HeapObj::Coroutine(..) | HeapObj::Instance(..)));
        let (co, mut cur) = if lazy { (self.iter_of(o, chunk)?, None) } else { (o, Some(self.iter_cursor(o)?)) };
        let mut j = self.gather_of(takes, extra.to_vec(), co);
        let (mut first, mut held) = (None, None);
        if let Some(c) = cur.as_mut() {
            // Ints of a machine word add with no look at the job.
            if let (Takes::Sum, Total::Word(w)) = (j.takes, j.total) {
                let (heap, budget) = self.heap_budget();
                let (total, stop) = sum_words(c, w, heap, budget);
                (j.total, held) = (Total::Word(total), stop);
            }
            while first.is_none() {
                let next = match held.take() { Some(v) => Some(v), None => c.next_item(self.heap_mut())? };
                match next {
                    None => { let r = self.fold_end(&mut j)?; self.push(r); return Ok(()); }
                    Some(v) if *self.budget_mut() != 0 && !self.heap().has_work() && take_plain(&mut j, v, self.heap()) => *self.budget_mut() -= 1,
                    // An item that is no instance, the best or total no instance either, takes no user code either.
                    Some(v) if !self.is_instance(v) && !self.is_instance(j.acc) && match j.takes {
                        // Two lists or tuples whose walk meets an instance compare through its dunders.
                        Takes::Extreme(_) => !matches!(self.pair_scan(v, j.acc, 0), Some(PairScan::Dunder(_))),
                        Takes::Sum | Takes::Any | Takes::Every => true,
                        Takes::Keyed(_, k) => matches!(self.heap().try_get(k), Some(HeapObj::NativeFn(_))),
                        _ => false,
                    } => {
                        self.charge_step()?;
                        match j.takes {
                            Takes::Sum => (j.total, j.acc) = self.sum_on(j.total, j.acc, v, chunk)?,
                            Takes::Extreme(is_min) => if j.acc.is_undef() || self.beats(v, j.acc, is_min, chunk)? { j.acc = v; },
                            // A builtin key runs at once, the keys it gives held while any `__lt__` of theirs compares them.
                            Takes::Keyed(is_min, k) => {
                                self.push(k);
                                self.push(v);
                                self.exec_call(1, chunk)?;
                                let kv = self.pop()?;
                                let best = j.best_key;
                                if best.is_undef() || self.with_roots([kv, best, j.acc, v], |vm| vm.beats(kv, best, is_min, chunk))? { (j.acc, j.best_key) = (v, kv); }
                            }
                            t => if self.truthy(v) == (t == Takes::Any) { self.push(Val::bool(t == Takes::Any)); return Ok(()); },
                        }
                    }
                    Some(v) => first = Some(v),
                }
            }
            let left = *self.budget_mut();
            self.heap().set_budget(left);
        }
        *rest = Some(Folded { j, cur, first, lazy, co });
        Ok(())
    }

    /* The result of the builtin of `j` once its generator returned `ret`, its items taken out of it. */
    // Total, best or default once the items end.
    fn fold_end(&mut self, j: &mut Gather) -> Result<Val, VmErr> {
        match j.takes {
            Takes::Any => Ok(Val::bool(false)),
            Takes::Every => Ok(Val::bool(true)),
            Takes::Sum => Ok(match j.total {
                Total::Word(w) => return self.int_to_val(Some(w as i128)),
                Total::Float(s, c) => Val::float(super::globals::numeric::settle((s, c))),
                Total::Any => j.acc,
            }),
            Takes::Extreme(is_min) | Takes::Keyed(is_min, _) if j.acc.is_undef() => match j.extra.first() {
                // A `default` given is the result of an empty iterable.
                Some(&d) => Ok(d),
                None => Err(crate::vm::cold_value(if is_min { "min() iterable argument is empty" } else { "max() iterable argument is empty" })),
            },
            Takes::Extreme(_) | Takes::Keyed(..) => Ok(j.acc),
            _ => Err(cold_runtime("a gather that is no fold")),
        }
    }

    /* A job for the builtin `takes` stands for over `of`, a generator or a user iterator, `extra` its arguments after. */
    fn gather_of(&self, takes: Takes, extra: Vec<Val>, of: Val) -> Gather {
        let mut j = Gather::new(takes, extra);
        // What the items come from stays held while user code the builtin runs between them collects.
        j.co = of;
        if takes == Takes::Sum {
            // The start the items add to, 0 unless passed.
            let start = j.extra.first().copied().unwrap_or(Val::int(0));
            let word = if start.is_int() || start.is_foreign() || start.is_bool() { self.as_i128(start).and_then(|n| i64::try_from(n).ok()) } else { None };
            j.total = match word { Some(w) => Total::Word(w), None if start.is_float() => Total::Float(start.as_float(), 0.0), None => Total::Any };
            j.acc = start;
        }
        j
    }

    /* The total of a `sum` that went as far as `total`, `acc` its value past the floats, once `v` adds to it. */
    fn sum_on(&mut self, total: Total, acc: Val, v: Val, chunk: &Self::Code) -> Result<(Total, Val), VmErr> {
        let word = |vm: &Self, v: Val| if v.is_int() || v.is_bool() || v.is_foreign() { vm.as_i128(v).and_then(|n| i64::try_from(n).ok()) } else { None };
        let (total, acc) = match total {
            Total::Word(w) => match word(self, v).and_then(|b| w.checked_add(b)) {
                Some(n) => (Total::Word(n), acc),
                None => {
                    let a = self.int_to_val(Some(w as i128))?;
                    let a = self.sum_through(a, v, chunk)?;
                    if a.is_float() { (Total::Float(a.as_float(), 0.0), a) } else { (Total::Any, a) }
                }
            },
            Total::Float(s, c) if v.is_float() => { let (s, c) = super::globals::numeric::neumaier(s, c, v.as_float()); (Total::Float(s, c), acc) }
            Total::Float(s, c) => match word(self, v) {
                Some(b) => (Total::Float(s + b as f64, c), acc),
                None => (Total::Any, self.sum_through(Val::float(super::globals::numeric::settle((s, c))), v, chunk)?),
            },
            Total::Any => (Total::Any, self.sum_through(acc, v, chunk)?),
        };
        Ok((total, acc))
    }

    /* `a + b` for `sum`, both rooted while a user `__add__` runs. */
    fn sum_through(&mut self, a: Val, b: Val, chunk: &Self::Code) -> Result<Val, VmErr> {
        self.with_roots([a, b], |vm| vm.sum_add(a, b, chunk))
    }

    /* One step of `sum`, through a user `__add__` or `__radd__` when either side defines one. */
    fn sum_add(&mut self, a: Val, b: Val, chunk: &Self::Code) -> Result<Val, VmErr> {
        match self.try_binary_dunder(crate::parser::types::OpCode::Add, a, b, false, chunk)? {
            Some(v) => Ok(v),
            None => self.add_vals(crate::value::unboxed(a, self.heap()), crate::value::unboxed(b, self.heap())),
        }
    }

    /* Whether `item < best` for min or `item > best` for max, raising with the operator that ran. */
    #[inline]
    fn beats(&mut self, item: Val, best: Val, is_min: bool, chunk: &Self::Code) -> Result<bool, VmErr> {
        if item.is_int() && best.is_int() { return Ok(if is_min { item.as_int() < best.as_int() } else { item.as_int() > best.as_int() }); }
        if is_min { return self.sort_lt(item, best, chunk); }
        if let Some(r) = self.try_compare_dunder(crate::parser::OpCode::Gt, item, best, chunk)? { return Ok(self.truthy(r)); }
        match self.values_lt(best, item, chunk) {
            Err(VmErr::TypeMsg(_)) => Err(VmErr::TypeMsg(crate::s!("'>' not supported between instances of '", str self.type_name(item), "' and '", str self.type_name(best), "'"))),
            r => r,
        }
    }

    /* Index of the least key, or with `!is_min` the greatest, the first one on a tie. */
    fn extreme_index(&mut self, keys: &[Val], is_min: bool, chunk: &Self::Code) -> Result<usize, VmErr> {
        let mut best = 0;
        for i in 1..keys.len() {
            if self.beats(keys[i], keys[best], is_min, chunk)? { best = i; }
        }
        Ok(best)
    }

    fn call_min(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> { self.call_minmax(op, true, chunk) }

    fn call_max(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> { self.call_minmax(op, false, chunk) }

    fn call_minmax(&mut self, op: u16, is_min: bool, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        let (positional, kw_flat) = self.parse_call_args(op)?;
        // Optional `default=` (returned when a single iterable is empty) and `key=` (compare by key(x)).
        let mut default: Option<Val> = None;
        let mut key: Option<Val> = None;
        for pair in kw_flat.as_chunks::<2>().0 {
            match self.kw_name(pair[0]) {
                Some("default") => default = Some(pair[1]),
                Some("key") => { if !pair[1].is_none() { key = Some(pair[1]); } }
                _ => return Err(cold_type("min()/max() got an unexpected keyword argument")),
            }
        }
        let label = if is_min { "min() iterable argument is empty" } else { "max() iterable argument is empty" };
        // An `eval` or `exec` key runs each text as a floor of the loop.
        if ok && let Some(k) = key && self.runs_text(k) {
            // An iterable that may run Python has its items gathered by a job of the loop first, the call run again on them.
            if let [o] = positional[..] && matches!(self.heap().try_get(o), Some(HeapObj::Instance(..) | HeapObj::Coroutine(..) | HeapObj::Iter(..))) {
                let base = self.stack().len();
                self.push(o);
                for &v in &kw_flat { self.push(v); }
                let id = if is_min { NativeFnId::Min } else { NativeFnId::Max };
                if self.gather_call(id, op, base, chunk)? { return Ok(()); }
                let o = self.stack()[base];
                self.stack().truncate(base);
                let items = self.extract_iter(o)?;
                if items.is_empty() { return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) }; }
                return self.keys_floor(k, items, is_min);
            }
            let items = match positional[..] { [o] => self.extract_iter(o)?, _ => positional };
            if items.is_empty() { return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) }; }
            return self.keys_floor(k, items, is_min);
        }
        // A builtin key stepping a generator among the items in Rust runs through a function of the loop, as a job stepping it, any iterable's items unknown yet.
        if ok && let Some(k) = key && let Some(held) = self.held_key(k, &positional)? {
            key = Some(held);
            // Values whose key now runs as frames fold as one iterable does.
            if positional.len() > 1 {
                let all = self.heap_mut().alloc(HeapObj::Tuple(positional))?;
                return self.with_roots([all], |vm| vm.fold_run(all, crate::vm::jobs::Takes::Keyed(is_min, held), &[], ok, chunk));
            }
        }
        // One iterable folds, an item at a time, so a range never becomes a list.
        if let [o] = positional[..] {
            let takes = match key { Some(k) => crate::vm::jobs::Takes::Keyed(is_min, k), None => crate::vm::jobs::Takes::Extreme(is_min) };
            return self.fold_run(o, takes, default.as_slice(), ok, chunk);
        }
        // Many args are values.
        let items = positional;
        if items.is_empty() {
            return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) };
        }
        // Values whose comparison or key may run user code fold as one iterable does, its dunders frames of the loop.
        if self.heap().users && (items.iter().any(|v| v.is_heap()) || key.is_some_and(|k| self.heap().user(k))) {
            let takes = match key { Some(k) => crate::vm::jobs::Takes::Keyed(is_min, k), None => crate::vm::jobs::Takes::Extreme(is_min) };
            let all = self.heap_mut().alloc(HeapObj::Tuple(items))?;
            return self.with_roots([all], |vm| vm.fold_run(all, takes, &[], ok, chunk));
        }
        // Without a key, compare elements directly, with one, compare key(x) but return the winning element.
        let keys = match key {
            Some(k) => Some(self.call_rows(k, core::slice::from_ref(&items), items.len(), chunk)?),
            None => None,
        };
        let keys = keys.as_deref().unwrap_or(&items);
        // An instance key runs `__lt__` like `sorted`, so then the operands stay rooted.
        let roots = if self.any_instance(keys) { items.len() + keys.len() } else { 0 };
        let best = self.with_roots(items.iter().chain(keys).copied().take(roots), |vm| vm.extreme_index(keys, is_min, chunk))?;
        self.push(items[best]); Ok(())
    }

    fn call_sum(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        let args = self.pop_n(op as usize)?;
        if args.is_empty() { return Err(cold_type("sum() requires at least 1 argument")); }
        // Text and bytes join, which sum points to.
        match args.get(1).and_then(|&start| self.heap().try_get(start)) {
            Some(HeapObj::Str(_)) => return Err(cold_type("sum() can't sum strings [use ''.join(seq) instead]")),
            Some(HeapObj::Bytes(_)) => return Err(cold_type("sum() can't sum bytes [use b''.join(seq) instead]")),
            Some(HeapObj::ByteArray(_)) => return Err(cold_type("sum() can't sum bytearray [use b''.join(seq) instead]")),
            _ => {}
        }
        // Each item adds as it comes, as CPython 3.15 adds it.
        self.fold_run(args[0], crate::vm::jobs::Takes::Sum, &args[1..], ok, chunk)
    }

    // a < b via __lt__ when either side defines it, else the built-in comparison.
    #[inline]
    fn sort_lt(&mut self, a: Val, b: Val, chunk: &Self::Code) -> Result<bool, VmErr> {
        if let Some(r) = self.try_compare_dunder(OpCode::Lt, a, b, chunk)? {
            return Ok(self.truthy(r));
        }
        self.values_lt(a, b, chunk)
    }

    /* Steps `o` lazily when a range or list, so `all`, `any` and `sum` stop early. */
    #[inline(always)]
    fn iter_cursor(&mut self, o: Val) -> Result<IterFrame, VmErr> {
        Ok(match self.heap().try_get(o) {
            Some(&HeapObj::Range(cur, end, step)) => IterFrame::Range { cur, end, step },
            Some(&HeapObj::BigRange(cur, end, step)) => IterFrame::BigRange { cur, end, step },
            Some(HeapObj::List(rc)) => IterFrame::List { rc: rc.clone(), idx: 0 },
            _ => IterFrame::Seq { items: self.extract_iter(o)?.into(), idx: 0 },
        })
    }

    /* Short-circuit truthiness scan shared by `all`/`any`, stopping at the first item whose truth is `find` and pushing it, `!find` once spent. */
    fn scan_truthy(&mut self, op: u16, find: bool, arity_err: &'static str, chunk: &Self::Code) -> Result<(), VmErr> {
        let ok = self.take_job_ok();
        if op != 1 { return Err(cold_type(arity_err)); }
        let o = self.pop()?;
        self.fold_run(o, if find { crate::vm::jobs::Takes::Any } else { crate::vm::jobs::Takes::Every }, &[], ok, chunk)
    }

    fn call_all(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> { self.scan_truthy(op, false, "all() takes exactly 1 argument", chunk) }

    fn call_any(&mut self, op: u16, chunk: &Self::Code) -> Result<(), VmErr> { self.scan_truthy(op, true, "any() takes exactly 1 argument", chunk) }

    /* Walks the pairs of `a` and `b` from `from` as `PairScan` says, items read as they are now, None unless both are builtin lists or both builtin tuples. */
    fn pair_scan(&self, a: Val, b: Val, from: usize) -> Option<PairScan> {
        let (xs, ys): (&[Val], &[Val]) = match (self.heap().plain(a)?, self.heap().plain(b)?) {
            (HeapObj::Tuple(x), HeapObj::Tuple(y)) => (x, y),
            (HeapObj::List(x), HeapObj::List(y)) => return Some(scan_pairs(&x.borrow(), &y.borrow(), from, self.heap())),
            _ => return None,
        };
        Some(scan_pairs(xs, ys, from, self.heap()))
    }
    /* `sorted` with no key, its `__lt__` run as frames of the loop when `ok`, called where one may open. */
    fn sorted_of(&mut self, reverse: bool, ok: bool, chunk: &Self::Code) -> Result<(), VmErr> {
        let o = self.pop()?;
        let items = self.extract_iter(o)?;
        let mut items = if ok { match self.merge_job(items, reverse, Val::undef()) { Ok(()) => return Ok(()), Err(items) => items } } else { items };
        self.sort_by_lt(&mut items, reverse, chunk)?;
        self.alloc_and_push_list(items)
    }

    /* In-place sort dispatching `__lt__`, roots items since a comparison can run user code that GCs. */
    fn sort_by_lt(&mut self, items: &mut [Val], reverse: bool, chunk: &Self::Code) -> Result<(), VmErr> {
        let snapshot = items.to_vec();
        let order = self.sorted_order(&snapshot, &[], reverse, chunk)?;
        for (dst, &src) in order.iter().enumerate() { items[dst] = snapshot[src]; }
        Ok(())
    }

    /* Root the operands (comparators can run GC-triggering user code), sort `keys` the way Python does through `sort_lt`, and return the index permutation. `extra_roots` keeps caller-only values (e.g. the items in a keyed sort) alive across comparisons. The first comparison error ends the sort. */
    fn sorted_order(&mut self, keys: &[Val], extra_roots: &[Val], reverse: bool, chunk: &Self::Code) -> Result<Vec<usize>, VmErr> {
        if let Some(order) = super::globals::sequence::plain_order(keys, reverse, self.heap()) { return Ok(order); }
        let mut rooted = 0;
        // Only an instance key runs user code that could collect.
        if self.any_instance(keys) { for &v in keys.iter().chain(extra_roots) { self.root(v); rooted += 1; } }
        // A descending sort reverses, sorts ascending and reverses back, so equal keys keep their order.
        let mut order: Vec<usize> = if reverse { (0..keys.len()).rev().collect() } else { (0..keys.len()).collect() };
        let sorted = crate::vm::timsort::sort(&mut order, &mut |x, y| self.sort_lt(keys[x], keys[y], chunk));
        if reverse { order.reverse(); }
        let sort_err = sorted.err();
        self.unroot(rooted);
        if let Some(e) = sort_err { return Err(e); }
        Ok(order)
    }

    // One zip step and a strict zip raises where lengths differ.
    fn zip_step(&mut self, its: &[Val], strict: bool, chunk: &Self::Code) -> Result<Option<Val>, VmErr> {
        let base = self.stack().len();
        if its.is_empty() { return Ok(None); }
        if !strict {
            if !self.push_next_of(its, chunk)? { return Ok(None); }
        } else {
            // A strict zip ends only when every iterable ends together.
            for (k, &it) in its.iter().enumerate() {
                match self.next_of(it, chunk)? {
                    Some(v) => self.push(v),
                    None if k > 0 => { self.stack().truncate(base); return Err(zip_uneven(k, false)); }
                    None => {
                        for (j, &rest) in its.iter().enumerate().skip(1) {
                            if let Some(extra) = self.next_of(rest, chunk)? { let _ = extra; return Err(zip_uneven(j, true)); }
                        }
                        return Ok(None);
                    }
                }
            }
        }
        let items = self.stack().split_off(base);
        self.heap_mut().alloc(HeapObj::Tuple(items)).map(Some)
    }

    // One enumerate step moving the count on in `frame`.
    fn enumerate_step(&mut self, frame: Option<&Rc<RefCell<IterFrame>>>, it: Val, n: Val, chunk: &Self::Code) -> Result<Option<Val>, VmErr> {
        let Some(item) = self.next_of(it, chunk)? else { return Ok(None) };
        self.push(item);
        let next = self.int_to_val(self.as_i128(n).and_then(|i| i.checked_add(1)))?;
        if let Some(frame) = frame && let IterFrame::Enumerate { n: count, .. } = &mut *frame.borrow_mut() { *count = next; }
        let item = self.pop()?;
        self.heap_mut().alloc(HeapObj::Tuple(alloc::vec![n, item])).map(Some)
    }
    /* Pushes the next item of each iterator, false with nothing pushed once one is spent. */
    fn push_next_of(&mut self, its: &[Val], chunk: &Self::Code) -> Result<bool, VmErr> {
        let base = self.stack().len();
        for &it in its {
            match self.next_of(it, chunk)? {
                Some(v) => self.push(v),
                None => { self.stack().truncate(base); return Ok(false); }
            }
        }
        Ok(true)
    }
}

impl<H: Frames> Builtins for H {}

/* The error of a strict zip whose argument `k` ran out, or ran on, against those before it. */
#[cold]
fn zip_uneven(k: usize, longer: bool) -> VmErr {
    let before = if k == 1 { alloc::string::String::from("argument 1") } else { s!("arguments 1-", int k as i64) };
    let (n, how) = (k as i64 + 1, if longer { " is longer than " } else { " is shorter than " });
    cold_value_s(s!("zip() argument ", int n, str how, str &before))
}

// Adds machine word ints from `c` and gives back the first other item.
#[inline(always)]
fn sum_words(c: &mut IterFrame, mut w: i64, heap: &HeapPool, budget: &mut u64) -> (i64, Option<Val>) {
    let mut left = *budget;
    let mut stop = None;
    while left != 0 && let Some(v) = c.next_ready(heap) {
        match v.is_int().then(|| w.checked_add(v.as_int())).flatten() {
            Some(n) => { w = n; left -= 1; }
            None => { stop = Some(v); break; }
        }
    }
    *budget = left;
    (w, stop)
}
