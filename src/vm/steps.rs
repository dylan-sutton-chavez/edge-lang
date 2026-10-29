use alloc::{boxed::Box, vec::Vec};

use crate::lang::{self, Args, Cx, Flow, Inline, Item, KindId, Native, Out, Sent, Step, Then};
use super::VM;
use super::act::Dest;
use super::jobs::{self, Call, Gather, Job, Takes};
use crate::value::*;

/* a native's fold as its job keeps it with what an item already asked */
pub(crate) struct Folding {
    fold: lang::Fold,
    // the item a flow came from and that flow kept until its call comes back
    took: Option<(Val, Result<Flow, VmErr>)>,
    // an item the fold took whole whose later look finds nothing left to do
    more: Val,
    // what a fold over a container stopped with after a call
    done: Option<Val>,
    // a words fold that met another item hands every later one to its function
    spilled: bool,
}

impl Folding {
    fn new(fold: lang::Fold) -> Self { Self { fold, took: None, more: Val::undef(), done: None, spilled: false } }

    pub(crate) fn for_each_val(&self, f: &mut dyn FnMut(Val)) {
        self.fold.acc.trace(f);
        if let Some((v, r)) = &self.took {
            f(*v);
            match r {
                Ok(Flow::Call(g, x)) => { f(*g); f(*x); }
                Ok(Flow::Stop(x)) => f(*x),
                _ => {}
            }
        }
        if let Some(v) = self.done { f(v); }
        f(self.more);
    }

    /* whether the fold kept the flow item `v` made */
    pub(crate) fn kept(&self, v: Val) -> bool { self.more.0 == v.0 || self.took.as_ref().is_some_and(|(item, _)| item.0 == v.0) }

    /* the machine word total a words fold starts or goes on from */
    pub(crate) fn words(&self) -> i64 { self.fold.acc.n }

    /* the best an extreme fold starts or goes on from */
    pub(crate) fn best(&self) -> Val { self.fold.acc.v[0] }

    /* whether a keys fold has room for one more key read only where it grows */
    #[inline(always)]
    fn key_room(&self, heap: &HeapPool) -> bool {
        let keys = &self.fold.acc.items;
        keys.len() < keys.capacity() || (keys.len() + 1).saturating_mul(core::mem::size_of::<Val>()) < heap.room()
    }

    /* keeps key `v` in a keys fold and gives false past the memory left */
    #[inline(always)]
    fn keep_key(&mut self, v: Val, heap: &HeapPool) -> bool {
        if !self.key_room(heap) { return false; }
        self.fold.acc.items.push(v);
        true
    }

    /* whether key `k` of a keyed fold goes in with no call of its function */
    #[inline(always)]
    pub(crate) fn key_ready(&self, k: Val, heap: &HeapPool) -> bool {
        match self.fold.inline {
            Inline::Best(min) => plain_best(self.fold.acc.v[1], k, min).is_some(),
            _ => self.key_room(heap),
        }
    }

    /* takes key `k` that `key_ready` allowed and names `next` as the item keyed next */
    #[inline(always)]
    pub(crate) fn take_key(&mut self, k: Val, next: Val) {
        let acc = &mut self.fold.acc;
        match self.fold.inline {
            Inline::Best(min) => {
                if plain_best(acc.v[1], k, min) == Some(true) { (acc.v[0], acc.v[1]) = (acc.v[2], k); }
                acc.v[2] = next;
            }
            _ => acc.items.push(k),
        }
    }

    /* takes item `v` with no call where the inline kind keeps going and false otherwise */
    #[inline(always)]
    pub(crate) fn take_inline(&mut self, v: Val, heap: &HeapPool) -> bool {
        match self.fold.inline {
            Inline::Truth(any) => (if v.is_int() { heap.falsy.int_truth(v.as_int()) } else { heap.truthy(v) }) != any,
            Inline::Collect => {
                let items = &mut self.fold.acc.items;
                if (items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= heap.room() { return false; }
                items.push(v);
                true
            }
            Inline::Words if !self.spilled => {
                if v.is_int() && let Some(n) = self.fold.acc.n.checked_add(v.as_int()) { self.fold.acc.n = n; return true; }
                // any other item ends the words and goes to the take or the function
                self.spilled = true;
                self.fold.take.is_some_and(|take| take(&mut self.fold.acc, v))
            }
            Inline::Extreme(min) => match plain_best(self.fold.acc.v[0], v, min) {
                Some(wins) => { if wins { self.fold.acc.v[0] = v; } true }
                None => false,
            },
            _ => self.fold.take.is_some_and(|take| take(&mut self.fold.acc, v)),
        }
    }

    /* takes the run of `items` an inline kind keeps going on and gives how many */
    #[inline(always)]
    fn take_run(&mut self, items: &[Val], heap: &HeapPool, budget: &mut u64) -> usize {
        let items = &items[..items.len().min(usize::try_from(*budget).unwrap_or(usize::MAX))];
        let mut taken = 0;
        if let Inline::Collect = self.fold.inline {
            let have = &mut self.fold.acc.items;
            // a run that fits the memory left lands in one copy
            if (have.len() + items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) < heap.room() {
                have.extend_from_slice(items);
                *budget -= items.len() as u64;
                return items.len();
            }
            // the memory left read again only where the items grow their buffer
            let mut room = heap.room() / core::mem::size_of::<Val>();
            for &v in items {
                let have = &mut self.fold.acc.items;
                if have.len() == have.capacity() { room = heap.room() / core::mem::size_of::<Val>(); }
                if have.len() + 1 >= room { break; }
                have.push(v);
                taken += 1;
            }
        } else {
            for &v in items {
                if !self.take_inline(v, heap) { break; }
                taken += 1;
            }
        }
        *budget -= taken as u64;
        taken
    }

    /* what the fold makes of `item` with its inline kind first and its function after */
    pub(crate) fn step(&mut self, vm: &mut VM<'_>, item: Item) -> Result<Flow, VmErr> {
        if matches!(self.fold.inline, Inline::None) && self.fold.take.is_none() { return (self.fold.on)(&mut self.fold.acc, &mut Cx::new(vm), item); }
        match (self.fold.inline, item) {
            (Inline::Truth(any), Item::Next(v)) => return Ok(if vm.heap.truthy(v) == any { Flow::Stop(Val::bool(any)) } else { Flow::More }),
            (Inline::Truth(any), Item::End(_)) => return Ok(Flow::Stop(Val::bool(!any))),
            (Inline::Collect, Item::Next(v)) => return if self.take_inline(v, &vm.heap) { Ok(Flow::More) } else { Err(crate::vm::cold_heap()) },
            (Inline::Words, Item::Next(v)) if self.take_inline(v, &vm.heap) => return Ok(Flow::More),
            (Inline::Words, Item::Next(_)) => self.spilled = true,
            (Inline::Extreme(_), Item::Next(v)) if self.take_inline(v, &vm.heap) => return Ok(Flow::More),
            (Inline::Keys, Item::Next(v)) => return Ok(Flow::Call(self.fold.acc.v[0], v)),
            (Inline::Best(_), Item::Next(v)) => { self.fold.acc.v[2] = v; return Ok(Flow::Call(self.fold.acc.v[3], v)); }
            (Inline::Best(min), Item::Called(k)) if let Some(wins) = plain_best(self.fold.acc.v[1], k, min) => {
                let acc = &mut self.fold.acc;
                if wins { (acc.v[0], acc.v[1]) = (acc.v[2], k); }
                acc.v[2] = Val::undef();
                return Ok(Flow::More);
            }
            (Inline::Keys, Item::Called(k)) => return if self.keep_key(k, &vm.heap) { Ok(Flow::More) } else { Err(crate::vm::cold_heap()) },
            (Inline::Keys, Item::End(_)) => return match (self.fold.on)(&mut self.fold.acc, &mut Cx::new(vm), item)? {
                Flow::Stop(v) => Ok(Flow::Stop(v)),
                _ => Err(cold_runtime("a keys fold stops at its end")),
            },
            (Inline::None, Item::Next(v)) if self.take_inline(v, &vm.heap) => return Ok(Flow::More),
            (Inline::First, Item::Next(v)) => return Ok(Flow::Stop(v)),
            (Inline::First, Item::End(_)) if !self.fold.acc.v[0].is_undef() => return Ok(Flow::Stop(self.fold.acc.v[0])),
            _ => {}
        }
        (self.fold.on)(&mut self.fold.acc, &mut Cx::new(vm), item)
    }
}

/* whether `v` beats `best` as plain numbers and None for a pair the function compares */
#[inline(always)]
fn plain_best(best: Val, v: Val, min: bool) -> Option<bool> {
    if best.is_undef() { return Some(true); }
    if v.is_int() && best.is_int() { return Some(if min { v.as_int() < best.as_int() } else { v.as_int() > best.as_int() }); }
    if v.is_float() && best.is_float() && !v.as_float().is_nan() && !best.as_float().is_nan() {
        return Some(if min { v.as_float() < best.as_float() } else { v.as_float() > best.as_float() });
    }
    None
}

/* how a drain of one iterable ended */
enum Drained {
    Done(Val),
    // frames opened and their end gives the native's value
    Opened,
    // the fold walks this iterable next
    Next(Val, Box<Folding>),
}

/* what a fold made of its end */
enum Ended { Val(Val), Next(Val) }

/* what the plain run of a drain's cursor left for the item by item path */
enum Ready {
    // the cursor needs its full step or ran out
    Empty,
    // an item already charged and the flow the fold gave it
    Flow(Val, Flow),
}

// the positionals a call passes in place without a buffer on the heap
const FEW: usize = 4;

// the keyword only parameters a call binds in place without a buffer on the heap
const KW_HELD: usize = 4;

// the arguments and keyword pairs a stacked call moves off the stack in place
const HELD_ARGS: usize = 10;

/* a native's call run as loop frames through the calls job */
struct NativeCall { f: Val, args: [Val; 3], then: Then }

impl jobs::Calls for NativeCall {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<Call, VmErr> {
        let v = match got { None => return Ok(Call::Frame(self.f, self.args)), Some(r) => r? };
        let out = match core::mem::replace(&mut self.then, Then::Give) {
            Then::Give => Out::Val(v),
            Then::Fn(f) => f(&mut Cx::new(vm), v)?,
            Then::Cont(c) => c.resume(&mut Cx::new(vm), v)?,
        };
        let step = match out { Out::Val(r) => return Ok(Call::Give(r)), Out::Step(s) => *s, Out::Next(g) => Step::Resume { g, sent: Sent::Value(Val::none()) } };
        let Step::Call { f, args, n, then } = step else { return Err(cold_runtime("a call's continuation may only call again")) };
        self.f = vm.framed(f, n as usize)?;
        self.args = trimmed(args, n);
        self.then = then;
        Ok(Call::Frame(self.f, self.args))
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) {
        f(self.f);
        for &a in &self.args { f(a); }
        if let Then::Cont(c) = &self.then { c.trace(f); }
    }
}

/* the answer of `n`'s one or two argument path or None for its full call */
#[inline(always)]
fn fast_value(heap: &mut HeapPool, n: &Native, args: &[Val]) -> Option<Val> {
    match *args {
        [x] => (n.fast1?)(heap, x),
        [x, y] => (n.fast2?)(heap, x, y),
        _ => None,
    }
}

/* the error of a call naming a native the runtime's table does not hold */
#[cold]
pub(crate) fn no_native() -> VmErr { VmErr::Runtime("a call names a native this runtime does not give") }

/* the line plain output `out` builds in `line` for `pos` with the units it charges */
#[inline(always)]
pub(crate) fn plain_line(out: lang::FastOut, heap: &HeapPool, pos: &[Val], line: &mut alloc::string::String) -> Option<usize> {
    line.clear();
    out(heap, pos, line)
}

/* the first `n` arguments of a call with undef after them */
fn trimmed(mut args: [Val; 3], n: u8) -> [Val; 3] {
    for a in args.iter_mut().skip(n as usize) { *a = Val::undef(); }
    args
}

impl<'a> VM<'a> {
    /* runs native `n` into `to` giving false once frames took its call or it parked */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call_native(&mut self, n: &'static Native, recv: Val, owner: Option<KindId>, pos: &[Val], kw: &[Val], ok: bool, to: Dest) -> Result<bool, VmErr> {
        match self.native_val(n, recv, owner, pos, kw, ok)? {
            Some(v) => { self.put(v, to); Ok(true) }
            None => Ok(false),
        }
    }

    /* the value native `n` gives or none once frames took its call or it parked */
    #[inline(always)]
    pub(crate) fn native_val(&mut self, n: &'static Native, recv: Val, owner: Option<KindId>, pos: &[Val], kw: &[Val], ok: bool) -> Result<Option<Val>, VmErr> {
        if !kw.is_empty() || !n.sig.fits(pos.len()) { return self.native_val_bound(n, recv, owner, pos, kw, ok); }
        let out = (n.run)(&mut Cx::new(self), Args { recv, pos, kw: &[], named: &[] })?;
        self.out_val(out, ok)
    }

    /* pushes the value a native gave or runs the step it asked */
    #[inline(always)]
    pub(crate) fn out_pushed(&mut self, out: Out, ok: bool) -> Result<(), VmErr> { self.out_to(out, ok, Dest::Top).map(drop) }

    /* a native's value put in `to` or false once frames took it or it parked */
    #[inline(always)]
    pub(crate) fn out_to(&mut self, out: Out, ok: bool, to: Dest) -> Result<bool, VmErr> {
        match self.out_val(out, ok)? {
            Some(v) => { self.put(v, to); Ok(true) }
            None => Ok(false),
        }
    }

    /* the value a native's out gives at once or none once frames took it */
    #[inline(always)]
    pub(crate) fn out_val(&mut self, out: Out, ok: bool) -> Result<Option<Val>, VmErr> {
        match out {
            Out::Val(v) => self.pay_work().map(|()| Some(v)),
            Out::Step(s) => self.run_step(s, ok),
            Out::Next(g) => self.resume(g, Sent::Value(Val::none()), ok),
        }
    }

    /* writes `v` where `to` names */
    #[inline(always)]
    pub(crate) fn put(&mut self, v: Val, to: Dest) {
        match to {
            Dest::Reg(r) | Dest::Word(r) => self.regs[self.base + r as usize] = v,
            Dest::Retake => self.retake = v,
            Dest::Top => self.push(v),
            Dest::Void => {}
        }
    }

    /* `native_val` for keywords or a count off the signature laid out as `n` declares */
    #[cold]
    #[inline(never)]
    fn native_val_bound(&mut self, n: &'static Native, recv: Val, owner: Option<KindId>, pos: &[Val], kw: &[Val], ok: bool) -> Result<Option<Val>, VmErr> {
        if n.sig.names.is_empty() && n.sig.kw.len() <= KW_HELD && n.sig.fits(pos.len()) && let Some(held) = self.held_kw(n, owner, kw)? {
            let out = (n.run)(&mut Cx::new(self), Args { recv, pos, kw: &held[..n.sig.kw.len()], named: &[] })?;
            return self.out_val(out, ok);
        }
        let heap = &self.heap;
        let bound = lang::sig::bind(&n.sig, pos, kw, |k| match heap.try_get(k) { Some(HeapObj::Str(s)) => Some(s.as_str()), _ => None })
            .map_err(|m| self.misfit(n, owner, m))?;
        let out = (n.run)(&mut Cx::new(self), Args { recv, pos: &bound.pos, kw: &bound.kw, named: &bound.named })?;
        self.out_val(out, ok)
    }

    /* a call's keyword only values in place and None for a keyword the extras take */
    #[inline(always)]
    fn held_kw(&self, n: &'static Native, owner: Option<KindId>, kw: &[Val]) -> Result<Option<[Val; KW_HELD]>, VmErr> {
        let mut held = [Val::undef(); KW_HELD];
        for pair in kw.as_chunks::<2>().0 {
            let name = match self.heap.try_get(pair[0]) { Some(HeapObj::Str(s)) => s.as_str(), _ => "" };
            match n.sig.kw.iter().position(|&k| k == name) {
                Some(j) if held[j].is_undef() => held[j] = pair[1],
                Some(_) => return Err(self.misfit(n, owner, lang::Misfit::Twice(name))),
                None if n.sig.extra => return Ok(None),
                None => return Err(self.misfit(n, owner, lang::Misfit::Unexpected(name))),
            }
        }
        Ok(Some(held))
    }

    /* the error of a call that does not fit `n` in the runtime's words */
    #[cold]
    pub(crate) fn misfit(&self, n: &Native, owner: Option<KindId>, m: lang::Misfit<'_>) -> VmErr {
        let owner = owner.map(|k| self.natives.names().kind(k));
        (self.rt.misfit)(n, owner, m)
    }

    /* method `name` of kind `kind` or of int for a bool where bools are ints */
    #[inline]
    pub(crate) fn kind_method(&self, kind: KindId, name: &str) -> Option<(&'static Native, KindId)> {
        if let Some(n) = self.natives.method(kind, name) { return Some((n, kind)); }
        if kind == KindId::Bool && self.natives.policy.bool_is_int { return self.natives.method(KindId::Int, name).map(|n| (n, KindId::Int)); }
        None
    }

    /* the native method `obj.name` finds with a foreign kind's own table before its engine kind's */
    #[inline]
    pub(crate) fn native_method(&self, obj: Val, name: &str) -> Option<(&'static Native, KindId)> {
        let kind = lang::kind_of(&self.heap, obj);
        if kind == KindId::Foreign && self.natives.has_foreign() && let Some(n) = self.own_method(obj, name) { return Some((n, KindId::Foreign)); }
        self.kind_method(kind, name)
    }

    /* method `name` from the own table of the foreign kind of `obj` */
    #[inline(never)]
    fn own_method(&self, obj: Val, name: &str) -> Option<&'static Native> {
        let k = self.heap.foreign_kind(obj)?;
        self.natives.foreign_method(k.name, name)
    }

    /* native `n` on its stacked `pos` values and `kw` pairs pushing its value unless `void` */
    #[inline(never)]
    pub(crate) fn native_stack(&mut self, n: &'static Native, pos: usize, kw: usize, void: bool) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        let len = self.sp();
        // one argument a heap op answers replaces the stack top with no call frame
        if pos == 1 && kw == 0 && len >= 1 && let Some(fast) = n.fast1 {
            let x = self.slot(len - 1);
            if let Some(v) = fast(&mut self.heap, x) {
                if void { self.cut_to(len - 1); } else { self.set_slot(len - 1, v); }
                return self.pay_work();
            }
        }
        // two arguments a heap op answers leave its value in their place
        if pos == 2 && kw == 0 && len >= 2 && let Some(fast) = n.fast2 {
            let (x, y) = (self.slot(len - 2), self.slot(len - 1));
            if let Some(v) = fast(&mut self.heap, x, y) {
                if void { self.cut_to(len - 2); } else { self.cut_to(len - 1); self.set_slot(len - 2, v); }
                return self.pay_work();
            }
        }
        if kw == 0 && pos <= FEW && pos <= len && n.sig.fits(pos) {
            let at = len - pos;
            let mut few = [Val::undef(); FEW];
            few[..pos].copy_from_slice(self.vals_at(at, pos));
            // a native whose plain output answers these positionals writes them where they lie
            if let Some(out) = n.fast_out && self.plain(&few[..pos]) && let Some(units) = plain_line(out, &self.heap, &few[..pos], &mut self.out_line) {
                self.cut_to(at);
                self.wrote(units)?;
                if !void { self.push(Val::none()); }
                return self.pay_work();
            }
            self.cut_to(at);
            let v = match (n.run)(&mut Cx::new(self), Args { recv: Val::undef(), pos: &few[..pos], kw: &[], named: &[] })? {
                Out::Val(v) => v,
                out => return self.out_pushed(out, ok),
            };
            if !void { self.push(v); }
            return self.pay_work();
        }
        self.native_bound(n, pos, kw, ok)?;
        // the value a statement's call gave leaves the stack where its arguments were
        if void { self.pop()?; }
        Ok(())
    }

    /* whether each positional is a scalar or a text a plain output may write */
    #[inline(always)]
    pub(crate) fn plain(&self, pos: &[Val]) -> bool {
        pos.iter().all(|&v| v.is_int() || matches!(self.heap.try_get(v), Some(HeapObj::Str(_))) || v.is_float() || v.is_bool() || v.is_none())
    }

    /* writes the line a plain output gives `pos` and false where the full call runs */
    #[inline(never)]
    pub(crate) fn out_plain(&mut self, out: lang::FastOut, pos: &[Val]) -> Result<bool, VmErr> {
        let Some(units) = plain_line(out, &self.heap, pos, &mut self.out_line) else { return Ok(false) };
        self.wrote(units).map(|()| true)
    }

    /* charges the line a plain output built and hands it to the output */
    #[inline]
    pub(crate) fn wrote(&mut self, units: usize) -> Result<(), VmErr> {
        self.charge_steps(units)?;
        let line = core::mem::take(&mut self.out_line);
        self.write_out(&line);
        self.out_line = line;
        Ok(())
    }

    /* the stack form of a statement's native call at `rip` whose value goes nowhere */
    #[inline(never)]
    pub(crate) fn native_op_void(&mut self, site: crate::syntax::chunk::NativeSite, rip: usize, module: usize, chunk: &crate::syntax::chunk::SSAChunk) -> Result<(), VmErr> {
        // a native that never steps and no binding redirects gives its value nowhere at once
        if !site.spread && (site.pinned || !self.native_rebound(site.id)) && let Some(n) = self.natives.get(lang::NativeId(site.id)) && !n.steps() {
            self.job_ok = false;
            return self.native_stack(n, site.pos as usize, site.kw as usize, true);
        }
        self.delivering(Dest::Void, |vm| vm.native_op(site, module, chunk))?;
        self.voided(rip);
        Ok(())
    }

    /* drops what a statement's call at `rip` gave or sends its frame's value nowhere */
    pub(crate) fn voided(&mut self, rip: usize) {
        if !self.entered {
            if !self.yielded { let _ = self.pop(); }
            return;
        }
        self.void_frame(rip);
    }

    /* sends the value of the frame a statement's call at `rip` opened nowhere */
    pub(crate) fn void_frame(&mut self, rip: usize) {
        if self.entered && self.top().is_some_and(|f| !f.job && !f.dunder && f.generator.is_undef()) { self.mark_dunder(rip, super::act::VOID); }
    }

    /* the native call of `site` on its stacked arguments in module `module` pushing its value */
    #[inline(never)]
    pub(crate) fn native_op(&mut self, site: crate::syntax::chunk::NativeSite, module: usize, chunk: &crate::syntax::chunk::SSAChunk) -> Result<(), VmErr> {
        let (pos, kw) = (site.pos as usize, site.kw as usize);
        let Some(n) = self.natives.get(lang::NativeId(site.id)) else { return Err(no_native()) };
        let rebound = (!site.pinned && self.native_rebound(site.id)).then(|| self.native_binding(n, module)).flatten();
        if site.spread || rebound.is_some() {
            // the operands laid out with each packed spread spread before the call
            let (_, args, keyed) = self.spread_call(pos, kw, false)?;
            let v = match rebound {
                // the binding runs as a plain call whose function is a frame of this loop
                Some(f) => {
                    (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
                    let r = self.call_args(f, &args, &keyed, chunk);
                    (self.pending_exec_safe, self.inline_call) = (false, false);
                    r?
                }
                None => { self.job_ok = n.steps(); let ok = core::mem::take(&mut self.job_ok); self.native_val(n, Val::undef(), None, &args, &keyed, ok)? }
            };
            if let Some(v) = v { self.push(v); }
            return Ok(());
        }
        self.job_ok = n.steps();
        self.native_stack(n, pos, kw, false)
    }

    /* the tuple of the items of the list `l` a starred display built */
    pub(crate) fn list_tuple(&mut self, l: Val) -> Result<Val, VmErr> {
        let items = match self.heap.try_get(l) { Some(HeapObj::List(v)) => v.borrow().clone(), _ => return Err(cold_runtime("a starred display built no list")) };
        self.heap.alloc(HeapObj::Tuple(items))
    }

    /* whether exception `exc` is of the class or tuple `ty` as the runtime's matcher answers */
    pub(crate) fn match_exc(&self, exc: Val, ty: Val) -> Result<bool, VmErr> {
        (self.rt.handles)(&self.heap, exc, ty)
    }

    /* what module `module` binds native `n`'s name to unless it is the native */
    #[cold]
    #[inline(never)]
    pub(crate) fn native_binding(&self, n: &'static Native, module: usize) -> Option<Val> {
        let v = self.scopes.get(module)?.get(n.name)?;
        let same = self.heap.native_of(v) == Some(n.id);
        (!same).then_some(v)
    }

    /* `native_stack` for keywords many arguments or a count off the signature */
    #[inline(never)]
    fn native_bound(&mut self, n: &'static Native, pos: usize, kw: usize, ok: bool) -> Result<(), VmErr> {
        let at = self.under(pos + 2 * kw)?;
        // a few arguments move off the stack with no buffer on the heap
        if pos + 2 * kw <= HELD_ARGS {
            let mut few = [Val::undef(); HELD_ARGS];
            few[..pos + 2 * kw].copy_from_slice(self.vals_at(at, pos + 2 * kw));
            self.cut_to(at);
            return self.call_native(n, Val::undef(), None, &few[..pos], &few[pos..pos + 2 * kw], ok, Dest::Top).map(drop);
        }
        let all = self.vals_at(at, pos + 2 * kw).to_vec();
        self.cut_to(at);
        self.call_native(n, Val::undef(), None, &all[..pos], &all[pos..], ok, Dest::Top).map(drop)
    }

    /* calls native `n` where an unbound method takes the first of `pos` as receiver */
    #[inline(never)]
    pub(crate) fn native_value_call(&mut self, recv: Val, n: &'static Native, owner: Option<KindId>, pos: &[Val], kw: &[Val]) -> Result<Option<Val>, VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        match owner {
            Some(k) if recv.is_undef() => {
                let Some((&first, rest)) = pos.split_first() else {
                    return Err(lang::EngineError::Call(lang::CallFault::NoReceiver { kind: k, name: n.name }).into());
                };
                let fits = lang::kind_of(&self.heap, first) == k || k == KindId::Int && lang::kind_of(&self.heap, first) == KindId::Bool;
                if !fits { return Err(lang::EngineError::Call(lang::CallFault::WrongReceiver { kind: k, name: n.name, got: first }).into()); }
                self.native_val(n, first, Some(k), rest, kw, ok)
            }
            _ => self.native_val(n, recv, owner, pos, kw, ok),
        }
    }

    /* the register loop's charged call of a native value on `args` or none once taken */
    #[inline(never)]
    pub(crate) fn native_value_direct(&mut self, recv: Val, n: &'static Native, owner: Option<KindId>, args: &[Val]) -> Result<Option<Val>, VmErr> {
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.charge_step()?;
        // a function a heap op answers on its one or two arguments runs no call
        if owner.is_none() && let Some(v) = fast_value(&mut self.heap, n, args) {
            return self.pay_work().map(|()| Some(v));
        }
        self.job_ok = true;
        self.native_value_call(recv, n, owner, args, &[])
    }

    /* runs a native's step to its value or None once frames opened or it parked */
    #[inline(never)]
    #[allow(clippy::boxed_local)]
    pub(crate) fn run_step(&mut self, step: Box<Step>, ok: bool) -> Result<Option<Val>, VmErr> {
        match *step {
            Step::Wait => {
                // a frame holding rows takes the event's placeholder now and lowered code as it parks
                self.pending.event_wait_request = true;
                if self.top != 0 { self.push(Val::none()); }
                self.yielded = true;
                Ok(None)
            }
            Step::Call { f, args, n, then } => {
                let f = if ok { self.framed(f, n as usize)? } else { f };
                self.calls_run(Box::new(NativeCall { f, args: trimmed(args, n), then }), ok)
            }
            Step::Drain { it, fold } => self.drain(it, fold, ok),
            Step::Resume { g, sent } => self.resume(g, sent, ok),
        }
    }

    /* `f` as the loop opens it with classes and stepping natives wrapped in a call */
    fn framed(&mut self, f: Val, n: usize) -> Result<Val, VmErr> {
        if !(self.heap.is_class(f) || self.steps_whole(f)) { return Ok(f); }
        Ok(self.through_loop(f, n)?.unwrap_or(f))
    }

    /* resumes generator `g` as its method for `sent` would so its frame takes the step */
    fn resume(&mut self, g: Val, sent: Sent, ok: bool) -> Result<Option<Val>, VmErr> {
        // a generator sent None runs to its next yield as the engine's next runs it
        if ok && let Sent::Value(v) = sent && v.is_none() && self.gather_job(g, Takes::Next(Val::undef()), Vec::new())? { return self.drain_taken(); }
        let chunk = self.chunk;
        self.job_ok = ok;
        let v = self.coro_method(g, sent, chunk)?;
        if self.yielded || self.entered { return Ok(None); }
        Ok(v)
    }

    /* folds `it` here for containers and as loop frames for generators or user steps */
    fn drain(&mut self, it: Val, fold: lang::Fold, ok: bool) -> Result<Option<Val>, VmErr> {
        // any all and next of a generator are the engine's own and keep no box
        let takes = match fold.inline { Inline::Truth(any) => Some(if any { Takes::Any } else { Takes::Every }), Inline::First => Some(Takes::Next(fold.acc.v[0])), _ => None };
        if ok && let Some(takes) = takes && self.steps_user(it) {
            let before = self.jobs.len();
            if self.gather_gen(it, takes, &[])? { return self.drain_taken(); }
            self.jobs.truncate(before);
            // a refused gather steps the iterable here with no second gather asked
            return match self.drain_here(it, Box::new(Folding::new(fold)), true, ok)? {
                Drained::Done(v) => Ok(Some(v)),
                Drained::Opened => Ok(None),
                Drained::Next(next, back) => self.drain_with(next, back, ok),
            };
        }
        self.drain_with(it, Box::new(Folding::new(fold)), ok)
    }

    /* `drain` of fold `f` going on to each iterable its end names until it stops */
    fn drain_with(&mut self, mut it: Val, mut f: Box<Folding>, ok: bool) -> Result<Option<Val>, VmErr> {
        loop {
            let framed = self.steps_user(it);
            if framed && ok {
                // the inline kinds take a generator's items as the engine's own folds do
                let takes = match f.fold.inline {
                    Inline::Truth(true) => Takes::Any,
                    Inline::Truth(false) => Takes::Every,
                    Inline::Collect => Takes::FoldItems,
                    Inline::Words => Takes::FoldWords,
                    Inline::Extreme(min) => Takes::FoldExtreme(min),
                    Inline::First => Takes::Next(f.fold.acc.v[0]),
                    Inline::None | Inline::Keys | Inline::Best(_) => Takes::Fold,
                };
                let before = self.jobs.len();
                let taken = if matches!(takes, Takes::Any | Takes::Every | Takes::Next(_)) { self.gather_gen(it, takes, &[]) } else {
                    // the job takes this very box and a gather that refuses hands it back
                    self.next_fold.set(Some(f));
                    let taken = self.gather_gen(it, takes, &[]);
                    match (taken, self.next_fold.take()) {
                        (Ok(true), _) => return self.drain_taken(),
                        (Ok(false), Some(back)) => { f = back; Ok(false) }
                        (Ok(false), None) => return Err(cold_runtime("a refused gather kept its fold")),
                        (Err(e), _) => return Err(e),
                    }
                };
                if taken? { return self.drain_taken(); }
                self.jobs.truncate(before);
            }
            match self.drain_here(it, f, framed, ok)? {
                Drained::Done(v) => return Ok(Some(v)),
                Drained::Opened => return Ok(None),
                Drained::Next(next, back) => (it, f) = (next, back),
            }
        }
    }

    /* what a drain the jobs took gives now or None once its frames opened */
    fn drain_taken(&mut self) -> Result<Option<Val>, VmErr> { Ok(None) }

    /* the frame of builtin iterator `o` lent as a cursor when its steps run nothing */
    fn lend_frame(&mut self, o: Val) -> Option<IterFrame> {
        let Some(HeapObj::Iter(cell, _)) = self.heap.try_get(o) else { return None };
        let mut f = cell.try_borrow_mut().ok()?;
        if !matches!(*f, IterFrame::DictWalk { .. } | IterFrame::Seq { .. } | IterFrame::List { .. } | IterFrame::ListRev { .. } | IterFrame::Range { .. } | IterFrame::BigRange { .. } | IterFrame::Watched { .. } | IterFrame::Octets { .. } | IterFrame::Chars { .. }) { return None; }
        // an empty range holds its place with nothing to allocate
        Some(core::mem::replace(&mut *f, IterFrame::Range { cur: 0, end: 0, step: 1 }))
    }

    /* hands builtin iterator `o` back the frame a fold stepped */
    fn give_frame(&mut self, o: Val, frame: IterFrame) {
        if let Some(HeapObj::Iter(cell, _)) = self.heap.try_get(o) { *cell.borrow_mut() = frame; }
    }

    /* a cursor over container `o` stepped lazily for a range or a list */
    #[inline(always)]
    fn iter_cursor(&mut self, o: Val) -> Result<IterFrame, VmErr> {
        Ok(match self.heap.try_get(o) {
            Some(&HeapObj::Range(cur, end, step)) => IterFrame::Range { cur, end, step },
            Some(&HeapObj::BigRange(cur, end, step)) => IterFrame::BigRange { cur, end, step },
            Some(HeapObj::List(rc)) => IterFrame::List { rc: rc.clone(), idx: 0 },
            Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => IterFrame::Octets { bytes: b.as_slice().into(), idx: 0 },
            Some(HeapObj::Str(s)) if s.len() > SHORT_TEXT => IterFrame::Chars { text: s.as_str().into(), at: 0 },
            _ => IterFrame::Seq { items: self.extract_iter(o)?.into(), idx: 0 },
        })
    }

    /* folds `it` by cursor or steps opening frames only for calls the fold asks */
    fn drain_here(&mut self, it: Val, f: Box<Folding>, framed: bool, ok: bool) -> Result<Drained, VmErr> {
        // a first item fold steps an iterator once and reads no items ahead
        let lazy = match self.heap.try_get(it) {
            Some(HeapObj::Iter(..)) => !framed || f.fold.inline == Inline::First,
            Some(HeapObj::Coroutine(_)) => f.fold.inline == Inline::First,
            _ => false,
        };
        // a builtin iterator whose steps run nothing lends its frame as the cursor
        if lazy && let Some(frame) = self.lend_frame(it) {
            let mut cur = Some(frame);
            let r = self.drain_cursor(it, f, &mut cur, ok, true);
            if let Some(c) = cur.take() { self.give_frame(it, c); }
            return r;
        }
        let mut cur = if lazy { None } else { Some(self.iter_cursor(it)?) };
        self.drain_cursor(it, f, &mut cur, ok, false)
    }

    /* `drain_here` stepping `cur` or else `it` where a `lent` cursor goes back before any call */
    fn drain_cursor(&mut self, it: Val, mut f: Box<Folding>, cur: &mut Option<IterFrame>, ok: bool, lent: bool) -> Result<Drained, VmErr> {
        // a function found to open no frame is called at once on every later item
        let mut sync = Val::undef();
        loop {
            let ready = match cur.as_mut() { Some(c) => self.drain_ready(c, &mut f)?, None => Ready::Empty };
            let (v, flow) = match ready {
                Ready::Flow(v, flow) => (v, flow),
                Ready::Empty => {
                    let next = match cur.as_mut() { Some(c) => c.next_item(&mut self.heap)?, None => self.iter_step(it)? };
                    let Some(v) = next else {
                        // a lent walk that ran out leaves its iterator spent and drops its frame
                        if lent && cur.take().is_some() { self.heap.spend_iter(it); }
                        return Ok(match self.fold_end(&mut f, Val::none())? { Ended::Val(v) => Drained::Done(v), Ended::Next(n) => Drained::Next(n, f) });
                    };
                    self.charge_step()?;
                    (v, f.step(self, Item::Next(v))?)
                }
            };
            // user code may step the iterator itself so its frame goes back first
            if lent && matches!(flow, Flow::Call(..)) && let Some(c) = cur.take() { self.give_frame(it, c); }
            let flow = match flow {
                Flow::Call(g, x) => match if ok && g.0 != sync.0 { self.fold_frame(g) } else { None } {
                    // a keys fold over a container keys each item in one frame run again
                    Some((fi, func)) if matches!(f.fold.inline, Inline::Keys | Inline::Best(_)) && cur.is_some() => {
                        let Some(c) = cur.take() else { return Err(cold_runtime("a keys fold lost its items")) };
                        let job = Job::Keyed(Box::new(jobs::Keyed { fold: Some(f), cur: c, again: Default::default(), to: self.job_to }));
                        self.key_job(fi, func, x, job);
                        return Ok(Drained::Opened);
                    }
                    Some((fi, func)) => {
                        f.took = Some((v, Ok(Flow::Call(g, x))));
                        let mut j = Gather::new(Takes::Fold, Vec::new());
                        (j.co, j.cur, j.fold, j.before, j.to) = (it, cur.take(), Some(f), self.pending.exc_val.unwrap_or(Val::undef()), self.job_to);
                        self.jobs.push(Job::Gather(Box::new(j)));
                        self.open_fold(fi, func, jobs::Fold::native(v), [x, Val::undef()], 1);
                        (self.entered, self.yielded) = (true, true);
                        return Ok(Drained::Opened);
                    }
                    None => {
                        sync = g;
                        let r = self.fold_sync(g, x)?;
                        f.step(self, Item::Called(r))?
                    }
                },
                flow => flow,
            };
            match flow {
                Flow::More => {}
                Flow::Stop(r) => return Ok(Drained::Done(r)),
                Flow::Call(..) => return Err(cold_runtime("a fold makes one call per item")),
                Flow::Drain(_) => return Err(cold_runtime("a fold drains on only at its end")),
            }
        }
    }

    /* what the keyed job makes of key `v` its next item's frame or the end */
    #[inline(never)]
    pub(crate) fn keyed_take(&mut self, v: Val) -> Result<jobs::Step, VmErr> {
        self.charge_step()?;
        let Some(Job::Keyed(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
        // the fold leaves the job while its function runs and its end ends the job
        let Some(mut f) = j.fold.take() else { return Err(cold_runtime("a fold without its native")) };
        if !matches!(f.step(self, Item::Called(v))?, Flow::More) { return Err(cold_runtime("a keyed fold stops only at its end")); }
        let Some(Job::Keyed(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a job frame without its job")) };
        let Some(x) = j.cur.next_item(&mut self.heap)? else {
            return match self.fold_end(&mut f, Val::none())? {
                Ended::Val(r) => Ok(jobs::Step::Give(r)),
                Ended::Next(_) => Err(cold_runtime("a keys fold stops at its end")),
            };
        };
        let flow = f.step(self, Item::Next(x))?;
        if let Some(Job::Keyed(j)) = self.jobs.last_mut() { j.fold = Some(f); }
        match flow {
            Flow::Call(..) => Ok(jobs::Step::Again(x)),
            _ => Err(cold_runtime("a keyed fold calls its key on each item")),
        }
    }

    /* folds what `c` gives with no frame while budget lasts and no native work waits */
    #[inline(always)]
    fn drain_ready(&mut self, c: &mut IterFrame, f: &mut Folding) -> Result<Ready, VmErr> {
        if let IterFrame::Range { cur, end, step } = c && *step > 0 && let Some(r) = self.range_inline(cur, *end, *step, f) { return Ok(r); }
        // a list or tuple runs an inline kind over its items with one borrow
        if !matches!(f.fold.inline, Inline::None | Inline::Keys | Inline::Best(_) | Inline::First) && !self.heap.has_work() {
            let (heap, budget) = (&self.heap, &mut self.budget);
            match c {
                IterFrame::List { rc, idx } => *idx += f.take_run(rc.borrow().get(*idx..).unwrap_or(&[]), heap, budget),
                IterFrame::Seq { items, idx } => *idx += f.take_run(items.get(*idx..).unwrap_or(&[]), heap, budget),
                _ => {}
            }
        }
        while self.budget != 0 && !self.heap.has_work() {
            // the items the inline kind takes run with the budget held in a local
            let (heap, mut left, mut stray) = (&self.heap, self.budget, None);
            while left != 0 && let Some(v) = c.next_ready(heap) {
                left -= 1;
                if !f.take_inline(v, heap) { stray = Some(v); break; }
            }
            self.budget = left;
            self.heap.set_budget(left);
            let Some(v) = stray else { return Ok(Ready::Empty) };
            let flow = f.step(self, Item::Next(v))?;
            if !matches!(flow, Flow::More) { return Ok(Ready::Flow(v, flow)); }
        }
        self.heap.set_budget(self.budget);
        Ok(Ready::Empty)
    }

    /* an inline fold over a rising range reading its ints with no value for each */
    #[inline(always)]
    fn range_inline(&mut self, cur: &mut i64, end: i64, step: i64, f: &mut Folding) -> Option<Ready> {
        if self.heap.has_work() || *cur >= end || matches!(f.fold.inline, Inline::None) || f.spilled { return None; }
        // counted so no step past the last item can overflow
        let count = (end.wrapping_sub(*cur) as u64 - 1) / step as u64 + 1;
        let last = cur.wrapping_add(((count - 1) * step as u64) as i64);
        let inline = *cur >= Val::INT_MIN && last <= Val::INT_MAX;
        let n = count.min(self.budget);
        let (mut x, mut taken, mut stop) = (*cur, 0u64, false);
        match f.fold.inline {
            Inline::Truth(any) => {
                // zero reads as the runtime's mask says as any other int does
                let falsy = self.heap.falsy;
                while taken < n {
                    taken += 1;
                    if falsy.int_truth(x) == any { stop = true; break; }
                    x = x.wrapping_add(step);
                }
            }
            Inline::Words => {
                let mut total = f.fold.acc.n;
                while taken < n {
                    let Some(t) = total.checked_add(x) else { break };
                    (total, taken, x) = (t, taken + 1, x.wrapping_add(step));
                }
                f.fold.acc.n = total;
            }
            // every item fits an inline int so the best stays a plain int
            Inline::Extreme(min) if inline && (f.fold.acc.v[0].is_undef() || f.fold.acc.v[0].is_int()) => {
                let mut best = if f.fold.acc.v[0].is_undef() { None } else { Some(f.fold.acc.v[0].as_int()) };
                while taken < n {
                    best = Some(match best { Some(b) if (x < b) != min || x == b => b, _ => x });
                    (taken, x) = (taken + 1, x.wrapping_add(step));
                }
                if let Some(b) = best { f.fold.acc.v[0] = Val::int(b); }
            }
            Inline::Collect if inline => {
                let items = &mut f.fold.acc.items;
                let room = self.heap.room() / core::mem::size_of::<Val>();
                let n = n.min(room.saturating_sub(items.len() + 1) as u64);
                items.reserve_exact(n as usize);
                while taken < n {
                    items.push(Val::int(x));
                    (taken, x) = (taken + 1, x.wrapping_add(step));
                }
            }
            _ => return None,
        }
        // a stop already read its item and a spent range sits at its end
        *cur = if taken == count { end } else if stop { x.wrapping_add(step) } else { x };
        self.budget -= taken;
        self.heap.set_budget(self.budget);
        Some(if stop { Ready::Flow(Val::none(), Flow::Stop(Val::bool(matches!(f.fold.inline, Inline::Truth(true))))) } else { Ready::Empty })
    }

    /* folds item `v` the job's generator yielded through its native fold out of the job */
    #[inline(never)]
    pub(crate) fn fold_fast(&mut self, v: Val) -> bool {
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return false };
        if !matches!(j.takes, Takes::Fold) { return false; }
        let Some(mut f) = j.fold.take() else { return false };
        let r = f.step(self, Item::Next(v));
        let more = matches!(r, Ok(Flow::More));
        // a stop a call or an error waits for the full path to take it
        if !more { f.took = Some((v, r)); }
        if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.fold = Some(f); }
        if more {
            self.budget = self.budget.saturating_sub(1);
            self.heap.set_budget(self.budget);
        }
        more
    }

    /* whether stepping `v` runs a generator or user code which only loop frames may run */
    pub(crate) fn steps_user(&self, v: Val) -> bool {
        match self.heap.try_get(v) {
            Some(HeapObj::Coroutine(_)) => true,
            Some(HeapObj::Iter(..)) => self.holds_user(v, 0, true),
            _ => false,
        }
    }

    /* what fold `f` makes of the end given what a generator returned */
    fn fold_end(&mut self, f: &mut Folding, ret: Val) -> Result<Ended, VmErr> {
        match f.step(self, Item::End(ret))? {
            Flow::Stop(v) => Ok(Ended::Val(v)),
            Flow::More => Ok(Ended::Val(Val::none())),
            Flow::Drain(next) => { (f.took, f.more, f.done) = (None, Val::undef(), None); Ok(Ended::Next(next)) }
            Flow::Call(..) => Err(cold_runtime("a fold called past its end")),
        }
    }

    /* the frame a fold's call of `f` opens or None to call it at once */
    pub(crate) fn fold_frame(&mut self, f: Val) -> Option<(usize, Val)> {
        if let Some(fi) = self.job_fn(f) { return Some((fi, f)); }
        let user = self.heap.is_user_call(f) || self.heap.is_class(f);
        if !user && !self.steps_whole(f) { return None; }
        let g = self.through_loop(f, 1).ok()??;
        self.job_fn(g).map(|fi| (fi, g))
    }

    /* calls `f` on `x` at once when `f` runs no user code */
    pub(crate) fn fold_sync(&mut self, f: Val, x: Val) -> Result<Val, VmErr> {
        let chunk = self.chunk;
        let v = self.call_with(f, None, &[x], &[], chunk)?;
        self.got(v)
    }

    /* runs the top job's fold on `item` taken out of the job while it runs */
    fn job_fold(&mut self, item: Item) -> Result<Flow, VmErr> {
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a fold without its job")) };
        let Some(mut f) = j.fold.take() else { return Err(cold_runtime("a fold without its native")) };
        let r = f.step(self, item);
        if let Some(Job::Gather(j)) = self.jobs.last_mut() { j.fold = Some(f); }
        r
    }

    /* the call the top fold asks for item `v` asked once however many paths look */
    fn fold_peek(&mut self, v: Val) -> Option<(Val, Val)> {
        if let Some(Job::Gather(j)) = self.jobs.last() && let Some(f) = j.fold.as_ref() {
            if f.more.0 == v.0 { return None; }
            if let Some((item, r)) = &f.took && item.0 == v.0 { return match *r { Ok(Flow::Call(g, x)) => Some((g, x)), _ => None }; }
        }
        let r = self.job_fold(Item::Next(v));
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return None };
        let f = j.fold.as_mut()?;
        match r {
            // an item taken whole leaves only its mark for the look that follows
            Ok(Flow::More) => { f.more = v; None }
            Ok(Flow::Call(g, x)) => { f.took = Some((v, r)); Some((g, x)) }
            r => { f.took = Some((v, r)); None }
        }
    }

    /* forgets the flow the fold on top kept for its item */
    fn fold_forget(&mut self) {
        if let Some(Job::Gather(j)) = self.jobs.last_mut() && let Some(f) = j.fold.as_mut() { (f.took, f.more) = (None, Val::undef()); }
    }

    /* the frame a fold's item `v` asks for or None when no frame runs it */
    pub(crate) fn fold_call(&mut self, v: Val, resume: usize) -> Option<(usize, Val, jobs::Fold, [Val; 2], usize)> {
        let (g, x) = self.fold_peek(v)?;
        let (fi, func) = self.fold_frame(g)?;
        Some((fi, func, jobs::Fold::at(v, resume), [x, Val::undef()], 1))
    }

    /* takes item `v` into the top fold giving Some with its value once it stops */
    pub(crate) fn fold_take_item(&mut self, v: Val) -> Result<Option<Val>, VmErr> {
        // a flow an earlier look kept for this item comes out and none stays kept
        let kept = match self.jobs.last_mut() {
            Some(Job::Gather(j)) => match j.fold.as_mut() {
                Some(f) if f.more.0 == v.0 => { f.more = Val::undef(); return Ok(None); }
                Some(f) => f.took.take(),
                None => None,
            },
            _ => None,
        };
        let flow = match kept { Some((item, r)) if item.0 == v.0 => r, _ => self.job_fold(Item::Next(v)) };
        let flow = match flow? {
            Flow::Call(g, x) => { let r = self.fold_sync(g, x)?; self.job_fold(Item::Called(r))? }
            flow => flow,
        };
        match flow {
            Flow::More => Ok(None),
            Flow::Stop(r) => Ok(Some(r)),
            Flow::Call(..) => Err(cold_runtime("a fold makes one call per item")),
            Flow::Drain(_) => Err(cold_runtime("a fold drains on only at its end")),
        }
    }

    /* takes `r` a fold's frame gave where only a `native` container fold may stop here */
    pub(crate) fn fold_called(&mut self, r: Val, native: bool) -> Result<(), VmErr> {
        self.fold_forget();
        match self.job_fold(Item::Called(r))? {
            Flow::More => Ok(()),
            Flow::Stop(v) if native => {
                if let Some(Job::Gather(j)) = self.jobs.last_mut() && let Some(f) = j.fold.as_mut() { f.done = Some(v); }
                Ok(())
            }
            _ => Err(cold_runtime("a fold over a generator stops only at an item")),
        }
    }

    /* what a container fold stopped with after a call and where it goes */
    pub(crate) fn fold_done(&mut self) -> Option<(Val, super::act::Dest)> {
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return None };
        let (v, to) = (j.fold.as_mut()?.done.take()?, j.to);
        self.jobs.pop();
        Some((v, to))
    }

    /* what the fold of gather `j` makes of its end once its items ran out */
    pub(crate) fn fold_ended(&mut self, j: &mut Gather, ret: Val) -> Result<Val, VmErr> {
        let Some(mut f) = j.fold.take() else { return Err(cold_runtime("a fold without its native")) };
        match j.takes {
            // what a chain gathered before this iterable stays ahead of its items
            Takes::FoldItems if f.fold.acc.items.is_empty() => f.fold.acc.items = core::mem::take(&mut j.items),
            Takes::FoldItems => f.fold.acc.items.append(&mut j.items),
            Takes::FoldWords => if let jobs::Total::Word(w) = j.total { f.fold.acc.n = w },
            Takes::FoldExtreme(_) => f.fold.acc.v[0] = j.acc,
            _ => {}
        }
        match self.fold_end(&mut f, ret)? {
            Ended::Val(v) => Ok(v),
            // the next iterable may open frames whose end gives what this end would have
            Ended::Next(next) => Ok(self.drain_with(next, f, true)?.unwrap_or(Val::undef())),
        }
    }

    /* takes item `v` past plain numbers through the extreme fold with the job's best */
    pub(crate) fn fold_best(&mut self, v: Val) -> Result<Option<Val>, VmErr> {
        let sync = |vm: &mut Self, into: bool| if let Some(Job::Gather(j)) = vm.jobs.last_mut() && let Some(f) = j.fold.as_deref_mut() {
            if into { f.fold.acc.v[0] = j.acc; } else { j.acc = f.fold.acc.v[0]; }
        };
        sync(self, true);
        let r = self.fold_take_item(v);
        sync(self, false);
        r
    }

    /* hands the words fold on top its total and every later item through its function */
    pub(crate) fn fold_spill(&mut self) {
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return };
        if let (jobs::Total::Word(w), Some(f)) = (j.total, j.fold.as_deref_mut()) { (f.fold.acc.n, f.spilled) = (w, true); }
        j.takes = Takes::Fold;
    }

    /* fold `f` takes key value `v` and gives the argument its key runs again on */
    pub(crate) fn fold_on(&mut self, mut f: Box<Folding>, v: Val, callee: Val) -> Option<Val> {
        let g = self.jobs.len() - 2;
        if f.took.is_some() { f.took = None; }
        f.more = Val::undef();
        let mut flow = f.step(self, Item::Called(v));
        // the item the flow came of and undef for the frame's value
        let mut item = Val::undef();
        loop {
            match flow {
                Ok(Flow::More) => {}
                Ok(Flow::Call(h, x)) if !item.is_undef() && h.0 == callee.0 => {
                    if let Job::Gather(j) = &mut self.jobs[g] { j.fold = Some(f); }
                    if let Some(Job::Fold(d)) = self.jobs.last_mut() { d.item = item; }
                    return Some(x);
                }
                _ => break,
            }
            // an item the fold took with no call pays its step
            if !item.is_undef() { (self.budget, item) = (self.budget - 1, Val::undef()); }
            if self.budget == 0 || self.heap.has_work() { break; }
            let Job::Gather(j) = &mut self.jobs[g] else { break };
            let Some(x) = j.cur.as_mut().and_then(|c| c.next_ready(&self.heap)) else { break };
            (item, flow) = (x, f.step(self, Item::Next(x)));
        }
        self.heap.set_budget(self.budget);
        f.took = Some((item, flow));
        if let Job::Gather(j) = &mut self.jobs[g] { j.fold = Some(f); }
        if let Some(Job::Fold(d)) = self.jobs.last_mut() { *d = jobs::Fold::at(Val::undef(), jobs::TOOK); }
        None
    }

    /* takes what the fold kept of its key's value giving an item it stepped */
    pub(crate) fn fold_kept(&mut self) -> Result<Option<Val>, VmErr> {
        let Some(Job::Gather(j)) = self.jobs.last_mut() else { return Err(cold_runtime("a fold without its job")) };
        let Some(f) = j.fold.as_mut() else { return Err(cold_runtime("a fold without its native")) };
        match f.took.take() {
            // the frame's value goes in as `fold_called` takes it
            Some((item, flow)) if item.is_undef() => match flow? {
                Flow::More => Ok(None),
                Flow::Stop(r) => { f.done = Some(r); Ok(None) }
                _ => Err(cold_runtime("a fold over a generator stops only at an item")),
            },
            Some((item, flow)) => { f.took = Some((item, flow)); Ok(Some(item)) }
            None => Ok(None),
        }
    }
}
