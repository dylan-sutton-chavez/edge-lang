use super::*;
use crate::alloc::string::ToString;
use crate::value::View;

/* Single source of truth for opcode -> (forward, reflected) arithmetic dunder names. */
pub(crate) fn binary_dunder_names(op: OpCode) -> Option<(&'static str, &'static str)> {
    Some(match op {
        OpCode::Add => ("__add__", "__radd__"),
        OpCode::Sub => ("__sub__", "__rsub__"),
        OpCode::Mul => ("__mul__", "__rmul__"),
        OpCode::Div => ("__truediv__", "__rtruediv__"),
        OpCode::FloorDiv => ("__floordiv__", "__rfloordiv__"),
        OpCode::MatMul => ("__matmul__", "__rmatmul__"),
        OpCode::Mod => ("__mod__", "__rmod__"),
        OpCode::Pow => ("__pow__", "__rpow__"),
        OpCode::BitAnd => ("__and__", "__rand__"),
        OpCode::BitOr => ("__or__", "__ror__"),
        OpCode::BitXor => ("__xor__", "__rxor__"),
        OpCode::Shl => ("__lshift__", "__rlshift__"),
        OpCode::Shr => ("__rshift__", "__rrshift__"),
        _ => return None,
    })
}

/* The in-place dunder `a op= b` asks before the binary pair. */
pub(crate) fn inplace_dunder_name(op: OpCode) -> Option<&'static str> {
    Some(match op {
        OpCode::Add => "__iadd__", OpCode::Sub => "__isub__", OpCode::Mul => "__imul__",
        OpCode::Div => "__itruediv__", OpCode::FloorDiv => "__ifloordiv__", OpCode::Mod => "__imod__",
        OpCode::Pow => "__ipow__", OpCode::MatMul => "__imatmul__", OpCode::BitAnd => "__iand__",
        OpCode::BitOr => "__ior__", OpCode::BitXor => "__ixor__", OpCode::Shl => "__ilshift__",
        OpCode::Shr => "__irshift__",
        _ => return None,
    })
}

/* Same for comparisons, (forward, reflected). `__eq__` reflects to itself. `<` reflects to `>` and vice-versa. */
pub(crate) fn compare_dunder_names(op: OpCode) -> Option<(&'static str, &'static str)> {
    Some(match op {
        OpCode::Eq => ("__eq__", "__eq__"),
        OpCode::NotEq => ("__ne__", "__ne__"),
        OpCode::Lt => ("__lt__", "__gt__"),
        OpCode::LtEq => ("__le__", "__ge__"),
        OpCode::Gt => ("__gt__", "__lt__"),
        OpCode::GtEq => ("__ge__", "__le__"),
        _ => return None,
    })
}

impl<'a> VM<'a> {
    /* `recv.<name>(*args)` probes the instance method and invokes it with `self` prepended. `Some(v)` on return, `None` on miss / `NotImplemented` (triggers reflected/fallback dispatch), `Err` only on a raised dunder. */
    pub(crate) fn try_call_dunder(&mut self, recv: Val, name: &str, args: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        // Built-in types route through their native handlers, and dunder dispatch only fires on user instances.
        if !recv.is_heap() { return Ok(None); }
        let HeapObj::Instance(cls_val, _) = self.heap.get(recv) else { return Ok(None); };
        let cls_val = *cls_val;

        // Special methods resolve on the type's MRO, bypassing the instance __dict__, like Python.
        let Some((func, class)) = self.lookup_class_member(cls_val, name) else { return Ok(None); };
        // Only plain functions bind as methods, so data attributes never dispatch implicitly.
        if !(func.is_heap() && matches!(self.heap.get(func), HeapObj::Func(..))) { return Ok(None); }

        // Mirror `__init__` dispatch, depth guard before pushing so a recursive blow-up leaves no half-built frame.
        if self.depth >= self.max_calls { return Err(cold_depth()); }

        self.pending.method_binding = Some((class, recv));
        self.push(func);
        self.push(recv);
        for &a in args { self.push(a); }
        let argc = (1 + args.len()) as u16;
        self.exec_call(argc, chunk)?;

        let result = self.pop()?;
        if self.heap.is_not_implemented(result) { return Ok(None); }
        Ok(Some(result))
    }

    /* Class of an Instance, or `None` for built-in operands. Powers the subclass-first ordering rule. */
    #[inline]
    fn instance_class(&self, v: Val) -> Option<Val> {
        if !v.is_heap() { return None; }
        match self.heap.get(v) { HeapObj::Instance(c, _) => Some(*c), _ => None }
    }

    /* Ordered forward/reflected dunder dispatch, reflected (`b.rname(a)`) runs first when `type(b)` strictly subclasses `type(a)` so overrides win. Returns the first non-None result. */
    fn dispatch_reflected(&mut self, a: Val, b: Val, lname: &str, rname: &str, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let b_overrides = match (self.instance_class(a), self.instance_class(b)) {
            (Some(ac), Some(bc)) => ac.0 != bc.0 && self.heap.is_subclass(bc, ac),
            _ => false,
        };
        let calls: [(Val, &str, Val); 2] = if b_overrides {
            [(b, rname, a), (a, lname, b)]
        } else {
            [(a, lname, b), (b, rname, a)]
        };
        for (recv, name, arg) in calls {
            if let Some(r) = self.try_call_dunder(recv, name, &[arg], chunk)? { return Ok(Some(r)); }
        }
        Ok(None)
    }

    /* Binary arithmetic dunder dispatch with Python's subclass-first ordering, if `type(b)` is a strict subclass of `type(a)` the reflected op runs first so overrides win. */
    pub(crate) fn try_binary_dunder(&mut self, op: OpCode, a: Val, b: Val, inplace: bool, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        if self.instance_class(a).is_none() && self.instance_class(b).is_none() { return Ok(None); }
        // The dunder runs user code that can collect, so both operands stay rooted.
        self.with_roots([a, b], |vm| {
            if inplace && let Some(name) = inplace_dunder_name(op)
                && let Some(r) = vm.try_call_dunder(a, name, &[b], chunk)? {
                return Ok(Some(r));
            }
            let Some((lname, rname)) = binary_dunder_names(op) else { return Ok(None); };
            vm.dispatch_reflected(a, b, lname, rname, chunk)
        })
    }

    /* The dunder an arithmetic site settles on for the IC, `__iop__` when the class of `a` defines it at an in-place site. */
    pub(crate) fn site_dunder_name(&self, op: OpCode, a: Val, inplace: bool) -> Option<&'static str> {
        let defines = |n: &&str| self.instance_class(a).is_some_and(|c| self.lookup_class_member(c, n).is_some());
        inplace_dunder_name(op).filter(|n| inplace && defines(n)).or_else(|| binary_dunder_names(op).map(|(l, _)| l))
    }

    /* Comparison dunder dispatch. `__eq__` reflects to itself. `__ne__` falls back to `not __eq__`. `<` reflects to `>` and vice-versa. */
    #[inline]
    pub(crate) fn try_compare_dunder(&mut self, op: OpCode, a: Val, b: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        if !(a.is_heap() || b.is_heap()) || (self.instance_class(a).is_none() && self.instance_class(b).is_none()) { return Ok(None); }
        self.compare_dunder(op, a, b, chunk)
    }

    /* The comparison dunder of an instance operand, kept out of line so builtin compares stay inlined. */
    #[inline(never)]
    fn compare_dunder(&mut self, op: OpCode, a: Val, b: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let Some((lname, rname)) = compare_dunder_names(op) else { return Ok(None); };

        let Some(r) = self.with_roots([a, b], |vm| vm.dispatch_reflected(a, b, lname, rname, chunk))? else {
            // `!=` falls back to negated `__eq__` when `__ne__` is absent.
            if matches!(op, OpCode::NotEq)
                && let Some(eq) = self.try_compare_dunder(OpCode::Eq, a, b, chunk)? {
                return Ok(Some(Val::bool(!self.truthy(eq))));
            }
            return Ok(None);
        };
        Ok(Some(r))
    }

    /* Python `bool()` semantics, try `__bool__`, then `__len__` (0 = False), else default True for instances. Pass-through for built-in types. */
    pub(crate) fn truthy_op(&mut self, v: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if !v.is_heap() || !matches!(self.heap.get(v), HeapObj::Instance(..)) {
            return Ok(self.truthy(v));
        }
        if let Some(r) = self.try_call_dunder(v, "__bool__", &[], chunk)? {
            if !matches!(r, x if x.is_bool()) {
                return Err(cold_type("__bool__ should return bool"));
            }
            return Ok(r.as_bool());
        }
        if let Some(r) = self.try_call_dunder(v, "__len__", &[], chunk)? {
            return self.len_to_bool(r);
        }
        Ok(true)
    }

    /* Steps an iterator once, None once spent or when the value has no `__next__`. */
    /* What a swallowed StopIteration carries, the pending exception set back to `before` the step raised it. */
    pub(crate) fn swallow_stop(&mut self, before: Option<Val>) -> Val {
        let raised = core::mem::replace(&mut self.pending.exc_val, before).filter(|e| before.is_none_or(|b| b.0 != e.0));
        match raised.and_then(|e| self.heap.try_get(e)) { Some(HeapObj::ExcInstance(_, args, _)) => args.first().copied().unwrap_or(Val::none()), _ => Val::none() }
    }

    pub(crate) fn iter_next_proto(&mut self, iter: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        if matches!(self.heap.try_get(iter), Some(HeapObj::Iter(..))) { return self.iter_step(iter); }
        self.try_call_dunder(iter, "__next__", &[], chunk)
    }

    /* `in` operator prefers the container's `__contains__`. For built-in sequences with an instance item, iterate using `__eq__` so user equality is honoured. */
    pub(crate) fn contains_op(&mut self, container: Val, item: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if let Some(r) = self.try_call_dunder(container, "__contains__", &[item], chunk)? {
            return Ok(self.truthy(r));
        }

        // A values view compares each value, an items view finds the key and compares its value.
        match self.heap.try_get(container) {
            Some(&HeapObj::DictView(d, View::Values)) => {
                let values = self.view_items(d, View::Values)?;
                let t = self.heap.alloc(HeapObj::Tuple(values))?;
                return self.with_roots([t], |vm| vm.seq_contains(t, item, chunk));
            }
            Some(&HeapObj::DictView(d, View::Items)) => {
                let Some(HeapObj::Tuple(kv)) = self.heap.try_get(item) else { return Ok(false) };
                let &[k, v] = kv.as_slice() else { return Ok(false) };
                let found = match self.heap.get(d) { HeapObj::Dict(m) => m.borrow().get(&k, &self.heap).copied(), _ => None };
                let Some(x) = found else { self.require_hashable(k)?; return Ok(false) };
                return self.with_roots([x, v], |vm| vm.member_eq(x, v, chunk));
            }
            _ => {}
        }

        // A dict or set probes by hash.
        let probe = match self.heap.try_get(container) {
            Some(HeapObj::Dict(rc)) => Some((rc.borrow().contains_key(&item, &self.heap), false)),
            Some(&HeapObj::DictView(d, _)) => match self.heap.get(d) { HeapObj::Dict(rc) => Some((rc.borrow().contains_key(&item, &self.heap), false)), _ => None },
            Some(HeapObj::Set(rc)) => Some((rc.borrow().contains(item, &self.heap), true)),
            Some(HeapObj::FrozenSet(s)) => Some((s.contains(item, &self.heap), true)),
            Some(HeapObj::List(_) | HeapObj::Tuple(_)) => return self.seq_contains(container, item, chunk),
            _ => None,
        };
        // A miss still rejects an unhashable probe, a set probing as the frozenset it equals.
        if let Some((hit, set)) = probe {
            if !hit { if set { self.require_set_probe(item)?; } else { self.require_hashable(item)?; } }
            return Ok(hit);
        }

        // User instance container with `__iter__` walks via the iterator protocol, comparing items with `__eq__`.
        if container.is_heap() && matches!(self.heap.get(container), HeapObj::Instance(..))
            && let Some(iter) = self.try_call_dunder(container, "__iter__", &[], chunk)? {
            // A generator or builtin iterator is a sequence of its own.
            if !matches!(self.heap.try_get(iter), Some(HeapObj::Instance(..))) {
                return self.with_roots([item, iter], |vm| vm.contains_op(iter, item, chunk));
            }
            return self.with_roots([container, item, iter], |vm| loop {
                vm.charge_step()?;
                let before = vm.pending.exc_val;
                match vm.iter_next_proto(iter, chunk) {
                    Ok(Some(v)) => {
                        vm.temp_roots.push(v);
                        if vm.eq_op(item, v, chunk)? { return Ok(true); }
                    }
                    Ok(None) => return Ok(false),
                    Err(VmErr::Raised(ref m)) if m == "StopIteration" || m.starts_with("StopIteration:") => { vm.swallow_stop(before); return Ok(false) }
                    Err(e) => return Err(e),
                }
            });
        }

        // An iterator steps until a match, leaving the rest unconsumed.
        if matches!(self.heap.try_get(container), Some(HeapObj::Iter(..))) {
            return self.with_roots([container, item, Val::none()], |vm| loop {
                vm.charge_step()?;
                let Some(v) = vm.iter_step(container)? else { return Ok(false) };
                if let Some(slot) = vm.temp_roots.last_mut() { *slot = v; }
                if vm.eq_op(item, v, chunk)? { return Ok(true); }
            });
        }
        if matches!(self.heap.try_get(container), Some(HeapObj::Coroutine(..))) {
            return self.with_roots([container, item, Val::none()], |vm| loop {
                vm.charge_step()?;
                let v = vm.resume_coroutine(container)?;
                if !vm.yielded { return Ok(false); }
                vm.yielded = false;
                if let Some(slot) = vm.temp_roots.last_mut() { *slot = v; }
                if vm.eq_op(item, v, chunk)? { return Ok(true); }
            });
        }
        self.contains(container, item)
    }

    /* Member `==` for `contains_op`, identity first, then the dunder, then content equality. */
    pub(crate) fn eq_op(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if a.0 == b.0 { return Ok(true); }
        if let Some(r) = self.try_compare_dunder(OpCode::Eq, a, b, chunk)? { return Ok(self.truthy(r)); }
        self.values_eq(a, b, chunk)
    }

    /* `x in seq` read live, identity and content first, a user `__eq__` only where content cannot settle. */
    fn seq_contains(&mut self, seq: Val, item: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        // The scan counts the items it may read as native work.
        let scan = |items: &[Val], heap: &HeapPool| -> Result<bool, usize> {
            heap.spend(items.len());
            for (j, &x) in items.iter().enumerate() {
                if x.0 == item.0 { return Ok(true); }
                match eq_checked(x, item, heap, 0) { Some(true) => return Ok(true), Some(false) => {}, None => return Err(j) }
            }
            Ok(false)
        };
        let from = match self.heap.try_get(seq) {
            Some(HeapObj::List(rc)) => scan(&rc.borrow(), &self.heap),
            Some(HeapObj::Tuple(t)) => scan(t, &self.heap),
            _ => Ok(false),
        };
        let mut i = match from { Ok(hit) => return Ok(hit), Err(j) => j };
        self.with_roots([seq, item], |vm| loop {
            let x = match vm.heap.try_get(seq) {
                Some(HeapObj::List(rc)) => rc.borrow().get(i).copied(),
                Some(HeapObj::Tuple(t)) => t.get(i).copied(),
                _ => None,
            };
            let Some(x) = x else { return Ok(false) };
            if vm.member_eq(x, item, chunk)? { return Ok(true); }
            i += 1;
        })
    }

    /* Drive a user instance's `__iter__` result to a Vec, stepping a user `__next__` or draining a builtin-iterator list. Treats a missing `__iter__` as "no protocol" by returning `None`. Used by `list(custom)`, `tuple(custom)`, etc. */
    pub(crate) fn iter_to_vec_op(&mut self, obj: Val, chunk: &SSAChunk) -> Result<Option<Vec<Val>>, VmErr> {
        if !obj.is_heap() || !matches!(self.heap.get(obj), HeapObj::Instance(..)) { return Ok(None); }
        let Some(iter) = self.try_call_dunder(obj, "__iter__", &[], chunk)? else { return Ok(None); };
        // A builtin iterator or generator drains as itself, a user iterator steps `__next__`.
        if !matches!(self.heap.try_get(iter), Some(HeapObj::Instance(..))) { return self.extract_iter(iter).map(Some); }
        // Each `__next__` can run a collection, so the iterator and what it yielded stay rooted.
        self.with_roots([obj, iter], |vm| {
            let mut out = Vec::new();
            loop {
                vm.charge_step()?;
                let before = vm.pending.exc_val;
                match vm.iter_next_proto(iter, chunk) {
                    // Inline ints take no slot, so the list they fill is capped by the memory left.
                    Ok(Some(_)) if out.len().saturating_mul(VAL_BYTES) >= vm.heap.room() => return Err(crate::vm::cold_heap()),
                    Ok(Some(v)) => { vm.temp_roots.push(v); out.push(v); }
                    Ok(None) => return Ok(Some(out)),
                    Err(VmErr::Raised(ref m)) if m == "StopIteration" || m.starts_with("StopIteration:") => { vm.swallow_stop(before); return Ok(Some(out)) }
                    Err(e) => return Err(e),
                }
            }
        })
    }

    /* Items of a user `__iter__` as a list a native can read, None for other values. */
    pub(crate) fn lift_iterable(&mut self, v: Val, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let Some(items) = self.iter_to_vec_op(v, chunk)? else { return Ok(None); };
        Ok(Some(self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))?))
    }

    /* `str(v)` semantics, instance `__str__` wins, then `__repr__`, else the built-in display. */
    pub(crate) fn display_op(&mut self, v: Val, chunk: &SSAChunk) -> Result<String, VmErr> {
        if v.is_heap() && matches!(self.heap.get(v), HeapObj::Instance(..)) {
            if let Some(r) = self.try_call_dunder(v, "__str__", &[], chunk)? {
                return self.require_str(r, "__str__");
            }
            if let Some(r) = self.try_call_dunder(v, "__repr__", &[], chunk)? {
                return self.require_str(r, "__repr__");
            }
        }
        // Containers render their elements with repr, dispatching user __repr__ on instances.
        if self.is_container_val(v) { return self.repr_op(v, chunk); }
        let s = self.display(v);
        // Render is O(size). Charge it so reprinting growing data can't outrun the budget.
        self.charge_steps(s.len())?;
        Ok(s)
    }

    /* `repr(v)` semantics, instance `__repr__` wins, otherwise the built-in repr (which adds quotes for strings, etc.). */
    pub(crate) fn repr_op(&mut self, v: Val, chunk: &SSAChunk) -> Result<String, VmErr> {
        let s = self.repr_deep(v, chunk, &mut Vec::new())?;
        self.charge_steps(s.len())?;
        Ok(s)
    }

    fn is_container_val(&self, v: Val) -> bool {
        v.is_heap() && matches!(
            self.heap.get(v),
            HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_)
        )
    }

    /* Container-aware repr dispatches `__repr__` on nested instances. Elements always use repr, with `seen` tracking heap ids for cycle detection. */
    pub(crate) fn repr_deep(&mut self, v: Val, chunk: &SSAChunk, seen: &mut Vec<u32>) -> Result<String, VmErr> {
        if !v.is_heap() { return Ok(self.repr(v)); }
        if !self.is_container_val(v) {
            if matches!(self.heap.get(v), HeapObj::Instance(..))
                && let Some(r) = self.try_call_dunder(v, "__repr__", &[], chunk)? {
                return self.require_str(r, "__repr__");
            }
            // The outer path goes along, so an exception inside its own args stops there.
            return Ok(self.repr_d(v, seen));
        }
        let id = v.as_heap();
        if seen.contains(&id) {
            return Ok(match self.heap.get(v) {
                HeapObj::Dict(_) | HeapObj::Set(_) => "{...}".into(),
                HeapObj::Tuple(_) => "(...)".into(),
                HeapObj::FrozenSet(_) => "frozenset({...})".into(),
                _ => "[...]".into(),
            });
        }
        if seen.len() > crate::vm::value_ops::RENDER_DEPTH_MAX { return Ok("...".into()); }
        seen.push(id);
        let body = self.repr_container_body(v, chunk, seen);
        seen.pop();
        body
    }

    /* Builds the bracketed body for a container `v` (caller has pushed `v` to `seen`). */
    fn repr_container_body(&mut self, v: Val, chunk: &SSAChunk, seen: &mut Vec<u32>) -> Result<String, VmErr> {
        // Clone element handles first so a dunder call (which may GC/mutate) can't dangle a borrow.
        match self.heap.get(v) {
            HeapObj::List(rc) => {
                let items = rc.borrow().clone();
                let mut out = String::from("[");
                self.join_reprs(&mut out, &items, chunk, seen)?;
                out.push(']');
                Ok(out)
            }
            HeapObj::Tuple(t) => {
                let items = t.clone();
                let mut out = String::from("(");
                if items.len() == 1 {
                    let r = self.repr_deep(items[0], chunk, seen)?;
                    out.push_str(&r);
                    out.push(',');
                } else {
                    self.join_reprs(&mut out, &items, chunk, seen)?;
                }
                out.push(')');
                Ok(out)
            }
            HeapObj::Set(s) => {
                let items: Vec<Val> = s.borrow().iter().copied().collect();
                if items.is_empty() { return Ok("set()".into()); }
                let mut out = String::from("{");
                self.join_reprs(&mut out, &items, chunk, seen)?;
                out.push('}');
                Ok(out)
            }
            HeapObj::FrozenSet(s) => {
                let items: Vec<Val> = s.iter().copied().collect();
                if items.is_empty() { return Ok("frozenset()".into()); }
                let mut out = String::from("frozenset({");
                self.join_reprs(&mut out, &items, chunk, seen)?;
                out.push_str("})");
                Ok(out)
            }
            HeapObj::Dict(d) => {
                let entries: Vec<(Val, Val)> = d.borrow().iter().collect();
                let mut out = String::from("{");
                for (i, (k, val)) in entries.iter().enumerate() {
                    if i > 0 {
                        // The output cap bounds breadth the way the depth cap bounds nesting.
                        if out.len() > crate::vm::value_ops::MAX_REPR_LEN { out.push_str(", ..."); break; }
                        out.push_str(", ");
                    }
                    let kr = self.repr_deep(*k, chunk, seen)?;
                    out.push_str(&kr);
                    out.push_str(": ");
                    let vr = self.repr_deep(*val, chunk, seen)?;
                    out.push_str(&vr);
                }
                out.push('}');
                Ok(out)
            }
            _ => Ok(self.repr(v)),
        }
    }

    fn join_reprs(&mut self, out: &mut String, items: &[Val], chunk: &SSAChunk, seen: &mut Vec<u32>) -> Result<(), VmErr> {
        for (i, e) in items.iter().enumerate() {
            if i > 0 {
                if out.len() > crate::vm::value_ops::MAX_REPR_LEN { out.push_str(", ..."); break; }
                out.push_str(", ");
            }
            let r = self.repr_deep(*e, chunk, seen)?;
            out.push_str(&r);
        }
        Ok(())
    }

    fn require_str(&self, v: Val, name: &str) -> Result<String, VmErr> {
        if v.is_heap() && let HeapObj::Str(s) = self.heap.get(v) { return Ok(s.clone()); }
        Err(VmErr::TypeMsg(crate::s!("'", str name, "' did not return a string")))
    }

    /* `format(v, spec)`, an instance `__format__(spec)` wins, an empty spec is `str(v)` and the rest runs the spec engine. */
    pub(crate) fn format_op(&mut self, v: Val, spec: &str, chunk: &SSAChunk) -> Result<String, VmErr> {
        if v.is_heap() && matches!(self.heap.get(v), HeapObj::Instance(..)) {
            let spec_val = self.heap.alloc(HeapObj::Str(spec.to_string()))?;
            if let Some(r) = self.try_call_dunder(v, "__format__", &[spec_val], chunk)? {
                return self.require_str(r, "__format__");
            }
        }
        if spec.is_empty() { return self.display_op(v, chunk); }
        // Only numbers and strings read a spec, `f"{x!s:>9}"` pads anything else.
        let own = v.is_int() || v.is_float() || v.is_bool() || matches!(self.heap.try_get(v), Some(HeapObj::Str(_) | HeapObj::LongInt(_)));
        if !own { return Err(VmErr::TypeMsg(crate::s!("unsupported format string passed to ", str &self.type_repr_name(v), ".__format__"))); }
        crate::vm::format_spec::format_value(v, spec, &self.heap).map_err(crate::vm::format_spec::fmt_err)
    }

    /* Coerce a `__len__` / `__length_hint__` return value to bool semantics and reject negatives. */
    fn len_to_bool(&self, v: Val) -> Result<bool, VmErr> {
        let n = if v.is_int() { v.as_int() as i128 }
        else if let Some(i) = crate::vm::types::as_i128(v, &self.heap) { i }
        else { return Err(cold_type("__len__ must return int")); };
        if n < 0 { return Err(cold_value("__len__() should return >= 0")); }
        Ok(n != 0)
    }
}
