use super::*;

impl<'a> VM<'a> {

    /* StoreName does a single SSA slot write after register coalescing. */
    pub(crate) fn handle_store(&mut self, operand: u16) -> Result<(), VmErr> {
        let v = self.pop()?;
        // Malformed bytecode can carry an out-of-range slot, so drop the write rather than panic.
        if let Some(s) = self.regs.get_mut(self.base + operand as usize) { *s = v; }
        Ok(())
    }

    /* Container constructors for list / tuple / dict / set / slice / string. */
    pub(crate) fn handle_build(&mut self, op: OpCode, operand: u16) -> Result<(), VmErr> {
        match op {
            OpCode::BuildList => {
                let v = self.pop_n(operand as usize)?;
                let val = self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(v))))?;
                self.push(val);
            }
            OpCode::BuildTuple => {
                let v = self.pop_n(operand as usize)?;
                let val = self.heap.alloc(HeapObj::Tuple(v))?;
                self.push(val);
            }
            OpCode::BuildDict => {
                let flat = self.pop_n(operand as usize * 2)?;
                let dm = self.dictmap_of(flat.chunks(2).map(|c| (c[0], c[1])).collect())?;
                let val = self.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(dm))))?;
                self.push(val);
            }
            OpCode::BuildString => {
                let parts = self.pop_n(operand as usize)?;
                let s: String = parts.iter().map(|v| self.display(*v)).collect();
                let val = self.heap.alloc(HeapObj::Str(s))?;
                self.push(val);
            }
            OpCode::BuildSet => self.build_set(operand)?,
            OpCode::BuildSlice => self.build_slice(operand)?,
            _ => return Err(cold_runtime("non-build opcode in handle_build")),
        }
        Ok(())
    }

    /* Unpacking and `{value!s:spec}` formatting. Indexed get/store/del are dispatched directly from the hot loop, never here. */
    pub(crate) fn handle_container(&mut self, op: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        match op {
            OpCode::UnpackSequence => self.unpack_iterable(operand as usize, None, chunk)?,
            OpCode::UnpackEx => self.unpack_iterable((operand >> 8) as usize, Some((operand & 0xFF) as usize), chunk)?,
            OpCode::FormatValue => {
                /* Operand layout is bit 0 has_spec, bits 1..=2 conversion (0 none, 1 !r, 2 !s, 3 !a). See parser/literals.rs. */
                let has_spec = (operand & 1) != 0;
                let conv = (operand >> 1) & 0b11;
                let spec_val = if has_spec { Some(self.pop()?) } else { None };
                let v = self.pop()?;

                // Conversions run user dunders with the spec rooted and charge the length of the text.
                let converted = self.with_roots(spec_val, |vm| {
                    let s = match conv {
                        1 => vm.repr_op(v, chunk)?,
                        2 => vm.display_op(v, chunk)?,
                        3 => crate::vm::format_spec::ascii_escape(&vm.repr_op(v, chunk)?),
                        _ => return Ok(v),
                    };
                    vm.charge_steps(s.len())?;
                    vm.heap.alloc(HeapObj::Str(s))
                })?;

                let spec = match spec_val.map(|sv| (sv, self.heap.try_get(sv))) {
                    None => String::new(),
                    Some((_, Some(HeapObj::Str(s)))) => s.clone(),
                    // A spec of one bare field arrives as the inline value it named.
                    Some((sv, None)) if !sv.is_undef() => self.display(sv),
                    Some(_) => return Err(cold_type("format spec must be a string")),
                };
                let result = self.format_op(converted, &spec, chunk)?;
                let val = self.heap.alloc(HeapObj::Str(result))?;
                self.push(val);
            }
            _ => return Err(cold_runtime("non-container opcode in handle_container")),
        }
        Ok(())
    }

    /* Append/add to the comprehension accumulator at the top of the stack. */
    pub(crate) fn handle_comprehension(&mut self, op: OpCode) -> Result<(), VmErr> {
        let value = self.pop()?;
        let key = if op == OpCode::MapAdd { Some(self.pop()?) } else { None };
        let Some(&acc) = self.stack.last() else { return Err(VmErr::Runtime("stack underflow")) };
        match (op, key, self.heap.try_get(acc)) {
            (OpCode::ListAppend, _, Some(HeapObj::List(rc))) => rc.borrow_mut().push(value),
            (OpCode::SetAdd, _, Some(HeapObj::Set(rc))) => { self.require_element(value)?; rc.borrow_mut().insert(value, &self.heap); }
            (OpCode::MapAdd, Some(k), Some(HeapObj::Dict(rc))) => { self.require_key(k)?; rc.borrow_mut().insert(k, value, &self.heap); }
            _ => return Err(cold_runtime("comprehension accumulator corrupted")),
        }
        Ok(())
    }

    /* Merge the source on top of the stack into the container below it for `{**m}`, `{*s}`, `[*it]`. */
    pub(crate) fn handle_spread_merge(&mut self, op: OpCode, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let src = self.pop()?;
        let Some(&acc) = self.stack.last() else { return Err(VmErr::Runtime("stack underflow")) };
        if !acc.is_heap() { return Err(cold_runtime("spread accumulator corrupted")); }
        match op {
            // `**` requires a mapping, and later keys overwrite earlier ones.
            OpCode::DictUpdate => self.dict_spread_into(acc, src)?,
            OpCode::SetUpdate => {
                if !matches!(self.heap.get(acc), HeapObj::Set(_)) { return Err(cold_runtime("spread accumulator corrupted")); }
                self.spread_into(acc, src, chunk)?;
            }
            OpCode::ListExtend => {
                let items = self.iterable_items(src, chunk)?;
                match self.heap.get(acc) {
                    HeapObj::List(rc) => rc.borrow_mut().extend(items),
                    _ => return Err(cold_runtime("spread accumulator corrupted")),
                }
            }
            _ => return Err(cold_runtime("non-spread opcode in handle_spread_merge")),
        }
        Ok(())
    }

    /* Side-effecting / impure ops, assert, del, global/nonlocal, import, type alias, raise, await. */
    pub(crate) fn handle_side(&mut self, op: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        match op {
            OpCode::Assert => {
                let v = self.pop()?;
                if !self.truthy_op(v, chunk)? {
                    // Bare `assert` raises a catchable AssertionError with empty args.
                    let inst = self.heap.alloc(HeapObj::ExcInstance("AssertionError".into(), Vec::new(), Val::undef()))?;
                    self.pending.exc_val = Some(inst);
                    return Err(VmErr::Raised("AssertionError".into()));
                }
            }
            OpCode::Del => {
                let slot = operand as usize;
                // Deleting an already-unbound name raises NameError, matching Python.
                match self.regs.get_mut(self.base + slot) {
                    Some(s) if !s.is_undef() => *s = Val::undef(),
                    _ => return Err(self.unbound_err(chunk, slot)),
                }
            }
            OpCode::Global | OpCode::Nonlocal => self.mark_impure(),
            OpCode::Raise | OpCode::RaiseFrom => {
                self.mark_impure();
                // Bare `raise` (operand 1) re-raises the exception currently being handled.
                if op == OpCode::Raise && operand == 1 {
                    let Some(&(exc, at)) = self.handling.last() else {
                        return Err(VmErr::Runtime("No active exception to reraise"));
                    };
                    let name = self.exc_type_name(exc);
                    self.pending.exc_val = Some(exc);
                    self.error_byte_pos = at;
                    return Err(VmErr::Raised(name));
                }
                // RaiseFrom emits both `expr` then `from expr`, the topmost value is the cause, but the exception to raise is the LHS.
                let cause = if op == OpCode::RaiseFrom { self.pop()? } else { Val::undef() };
                let exc = self.pop()?;
                let err = self.with_roots([cause], |vm| vm.raise_value(exc, chunk));
                // A bad cause raises its own TypeError in place of the exception.
                if op == OpCode::RaiseFrom && let Some(e) = self.pending.exc_val && let Err(bad) = self.with_roots([e], |vm| vm.link_cause(e, cause, chunk)) {
                    self.pending.exc_val = None;
                    return Err(bad);
                }
                return Err(err);
            }
            OpCode::Await => {
                // Coroutine parks on it (single-driver) so the top loop runs it to completion, even across suspension. Sync values pass through.
                let val = self.pop()?;
                if val.is_heap() && matches!(self.heap.get(val), HeapObj::Coroutine(..)) {
                    self.await_coroutine(val)?;
                } else {
                    self.push(val);
                }
            }
            _ => return Err(cold_runtime("non-side opcode in handle_side")),
        }
        Ok(())
    }

    /* The error `raise exc` raises, a class made into its instance, the instance kept for `except as`. */
    fn raise_value(&mut self, mut exc: Val, chunk: &SSAChunk) -> VmErr {
        // The exception being handled raised again keeps the line it was first raised at.
        if let Some(&(handled, at)) = self.handling.last() && handled.0 == exc.0 {
            self.error_byte_pos = at;
        }
        // A class deriving from an exception raises an instance of itself made with no arguments.
        if matches!(self.heap.try_get(exc), Some(HeapObj::Class(..))) && self.exc_base(exc).is_some() {
            self.push(exc);
            if let Err(e) = self.exec_call(0, chunk) { return e; }
            exc = match self.pop() { Ok(v) => v, Err(e) => return e };
        }
        // A user exception reports its class name and `str(e)`, its own `__str__` included.
        if let Some(&HeapObj::Instance(cls, _)) = self.heap.try_get(exc) && self.exc_base(cls).is_some() {
            let name = self.exc_type_name(exc);
            let text = match self.with_roots([exc], |vm| vm.display_op(exc, chunk)) { Ok(t) => t, Err(e) => return e };
            self.pending.exc_val = Some(exc);
            return VmErr::Raised(if text.is_empty() { name } else { crate::s!(str &name, ": ", str &text) });
        }
        // Stash the Val for `except as e` binding, with non-Exc values using `display()`.
        self.pending.exc_val = None;
        // Extract owned (class name, instance with args) so display() can run after the heap borrow ends.
        let info: Option<(alloc::string::String, Option<Val>)> = if exc.is_heap() {
            match self.heap.get(exc) {
                HeapObj::ExcInstance(n, args, _) => {
                    self.pending.exc_val = Some(exc);
                    Some((n.clone(), (!args.is_empty()).then_some(exc)))
                }
                HeapObj::Type(n) => {
                    // Bare `raise X` builds an empty ExcInstance so `e.args` is `()`.
                    let n = n.clone();
                    let inst = match self.heap.alloc(HeapObj::ExcInstance(n.clone(), Vec::new(), Val::undef())) { Ok(v) => v, Err(e) => return e };
                    self.pending.exc_val = Some(inst);
                    Some((n, None))
                }
                _ => None,
            }
        } else {
            None
        };
        // Append the str of the instance so an uncaught traceback reads "Class: message".
        let msg = match info {
            Some((n, Some(arg))) => { let detail = self.display(arg); crate::s!(str &n, ": ", str &detail) }
            Some((n, None)) => n,
            // Non-exception value (str, int, ...) raises TypeError, catchable by `except Exception`.
            None => crate::s!("TypeError: exceptions must derive from BaseException"),
        };
        VmErr::Raised(msg)
    }
}
