use crate::code::Ins;
use crate::lang::{Access, CallRule, CallShape, Cmp, Cx, EngineError, GetRule, Key, Name, Op, OpRule};
use crate::syntax::chunk::SSAChunk;
use crate::value::{HeapObj, Val, VmErr, cold_depth, cold_runtime};
use crate::vm::VM;
use crate::vm::cache::OpcodeCache;
use crate::vm::lower::Code;
use crate::vm::sites::{call_kept, get_kept, key_bits, kept_call, kept_get, kept_op, op_kept};

impl VM<'_> {
    /* the rule for `key` and `name` read as `how` kept at `at` once it answers */
    fn bound_get(&mut self, key: Key, name: Val, how: Access, at: Option<(&OpcodeCache, usize)>) -> GetRule {
        let bits = key_bits(key);
        if let Some((sites, i)) = at && let Some(rule) = get_kept(sites.site(i), bits, name, how, self.layout_epoch) { return rule; }
        let rule = (self.rt.bind_get)(&mut Cx::new(self), key, Name(name), how);
        // a refusal ends in an error so only an answer is worth a site
        if let Some((sites, i)) = at && !matches!(rule, GetRule::Slot | GetRule::Fail | GetRule::Ask) { sites.set_site(i, kept_get(rule, bits, name, how, self.layout_epoch)); }
        rule
    }

    /* what the runtime's rule gives `obj.name` once the engine's own lookup missed */
    #[cold]
    #[inline(never)]
    pub(crate) fn get_rule(&mut self, obj: Val, name: &str, how: Access, at: Option<(&OpcodeCache, usize)>) -> Result<Option<Val>, VmErr> {
        let (key, text) = (Key::of(&self.heap, obj), self.heap.intern_str(name)?);
        let rule = self.bound_get(key, text, how, at);
        Ok(Some(match rule {
            GetRule::Const(v) | GetRule::Method { func: v, bind: false } => v,
            GetRule::Method { func, bind: true } => self.heap.bind(obj, func)?,
            GetRule::Native(id) => {
                if self.natives.get(id).is_none() { return Err(cold_runtime("a rule names a native the runtime lacks")); }
                self.heap.bind_native(obj, id)?
            }
            GetRule::Field => return self.key_field(obj, text),
            GetRule::Slot | GetRule::Fail | GetRule::Ask => return Ok(None),
        }))
    }

    /* whether the runtime's rule stored `obj.name = value` the engine refused */
    #[cold]
    #[inline(never)]
    pub(crate) fn put_rule(&mut self, obj: Val, name: &str, value: Val, at: Option<(&OpcodeCache, usize)>) -> Result<bool, VmErr> {
        let (key, text) = (Key::of(&self.heap, obj), self.heap.intern_str(name)?);
        if self.bound_get(key, text, Access::Store, at) != GetRule::Field { return Ok(false); }
        let Some(d) = self.heap.dict_cell(obj) else { return Ok(false) };
        d.borrow_mut().insert(text, value, &self.heap);
        Ok(true)
    }

    /* whether the runtime's rule removed `obj.name` the engine found no attribute for */
    #[cold]
    #[inline(never)]
    pub(crate) fn drop_rule(&mut self, obj: Val, name: &str, at: Option<(&OpcodeCache, usize)>) -> Result<bool, VmErr> {
        let (key, text) = (Key::of(&self.heap, obj), self.heap.intern_str(name)?);
        if self.bound_get(key, text, Access::Delete, at) != GetRule::Field { return Ok(false); }
        let Some(d) = self.heap.dict_cell(obj) else { return Ok(false) };
        let gone = d.borrow_mut().remove(&text, &self.heap).is_some();
        Ok(gone)
    }

    /* what a foreign kind's get or a map's key gives a missed `obj.name` */
    #[cold]
    #[inline(never)]
    pub(crate) fn own_field(&mut self, obj: Val, name: &str) -> Result<Option<Val>, VmErr> {
        if obj.is_foreign() { return self.heap.foreign_get(obj, name); }
        if !self.natives.policy.map_fields || self.heap.dict_cell(obj).is_none() { return Ok(None); }
        let key = self.heap.intern_str(name)?;
        let Some(d) = self.heap.dict_cell(obj) else { return Ok(None) };
        let found = d.borrow().get(&key, &self.heap).copied();
        Ok(found)
    }

    /* the key `name` of map `obj` or a foreign value's field of that name */
    fn key_field(&mut self, obj: Val, name: Val) -> Result<Option<Val>, VmErr> {
        if obj.is_foreign() {
            let Some(HeapObj::Str(s)) = self.heap.try_get(name) else { return Ok(None) };
            let s = s.clone();
            return self.heap.foreign_get(obj, &s);
        }
        let Some(d) = self.heap.dict_cell(obj) else { return Ok(None) };
        let found = d.borrow().get(&name, &self.heap).copied();
        Ok(found)
    }

    /* what the runtime's rule makes of calling `callee` which the engine cannot call */
    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call_rule(&mut self, callee: Val, pos: &[Val], kw: &[Val], call_safe: bool, inline: bool, chunk: &SSAChunk) -> Result<Option<Option<Val>>, VmErr> {
        let shape = CallShape { positional: pos.len().min(255) as u8, keywords: (kw.len() / 2).min(255) as u8, spread: false };
        // the rule its site kept for this callee or one asked for it to keep
        let rule = match self.rule_hand.take().and_then(|s| call_kept(s, callee, self.layout_epoch)) {
            Some(rule) => rule,
            None => {
                let rule = (self.rt.bind_call)(&mut Cx::new(self), callee, shape);
                if !matches!(rule, CallRule::Function | CallRule::Fail | CallRule::Ask) { self.rule_hand = Some(kept_call(rule, callee, self.layout_epoch)); }
                rule
            }
        };
        if matches!(rule, CallRule::Function | CallRule::Fail | CallRule::Ask) {
            // a foreign kind answers a call of its values with positionals through its own hook
            return if callee.is_foreign() && kw.is_empty() { self.heap.foreign_call(callee, pos).map(|r| r.map(Some)) } else { Ok(None) };
        }
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        // what the rule calls takes a frame of the loop as the call would
        self.pending_exec_safe = call_safe;
        let made = match rule {
            CallRule::Bound { func, recv } => {
                self.inline_call = inline;
                self.call_with(func, Some(recv), pos, kw, chunk)
            }
            CallRule::Native(id) => match self.natives.get(id) {
                Some(n) => self.native_val(n, Val::undef(), None, pos, kw, inline && call_safe == self.frame_safe),
                None => Err(cold_runtime("a rule names a native the runtime lacks")),
            },
            CallRule::Construct { meta, init, slots } => self.new_instance(meta, slots as usize).and_then(|made| match init {
                Some(init) => self.run_init(init, made, pos, kw, chunk, inline).map(|later| (!later).then_some(made)),
                None => Ok(Some(made)),
            }),
            CallRule::Function | CallRule::Fail | CallRule::Ask => Ok(None),
        };
        self.pending_exec_safe = false;
        made.map(Some)
    }

    /* a runtime opcode's register form past its fast path raising for an unbound operand */
    #[cold]
    #[inline(never)]
    pub(crate) fn own_reg(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, sites: &OpcodeCache) -> Result<(), VmErr> {
        let args = self.reg_row(chunk, ins.b, ins.c as usize)?;
        self.regs[self.base + ins.a as usize] = self.own_slow(ins.x, &args, rip, 0, chunk, sites)?;
        Ok(())
    }

    /* a runtime opcode over rows past its fast path taking its operands off the top */
    #[cold]
    #[inline(never)]
    pub(crate) fn own_rows(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (k, top) = (usize::from(ins.a >> 8), self.top as usize);
        if top < k + code.rows as usize || top - k >= code.frame { return Err(cold_runtime("a runtime opcode past the rows of its frame")); }
        let at = self.base + top - k;
        let args = self.regs[at..at + k].to_vec();
        self.regs[at] = self.own_slow(ins.a as u8, &args, i, k, chunk, &code.cache)?;
        self.top = (top - k + 1) as u32;
        Ok(())
    }

    /* runtime opcode `op` on `args` past its fast path at `rip` over `n` stacked operands */
    #[cold]
    #[inline(never)]
    pub(crate) fn own_slow(&mut self, op: u8, args: &[Val], rip: usize, n: usize, chunk: &SSAChunk, sites: &OpcodeCache) -> Result<Val, VmErr> {
        let own = self.rt.ops.ops.get(op as usize).ok_or_else(crate::lang::OpSet::lacks)?;
        // a fused native whose name the program bound runs that binding as its call would
        let rebound = own.fuse.and_then(|f| self.natives.function_id(f)).filter(|id| self.native_rebound(id.0)).and_then(|id| self.natives.get(id));
        if let Some(native) = rebound && let Some(f) = self.native_binding(native, self.chunk_module_id(chunk)) {
            let armed = self.redo_begin(true, rip, n);
            let called = self.call_with(f, None, args, &[], chunk);
            self.redo_done(armed);
            return self.got(called?);
        }
        match (self.rt.ops.slow)(&mut Cx::new(self), op, args) {
            Ok(v) => Ok(v),
            Err(e) => self.op_rule(e, rip, n, chunk, sites),
        }
    }

    /* the value the rule gives op `rip` of `n` stacked operands the kernels refused */
    #[cold]
    #[inline(never)]
    pub(crate) fn op_rule(&mut self, e: VmErr, rip: usize, n: usize, chunk: &SSAChunk, sites: &OpcodeCache) -> Result<Val, VmErr> {
        let VmErr::Engine(e) = e else { return Err(e) };
        let EngineError::Unsupported { op, a, b } = *e else { return Err(VmErr::Engine(e)) };
        // a unary operator keys its one operand twice
        let (ka, kb) = (Key::of(&self.heap, a), Key::of(&self.heap, b.unwrap_or(a)));
        let (pair, k) = ([a, b.unwrap_or(a)], 1 + usize::from(b.is_some()));
        // the site keeps the rule for its operands' keys asking once while classes stand
        let (ba, bb) = (key_bits(ka), key_bits(kb));
        let rule = match op_kept(sites.site(rip), op, ba, bb, self.layout_epoch) {
            Some(rule) => rule,
            None => {
                let rule = (self.rt.bind_op)(&mut Cx::new(self), op, ka, kb);
                if !matches!(rule, OpRule::Kernel | OpRule::Fail) { sites.set_site(rip, kept_op(rule, op, ba, bb, self.layout_epoch)); }
                rule
            }
        };
        match rule {
            OpRule::Native(id) => {
                let n = self.natives.get(id).ok_or_else(|| cold_runtime("a rule names a native the runtime lacks"))?;
                let v = self.native_val(n, Val::undef(), None, &pair[..k], &[], false)?;
                self.got(v)
            }
            OpRule::Call { func, swap, negate } => {
                let args = if swap { [pair[1], pair[0]] } else { pair };
                let args = &args[..k];
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                // the call runs as a frame and the op runs again taking what it gave
                let armed = self.redo_begin(true, rip, n);
                let called = self.call_with(func, None, args, &[], chunk);
                self.redo_done(armed);
                let v = self.got(called?)?;
                Ok(if negate { Val::bool(!self.truthy(v)) } else { v })
            }
            OpRule::Identity => Ok(Val::bool((a.0 == b.unwrap_or(a).0) != matches!(op, Op::Cmp(Cmp::Ne)))),
            OpRule::Kernel | OpRule::Fail => Err(VmErr::Engine(e)),
        }
    }
}
