/* Exception groups, their methods and the steps `except*` compiles to. */

use super::VM;
use super::types::*;
use super::opcodes::attr_lookup::AttrLookup;
use crate::parser::SSAChunk;
use crate::s;
use alloc::{rc::Rc, vec::Vec};
use core::cell::RefCell;
use crate::vm::natives::Builtins;

/* What a split keeps, instances of a classinfo or those a predicate accepts. */
enum Cond { Types(Val), Pred(Val) }

/* The Python calls a group method makes, which a job runs it again after, holding what each gave or raised, the next to take and the first not yet made. */
struct Replay { log: Vec<Result<Val, (VmErr, Option<Val>)>>, pos: usize, ask: Option<(Val, Val, [Val; 3])>, nested: bool }

impl Replay {
    fn nested() -> Self { Replay { log: Vec::new(), pos: 0, ask: None, nested: true } }

    /* One whose calls the job makes. */
    fn asked() -> Self { Replay { nested: false, ..Replay::nested() } }

    /* Calls `f` on `args`, bound to `class` and its first argument unless `class` is undef, giving what it gave the run before or stopping the run for the job to make it. */
    fn call(&mut self, vm: &mut VM, f: Val, class: Val, args: [Val; 3]) -> Result<Val, VmErr> {
        if let Some(r) = self.log.get(self.pos) {
            self.pos += 1;
            return r.clone().map_err(|(e, exc)| { vm.pending.exc_val = exc; e });
        }
        // Past the calls a run takes again a few thousand times over, the rest are made at once.
        if !self.nested && self.log.len() < super::redo::CALLS { self.ask = Some((f, class, args)); return Err(VmErr::Redo); }
        if !class.is_undef() { vm.pending.method_binding = Some((class, args[0])); }
        let chunk = vm.chunk;
        vm.call_with(f, None, crate::vm::jobs::call_args(&args), &[], chunk)?;
        vm.pop()
    }
}

/* What a group's job runs again after each of its calls, a group method with its argument and `derive` members, an `except*` clause's split, or the statement's end. */
enum GroupRun { Method(Val, &'static str, Val, Vec<Val>), Split(Val, Val, Val), End(Val) }

/* A group's run as a job, its calls of Python frames of the loop, and what they gave. */
struct GroupCalls { run: GroupRun, replay: Replay }

impl super::jobs::Calls for GroupCalls {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<super::jobs::Call, VmErr> {
        if let Some(r) = got { let exc = vm.pending.exc_val; self.replay.log.push(r.map_err(|e| (e, exc))); }
        (self.replay.pos, self.replay.ask) = (0, None);
        let r = match &self.run {
            GroupRun::Method(g, name, arg, items) => vm.group_value(*g, name, *arg, items.clone(), &mut self.replay),
            &GroupRun::Split(state, left, t) => vm.split_group(left, &Cond::Types(t), &mut self.replay).and_then(|r| vm.star_took(state, r)),
            &GroupRun::End(state) => vm.star_ended(state, &mut self.replay),
        };
        match (r, self.replay.ask.take()) {
            (Err(VmErr::Redo), Some((f, class, args))) => Ok(super::jobs::Call::Frame(f, class, args)),
            (r, _) => r.map(super::jobs::Call::Give),
        }
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) {
        match &self.run {
            GroupRun::Method(g, _, arg, items) => { f(*g); f(*arg); for &v in items { f(v) } }
            GroupRun::Split(state, left, t) => { f(*state); f(*left); f(*t); }
            GroupRun::End(state) => f(*state),
        }
        for r in &self.replay.log { if let Ok(v) | Err((_, Some(v))) = *r { f(v) } }
    }
}

impl<'a> VM<'a> {
    /* Whether `v` is an exception group, builtin or of a class over one. */
    pub(crate) fn is_group(&self, v: Val) -> bool {
        self.exc_class_name(v).is_some_and(crate::vm::globals::is_group_type)
    }

    /* The builtin exception an instance belongs to, None for any other value. */
    /* Whether `e` is an error an `except base` clause catches, a raised instance judged by its class. */
    pub(crate) fn raised_as(&self, e: &VmErr, base: &str) -> bool {
        crate::vm::globals::matches_exc_class(&e.class_name(), base)
            || matches!(e, VmErr::Raised(_)) && self.pending.exc_val.and_then(|x| self.exc_class_name(x)).is_some_and(|n| crate::vm::globals::matches_exc_class(n, base))
    }

    pub(crate) fn exc_class_name(&self, v: Val) -> Option<&str> {
        match self.heap.try_get(v)? {
            HeapObj::ExcInstance(n, ..) => Some(n.as_str()),
            &HeapObj::Instance(c, _) => self.exc_base(c),
            _ => None,
        }
    }

    /* `g.message`, `g.exceptions` and the group methods, None for any other name or value. */
    pub(crate) fn group_attr(&self, g: Val, name: &str) -> Option<AttrLookup> {
        if !self.is_group(g) { return None; }
        match name {
            "message" => Some(AttrLookup::ClassMember(self.group_parts(g).0)),
            "exceptions" => Some(AttrLookup::ExcArgs(self.group_parts(g).1)),
            _ => crate::vm::methods::lookup_method("BaseExceptionGroup", name).map(AttrLookup::BuiltinMethod),
        }
    }

    /* The members of group `g` an uncaught traceback lists under its message, a tree with one box per member. */
    pub(crate) fn group_text(&self, g: Val) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        if self.is_group(g) { self.group_lines(g, "", &mut out, 0); }
        out
    }

    fn group_lines(&self, g: Val, pad: &str, out: &mut alloc::string::String, depth: usize) {
        const SHOWN: usize = 15;
        let items = self.group_parts(g).1;
        let inner = s!(str pad, "  ");
        for (i, &x) in items.iter().take(SHOWN).enumerate() {
            let rule = if i == 0 { "+-+" } else { "  +" };
            out.push_str(&s!("\n", str pad, str rule, "---------------- ", int i as i64 + 1, " ----------------"));
            let name = self.exc_type_name(x);
            let text = self.display(x);
            let line = if text.is_empty() { name } else { s!(str &name, ": ", str &text) };
            for l in s!(str &line, str &self.notes_text(x)).split('\n') { out.push_str(&s!("\n", str &inner, "| ", str l)); }
            if self.is_group(x) && depth < 10 { self.group_lines(x, &inner, out, depth + 1); }
        }
        if items.len() > SHOWN { out.push_str(&s!("\n", str &inner, "+---------------- ... ----------------\n", str &inner, "| and ", int (items.len() - SHOWN) as i64, " more exceptions")); }
        out.push_str(&s!("\n", str &inner, "+------------------------------------"));
    }

    /* The message and members of group `g`. */
    fn group_parts(&self, g: Val) -> (Val, Vec<Val>) {
        let args = match self.heap.try_get(g) { Some(HeapObj::ExcInstance(_, a, _)) => a.clone(), _ => self.exc_args(g) };
        let items = match args.get(1).and_then(|&e| self.heap.try_get(e)) {
            Some(HeapObj::List(l)) => l.borrow().clone(),
            Some(HeapObj::Tuple(t)) => t.clone(),
            _ => Vec::new(),
        };
        (args.first().copied().unwrap_or(Val::none()), items)
    }

    /* `ExceptionGroup(msg, excs)`, which `BaseExceptionGroup` becomes when every member is an `Exception`. */
    pub(crate) fn new_group(&mut self, name: &str, args: &[Val]) -> Result<Val, VmErr> {
        let [msg, excs] = *args else {
            return Err(VmErr::TypeMsg(s!("BaseExceptionGroup.__new__() takes exactly 2 arguments (", int args.len() as i64, " given)")));
        };
        if !matches!(self.heap.try_get(msg), Some(HeapObj::Str(_))) {
            return Err(VmErr::TypeMsg(s!("argument 1 must be str, not ", str self.type_name(msg))));
        }
        let items = match self.heap.try_get(excs) {
            Some(HeapObj::List(l)) => l.borrow().clone(),
            Some(HeapObj::Tuple(t)) => t.clone(),
            _ => return Err(cold_type("second argument (exceptions) must be a sequence")),
        };
        if items.is_empty() { return Err(cold_value("second argument (exceptions) must be a non-empty sequence")); }
        let mut plain = true;
        for (i, &x) in items.iter().enumerate() {
            let Some(n) = self.exc_class_name(x) else {
                return Err(VmErr::Raised(s!("ValueError: Item ", int i as i64, " of second argument (exceptions) is not an exception")));
            };
            plain &= crate::vm::globals::matches_exc_class(n, "Exception");
        }
        let name = match (name, plain) {
            ("ExceptionGroup", false) => return Err(cold_type("Cannot nest BaseExceptions in an ExceptionGroup")),
            (_, true) => "ExceptionGroup",
            _ => "BaseExceptionGroup",
        };
        self.heap.alloc(HeapObj::ExcInstance(name.into(), alloc::vec![msg, excs], Val::undef()))
    }

    /* A group like `g` holding `items`, through its own `derive` when a class over a group defines one. */
    fn derive_group(&mut self, g: Val, items: Vec<Val>, r: &mut Replay) -> Result<Val, VmErr> {
        let list = self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))?;
        if let Some((f, class)) = self.instance_class(g).and_then(|c| self.lookup_class_member(c, "derive")) { return self.with_roots([list], |vm| r.call(vm, f, class, [g, list, Val::undef()])); }
        let msg = self.group_parts(g).0;
        self.with_roots([list], |vm| vm.new_group("BaseExceptionGroup", &[msg, list]))
    }

    /* Whether member `x` passes `cond`. */
    fn passes(&mut self, cond: &Cond, x: Val, r: &mut Replay) -> Result<bool, VmErr> {
        match *cond {
            Cond::Types(t) => self.instance_of(x, t),
            Cond::Pred(f) => {
                let v = r.call(self, f, Val::undef(), [x, Val::undef(), Val::undef()])?;
                Ok(self.truthy(v))
            }
        }
    }

    /* `g.split(cond)`, the part that passes and the part that does not, each None when empty, the notes and chain copied over. */
    fn split_group(&mut self, g: Val, cond: &Cond, r: &mut Replay) -> Result<(Option<Val>, Option<Val>), VmErr> {
        if self.passes(cond, g, r)? { return Ok((Some(g), None)); }
        let items = self.group_parts(g).1;
        let (mut hit, mut miss) = (Vec::new(), Vec::new());
        self.with_roots(items.iter().copied().chain([g]), |vm| {
            for &x in &items {
                if vm.is_group(x) {
                    let (m, rest) = vm.split_group(x, cond, r)?;
                    hit.extend(m);
                    miss.extend(rest);
                } else if vm.passes(cond, x, r)? { hit.push(x); } else { miss.push(x); }
                vm.temp_roots.extend(hit.last().copied().into_iter().chain(miss.last().copied()));
            }
            let chain = vm.exc_chain(g);
            let mut part = |vm: &mut Self, xs: Vec<Val>| -> Result<Option<Val>, VmErr> {
                if xs.is_empty() { return Ok(None); }
                let d = vm.derive_group(g, xs, r)?;
                vm.temp_roots.push(d);
                vm.set_exc_chain(d, chain)?;
                Ok(Some(d))
            };
            Ok((part(vm, hit)?, part(vm, miss)?))
        })
    }

    /* `split`, `subgroup` and `derive` of an exception group, their Python calls frames of the loop when `frames`, whose job gives the result. */
    pub(crate) fn group_method(&mut self, recv: Val, name: &str, pos: &[Val], chunk: &SSAChunk, frames: bool) -> Result<(), VmErr> {
        let name = match name { "derive" => "derive", "subgroup" => "subgroup", _ => "split" };
        // What `derive` takes is read once, the job running it again over the list.
        let items = if name == "derive" { self.iterable_items(pos[0], chunk)? } else { Vec::new() };
        let v = if frames {
            self.calls_run(alloc::boxed::Box::new(GroupCalls { run: GroupRun::Method(recv, name, pos[0], items), replay: Replay::asked() }), true)?
        } else {
            Some(self.with_roots(items.iter().copied().chain([recv, pos[0]]), |vm| vm.group_value(recv, name, pos[0], items.clone(), &mut Replay::nested()))?)
        };
        if let Some(v) = v { self.push(v); }
        Ok(())
    }

    /* What group method `name` of `g` given `arg` gives, `derive` making a group of `items`, its calls made through `r`. */
    fn group_value(&mut self, g: Val, name: &str, arg: Val, items: Vec<Val>, r: &mut Replay) -> Result<Val, VmErr> {
        if name == "derive" { return self.derive_group(g, items, r); }
        let cond = self.group_cond(arg)?;
        let (m, rest) = self.split_group(g, &cond, r)?;
        let (m, rest) = (m.unwrap_or(Val::none()), rest.unwrap_or(Val::none()));
        if name == "subgroup" { return Ok(m); }
        self.heap.alloc(HeapObj::Tuple(alloc::vec![m, rest]))
    }

    /* The condition `split` and `subgroup` take, an exception type, a tuple of them or a predicate. */
    fn group_cond(&self, c: Val) -> Result<Cond, VmErr> {
        let exc_type = |vm: &Self, t: Val| match vm.heap.try_get(t) {
            Some(HeapObj::Type(n)) => crate::vm::globals::matches_exc_class(n, "BaseException"),
            Some(HeapObj::Class(..)) => vm.exc_base(t).is_some(),
            _ => false,
        };
        let types = match self.heap.try_get(c) { Some(HeapObj::Tuple(t)) => t.iter().all(|&x| exc_type(self, x)), _ => exc_type(self, c) };
        if types { return Ok(Cond::Types(c)); }
        if self.is_callable(c) { return Ok(Cond::Pred(c)); }
        Err(cold_type("expected an exception type, a tuple of exception types, or a callable (other than a class)"))
    }

    /* The leaves of group `g`, the plain exceptions under every nesting. */
    fn leaves(&self, g: Val, out: &mut Vec<Val>) {
        for x in self.group_parts(g).1 {
            if self.is_group(x) { self.leaves(x, out); } else { out.push(x); }
        }
    }

    /* `except*` starts, the state list `[raised, group, claimed, reraised, new]` its steps share. */
    pub(crate) fn star_start(&mut self, exc: Val) -> Result<Val, VmErr> {
        let group = if self.is_group(exc) { exc } else {
            let t = self.heap.alloc(HeapObj::Tuple(alloc::vec![exc]))?;
            let empty = self.heap.alloc(HeapObj::Str(alloc::string::String::new()))?;
            self.heap.alloc(HeapObj::ExcInstance(if self.exc_class_name(exc).is_some_and(|n| crate::vm::globals::matches_exc_class(n, "Exception")) { "ExceptionGroup".into() } else { "BaseExceptionGroup".into() }, alloc::vec![empty, t], Val::undef()))?
        };
        let lists = [self.empty_list()?, self.empty_list()?, self.empty_list()?];
        self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(alloc::vec![exc, group, lists[0], lists[1], lists[2], group]))))
    }

    fn empty_list(&mut self) -> Result<Val, VmErr> { self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(Vec::new())))) }

    /* The six parts of an `except*` state, the last the part of the group no clause matched yet, None once none is left. */
    fn star_state(&self, state: Val) -> Result<[Val; 6], VmErr> {
        match self.heap.try_get(state) {
            Some(HeapObj::List(l)) if l.borrow().len() == 6 => { let l = l.borrow(); Ok([l[0], l[1], l[2], l[3], l[4], l[5]]) }
            _ => Err(cold_runtime("except* state corrupted")),
        }
    }

    fn list_items(&self, l: Val) -> Vec<Val> { match self.heap.try_get(l) { Some(HeapObj::List(l)) => l.borrow().clone(), _ => Vec::new() } }

    fn list_push(&self, l: Val, v: Val) { if let Some(HeapObj::List(l)) = self.heap.try_get(l) { l.borrow_mut().push(v); } }

    /* One `except* T` clause, the part it matches no earlier clause took (Python None for none), or None once a `derive` call opens a frame whose job gives it. */
    pub(crate) fn star_split(&mut self, state: Val, t: Val) -> Result<Option<Val>, VmErr> {
        let group_type = |vm: &Self, t: Val| match vm.heap.try_get(t) {
            Some(HeapObj::Type(n)) => crate::vm::globals::matches_exc_class(n, "BaseExceptionGroup"),
            Some(HeapObj::Class(..)) => vm.exc_base(t).is_some_and(|b| crate::vm::globals::matches_exc_class(b, "BaseExceptionGroup")),
            _ => false,
        };
        let catches_group = match self.heap.try_get(t) { Some(HeapObj::Tuple(ts)) => ts.iter().any(|&x| group_type(self, x)), _ => group_type(self, t) };
        if catches_group { return Err(cold_type("catching ExceptionGroup with except* is not allowed. Use except instead.")); }
        let [raised, group, claimed, .., left] = self.star_state(state)?;
        // A plain exception matches as the group made around it.
        if raised.0 != group.0 {
            if self.list_items(claimed).is_empty() && self.instance_of(raised, t)? { self.list_push(claimed, raised); return Ok(Some(group)); }
            return Ok(Some(Val::none()));
        }
        // Each clause splits what the clauses before it left, as Python's does.
        if left.is_none() { return Ok(Some(Val::none())); }
        self.calls_run(alloc::boxed::Box::new(GroupCalls { run: GroupRun::Split(state, left, t), replay: Replay::asked() }), true)
    }

    /* The split an `except*` clause made of what was left, keeping the part left now in `state` and claiming the matched part's leaves, giving that part or None. */
    fn star_took(&mut self, state: Val, (m, rest): (Option<Val>, Option<Val>)) -> Result<Val, VmErr> {
        let [_, _, claimed, ..] = self.star_state(state)?;
        if let Some(HeapObj::List(l)) = self.heap.try_get(state) { l.borrow_mut()[5] = rest.unwrap_or(Val::none()); }
        let Some(m) = m else { return Ok(Val::none()) };
        let mut leaves = Vec::new();
        self.leaves(m, &mut leaves);
        for x in leaves { self.list_push(claimed, x); }
        Ok(m)
    }

    /* An exception the body of a clause raised, its own match raised again joins what stays unhandled. */
    pub(crate) fn star_raised(&mut self, state: Val, x: Val, matched: Val) -> Result<(), VmErr> {
        let [.., reraised, new, _] = self.star_state(state)?;
        if x.0 == matched.0 {
            let mut leaves = Vec::new();
            self.leaves(matched, &mut leaves);
            for l in leaves { self.list_push(reraised, l); }
        } else {
            self.list_push(new, x);
        }
        Ok(())
    }

    /* The end of the clauses, what the statement raises (Python None when every member was handled), or None once a `derive` call opens a frame whose job gives it. */
    pub(crate) fn star_end(&mut self, state: Val) -> Result<Option<Val>, VmErr> {
        self.calls_run(alloc::boxed::Box::new(GroupCalls { run: GroupRun::End(state), replay: Replay::asked() }), true)
    }

    /* What `star_end` gives, its calls of `derive` made through `r`. */
    fn star_ended(&mut self, state: Val, r: &mut Replay) -> Result<Val, VmErr> {
        let [raised, group, claimed, reraised, new, _] = self.star_state(state)?;
        let (claimed, reraised, new) = (self.list_items(claimed), self.list_items(reraised), self.list_items(new));
        let handled: Vec<Val> = claimed.into_iter().filter(|c| !reraised.iter().any(|r| r.0 == c.0)).collect();
        let mut all = Vec::new();
        self.leaves(group, &mut all);
        // Nothing handled and nothing new raises what came in, the same object.
        if handled.is_empty() && new.is_empty() { return Ok(raised); }
        let rest = if all.iter().all(|x| handled.iter().any(|h| h.0 == x.0)) { None } else {
            let unhandled: Vec<Val> = all.iter().copied().filter(|x| !handled.iter().any(|h| h.0 == x.0)).collect();
            self.split_kept(group, &unhandled, r)?
        };
        // A plain exception left unhandled raises as itself.
        let rest = if self.is_group(raised) { rest } else { rest.map(|_| raised) };
        match (rest, new.len()) {
            (None, 0) => Ok(Val::none()),
            (Some(r), 0) => Ok(r),
            (None, 1) => Ok(new[0]),
            // The part left unhandled stays rooted while the group around it is made.
            (rest, _) => self.with_roots(rest, |vm| {
                let mut items = new;
                items.extend(rest);
                let list = vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))?;
                let empty = vm.with_roots([list], |vm| vm.heap.alloc(HeapObj::Str(alloc::string::String::new())))?;
                vm.with_roots([list, empty], |vm| vm.new_group("BaseExceptionGroup", &[empty, list]))
            }),
        }
    }

    /* The part of `g` whose leaves are among `keep`, its structure kept. */
    fn split_kept(&mut self, g: Val, keep: &[Val], r: &mut Replay) -> Result<Option<Val>, VmErr> {
        // The parts made below stay rooted until the group holding them is made, then the caller roots that one.
        let base = self.temp_roots.len();
        let kept = self.split_kept_rooted(g, keep, r);
        self.temp_roots.truncate(base);
        kept
    }

    fn split_kept_rooted(&mut self, g: Val, keep: &[Val], r: &mut Replay) -> Result<Option<Val>, VmErr> {
        let items = self.group_parts(g).1;
        let mut hit = Vec::new();
        for &x in &items {
            if self.is_group(x) {
                if let Some(m) = self.split_kept(x, keep, r)? { hit.push(m); self.temp_roots.push(m); }
            } else if keep.iter().any(|k| k.0 == x.0) { hit.push(x); }
        }
        if hit.is_empty() { return Ok(None); }
        if hit.len() == items.len() && hit.iter().zip(&items).all(|(a, b)| a.0 == b.0) { return Ok(Some(g)); }
        let d = self.derive_group(g, hit, r)?;
        let chain = self.exc_chain(g);
        self.with_roots([d], |vm| vm.set_exc_chain(d, chain))?;
        Ok(Some(d))
    }
}
