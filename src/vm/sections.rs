use alloc::rc::Rc;
use alloc::vec::Vec;
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use super::{VM, lower};
use crate::value::{Val, VmErr, HeapObj, HeapPool, Host, cold_depth};
use super::lower::Code;
use crate::code::Ins;
use super::frame::chunk_in;
use super::run::sections_hold;
use crate::r#unsafe::{CodePtr, get_in, known, put, reg};

/* `body_fits` then writing the cells of `f` and its defaults past the `n` passed arguments */
#[inline(never)]
fn body_enter(heap: &crate::value::HeapPool, r: &mut [Val], f: Val, s: &lower::Section, n: usize) -> bool {
    let (Some((arity, cells)), Some(HeapObj::Func { fi, defaults, cells: captures })) = (&s.by_body, heap.try_get(f)) else { return false };
    if *fi != s.fi || captures.len() != cells.len() || n + defaults.len() < *arity as usize { return false; }
    for (&at, &(_, cell)) in cells.iter().zip(captures.iter()) { r[at as usize] = cell; }
    let (arity, first) = (*arity as usize, s.link as usize + 1);
    for p in n..arity { r[first + p] = defaults[p + defaults.len() - arity]; }
    true
}

/* whether `n` arguments fit `f` as a function of the body `s` was laid from */
fn body_fits(heap: &crate::value::HeapPool, f: Val, s: &lower::Section, n: usize) -> bool {
    let (Some((arity, cells)), Some(HeapObj::Func { fi, defaults, cells: captures })) = (&s.by_body, heap.try_get(f)) else { return false };
    *fi == s.fi && captures.len() == cells.len() && n + defaults.len() >= *arity as usize
}

/* the section of `code` that holds index `at` */
pub(crate) fn section_at(code: &Code, at: usize) -> Option<&lower::Section> { code.block.as_deref()?.holding(at).map(|h| h.0) }

// a head asking growth gives this so the loop leaves at it for its handler
pub(crate) const GROW: usize = usize::MAX;

// the most head and function pairs one code asks a growth for
const TRIED: usize = 256;

// the preempt count `note_alt` leaves with preemption off to reach the handler once without preempting
pub(crate) const RELAY: usize = usize::MAX;

/* notes `func` the stub call at `i` met beyond its head's so relowering lays it */
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn note_alt(heap: &HeapPool, learned: &mut alloc::vec::Vec<Val>, bodies: &[super::Body], functions: &[&(Vec<alloc::string::String>, SSAChunk, u16, u16)], preempt_left: &mut usize, code: &Code, chunk: &SSAChunk, i: usize, func: Val, known: bool) {
    // only functions are noted while a class's stub call just counts toward a relowering
    if heap.is_class(func) && is_stub(code, i) && code.misses.get() < lower::MISSES { miss(preempt_left, code); }
    if !matches!(heap.try_get(func), Some(HeapObj::Func { .. })) { return; }
    note_met(heap, learned, bodies, functions, preempt_left, code, chunk, i, func, known);
}

/* counts a call the code's lowering did not lay toward a relowering as `note_met` does */
#[cold]
#[inline(never)]
pub(crate) fn miss(preempt_left: &mut usize, code: &Code) {
    code.misses.set(code.misses.get() + 1);
    if code.misses.get() == lower::MISSES && *preempt_left == 0 { *preempt_left = RELAY; }
}

/* `note_alt` past where the call lies */
#[allow(clippy::too_many_arguments)]
fn note_met(heap: &HeapPool, learned: &mut alloc::vec::Vec<Val>, bodies: &[super::Body], functions: &[&(Vec<alloc::string::String>, SSAChunk, u16, u16)], preempt_left: &mut usize, code: &Code, chunk: &SSAChunk, i: usize, func: Val, known: bool) {
    // comprehensions are remade each pass and recursions are laid as reached so neither is noted
    match heap.try_get(func) { Some(&HeapObj::Func { fi, .. }) if !functions.get(fi).is_some_and(|f| super::scope::is_comp(&f.0) || core::ptr::eq(&f.1, chunk)) => {}, _ => return }
    if code.misses.get() >= lower::MISSES { return; }
    // a stub's call is its head's which notes it as it asks to grow
    if code.block.is_some() && code.ins.get(i + 1).is_some_and(|s| s.b == lower::STUB_STORE || s.op == OpCode::ArgsR) && asked_by_head(code, i) { return; }
    // so is one whose own head for the function the growth laid or refused
    if let Some(g) = code.block.as_deref().and_then(|b| b.grown.get()) && let Some(&HeapObj::Func { fi, .. }) = heap.try_get(func) && g.tried.borrow().binary_search(&(i as u32, fi as u64 | 1 << 63)).is_ok() { return; }
    // a section call lies in its source body and others where relowered code finds them
    let (key, at) = match code.block.as_deref().and_then(|b| b.holding(i)) {
        Some((s, i)) => (s.fi as i64, s.orig[i - s.start as usize]),
        None if known => return miss(preempt_left, code),
        None => (VM::code_key_of(chunk, super::fi_in(bodies, chunk)), code.orig(i)),
    };
    // relowering gains nothing from one the lowering read or past the sixteen a call notes
    let seen = learned.chunks(3).enumerate().filter(|(_, t)| t[0].as_int() == key && t[1].as_int() == at as i64);
    if let Some((k, _)) = seen.clone().find(|(_, t)| t[2].0 == func.0) {
        if k >= code.read.get() as usize { miss(preempt_left, code); }
        return;
    }
    if seen.count() >= 16 { return; }
    miss(preempt_left, code);
    learned.extend_from_slice(&[Val::int(key), Val::int(at as i64), func]);
}

/* the section a head's table at `start` lays for function `f` or for its body */
#[inline(always)]
fn table_find(heap: &HeapPool, block: &lower::Block, start: usize, mask: usize, f: Val) -> Option<usize> {
    let find = |key: Val| {
        let mut slot = lower::alt_slot(key, mask);
        loop {
            // a table holds `mask + 1` slots from `start` with one free at least
            let (v, k) = *get_in(&block.alt_tab, start + slot);
            if v.0 == key.0 { return Some((k & !lower::DEFAULTS) as usize); }
            if k == u32::MAX { return None; }
            slot = (slot + 1) & mask;
        }
    };
    find(f).or_else(|| find(lower::with_defaults(f))).or_else(|| match heap.try_get(f) { Some(&HeapObj::Func { fi, .. }) => find(lower::body_key(fi)), _ => None })
}

/* the section a method head's table at `start` lays for `func` and `MAX` for none */
fn alt_of(block: &lower::Block, start: u32, mask: u32, func: Val) -> usize {
    if mask == u32::MAX { return u32::MAX as usize; }
    match alt_key(block, start, mask, func) { u32::MAX => alt_key(block, start, mask, lower::with_defaults(func)) as usize, k => k as usize }
}

/* the section the table finds for key `func` and `MAX` for none */
fn alt_key(block: &lower::Block, start: u32, mask: u32, func: Val) -> u32 {
    let mut slot = lower::alt_slot(func, mask as usize);
    loop {
        let Some(&(v, k)) = block.alt_tab.get(start as usize + slot) else { return u32::MAX };
        if k == u32::MAX { return k; }
        if v.0 == func.0 { return k & !lower::DEFAULTS; }
        slot = (slot + 1) & mask as usize;
    }
}

/* for `to` entered instead of `from` copies parameters past the second and defaults past `n` */
#[inline(never)]
pub(crate) fn moved(r: &mut [Val], base: usize, from: &lower::Section, to: &lower::Section, n: usize) {
    let (first, at) = (base + to.link as usize + 1, base + from.link as usize + 1);
    for p in 2..n.min(to.arity as usize) { r[first + p] = r[at + p]; }
    // parameters past those passed take the defaults of the function the section was laid from
    let (arity, nd) = (to.arity as usize, to.defaults.len());
    for p in n.max(arity - nd)..arity { r[first + p] = to.defaults[p + nd - arity]; }
}

/* whether the call at `i` lies in a stub whose head asks for growth */
#[inline(never)]
fn asked_by_head(code: &Code, i: usize) -> bool {
    matches!(code.ins[i].op, OpCode::CallR | OpCode::CallGlobalR | OpCode::CallMethodR) && is_stub(code, i)
}

/* the head before return point `ret` or the one a grown block noted for it */
#[inline(always)]
pub(crate) fn head_of(code: &Code, ret: usize) -> Option<usize> {
    if let Some(b) = code.block.as_deref() && !b.displaced().is_empty() { return displaced_head(b, ret); }
    ret.checked_sub(lower::HEADER as usize)
}

/* `head_of` for a block that laid heads past the end of code that grew */
#[inline(never)]
fn displaced_head(b: &lower::Block, ret: usize) -> Option<usize> {
    match b.displaced().binary_search_by_key(&(ret as u32), |d| d.0) { Ok(k) => Some(b.displaced()[k].1 as usize), Err(_) => ret.checked_sub(lower::HEADER as usize) }
}

/* the head whose stub makes the call at `i` of `code` */
pub(crate) fn stub_head(code: &Code, i: usize) -> Option<usize> {
    if !is_stub(code, i) { return None; }
    let j = code.ins[i + 1..].iter().take(10).find(|j| j.op == OpCode::Jump && j.x == 1)?;
    let h = (j.a as usize).checked_sub(lower::HEADER as usize)?;
    code.ins.get(h + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i).then_some(h)
}

pub(crate) fn is_stub(code: &Code, i: usize) -> bool {
    // a stub's call or row call stores through the take its `b` marks
    let rowed = code.ins.get(i).is_some_and(|c| c.op == OpCode::RowsR);
    let w = code.ins.get(i).is_some_and(|c| !rowed && lower::wide(*c)) as usize;
    if !code.ins.get(i + 1 + w).is_some_and(|s| (s.op == OpCode::StoreTopR || rowed && s.op == OpCode::RowsBackR) && s.b == lower::STUB_STORE) { return false; }
    // every stub lies past the code's own instructions among or after its sections
    if !code.block.as_deref().is_some_and(|b| b.sections.first().is_some_and(|s| i >= s.start as usize)) { return false; }
    let Some(j) = code.ins[i + 1..].iter().take(10).find(|j| j.op == OpCode::Jump && j.x == 1) else { return false };
    (j.a as usize).checked_sub(lower::HEADER as usize).and_then(|h| code.ins.get(h + 3)).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i)
}

impl<'a> VM<'a> {

    /* where a turned section call carries on in a body section or at its stub */
    #[inline(never)]
    pub(crate) fn section_turned(&mut self, code: &Code, rip: usize) -> usize {
        self.enter_section_of_body(code, rip).or_else(|| self.enter_spread(code, rip)).unwrap_or(code.ins[rip + 3].a as usize)
    }

    /* enters the section laid for another function a register call's head met or gives None */
    #[inline(never)]
    pub(crate) fn enter_value(&mut self, code: &Code, ins: Ins) -> Option<usize> {
        // a lookup lies in the stub of a head in code that laid its block
        let (block, at) = (known(code.block.as_deref()), ins.a as usize);
        let (call, store) = (*get_in(&code.ins, at), *get_in(&code.ins, at + 1));
        if self.budget == 0 { return None; }
        let base = self.base;
        let callee = reg(&self.regs, base + call.a as usize);
        let (start, mask) = (ins.b as usize, ins.c as usize);
        let k = table_find(&self.heap, block, start, mask, callee)?;
        let s = get_in(&block.sections, k);
        let p = base + s.link as usize;
        // a section any function of its body runs takes the callee's cells and missing defaults
        if s.by_body.is_some() && !body_fits(&self.heap, callee, s, call.x as usize) { return None; }
        let r = &mut self.regs;
        let (b, c) = (reg(r, base + call.b as usize), reg(r, base + call.c as usize));
        put(r, p + 1, b);
        put(r, p + 2, c);
        put(r, p, Val::int((store.b as i64) << 16 | store.a as i64));
        if call.x > 2 || (s.arity as usize) > call.x as usize { moved(r, base, &block.sections[code.ins[at + 2].c as usize], s, call.x as usize); }
        if s.by_body.is_some() { body_enter(&self.heap, &mut r[base..], callee, s, call.x as usize); }
        self.budget -= 1;
        Some(s.start as usize)
    }

    /* where a turned down register call goes as a met function's section or the stub */
    #[inline(never)]
    pub(crate) fn enter_value_alt(&mut self, code: &Code, at: usize) -> usize {
        // a head lies in code that laid its block with its section and stub inside
        let block = known(code.block.as_deref());
        let (call, sect, stub) = (*get_in(&code.ins, at), *get_in(&code.ins, at + 2), *get_in(&code.ins, at + 3));
        let (start, mask) = get_in(&block.sections, sect.c as usize).table;
        if mask == u32::MAX || self.budget == 0 { return stub.a as usize; }
        let base = self.base;
        let f = reg(&self.regs, base + call.a as usize);
        let mut slot = lower::alt_slot(f, mask as usize);
        let k = loop {
            // a table holds `mask + 1` slots from `start` with one free at least
            let (v, k) = *get_in(&block.alt_tab, start as usize + slot);
            if v.0 == f.0 { break k; }
            // a function the table lacks may still fit a body section which the stub finds
            if k == u32::MAX { return stub.a as usize; }
            slot = (slot + 1) & mask as usize;
        };
        let s = get_in(&block.sections, k as usize);
        // arguments past the second go from the head's own section's parameters to this one's
        if call.x > 2 { moved(&mut self.regs, base, get_in(&block.sections, sect.c as usize), s, call.x as usize); }
        let (p, store) = (base + s.link as usize, *get_in(&code.ins, at + 1));
        let r = &mut self.regs;
        let (b, c) = (reg(r, base + call.b as usize), reg(r, base + call.c as usize));
        put(r, p + 1, b);
        put(r, p + 2, c);
        put(r, p, Val::int((store.b as i64) << 16 | store.a as i64));
        self.budget -= 1;
        s.start as usize
    }

    /* the current value of the first `block` section binding that no longer holds its function */
    fn global_changed(&self, block: &lower::Block) -> Option<Val> {
        let s = block.sections.iter().find(|s| s.global != u16::MAX && self.scopes.get(s.bound).is_some_and(|g| g.read(s.global as u32).0 != s.value.get().0))?;
        Some(self.scopes[s.bound].read(s.global as u32))
    }

    /* whether call `i` laid as no head asks for a head laid for `callee` */
    #[cold]
    #[inline(never)]
    pub(crate) fn site_grows(&mut self, code: &Code, chunk: &SSAChunk, i: usize, callee: Val) -> bool {
        let Some(b) = code.block.as_deref() else { return false };
        // class calls ask only outside sections but stub heads are laid again with the class
        let inside = b.sections.first().is_some_and(|s| i >= s.start as usize);
        if inside && self.heap.is_class(callee) && !stub_head(code, i).is_some_and(|h| lower::kw_head(&code.ins, h) || code.ins[h + 2].x & 16 != 0) { return false; }
        // the first few functions turned down are only counted and kept for the first ask
        let g = b.grown();
        if g.turned.get() + 1 < lower::TURNS {
            g.turned.set(g.turned.get() + 1);
            if g.last.replace((i as u32, callee.0)) != (i as u32, callee.0) && g.pending.borrow().len() < 32 { g.pending.borrow_mut().push((i as u32, callee)); }
            return false;
        }
        if g.refuses(i as u32, callee.0) { return false; }
        if !g.pending.borrow().is_empty() { self.note_pending(code, chunk); }
        // a stub asks for its head with the met function or the binding's current value
        if inside {
            // a loop's stub calls its turned down generator which the loop steps as any iterator
            let looped = || b.loops.iter().find(|g| code.ins.get(g.3 as usize + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i)).map(|g| g.3 as usize);
            let Some(h) = stub_head(code, i).or_else(looped) else {
                // a headless section call asks for a head past the end unless it recurses
                let Some(s) = b.body_at(i) else { return false };
                if !matches!(self.heap.try_get(callee), Some(&HeapObj::Func { fi, .. }) if fi != s.fi && !self.functions.get(fi).is_some_and(|f| super::scope::is_comp(&f.0))) && !matches!(self.heap.try_get(callee), Some(HeapObj::Bound { func, .. }) if func.is_heap())
                    || self.redo.keep { return false; }
                self.refused = false;
                let asks = self.asks_growth(code, chunk, i, lower::Grow::Site, callee);
                if core::mem::take(&mut self.refused) { g.refuse(i as u32, callee.0); }
                return asks;
            };
            // a bound method's head meeting a function notes it where the head lies
            if code.ins[h].op == OpCode::CallMethodSectionR && b.sections.get(code.ins[h + 2].c as usize).is_some_and(|s| s.on_class & 3 == 3) {
                note_met(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, h, callee, false);
                return false;
            }
            let kind = match code.ins[h].op {
                // a constructor meets a class for the init it finds now or its binding's class
                OpCode::CallCtorSectionR | OpCode::CallSectionR if code.ins[h + 2].x & 16 != 0 && self.heap.is_class(callee) => lower::Grow::Global,
                OpCode::CallValueSectionR => lower::Grow::Value,
                OpCode::CallSectionR if code.ins[h + 2].x & 4 != 0 => lower::Grow::Value,
                OpCode::CallGlobalSectionR | OpCode::CallSectionR if !sections_hold(&self.scopes, b) => lower::Grow::Global,
                _ => return false,
            };
            if kind == lower::Grow::Value && matches!(self.heap.try_get(callee), Some(&HeapObj::Func { fi, .. }) if self.functions.get(fi).is_some_and(|f| core::ptr::eq(&f.1, chunk))) { return false; }
            let func = if kind == lower::Grow::Global { self.global_changed(b).unwrap_or(callee) } else { callee };
            self.refused = false;
            let asks = self.asks_growth(code, chunk, h, kind, func);
            // a binding may change unlike a head's function so its refusal counts toward a relowering
            if core::mem::take(&mut self.refused) {
                if kind != lower::Grow::Global { g.refuse(i as u32, callee.0); } else if code.misses.get() < lower::MISSES { miss(&mut self.preempt_left, code); }
            }
            return asks;
        }
        // remade comprehensions and tier-laid recursions ask nothing and a class's call lays its constructor
        match self.heap.try_get(callee) { Some(&HeapObj::Func { fi, .. }) if !self.functions.get(fi).is_some_and(|f| super::scope::is_comp(&f.0) || core::ptr::eq(&f.1, chunk)) => {}, Some(HeapObj::Bound { func, .. }) if func.is_heap() => {}, _ if self.heap.is_class(callee) => {}, _ => return false }
        self.refused = false;
        let asks = self.asks_growth(code, chunk, i, lower::Grow::Site, callee);
        if core::mem::take(&mut self.refused) { g.refuse(i as u32, callee.0); }
        asks
    }

    /* notes functions that calls of `code` met before its first ask under their heads */
    #[cold]
    #[inline(never)]
    fn note_pending(&mut self, code: &Code, chunk: &SSAChunk) {
        let Some(b) = code.block.as_deref() else { return };
        let pending = core::mem::take(&mut *b.grown().pending.borrow_mut());
        for (i, f) in pending {
            let at = stub_head(code, i as usize).unwrap_or(i as usize);
            let (scope, cat) = lower::head_origin(code, at);
            let key = scope.map_or_else(|| VM::code_key_of(chunk, self.fi_of(chunk)), |fi| fi as i64);
            let mut seen = self.learned.chunks(3).filter(|t| t[0].as_int() == key && t[1].as_int() == cat as i64);
            if matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })) && seen.clone().count() < 16 && !seen.any(|t| t[2].0 == f.0) { self.learned.extend_from_slice(&[Val::int(key), Val::int(cat as i64), f]); }
        }
    }

    /* whether head `at` turning down `func` asks growth once per pair after a few calls */
    #[cold]
    #[inline(never)]
    pub(crate) fn asks_growth(&mut self, code: &Code, chunk: &SSAChunk, at: usize, kind: lower::Grow, func: Val) -> bool {
        if cfg!(feature = "coverage") || self.growing.is_some() { return false; }
        let Some(block) = code.block.as_deref() else { return false };
        let found = self.heap.try_get(func);
        // a head asks once by body or bound function or a class's init
        let pair = (at as u32, match found {
            Some(&HeapObj::Func { fi, .. }) => fi as u64 | 1 << 63,
            Some(&HeapObj::Bound { func: f, .. }) if f.is_heap() => f.0 | 1 << 62,
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() && kind == lower::Grow::Global => self.lookup_class_member(func, self.natives.names().constructor).map_or(func.0, |(f, _)| f.0 | 1 << 61),
            _ => func.0,
        });
        // a pair already asked is turned down at once since it keeps coming back
        let grown = block.grown();
        let at_tried = grown.tried.borrow().binary_search(&pair);
        let Err(slot) = at_tried else { self.refused = true; return false };
        let class = matches!(kind, lower::Grow::Site | lower::Grow::Value) && self.heap.is_class(func)
            || kind == lower::Grow::Site && matches!(found, Some(HeapObj::Bound { func, .. }) if func.is_heap());
        // comprehensions and recursions in their own section stay refused as the tier lays them
        let own = matches!(found, Some(&HeapObj::Func { fi, .. }) if section_at(code, at).is_some_and(|s| s.fi == fi));
        if kind != lower::Grow::Global && !class && !matches!(found, Some(HeapObj::Func { fi, .. }) if !self.functions.get(*fi).is_some_and(|f| super::scope::is_comp(&f.0))) || own {
            if grown.tried.borrow().len() < TRIED { grown.tried.borrow_mut().insert(slot, pair); }
            self.refused = true;
            return false;
        }
        // past a few hundred pairs a code's head tables are as full as they get
        if grown.tried.borrow().len() >= TRIED { return false; }
        // noted met functions let growth lay all a head met as does relowering after snapshots
        if !matches!(kind, lower::Grow::Global | lower::Grow::Chain) && !class && grown.last.replace(pair) != pair {
            let (scope, cat) = lower::head_origin(code, at);
            let key = scope.map_or_else(|| VM::code_key_of(chunk, self.fi_of(chunk)), |fi| fi as i64);
            let mut seen = self.learned.chunks(3).filter(|t| t[0].as_int() == key && t[1].as_int() == cat as i64);
            if seen.clone().count() < 16 && !seen.any(|t| t[2].0 == func.0) { self.learned.extend_from_slice(&[Val::int(key), Val::int(cat as i64), func]); }
        }
        grown.turned.set(grown.turned.get() + 1);
        if grown.turned.get() < lower::TURNS { return false; }
        grown.tried.borrow_mut().insert(slot, pair);
        (self.growing, self.yielded) = (Some(alloc::boxed::Box::new((CodePtr::of_code(code), at, kind, func))), true);
        true
    }

    /* where a turned down method call goes as its receiver's method section or the stub */
    #[inline(never)]
    pub(crate) fn enter_method_alt(&mut self, code: &Code, chunk: &SSAChunk, at: usize) -> usize {
        // a head lies in code that laid its block with its section and table inside
        let block = known(code.block.as_deref());
        let (call, store, sect) = (*get_in(&code.ins, at), *get_in(&code.ins, at + 1), *get_in(&code.ins, at + 2));
        let recv = reg(&self.regs, self.base + call.b as usize);
        let (start, mask) = get_in(&block.sections, sect.c as usize).table;
        if mask != u32::MAX && !self.shadowed && self.budget != 0 && let Some(class) = self.heap.meta_of(recv) {
            let (c, epoch, k) = get_in(&block.by_class, start as usize + lower::alt_slot(class, mask as usize)).get();
            // a call that keeps values keeps them on entering as its head would
            if c.0 == class.0 && epoch == self.layout_epoch { return self.enter_alt(block, k as usize, sect.c as usize, call, store, recv); }
        }
        self.enter_method_found(code, chunk, at, recv)
    }

    /* `enter_method_alt` by name for a class the table lacks noting an unlaid method for relowering */
    #[inline(never)]
    fn enter_method_found(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> usize {
        let block = known(code.block.as_deref());
        let (call, store, sect, stub) = (code.ins[at], code.ins[at + 1], code.ins[at + 2], code.ins[at + 3]);
        let head = &block.sections[sect.c as usize];
        if head.on_class != 0 { return self.enter_on_class(code, chunk, at, recv).unwrap_or_else(|| self.enter_held(code, chunk, at, recv)); }
        let (start, mask) = head.table;
        let Some(func) = self.method_found(code, chunk, at, recv) else { return self.enter_held(code, chunk, at, recv) };
        let k = if func.0 == head.value.get().0 { sect.c as usize } else { alt_of(block, start, mask, func) };
        if k == u32::MAX as usize || self.budget == 0 {
            // the head's own method is noted first so relowering keeps it beside the one met
            if k == u32::MAX as usize {
                note_alt(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, stub.b as usize, head.value.get(), false);
                note_alt(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, stub.b as usize, func, false);
                if self.budget != 0 && self.asks_growth(code, chunk, at, lower::Grow::Method, func) { return GROW; }
            }
            return stub.a as usize;
        }
        // a function a receiver holds past a method head takes arguments where its parameters lie
        if block.sections[k].on_class == 4 { return self.enter_mixed(block, k, sect.c as usize, call, store, recv); }
        let class = self.heap.class_of(recv).unwrap_or(Val::undef());
        if !class.is_undef() && !self.shadowed { block.sections[k].guard.set((class, self.layout_epoch, recv)); }
        if mask != u32::MAX { block.by_class[start as usize + lower::alt_slot(class, mask as usize)].set((class, self.layout_epoch, k as u32)); }
        self.enter_alt(block, k, sect.c as usize, call, store, recv)
    }

    /* `enter_method_found` for a function the receiver holds or its class finds entering or noting it */
    #[cold]
    #[inline(never)]
    fn enter_held(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> usize {
        let block = known(code.block.as_deref());
        let (call, store, sect, stub) = (code.ins[at], code.ins[at + 1], code.ins[at + 2], code.ins[at + 3]);
        let head = &block.sections[sect.c as usize];
        if head.on_class == 1 && self.chain_grows(code, chunk, at, recv) { return GROW; }
        if !matches!(head.on_class, 0 | 4) { return stub.a as usize; }
        let held = self.held_func(code, chunk, at, recv);
        let found = if held.is_none() && head.on_class == 4 { self.method_found(code, chunk, at, recv) } else { None };
        let Some(func) = held.or(found) else { return stub.a as usize };
        let (start, mask) = head.table;
        let k = if func.0 == head.value.get().0 && held.is_some() { sect.c as usize } else { alt_of(block, start, mask, func) };
        // a held function enters only a section laid for one which takes the arguments alone
        let k = if held.is_some() && block.sections.get(k).is_some_and(|s| s.on_class != 4) { u32::MAX as usize } else { k };
        if k == u32::MAX as usize {
            if held.is_some() {
                // a held function is noted apart and its head asks growth with it
                self.note_called(code, chunk, stub.b as usize, func, lower::FIELD);
                if self.budget != 0 && self.asks_growth(code, chunk, at, lower::Grow::Method, func) { return GROW; }
            } else {
                note_alt(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, stub.b as usize, func, false);
            }
            return stub.a as usize;
        }
        if self.budget == 0 { return stub.a as usize; }
        self.enter_mixed(block, k, sect.c as usize, call, store, recv)
    }

    /* whether a head calling a function its class holds asks for one past the end */
    #[cold]
    #[inline(never)]
    fn chain_grows(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> bool {
        if self.budget == 0 || self.redo.keep || code.unchanged() { return false; }
        let name = chunk_in(&self.functions, code, chunk, at).names.get(code.ins[at].a as usize).map_or("", |n| crate::syntax::ssa::ssa_strip(n));
        let held = match self.heap.try_get(recv) {
            Some(HeapObj::Object { meta, slots: attrs }) if meta.is_heap() => { let a = attrs.borrow(); a.position_str(name, crate::value::hash_str(name), &self.heap).map(|i| a.value_at(i)) }
            _ => None,
        };
        let class = match self.heap.class_of(recv) { Some(c) => c, None if self.heap.is_class(recv) => recv, None => return false };
        let (v, mark) = match held { Some(f) => (f, lower::FIELD), None => match self.lookup_class_member(class, name) { Some((v, _)) => (v, lower::STATIC), None => return false } };
        if !matches!(self.heap.try_get(v), Some(HeapObj::Func { .. })) { return false; }
        // noted where growth finds it and marked with the kind of call it is
        self.note_called(code, chunk, at, v, mark);
        self.asks_growth(code, chunk, at, lower::Grow::Chain, v)
    }

    // the function instance or module `recv` holds under the name method head `at` calls
    fn held_func(&self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<Val> {
        let name = chunk_in(&self.functions, code, chunk, at).names.get(code.ins[at].a as usize).map_or("", |n| crate::syntax::ssa::ssa_strip(n));
        let f = match self.heap.try_get(recv) {
            Some(HeapObj::Object { meta, slots: attrs }) if meta.is_heap() => { let a = attrs.borrow(); a.position_str(name, crate::value::hash_str(name), &self.heap).map(|i| a.value_at(i)) }
            Some(HeapObj::Module(_, attrs)) => attrs.iter().find(|(n, _)| n == name).map(|&(_, v)| v),
            _ => None,
        }?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })).then_some(f)
    }

    /* `enter_alt` for a held function's section `k` moving the head's arguments with no receiver first */
    #[inline(never)]
    fn enter_mixed(&mut self, block: &lower::Block, k: usize, head: usize, call: Ins, store: Ins, recv: Val) -> usize {
        let (h, s, base) = (&block.sections[head], &block.sections[k], self.base);
        let (from, to, n) = (base + h.link as usize + 1, base + s.link as usize + 1, call.x as usize);
        let r = &mut self.regs;
        let args: [Val; 5] = core::array::from_fn(|j| if j >= n { Val::undef() } else if h.on_class == 4 { r[from + j] } else if j == 0 { r[base + call.c as usize] } else { r[from + 1 + j] });
        let first = if s.on_class == 4 { to } else { r[to] = recv; to + 1 };
        r[first..first + n].copy_from_slice(&args[..n]);
        r[to - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        self.budget -= 1;
        s.start as usize
    }

    /* enters section `k` for method head `call` on `recv` returning to the register `store` names */
    #[inline(always)]
    fn enter_alt(&mut self, block: &lower::Block, k: usize, head: usize, call: Ins, store: Ins, recv: Val) -> usize {
        // `k` and `head` are sections of `block` and the frame running it covers their registers
        let (s, base) = (get_in(&block.sections, k), self.base);
        let p = base + s.link as usize;
        let r = &mut self.regs;
        let c = reg(r, base + call.c as usize);
        put(r, p + 1, recv);
        put(r, p + 2, c);
        put(r, p, Val::int((store.b as i64) << 16 | store.a as i64));
        if k != head && (call.x > 1 || s.arity as usize > call.x as usize + 1) { moved(r, base, get_in(&block.sections, head), s, call.x as usize + 1); }
        self.budget -= 1;
        s.start as usize
    }

    /* notes the class a method call at `rip` met so a relowering lays its method */
    #[cold]
    #[inline(never)]
    pub(crate) fn note_class(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, class: Val) {
        if !self.heap.is_class(class) { return; }
        self.note_called(code, chunk, rip, class, 0);
    }

    /* counts an unlaid keyword call made before its binding held the callee toward a relowering */
    #[cold]
    #[inline(never)]
    pub(crate) fn kw_unlaid(&mut self, code: &Code, chunk: &SSAChunk, i: usize, op: u16) {
        if is_stub(code, i) { return; }
        let n = (op & 0xFF) as usize + 2 * (op >> 8) as usize;
        let Ok(at) = self.under(n + 1) else { return };
        let callee = self.slot(at);
        match self.heap.try_get(callee) {
            Some(HeapObj::Func { .. }) => note_alt(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, i, callee, false),
            _ if self.heap.is_class(callee) => miss(&mut self.preempt_left, code),
            _ => {}
        }
    }

    /* whether `code` may relower with its calls' notes as a function's or a turned loop's */
    pub(crate) fn lays_later(&self, code: &Code) -> bool { code.block.is_some() || code.hot.get() != 0 || self.pools[self.pool_now].func != super::cache::NO_FN }

    /* `note_class` for any value */
    pub(crate) fn note_called(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, class: Val, mark: u32) {
        // a stub's call of a bound method is noted inside the section that laid it
        let (key, at) = match code.block.as_deref().and_then(|b| b.holding(rip).or_else(|| if mark & lower::BOUND == 0 { None } else { b.holding(stub_head(code, rip)?) })) {
            Some((s, rip)) => (s.fi as i64, s.orig[rip - s.start as usize] | mark),
            None => (VM::code_key_of(chunk, self.fi_of(chunk)), code.orig(rip) | mark),
        };
        // a stub's call counts toward laying the code again with it
        if is_stub(code, rip) && code.misses.get() < lower::MISSES {
            code.misses.set(code.misses.get() + 1);
            if code.misses.get() == lower::MISSES && self.preempt_left == 0 { self.preempt_left = RELAY; }
        }
        let mut seen = self.learned.chunks(3).filter(|t| t[0].as_int() == key && t[1].as_int() == at as i64);
        if seen.clone().count() >= 16 || seen.any(|t| t[2].0 == class.0) { return; }
        self.learned.extend_from_slice(&[Val::int(key), Val::int(at as i64), class]);
    }

    /* enters the head section for a function its class still finds with the arguments alone */
    fn enter_on_class(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<usize> {
        let block = code.block.as_deref()?;
        let (call, store, sect) = (code.ins[at], code.ins[at + 1], code.ins[at + 2]);
        let s = &block.sections[sect.c as usize];
        if self.budget == 0 { return None; }
        // a function the instance holds by the call's name takes the arguments alone
        if s.on_class == 4 {
            let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| crate::syntax::ssa::ssa_strip(n));
            let held = match self.heap.try_get(recv) {
                Some(HeapObj::Object { meta, slots: attrs }) if meta.is_heap() => { let a = attrs.borrow(); a.position_str(name, crate::value::hash_str(name), &self.heap).map(|i| a.value_at(i)) }
                Some(HeapObj::Module(_, attrs)) => attrs.iter().find(|(n, _)| n == name).map(|&(_, v)| v),
                _ => return None,
            };
            if held.is_none_or(|f| f.0 != s.value.get().0) { return None; }
            self.budget -= 1;
            self.regs[self.base + s.link as usize] = Val::int((store.b as i64) << 16 | store.a as i64);
            return Some(s.start as usize);
        }
        // a bound method of the head's function enters it with its receiver first
        if s.on_class == 7 {
            let Some(&HeapObj::Bound { recv: own, func }) = self.heap.try_get(recv) else { return None };
            if func.0 != s.value.get().0 { return None; }
            return Some(self.enter_alt(block, sect.c as usize, sect.c as usize, call, store, own));
        }
        if !self.heap.is_class(recv) { return None; }
        // marked apart from an instance's class so the head never takes an instance for it
        let held = (recv, self.layout_epoch | 1 << 31, Val::undef());
        if s.guard.get() != held {
            let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| crate::syntax::ssa::ssa_strip(n));
            let (v, _) = self.lookup_class_member(recv, name)?;
            if s.on_class != 1 || !matches!(self.heap.try_get(v), Some(HeapObj::Func { .. })) || v.0 != s.value.get().0 { return None; }
            s.guard.set(held);
        }
        self.budget -= 1;
        self.regs[self.base + s.link as usize] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* the plain method `recv` finds by the name method head `at` calls or else None */
    #[inline(always)]
    fn method_found(&self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<Val> {
        let (call, stub) = (code.ins[at], code.ins[at + 3]);
        let (class, attrs) = self.heap.instance(recv)?;
        let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| n.as_str());
        if let Some(f) = super::sites::site_method_func(&self.heap, self.layout_epoch, self.shadowed, &code.cache.site(stub.b as usize), recv, name) { return Some(f); }
        if self.shadowed && attrs.borrow().position_str(name, crate::value::hash_str(name), &self.heap).is_some() { return None; }
        let (f, _) = self.lookup_class_member(class, name)?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func { .. })).then_some(f)
    }

    /* enters call `rip` in a body section paying budget and setting parameters or gives None */
    pub(crate) fn enter_section_of_body(&mut self, code: &Code, rip: usize) -> Option<usize> {
        let (ins, store, x) = (code.ins[rip], code.ins[rip + 1], code.ins[rip + 2]);
        let sections = &code.block.as_deref()?.sections;
        if x.x & 32 == 0 || self.budget == 0 { return None; }
        // the section itself or for a closure the twin laid after it
        let own = sections.get(x.c as usize)?;
        let s = if own.by_body.is_some() { own } else { sections.get(x.c as usize + 1).filter(|t| t.by_body.is_some() && t.fi == own.fi)? };
        let base = self.base;
        let (f, b, c, n) = (self.regs[base + ins.a as usize], self.regs[base + ins.b as usize], self.regs[base + ins.c as usize], ins.x as usize);
        if !body_enter(&self.heap, &mut self.regs[base..], f, s, n) { return None; }
        self.budget -= 1;
        // a function without defaults or cells enters alike each call so the head checks it
        if core::ptr::eq(s, own) && matches!(self.heap.try_get(f), Some(HeapObj::Func { defaults: d, cells: c, .. }) if d.is_empty() && c.is_empty()) { own.value.set(f); }
        let (at, from) = (base + s.link as usize + 1, base + own.link as usize + 1);
        if n > 0 { self.regs[at] = b; }
        if n > 1 { self.regs[at + 1] = c; }
        // arguments past the first two sit in the section's own parameters which the twin takes
        if !core::ptr::eq(s, own) { for p in 2..n { self.regs[at + p] = self.regs[from + p]; } }
        self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* enters the section of spreading call `rip` with items as parameters or a `*args` tuple */
    fn enter_spread(&mut self, code: &Code, rip: usize) -> Option<usize> {
        let (ins, store, x) = (code.ins[rip], code.ins[rip + 1], code.ins[rip + 2]);
        if x.x & 72 != 72 || self.budget == 0 { return None; }
        let sections = &code.block.as_deref()?.sections;
        let (mut s, base) = (sections.get(x.c as usize)?, self.base);
        let f = self.regs[base + ins.a as usize];
        // another function its head met runs the section its table names for it
        let found = if f.0 != s.value.get().0 && s.table.1 != u32::MAX { sections.get(alt_of(code.block.as_deref()?, s.table.0, s.table.1, f)) } else { None };
        if let Some(t) = found { s = t; }
        // another closure of the body runs the twin after the section filling cells from it
        let twin = f.0 != s.value.get().0;
        if twin {
            s = sections.get(x.c as usize + 1).filter(|t| x.x & 32 != 0 && t.by_body.is_some() && t.fi == s.fi)?;
            if !body_enter(&self.heap, &mut self.regs[base..], f, s, s.arity as usize) { return None; }
        }
        let seq = self.regs[base + ins.b as usize];
        // a function taking only `**kwargs` takes a copied mapping whose keys are strings like keywords
        if s.packs == 2 {
            if ins.x != 2 { return None; }
            let Some(HeapObj::Dict(d)) = self.heap.try_get(seq) else { return None };
            let d = d.borrow().clone();
            if !d.iter().all(|(k, _)| matches!(self.heap.try_get(k), Some(HeapObj::Str(_)))) { return None; }
            let kw = self.heap.alloc(HeapObj::Dict(Rc::new(core::cell::RefCell::new(d)))).ok()?;
            self.budget -= 1;
            let at = base + s.link as usize + 1;
            self.regs[at] = kw;
            self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
            return Some(s.start as usize);
        }
        if ins.x == 2 { return None; }
        let items: alloc::vec::Vec<Val> = match self.heap.try_get(seq) {
            Some(HeapObj::Tuple(v)) => v.clone(),
            Some(HeapObj::List(v)) => v.borrow().clone(),
            _ => return None,
        };
        let args = if s.packs == 1 {
            let tuple = if matches!(self.heap.try_get(seq), Some(HeapObj::Tuple(_))) { seq } else { self.heap.alloc(HeapObj::Tuple(items)).ok()? };
            alloc::vec![tuple]
        } else { items };
        if args.len() != s.arity as usize { return None; }
        self.budget -= 1;
        let at = base + s.link as usize + 1;
        self.regs[at..at + args.len()].copy_from_slice(&args);
        self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }
}

/* the class of an instance and None for builtin operands */
#[inline]
pub(crate) fn class_of(heap: &HeapPool, v: Val) -> Option<Val> { instance_class(heap, v) }

#[inline]
fn instance_class(heap: &HeapPool, v: Val) -> Option<Val> {
    heap.class_of(v)
}

impl<'a> VM<'a> {

    /* calls the plain function class member `name` is on instance `recv` and gives its value */
    pub(crate) fn try_call_dunder(&mut self, recv: Val, name: &str, args: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        // engine kinds take their native handlers and only user instances call a method
        let Some(cls_val) = self.instance_class(recv) else { return Ok(None); };
        // an op run again takes what the method gave before
        if let Some(r) = self.redo_dunder(recv, name) { return r.map(Some); }

        // special methods resolve on the type's order and skip the instance's own fields
        let Some((func, _)) = self.lookup_class_member(cls_val, name) else { return Ok(None); };
        // only plain functions bind as methods so data attributes never dispatch implicitly
        if !(func.is_heap() && matches!(self.heap.get(func), HeapObj::Func { .. })) { return Ok(None); }

        // the depth guard runs before pushing so a runaway recursion leaves no half built frame
        if self.depth >= self.max_calls { return Err(cold_depth()); }

        // a call an op's handler hands to the loop keeps which method it was
        let v = if self.redo.on { self.exec_hinted(func, recv, name, args, chunk)? } else { self.call_with(func, Some(recv), args, &[], chunk)? };
        self.got(v).map(Some)
    }

    /* the call of method `name` an op's handler hands the loop noting which it was */
    #[inline(never)]
    fn exec_hinted(&mut self, func: Val, recv: Val, name: &str, args: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        self.redo_hint(recv, name);
        let called = self.call_with(func, Some(recv), args, &[], chunk);
        self.redo.set_hint(None);
        called
    }

    /* what a swallowed end of iteration carries with the pending exception reset to `before` */
    #[inline(never)]
    pub(crate) fn swallow_stop(&mut self, before: Option<Val>) -> Val {
        let raised = core::mem::replace(&mut self.pending.exc_val, before).filter(|e| before.is_none_or(|b| b.0 != e.0));
        raised.and_then(|e| self.heap.error_parts(e)).and_then(|(_, args)| args.first().copied()).unwrap_or(Val::none())
    }
}
