/* Exception groups, their methods and the steps `except*` compiles to. */

use super::VM;
use super::types::*;
use super::opcodes::attr_lookup::AttrLookup;
use crate::parser::SSAChunk;
use crate::s;
use alloc::{rc::Rc, vec::Vec};
use core::cell::RefCell;

/* What a split keeps, instances of a classinfo, those a predicate accepts, or the leaves of an `except*` clause. */
enum Cond<'c> { Types(Val), Pred(Val), Star(Val, &'c [Val]) }

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
    fn derive_group(&mut self, g: Val, items: Vec<Val>, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let list = self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items))))?;
        let own = self.instance_class(g).and_then(|c| self.lookup_class_member(c, "derive")).is_some();
        if own { return self.with_roots([list], |vm| vm.call_hook(g, "derive", &[list], chunk)); }
        let msg = self.group_parts(g).0;
        self.with_roots([list], |vm| vm.new_group("BaseExceptionGroup", &[msg, list]))
    }

    /* Whether member `x` passes `cond`. */
    fn passes(&mut self, cond: &Cond, x: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        match *cond {
            Cond::Types(t) => self.instance_of(x, t),
            Cond::Pred(f) => {
                self.call_with(f, None, &[x], &[], chunk)?;
                let r = self.pop()?;
                Ok(self.truthy(r))
            }
            Cond::Star(t, claimed) => Ok(!claimed.iter().any(|c| c.0 == x.0) && self.instance_of(x, t)?),
        }
    }

    /* `g.split(cond)`, the part that passes and the part that does not, each None when empty, the notes and chain copied over. */
    fn split_group(&mut self, g: Val, cond: &Cond, chunk: &SSAChunk) -> Result<(Option<Val>, Option<Val>), VmErr> {
        if !matches!(cond, Cond::Star(..)) && self.passes(cond, g, chunk)? { return Ok((Some(g), None)); }
        let items = self.group_parts(g).1;
        let (mut hit, mut miss) = (Vec::new(), Vec::new());
        self.with_roots(items.iter().copied().chain([g]), |vm| {
            for &x in &items {
                if vm.is_group(x) {
                    let (m, r) = vm.split_group(x, cond, chunk)?;
                    hit.extend(m);
                    miss.extend(r);
                } else if vm.passes(cond, x, chunk)? { hit.push(x); } else { miss.push(x); }
                vm.temp_roots.extend(hit.last().copied().into_iter().chain(miss.last().copied()));
            }
            let chain = vm.exc_chain(g);
            let part = |vm: &mut Self, xs: Vec<Val>| -> Result<Option<Val>, VmErr> {
                if xs.is_empty() { return Ok(None); }
                let d = vm.derive_group(g, xs, chunk)?;
                vm.temp_roots.push(d);
                vm.set_exc_chain(d, chain)?;
                Ok(Some(d))
            };
            Ok((part(vm, hit)?, part(vm, miss)?))
        })
    }

    /* `split`, `subgroup` and `derive` of an exception group. */
    pub(crate) fn group_method(&mut self, recv: Val, name: &str, pos: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        if name == "derive" {
            let items = self.iterable_items(pos[0], chunk)?;
            let d = self.derive_group(recv, items, chunk)?;
            self.push(d);
            return Ok(());
        }
        let cond = self.group_cond(pos[0])?;
        let (m, r) = self.split_group(recv, &cond, chunk)?;
        let (m, r) = (m.unwrap_or(Val::none()), r.unwrap_or(Val::none()));
        if name == "subgroup" { self.push(m); return Ok(()); }
        let pair = self.heap.alloc(HeapObj::Tuple(alloc::vec![m, r]))?;
        self.push(pair);
        Ok(())
    }

    /* The condition `split` and `subgroup` take, an exception type, a tuple of them or a predicate. */
    fn group_cond(&self, c: Val) -> Result<Cond<'static>, VmErr> {
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
        self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(alloc::vec![exc, group, lists[0], lists[1], lists[2]]))))
    }

    fn empty_list(&mut self) -> Result<Val, VmErr> { self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(Vec::new())))) }

    /* The five parts of an `except*` state. */
    fn star_state(&self, state: Val) -> Result<[Val; 5], VmErr> {
        match self.heap.try_get(state) {
            Some(HeapObj::List(l)) if l.borrow().len() == 5 => { let l = l.borrow(); Ok([l[0], l[1], l[2], l[3], l[4]]) }
            _ => Err(cold_runtime("except* state corrupted")),
        }
    }

    fn list_items(&self, l: Val) -> Vec<Val> { match self.heap.try_get(l) { Some(HeapObj::List(l)) => l.borrow().clone(), _ => Vec::new() } }

    fn list_push(&self, l: Val, v: Val) { if let Some(HeapObj::List(l)) = self.heap.try_get(l) { l.borrow_mut().push(v); } }

    /* One `except* T` clause, the part of the group it matches that no earlier clause took, None when nothing matches. */
    pub(crate) fn star_split(&mut self, state: Val, t: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let group_type = |vm: &Self, t: Val| match vm.heap.try_get(t) {
            Some(HeapObj::Type(n)) => crate::vm::globals::matches_exc_class(n, "BaseExceptionGroup"),
            Some(HeapObj::Class(..)) => vm.exc_base(t).is_some_and(|b| crate::vm::globals::matches_exc_class(b, "BaseExceptionGroup")),
            _ => false,
        };
        let catches_group = match self.heap.try_get(t) { Some(HeapObj::Tuple(ts)) => ts.iter().any(|&x| group_type(self, x)), _ => group_type(self, t) };
        if catches_group { return Err(cold_type("catching ExceptionGroup with except* is not allowed. Use except instead.")); }
        let [raised, group, claimed, ..] = self.star_state(state)?;
        let taken = self.list_items(claimed);
        // A plain exception matches as the group made around it.
        if raised.0 != group.0 {
            if taken.is_empty() && self.instance_of(raised, t)? { self.list_push(claimed, raised); return Ok(group); }
            return Ok(Val::none());
        }
        let (m, _) = self.split_group(group, &Cond::Star(t, &taken), chunk)?;
        let Some(m) = m else { return Ok(Val::none()) };
        let mut leaves = Vec::new();
        self.leaves(m, &mut leaves);
        for x in leaves { self.list_push(claimed, x); }
        Ok(m)
    }

    /* An exception the body of a clause raised, its own match raised again joins what stays unhandled. */
    pub(crate) fn star_raised(&mut self, state: Val, x: Val, matched: Val) -> Result<(), VmErr> {
        let [.., reraised, new] = self.star_state(state)?;
        if x.0 == matched.0 {
            let mut leaves = Vec::new();
            self.leaves(matched, &mut leaves);
            for l in leaves { self.list_push(reraised, l); }
        } else {
            self.list_push(new, x);
        }
        Ok(())
    }

    /* The end of the clauses, what the statement raises, None when every member was handled. */
    pub(crate) fn star_end(&mut self, state: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let [raised, group, claimed, reraised, new] = self.star_state(state)?;
        let (claimed, reraised, new) = (self.list_items(claimed), self.list_items(reraised), self.list_items(new));
        let handled: Vec<Val> = claimed.into_iter().filter(|c| !reraised.iter().any(|r| r.0 == c.0)).collect();
        let mut all = Vec::new();
        self.leaves(group, &mut all);
        // Nothing handled and nothing new raises what came in, the same object.
        if handled.is_empty() && new.is_empty() { return Ok(raised); }
        let rest = if all.iter().all(|x| handled.iter().any(|h| h.0 == x.0)) { None } else {
            let unhandled: Vec<Val> = all.iter().copied().filter(|x| !handled.iter().any(|h| h.0 == x.0)).collect();
            self.split_kept(group, &unhandled, chunk)?
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
    fn split_kept(&mut self, g: Val, keep: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        // The parts made below stay rooted until the group holding them is made, then the caller roots that one.
        let base = self.temp_roots.len();
        let kept = self.split_kept_rooted(g, keep, chunk);
        self.temp_roots.truncate(base);
        kept
    }

    fn split_kept_rooted(&mut self, g: Val, keep: &[Val], chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let items = self.group_parts(g).1;
        let mut hit = Vec::new();
        for &x in &items {
            if self.is_group(x) {
                if let Some(m) = self.split_kept(x, keep, chunk)? { hit.push(m); self.temp_roots.push(m); }
            } else if keep.iter().any(|k| k.0 == x.0) { hit.push(x); }
        }
        if hit.is_empty() { return Ok(None); }
        if hit.len() == items.len() && hit.iter().zip(&items).all(|(a, b)| a.0 == b.0) { return Ok(Some(g)); }
        let d = self.derive_group(g, hit, chunk)?;
        let chain = self.exc_chain(g);
        self.with_roots([d], |vm| vm.set_exc_chain(d, chain))?;
        Ok(Some(d))
    }
}
