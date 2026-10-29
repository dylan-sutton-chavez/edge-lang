use super::VM;
use crate::r#unsafe::ChunkPtr;
use crate::value::{Val, VmErr, HeapObj, DictMap, Host, BodyRef, ParkedFrame, cold_depth, cold_runtime};
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use alloc::{rc::Rc, string::String, vec, vec::Vec};
use core::cell::RefCell;
use super::{Ctor, ParamKind};
use super::act::CallFacts;
use alloc::string::ToString;
use crate::syntax::ssa::ssa_strip;
use crate::lang::{Act, CallFault, Callee, EngineError, Fit};
use super::sites::Site;
use super::act::Dest;
use super::lower;
use super::lower::Code;
use crate::code::Ins;
use super::scope::Kind;
use super::slow::attr::AttrLookup;

use super::run::global_callee;
use super::sections::{note_alt, miss, is_stub};

impl VM<'_> {
    /* the native a call of type `name` runs and None when the runtime gives none */
    #[inline(never)]
    pub(crate) fn type_native(&self, name: &str) -> Option<&'static crate::lang::Native> {
        // most other names miss on their shape before any hash
        self.natives.function(name).filter(|n| n.is_type())
    }
}

/* what the entry of a body noted for its exit */
#[derive(Clone, Copy)]
pub(crate) struct Entry {
    call_ip: Option<u32>,
}

/* a call's arguments with up to eight inline so a call allocates nothing */
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

/* whether a call opened no frame or a frame or made its value at once */
#[derive(Clone, Copy)]
pub(crate) enum Opened { No, Frame, Made(Val) }

impl Opened {
    /* whether the call needs no general path */
    #[inline(always)]
    pub(crate) fn done(self) -> bool { !matches!(self, Opened::No) }
}

impl core::ops::Deref for Args {
    type Target = [Val];
    #[inline]
    fn deref(&self) -> &[Val] {
        match self { Self::Inline(n, vals) => &vals[..*n as usize], Self::Heap(v) => v }
    }
}

/* whether a call of `f` reruns from its start once a call it made returns */
pub(crate) fn redoable(heap: &crate::value::HeapPool, f: Val) -> bool {
    use crate::lang::KindId as K;
    // the container types make their value from what they are given
    matches!(heap.type_kind(f), Some(K::List | K::Tuple | K::Set | K::Dict | K::Bytes))
}

impl<'a> VM<'a> {
    /* runs every function opcode from a call to a made function or an imported native */
    pub(crate) fn handle_function(&mut self, op: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        match op {
            OpCode::Call => self.exec_call(operand, chunk),
            OpCode::CallSpread => {
                let (callee, pos, kw) = self.spread_call((operand & 0xFF) as usize, ((operand >> 8) & 0xFF) as usize, true)?;
                // a spread call runs as a frame of this loop as a plain call does
                self.inline_call = self.pending_exec_safe;
                if let Some(v) = self.call_args(callee, &pos, &kw, chunk)? { self.push(v); }
                Ok(())
            }
            OpCode::MakeFunction | OpCode::MakeCoroutine => self.exec_make_function(op, operand, chunk),
            OpCode::CallExtern => self.call_extern(operand, chunk),
            _ => Err(cold_runtime("non-function opcode in handle_function")),
        }
    }

    fn exec_make_function(&mut self, opcode: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> { self.make_function(opcode, operand, None, chunk) }

    /* makes function `operand` of `chunk` from defaults on the stack or in registers `row` names */
    #[inline(always)]
    pub(crate) fn make_function(&mut self, opcode: OpCode, operand: u16, row: Option<(u16, u16)>, chunk: &SSAChunk) -> Result<(), VmErr> {
        let global = self.body_of(chunk).and_then(|b| b.fns.get(operand as usize).copied())
            .ok_or_else(|| cold_runtime("MakeFunction: unknown function index"))? as usize;

        if opcode == OpCode::MakeCoroutine {
            if self.is_async.len() <= global { self.is_async.resize(global + 1, false); }
            self.is_async[global] = true;
            if let Some(f) = self.call_facts.get_mut(global) { f.bits &= !super::act::OPENS; }
        }

        let n_defaults = self.functions[global].2 as usize;
        let defaults = match row {
            _ if n_defaults == 0 => vec![],
            None => self.pop_n(n_defaults)?,
            Some((_, b)) => self.reg_row(chunk, b, n_defaults)?,
        };

        // free variables take cells from the defining frame or class scope in any section's layout
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
        let val = self.heap.alloc(HeapObj::Func { fi: global, defaults: defaults.into(), cells: captures.into() })?;
        self.temp_roots.truncate(roots_base);
        // the register form names where the function goes and the stack form pushes it
        match row { Some((a, _)) => self.regs[self.base + a as usize] = val, None => self.push(val) }
        Ok(())
    }

    /* takes a spread call's operands off the stack laying out each packed spread */
    pub(crate) fn spread_call(&mut self, pos: usize, kw: usize, callee: bool) -> Result<(Val, Vec<Val>, Vec<Val>), VmErr> {
        let m = pos + 2 * kw;
        let at = self.under(m + callee as usize)?;
        let f = if callee { self.slot(at) } else { Val::undef() };
        let first = at + callee as usize;
        let (maps, packs) = core::mem::take(&mut self.pending.spread);
        let (mut out, mut keyed) = (Vec::with_capacity(pos), Vec::with_capacity(2 * kw));
        // positionals lie first and the plain operands past them pair as keywords
        let plain = pos.saturating_sub(packs.count_ones() as usize);
        let (mut seen, mut packed) = (0usize, 0u32);
        for j in 0..m {
            let v = self.slot(first + j);
            if packs >> j & 1 == 0 {
                if seen < plain { out.push(v) } else { keyed.push(v) }
                seen += 1;
                continue;
            }
            let items: &[Val] = match self.heap.try_get(v) { Some(HeapObj::Tuple(t)) => t, _ => &[] };
            // a mapping's pairs follow every keyword and a sequence's items every positional
            if maps >> packed & 1 != 0 { keyed.extend_from_slice(items); } else { out.extend_from_slice(items); }
            packed += 1;
        }
        self.cut_to(at);
        Ok((f, out, keyed))
    }

    /* calls `callee` with `recv` first when bound giving none once a frame took the call */
    pub(crate) fn call_with(&mut self, callee: Val, recv: Option<Val>, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let Some(recv) = recv else { return self.call_args(callee, positional, kw_flat, chunk) };
        let mut args = Vec::with_capacity(positional.len() + 1);
        args.push(recv);
        args.extend_from_slice(positional);
        self.call_args(callee, &args, kw_flat, chunk)
    }

    /* what a call run at once gave or the placeholder a wait inside it left */
    pub(crate) fn got(&mut self, v: Option<Val>) -> Result<Val, VmErr> {
        match v { Some(v) => Ok(v), None => self.pop() }
    }

    pub(crate) fn exec_call(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.exec_call_n((operand & 0xFF) as usize, ((operand >> 8) & 0xFF) as usize, chunk)
    }

    /* readies the op a general call of builtin `callee` made for the builtin to arm */
    #[inline(always)]
    fn native_defer(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val]) -> crate::vm::redo::Defer {
        let site = self.redo.take_site();
        if site.1 == crate::vm::redo::Rerun::No { return crate::vm::redo::Defer::No; }
        if !redoable(&self.heap, callee) { return crate::vm::redo::Defer::No; }
        let n = 1 + positional.len() + kw_flat.len();
        self.redo_defer_held(site.0 as usize, site.1, self.sp() + n, callee, positional, kw_flat)
    }

    /* runs `Call` over the top rows pushing its value unless a frame takes the call */
    pub(crate) fn exec_call_n(&mut self, num_pos: usize, num_kw: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        let k = num_pos + 2 * num_kw;
        let at = self.under(k + 1)?;
        let (callee, args) = (self.slot(at), Args::of(self.vals_at(at + 1, k)));
        self.cut_to(at);
        if let Some(v) = self.call_args(callee, &args[..num_pos], &args[num_pos..], chunk)? { self.push(v); }
        Ok(())
    }

    /* calls `callee` on `pos` and keyword pairs `kw` or none once a frame took it */
    pub(crate) fn call_args(&mut self, callee: Val, pos: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        // taken so nested native calls see false
        let call_safe = core::mem::take(&mut self.pending_exec_safe);
        let inline = core::mem::take(&mut self.inline_call);
        // a call an op's handler made before it ran again gives what it gave then
        if self.redo.on && let Some(r) = self.redo_taken(callee) {
            (self.pending.call_ip, self.pending_instance) = (None, Val::undef());
            return r.map(Some);
        }
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let num_pos = pos.len();
        // a function taking exactly these positionals binds them at once or opens in this loop
        if kw.is_empty() && let Some(&HeapObj::Func { fi, cells: ref captures, .. }) = self.heap.try_get(callee) && self.call_facts[fi].arity == Some(num_pos) {
            let plain = captures.is_empty();
            let captures = (!plain).then(|| captures.clone());
            if inline && self.call_facts[fi].opens() && call_safe == self.frame_safe && num_pos <= 8 && self.warm_code(fi) && self.open_keyed(callee, pos, kw)? { return Ok(None); }
            // a generator capturing nothing is made at once as its call does nothing else
            let generator = self.makes_gen(fi);
            if generator && plain && self.fn_scope[fi].cellvars.is_empty() { return self.make_gen(fi, pos).map(Some); }
            self.charge_step()?;
            let base = self.bind_plain(fi, callee, pos);
            // most bodies share no variable so there is no cell to make
            if (captures.is_some() || !self.fn_scope[fi].cellvars.is_empty())
                && let Err(e) = self.enter_scope(fi, captures.as_deref().unwrap_or(&[]), base)
            {
                self.close_window(self.fn_pool[fi]);
                return Err(e);
            }
            if generator { return self.gen_of_window(fi, base).map(Some); }
            return self.run_call(fi, callee, base, call_safe, inline, chunk);
        }
        self.call_bound(callee, pos, kw, call_safe, inline, chunk)
    }

    /* the suspended generator or coroutine `fi` makes of exactly its parameters `pos` */
    #[inline(never)]
    fn make_gen(&mut self, fi: usize, pos: &[Val]) -> Result<Val, VmErr> {
        self.charge_step()?;
        let mut slots = alloc::vec![Val::undef(); self.functions[fi].1.names.len()];
        for (k, &(_, slot)) in self.param_slots[fi].iter().take(pos.len()).enumerate() {
            if let Some(s) = slots.get_mut(slot) { *s = pos[k]; }
        }
        self.heap.alloc(HeapObj::Coroutine(crate::value::Coro::fresh(slots, BodyRef::Fn(fi))))
    }

    /* `call_args` past its plain functions where each callee kind binds its own arguments */
    #[inline(always)]
    fn call_bound(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val], call_safe: bool, inline: bool, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let (num_pos, num_kw) = (positional.len(), kw_flat.len() / 2);
        // charge each call so wide recursion is op-budget bounded
        self.charge_step()?;
        let not_callable = |_: &Self| -> VmErr { EngineError::cannot(Act::Call, Some(callee)) };
        if !callee.is_heap() { return self.call_rule(callee, positional, kw_flat, call_safe, inline, chunk)?.ok_or_else(|| not_callable(self)); }

        // most functions have no defaults or captures so those clone only when present
        let (fi, defaults, captures) = match self.heap.get(callee) {
            HeapObj::Func { fi: i, defaults: d, cells: c } => (*i, (!d.is_empty()).then(|| d.to_vec()), (!c.is_empty()).then(|| c.to_vec())),
            _ => {
                // bound methods and constructors and callable instances end in a call the loop may run
                if matches!(self.heap.get(callee), HeapObj::Bound { func, .. } if func.is_heap()) || matches!(self.heap.get(callee), HeapObj::Object { meta, .. } if !meta.is_type_meta()) {
                    self.pending_exec_safe = call_safe;
                    self.inline_call = inline;
                }
                // a builtin called where a frame may open in place may run its callback so
                self.job_ok = inline && call_safe == self.frame_safe;
                // a builtin given user objects hands calls to the loop rerunning its op after each
                let defer = if self.heap.users { self.native_defer(callee, positional, kw_flat) } else { crate::vm::redo::Defer::No };
                let dispatched = self.try_dispatch_non_func_callable(callee, positional, kw_flat, chunk);
                self.redo_undefer(defer);
                (self.pending_exec_safe, self.job_ok) = (false, false);
                return match dispatched? { Some(r) => Ok(r), None => self.call_rule(callee, positional, kw_flat, call_safe, inline, chunk)?.ok_or_else(|| not_callable(self)) };
            }
        };
        // a function whose parameters keywords and defaults fill opens its block in this loop
        if inline && call_safe == self.frame_safe && num_pos + num_kw <= 8 && !self.functions[fi].1.is_generator && self.warm_code(fi) && !self.is_async.get(fi).copied().unwrap_or(false)
            && self.open_keyed(callee, positional, kw_flat)? { return Ok(None); }

        let (defaults, captures) = (defaults.as_deref().unwrap_or(&[]), captures.as_deref().unwrap_or(&[]));

        let names = self.functions[fi].1.names.len();
        let base = self.open_window(self.fn_pool[fi], names + self.functions[fi].1.constants.len() + 3, names, true);
        if let Err(e) = self.bind_function_args(fi, defaults, positional, kw_flat, base).and_then(|()| self.enter_scope(fi, captures, base)) {
            self.close_window(self.fn_pool[fi]);
            return Err(e);
        }
        if self.makes_gen(fi) { return self.gen_of_window(fi, base).map(Some); }
        self.run_call(fi, callee, base, call_safe, inline, chunk)
    }

    /* whether calling `fi` makes a suspended generator or coroutine rather than running its body */
    #[inline(always)]
    fn makes_gen(&self, fi: usize) -> bool { self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) }

    /* `fi` suspended with the names bound at `base` and the rest laid on first resume */
    #[inline(never)]
    fn gen_of_window(&mut self, fi: usize, base: usize) -> Result<Val, VmErr> {
        let frame = self.regs[base..base + self.functions[fi].1.names.len()].to_vec();
        self.close_window(self.fn_pool[fi]);
        self.heap.alloc(HeapObj::Coroutine(crate::value::Coro::fresh(frame, BodyRef::Fn(fi))))
    }

    /* opens a frame whose positionals `args` are exactly its parameters giving its base */
    #[inline(always)]
    pub(crate) fn bind_plain(&mut self, fi: usize, callee: Val, args: &[Val]) -> usize {
        let n = args.len();
        // the callee runs in its window
        let names = self.functions[fi].1.names.len();
        let base = self.open_window(self.fn_pool[fi], names + self.functions[fi].1.constants.len() + 3, names, true);
        let size = names;
        match self.call_facts[fi] {
            // positionals that fill the first slots copy in at once
            CallFacts { in_loop: true, arity: Some(k), .. } if k <= size => {
                self.regs[base..base + n].copy_from_slice(&args[..n]);
                if n < k { self.fill_frame(callee, base, n, k); }
            }
            _ => for (k, &(_, slot)) in self.param_slots[fi].iter().enumerate() {
                if slot < size { self.regs[base + slot] = args[k]; }
            },
        }
        base
    }

    /* opens a plain call as its loop's next frame or false for the general path */
    #[inline(always)]
    pub(crate) fn call_in_loop(&mut self, operand: u16) -> Result<bool, VmErr> {
        if operand >> 8 != 0 { return Ok(false); }
        let num_pos = operand as usize;
        let Some(at) = self.sp().checked_sub(num_pos + 1) else { return Ok(false) };
        let (callee, args) = (self.slot(at), Args::of(self.vals_at(at + 1, num_pos)));
        match self.open_in_loop(callee, &args, Some(at))? {
            Opened::No => Ok(false),
            Opened::Frame => Ok(true),
            Opened::Made(v) => { self.push(v); Ok(true) }
        }
    }

    /* opens `callee` on `args` as the loop's next frame with the rows cut to `cut` */
    #[inline(always)]
    pub(crate) fn open_in_loop(&mut self, callee: Val, args: &[Val], cut: Option<usize>) -> Result<Opened, VmErr> {
        let n = args.len();
        let (fi, captured) = match self.heap.try_get(callee) {
            Some(&HeapObj::Func { fi, cells: ref captures, .. }) => (fi, captures.len()),
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() => return self.construct_in_loop(callee, args, cut),
            _ => return Ok(Opened::No),
        };
        // one that never ran runs the code the compiler wrote as a first call does
        let facts = &self.call_facts[fi];
        if !facts.in_loop || !facts.arity.is_some_and(|k| n <= k && n >= facts.required as usize) || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(Opened::No); }
        let p = &self.pools[facts.pool];
        if (p.code.is_none() || p.cold || p.consts.is_none()) && !self.opens_first(fi) { return Ok(Opened::No); }
        self.open_frame(fi, callee, args, cut, captured)?;
        Ok(Opened::Frame)
    }

    /* whether `fi` run at most once opens on unlowered code made here when missing */
    #[cold]
    #[inline(never)]
    fn opens_first(&mut self, fi: usize) -> bool {
        let pool = self.call_facts[fi].pool;
        let p = &self.pools[pool];
        if p.code.is_none() || p.cold { self.cold_code(fi); }
        let p = &self.pools[pool];
        p.code.is_some() && p.consts.is_some()
    }

    /* opens function `callee` on `pos` and keyword pairs `kw` in this loop when they fit */
    #[cold]
    #[inline(never)]
    pub(crate) fn open_keyed(&mut self, callee: Val, pos: &[Val], kw: &[Val]) -> Result<bool, VmErr> {
        let (num_pos, num_kw) = (pos.len(), kw.len() / 2);
        let Some(&HeapObj::Func { fi, ref defaults, .. }) = self.heap.try_get(callee) else { return Ok(false) };
        let f = self.call_facts[fi];
        // a generator or coroutine makes its object which the full path does
        if self.functions[fi].1.is_generator || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(false); }
        // a `*args` function called by position alone takes leftover positionals in a tuple
        if f.arity.is_none() && num_kw == 0 && self.pending_instance.is_undef() && let Some(p) = self.star_slot(fi) && num_pos >= p && p < 8 && !self.pools[f.pool].cold && self.pools[f.pool].code.is_some() && self.pools[f.pool].consts.is_some() {
            let rest = self.heap.alloc(HeapObj::Tuple(pos[p..].to_vec()))?;
            let mut args = [Val::undef(); 8];
            args[..p].copy_from_slice(&pos[..p]);
            args[p] = rest;
            let args = &args[..p + 1];
            if self.open_block(fi, callee, args, (super::act::NO_DEST as i64) << 24).is_none() { return Ok(false); }
            self.pending.call_ip = None;
            (self.entered, self.yielded) = (true, true);
            return Ok(true);
        }
        // keyword-only parameters each bind by name or default with every slot filled here
        if f.arity.is_none() {
            if !self.pending_instance.is_undef() || self.full_slots(fi).is_none() || self.star_slot(fi).is_some() { return Ok(false); }
            return self.open_named(callee, fi, pos, kw);
        }
        let Some(arity) = f.arity else { return Ok(false) };
        let p = &self.pools[f.pool];
        if !(f.in_loop || self.cells_only(fi)) || arity > 8 || num_pos > arity || p.code.is_none() || p.cold || p.consts.is_none()
            || self.is_async.get(fi).copied().unwrap_or(false) { return Ok(false); }
        let mut args = [Val::undef(); 8];
        args[..num_pos].copy_from_slice(pos);
        let params = &self.functions[fi].0;
        for k in 0..num_kw {
            let (name, v) = (kw[2 * k], kw[2 * k + 1]);
            let Some(HeapObj::Str(name)) = self.heap.try_get(name) else { return Ok(false) };
            let Some(p) = params.iter().position(|p| crate::syntax::ssa::param_base_name(p) == name.as_str()) else { return Ok(false) };
            // a positional-only name given by keyword is refused by the general path
            if p < num_pos || p < self.functions[fi].1.posonly as usize || !args[p].is_undef() { return Ok(false); }
            args[p] = v;
        }
        // the parameters past those given take the trailing defaults
        let first = arity - defaults.len();
        for (p, a) in args.iter_mut().enumerate().take(arity) {
            if a.is_undef() { if p < first { return Ok(false) } else { *a = defaults[p - first]; } }
        }
        // a constructor's init hands back its instance
        let instance = self.pending_instance;
        let link = (super::act::NO_DEST as i64) << 24 | if instance.is_undef() { 0 } else { super::act::CTOR << 40 };
        if self.open_block(fi, callee, &args[..arity], link).is_none() { return Ok(false); }
        if !instance.is_undef() { self.instances.push(instance); self.pending_instance = Val::undef(); }
        self.pending.call_ip = None;
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* `open_keyed` for `fi` with keyword-only parameters filled by position then name then default */
    #[inline(never)]
    fn open_named(&mut self, callee: Val, fi: usize, pos: &[Val], kw: &[Val]) -> Result<bool, VmErr> {
        use super::ParamKind;
        let (num_pos, num_kw) = (pos.len(), kw.len() / 2);
        let f = self.call_facts[fi];
        let p = &self.pools[f.pool];
        if p.code.is_none() || p.cold || p.consts.is_none() { return Ok(false); }
        let Some(HeapObj::Func { defaults, .. }) = self.heap.try_get(callee) else { return Ok(false) };
        let (params, slots) = (&self.functions[fi].0, &self.param_slots[fi]);
        let width = params.len();
        let plain = slots.iter().take_while(|(k, _)| matches!(k, ParamKind::Normal)).count();
        if width > 8 || num_pos > plain { return Ok(false); }
        let mut args = [Val::undef(); 8];
        args[..num_pos].copy_from_slice(pos);
        for k in 0..num_kw {
            let (name, v) = (kw[2 * k], kw[2 * k + 1]);
            let Some(HeapObj::Str(name)) = self.heap.try_get(name) else { return Ok(false) };
            let Some(q) = params.iter().position(|q| crate::syntax::ssa::param_base_name(q) == name.as_str()) else { return Ok(false) };
            if q < self.functions[fi].1.posonly as usize || !args[q].is_undef() { return Ok(false); }
            args[q] = v;
        }
        let mut d = 0;
        for (q, a) in args.iter_mut().enumerate().take(width) {
            let has = params[q].ends_with('=');
            if a.is_undef() { if has && d < defaults.len() { *a = defaults[d]; } else { return Ok(false) } }
            d += has as usize;
        }
        if self.open_block(fi, callee, &args[..width], (super::act::NO_DEST as i64) << 24).is_none() { return Ok(false); }
        self.pending.call_ip = None;
        (self.entered, self.yielded) = (true, true);
        Ok(true)
    }

    /* whether function `fi` holds lowered code with its constants for a block to open in */
    #[inline(always)]
    fn warm_code(&self, fi: usize) -> bool {
        let p = &self.pools[self.call_facts[fi].pool];
        p.code.is_some() && !p.cold && p.consts.is_some()
    }

    /* opens checked plain function `fi` on `args` as the next frame cutting rows to `cut` */
    #[inline(always)]
    pub(crate) fn open_frame(&mut self, fi: usize, callee: Val, args: &[Val], cut: Option<usize>, captured: usize) -> Result<(), VmErr> {
        // the call's operands leave the stack before the frame keeps where it stands
        if let Some(to) = cut { self.cut_to(to); }
        // a plain function opens its block like a register call setting the caller's place
        if args.len() <= 8 && self.open_block(fi, callee, args, (super::act::NO_DEST as i64) << 24).is_some() {
            self.pending.call_ip = None;
            (self.entered, self.yielded) = (true, true);
            return Ok(());
        }
        self.charge_step()?;
        let base = self.bind_plain(fi, callee, args);
        // a closure takes its captured cells off the function each time so none is copied
        if captured > 0 && let Some(HeapObj::Func { cells: captures, .. }) = self.heap.try_get(callee) {
            for &(slot, cell) in captures { if let Some(r) = self.regs.get_mut(base + slot) { *r = cell; } }
        }
        self.enter_frame(fi, callee)
    }

    /* whether `fi` on `n` positionals opens as a loop frame unless async or unlowered */
    #[inline(always)]
    pub(crate) fn opens_in_loop(&self, fi: usize, n: usize) -> bool {
        let facts = &self.call_facts[fi];
        facts.in_loop && facts.arity.is_some_and(|k| n <= k && n >= facts.required as usize) && !self.is_async_fn(fi)
            && self.pools[facts.pool].code.is_some() && !self.pools[facts.pool].cold && self.pools[facts.pool].consts.is_some()
    }

    /* opens a new instance's init on `args` or makes the instance at once */
    #[inline(never)]
    fn construct_in_loop(&mut self, class: Val, args: &[Val], cut: Option<usize>) -> Result<Opened, VmErr> {
        let n = args.len();
        // only a constructor the general path already ran knows its init
        let Some(&Ctor { epoch, init, exception: false, attrs }) = self.ctors.get(&class.0) else { return Ok(Opened::No) };
        if epoch != self.layout_epoch { return Ok(Opened::No); }
        let Some(init) = init else {
            // a class without an init takes no arguments and its instance is the whole call
            if n != 0 { return Ok(Opened::No); }
            self.charge_step()?;
            let instance = self.new_instance(class, attrs)?;
            if let Some(to) = cut { self.cut_to(to); }
            return Ok(Opened::Made(instance));
        };
        let Some(&HeapObj::Func { fi, cells: ref captures, .. }) = self.heap.try_get(init) else { return Ok(Opened::No) };
        if !captures.is_empty() || !self.opens_in_loop(fi, n + 1) { return Ok(Opened::No); }
        let instance = self.new_instance(class, attrs)?;
        if let Some(to) = cut { self.cut_to(to); }
        let (mut held, mut more) = ([Val::undef(); 8], Vec::new());
        let with: &[Val] = if n < 8 {
            held[0] = instance;
            held[1..n + 1].copy_from_slice(args);
            &held[..n + 1]
        } else {
            more.push(instance);
            more.extend_from_slice(args);
            &more
        };
        // its init opens its block with its instance first and what the call gives back
        if n < 8 && self.open_block(fi, init, with, (super::act::NO_DEST as i64) << 24 | super::act::CTOR << 40).is_some() {
            self.instances.push(instance);
            self.pending.call_ip = None;
            (self.entered, self.yielded) = (true, true);
            return Ok(Opened::Frame);
        }
        self.charge_step()?;
        self.pending_instance = instance;
        self.bind_plain(fi, init, with);
        self.enter_frame(fi, init)?;
        Ok(Opened::Frame)
    }

    /* a fresh `class` instance whose dict fits the last one's `attrs` fields and never regrows */
    pub(crate) fn new_instance(&mut self, class: Val, attrs: usize) -> Result<Val, VmErr> {
        let mut dict = DictMap::with_capacity(attrs);
        dict.set_shape(super::sites::ROOT_SHAPE);
        self.heap.new_instance(class, dict)
    }

    /* runs a bound call's body in the running loop or a loop of its own */
    #[inline]
    fn run_call(&mut self, fi: usize, callee: Val, base: usize, call_safe: bool, inline: bool, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let (_params, body, _, _) = self.functions[fi];
        // only a constructor's own init call may take the instance it builds
        let instance = core::mem::replace(&mut self.pending_instance, Val::undef());
        // handler calls open loop frames before the op reruns and refused frames closed their windows
        if instance.is_undef() && (self.redo.on || self.redo.lazy && self.redo_wake()) && let Some(r) = self.redo_call(fi, callee) {
            return r.map(Some);
        }
        // even a first plain call runs as a frame of its loop and not nested
        if inline && call_safe == self.frame_safe && { let p = &self.pools[self.fn_pool[fi]]; p.code.is_none() || p.cold } { self.cold_code(fi); }
        if inline && self.runs_in_loop(fi, call_safe) {
            self.pending_instance = instance;
            return self.enter_frame(fi, callee).map(|()| None);
        }

        // a native's call in a running loop takes its frame unless the host waits
        if self.depth != 0 && self.hosted == 0 { return Err(self.nested_call(fi)); }
        // these depths split a helper's levels loops and handlers out if a builtin suspends it
        let level_base = self.levels.len();
        let iter_base = self.iter_stack.len();
        let exc_base = self.exception_stack.len();
        self.pending_exec_safe = call_safe;
        let entry = self.enter_body();
        let exec_result = self.exec_in(body, base, self.fn_pool[fi]);
        self.leave_body(fi, entry, exec_result.is_ok(), chunk);

        // suspended bodies keep their frame for the coroutine and others drop it with leftover loops
        if exec_result.is_err() || !self.yielded {
            self.close_window(self.fn_pool[fi]);
            self.iter_stack.truncate(iter_base);
        }
        let result = exec_result?;

        if self.yielded {
            // a suspended sync helper parks its frame so `resume_coroutine` re-enters it at the right ip
            let helper_resume_ip = self.resume_ip;
            self.resume_ip = 0;
            let (mut levels, mut iter_delta, mut exception_delta) = (Vec::new(), Vec::new(), Vec::new());
            self.save_frames(level_base, iter_base, exc_base, &mut levels, &mut iter_delta, &mut exception_delta);
            let slots = self.window_slots(self.fn_pool[fi]);
            self.close_window(self.fn_pool[fi]);
            let layout = core::mem::take(&mut self.resume_layout);
            self.pending_sync_frames.push(ParkedFrame { ip: helper_resume_ip, fi, func: callee, slots, levels, iter_delta, exception_delta, instance, layout, top: core::mem::take(&mut self.park_top) as u16 });
            return Ok(None);
        }
        Ok(Some(result))
    }

    /* the internal error of a native's call no frame of the loop took */
    #[cold]
    #[inline(never)]
    pub(crate) fn nested_call(&mut self, fi: usize) -> VmErr {
        self.close_window(self.fn_pool[fi]);
        // a call a job refused at a limit raises here as its frame would have
        if self.too_deep() { return cold_depth(); }
        if self.budget == 0 { return VmErr::Budget; }
        debug_assert!(false, "a native's call into user code took no frame of the running loop: {}", self.functions[fi].1.qualname);
        VmErr::Runtime("internal error: a native's call into user code took no frame of the running loop")
    }

    /* keyword pairs as a dict or none when empty so the FFI sends handle zero */
    pub(crate) fn pack_kw_dict(heap: &mut crate::value::HeapPool, kw_flat: &[Val]) -> Result<Option<Val>, VmErr> {
        if kw_flat.is_empty() { return Ok(None); }
        let dm = crate::value::DictMap::from_pairs(kw_flat.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect(), heap);
        Ok(Some(heap.alloc(crate::value::HeapObj::Dict(Rc::new(RefCell::new(dm))))?))
    }

    /* dispatches non-Func callees or None for a callee no call takes */
    #[inline(always)]
    fn try_dispatch_non_func_callable(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<Option<Option<Val>>, VmErr> {
        Ok(Some(match self.heap.try_get(callee) {
            Some(&HeapObj::Native(id)) => match self.natives.native(id) {
                Some(n) => self.native_value_call(Val::undef(), n, n.owner, positional, kw_flat)?,
                // parks on a host deferral like `call_extern` for an extern reached as a module attribute
                None => {
                    let Some(func) = self.heap.host_at(id).map(|f| f.func.clone()) else { return Ok(None) };
                    if core::mem::take(&mut self.job_ok) && kw_flat.is_empty() && self.extern_held(callee, positional, &chunk.path)? { return Ok(Some(None)); }
                    self.invoke_extern(&func, positional, kw_flat, &chunk.path)?
                }
            },
            Some(&HeapObj::Bound { recv, func }) if let Some(n) = func.as_native_ref().and_then(|id| self.natives.native(id)) => self.native_value_call(recv, n, n.owner, positional, kw_flat)?,
            Some(HeapObj::Object { meta, .. }) if meta.is_type_meta() => self.call_type_object(callee, positional, kw_flat)?,
            // calling a class creates an instance and runs any init its bases define
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() => {
                let ctor = self.ctor_of(callee);
                let inline = core::mem::take(&mut self.inline_call);
                // without an init a class takes no arguments and an exception only positional ones
                if ctor.init.is_none() && (!kw_flat.is_empty() || !ctor.exception && !positional.is_empty()) {
                    let Some(name) = self.heap.class_name(callee) else { return Err(EngineError::Call(CallFault::CtorArgs).into()) };
                    return Err(EngineError::Call(CallFault::NoArgs { name: name.into(), keywords: ctor.exception, called: true }).into());
                }
                let instance = self.new_instance(callee, ctor.attrs)?;
                // an exception keeps its constructor arguments as `args` until an init resets them
                if ctor.exception { self.set_exc_args(instance, positional.to_vec())?; }
                if let Some(init) = ctor.init {
                    if self.run_init(init, instance, positional, kw_flat, chunk, inline)? { return Ok(Some(None)); }
                    if let Some(d) = self.heap.fields(instance)
                        && let Some(c) = self.ctors.get_mut(&callee.0) { c.attrs = d.borrow().len(); }
                }
                Some(instance)
            }
            // a bound user method dispatches again with its receiver prepended to the arguments
            Some(&HeapObj::Bound { recv, func }) => {
                // as in the Class branch the depth check comes before binding anything
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.call_with(func, Some(recv), positional, kw_flat, chunk)?
            }
            // generators and coroutines are not callable as only `next` or `for` or `await` run them
            Some(HeapObj::Coroutine(_)) => return Err(EngineError::cannot(Act::Call, Some(callee))),
            _ => return Ok(None),
        }))
    }

    /* what calling class `class` needs found once per layout epoch */
    pub(crate) fn ctor_of(&mut self, class: Val) -> Ctor {
        match self.ctors.get(&class.0) {
            Some(c) if c.epoch == self.layout_epoch => *c,
            _ => {
                let c = Ctor { epoch: self.layout_epoch, init: self.lookup_class_member(class, self.natives.names().constructor).map(|(f, _)| f), exception: self.exc_base(class).is_some(), attrs: 0 };
                self.ctors.insert(class.0, c);
                c
            }
        }
    }

    /* calls type object `callee` through a native of its name or the engine */
    #[inline(never)]
    fn call_type_object(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val]) -> Result<Option<Val>, VmErr> {
        let names = self.natives.names();
        // the name is read in place and copied only into an error
        let (native, object, single) = {
            let Some(name) = self.heap.type_object_name(callee) else { return Err(cold_runtime("a type call without its type")) };
            let same = |n: &str| crate::lang::same_text(name, n);
            (self.type_native(name), same(names.kind(crate::lang::KindId::Instance)), same(names.kind(crate::lang::KindId::None)) || same(names.ellipsis_type))
        };
        let named = |vm: &Self| String::from(vm.heap.type_object_name(callee).unwrap_or(""));
        // a type the runtime gives a native constructs through it
        if let Some(n) = native { return self.native_value_call(Val::undef(), n, None, positional, kw_flat); }
        let v = if self.heap.is_exc_type(callee) {
            // an exception class builds an instance holding its arguments
            if !kw_flat.is_empty() { return Err(EngineError::Call(CallFault::ExcKeywords).into()); }
            self.heap.error_of(callee, positional.to_vec())?
        } else if object {
            // the type every class ends in builds a unique featureless instance
            if !positional.is_empty() || !kw_flat.is_empty() { return Err(EngineError::Call(CallFault::NoArgs { name: named(self), keywords: false, called: true }).into()); }
            self.heap.new_instance(callee, DictMap::new())?
        } else if single {
            // the type of a singleton gives the singleton back
            if !positional.is_empty() || !kw_flat.is_empty() { return Err(EngineError::Call(CallFault::NoArgs { name: named(self), keywords: false, called: false }).into()); }
            if self.heap.type_object_name(callee).is_some_and(|n| crate::lang::same_text(n, names.ellipsis_type)) { self.builtin_value(names.ellipsis)? } else { Val::none() }
        } else {
            return Err(EngineError::Call(CallFault::Instantiate(named(self))).into());
        };
        Ok(Some(v))
    }

    /* runs the init on `instance` and is true when a frame hands it back later */
    pub(crate) fn run_init(&mut self, init_fn: Val, instance: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk, inline: bool) -> Result<bool, VmErr> {
        // fail fast before binding since the inner check fires only after the arguments bind
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        // keywords reach the init as any function and its return value drops
        (self.inline_call, self.pending_instance) = (inline, instance);
        let called = self.call_with(init_fn, Some(instance), positional, kw_flat, chunk);
        self.pending_instance = Val::undef();
        Ok(called?.is_none())
    }

    /* binds parameters from the positional and keyword buffers then fills undef slots with defaults */
    pub(crate) fn bind_function_args(&mut self, fi: usize, defaults: &[Val], positional: &[Val], kw_flat: &[Val], base: usize) -> Result<(), VmErr> {
        let size = self.functions[fi].1.names.len();
        // index by position to avoid an iterator borrow on `param_slots` across `heap.alloc`
        let n_params = self.param_slots[fi].len();
        // without a `*args` sink positionals past the normal parameters are an error
        let has_star = self.param_slots[fi].iter().any(|(k, _)| matches!(k, ParamKind::Star));
        let normal_count = self.param_slots[fi].iter().filter(|(k, _)| matches!(k, ParamKind::Normal)).count();
        if !has_star && positional.len() > normal_count {
            return Err(self.too_many_err(fi, normal_count, positional.len()));
        }
        let mut pos_idx = 0usize;
        for i in 0..n_params {
            let (kind, slot) = self.param_slots[fi][i];
            match kind {
                ParamKind::KwRest => {
                    // `**kwargs` gets only keys not bound to a named parameter
                    let (params, posonly) = (&self.functions[fi].0, self.functions[fi].1.posonly as usize);
                    // a positional-only name given by keyword lands here too
                    let pairs: Vec<(Val, Val)> = kw_flat.as_chunks::<2>().0.iter()
                        .filter(|p| match self.heap.try_get(p[0]) {
                            Some(HeapObj::Str(s)) => !params.iter().enumerate().any(|(i, pp)| i >= posonly && !pp.starts_with('*') && crate::syntax::ssa::param_base_name(pp) == s.as_str()),
                            _ => true,
                        })
                        .map(|p| (p[0], p[1])).collect();
                    let dm = DictMap::from_pairs(pairs, &self.heap);
                    let dict_val = self.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(dm))))?;
                    if slot < size { self.regs[base + slot] = dict_val; }
                }
                ParamKind::Star => {
                    // `*args` binds to an immutable tuple
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
                // `KwOnly` slots never take positionals and bind only by keyword
                ParamKind::KwOnly => {}
            }
        }

        // keyword binding is a rare path left unoptimised
        if !kw_flat.is_empty() {
            let params = &self.functions[fi].0;
            let posonly = self.functions[fi].1.posonly as usize;
            let has_kw_rest = self.param_slots[fi].iter().any(|(k, _)| matches!(k, ParamKind::KwRest));
            for pair in kw_flat.as_chunks::<2>().0 {
                // malformed keyword bytecode may leave a non-string name so the heap read is guarded
                let key = match self.heap.try_get(pair[0]) {
                    Some(HeapObj::Str(s)) => s.clone(),
                    _ => return Err(cold_runtime("malformed kwarg on stack")),
                };
                // star parameters are no keyword targets so a keyword named like one goes to `**kwargs`
                let at = params.iter().position(|p| !p.starts_with('*') && crate::syntax::ssa::param_base_name(p) == key.as_str());
                // a positional-only name given by keyword goes to `**kwargs` or is refused without one
                if at.is_some_and(|pi| pi < posonly) && !has_kw_rest {
                    return Err(self.arity_err(fi, Fit::ByKeyword(key)));
                }
                if let Some(pi) = at.filter(|&pi| pi >= posonly) {
                    let s = self.param_slots[fi][pi].1;
                    if s < size {
                        if !self.regs[base + s].is_undef() { return Err(self.arity_err(fi, Fit::Twice(key))); }
                        self.regs[base + s] = pair[1];
                    }
                } else if !has_kw_rest {
                    let near = crate::util::suggest::nearest(&key, params.iter().filter(|p| !p.starts_with('*')).map(|p| crate::syntax::ssa::param_base_name(p))).map(String::from);
                    return Err(self.arity_err(fi, Fit::Unexpected { key, near }));
                }
            }
        }

        // defaults only fill slots still undef after binding
        if !defaults.is_empty() {
            let ds = &self.default_slots[fi];
            for (di, &dv) in defaults.iter().enumerate() {
                if let Some(&(slot, _)) = ds.get(di)
                    && slot < size && self.regs[base + slot].is_undef() {
                        self.regs[base + slot] = dv;
                    }
            }
        }

        // a parameter left without a positional or keyword or default is a missing argument
        if positional.len() < n_params
            && self.param_slots[fi].iter().any(|&(k, slot)| matches!(k, ParamKind::Normal | ParamKind::KwOnly) && slot < size && self.regs[base + slot].is_undef()) {
            return Err(self.missing_err(fi, base, size));
        }

        Ok(())
    }

    /* the name a call error shows for function `fi` */
    fn callee_name(&self, fi: usize) -> &str {
        self.function_qualnames.get(fi).map(String::as_str).filter(|n| !n.is_empty()).unwrap_or(self.natives.names().anonymous)
    }

    /* the misfit `fit` of a call of function `fi` with its parameters and defaults */
    #[cold]
    #[inline(never)]
    fn arity_err(&self, fi: usize, fit: Fit) -> VmErr {
        let normal = self.param_slots[fi].iter().filter(|&&(k, _)| k == ParamKind::Normal).count();
        let defaults = self.default_slots[fi].iter().filter(|(slot, _)| self.param_slots[fi].iter().any(|&(k, s)| k == ParamKind::Normal && s == *slot)).count();
        EngineError::Arity { callee: Callee::Function { name: String::from(self.callee_name(fi)), normal: normal as u32, defaults: defaults as u32 }, fit }.into()
    }

    /* the call of `fi` given `given` positionals past the `normal` it takes */
    #[cold]
    #[inline(never)]
    fn too_many_err(&self, fi: usize, normal: usize, given: usize) -> VmErr {
        let defaults = self.default_slots[fi].iter().filter(|(slot, _)| self.param_slots[fi].iter().any(|&(k, s)| k == ParamKind::Normal && s == *slot)).count();
        EngineError::Arity { callee: Callee::Function { name: String::from(self.callee_name(fi)), normal: normal as u32, defaults: defaults as u32 }, fit: Fit::Count(given as u32) }.into()
    }

    /* the missing-argument error naming unbound positionals or else the unbound keyword-only ones */
    #[cold]
    #[inline(never)]
    fn missing_err(&self, fi: usize, base: usize, size: usize) -> VmErr {
        let unbound = |kind: ParamKind| -> Vec<&str> {
            self.param_slots[fi].iter().enumerate()
                .filter(|&(_, &(k, slot))| k == kind && slot < size && self.regs[base + slot].is_undef())
                .map(|(i, _)| crate::syntax::ssa::param_base_name(&self.functions[fi].0[i])).collect()
        };
        let (names, kw_only) = match unbound(ParamKind::Normal) { n if n.is_empty() => (unbound(ParamKind::KwOnly), true), n => (n, false) };
        let names = names.into_iter().map(String::from).collect();
        self.arity_err(fi, Fit::Missing { at: 0, names, keyword: kw_only })
    }

    /* wraps a body's cell variables and hands it the cells it closed over */
    pub(crate) fn enter_scope(&mut self, fi: usize, captures: &[(usize, Val)], base: usize) -> Result<(), VmErr> {
        let size = self.functions[fi].1.names.len();
        for k in 0..self.fn_scope[fi].cellvars.len() {
            let s = self.fn_scope[fi].cellvars[k];
            if s < size { self.regs[base + s] = self.heap.alloc(HeapObj::Cell(self.regs[base + s]))?; }
        }
        for &(s, cell) in captures { if s < size { self.regs[base + s] = cell; } }
        Ok(())
    }

    /* notes what a body needs on exit while its traceback frame waits for an error */
    pub(crate) fn enter_body(&mut self) -> Entry { Entry { call_ip: self.pending.call_ip.take() } }

    /* undoes what `enter_body` noted and a body that raised leaves its traceback frame */
    pub(crate) fn leave_body(&mut self, fi: usize, entry: Entry, ok: bool, chunk: &SSAChunk) {
        if !ok { self.note_frame(fi, entry.call_ip, chunk); }
    }

    /* notes the frame of `fi` an error unwound as called at `call_ip` of `chunk` */
    pub(crate) fn note_frame(&mut self, fi: usize, call_ip: Option<u32>, chunk: &SSAChunk) {
        // a position from another text would misread here so the error points at this call
        if !alloc::sync::Arc::ptr_eq(&self.functions[fi].1.source, &chunk.source) { self.error_byte_pos = None; }
        let call_byte_pos = call_ip.and_then(|ip| chunk.resolve_call(ip).or_else(|| chunk.resolve(ip))).unwrap_or(0);
        // a recursion unwinds the same call repeatedly which the frame below only counts
        if let Some(last) = self.call_stack.last_mut()
            && last.fi == fi && last.call_byte_pos == call_byte_pos && last.caller_path.as_ref().is_some_and(|p| alloc::sync::Arc::ptr_eq(p, &chunk.path))
        {
            last.repeats += 1;
            return;
        }
        // a traceback shows the innermost frames so a long chain keeps its first few hundred
        if self.call_stack.len() >= crate::value::MAX_CALLS { return; }
        // frames go innermost first with their caller's text so rendering borrows no live chunk
        self.call_stack.push(crate::value::CallFrame { fi, call_byte_pos, caller_source: Some(chunk.source.clone()), caller_path: Some(chunk.path.clone()), repeats: 0 });
    }

    /* `CallExtern` packs `(extern_idx<<8)|(kw<<4)|pos` and its keyword pairs go in one kwargs dict */
    pub(crate) fn call_extern(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let extern_idx = (operand >> 8) as usize;
        // a star spread grows the counts the operand was written with
        let (_, positional, kw_flat) = self.spread_call((operand & 0xF) as usize, ((operand >> 4) & 0xF) as usize, false)?;
        let extern_fn = chunk.extern_table.get(extern_idx).ok_or_else(|| cold_runtime("CallExtern: extern index out of bounds"))?;
        let func = extern_fn.func.clone(); // an Arc clone that only bumps the refcount
        if core::mem::take(&mut self.job_ok) && kw_flat.is_empty() && positional.iter().any(|&v| self.gen_leaf(v, 0).is_some()) {
            let callee = self.with_roots(positional.iter().copied(), |vm| vm.heap.host_value(extern_fn))?;
            if self.extern_held(callee, &positional, &chunk.path)? { return Ok(()); }
        }
        if let Some(v) = self.invoke_extern(&func, &positional, &kw_flat, &chunk.path)? { self.push(v); }
        Ok(())
    }

    /* whether generators under `pos` are held as jobs before binding `callee` runs on them */
    #[cold]
    #[inline(never)]
    fn extern_held(&mut self, callee: Val, pos: &[Val], caller: &alloc::sync::Arc<String>) -> Result<bool, VmErr> {
        self.extern_caller = Some(caller.clone());
        self.hold_start(core::iter::once(callee).chain(pos.iter().copied()).collect())
    }

    /* calls the native binding first in `call` on the rest giving none once it parked */
    #[inline(never)]
    pub(crate) fn extern_call(&mut self, call: &[Val]) -> Result<Option<Val>, VmErr> {
        let Some(f) = self.heap.host_of(call[0]) else { return Err(cold_runtime("a held call lost its binding")) };
        let func = f.func.clone();
        let caller = self.extern_caller.clone().unwrap_or_else(|| self.chunk.path.clone());
        self.invoke_extern(&func, &call[1..], &[], &caller)
    }

    /* roots a native binding's args as a plugin may call back into code that collects */
    fn invoke_extern(&mut self, func: &crate::value::ExternCallable, positional: &[Val], kw_flat: &[Val], caller: &alloc::sync::Arc<String>) -> Result<Option<Val>, VmErr> {
        self.extern_caller = Some(caller.clone());
        let kwargs = Self::pack_kw_dict(&mut self.heap, kw_flat)?;
        let roots = positional.iter().chain(kw_flat).copied().chain(kwargs);
        match self.with_roots(roots, |vm| vm.lent(|vm| func(&mut vm.heap, positional, kwargs))) {
            Ok(result) => Ok(Some(result)),
            Err(VmErr::HostCallDeferred) => { self.park_host_call(); Ok(None) }
            Err(e) => Err(e),
        }
    }

    /* `f` over a loan of the machine the host's calls back reach while it runs */
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    #[inline(always)]
    fn lent<R>(&mut self, f: impl FnOnce(&mut VM<'static>) -> R) -> R {
        let (_loan, vm) = crate::abi::bridge::Lend::of(self);
        f(vm)
    }

    /* `f` over the machine where no host calls back */
    #[cfg(not(all(target_arch = "wasm32", feature = "runtime")))]
    #[inline(always)]
    fn lent<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        // a host in this process reads which call runs while its binding does
        #[cfg(feature = "std")]
        let _now = super::calling::Now::enter(self.next_host_call_id, self.extern_caller.clone());
        f(self)
    }

    /* parks a host-deferred native under an id and yields with a placeholder `set_host_result_by_id` overwrites */
    fn park_host_call(&mut self) {
        // lowered code takes its placeholder as it parks
        if self.top != 0 { self.push(Val::none()); }
        self.pending.host_call_id = self.next_host_call_id;
        self.next_host_call_id += 1;
        self.pending.host_call_request = true;
        self.yielded = true;
    }
}

/* the value an opening made at once or none for a frame it opened */
#[inline(always)]
pub(crate) fn made(opened: crate::vm::calls::Opened) -> Option<Val> {
    match opened { crate::vm::calls::Opened::Made(v) => Some(v), _ => None }
}

// most times code is laid again for its callees past the two for class changes

// the take of a statement's call whose value goes nowhere
const VOID_TAKE: usize = usize::MAX;

impl<'a> VM<'a> {

    /* resolves the receiver's method and calls it directly with arguments from `CallMethodArgs` */
    #[allow(clippy::too_many_arguments)]
    /* the items of a tuple or list of at most three as a call's positionals */
    pub(crate) fn few_items(&self, seq: Val) -> Option<([Val; 3], usize)> {
        let mut out = [Val::undef(); 3];
        let n = match self.heap.try_get(seq)? {
            HeapObj::Tuple(v) if v.len() <= 3 => { out[..v.len()].copy_from_slice(v); v.len() }
            HeapObj::List(rc) => { let v = rc.borrow(); if v.len() > 3 { return None; } out[..v.len()].copy_from_slice(&v); v.len() }
            _ => return None,
        };
        Some((out, n))
    }

    /* the method call at `rip` over the top rows pushing any value made at once */
    pub(crate) fn exec_call_method(&mut self, attr_idx: u16, call_op: u16, rip: usize, code: &Code, chunk: &SSAChunk) -> Result<(), VmErr> {
        let raw = call_op as usize;
        let (num_pos, num_kw) = (raw & 0xFF, (raw >> 8) & 0xFF);
        let total = num_pos + 2 * num_kw;
        let at = self.under(total + 1)?;
        let (obj, args) = (self.slot(at), Args::of(self.vals_at(at + 1, total)));
        self.cut_to(at);
        if let Some(v) = self.method_call(attr_idx, obj, &args[..num_pos], &args[num_pos..], rip, code, chunk)? { self.push(v); }
        Ok(())
    }

    /* calls method `attr_idx` of `obj` at `rip` giving none once a frame took the call */
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn method_call(&mut self, attr_idx: u16, obj: Val, positional: &[Val], kw_flat: &[Val], rip: usize, code: &Code, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let cache = &code.cache;
        // borrow rather than clone since `chunk` outlives every `&mut self` call below
        let Some(name) = chunk.names.get(attr_idx as usize) else { return Err(VmErr::Runtime("CallMethod: bad name index")) };
        let site = cache.site(rip);
        if let Some((n, owner)) = self.site_native(site, obj) { self.job_ok = false; return self.native_val(n, obj, Some(owner), positional, kw_flat, true); }
        // any call past a builtin one may run user code whose frame points back here
        self.pending.call_ip = Some(code.orig(rip));
        // a known method calls with the receiver slotted in as its first argument
        if let Some(func) = self.site_method(site, obj, name) {
            let mut args = Vec::with_capacity(positional.len() + 1);
            args.push(obj);
            args.extend_from_slice(positional);
            if kw_flat.is_empty() && let opened = self.open_in_loop(func, &args, None)? && opened.done() { return Ok(made(opened)); }
            (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
            let called = self.call_args(func, &args, kw_flat, chunk);
            self.pending_exec_safe = false;
            return called;
        }
        if let Some(value) = self.site_get(site, obj) {
            // a function the instance or module holds opens as a frame of this loop
            if kw_flat.is_empty() && let opened = self.open_in_loop(value, positional, None)? && opened.done() { return Ok(made(opened)); }
            (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
            let called = self.call_args(value, positional, kw_flat, chunk);
            self.pending_exec_safe = false;
            return called;
        }
        let lookup = match self.find_attr(obj, name) {
            Ok(Some(l)) => l,
            Ok(None) => {
                if let Some(f) = self.get_rule(obj, name, crate::lang::Access::Method, Some((cache, rip)))? { return self.call_in_loop_with(f, positional, kw_flat, chunk); }
                self.note_missing_attr(obj, name);
                return Err(self.no_attr(obj, name));
            }
            Err(other) => return Err(other),
        };
        match lookup {
            AttrLookup::ModuleAttr(callee) => {
                cache.set_site(rip, self.learn_get(obj, name));
                // a module's function is noted like an instance's so code laid again calls its section
                if matches!(self.heap.try_get(callee), Some(HeapObj::Func { .. })) { self.note_called(code, chunk, rip, callee, lower::FIELD); }
                self.call_in_loop_with(callee, positional, kw_flat, chunk)
            }
            AttrLookup::ClassMember(callee)
            // an instance attribute callable gets no receiver as only class-level functions bind
            | AttrLookup::InstanceField(callee) => {
                let func = matches!(self.heap.try_get(callee), Some(HeapObj::Func { .. }));
                if func {
                    self.note_class(code, chunk, rip, obj);
                    // one the instance holds is noted so the code laid again calls its section
                    if self.heap.class_of(obj).is_some() { self.note_called(code, chunk, rip, callee, lower::FIELD); }
                }
                // a function a class or an instance holds runs as a frame of this loop
                if !func { return self.call_in_loop_with(callee, positional, kw_flat, chunk); }
                if kw_flat.is_empty() && positional.len() <= 3 && let opened = self.open_in_loop(callee, positional, None)? && opened.done() { return Ok(made(opened)); }
                (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
                let called = self.call_with(callee, None, positional, kw_flat, chunk);
                self.pending_exec_safe = false;
                called
            }
            AttrLookup::InstanceMethod { recv, func } => {
                cache.set_site(rip, self.learn_method(obj, name, func));
                // prepend self as method bodies stage like plain calls
                self.pending_exec_safe = self.frame_safe;
                self.inline_call = true;
                let called = self.call_with(func, Some(recv), positional, kw_flat, chunk);
                self.pending_exec_safe = false;
                called
            }
            AttrLookup::Native(recv, n, owner) => {
                if recv.0 == obj.0 { cache.set_site(rip, self.learn_native(obj, n, owner)); }
                self.job_ok = true;
                self.native_value_call(recv, n, Some(owner), positional, kw_flat)
            }
            AttrLookup::Name(_) | AttrLookup::Int(_) => {
                // calling a non callable `e.args()` or function name reports a missing attribute
                Err(EngineError::NoAttribute { recv: obj, name: String::from(name), store: false }.into())
            }
        }
    }

    /* calls `callee` from an op of this loop so a function opens as its frame */
    #[inline(never)]
    fn call_in_loop_with(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
        let called = self.call_with(callee, None, positional, kw_flat, chunk);
        (self.inline_call, self.pending_exec_safe) = (false, false);
        called
    }

    /* runs `f` with each job it opens handing its value to the take at `take` */
    #[inline(never)]
    fn taking<R>(&mut self, code: &Code, take: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        self.delivering(if take == VOID_TAKE { Dest::Void } else { Self::take_dest(code, take) }, f)
    }

    /* a register call past its arm giving the next index or None keeping the error */
    #[inline(never)]
    pub(crate) fn call_reg(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        if lower::wide(ins) { return self.call_wide(ins, i, code, chunk); }
        let called = if ins.op != OpCode::CallMethodR {
            let (r, n) = (&self.regs[self.base..], ins.x as usize);
            let callee = if ins.op == OpCode::CallGlobalR { global_callee(&self.scopes, code, ins) } else { r[ins.a as usize] };
            let vals = [callee, r[ins.b as usize], r[ins.c as usize]];
            // an unbound operand leaves for the slow path which names it
            if vals[..=n].iter().any(|v| v.is_undef()) { return None; }
            self.call_value(vals[0], &vals[1..=n], (i, i + 1), code, chunk).map(|v| self.after_call(code, i + 1, v))
        } else {
            let (r, n) = (&self.regs[self.base..], ins.x as usize);
            let vals = [r[ins.b as usize], r[ins.c as usize]];
            if vals[..=n].iter().any(|v| v.is_undef()) { return None; }
            if let Some((m, owner)) = self.site_native(code.cache.site(i), vals[0]) {
                self.job_ok = false;
                // a native that steps hands what its job gives to the call's take
                let v = if m.steps() { self.taking(code, i + 1, |vm| vm.native_val(m, vals[0], Some(owner), &vals[1..=n], &[], true)) } else { self.native_val(m, vals[0], Some(owner), &vals[1..=n], &[], true) };
                return v.map(|v| self.after_call(code, i + 1, v)).map_err(|e| self.reg_error = Some(e)).ok();
            }
            // a method the site knows opens straight from the registers with the receiver first
            if let Some(name) = chunk.names.get(ins.a as usize) && let Some(func) = self.site_method(code.cache.site(i), vals[0], name) {
                self.pending.call_ip = Some(code.orig(i));
                match self.open_in_loop(func, &vals[..=n], None) {
                    Ok(Opened::Frame) => return Some(i + 1),
                    Ok(Opened::Made(v)) => return Some(self.deliver_at(code, i + 1, v)),
                    Ok(Opened::No) => {}
                    Err(e) => { self.reg_error = Some(e); return None; }
                }
            }
            // any other receiver and argument take the method call's full path
            self.taking(code, i + 1, |vm| vm.method_call(ins.a, vals[0], &vals[1..=n], &[], i, code, chunk)).map(|v| self.after_call(code, i + 1, v))
        };
        called.map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* the call `ins` over the top rows noting what it calls for a later lowering */
    #[inline(never)]
    pub(crate) fn call_op(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        // unlowered code learns its callees for later lowering with keyword pairs past the eighth bit
        let callee = self.sp().checked_sub((ins.a & 0xFF) as usize + 2 * (ins.a >> 8) as usize + 1).map_or(Val::undef(), |k| self.slot(k));
        // headless keyword calls ask for a laid head first and missed ones count toward re-laying
        if (ins.a >> 8 != 0 || ins.a > 5) && code.block.is_some() && self.budget != 0 && (matches!(self.heap.try_get(callee), Some(HeapObj::Func { .. })) || self.heap.is_class(callee)) && self.site_grows(code, chunk, i, callee) { return Some(i); }
        if ins.a >> 8 != 0 && code.block.is_some() && code.misses.get() < lower::MISSES { self.kw_unlaid(code, chunk, i, ins.a); }
        match self.heap.try_get(callee) {
            Some(&HeapObj::Bound { func, .. }) if func.is_heap() && ins.a < 0x100 && self.lays_later(code) => self.note_called(code, chunk, i, func, lower::BOUND),
            // a register call's generator is noted too once a loop over it nears its lowering
            Some(&HeapObj::Func { fi, .. }) if ins.a < 0x100 && code.unchanged() && code.hot.get() >= lower::HOT_LOOP / 2 && self.functions.get(fi).is_some_and(|f| f.1.is_generator) => self.note_called(code, chunk, i, callee, lower::GEN),
            // a keyword or wide call's stub counts functions it meets so re-laid code includes them
            Some(HeapObj::Func { .. }) if code.block.is_some() && code.misses.get() < lower::MISSES && is_stub(code, i) => note_alt(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, i, callee, false),
            // a class's stub counts toward re-laying as its constructor was laid for the old class
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() && code.block.is_some() && code.misses.get() < lower::MISSES && is_stub(code, i) => miss(&mut self.preempt_left, code),
            _ => {}
        }
        self.pending.call_ip = Some(code.orig(i));
        let called = match self.call_in_loop(ins.a) {
            Ok(true) => Ok(()),
            Ok(false) => self.ruled_call(callee, i, code, |vm| vm.call_general(ins.a, chunk, (i, super::redo::Rerun::Stack))),
            Err(e) => Err(e),
        };
        called.map(|()| { if self.entered && code.unchanged() { self.learn_call(i, code, callee); } i + 1 }).map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* a register call past two operands reading its values straight from the registers */
    #[inline(never)]
    fn call_wide(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let (regs, count) = lower::call_regs(ins, *code.ins.get(i + 1)?);
        if ins.op == OpCode::CallMethodR && count <= 4 && let Some((m, owner)) = self.site_native(code.cache.site(i), self.regs[self.base + regs[0] as usize]) {
            let r = &self.regs[self.base..];
            let vals = [r[regs[0] as usize], r[regs[1] as usize], r[regs[2] as usize], r[regs[3] as usize]];
            if vals[..count].iter().any(|v| v.is_undef()) { return None; }
            self.job_ok = false;
            let v = if m.steps() { self.taking(code, i + 2, |vm| vm.native_val(m, vals[0], Some(owner), &vals[1..count], &[], true)) } else { self.native_val(m, vals[0], Some(owner), &vals[1..count], &[], true) };
            return v.map(|v| self.after_call(code, i + 2, v)).map_err(|e| self.reg_error = Some(e)).ok();
        }
        // any other callee takes its arguments straight from the registers
        if ins.op != OpCode::CallMethodR {
            let r = &self.regs[self.base..];
            let callee = if ins.op == OpCode::CallGlobalR { self.scopes[code.module].read(ins.a as u32) } else { r[regs[0] as usize] };
            let mut vals = [Val::undef(); 5];
            for (v, &k) in vals.iter_mut().zip(&regs[1..count]) { *v = r[k as usize]; }
            // an unbound operand leaves for the handler which names it
            if callee.is_undef() || vals[..count - 1].iter().any(|v| v.is_undef()) { return None; }
            return self.call_value(callee, &vals[..count - 1], (i, i + 2), code, chunk).map(|v| self.after_call(code, i + 2, v)).map_err(|e| self.reg_error = Some(e)).ok();
        }
        let r = &self.regs[self.base..];
        let mut vals = [Val::undef(); 6];
        for (v, &k) in vals.iter_mut().zip(&regs[..count]) { *v = r[k as usize]; }
        // an unbound operand leaves for the handler which names it
        if vals[..count].iter().any(|v| v.is_undef()) { return None; }
        self.taking(code, i + 2, |vm| vm.method_call(ins.a, vals[0], &vals[1..count], &[], i, code, chunk)).map(|v| self.after_call(code, i + 2, v)).map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* where the loop goes past a register call returning to `ret` with `v` */
    #[inline(always)]
    fn after_call(&mut self, code: &Code, ret: usize, v: Option<Val>) -> usize {
        match v { Some(v) => self.deliver_at(code, ret, v), None => ret }
    }

    /* puts what a call returning to `ret` gave where the word there takes it */
    #[inline(always)]
    pub(crate) fn deliver_at(&mut self, code: &Code, ret: usize, v: Val) -> usize {
        match code.ins.get(ret) {
            Some(t) if t.op == OpCode::StoreTopR && t.x == 0 => { self.regs[self.base + t.a as usize] = v; ret + 1 }
            Some(t) if t.op == OpCode::StoreTopR => { self.scopes[code.module].set_at(t.a as u32, v); ret + 1 }
            _ => { self.push(v); ret }
        }
    }

    /* calls `callee` on `args` at `i` giving none once a frame took the call */
    #[inline(always)]
    fn call_value(&mut self, callee: Val, args: &[Val], (i, take): (usize, usize), code: &Code, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let n = args.len();
        self.pending.call_ip = Some(code.orig(i));
        // the function this call opened last opens again unchecked
        if n <= 2 && let Site::Call { callee: c, fi } = code.cache.site(i) && c.0 == callee.0 && self.call_facts[fi as usize].in_loop {
            return self.open_frame(fi as usize, c, args, None, self.call_facts[fi as usize].closure as usize).map(|()| None);
        }
        match self.heap.try_get(callee) {
            Some(o) if n <= 2 && opens_in_loop(o) => match self.open_in_loop(callee, args, None)? {
                Opened::Frame => { self.learn_call(i, code, callee); return Ok(None); }
                Opened::Made(v) => return Ok(Some(v)),
                Opened::No => {}
            },
            // a runtime native runs on the registers its jobs handing values to the take
            Some(&HeapObj::Native(id)) if let Some(m) = self.natives.native(id) => return self.taking(code, take, |vm| vm.native_value_direct(Val::undef(), m, m.owner, args)),
            Some(&HeapObj::Bound { recv, func }) if let Some(m) = func.as_native_ref().and_then(|id| self.natives.native(id)) => return self.taking(code, take, |vm| vm.native_value_direct(recv, m, m.owner, args)),
            // a bound method's function is noted for code that may lay it receiver first
            Some(&HeapObj::Bound { func, .. }) if self.lays_later(code) => self.note_called(code, chunk, i, func, lower::BOUND),
            _ => {}
        }
        // any other callee takes the general call
        let v = if n <= 2 { self.ruled_call(callee, i, code, |vm| vm.taking(code, take, |vm| vm.call_general_args(callee, args, chunk, (i, super::redo::Rerun::Regs))))? } else {
            self.pending.call_ip = Some(code.orig(i));
            match self.open_in_loop(callee, args, None)? {
                Opened::Frame => None,
                Opened::Made(v) => Some(v),
                Opened::No => self.ruled_call(callee, i, code, |vm| vm.taking(code, take, |vm| vm.call_general_args(callee, args, chunk, (i, super::redo::Rerun::Regs))))?,
            }
        };
        if self.entered { self.learn_call(i, code, callee); }
        Ok(v)
    }

    /* call `f` of `callee` at `i` trying the kept rule and keeping one asked */
    #[inline(always)]
    fn ruled_call<R>(&mut self, callee: Val, i: usize, code: &Code, f: impl FnOnce(&mut Self) -> Result<R, VmErr>) -> Result<R, VmErr> {
        // a rule an outer call holds for its own site waits while this one runs
        let held = if self.rule_hand.is_some() { self.rule_hand.take() } else { None };
        if code.cache.with_site(i, |s| matches!(s, Site::Rule { .. })) { self.rule_hand = Some(code.cache.site(i)); }
        let r = f(self);
        // what a call rule handed back is the rule this site keeps for its callee
        if self.rule_hand.is_some() { self.keep_hand(callee, i, code); }
        if held.is_some() { self.rule_hand = held; }
        r
    }

    /* keeps at site `i` the call rule a call of `callee` handed back */
    #[cold]
    #[inline(never)]
    fn keep_hand(&mut self, callee: Val, i: usize, code: &Code) {
        if let Some(kept) = self.rule_hand.take() && matches!(kept, Site::Rule { k1, .. } if k1 == callee.0) { code.cache.set_site(i, kept); }
    }

    /* the CallNativeR at `i` past its fast path and where the loop goes on */
    #[inline(never)]
    pub(crate) fn native_regs(&mut self, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let ins = code.ins[i];
        let (n, wide, void) = ((ins.x & lower::ARGC) as usize, lower::wide(ins), ins.x & lower::VOID != 0);
        let rest = if wide { code.ins[i + 1] } else { ins };
        let (take, at) = (i + 1 + wide as usize, [ins.b, ins.c, rest.a, rest.b, rest.c]);
        let r = &self.regs[self.base..];
        let mut args = [Val::undef(); lower::NATIVE_ARGS];
        for (v, &k) in args.iter_mut().zip(&at[..n]) { *v = r[k as usize]; }
        // an unbound argument leaves for the handler which names it
        if args[..n].iter().any(|v| v.is_undef()) { return None; }
        let Some(f) = self.natives.get(crate::lang::NativeId(ins.a)) else { self.reg_error = Some(super::steps::no_native()); return None };
        self.pending.call_ip = Some(code.orig(i));
        if ins.x & lower::PINNED == 0 && self.native_rebound(ins.a) && let Some(binding) = self.native_binding(f, code.module_at(i)) {
            if !void { return self.call_value(binding, &args[..n], (i, take), code, chunk).map(|v| self.after_call(code, take, v)).map_err(|e| self.reg_error = Some(e)).ok(); }
            // a statement's binding gives its value nowhere and its frame returns past the call
            if let Err(e) = self.delivering(Dest::Void, |vm| vm.call_value(binding, &args[..n], (i, VOID_TAKE), code, chunk)) { self.reg_error = Some(e); return None; }
            self.void_frame(take - 1);
            return Some(take);
        }
        // a plain output writes its line here
        if let Some(out) = f.fast_out && self.plain(&args[..n]) {
            match self.out_plain(out, &args[..n]) {
                Ok(true) if void => return Some(take),
                Ok(true) => { self.regs[self.base + code.ins[take].a as usize] = Val::none(); return Some(take + 1); }
                Ok(false) => {}
                Err(e) => { self.reg_error = Some(e); return None; }
            }
        }
        // the value and any job's value land in the take's register or nowhere
        let to = if void { Dest::Void } else { Dest::Word(code.ins[take].a) };
        match self.delivering(to, |vm| vm.call_native(f, Val::undef(), None, &args[..n], &[], f.steps(), to)) {
            Ok(true) => Some(take + usize::from(!void)),
            Ok(false) => {
                if void { self.void_frame(take - 1); }
                Some(take)
            }
            Err(e) => { self.reg_error = Some(e); None }
        }
    }

    /* keeps in site `i` the function or class with an init its call opened */
    pub(crate) fn learn_call(&self, i: usize, code: &Code, callee: Val) {
        // a function's first two calls keep its sites as the second replaces its first code
        if !self.entered || self.acts == 0 || self.pools[self.top_pool].calls <= 2 { return; }
        let Some(f) = self.top() else { return };
        if f.callee.0 == callee.0 && f.instance.is_undef() && matches!(self.heap.try_get(callee), Some(HeapObj::Func { .. })) {
            code.cache.set_site(i, Site::Call { callee, fi: f.fi as u32 });
        } else if !f.instance.is_undef() && let Some(&super::Ctor { epoch, init: Some(init), exception: false, attrs }) = self.ctors.get(&callee.0)
            && init.0 == f.callee.0 && epoch == self.layout_epoch {
            code.cache.set_site(i, Site::Ctor { class: callee.as_heap(), init, fi: f.fi as u32, attrs: attrs.min(u16::MAX as usize) as u16, epoch });
        }
    }

    /* the general stacked call on `count` positionals which may still run in the loop */
    #[inline(always)]
    fn call_general(&mut self, count: u16, chunk: &SSAChunk, site: (usize, super::redo::Rerun)) -> Result<(), VmErr> {
        // until the program makes a class with special methods no builtin call runs user code
        if self.heap.users { return self.call_sited(count, chunk, site); }
        self.pending_exec_safe = self.frame_safe;
        self.inline_call = true;
        let called = self.exec_call(count, chunk);
        self.pending_exec_safe = false;
        called
    }

    /* `call_general` of `callee` on `args` given at once or none once a frame took it */
    #[inline(never)]
    fn call_general_args(&mut self, callee: Val, args: &[Val], chunk: &SSAChunk, site: (usize, super::redo::Rerun)) -> Result<Option<Val>, VmErr> {
        (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
        // until the program makes a class with special methods no builtin call runs user code
        if self.heap.users { self.redo.set_site((site.0 as u32, site.1)); }
        let called = self.call_args(callee, args, &[], chunk);
        self.pending_exec_safe = false;
        if self.heap.users { self.redo.end_site(); }
        called
    }

    /* `call_general` where a dispatched builtin knows its op to rerun after a call it makes */
    #[inline(never)]
    fn call_sited(&mut self, count: u16, chunk: &SSAChunk, site: (usize, super::redo::Rerun)) -> Result<(), VmErr> {
        self.pending_exec_safe = self.frame_safe;
        self.inline_call = true;
        self.redo.set_site((site.0 as u32, site.1));
        let called = self.exec_call(count, chunk);
        self.pending_exec_safe = false;
        self.redo.end_site();
        called
    }

    /* `*a` or `**a` spreading the popped value as an argument list's positionals or keyword pairs */
    pub(crate) fn unpack_args(&mut self, operand: u16, rip: Option<usize>) -> Result<(), VmErr> {
        let val = self.pop()?;
        // a spread packs into one value its call lays out as it runs
        let packed = match operand & 0x3 {
            // a tuple never changes so it goes as its own pack
            1 if matches!(self.heap.try_get(val), Some(HeapObj::Tuple(_))) => val,
            1 => {
                let val = match rip { Some(rip) if matches!(self.heap.try_get(val), Some(HeapObj::Iter(..))) => match self.lazy_items(val, rip)? { Some(v) => v, None => return Ok(()) }, _ => val };
                let items = self.extract_iter(val)?;
                self.heap.alloc(HeapObj::Tuple(items))?
            }
            2 => {
                let pairs = self.mapping_to_kw_pairs(val)?;
                let flat: Vec<Val> = pairs.into_iter().flat_map(|(k, v)| [k, v]).collect();
                self.heap.alloc(HeapObj::Tuple(flat))?
            }
            _ => return Err(cold_runtime("UnpackArgs: bad operand")),
        };
        self.push(packed);
        Ok(())
    }

    #[inline(never)]
    pub(crate) fn exec_make_class(&mut self, op: u16, ip: usize, chunk: &SSAChunk, row: Option<u16>, dest: u16) -> Result<(), VmErr> {
        // the operand is the class chunk index and the body keeps its base count
        let class_idx = op as usize;
        let num_bases = chunk.classes.get(class_idx).map_or(0, |b| b.bases as usize);
        // pop the bases first so a misencoded operand fails before the body is touched
        let mut bases = if num_bases == crate::syntax::ssa::SPREAD_BASES as usize {
            let list = match row { None => self.pop()?, Some(b) => self.reg(chunk, b)? };
            match self.heap.try_get(list) { Some(HeapObj::List(l)) => l.borrow().clone(), _ => return Err(cold_runtime("spread bases corrupted")) }
        } else {
            match row { None => self.pop_n(num_bases)?, Some(b) => self.reg_row(chunk, b, num_bases)? }
        };
        self.check_bases(&mut bases)?;
        let Some(body) = chunk.classes.get(class_idx) else {
            return Err(cold_runtime("class index out of range"));
        };
        // the cells around the class from its defining function or its enclosing class
        let around: Vec<(String, Val)> = match self.fi_of(chunk) {
            Some(fi) => chunk.names.iter().enumerate()
                .filter(|&(i, _)| self.fn_scope[fi].kinds.get(i) == Some(&Kind::Cell))
                .filter_map(|(i, n)| self.regs.get(self.base + i).filter(|&&c| matches!(self.heap.try_get(c), Some(HeapObj::Cell(_)))).map(|&c| (String::from(ssa_strip(n)), c)))
                .collect(),
            None if self.is_class_body(chunk) => self.class_cells.last().cloned().unwrap_or_default(),
            None => Vec::new(),
        };
        // members are only slots the body stores so loaded builtins or globals never leak in
        let mut member = alloc::vec![false; body.names.len()];
        for ins in body.instructions.iter().filter(|i| matches!(i.opcode, OpCode::StoreName | OpCode::Phi)) {
            if let Some(m) = member.get_mut(ins.operand as usize) { *m = true; }
        }
        // a defining function lends plain locals by value since the body runs before they change
        let lender = self.fi_of(chunk);
        let local = |bare: &str| -> Option<Val> {
            let fi = lender?;
            chunk.names.iter().enumerate()
                .filter(|&(i, n)| self.fn_scope[fi].kinds.get(i) == Some(&Kind::Local) && ssa_strip(n) == bare)
                .find_map(|(i, _)| self.regs.get(self.base + i).copied().filter(|v| !v.is_undef()))
        };
        // a class body reads enclosing variables first then its module then the builtins
        let module = self.chunk_module_id(body);
        let mut class_slots = alloc::vec![Val::undef(); body.names.len()];
        let reads = !around.is_empty() || lender.is_some();
        for (i, name) in body.names.iter().enumerate() {
            let bare = ssa_strip(name);
            // bare names start from the entry module's binding first and versioned ones only from builtins
            let first = if bare.len() == name.len() { self.global(name) } else {
                self.builtins.get(name.as_str()).or_else(|| name.strip_suffix("_0").and_then(|b| self.builtins.get(b))).copied()
            };
            if let Some(v) = first { class_slots[i] = v; }
            // a body-assigned name is its own and an earlier read falls to the module
            let enclosing = if member[i] || !reads { None } else {
                around.iter().find(|(n, _)| n == bare).map(|&(_, c)| self.deref(c)).or_else(|| local(bare))
            };
            if let Some(v) = enclosing {
                class_slots[i] = v;
            } else if class_slots[i].is_undef() {
                // the entry module was searched for bare names above and is module zero itself
                let gv = if module == 0 { if bare.len() == name.len() { None } else { self.global(bare) } }
                    else { self.scopes[module].get(bare).or_else(|| self.global(bare)) };
                if let Some(gv) = gv { class_slots[i] = gv; }
            }
        }
        // it nests as deep as its own run would
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.class_cells.push(around);
        // the name comes from the `StoreName` target past any decorator `Call` ops emitted between
        let written = &chunk.instructions;
        let mut j = ip;
        while matches!(written.get(j).map(|i| i.opcode), Some(OpCode::Call)) { j += 1; }
        let name = written.get(j)
            .filter(|i| i.opcode == OpCode::StoreName)
            .and_then(|i| chunk.names.get(i.operand as usize))
            .map(|n| ssa_strip(n))
            // a generic class returns from its parameters' scope so its qualified name gives the name
            .unwrap_or_else(|| body.qualname.rsplit('.').next().unwrap_or("?"))
            .to_string();
        // the body runs as this loop's floor and `class_end` makes the class once it ends
        let run = crate::vm::frame::FloorRun::Class(crate::vm::frame::ClassRun { bases, member, name });
        // room for one floor at first as a block small enough for the allocator's pool
        if self.floors.capacity() == 0 { self.floors.reserve_exact(1); }
        self.floors.push(crate::vm::frame::Floored::new(ChunkPtr::of(body), class_slots, 1, run, dest));
        (self.floor_req, self.yielded) = (true, true);
        Ok(())
    }

    /* whether a class of `bases` is plain as `class_end` tells */
    #[inline(never)]
    fn plain_class(&self, bases: &[Val]) -> bool {
        bases.iter().all(|&b| self.heap.is_plain(b) || self.heap.is_type(b))
    }

    /* ends the class body atop the floors and makes the class from its slots */
    pub(crate) fn class_end(&mut self, ran: Result<Val, VmErr>, yielded: bool) -> Result<Val, VmErr> {
        let Some(f) = self.floors.pop() else { return ran };
        let crate::vm::frame::FloorRun::Class(crate::vm::frame::ClassRun { bases, member, name: name_str }) = f.run;
        self.class_cells.pop();
        if yielded {
            self.resume_ip = 0;
            self.pending.clear_waits();
            return Err(VmErr::Runtime("a class body cannot suspend, await outside it"));
        }
        ran?;
        let body: &SSAChunk = f.chunk.get();
        let class_slots = f.slots;
        let mut methods: Vec<(alloc::boxed::Box<str>, Val)> = Vec::new();
        for (i, name) in body.names.iter().enumerate() {
            if !member[i] { continue; }
            if let Some(&v) = class_slots.get(i)
                && !v.is_undef() {
                    let base = ssa_strip(name);
                    if let Some(pos) = methods.iter().position(|(n, _)| &**n == base) {
                        methods[pos].1 = v;
                    } else {
                        methods.push((base.into(), v));
                    }
                }
        }
        let qualname = (body.qualname != name_str).then(|| body.qualname.clone());
        self.finish_class(name_str, qualname, bases, methods)
    }

    /* refuses a base that is no class and keeps an object base where listed */
    pub(crate) fn check_bases(&self, bases: &mut [Val]) -> Result<(), VmErr> {
        // a builtin exception joins as a base so the class enters its tree
        for &b in bases.iter() {
            let ok = match self.heap.type_object_name(b) {
                _ if self.heap.is_class(b) => true,
                Some(n) if n == self.natives.names().kind(crate::lang::KindId::Instance) || self.natives.names().is_exception(n) => true,
                Some(n) => return Err(EngineError::Call(CallFault::Base(Some(n.into()))).into()),
                None => false,
            };
            if !ok { return Err(EngineError::Call(CallFault::Base(None)).into()); }
        }
        Ok(())
    }

    /* allocates the class from its checked bases and members and caches its order */
    #[inline(never)]
    pub(crate) fn finish_class(&mut self, name: String, qualname: Option<String>, bases: Vec<Val>, members: Vec<(alloc::boxed::Box<str>, Val)>) -> Result<Val, VmErr> {
        // a class of plain methods over such bases gives the ops nothing to run
        let plain = !self.heap.users && self.plain_class(&bases);
        let cls = self.heap.new_class(name, bases, members)?;
        if plain { self.heap.set_plain(cls); } else { self.heap.users = true; }
        // sites keyed on a swept class's slot must not match its successor or qualified name
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
        match qualname { Some(q) => { self.class_qualnames.insert(cls.0, q); } None => { self.class_qualnames.remove(&cls.0); } }
        let order = self.order_of(cls, self.heap.bases(cls));
        self.mro_cache.insert(cls.0, alloc::rc::Rc::new(order));
        Ok(cls)
    }

    /* materialises a mapping into string key and value pairs for a `**kwargs` spread */
    pub(crate) fn mapping_to_kw_pairs(&self, v: Val) -> Result<Vec<(Val, Val)>, VmErr> {
        let not_mapping = || -> VmErr { EngineError::cannot(Act::Map { spread: true }, Some(v)) };
        if !v.is_heap() { return Err(not_mapping()); }
        match self.heap.get(v) {
            HeapObj::Dict(rc) => {
                let entries: Vec<(Val, Val)> = rc.borrow().iter().collect();
                for (k, _) in &entries {
                    if !k.is_heap() || !matches!(self.heap.get(*k), HeapObj::Str(_)) {
                        return Err(EngineError::cannot(Act::Keyword, None));
                    }
                }
                Ok(entries)
            }
            _ => Err(not_mapping()),
        }
    }

    /* a dict of the pairs laid flat in `flat` as a display builds it */
    pub(crate) fn dictmap_of_flat(&mut self, flat: &[Val]) -> Result<DictMap, VmErr> {
        let mut dm = DictMap::with_capacity(flat.len() / 2);
        for p in flat.as_chunks::<2>().0 {
            if p[0].is_heap() { self.require_key(p[0])?; }
            dm.insert(p[0], p[1], &self.heap);
        }
        Ok(dm)
    }

    /* `{*src}` into set `acc` */
    pub(crate) fn spread_into(&mut self, acc: Val, src: Val) -> Result<(), VmErr> {
        let items: Vec<Val> = match self.heap.try_get(src) {
            Some(HeapObj::Set(rc)) => {
                // a long set pays for its items as iteration copies them
                let n = rc.borrow().len();
                if n > 64 { self.charge_steps(n)?; }
                match self.heap.try_get(src) { Some(HeapObj::Set(rc)) => rc.borrow().iter().copied().collect(), _ => Vec::new() }
            }
            _ => self.extract_iter(src)?,
        };
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        // the set grows within the memory left and colliding keys stop at the budget
        if let Some(HeapObj::Set(rc)) = self.heap.try_get(acc) { rc.borrow_mut().put_all(&items, &self.heap)?; }
        Ok(())
    }

    /* `{**src}` merged into dict `acc` with later keys overwriting earlier values */
    pub(crate) fn dict_spread_into(&mut self, acc: Val, src: Val) -> Result<(), VmErr> {
        let n = match self.heap.try_get(src) {
            Some(HeapObj::Dict(rc)) => rc.borrow().len(),
            _ => return Err(EngineError::cannot(Act::Map { spread: false }, Some(src))),
        };
        // a long map pays for its copied entries and refuses them past the memory left
        if n > 64 { self.charge_steps(n)?; self.heap.reserve(n.saturating_mul(6 * crate::value::VAL_BYTES))?; }
        let entries: Vec<(Val, Val)> = match self.heap.try_get(src) { Some(HeapObj::Dict(rc)) => rc.borrow().iter().collect(), _ => Vec::new() };
        if let Some(HeapObj::Dict(rc)) = self.heap.try_get(acc) { let mut d = rc.borrow_mut(); for (k, v) in entries { d.insert(k, v, &self.heap); self.heap.within_budget()?; } }
        Ok(())
    }
}

impl<'a> VM<'a> {
    /* the receiver native and owner kind a native or a bound native value calls with */
    #[inline(always)]
    pub(crate) fn native_parts(&self, v: Val) -> Option<(Val, &'static crate::lang::Native, Option<crate::lang::KindId>)> {
        let (recv, id) = match *self.heap.try_get(v)? {
            HeapObj::Native(id) => (Val::undef(), id),
            HeapObj::Bound { recv, func } => (recv, func.as_native_ref()?),
            _ => return None,
        };
        let n = self.natives.native(id)?;
        Some((recv, n, n.owner))
    }
}

/* whether a function or class callee opens as a frame of the loop */
#[inline(always)]
fn opens_in_loop(o: &HeapObj) -> bool { matches!(o, HeapObj::Func { .. }) || matches!(o, HeapObj::Object { meta, .. } if meta.is_class_meta()) }
