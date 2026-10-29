use crate::vm::VM;
use crate::value::{Val, VmErr, HeapObj, HeapPool, DictMap, Host, cold_runtime};
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;
use crate::lang::{Cx, Mode};
use crate::code::Ins;
use crate::value::*;
use crate::vm::lower;
use crate::vm::slow::item::sure_key;

impl<'a> VM<'a> {

    /* the list tuple dict string set or slice build op makes of items */
    #[inline(always)]
    pub(crate) fn build_vals(&mut self, op: OpCode, items: Vec<Val>) -> Result<Val, VmErr> {
        let obj = match op {
            OpCode::BuildList => HeapObj::List(Rc::new(RefCell::new(items))),
            OpCode::BuildTuple => HeapObj::Tuple(items),
            OpCode::BuildDict => HeapObj::Dict(Rc::new(RefCell::new(self.dictmap_of_flat(&items)?))),
            OpCode::BuildString => {
                let mut text = String::new();
                for &v in &items { text.push_str(&self.render(v, Mode::Str)?); }
                HeapObj::Str(text)
            }
            OpCode::BuildSet => HeapObj::Set(Rc::new(RefCell::new(self.valset_of(&items)?))),
            OpCode::BuildSlice if items.len() >= 2 => return self.heap.new_slice([items[0], items[1], items.get(2).copied().unwrap_or(Val::none())]),
            _ => return Err(cold_runtime("non-build opcode in build_vals")),
        };
        self.heap.alloc(obj)
    }

    /* unpacking and field formatting while the hot loop dispatches indexed access itself */
    pub(crate) fn handle_container(&mut self, op: OpCode, operand: u16, rip: usize) -> Result<(), VmErr> {
        match op {
            OpCode::UnpackSequence => self.unpack_iterable(operand as usize, None, Some(rip))?,
            OpCode::UnpackEx => self.unpack_iterable((operand >> 8) as usize, Some((operand & 0xFF) as usize), Some(rip))?,
            OpCode::FormatValue => {
                let spec = if operand & 1 != 0 { Some(self.pop()?) } else { None };
                let v = self.pop()?;
                let val = self.format_val(operand, v, spec)?;
                self.push(val);
            }
            _ => return Err(cold_runtime("non-container opcode in handle_container")),
        }
        Ok(())
    }

    /* field v formatted as the operand says with its spec when bit 0 is set */
    pub(crate) fn format_val(&mut self, operand: u16, v: Val, spec_val: Option<Val>) -> Result<Val, VmErr> {
        let mode = match (operand >> 1) & 0b11 { 1 => Some(Mode::Repr), 2 => Some(Mode::Str), 3 => Some(Mode::Ascii), _ => None };
        // conversions keep the spec rooted and charge for the length of the text
        let converted = match mode {
            None => v,
            Some(mode) => self.with_roots(spec_val, |vm| {
                let s = vm.render(v, mode)?;
                vm.charge_steps(s.len())?;
                vm.heap.alloc(HeapObj::Str(s))
            })?,
        };
        let spec = match spec_val.map(|sv| (sv, self.heap.try_get(sv))) {
            None => String::new(),
            Some((_, Some(HeapObj::Str(s)))) => s.clone(),
            // a spec of one bare field arrives as the plain value it named
            Some((sv, None)) if !sv.is_undef() => self.render(sv, Mode::Str)?,
            Some(_) => return Err(crate::lang::EngineError::cannot(crate::lang::Act::Spec, None)),
        };
        let result = (self.rt.format)(&mut Cx::new(self), converted, &spec)?;
        self.heap.alloc(HeapObj::Str(result))
    }

    /* appends or adds to the comprehension accumulator at the top of the stack */
    pub(crate) fn handle_comprehension(&mut self, op: OpCode) -> Result<(), VmErr> {
        let value = self.pop()?;
        let key = if op == OpCode::MapAdd { self.pop()? } else { Val::undef() };
        let acc = self.slot(self.under(1)?);
        self.accumulated(op, acc, key, value)
    }

    /* adds `value` under `key` for a map to the accumulator `acc` as op `op` does */
    pub(crate) fn accumulated(&mut self, op: OpCode, acc: Val, key: Val, value: Val) -> Result<(), VmErr> {
        match (op, self.heap.try_get(acc)) {
            (OpCode::ListAppend, Some(HeapObj::List(rc))) => rc.borrow_mut().push(value),
            (OpCode::SetAdd, Some(HeapObj::Set(rc))) => { let rc = rc.clone(); self.require_element(value)?; rc.borrow_mut().insert(value, &self.heap); }
            (OpCode::MapAdd, Some(HeapObj::Dict(rc))) if !key.is_undef() => { let rc = rc.clone(); self.require_key(key)?; rc.borrow_mut().insert(key, value, &self.heap); }
            _ => return Err(cold_runtime("comprehension accumulator corrupted")),
        }
        Ok(())
    }

    /* merges the stack top into the container below it for `{**m}` and `{*s}` and `[*it]` */
    pub(crate) fn handle_spread_merge(&mut self, op: OpCode, rip: usize) -> Result<(), VmErr> {
        let src = self.pop()?;
        let acc = self.slot(self.under(1)?);
        if !acc.is_heap() { return Err(cold_runtime("spread accumulator corrupted")); }
        match op {
            // `**` requires a mapping and later keys overwrite earlier ones
            OpCode::DictUpdate => self.dict_spread_into(acc, src)?,
            OpCode::SetUpdate => {
                if !matches!(self.heap.get(acc), HeapObj::Set(_)) { return Err(cold_runtime("spread accumulator corrupted")); }
                let src = if matches!(self.heap.try_get(src), Some(HeapObj::Iter(..))) { match self.lazy_items(src, rip)? { Some(v) => v, None => return Ok(()) } } else { src };
                self.spread_into(acc, src)?;
            }
            OpCode::ListExtend => {
                // a `map` or `filter` running user code appends as a job handing the list back
                if src.is_heap() && matches!(self.heap.get(src), HeapObj::Iter(..)) {
                    self.pop()?;
                    if self.extend_job(acc, src)? { return Ok(()); }
                    self.push(acc);
                }
                let items = self.extract_iter(src)?;
                match self.heap.get(acc) {
                    // an empty list takes the items it is given with no second copy
                    HeapObj::List(rc) => {
                        let mut list = rc.borrow_mut();
                        if list.is_empty() { *list = items; } else { list.extend(items); }
                    }
                    _ => return Err(cold_runtime("spread accumulator corrupted")),
                }
            }
            _ => return Err(cold_runtime("non-spread opcode in handle_spread_merge")),
        }
        Ok(())
    }

    // an assert of `v` failing with the runtime's assert class and no arguments
    pub(crate) fn assert_holds(&mut self, v: Val) -> Result<(), VmErr> {
        if self.truthy(v) { return Ok(()); }
        let inst = self.heap.engine_error(crate::lang::ErrKind::Assert, Vec::new())?;
        self.pending.exc_val = Some(inst);
        Err(VmErr::Of(crate::lang::ErrKind::Assert, String::new()))
    }

    // the error a raise gives and with no value the handled exception raised again
    pub(crate) fn raise_of(&mut self, exc: Option<Val>, chunk: &SSAChunk) -> VmErr {
        match exc.or_else(|| self.handling.last().map(|h| h.0)) {
            Some(exc) => self.raise_value(exc, chunk),
            None => VmErr::Runtime("No active exception to reraise"),
        }
    }

    // the error a RaiseR at `rip` gives once the instance its class makes is back
    #[inline(never)]
    pub(crate) fn raise_reg(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk) -> VmErr {
        if ins.x == 1 { return self.raise_of(None, chunk); }
        if ins.x == crate::vm::lower::TOO_DEEP { return cold_runtime("code too large to run"); }
        let exc = match core::mem::replace(&mut self.retake, Val::undef()) { v if !v.is_undef() => v, _ => match self.reg(chunk, ins.b) { Ok(v) => v, Err(e) => return e } };
        self.raise_with(exc, rip, chunk)
    }

    // the error a stack raise at `rip` gives once its class's instance is back
    #[inline(never)]
    pub(crate) fn raise_stacked(&mut self, operand: u16, rip: usize, chunk: &SSAChunk) -> VmErr {
        if operand == 1 { return self.raise_of(None, chunk); }
        let at = match self.under(1) { Ok(at) => at, Err(e) => return e };
        // the raise runs again over the class whose init made the instance it takes back
        let exc = match core::mem::replace(&mut self.retake, Val::undef()) { v if v.is_undef() => self.slot(at), v => v };
        let e = self.raise_with(exc, rip, chunk);
        // the class keeps its row while its init runs as a frame
        if !matches!(e, VmErr::Redo) { self.cut_to(at); }
        e
    }

    // raising `exc` at `rip` or Redo once its class's init method opened as a frame
    fn raise_with(&mut self, exc: Val, rip: usize, chunk: &SSAChunk) -> VmErr {
        match self.raise_made(exc, rip, chunk) {
            Ok(true) => VmErr::Redo,
            Ok(false) => self.raise_of(Some(exc), chunk),
            Err(e) => e,
        }
    }

    // opens the init method of class `exc` as a frame whose instance the raise retakes
    fn raise_made(&mut self, exc: Val, rip: usize, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if !self.heap.is_class(exc) { return Ok(false); }
        let ctor = self.ctor_of(exc);
        let Some(init) = ctor.init.filter(|_| ctor.exception) else { return Ok(false) };
        if !matches!(self.heap.try_get(init), Some(HeapObj::Func { .. })) { return Ok(false); }
        let instance = self.new_instance(exc, ctor.attrs)?;
        self.set_exc_args(instance, Vec::new())?;
        (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
        let opened = self.with_roots([instance], |vm| vm.run_init(init, instance, &[], &[], chunk, true));
        (self.pending_exec_safe, self.inline_call) = (false, false);
        if !opened? { return Ok(false); }
        if self.entered { self.mark_dunder(rip, crate::vm::act::AGAIN); }
        Ok(true)
    }

    /* the error `raise exc` gives with a class made an instance kept for `except as` */
    pub(crate) fn raise_value(&mut self, mut exc: Val, chunk: &SSAChunk) -> VmErr {
        // the exception being handled raised again keeps the line it was first raised at
        if let Some(&(handled, at)) = self.handling.last() && handled.0 == exc.0 {
            self.error_byte_pos = at;
        }
        // a class deriving from an exception raises an instance of itself made with no arguments
        if self.heap.is_class(exc) && self.exc_base(exc).is_some() {
            exc = match self.call_with(exc, None, &[], &[], chunk).and_then(|v| self.got(v)) { Ok(v) => v, Err(e) => return e };
        }
        // a user exception reports its class name and `str(e)`
        if let Some(cls) = self.heap.class_of(exc) && self.heap.is_class(cls) && self.exc_base(cls).is_some() {
            let name = self.exc_type_name(exc);
            let text = match self.with_roots([exc], |vm| vm.render(exc, Mode::Str)) { Ok(t) => t, Err(e) => return e };
            self.pending.exc_val = Some(exc);
            return VmErr::Raised(if text.is_empty() { name } else { crate::s!(str &name, ": ", str &text) });
        }
        // a raised instance stays for its handler and a bare class makes an empty one
        self.pending.exc_val = None;
        // an instance of a builtin error keeps the arguments it was made with
        match self.heap.made_args(exc).and_then(|a| Some((self.heap.instance_type(exc)?, a))).filter(|_| self.heap.class_of(exc).is_some_and(|c| self.heap.is_type(c))) {
            Some((n, [])) => { let n = String::from(n); self.pending.exc_val = Some(exc); return VmErr::Raised(n); }
            Some(_) => {}
            None => match self.heap.type_object_name(exc).map(String::from) {
                Some(n) => {
                    let inst = match self.heap.error_of(exc, Vec::new()) { Ok(v) => v, Err(e) => return e };
                    self.pending.exc_val = Some(inst);
                    return VmErr::Raised(n);
                }
                // any other value raises a type error
                None => return crate::lang::EngineError::cannot(crate::lang::Act::Raise { throw: false }, None),
            },
        }
        self.pending.exc_val = Some(exc);
        // the message reads as the class name then the text of the instance
        let detail = self.shown(exc, Mode::Str);
        match self.heap.instance_type(exc) { Some(n) => VmErr::Raised(crate::s!(str n, ": ", str &detail)), None => VmErr::Raised(detail) }
    }
}

/* whether a bare field keeps v as is for its string to render */
#[inline(always)]
pub(crate) fn bare_field(heap: &HeapPool, v: Val) -> bool { !v.is_undef() && !v.is_foreign() && (!v.is_heap() || matches!(heap.try_get(v), Some(HeapObj::Str(_)))) }

/* what `f` makes of `v`'s items when it is a tuple or list */
#[inline(always)]
pub(crate) fn seq_items<R>(heap: &HeapPool, v: Val, f: impl FnOnce(&[Val]) -> R) -> Option<R> {
    match heap.try_get(v) {
        Some(HeapObj::Tuple(t)) => Some(f(t)),
        Some(HeapObj::List(l)) => Some(f(&l.borrow())),
        _ => None,
    }
}

impl<'a> VM<'a> {

    // unpacking a tuple or list of the right length kept out of the loop
    #[inline(never)]
    pub(crate) fn unpack_fast(&mut self, ins: Ins) -> bool {
        // the list frames gathered of a lazy source stands in for it now
        let retaken = self.retake;
        let (base, regs) = (self.base, &mut self.regs);
        let from = if ins.x != 0 { ins.c } else { ins.b };
        let src = if retaken.is_undef() { regs[base + from as usize] } else { retaken };
        // a pair goes to its two targets and a row takes the first item last
        let fits = if ins.x != 0 {
            seq_items(&self.heap, src, |items| match *items {
                [x, y] => { regs[base + ins.a as usize] = x; regs[base + ins.b as usize] = y; true }
                _ => false,
            }) == Some(true)
        } else {
            let at = base + ins.a as usize;
            seq_items(&self.heap, src, |items| {
                if items.len() != ins.c as usize { return false; }
                for (k, &v) in items.iter().rev().enumerate() { regs[at + k] = v; }
                true
            }) == Some(true)
        };
        if fits { self.retake = Val::undef(); }
        fits
    }

    /* the list or tuple of n registers from `from` or none for an unbound one */
    #[inline(always)]
    pub(crate) fn seq_reg(&mut self, list: bool, from: usize, n: usize) -> Option<Val> {
        let items = &self.regs[from..from + n];
        if items.iter().any(|v| v.is_undef()) { return None; }
        let obj = if list { HeapObj::List(Rc::new(RefCell::new(items.to_vec()))) } else { HeapObj::Tuple(items.to_vec()) };
        self.heap.alloc(obj).ok()
    }

    /* the string set dict or slice a BuildR makes or none for the handler */
    #[inline(never)]
    pub(crate) fn build_reg(&mut self, ins: Ins) -> Option<Val> {
        let n = ins.c as usize;
        let items = &self.regs[self.base + ins.b as usize..][..n];
        let obj = match OpCode::ALL[(ins.x & !lower::TAKEN) as usize] {
            OpCode::BuildString => {
                let mut s = String::new();
                let from = self.base + ins.b as usize;
                for k in from..from + n {
                    let p = self.regs[k];
                    match self.heap.try_get(p) {
                        Some(HeapObj::Str(part)) => s.push_str(part),
                        // a bare field leaves a plain value rendered only here
                        None if p.is_int() => s.push_str(itoa::Buffer::new().format(p.as_int())),
                        None if !p.is_undef() => match self.render(p, crate::lang::Mode::Str) { Ok(t) => s.push_str(&t), Err(_) => return None },
                        _ => return None,
                    }
                }
                HeapObj::Str(s)
            }
            _ if items.iter().any(|v| v.is_undef()) => return None,
            // keys that hash with no check leave a set or dict nothing to fail on
            OpCode::BuildSet if items.iter().all(|&v| sure_key(&self.heap, v)) => HeapObj::Set(Rc::new(RefCell::new(ValSet::from_vals(items, &self.heap)))),
            OpCode::BuildDict if items.iter().step_by(2).all(|&k| sure_key(&self.heap, k)) => {
                let mut d = DictMap::with_capacity(n / 2);
                for kv in items.as_chunks::<2>().0 { d.insert(kv[0], kv[1], &self.heap); }
                HeapObj::Dict(Rc::new(RefCell::new(d)))
            }
            OpCode::BuildSlice if n >= 2 => return self.heap.new_slice([items[0], items[1], items.get(2).copied().unwrap_or(Val::none())]).ok(),
            _ => return None,
        };
        self.heap.alloc(obj).ok()
    }

    /* adds `key` and `value` to the accumulator or false when the handler checks the key */
    pub(crate) fn accumulate(&mut self, ins: Ins, key: Val, value: Val) -> bool {
        let acc = if ins.x == 1 { self.regs[self.base + ins.a as usize] } else { let Ok(at) = self.under(1) else { return false }; self.slot(at) };
        if value.is_undef() { return false; }
        let plain = self.plain_key(key);
        match (ins.op, self.heap.try_get(acc)) {
            (OpCode::ListAppendR, Some(HeapObj::List(rc))) => rc.borrow_mut().push(value),
            (OpCode::SetAddR, Some(HeapObj::Set(rc))) if plain => { rc.borrow_mut().insert(key, &self.heap); }
            (OpCode::MapAddR, Some(HeapObj::Dict(rc))) if plain => rc.borrow_mut().insert(key, value, &self.heap),
            _ => return false,
        }
        true
    }

    // builds a set from `items` rejecting unhashable elements first
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    pub(crate) fn set_from_items(&mut self, items: alloc::vec::Vec<Val>) -> Result<Val, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        self.alloc_set_result(items)
    }

    pub fn build_set(&mut self, op: u16) -> Result<(), VmErr> {
        let items = self.pop_n(op as usize)?;
        let s = self.valset_of(&items)?;
        let val = self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))?;
        self.push(val); Ok(())
    }

    #[inline]
    pub fn build_slice(&mut self, op: u16) -> Result<(), VmErr> {
        let step = if op == 3 { self.pop()? } else { Val::none() };
        let stop = self.pop()?;
        let start = self.pop()?;
        let val = self.heap.new_slice([start, stop, step])?;
        self.push(val); Ok(())
    }
}
