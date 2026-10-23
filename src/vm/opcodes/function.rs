use crate::s;
use super::*;
use super::super::{Ctor, ParamKind};
use super::super::act::CallFacts;
use crate::parser::fused_native;


// Builtin conversion-type name -> its constructor, None for exception/other types.
fn constructor_native(name: &str) -> Option<super::super::types::NativeFnId> {
    use super::super::types::NativeFnId::*;
    Some(match name {
        "int" => Int, "float" => Float, "complex" => Complex, "str" => Str, "bytes" => Bytes, "bytearray" => ByteArray, "memoryview" => MemoryView,
        "bool" => Bool, "list" => List, "tuple" => Tuple, "dict" => Dict,
        "set" => Set, "frozenset" => FrozenSet, "range" => Range, "type" => Type, "frozendict" => FrozenDict, "sentinel" => Sentinel,
        "slice" => Slice,
        _ => return None,
    })
}

/* Builtins that iterate an argument, where a user `__iter__` may stand in. */
#[inline]
fn iterates(id: super::super::types::NativeFnId) -> bool {
    use super::super::types::NativeFnId::*;
    matches!(id, Sum | Sorted | Set | FrozenSet | Bytes | ByteArray | Dict | Min | Max)
}

/* A count outside a builtin arity, ranged ones name the bound they miss. */
#[cold]
pub(crate) fn arity_err(id: super::super::types::NativeFnId, n: u16) -> VmErr {
    let (lo, hi) = id.arity();
    let name = id.name();
    // The name of a sentinel is its only positional, past two the count with `repr` is what is wrong.
    if id == super::super::types::NativeFnId::Sentinel {
        return VmErr::TypeMsg(if n <= 2 { crate::s!("sentinel() takes exactly 1 positional argument (", int n, " given)") } else { crate::s!("sentinel() takes at most 2 arguments (", int n, " given)") });
    }
    match (lo == hi, lo) {
        (true, 0) => return VmErr::TypeMsg(crate::s!(str name, "() takes no arguments (", int n, " given)")),
        (true, 1) => return VmErr::TypeMsg(crate::s!(str name, "() takes exactly one argument (", int n, " given)")),
        (true, k) => return VmErr::TypeMsg(crate::s!(str name, " expected ", int k, " arguments, got ", int n)),
        _ => {}
    }
    let (word, bound) = if n < lo { ("least", lo) } else { ("most", hi) };
    VmErr::TypeMsg(crate::s!(str id.name(), " expected at ", str word, " ", int bound, " argument", str if bound == 1 { "" } else { "s" }, ", got ", int n))
}

/* What the entry of a body noted for its exit. */
#[derive(Clone, Copy)]
pub(crate) struct Entry {
    call_ip: Option<u32>,
    bound: bool,
}

/* A call's arguments, up to eight inline so a call allocates nothing. */
pub(crate) enum Args {
    Inline(u8, [Val; 8]),
    Heap(Vec<Val>),
}

impl Args {
    #[inline]
    pub(crate) fn of(vals: &[Val]) -> Self {
        if vals.len() > 8 { return Self::Heap(vals.to_vec()); }
        let mut inline = [Val::undef(); 8];
        inline[..vals.len()].copy_from_slice(vals);
        Self::Inline(vals.len() as u8, inline)
    }
}

/* Where the positionals of a call a loop opens lie, on the stack from `first` until it drops to `to`, or read out of registers. */
#[derive(Clone, Copy)]
pub(crate) enum Passed {
    Stack { first: usize, to: usize },
    Regs([Val; 3]),
}

impl core::ops::Deref for Args {
    type Target = [Val];
    #[inline]
    fn deref(&self) -> &[Val] {
        match self { Self::Inline(n, vals) => &vals[..*n as usize], Self::Heap(v) => v }
    }
}

/* Whether builtin `f` can run again from its start once a call it made comes back, its user calls replayed from the log, changing nothing twice. */
pub(crate) fn native_redoable(id: crate::vm::types::NativeFnId) -> bool {
    use crate::vm::types::NativeFnId::*;
    // Builtins comparing, adding or testing every item take their own jobs, whose calls running again would make quadratic.
    matches!(id, Format | GetAttr | HasAttr | SetAttr | DelAttr | Iter | Next | Zip | Map | Filter | Enumerate | Reversed | Hash
        | Str | Repr | Len | Abs | Int | Float | Bool | Round | Divmod | Pow | Bin | Oct | Hex | Chr | Ord
        | List | Tuple | Set | FrozenSet | Dict | Bytes | Print | Range | AIter)
}

/* `native_redoable` of any callee, a builtin method or type among them. */
pub(crate) fn redoable(f: Option<&HeapObj>) -> bool {
    match f {
        Some(&HeapObj::NativeFn(id)) => native_redoable(id),
        Some(&HeapObj::BoundMethod(_, id)) => matches!(id.kind(), crate::vm::methods::MethodKind::Iterates | crate::vm::methods::MethodKind::Search),
        // The builtin types make their value from what they are given.
        Some(HeapObj::Type(name)) => matches!(name.as_str(), "list" | "tuple" | "set" | "frozenset" | "dict" | "bytes" | "str" | "int" | "float" | "bool" | "range"),
        _ => false,
    }
}

impl<'a> VM<'a> {
    /* Dispatch every function-shaped opcode (Call, MakeFunction, builtins). */
    pub(crate) fn handle_function(&mut self, op: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        // Only a fused builtin carries these flags, `Call` and the make-ops use the bits as counts.
        if operand & (crate::parser::SPREAD_ARGS | crate::parser::KEYWORDS) != 0
            && !matches!(op, OpCode::Call | OpCode::CallSpread | OpCode::CallExtern | OpCode::MakeFunction | OpCode::MakeCoroutine) {
            let (pos, kw) = ((operand & 0xFF) as usize, ((operand >> 8) & 0x3F) as usize);
            let (pos, kw) = if operand & crate::parser::SPREAD_ARGS != 0 { self.close_spread(pos, kw) } else { (pos, kw) };
            // A packed op stays fused while its counts fit, the rest run as a plain call.
            let packed = matches!(op, OpCode::CallPrint | OpCode::CallDict | OpCode::CallMin | OpCode::CallMax | OpCode::CallEnumerate);
            let fits = pos <= 0xFF && fused_native(op).is_some_and(|id| id.takes(pos as u16));
            if fits && packed && kw <= 0x3F { return self.handle_function(op, crate::parser::pack_call(pos as u16, kw as u16), chunk); }
            if fits && op == OpCode::CallRange && kw == 0 { return self.handle_function(op, pos as u16, chunk); }
            return self.call_spread_builtin(op, pos, kw, chunk);
        }
        // A module-scope rebind of a builtin name must win over call sites fused before it existed. Plain fused operands are a bare count, so counts past one byte fall back to the native (they cannot round-trip through `exec_call`'s packing).
        let packed_operand = matches!(op, OpCode::CallPrint | OpCode::CallDict | OpCode::CallMin | OpCode::CallMax | OpCode::CallEnumerate);
        if self.fused_rebound(op)
            && (packed_operand || operand <= 0xFF)
            && let Some(name) = fused_native(op).map(|id| id.name())
            && let Some(bound) = self.scopes[self.chunk_module_id(chunk)].get(name)
            && !(bound.is_heap() && matches!(self.heap.get(bound), HeapObj::NativeFn(id) if id.name() == name))
        {
            return self.call_rebound(bound, (operand & 0xFF) as usize, ((operand >> 8) & 0xFF) as usize, chunk);
        }
        // Fused builtins skip dispatch_native, the parser checked their counts and user iterables lift here.
        if let Some(id) = fused_native(op) {
            if iterates(id) {
                let (pos, kw) = if packed_operand { (operand & 0xFF, operand >> 8) } else { (operand, 0) };
                if self.lift_builtin_args(id, operand, pos as usize, kw as usize, chunk)? { self.job_ok = false; return Ok(()); }
            }
            return self.run_native(id, operand, chunk);
        }
        match op {
            OpCode::Call => self.exec_call(operand, chunk),
            OpCode::CallSpread => {
                let (pos, kw) = self.close_spread((operand & 0xFF) as usize, ((operand >> 8) & 0xFF) as usize);
                // A function the items are spread into runs as a frame of the loop that spread them, as a plain call's does.
                self.inline_call = self.pending_exec_safe;
                self.exec_call_n(pos, kw, chunk)
            }
            OpCode::MakeFunction | OpCode::MakeCoroutine => self.exec_make_function(op, operand, chunk),
            OpCode::CallExtern => self.call_extern(operand, chunk),
            _ => Err(cold_runtime("non-function opcode in handle_function")),
        }
    }

    /* A builtin method call, `sort`, `str.format` and user iterables run user code so they take the frame here. */
    pub(crate) fn exec_bound_method(&mut self, recv: Val, id: crate::vm::methods::BuiltinMethodId, pos: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        use crate::vm::methods::MethodKind;
        // Only a sort, a search, a generator's method and a generator an iterating method takes take the frames its call site may open.
        let ok = core::mem::take(&mut self.job_ok);
        let done = match id.kind() {
            _ if matches!(id.ty(), "generator" | "coroutine" | "async_generator" | "async_generator_asend" | "async_generator_athrow" | "anext_awaitable" | "coroutine_wrapper") => {
                crate::vm::methods::method_arity(id, pos.len())?;
                self.job_ok = ok;
                self.coro_method(recv, id.name(), pos, chunk)
            }
            _ if id.ty() == "memoryview" => {
                crate::vm::methods::method_arity(id, pos.len())?;
                self.view_method(recv, id.name(), pos)
            }
            _ if id.ty() == "BaseExceptionGroup" => {
                crate::vm::methods::method_arity(id, pos.len())?;
                self.group_method(recv, id.name(), pos, chunk, ok)
            }
            _ if id.ty() == "super" => {
                crate::vm::methods::method_arity(id, pos.len())?;
                self.super_get(recv, pos)
            }
            _ if id.ty() == "object" => {
                crate::vm::methods::method_arity(id, pos.len())?;
                self.object_method(recv, id.name(), pos, kw, chunk, ok)
            }
            // A dunder of a builtin value runs the operator it names.
            _ if id.ty() == "op" => {
                if !kw.is_empty() { return Err(VmErr::TypeMsg(crate::s!(str id.name(), "() takes no keyword arguments"))); }
                // A named method of a number counts its arguments under the type of the number.
                if id.name() == "conjugate" && !pos.is_empty() {
                    let ty = match self.type_name(recv) { "bool" => "int", ty => ty };
                    return Err(VmErr::TypeMsg(crate::s!(str ty, ".conjugate() takes no arguments (", int pos.len() as i64, " given)")));
                }
                crate::vm::methods::method_arity(id, pos.len())?;
                crate::vm::methods::operator::call(self, recv, id.name(), pos, chunk)
            }
            MethodKind::Sort => { self.job_ok = ok; self.exec_sort(recv, pos, kw, chunk) }
            // `list.__init__` and `dict.__init__`, as `super().__init__` reaches them, take a generator or instance gathered as frames of the loop.
            MethodKind::Plain if ok && kw.is_empty() && id.name() == "__init__" && matches!(id.ty(), "list" | "dict") && self.method_gen_job(recv, id, pos, chunk)? => return Ok(()),
            MethodKind::Format if id.name() == "format_map" && kw.is_empty() => {
                crate::vm::methods::method_arity(id, pos.len())?;
                let names = crate::vm::methods::string::Names::Map(pos[0]);
                // Called where a frame may open, the dunders its values run are frames of the loop.
                if ok && self.heap.users && crate::vm::methods::string::format_job(self, recv, &[], names, chunk)? { return Ok(()); }
                crate::vm::methods::string::format(self, recv, &[], names, chunk)
            }
            MethodKind::Format => {
                let names = crate::vm::methods::string::Names::Kw(kw);
                if ok && self.heap.users && crate::vm::methods::string::format_job(self, recv, pos, names, chunk)? { return Ok(()); }
                crate::vm::methods::string::format(self, recv, pos, names, chunk)
            }
            MethodKind::Mapped if id.ty() == "str" && kw.is_empty() && pos.len() == 1
                && (matches!(self.heap.try_get(pos[0]), Some(HeapObj::Instance(..))) || self.heap.classed(pos[0]).is_some()) => {
                crate::vm::methods::string::translate_mapped(self, recv, pos[0], ok)
            }
            MethodKind::Search if matches!(id.ty(), "list" | "tuple") && kw.is_empty() => {
                crate::vm::methods::method_arity(id, pos.len())?;
                // Items whose `__eq__` runs Python are compared by a job, its calls frames of the loop.
                if ok && self.scan_job(recv, id.name(), pos, chunk)? { return Ok(()); }
                self.with_roots(pos.iter().copied().chain([recv]), |vm| {
                    crate::vm::methods::list::search(vm, recv, id.name(), pos, |vm, a, b| vm.member_eq(a, b, chunk))
                })
            }
            // A generator an iterating method takes runs as a frame of the loop where one may open, its items gathered as the list the method takes.
            MethodKind::Iterates if ok && kw.is_empty() && self.method_gen_job(recv, id, pos, chunk)? => return Ok(()),
            MethodKind::Iterates if pos.iter().any(|&a| matches!(self.heap.try_get(a), Some(HeapObj::Instance(..)))) => {
                // Each lifted list stays rooted while later arguments run their `__iter__`.
                self.with_roots(pos.iter().copied().chain([recv]), |vm| {
                    let mut args = pos.to_vec();
                    for a in &mut args {
                        if let Some(l) = vm.lift_iterable(*a, chunk)? { vm.temp_roots.push(l); *a = l; }
                    }
                    crate::vm::methods::dispatch_method(vm, id, recv, &args, kw)
                })
            }
            _ => crate::vm::methods::dispatch_method(self, id, recv, pos, kw),
        };
        done?;
        if id.post() != crate::vm::methods::Post::None && let Some(&top) = self.stack.last() {
            let v = crate::vm::methods::post_result(self, id.post(), top)?;
            if let Some(t) = self.stack.last_mut() { *t = v; }
        }
        self.pay_work()
    }


    /* `str.lower(s)`, an unbound builtin method takes a receiver of its own type as the first argument. */
    pub(crate) fn exec_unbound_method(&mut self, id: crate::vm::methods::BuiltinMethodId, args: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        let Some((&recv, rest)) = args.split_first() else {
            return Err(VmErr::TypeMsg(crate::s!("unbound method ", str id.ty(), ".", str id.name(), "() needs an argument")));
        };
        // An exception instance takes the `BaseException` methods its builtin base gives it.
        let fits = self.type_name(recv) == id.ty() || id.ty() == "object" || (id.ty() == "op" && crate::vm::methods::operator::applies(self.type_name(recv), id.name()))
            || (id.ty() == "BaseException" && match self.heap.try_get(recv) { Some(&HeapObj::Instance(c, _)) => self.exc_base(c).is_some(), Some(HeapObj::ExcInstance(..)) => true, _ => false });
        if !fits {
            return Err(VmErr::TypeMsg(crate::s!("descriptor '", str id.name(), "' for '", str id.ty(), "' objects doesn't apply to a '", str self.type_name(recv), "' object")));
        }
        self.exec_bound_method(recv, id, rest, kw, chunk)
    }

    /* Lifts the user iterables among `pos` positional args under `kw` keyword pairs, wherever builtin `id` iterates. */
    fn lift_builtin_args(&mut self, id: super::super::types::NativeFnId, operand: u16, pos: usize, kw: usize, chunk: &SSAChunk) -> Result<bool, VmErr> {
        use super::super::types::NativeFnId::*;
        use crate::vm::jobs::Takes;
        let (from, to) = match id {
            Sum | Set | FrozenSet | Bytes | ByteArray | Dict => (0, 1),
            // `sorted` takes a generator, or the one an instance gives, as a frame of the loop itself.
            Sorted if !self.job_ok => (0, 1),
            // One iterable `min` or `max` folds as it steps, its items frames of the loop where one may open.
            Min | Max if pos == 1 && !self.job_ok => (0, 1),
            _ => return Ok(false),
        };
        let base = self.stack.len().saturating_sub(pos + 2 * kw);
        // What the first argument is where a frame may open, an instance, a generator or a builtin iterator, which may run Python.
        let first = if self.job_ok && pos >= 1 && let Some(&o) = self.stack.get(base) {
            match self.heap.try_get(o) { Some(HeapObj::Instance(..)) => Some((o, true)), Some(HeapObj::Coroutine(..) | HeapObj::Iter(..)) => Some((o, false)), _ => None }
        } else { None };
        // A user iterable called where a frame may open has its iterator's `__next__` run as frames of the loop.
        if kw == 0 && let Some((o, true)) = first {
            let takes = match id {
                Sum if pos <= 2 => Some(Takes::Sum),
                Set if pos == 1 => Some(Takes::All(OpCode::CallSet, 1)),
                Dict if pos == 1 => Some(Takes::All(OpCode::CallDict, 1)),
                _ => None,
            };
            if let Some(takes) = takes {
                let args = self.stack.split_off(base);
                self.job_ok = false;
                match self.pull_job(o, takes, &args[1..], chunk) {
                    Ok(true) => return Ok(true),
                    Ok(false) => self.stack.extend_from_slice(&args),
                    Err(e) => return Err(e),
                }
            }
        }
        // A builtin taking its iterable whole, given a generator, instance or Python-stepped iterator where a frame may open, runs on what a job gathers.
        if first.is_some() && matches!(id, Sorted | FrozenSet | Bytes | ByteArray) && self.gather_arg(id, operand, base, chunk)? { return Ok(true); }
        // A generator called where a frame may open runs as a frame of the loop, its items gathered as a list the builtin takes in its place.
        for i in from..to.min(pos) {
            if let Some(&v) = self.stack.get(base + i)
                && let Some(l) = self.lift_iterable(v, chunk)? { self.stack[base + i] = l; }
        }
        Ok(false)
    }

    /* `lift_builtin_args` for a builtin taking its iterable whole at stack index `base`, its items gathered by a job, whether the job took the call. */
    #[inline(never)]
    pub(crate) fn gather_arg(&mut self, id: super::super::types::NativeFnId, operand: u16, base: usize, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let Some(&(mut v)) = self.stack.get(base) else { return Ok(false) };
        if !matches!(self.heap.try_get(v), Some(HeapObj::Instance(..) | HeapObj::Coroutine(..) | HeapObj::Iter(..))) { return Ok(false); }
        if matches!(self.heap.get(v), HeapObj::Instance(..)) {
            // The call `__iter__` makes takes no frames of its own, the builtin's call keeping them.
            let ok = self.job_ok;
            let it = match self.gen_of_iter(v, chunk)? {
                Some(g) => g,
                // A plain `__iter__` runs as a frame of the loop, the job then gathering what its iterator gives.
                None if id != super::super::types::NativeFnId::Sorted && self.pull_job(v, crate::vm::jobs::Takes::Collect(id, operand, base as u32), &[], chunk)? => return Ok(true),
                None => match self.try_call_dunder(v, "__iter__", &[], chunk)? { Some(it) => it, None => return Ok(false) },
            };
            self.job_ok = ok;
            self.stack[base] = it;
            if matches!(self.heap.try_get(it), Some(HeapObj::Instance(..))) {
                let items = self.iter_items_op(v, it, chunk)?;
                self.stack[base] = self.alloc_list(items)?;
                return Ok(false);
            }
            v = it;
        }
        if id == super::super::types::NativeFnId::Sorted { return Ok(false); }
        let takes = crate::vm::jobs::Takes::Collect(id, operand, base as u32);
        match self.heap.get(v) {
            HeapObj::Iter(..) => self.drive_iter(v, takes, &[]),
            HeapObj::Coroutine(..) => { self.job_ok = false; self.gather_job(v, takes, alloc::vec::Vec::new()) }
            _ => Ok(false),
        }
    }

    /* Whether a generator, or an instance's, among arguments `pos` of iterating method `id` runs as a frame of the loop, its items gathered in a list. */
    #[cold]
    #[inline(never)]
    fn method_gen_job(&mut self, recv: Val, id: crate::vm::methods::BuiltinMethodId, pos: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        let call = || core::iter::once(recv).chain(pos.iter().copied()).collect::<alloc::vec::Vec<Val>>();
        let Some(i) = pos.iter().position(|&a| matches!(self.heap.try_get(a), Some(HeapObj::Coroutine(..) | HeapObj::Instance(..)))) else {
            let Some(i) = pos.iter().position(|&a| matches!(self.heap.try_get(a), Some(HeapObj::Iter(..)))) else { return Ok(false) };
            return self.drive_iter(pos[i], crate::vm::jobs::Takes::Method(id, i as u8), &call());
        };
        let g = match self.heap.try_get(pos[i]) { Some(HeapObj::Coroutine(..)) => pos[i], _ => match self.iter_of_user(pos[i], chunk)? { Some(g) => g, None => return Ok(false) } };
        let mut args = call();
        args[i + 1] = g;
        if self.with_roots([g], |vm| vm.gather_job(g, crate::vm::jobs::Takes::Method(id, i as u8), args.clone()))? { return Ok(true); }
        // the method steps the iterator the instance gave
        if matches!(self.heap.try_get(g), Some(HeapObj::Iter(..))) && self.drive_iter(g, crate::vm::jobs::Takes::Method(id, i as u8), &args)? { return Ok(true); }
        // a user iterator's items come through `__next__` as the method lifts them
        if matches!(self.heap.try_get(g), Some(HeapObj::Instance(..))) {
            let items = self.with_roots(args.iter().copied(), |vm| vm.iter_items_op(pos[i], g, chunk))?;
            args[i + 1] = self.alloc_list(items)?;
        }
        self.with_roots(args.iter().copied(), |vm| crate::vm::methods::dispatch_method(vm, id, recv, &args[1..], &[]))?;
        Ok(true)
    }

    /* Runs builtin method `id` on what `call` holds, the `items` argument `i` gave put there as a list, the result on the stack. */
    #[inline(never)]
    pub(crate) fn method_collected(&mut self, id: crate::vm::methods::BuiltinMethodId, i: usize, items: Vec<Val>, mut call: Vec<Val>, chunk: &SSAChunk) -> Result<(), VmErr> {
        let list = self.alloc_list(items)?;
        let Some(at) = call.get_mut(i + 1) else { return Err(cold_runtime("a collection lost its argument")) };
        *at = list;
        self.with_roots(call.iter().copied(), |vm| vm.exec_bound_method(call[0], id, &call[1..], &[], chunk))
    }

    /* Runs builtin `id` of `operand` arguments on `items` a generator at stack index `slot` gave, as a list in its place, `extra` its `sorted` options. */
    #[inline(never)]
    pub(crate) fn native_collected(&mut self, id: super::super::types::NativeFnId, operand: u16, slot: usize, items: Vec<Val>, extra: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        let list = self.alloc_list(items)?;
        let Some(at) = self.stack.get_mut(slot) else { return Err(cold_runtime("a collection lost its argument")) };
        *at = list;
        // Only `sorted`, `min` and `max` take the frames or floors their key may open, which a caller asking for them sees opened.
        let ok = core::mem::take(&mut self.job_ok);
        if id == super::super::types::NativeFnId::Sorted {
            let key = extra.first().copied().filter(|k| !k.is_undef());
            let reverse = extra.get(1).is_some_and(|&r| self.truthy(r));
            self.job_ok = ok;
            self.call_sorted_with_key(key, reverse, chunk)?;
            return self.pay_work();
        }
        self.job_ok = ok && matches!(id, super::super::types::NativeFnId::Min | super::super::types::NativeFnId::Max);
        self.run_native(id, operand, chunk)
    }

    fn exec_make_function(&mut self, opcode: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let chunk_ptr = chunk as *const _;
        let global = self.fn_index.iter()
            .find(|(p, _)| *p == chunk_ptr)
            .and_then(|(_, v)| v.get(operand as usize).copied())
            .ok_or_else(|| cold_runtime("MakeFunction: unknown function index"))? as usize;

        if opcode == OpCode::MakeCoroutine {
            if self.is_async.len() <= global { self.is_async.resize(global + 1, false); }
            self.is_async[global] = true;
            if let Some(f) = self.call_facts.get_mut(global) { f.bits &= !super::super::act::OPENS; }
        }

        let n_defaults = self.functions[global].2 as usize;
        let defaults = if n_defaults > 0 { self.pop_n(n_defaults)? } else { vec![] };

        // Each free variable takes its cell from the defining frame or class scope, a section's slots where it lays them.
        let map = self.cell_map.take();
        let at = |s: usize| map.as_deref().map_or(s, |m| m.get(s).map_or(usize::MAX, |&r| r as usize));
        let roots_base = self.temp_roots.len();
        let mut captures: Vec<(usize, Val)> = Vec::with_capacity(self.fn_scope[global].freevars.len());
        for k in 0..self.fn_scope[global].freevars.len() {
            let (slot, from, ref bare) = self.fn_scope[global].freevars[k];
            let held = match from {
                Some(s) => self.regs.get(self.base.wrapping_add(at(s))).copied().filter(|&v| matches!(self.heap.try_get(v), Some(HeapObj::Cell(_)))),
                None => self.class_cells.last().and_then(|cells| cells.iter().find(|(n, _)| n == bare).map(|&(_, c)| c)),
            };
            let cell = match held { Some(c) => c, None => self.heap.alloc(HeapObj::Cell(Val::undef()))? };
            self.temp_roots.push(cell);
            captures.push((slot, cell));
        }
        let val = self.heap.alloc(HeapObj::Func(global, defaults, captures, Rc::new(RefCell::new(Vec::new()))))?;
        self.temp_roots.truncate(roots_base);
        self.push(val);
        Ok(())
    }

    /* Calls a rebound builtin with the args a fused site stacked, the callee slotted under them. */
    fn call_rebound(&mut self, callee: Val, pos: usize, kw: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        let at = self.stack.len().checked_sub(pos + 2 * kw).ok_or_else(|| cold_runtime("stack underflow"))?;
        self.stack.insert(at, callee);
        self.exec_call_n(pos, kw, chunk)
    }

    /* A fused builtin given `*` or keywords it cannot count runs as a plain call through its binding. */
    fn call_spread_builtin(&mut self, op: OpCode, pos: usize, kw: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        let name = fused_native(op).ok_or_else(|| cold_runtime("spread on an unknown fused call"))?.name();
        self.register_builtin(name);
        let module = self.chunk_module_id(chunk);
        let callee = self.scopes[module].get(name).or_else(|| self.global(name)).ok_or_else(|| VmErr::Name(name.into()))?;
        self.call_rebound(callee, pos, kw, chunk)
    }

    /* The counts a call gains from its spreads, reopening the frame its first spread saved. */
    fn close_spread(&mut self, pos: usize, kw: usize) -> (usize, usize) {
        let counts = ((pos as i32 + self.pending.pos_delta).max(0) as usize, (kw as i32 + self.pending.kw_delta).max(0) as usize);
        let (p, k) = self.pending.delta_save.pop().unwrap_or((0, 0));
        self.pending.pos_delta = p;
        self.pending.kw_delta = k;
        counts
    }

    /* Calls `callee`, with `recv` first when bound, the arguments laid out as a plain call leaves them. */
    pub(crate) fn call_with(&mut self, callee: Val, recv: Option<Val>, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        self.push(callee);
        self.stack.extend(recv);
        self.stack.extend_from_slice(positional);
        self.stack.extend_from_slice(kw_flat);
        self.exec_call_n(positional.len() + recv.is_some() as usize, kw_flat.len() / 2, chunk)
    }

    pub(crate) fn exec_call(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.exec_call_n((operand & 0xFF) as usize, ((operand >> 8) & 0xFF) as usize, chunk)
    }

    /* Readies the call op a general call of builtin `callee` made, its site taken, for the builtin to arm. */
    #[inline(always)]
    fn native_defer(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val]) -> crate::vm::redo::Defer {
        let site = self.redo.take_site();
        if site.1 == crate::vm::redo::Rerun::No { return crate::vm::redo::Defer::No; }
        // So does a bound builtin method given a key a class hashes, and a class whose `__new__` runs Python before its `__init__`.
        let go = match self.heap.try_get(callee) {
            Some(HeapObj::BoundMethod(..)) if self.user_hash && self.user_keys(positional) => true,
            // One a job constructs where a frame may open hands no call to the op.
            Some(HeapObj::Class(..)) if self.user_new && !self.job_ok => self.ctors.get(&callee.0).map_or_else(|| self.special_class(callee), |c| c.special),
            f => redoable(f),
        };
        if !go { return crate::vm::redo::Defer::No; }
        let n = 1 + positional.len() + kw_flat.len();
        self.redo_defer_held(site.0 as usize, site.1, self.stack.len() + n, callee, positional, kw_flat)
    }

    /* `Call` orchestrator. Only user `Func` callees open a frame on the register stack and run the body inline, every other callee kind short-circuits in `try_dispatch_non_func_callable`. */
    pub(crate) fn exec_call_n(&mut self, num_pos: usize, num_kw: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        // Taken so nested native calls see false.
        let call_safe = core::mem::take(&mut self.pending_exec_safe);
        let inline = core::mem::take(&mut self.inline_call);
        // A call an op's handler made before it ran again gives what it gave then.
        if self.redo.on && let Some(at) = self.stack.len().checked_sub(num_pos + 2 * num_kw + 1) && let Some(r) = self.redo_taken(self.stack[at]) {
            self.stack.truncate(at);
            (self.pending.method_binding, self.pending.call_ip, self.pending_instance) = (None, None, Val::undef());
            return r.map(|v| self.push(v));
        }
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        // A function taking exactly these positionals binds them straight off the stack, or opens its block in this loop.
        if num_kw == 0
            && let Some(at) = self.stack.len().checked_sub(num_pos + 1)
            && let Some(&HeapObj::Func(fi, _, ref captures, _)) = self.heap.try_get(self.stack[at])
            && self.call_facts[fi].arity == Some(num_pos)
        {
            let callee = self.stack[at];
            let plain = captures.is_empty();
            let captures = (!plain).then(|| captures.clone());
            if inline && self.call_facts[fi].opens() && call_safe == self.frame_safe && num_pos <= 8 && self.warm_code(fi) && self.open_keyed(num_pos, 0)? { return Ok(()); }
            // A generator or coroutine that captures nothing is made straight off the stack, as its call makes nothing else.
            let generator = self.makes_gen(fi);
            if generator && plain && self.fn_scope[fi].cellvars.is_empty() { return self.make_gen(fi, at, num_pos); }
            self.charge_step()?;
            let base = self.bind_plain(fi, callee, &Passed::Stack { first: at + 1, to: at }, num_pos);
            // Most bodies share no variable, so there is no cell to make.
            if (captures.is_some() || !self.fn_scope[fi].cellvars.is_empty())
                && let Err(e) = self.enter_scope(fi, captures.as_deref().unwrap_or(&[]), base)
            {
                self.close_window(self.fn_pool[fi]);
                return Err(e);
            }
            if generator { return self.gen_of_window(fi, base); }
            return self.run_call(fi, callee, base, call_safe, inline, chunk);
        }
        self.call_bound(num_pos, num_kw, call_safe, inline, chunk)
    }

    /* The suspended generator or coroutine `fi` makes of the `n` positionals past its function at `at`, exactly its parameters, in place of the call. */
    #[inline(never)]
    fn make_gen(&mut self, fi: usize, at: usize, n: usize) -> Result<(), VmErr> {
        self.charge_step()?;
        let mut slots = alloc::vec![Val::undef(); self.functions[fi].1.names.len()];
        for (k, &(_, slot)) in self.param_slots[fi].iter().take(n).enumerate() {
            if let Some(s) = slots.get_mut(slot) { *s = self.stack[at + 1 + k]; }
        }
        self.stack.truncate(at);
        let coro = self.heap.alloc(HeapObj::Coroutine(crate::value::Coro::fresh(slots, BodyRef::Fn(fi))))?;
        self.push(coro);
        Ok(())
    }

    /* `exec_call_n` past its plain functions, where what the callee is binds its arguments as it takes them. */
    #[inline(always)]
    fn call_bound(&mut self, num_pos: usize, num_kw: usize, call_safe: bool, inline: bool, chunk: &SSAChunk) -> Result<(), VmErr> {
        let at = self.stack.len().checked_sub(num_pos + 2 * num_kw).ok_or_else(|| cold_runtime("stack underflow"))?;
        let (positional, kw_flat) = (Args::of(&self.stack[at..at + num_pos]), Args::of(&self.stack[at + num_pos..]));
        self.stack.truncate(at);

        // Charge each call so wide recursion is op-budget bounded.
        self.charge_step()?;

        let callee = self.pop()?;
        let not_callable = |vm: &Self| VmErr::TypeMsg(crate::s!("'", str vm.type_name(callee), "' object is not callable"));
        if !callee.is_heap() { return Err(not_callable(self)); }

        // Most functions have no defaults or captures, so those clone only when present.
        let (fi, defaults, captures) = match self.heap.get(callee) {
            HeapObj::Func(i, d, c, _) => (*i, (!d.is_empty()).then(|| d.clone()), (!c.is_empty()).then(|| c.clone())),
            _ => {
                // Bound methods, constructors and callable instances end in a call of their own, which may run in the loop.
                if matches!(self.heap.get(callee), HeapObj::BoundUserMethod(..) | HeapObj::Class(..) | HeapObj::Instance(..) | HeapObj::GenericAlias(..)) {
                    self.pending_exec_safe = call_safe;
                    self.inline_call = inline;
                }
                // A builtin called where a frame may open in place may run its callback so.
                self.job_ok = inline && call_safe == self.frame_safe;
                // A builtin given a user object hands its calls to the loop, the op run again after each and taken so the builtin's calls see none.
                let defer = if self.heap.users { self.native_defer(callee, &positional, &kw_flat) } else { crate::vm::redo::Defer::No };
                let dispatched = self.try_dispatch_non_func_callable(callee, &positional, &kw_flat, chunk);
                self.redo_undefer(defer);
                (self.pending_exec_safe, self.job_ok) = (false, false);
                if dispatched? { return Ok(()); }
                return Err(not_callable(self));
            }
        };
        // A function whose parameters keywords and defaults fill opens its block in this loop, its call laid back on the stack for the opening.
        if inline && call_safe == self.frame_safe && num_pos + num_kw <= 8 && !self.functions[fi].1.is_generator && self.warm_code(fi) && !self.is_async.get(fi).copied().unwrap_or(false) {
            let back = self.stack.len();
            self.stack.push(callee);
            self.stack.extend_from_slice(&positional);
            self.stack.extend_from_slice(&kw_flat);
            if self.open_keyed(num_pos, num_kw)? { return Ok(()); }
            self.stack.truncate(back);
        }

        let (defaults, captures) = (defaults.as_deref().unwrap_or(&[]), captures.as_deref().unwrap_or(&[]));

        let names = self.functions[fi].1.names.len();
        let base = self.open_window(self.fn_pool[fi], names + self.functions[fi].1.constants.len() + 3, names, true);
        if let Err(e) = self.bind_function_args(fi, defaults, &positional, &kw_flat, base).and_then(|()| self.enter_scope(fi, captures, base)) {
            self.close_window(self.fn_pool[fi]);
            return Err(e);
        }
        if self.makes_gen(fi) { return self.gen_of_window(fi, base); }
        self.run_call(fi, callee, base, call_safe, inline, chunk)
    }

    /* Whether a call of `fi` makes a suspended generator or coroutine rather than running its body. */
    #[inline(always)]
    fn makes_gen(&self, fi: usize) -> bool { self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) }

    /* The suspended generator or coroutine `fi` makes of the names its call bound in the window at `base`, its first resume laying the rest. */
    #[inline(never)]
    fn gen_of_window(&mut self, fi: usize, base: usize) -> Result<(), VmErr> {
        let frame = self.regs[base..base + self.functions[fi].1.names.len()].to_vec();
        self.close_window(self.fn_pool[fi]);
        let coro = self.heap.alloc(HeapObj::Coroutine(crate::value::Coro::fresh(frame, BodyRef::Fn(fi))))?;
        self.push(coro);
        Ok(())
    }

    /* Opens the frame of a call whose `n` positionals are exactly its parameters, copied in from where they were passed, its base. */
    #[inline(always)]
    pub(crate) fn bind_plain(&mut self, fi: usize, callee: Val, passed: &Passed, n: usize) -> usize {
        // The callee runs in its window.
        let names = self.functions[fi].1.names.len();
        let base = self.open_window(self.fn_pool[fi], names + self.functions[fi].1.constants.len() + 3, names, true);
        let size = names;
        let args = match passed {
            Passed::Stack { first, .. } => &self.stack[*first..*first + n],
            Passed::Regs(vals) => &vals[..n],
        };
        match self.call_facts[fi] {
            // Positionals that fill the first slots copy in at once.
            CallFacts { in_loop: true, arity: Some(k), .. } if k <= size => {
                self.regs[base..base + n].copy_from_slice(&args[..n]);
                if n < k { self.fill_frame(callee, base, n, k); }
            }
            _ => for (k, &(_, slot)) in self.param_slots[fi].iter().enumerate() {
                if slot < size { self.regs[base + slot] = args[k]; }
            },
        }
        if let Passed::Stack { to, .. } = *passed { self.stack.truncate(to); }
        base
    }

    /* Opens a plain call as the next frame of the loop that made it, `operand` its counts, false when the callee takes the general path. */
    #[inline(always)]
    pub(crate) fn call_in_loop(&mut self, operand: u16) -> Result<bool, VmErr> {
        if operand >> 8 != 0 { return Ok(false); }
        let num_pos = operand as usize;
        let Some(at) = self.stack.len().checked_sub(num_pos + 1) else { return Ok(false) };
        self.open_in_loop(self.stack[at], Passed::Stack { first: at + 1, to: at }, num_pos)
    }

    /* Opens `callee` as the next frame of the loop with its `n` positionals, false when it takes the general path. */
    #[inline(always)]
    pub(crate) fn open_in_loop(&mut self, callee: Val, passed: Passed, n: usize) -> Result<bool, VmErr> {
        let (fi, captured) = match self.heap.try_get(callee) {
            Some(&HeapObj::Func(fi, _, ref captures, _)) => (fi, captures.len()),
            Some(HeapObj::Class(..)) => return self.construct_in_loop(callee, passed, n),
            _ => return Ok(false),
        };
        // One that never ran runs in its code as the compiler wrote it, as a first call does.
        let facts = &self.call_facts[fi];
        if !facts.in_loop || !facts.arity.is_some_and(|k| n <= k && n >= facts.required as usize) || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(false); }
        let p = &self.pools[facts.pool];
        if (p.code.is_none() || p.cold || p.consts.is_none()) && !self.opens_first(fi) { return Ok(false); }
        self.open_frame(fi, callee, &passed, n, captured)?;
        Ok(true)
    }

    /* `open_in_loop` of a function taking `*args` or `**kwargs`, its `n` positionals bound as a call binds them, false for any other. */
    #[cold]
    #[inline(never)]
    pub(crate) fn open_bound(&mut self, callee: Val, passed: Passed, n: usize) -> Result<bool, VmErr> {
        let (fi, defaults, captures) = match self.heap.try_get(callee) { Some(HeapObj::Func(fi, d, c, _)) => (*fi, d.clone(), c.clone()), _ => return Ok(false) };
        if self.call_facts[fi].arity.is_some() || self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(false); }
        let args: Vec<Val> = match passed { Passed::Stack { first, .. } => self.stack[first..first + n].to_vec(), Passed::Regs(v) => v[..n].to_vec() };
        let p = &self.pools[self.call_facts[fi].pool];
        if (p.code.is_none() || p.cold || p.consts.is_none()) && !self.opens_first(fi) { return Ok(false); }
        let names = self.functions[fi].1.names.len();
        let base = self.open_window(self.fn_pool[fi], names + self.functions[fi].1.constants.len() + 3, names, true);
        if let Err(e) = self.bind_function_args(fi, &defaults, &args, &[], base).and_then(|()| self.enter_scope(fi, &captures, base)) { self.close_window(self.fn_pool[fi]); return Err(e); }
        if let Passed::Stack { to, .. } = passed { self.stack.truncate(to); }
        self.charge_step()?;
        self.enter_frame(fi, callee)?;
        Ok(true)
    }

    /* Whether `fi`, able to open but not warm (never run or run once), opens as a frame of the loop in its unlowered code, made here when missing. */
    #[cold]
    #[inline(never)]
    fn opens_first(&mut self, fi: usize) -> bool {
        let pool = self.call_facts[fi].pool;
        let p = &self.pools[pool];
        if p.code.is_none() || p.cold { self.cold_code(fi); }
        let p = &self.pools[pool];
        p.code.is_some() && p.consts.is_some()
    }

    /* Opens the function under `num_pos` positionals and `num_kw` name and value pairs as a block of this loop when they fill its plain parameters. */
    #[cold]
    #[inline(never)]
    pub(crate) fn open_keyed(&mut self, num_pos: usize, num_kw: usize) -> Result<bool, VmErr> {
        let Some(at) = self.stack.len().checked_sub(num_pos + 2 * num_kw + 1) else { return Ok(false) };
        let callee = self.stack[at];
        let Some(&HeapObj::Func(fi, ref defaults, _, _)) = self.heap.try_get(callee) else { return Ok(false) };
        let f = self.call_facts[fi];
        // A generator or a coroutine makes its object, which the full path does.
        if self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(false); }
        // A function taking `*args` takes what its positionals leave in a tuple, called by position alone.
        if f.arity.is_none() && num_kw == 0 && self.pending_instance.is_undef() && let Some(p) = self.star_slot(fi) && num_pos >= p && p < 8 && !self.pools[f.pool].cold && self.pools[f.pool].code.is_some() && self.pools[f.pool].consts.is_some() && self.link_of(fi).is_none() {
            let rest = self.heap.alloc(HeapObj::Tuple(self.stack[at + 1 + p..at + 1 + num_pos].to_vec()))?;
            let mut args = [Val::undef(); 8];
            args[..p].copy_from_slice(&self.stack[at + 1..at + 1 + p]);
            args[p] = rest;
            let args = &args[..p + 1];
            let binding = self.pending.method_binding;
            let bound = binding.is_some() && self.uses_super(fi);
            let link = (super::super::act::NO_DEST as i64) << 24 | if bound { super::super::act::BOUND << 40 } else { 0 };
            if self.open_block_as(fi, callee, args, link, bound).is_none() { return Ok(false); }
            if bound && let Some((class, recv)) = binding { self.bindings.push((self.level(), class, recv)); }
            self.stack.truncate(at);
            self.rebase_top();
            (self.pending.call_ip, self.pending.method_binding) = (None, None);
            (self.entered, self.yielded) = (true, true);
            return Ok(true);
        }
        // One with keyword-only parameters takes each by name or its default, its slots all filled here.
        if f.arity.is_none() {
            if !self.pending_instance.is_undef() || self.full_slots(fi).is_none() || self.star_slot(fi).is_some() { return Ok(false); }
            return self.open_named(at, fi, num_pos, num_kw);
        }
        let Some(arity) = f.arity else { return Ok(false) };
        let p = &self.pools[f.pool];
        if !(f.in_loop || self.cells_only(fi)) || arity > 8 || num_pos > arity || self.link_of(fi).is_some() || p.code.is_none() || p.cold || p.consts.is_none()
            || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(false); }
        let mut args = [Val::undef(); 8];
        args[..num_pos].copy_from_slice(&self.stack[at + 1..at + 1 + num_pos]);
        let params = &self.functions[fi].0;
        for k in 0..num_kw {
            let (name, v) = (self.stack[at + 1 + num_pos + 2 * k], self.stack[at + 2 + num_pos + 2 * k]);
            let Some(HeapObj::Str(name)) = self.heap.try_get(name) else { return Ok(false) };
            let Some(p) = params.iter().position(|p| crate::parser::types::param_base_name(p) == name.as_str()) else { return Ok(false) };
            // A positional-only name given by keyword is refused by the general path.
            if p < num_pos || p < self.functions[fi].1.posonly as usize || !args[p].is_undef() { return Ok(false); }
            args[p] = v;
        }
        // The parameters past those given take their defaults, the trailing ones.
        let first = arity - defaults.len();
        for (p, a) in args.iter_mut().enumerate().take(arity) {
            if a.is_undef() { if p < first { return Ok(false) } else { *a = defaults[p - first]; } }
        }
        let binding = self.pending.method_binding;
        let bound = binding.is_some() && self.uses_super(fi);
        // A constructor's `__init__` hands back its instance.
        let instance = self.pending_instance;
        let link = (super::super::act::NO_DEST as i64) << 24 | (if bound { super::super::act::BOUND } else { 0 } | if instance.is_undef() { 0 } else { super::super::act::CTOR }) << 40;
        if self.open_block_as(fi, callee, &args[..arity], link, bound).is_none() { return Ok(false); }
        if bound && let Some((class, recv)) = binding { self.bindings.push((self.level(), class, recv)); }
        if !instance.is_undef() { self.instances.push(instance); self.pending_instance = Val::undef(); }
        self.stack.truncate(at);
        self.rebase_top();
        (self.pending.call_ip, self.pending.method_binding) = (None, None);
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* `open_keyed` for `fi` at `at` with keyword-only parameters, positionals filling the plain ones, then names, then defaults in `=` order. */
    #[inline(never)]
    fn open_named(&mut self, at: usize, fi: usize, num_pos: usize, num_kw: usize) -> Result<bool, VmErr> {
        use super::super::ParamKind;
        let callee = self.stack[at];
        let f = self.call_facts[fi];
        let p = &self.pools[f.pool];
        if p.code.is_none() || p.cold || p.consts.is_none() || self.link_of(fi).is_some() { return Ok(false); }
        let Some(HeapObj::Func(_, defaults, _, _)) = self.heap.try_get(callee) else { return Ok(false) };
        let (params, slots) = (&self.functions[fi].0, &self.param_slots[fi]);
        let width = params.len();
        let plain = slots.iter().take_while(|(k, _)| matches!(k, ParamKind::Normal)).count();
        if width > 8 || num_pos > plain { return Ok(false); }
        let mut args = [Val::undef(); 8];
        args[..num_pos].copy_from_slice(&self.stack[at + 1..at + 1 + num_pos]);
        for k in 0..num_kw {
            let (name, v) = (self.stack[at + 1 + num_pos + 2 * k], self.stack[at + 2 + num_pos + 2 * k]);
            let Some(HeapObj::Str(name)) = self.heap.try_get(name) else { return Ok(false) };
            let Some(q) = params.iter().position(|q| crate::parser::types::param_base_name(q) == name.as_str()) else { return Ok(false) };
            if q < self.functions[fi].1.posonly as usize || !args[q].is_undef() { return Ok(false); }
            args[q] = v;
        }
        let mut d = 0;
        for (q, a) in args.iter_mut().enumerate().take(width) {
            let has = params[q].ends_with('=');
            if a.is_undef() { if has && d < defaults.len() { *a = defaults[d]; } else { return Ok(false) } }
            d += has as usize;
        }
        let binding = self.pending.method_binding;
        let bound = binding.is_some() && self.uses_super(fi);
        let link = (super::super::act::NO_DEST as i64) << 24 | if bound { super::super::act::BOUND << 40 } else { 0 };
        if self.open_block_as(fi, callee, &args[..width], link, bound).is_none() { return Ok(false); }
        if bound && let Some((class, recv)) = binding { self.bindings.push((self.level(), class, recv)); }
        self.stack.truncate(at);
        self.rebase_top();
        (self.pending.call_ip, self.pending.method_binding) = (None, None);
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* Whether function `fi` holds lowered code with its constants, which a block opens in. */
    #[inline(always)]
    fn warm_code(&self, fi: usize) -> bool {
        let p = &self.pools[self.call_facts[fi].pool];
        p.code.is_some() && !p.cold && p.consts.is_some()
    }

    /* Records as where the running frame's stack begins its length now, its call's operands taken off after it opened. */
    #[inline]
    fn rebase_top(&mut self) {
        let at = self.head_at(self.top_pool);
        self.regs[at + 3] = Val::int(self.stack.len() as i64);
    }

    /* Opens the plain function `fi` as the next frame of the loop, its checks already passed. */
    #[inline(always)]
    pub(crate) fn open_frame(&mut self, fi: usize, callee: Val, passed: &Passed, n: usize, captured: usize) -> Result<(), VmErr> {
        // A plain function opens its block, as a register call opens it, its caller's place set as the frame opens.
        if n <= 8 {
            let mut args = [Val::undef(); 8];
            match passed {
                Passed::Stack { first, .. } => args[..n].copy_from_slice(&self.stack[*first..*first + n]),
                Passed::Regs(vals) => args[..n].copy_from_slice(&vals[..n]),
            }
            if self.open_block(fi, callee, &args[..n], (super::super::act::NO_DEST as i64) << 24).is_some() {
                if let Passed::Stack { to, .. } = *passed { self.stack.truncate(to); self.rebase_top(); }
                (self.pending.call_ip, self.pending.method_binding) = (None, None);
                (self.entered, self.yielded) = (true, true);
                return Ok(());
            }
        }
        self.charge_step()?;
        let base = self.bind_plain(fi, callee, passed, n);
        // A closure takes the cells it captured, read off the function each time so none is copied.
        if captured > 0 && let Some(HeapObj::Func(_, _, captures, _)) = self.heap.try_get(callee) {
            for &(slot, cell) in captures { if let Some(r) = self.regs.get_mut(base + slot) { *r = cell; } }
        }
        self.enter_frame(fi, callee)
    }

    /* Whether a call of `fi` with `n` positionals can open as a frame of the loop, coroutines and code not lowered yet taking the general path. */
    #[inline(always)]
    pub(crate) fn opens_in_loop(&self, fi: usize, n: usize) -> bool {
        let facts = &self.call_facts[fi];
        facts.in_loop && facts.arity.is_some_and(|k| n <= k && n >= facts.required as usize) && !self.is_async_fn(fi)
            && self.pools[facts.pool].code.is_some() && !self.pools[facts.pool].cold && self.pools[facts.pool].consts.is_some()
    }

    /* Builds an instance of `class` and opens its `__init__` with the `n` positionals as the next frame of the loop, false when the constructor takes the general path. */
    #[inline(never)]
    fn construct_in_loop(&mut self, class: Val, passed: Passed, n: usize) -> Result<bool, VmErr> {
        // The instance goes first, in the slot of its class on the stack, so it and the arguments lie in parameter order.
        let (at, instance_first) = match passed {
            Passed::Stack { first, to } if first == to + 1 => (to, true),
            Passed::Regs(_) if n < 3 => (0, false),
            _ => return Ok(false),
        };
        // Only a constructor the general path already ran knows its `__init__`.
        let Some(&Ctor { epoch, init, exception: false, attrs, native: None, special: false }) = self.ctors.get(&class.0) else { return Ok(false) };
        if epoch != self.class_epoch { return Ok(false); }
        let Some((init, defining)) = init else {
            // A class without `__init__` takes no arguments, and its instance is the whole call.
            if n != 0 { return Ok(false); }
            self.charge_step()?;
            let instance = self.new_instance(class, attrs)?;
            if instance_first { self.stack[at] = instance; } else { self.push(instance); }
            return Ok(true);
        };
        let Some(&HeapObj::Func(fi, _, ref captures, _)) = self.heap.try_get(init) else { return Ok(false) };
        if !captures.is_empty() || !self.opens_in_loop(fi, n + 1) { return Ok(false); }
        let instance = self.new_instance(class, attrs)?;
        // Its `__init__` opens its block, its instance first and what the call gives back, its binding kept for a `super`.
        if n < 8 {
            let mut args = [Val::undef(); 8];
            args[0] = instance;
            match passed {
                Passed::Regs(v) => args[1..n + 1].copy_from_slice(&v[..n]),
                Passed::Stack { first, .. } => args[1..n + 1].copy_from_slice(&self.stack[first..first + n]),
            }
            let bound = self.uses_super(fi);
            let link = (super::super::act::NO_DEST as i64) << 24 | (super::super::act::CTOR | if bound { super::super::act::BOUND } else { 0 }) << 40;
            if self.open_block_as(fi, init, &args[..n + 1], link, bound).is_some() {
                self.instances.push(instance);
                if bound { self.bindings.push((self.level(), defining, instance)); }
                if let Passed::Stack { to, .. } = passed { self.stack.truncate(to); self.rebase_top(); }
                (self.pending.call_ip, self.pending.method_binding) = (None, None);
                (self.entered, self.yielded) = (true, true);
                return Ok(true);
            }
        }
        self.charge_step()?;
        (self.pending.method_binding, self.pending_instance) = (Some((defining, instance)), instance);
        let passed = match passed {
            Passed::Regs(v) => Passed::Regs([instance, v[0], v[1]]),
            Passed::Stack { .. } => { self.stack[at] = instance; Passed::Stack { first: at, to: at } }
        };
        self.bind_plain(fi, init, &passed, n + 1);
        self.enter_frame(fi, init)?;
        Ok(true)
    }

    /* A fresh instance of `class`, its dict sized for the `attrs` fields the last one ended with so they never regrow it. */
    pub(crate) fn new_instance(&mut self, class: Val, attrs: usize) -> Result<Val, VmErr> {
        let mut dict = DictMap::with_capacity(attrs);
        dict.set_shape(super::super::sites::ROOT_SHAPE);
        self.heap.alloc(HeapObj::Instance(class, Rc::new(RefCell::new(dict))))
    }

    /* Runs a bound call's body, its result left on the stack, as a frame of the running loop, or as a loop of its own when none runs or a plugin or the host calls while a call of the VM waits on it. */
    #[inline]
    fn run_call(&mut self, fi: usize, callee: Val, base: usize, call_safe: bool, inline: bool, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (_params, body, _, _) = self.functions[fi];
        // Only the call a constructor makes of `__init__` may take the instance it builds.
        let instance = core::mem::replace(&mut self.pending_instance, Val::undef());
        // A function exec defined in a dict reads and writes it, so its call nests and syncs with the dict around the body.
        let link = self.link_of(fi);
        // A call an op's handler makes runs as a frame of the loop, the op running again after it.
        // A frame that could not open, past the recursion or the memory limit, closed its window already.
        if link.is_none() && instance.is_undef() && (self.redo.on || self.redo.lazy && self.redo_wake()) && let Some(r) = self.redo_call(fi, callee) {
            return r.map(|v| self.push(v));
        }
        // A plain call the instruction loop made runs as a frame of that loop instead of a nested one, its first too.
        if inline && link.is_none() && call_safe == self.frame_safe && { let p = &self.pools[self.fn_pool[fi]]; p.code.is_none() || p.cold } { self.cold_code(fi); }
        if inline && link.is_none() && self.runs_in_loop(fi, call_safe) {
            self.pending_instance = instance;
            return self.enter_frame(fi, callee);
        }
        // One exec defined in a dict runs so too, the dict read as its outermost call begins and handed back as it ends.
        if inline && let Some(at) = link && call_safe == self.frame_safe && self.linked_code(fi) && self.runs_in_loop(fi, call_safe) {
            self.pending_instance = instance;
            return self.enter_linked(fi, callee, at);
        }

        // A call from Rust inside a running loop is a section, a job or a frame of it, but for one a plugin or the host makes while the VM waits.
        if self.depth != 0 && self.hosted == 0 { return Err(self.nested_call(fi)); }
        // Snapshot caller-visible depths so we can split the helper's stack/iter/exception contributions out if it suspends mid-body via a yielding builtin.
        let stack_base = self.stack.len();
        let iter_base = self.iter_stack.len();
        let exc_base = self.exception_stack.len();
        let with_base = self.with_stack.len();
        self.pending_exec_safe = call_safe;
        let entry = self.enter_body(fi);
        if let Some(at) = link { self.enter_link(at); }
        let mut exec_result = self.exec_in(body, base, self.fn_pool[fi]);
        if let Some(at) = link && let Err(e) = self.leave_link(at) { exec_result = Err(e); }
        self.leave_body(fi, entry, exec_result.is_ok(), chunk);

        // A body that suspended keeps its frame for the coroutine, any other drops it here, and the loops a return left with it.
        if exec_result.is_err() || !self.yielded {
            self.close_window(self.fn_pool[fi]);
            self.iter_stack.truncate(iter_base);
        }
        let result = exec_result?;

        if self.yielded {
            // Sync helper suspended mid-execution (e.g. `sleep(0)` from inside a sync fn called by an async coro). Stage its frame on the VM-level buffer. `resume_coroutine` drains it onto the enclosing coro so the helper is re-entered from the right ip. Without this, the outer's resume_ip would skip past the unfinished helper and the next StoreName would underflow. A nested sync call inside this helper would already have pushed its own frame first, so the buffer ends up innermost-last.
            let helper_resume_ip = self.resume_ip;
            self.resume_ip = 0;
            let (mut stack_delta, mut iter_delta, mut exception_delta, mut withs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            self.save_frames(stack_base, iter_base, exc_base, with_base, &mut stack_delta, &mut iter_delta, &mut exception_delta, &mut withs);
            let slots = self.window_slots(self.fn_pool[fi]);
            self.close_window(self.fn_pool[fi]);
            let layout = core::mem::take(&mut self.resume_layout);
            self.pending_sync_frames.push(ParkedFrame { ip: helper_resume_ip, fi, func: callee, slots, stack_delta, iter_delta, exception_delta, withs, instance, layout });
            return Ok(());
        }

        self.push(result);
        Ok(())
    }

    /* The internal error of a call from Rust no frame of the running loop took, which debug builds stop at. */
    #[cold]
    #[inline(never)]
    pub(crate) fn nested_call(&mut self, fi: usize) -> VmErr {
        self.close_window(self.fn_pool[fi]);
        // One a job declined past the recursion, the memory or the step limit raises there, as its frame would have.
        if self.too_deep() { return cold_depth(); }
        if self.budget == 0 { return VmErr::Budget; }
        debug_assert!(false, "a call into Python from Rust took no frame of the running loop: {}", self.functions[fi].1.qualname);
        VmErr::Runtime("internal error: a call into Python from Rust took no frame of the running loop")
    }

    /* Pops the positional args and the flat name and value keyword pairs a call counts. */
    pub(crate) fn parse_call_args(&mut self, operand: u16) -> Result<(Vec<Val>, Vec<Val>), VmErr> {
        self.take_args((operand & 0xFF) as usize, ((operand >> 8) & 0xFF) as usize)
    }

    /* Pops `num_pos` positionals and `num_kw` name/value pairs. */
    pub(crate) fn take_args(&mut self, num_pos: usize, num_kw: usize) -> Result<(Vec<Val>, Vec<Val>), VmErr> {
        let total_items = num_pos + 2 * num_kw;
        // Bulk-copy the stack tail, one memcpy instead of per-item pops plus a reverse.
        let at = self.stack.len().checked_sub(total_items).ok_or_else(|| cold_runtime("stack underflow"))?;
        let kw_flat: Vec<Val> = self.stack[at + num_pos..].to_vec();
        let mut positional = self.stack.split_off(at);
        positional.truncate(num_pos);
        Ok((positional, kw_flat))
    }

    /* Pack a flat `[name, val, name, val, ...]` slice into a heap dict for the trailing kwargs slot. `None` when there are no kwargs so the FFI layer can serialize handle 0 on the wire. */
    pub(crate) fn pack_kw_dict(heap: &mut super::super::types::HeapPool, kw_flat: &[Val]) -> Result<Option<Val>, VmErr> {
        if kw_flat.is_empty() { return Ok(None); }
        let dm = super::super::types::DictMap::from_pairs(kw_flat.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect(), heap);
        Ok(Some(heap.alloc(super::super::types::HeapObj::Dict(Rc::new(RefCell::new(dm))))?))
    }

    /* list.sort() parses key/reverse kwargs and sorts in place. Intercepted from both call paths since it needs the chunk for __lt__. */
    pub(crate) fn exec_sort(&mut self, recv: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        // Its two parameters are keyword-only, past two arguments the count is what is wrong.
        if positional.len() > 2 { return Err(VmErr::TypeMsg(s!("sort() takes at most 2 arguments (", int positional.len() as i64, " given)"))); }
        if !positional.is_empty() { return Err(cold_type("sort() takes no positional arguments")); }
        let mut sort_key: Option<Val> = None;
        let mut sort_reverse = false;
        for pair in kw_flat.chunks(2) {
            match self.kw_name(pair[0]) {
                Some("key") => sort_key = Some(pair[1]),
                Some("reverse") => sort_reverse = self.truthy(pair[1]),
                _ => return Err(cold_type("list.sort() got unexpected keyword argument")),
            }
        }
        self.call_list_sort_keyed(recv, sort_key, sort_reverse, chunk)
    }

    /* Dispatch non-Func callees. Returns Ok(true) when handled here, Ok(false) means the caller falls through to the Func path. */
    #[inline(always)]
    fn try_dispatch_non_func_callable(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<bool, VmErr> {
        match self.heap.try_get(callee) {
            // `list[int](xs)` builds what its origin builds.
            Some(&HeapObj::GenericAlias(origin, _)) => return self.try_dispatch_non_func_callable(origin, positional, kw_flat, chunk),
            Some(&HeapObj::BoundMethod(recv, id)) if self.method_unbound(recv, id) => self.exec_unbound_method(id, positional, kw_flat, chunk)?,
            Some(&HeapObj::BoundMethod(recv, id)) => self.exec_bound_method(recv, id, positional, kw_flat, chunk)?,
            Some(&HeapObj::NativeFn(id)) => self.dispatch_native(id, positional, kw_flat, chunk)?,
            // Park on host-deferral like `call_extern` (e.g. `time.sleep` via module attr).
            Some(HeapObj::Extern(extern_fn)) => {
                let func = extern_fn.func.clone();
                if core::mem::take(&mut self.job_ok) && kw_flat.is_empty() && self.extern_held(callee, positional, &chunk.path)? { return Ok(true); }
                self.invoke_extern(&func, positional, kw_flat, &chunk.path)?;
            }
            Some(HeapObj::Type(name)) => {
                let name = name.clone();
                if let Some(id) = constructor_native(&name) {
                    self.dispatch_native(id, positional, kw_flat, chunk)?; // int/set/list/... construct
                } else if name == "object" {
                    // `object()` builds a unique featureless instance.
                    if !positional.is_empty() || !kw_flat.is_empty() { return Err(cold_type("object() takes no arguments")); }
                    let inst = self.heap.alloc(HeapObj::Instance(callee, Rc::new(RefCell::new(DictMap::new()))))?;
                    self.push(inst);
                } else if let Some(single) = match name.as_str() { "NoneType" => Some(None), "ellipsis" => Some(Some("Ellipsis")), "NotImplementedType" => Some(Some("NotImplemented")), _ => None } {
                    // The type of a singleton gives the singleton back.
                    if !positional.is_empty() || !kw_flat.is_empty() { return Err(VmErr::TypeMsg(crate::s!(str if name == "ellipsis" { "EllipsisType" } else { &name }, " takes no arguments"))); }
                    let v = match single { None => Val::none(), Some(b) => self.builtin_value(b)? };
                    self.push(v);
                } else if !crate::vm::globals::is_exc_type(&name) {
                    return Err(VmErr::TypeMsg(crate::s!("cannot create '", str &name, "' instances")));
                } else {
                    // Other Type objects are exception classes, build an ExcInstance for `raise X("msg")`.
                    if !kw_flat.is_empty() { return Err(cold_type("exception class takes no keyword arguments")); }
                    let exc = if crate::vm::globals::is_group_type(&name) { self.new_group(&name, positional)? }
                        else { self.heap.alloc(HeapObj::ExcInstance(name, positional.to_vec(), Val::undef()))? };
                    self.push(exc);
                }
            }
            // Calling a class, create an instance and run `__init__` if defined (walks bases).
            Some(HeapObj::Class(..)) => {
                let ctor = match self.ctors.get(&callee.0) {
                    Some(c) if c.epoch == self.class_epoch => *c,
                    _ => {
                        let c = Ctor { epoch: self.class_epoch, init: self.lookup_class_member(callee, "__init__"), exception: self.exc_base(callee).is_some(), attrs: 0, native: self.native_base(callee), special: self.special_class(callee) };
                        self.ctors.insert(callee.0, c);
                        c
                    }
                };
                let inline = core::mem::take(&mut self.inline_call);
                if ctor.special {
                    self.construct_special(callee, ctor.init, positional, kw_flat, chunk, inline)?;
                    return Ok(true);
                }
                if let Some(native) = ctor.native {
                    self.new_classed(callee, native, ctor.init, positional, kw_flat, chunk, inline)?;
                    return Ok(true);
                }
                // Without `__init__` a class takes no arguments, and an exception only positional ones.
                if ctor.init.is_none() && (!kw_flat.is_empty() || !ctor.exception && !positional.is_empty()) {
                    let HeapObj::Class(name, ..) = self.heap.get(callee) else { return Err(cold_type("class constructor takes no arguments")) };
                    let what = if ctor.exception { "() takes no keyword arguments" } else { "() takes no arguments" };
                    return Err(VmErr::TypeMsg(s!(str name, str what)));
                }
                let instance = self.new_instance(callee, ctor.attrs)?;
                // An exception keeps its constructor arguments as `args`, a later `__init__` may reset them.
                if ctor.exception { self.set_exc_args(instance, positional.to_vec())?; }
                if let Some(init) = ctor.init {
                    if self.run_init(init, instance, positional, kw_flat, chunk, inline)? { return Ok(true); }
                    if let Some(HeapObj::Instance(_, d)) = self.heap.try_get(instance)
                        && let Some(c) = self.ctors.get_mut(&callee.0) { c.attrs = d.borrow().len(); }
                }
                self.push(instance);
            }
            // Bound user method, prepend `self` to the arg list and re-dispatch.
            Some(&HeapObj::BoundUserMethod(recv, func, class)) => {
                // Same as the Class branch, depth check before mutating the stack.
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.pending.method_binding = Some((class, recv));
                self.call_with(func, Some(recv), positional, kw_flat, chunk)?;
            }
            // `prop.setter(fn)` returns a copy of the property with `fn` in that slot.
            Some(&HeapObj::PropertyPart(prop_val, slot)) => {
                if positional.len() != 1 || !kw_flat.is_empty() {
                    return Err(cold_type("property method takes exactly 1 argument"));
                }
                let Some(&HeapObj::Property(g, s, d)) = self.heap.try_get(prop_val) else {
                    return Err(cold_runtime("PropertyPart wraps a non-Property value"));
                };
                let mut parts = [g, s, d];
                parts[slot as usize] = positional[0];
                let new_prop = self.heap.alloc(HeapObj::Property(parts[0], parts[1], parts[2]))?;
                self.push(new_prop);
            }
            // Instance with `__call__`, bind and dispatch through `BoundUserMethod`-style flow.
            Some(&HeapObj::Instance(cls, _)) => {
                let Some((func, class)) = self.lookup_class_member(cls, "__call__") else { self.inline_call = false; return Ok(false) };
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.pending.method_binding = Some((class, callee));
                self.call_with(func, Some(callee), positional, kw_flat, chunk)?;
            }
            // A generator or coroutine object is no callable, it runs only through `next`, `for` or `await`.
            Some(HeapObj::Coroutine(_)) => return Err(VmErr::TypeMsg(s!("'", str self.type_name(callee), "' object is not callable"))),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /* An instance of `cls`, a class over the builtin type `native`, a value of that type marked with the class. */
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_classed(&mut self, cls: Val, native: Val, init: Option<(Val, Val)>, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk, inline: bool) -> Result<(), VmErr> {
        let empty = match self.heap.get(native) {
            HeapObj::Type(n) if n == "dict" => Some(HeapObj::Dict(Rc::new(RefCell::new(DictMap::new())))),
            HeapObj::Type(n) if n == "list" => Some(HeapObj::List(Rc::new(RefCell::new(Vec::new())))),
            HeapObj::Type(n) if n == "set" => Some(HeapObj::Set(Rc::new(RefCell::new(crate::value::ValSet::new())))),
            _ => None,
        };
        let obj = match empty {
            // `__init__` fills a container, through `super().__init__` or writes of its own.
            Some(empty) if init.is_some() => self.heap.alloc(empty)?,
            // A container made from an iterable that steps Python comes from the builtin's call in the loop, the class then made from it.
            _ if init.is_none() && inline && kw_flat.is_empty() && let [a] = positional
                && matches!(self.heap.get(native), HeapObj::Type(n) if matches!(n.as_str(), "list" | "tuple" | "dict" | "set" | "frozenset" | "bytes"))
                && self.steps_py(*a) && let Some(f) = self.with_roots([cls, *a], |vm| vm.remake_through(native, cls))? => {
                let safe = self.pending_exec_safe;
                self.inline_call = true;
                let made = self.with_roots([f], |vm| vm.call_with(f, None, positional, &[], chunk));
                (self.pending_exec_safe, self.inline_call) = (safe, false);
                return made;
            }
            // A value of any other type comes from the arguments as the builtin makes it.
            _ => {
                let safe = self.pending_exec_safe;
                self.call_with(native, None, positional, kw_flat, chunk)?;
                self.pending_exec_safe = safe;
                let v = self.pop()?;
                self.own_value(v)?
            }
        };
        self.heap.set_class(obj, cls, Rc::new(RefCell::new(DictMap::new())));
        // Its `__init__` opens as a frame of the loop, as a plain class's does, which hands the container back as it returns.
        if let Some(init) = init && self.run_init(init, obj, positional, kw_flat, chunk, inline)? { return Ok(()); }
        self.push(obj);
        Ok(())
    }

    /* `v` in a slot of its own, so marking it with a class leaves every other holder of the value alone. */
    pub(crate) fn own_value(&mut self, v: Val) -> Result<Val, VmErr> {
        let obj = match self.heap.try_get(v) {
            Some(o @ (HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::Tuple(_))) => o.clone(),
            Some(HeapObj::FrozenSet(s)) => HeapObj::FrozenSet(Rc::new((**s).clone())),
            Some(_) => return Ok(v),
            None => HeapObj::Boxed(v),
        };
        self.heap.alloc_unshared(obj)
    }

    /* Calls `__init__` on a fresh `instance`, true when the instance comes back later from a frame that runs or suspended, false once the call is over. */
    pub(crate) fn run_init(&mut self, (init_fn, defining): (Val, Val), instance: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk, inline: bool) -> Result<bool, VmErr> {
        // Fail-fast before pushing, the inner check fires only after parse_call_args pops.
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.pending.method_binding = Some((defining, instance));
        // Keywords reach `__init__` as they reach any function, its return value is dropped.
        (self.inline_call, self.pending_instance) = (inline, instance);
        let called = self.call_with(init_fn, Some(instance), positional, kw_flat, chunk);
        self.pending_instance = Val::undef();
        called?;
        if self.entered || self.yielded { return Ok(true); }
        self.pop()?;
        Ok(false)
    }

    /* Bind formal params from positional/kw buffers, then fill remaining undef slots with defaults. */
    pub(crate) fn bind_function_args(&mut self, fi: usize, defaults: &[Val], positional: &[Val], kw_flat: &[Val], base: usize) -> Result<(), VmErr> {
        let size = self.functions[fi].1.names.len();
        // Index by position to avoid an iterator borrow on `param_slots` across `heap.alloc`.
        let n_params = self.param_slots[fi].len();
        // Without a `*args` sink, positionals past the normal params are an error.
        let has_star = self.param_slots[fi].iter().any(|(k, _)| matches!(k, ParamKind::Star));
        let normal_count = self.param_slots[fi].iter().filter(|(k, _)| matches!(k, ParamKind::Normal)).count();
        if !has_star && positional.len() > normal_count {
            return Err(self.too_many_err(fi, normal_count, positional.len()));
        }
        let mut pos_idx = 0usize;
        for i in 0..n_params {
            let (kind, slot) = self.param_slots[fi][i];
            match kind {
                ParamKind::DoubleStar => {
                    // **kwargs gets only keys not bound to a named param.
                    let (params, posonly) = (&self.functions[fi].0, self.functions[fi].1.posonly as usize);
                    // A positional-only name given by keyword lands here too.
                    let pairs: Vec<(Val, Val)> = kw_flat.as_chunks::<2>().0.iter()
                        .filter(|p| match self.heap.try_get(p[0]) {
                            Some(HeapObj::Str(s)) => !params.iter().enumerate().any(|(i, pp)| i >= posonly && !pp.starts_with('*') && crate::parser::types::param_base_name(pp) == s.as_str()),
                            _ => true,
                        })
                        .map(|p| (p[0], p[1])).collect();
                    let dm = DictMap::from_pairs(pairs, &self.heap);
                    let dict_val = self.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(dm))))?;
                    if slot < size { self.regs[base + slot] = dict_val; }
                }
                ParamKind::Star => {
                    // *args binds to an immutable tuple.
                    let rest: Vec<Val> = positional[pos_idx..].to_vec();
                    pos_idx = positional.len();
                    let tuple_val = self.heap.alloc(HeapObj::Tuple(rest))?;
                    if slot < size { self.regs[base + slot] = tuple_val; }
                }
                ParamKind::Normal => {
                    if pos_idx >= positional.len() { continue; }
                    if slot < size { self.regs[base + slot] = positional[pos_idx]; }
                    pos_idx += 1;
                }
                // KwOnly slots are NOT consumed positionally, they bind only via kwargs.
                ParamKind::KwOnly => {}
            }
        }

        // Kwargs binding (rare path, not optimised).
        if !kw_flat.is_empty() {
            let params = &self.functions[fi].0;
            let posonly = self.functions[fi].1.posonly as usize;
            let has_double_star = self.param_slots[fi].iter().any(|(k, _)| matches!(k, ParamKind::DoubleStar));
            for pair in kw_flat.as_chunks::<2>().0 {
                // Malformed `**`/kwarg bytecode can leave a non-string in the name slot, so guard the heap access.
                let key = match self.heap.try_get(pair[0]) {
                    Some(HeapObj::Str(s)) => s.clone(),
                    _ => return Err(cold_runtime("malformed kwarg on stack")),
                };
                // Star/double-star params are not keyword targets. A kwarg whose name matches `*a`/`**k` goes to **kwargs.
                let at = params.iter().position(|p| !p.starts_with('*') && crate::parser::types::param_base_name(p) == key.as_str());
                // A positional-only name given by keyword goes to `**kwargs`, or is refused without one.
                if at.is_some_and(|pi| pi < posonly) && !has_double_star {
                    return Err(VmErr::TypeMsg(s!(str self.callee_name(fi), "() got some positional-only arguments passed as keyword arguments: '", str &key, "'")));
                }
                if let Some(pi) = at.filter(|&pi| pi >= posonly) {
                    let s = self.param_slots[fi][pi].1;
                    if s < size {
                        if !self.regs[base + s].is_undef() { return Err(VmErr::TypeMsg(s!(str self.callee_name(fi), "() got multiple values for argument '", str &key, "'"))); }
                        self.regs[base + s] = pair[1];
                    }
                } else if !has_double_star {
                    let hint = crate::util::suggest::did_you_mean(&key, params.iter().filter(|p| !p.starts_with('*')).map(|p| crate::parser::types::param_base_name(p)));
                    return Err(VmErr::TypeMsg(s!(str self.callee_name(fi), "() got an unexpected keyword argument '", str &key, "'", str &hint)));
                }
            }
        }

        // Defaults only fill slots still undef after binding.
        if !defaults.is_empty() {
            let ds = &self.default_slots[fi];
            for (di, &dv) in defaults.iter().enumerate() {
                if let Some(&(slot, _)) = ds.get(di)
                    && slot < size && self.regs[base + slot].is_undef() {
                        self.regs[base + slot] = dv;
                    }
            }
        }

        // A parameter left without a positional, a keyword or a default is a missing argument.
        if positional.len() < n_params
            && self.param_slots[fi].iter().any(|&(k, slot)| matches!(k, ParamKind::Normal | ParamKind::KwOnly) && slot < size && self.regs[base + slot].is_undef()) {
            return Err(self.missing_err(fi, base, size));
        }

        Ok(())
    }

    /* The name a call error shows for function `fi`. */
    fn callee_name(&self, fi: usize) -> &str {
        self.function_qualnames.get(fi).map(String::as_str).filter(|n| !n.is_empty()).unwrap_or("<lambda>")
    }

    /* `f() takes 2 positional arguments but 3 were given`, a range when some have defaults. */
    #[cold]
    #[inline(never)]
    fn too_many_err(&self, fi: usize, normal: usize, given: usize) -> VmErr {
        let with_default = self.default_slots[fi].iter().filter(|(slot, _)| self.param_slots[fi].iter().any(|&(k, s)| k == ParamKind::Normal && s == *slot)).count();
        let takes = if with_default == 0 { s!(int normal as i64) } else { s!("from ", int (normal - with_default) as i64, " to ", int normal as i64) };
        let noun = if normal == 1 && with_default == 0 { " positional argument but " } else { " positional arguments but " };
        let verb = if given == 1 { " was given" } else { " were given" };
        VmErr::TypeMsg(s!(str self.callee_name(fi), "() takes ", str &takes, str noun, int given as i64, str verb))
    }

    /* `f() missing 2 required positional arguments: 'a' and 'b'`, the positionals named before the keyword-only ones. */
    #[cold]
    #[inline(never)]
    fn missing_err(&self, fi: usize, base: usize, size: usize) -> VmErr {
        let unbound = |kind: ParamKind| -> Vec<&str> {
            self.param_slots[fi].iter().enumerate()
                .filter(|&(_, &(k, slot))| k == kind && slot < size && self.regs[base + slot].is_undef())
                .map(|(i, _)| crate::parser::types::param_base_name(&self.functions[fi].0[i])).collect()
        };
        let (names, kw_only) = match unbound(ParamKind::Normal) { n if n.is_empty() => (unbound(ParamKind::KwOnly), true), n => (n, false) };
        let quoted: Vec<String> = names.iter().map(|n| s!("'", str n, "'")).collect();
        let list = match quoted.split_last() {
            Some((last, [])) => last.clone(),
            Some((last, rest)) => {
                let and = if rest.len() > 1 { ", and " } else { " and " };
                s!(str &rest.join(", "), str and, str last)
            }
            None => String::new(),
        };
        let kind = if kw_only { " required keyword-only argument" } else { " required positional argument" };
        let plural = if names.len() == 1 { ": " } else { "s: " };
        VmErr::TypeMsg(s!(str self.callee_name(fi), "() missing ", int names.len() as i64, str kind, str plural, str &list))
    }

    /* Wraps a body's cell variables and hands it the cells it closed over. */
    pub(crate) fn enter_scope(&mut self, fi: usize, captures: &[(usize, Val)], base: usize) -> Result<(), VmErr> {
        let size = self.functions[fi].1.names.len();
        for k in 0..self.fn_scope[fi].cellvars.len() {
            let s = self.fn_scope[fi].cellvars[k];
            if s < size { self.regs[base + s] = self.heap.alloc(HeapObj::Cell(self.regs[base + s]))?; }
        }
        for &(s, cell) in captures { if s < size { self.regs[base + s] = cell; } }
        Ok(())
    }

    /* Notes what a body needs on the way out, a traceback frame waiting until an error asks for one. */
    pub(crate) fn enter_body(&mut self, fi: usize) -> Entry {
        let call_ip = self.pending.call_ip.take();
        // Method-call paths set `method_binding` immediately before invoking `exec_call`, plain function calls leave it `None`.
        let bound = match self.pending.method_binding.take() {
            // The body runs one level down, in the loop its call nests.
            Some((c, s)) if self.uses_super(fi) => { self.bindings.push((self.level() + 1, c, s)); true }
            _ => false,
        };
        Entry { call_ip, bound }
    }

    /* Undoes what `enter_body` noted, a body that raised leaving its traceback frame. */
    pub(crate) fn leave_body(&mut self, fi: usize, entry: Entry, ok: bool, chunk: &SSAChunk) {
        if entry.bound { self.bindings.pop(); }
        if !ok { self.note_frame(fi, entry.call_ip, chunk); }
    }

    /* Notes the frame of `fi` an error unwound, called at `call_ip` of `chunk`. */
    pub(crate) fn note_frame(&mut self, fi: usize, call_ip: Option<u32>, chunk: &SSAChunk) {
        // A position in another text would read as a line of this one, so the error points at this call.
        if !alloc::sync::Arc::ptr_eq(&self.functions[fi].1.source, &chunk.source) { self.error_byte_pos = None; }
        let call_byte_pos = call_ip.and_then(|ip| chunk.resolve_call(ip).or_else(|| chunk.resolve(ip))).unwrap_or(0);
        // A recursion unwinds the same call again and again, which the frame below only counts.
        if let Some(last) = self.call_stack.last_mut()
            && last.fi == fi && last.call_byte_pos == call_byte_pos && last.caller_path.as_ref().is_some_and(|p| alloc::sync::Arc::ptr_eq(p, &chunk.path))
        {
            last.repeats += 1;
            return;
        }
        // The innermost frames are the ones a traceback shows, so a long chain keeps its first few hundred.
        if self.call_stack.len() >= super::super::types::MAX_CALLS { return; }
        // Frames go in as the error unwinds, the innermost first, each with its caller's text so a render never borrows a live chunk.
        self.call_stack.push(super::super::types::CallFrame { fi, call_byte_pos, caller_source: Some(chunk.source.clone()), caller_path: Some(chunk.path.clone()), repeats: 0 });
    }

    /* CallExtern's operand packs `(extern_idx<<8)|(kw<<4)|pos`. Pop kw `name,val` pairs then `pos` positional vals, pack pairs into a heap dict via `pack_kw_dict` and hand it off as the explicit `Option<Val>` kwargs slot. */
    pub(crate) fn call_extern(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let extern_idx = (operand >> 8) as usize;
        // A star spread grows the counts the operand was written with.
        let (pos, kw) = self.close_spread((operand & 0xF) as usize, ((operand >> 4) & 0xF) as usize);
        let extern_fn = chunk.extern_table.get(extern_idx).ok_or_else(|| cold_runtime("CallExtern: extern index out of bounds"))?;
        let func = extern_fn.func.clone(); // Arc clone, refcount bump only
        let kw_flat = if kw > 0 { self.pop_n(kw * 2)? } else { Vec::new() };
        let positional = self.pop_n(pos)?;
        if core::mem::take(&mut self.job_ok) && kw_flat.is_empty() && positional.iter().any(|&v| self.gen_leaf(v, 0).is_some()) {
            let callee = self.with_roots(positional.iter().copied(), |vm| vm.heap.alloc(HeapObj::Extern(extern_fn.clone())))?;
            if self.extern_held(callee, &positional, &chunk.path)? { return Ok(()); }
        }
        self.invoke_extern(&func, &positional, &kw_flat, &chunk.path)
    }

    /* Whether a generator under the arguments `pos` of binding `callee` is held as a job first, the binding called once all are. */
    #[cold]
    #[inline(never)]
    fn extern_held(&mut self, callee: Val, pos: &[Val], caller: &alloc::sync::Arc<String>) -> Result<bool, VmErr> {
        self.extern_caller = Some(caller.clone());
        self.hold_start(core::iter::once(callee).chain(pos.iter().copied()).collect())
    }

    /* Calls the native binding `call` keeps first on the values after it, the result on the stack. */
    #[inline(never)]
    pub(crate) fn extern_call(&mut self, call: &[Val]) -> Result<(), VmErr> {
        let Some(HeapObj::Extern(f)) = self.heap.try_get(call[0]) else { return Err(cold_runtime("a held call lost its binding")) };
        let func = f.func.clone();
        let caller = self.extern_caller.clone().unwrap_or_else(|| self.chunk.path.clone());
        self.invoke_extern(&func, &call[1..], &[], &caller)
    }

    /* Runs a native binding with its args rooted, a plugin can call back into code that collects. */
    fn invoke_extern(&mut self, func: &crate::vm::types::ExternCallable, positional: &[Val], kw_flat: &[Val], caller: &alloc::sync::Arc<String>) -> Result<(), VmErr> {
        self.extern_caller = Some(caller.clone());
        let kwargs = Self::pack_kw_dict(&mut self.heap, kw_flat)?;
        let roots = positional.iter().chain(kw_flat).copied().chain(kwargs);
        match self.with_roots(roots, |vm| func(&mut vm.heap, positional, kwargs)) {
            Ok(result) => { self.push(result); Ok(()) }
            Err(VmErr::HostCallDeferred) => { self.park_host_call(); Ok(()) }
            Err(e) => Err(e),
        }
    }

    /* Park a native that deferred to the host with a `None` placeholder (overwritten by `set_host_result_by_id`), correlation id, yield. */
    fn park_host_call(&mut self) {
        self.push(Val::none());
        self.pending.host_call_id = self.next_host_call_id;
        self.next_host_call_id += 1;
        self.pending.host_call_request = true;
        self.yielded = true;
    }

    /* A builtin that takes keywords sees each one as the positional it names, `int("ff", base=16)`. */
    #[cold]
    fn dispatch_native_named(&mut self, id: super::super::types::NativeFnId, positional: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        use super::super::types::NativeFnId::*;
        let names: &[&str] = match id {
            Int => &["x", "base"],
            Round => &["number", "ndigits"],
            Sum => &["iterable", "start"],
            Pow => &["base", "exp", "mod"],
            Str => &["object", "encoding", "errors"],
            Bytes | ByteArray => &["source", "encoding", "errors"],
            Eval | Exec => &["", "globals", "locals"],
            Compile => &["source", "filename", "mode", "flags", "dont_inherit", "optimize"],
            Complex => &["real", "imag"],
            Property => &["fget", "fset", "fdel", "doc"],
            _ => return Err(VmErr::TypeMsg(s!(str id.name(), "() takes no keyword arguments"))),
        };
        let mut args: Vec<Option<Val>> = positional.iter().map(|&v| Some(v)).collect();
        for pair in kw.chunks(2) {
            let name = self.kw_name(pair[0]).unwrap_or("");
            let Some(i) = names.iter().position(|&n| n == name) else {
                return Err(VmErr::TypeMsg(s!("'", str name, "' is an invalid keyword argument for ", str id.name(), "()")));
            };
            if args.get(i).is_some_and(|a| a.is_some()) { return Err(VmErr::TypeMsg(s!(str id.name(), "() got multiple values for argument '", str name, "'"))); }
            if args.len() <= i { args.resize(i + 1, None); }
            args[i] = Some(pair[1]);
        }
        // Every parameter of `property` and `complex` is optional.
        if matches!(id, Property | Complex) { let fill = if id == Complex { Val::int(0) } else { Val::none() }; for a in &mut args { a.get_or_insert(fill); } }
        let args: Option<Vec<Val>> = args.into_iter().collect();
        let args = args.ok_or_else(|| VmErr::TypeMsg(s!(str id.name(), "() is missing a positional argument")))?;
        self.dispatch_native(id, &args, &[], chunk)
    }

    pub(crate) fn dispatch_native(&mut self, id: super::super::types::NativeFnId, positional: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        use super::super::types::NativeFnId::*;

        // `sorted()` extracts `key=`/`reverse=` before the no-kw guard, print/min/max/enumerate/dict parse their own kwargs below like the fused opcodes do.
        let mut sort_key: Option<Val> = None;
        let mut sort_reverse = false;
        let leftover_storage: Vec<Val>;
        let kw_remaining: &[Val] = if id == Sorted {
            let mut leftover: Vec<Val> = Vec::new();
            for chunk_pair in kw.chunks(2) {
                let (name_v, val_v) = (chunk_pair[0], chunk_pair[1]);
                match self.kw_name(name_v) {
                    Some("key") => sort_key = Some(val_v),
                    Some("reverse") => sort_reverse = self.truthy(val_v),
                    _ => { leftover.push(name_v); leftover.push(val_v); }
                }
            }
            leftover_storage = leftover;
            &leftover_storage
        } else { kw };

        let kw_aware = matches!(id, Print | Min | Max | Enumerate | Dict | FrozenDict | Sentinel | Zip);
        if !kw_remaining.is_empty() && !kw_aware {
            return self.dispatch_native_named(id, positional, kw_remaining, chunk);
        }
        let argc = positional.len() as u16;

        // Pre-validate arity to keep the stack clean on error.
        if !id.takes(argc) { return Err(arity_err(id, argc)); }

        for &v in positional { self.push(v); }
        // Repack pos/kw counts so the handlers pop the same layout a fused call leaves.
        let operand = if kw_aware && !kw_remaining.is_empty() {
            for &v in kw_remaining { self.push(v); }
            (((kw_remaining.len() / 2) as u16) << 8) | argc
        } else { argc };
        if iterates(id) && self.lift_builtin_args(id, operand, argc as usize, kw_remaining.len() / 2, chunk)? { return Ok(()); }
        match id {
            Sorted => { self.call_sorted_with_key(sort_key, sort_reverse, chunk)?; self.pay_work() }
            // CallPrint is statement-shaped, reached through Call its result is popped, so it leaves None, which no job leaves.
            Print => { self.job_ok = false; self.run_native(id, operand, chunk)?; self.push(Val::none()); Ok(()) }
            _ => self.run_native(id, operand, chunk),
        }
    }

    /* Runs a builtin on stacked args, `operand` a count or packed counts for keyword-aware ones. */
    fn run_native(&mut self, id: super::super::types::NativeFnId, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        use super::super::types::NativeFnId::*;
        if matches!(id, Abs | Round | Chr | Range | Bin | Oct | Hex | Divmod | Pow) { self.unbox_args(id, operand); }
        let done = match id {
            // Variadic
            Print => { self.call_print(operand, chunk) }
            Range => self.call_range(operand),
            Round => self.call_round(operand),
            Min => self.call_min(operand, chunk),
            Max => self.call_max(operand, chunk),
            Sum => self.call_sum(operand, chunk),
            Zip => self.call_zip(operand, chunk),
            Dict => self.call_dict(operand),
            Set => self.call_set(operand),
            Pow => self.call_pow(operand),
            All => self.call_all(operand, chunk),
            Any => self.call_any(operand, chunk),
            GetAttr => self.call_getattr(operand, chunk),
            Format => self.call_format(operand, chunk),
            // 0/1/2-arg
            Input => { self.call_input() }
            Len => self.call_len(chunk),
            Abs => self.call_abs(chunk),
            Str => self.call_str(operand, chunk),
            Int => self.call_int(operand, chunk),
            Float => self.call_float(operand, chunk),
            Bool => self.call_bool(operand, chunk),
            Type => self.call_type(operand),
            Chr => self.call_chr(),
            Ord => self.call_ord(),
            Sorted => self.call_sorted_with_key(None, false, chunk),
            Enumerate => self.call_enumerate(operand, chunk),
            List => self.call_list(operand, chunk),
            Tuple => self.call_tuple(operand, chunk),
            Bin => self.call_bin(),
            Oct => self.call_oct(),
            Hex => self.call_hex(),
            Repr => self.call_repr(chunk),
            Reversed => self.call_reversed(),
            Callable => self.call_callable(),
            Divmod => self.call_divmod(),
            IsInstance => self.call_isinstance(),
            IsSubclass => self.call_issubclass(),
            HasAttr => self.call_hasattr(chunk),
            Next => self.call_next(operand, chunk),
            Receive => self.call_receive(),
            SendMsg => self.call_send(),
            Map => self.call_map(operand, chunk),
            Filter => self.call_filter(chunk),
            Iter => self.call_iter(operand, chunk),
            Bytes => self.call_bytes(operand),
            Slice => self.call_slice(operand),
            Vars => if operand == 0 { self.call_locals(chunk) } else { self.call_vars() },
            SetAttr => self.call_setattr(chunk),
            DelAttr => self.call_delattr(chunk),
            ImportModule => self.call_import_module(),
            BytesFromHex => self.call_bytes_fromhex(),
            IntFromBytes => self.call_int_from_bytes(),
            IntToBytes => self.call_int_to_bytes(),
            FrozenSet => self.call_frozenset(operand),
            Globals => self.call_globals(chunk),
            Super => self.call_super(operand),
            Property => self.call_property(operand),
            StaticMethod => self.call_staticmethod(operand),
            ClassMethod => self.call_classmethod(operand),
            Eval => { self.call_eval(operand, chunk) }
            Exec => { self.call_exec(operand, chunk) }
            Compile => self.call_compile(operand, chunk),
            Id => self.call_id(),
            Hash => self.call_hash(),
            _ => self.run_native_rest(id, operand, chunk),
        };
        done?;
        self.pay_work()
    }

    /* `run_native` of the builtins few programs call, out of its body so the loop that runs it keeps its registers. */
    #[inline(never)]
    fn run_native_rest(&mut self, id: super::super::types::NativeFnId, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        use super::super::types::NativeFnId::*;
        match id {
            Complex => self.call_complex(operand, chunk),
            ByteArray => self.call_bytearray(operand),
            MemoryView => self.call_memoryview(),
            StarStart => { let e = self.pop()?; let s = self.star_start(e)?; self.push(s); Ok(()) }
            StarSplit => { let (s, t) = self.pop2()?; if let Some(m) = self.star_split(s, t)? { self.push(m); } Ok(()) }
            StarRaised => { let m = self.pop()?; let (s, x) = self.pop2()?; self.star_raised(s, x, m)?; self.push(Val::none()); Ok(()) }
            StarEnd => { let s = self.pop()?; if let Some(r) = self.star_end(s)? { self.push(r); } Ok(()) }
            Locals => self.call_locals(chunk),
            Dir => self.call_dir(operand, chunk),
            Ascii => self.call_ascii(chunk),
            Sentinel => self.call_sentinel(operand),
            AIter => self.call_aiter(chunk),
            ANext => self.call_anext(operand, chunk),
            FrozenDict => self.call_frozendict(operand),
            _ => Err(cold_runtime("builtin without a body")),
        }
    }

    /* A number of a class over int or float reaches `id` as the number, unless the class gives the hook it asks. */
    #[cold]
    #[inline(never)]
    fn unbox_args(&mut self, id: super::super::types::NativeFnId, operand: u16) {
        use super::super::types::NativeFnId::*;
        let hook = match id { Abs => "__abs__", Round => "__round__", Divmod => "__divmod__", Pow => "__pow__", _ => "__index__" };
        let n = ((operand & 0xFF) + 2 * ((operand >> 8) & 0xFF)) as usize;
        let from = self.stack.len().saturating_sub(n);
        for i in from..self.stack.len() {
            let v = self.stack[i];
            if let Some(&HeapObj::Boxed(x)) = self.heap.try_get(v) && !self.has_hook(v, hook) { self.stack[i] = x; }
        }
    }
}
