use alloc::{rc::Rc, string::String, vec, vec::Vec};
use core::cell::RefCell;

use crate::parser::SSAChunk;
use super::VM;
use super::types::*;

// Instance attribute holding the chain of a user exception, a name no attribute syntax reaches.
const CHAIN_KEY: &str = "#chain";

// Links a traceback follows before it stops, so a cycle cannot run forever.
const CHAIN_MAX: usize = 100;

/* What an exception was raised from, each exception with where it was raised. */
#[derive(Clone, Copy)]
pub(crate) struct Chain {
    pub cause: Val,
    pub cause_at: Option<u32>,
    pub context: Val,
    pub context_at: Option<u32>,
    // Set by a `from` clause, which hides the context.
    pub suppress: bool,
    // The list `add_note` fills, None until the first note.
    pub notes: Val,
}

impl Chain {
    fn none() -> Self { Self { cause: Val::none(), cause_at: None, context: Val::none(), context_at: None, suppress: false, notes: Val::none() } }
}

fn at_val(at: Option<u32>) -> Val { at.map_or(Val::none(), |p| Val::int(p as i64)) }

fn val_at(v: Val) -> Option<u32> { v.is_int().then(|| v.as_int() as u32) }

impl<'a> VM<'a> {
    pub(crate) fn exc_chain(&self, e: Val) -> Chain {
        let stored = match self.heap.try_get(e) {
            Some(HeapObj::ExcInstance(_, _, c)) => *c,
            Some(HeapObj::Instance(_, attrs)) => attrs.borrow().iter()
                .find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == CHAIN_KEY))
                .map_or(Val::undef(), |(_, v)| v),
            _ => Val::undef(),
        };
        match self.heap.try_get(stored) {
            Some(HeapObj::Tuple(t)) if t.len() == 6 => Chain { cause: t[0], cause_at: val_at(t[1]), context: t[2], context_at: val_at(t[3]), suppress: t[4].is_true(), notes: t[5] },
            _ => Chain::none(),
        }
    }

    fn set_exc_chain(&mut self, e: Val, c: Chain) -> Result<(), VmErr> {
        let t = self.heap.alloc(HeapObj::Tuple(vec![c.cause, at_val(c.cause_at), c.context, at_val(c.context_at), Val::bool(c.suppress), c.notes]))?;
        if let Some(HeapObj::ExcInstance(_, _, slot)) = self.heap.try_get_mut(e) { *slot = t; return Ok(()); }
        let key = self.heap.intern_str(CHAIN_KEY)?;
        if let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(e) { attrs.borrow_mut().insert(key, t, &self.heap); }
        Ok(())
    }

    /* Makes `handled` the context of `exc` when it rose while `handled` was being handled. */
    pub(crate) fn link_context(&mut self, exc: Val, handled: Val, at: Option<u32>) -> Result<(), VmErr> {
        let c = self.exc_chain(exc);
        if exc.0 == handled.0 || !c.context.is_none() { return Ok(()); }
        // A context already leading back to `exc` would close a cycle.
        let mut cur = handled;
        for _ in 0..CHAIN_MAX {
            let next = self.exc_chain(cur).context;
            if next.is_none() { break; }
            if next.0 == exc.0 { return Ok(()); }
            cur = next;
        }
        self.set_exc_chain(exc, Chain { context: handled, context_at: at, ..c })
    }

    /* `raise exc from cause`, a class made into an instance and None hiding the context. */
    pub(crate) fn link_cause(&mut self, exc: Val, cause: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        let cause = match self.heap.try_get(cause) {
            _ if cause.is_none() => cause,
            Some(HeapObj::ExcInstance(..)) => cause,
            Some(&HeapObj::Instance(cls, _)) if self.exc_base(cls).is_some() => cause,
            Some(HeapObj::Type(_) | HeapObj::Class(..)) if self.exc_base(cause).is_some() => {
                self.push(cause);
                self.exec_call(0, chunk)?;
                self.pop()?
            }
            _ => return Err(cold_type("exception causes must derive from BaseException")),
        };
        let cause_at = self.handling.iter().rev().find(|(h, _)| h.0 == cause.0).and_then(|&(_, at)| at);
        let c = self.exc_chain(exc);
        self.with_roots([cause], |vm| vm.set_exc_chain(exc, Chain { cause, cause_at, suppress: true, ..c }))
    }

    /* `e.__cause__`, `e.__context__` and `e.__suppress_context__` of an exception. */
    pub(crate) fn chain_attr(&self, e: Val, name: &str) -> Option<Val> {
        let c = self.exc_chain(e);
        match name {
            "__cause__" => Some(c.cause),
            "__context__" => Some(c.context),
            "__suppress_context__" => Some(Val::bool(c.suppress)),
            "__notes__" if !c.notes.is_none() => Some(c.notes),
            _ => None,
        }
    }

    /* `e.add_note(note)`, appended to the list `__notes__` reads, made on the first note. */
    pub(crate) fn add_note(&mut self, e: Val, note: Val) -> Result<(), VmErr> {
        if !matches!(self.heap.try_get(note), Some(HeapObj::Str(_))) {
            return Err(VmErr::TypeMsg(crate::s!("note must be a str, not '", str self.type_name(note), "'")));
        }
        let c = self.exc_chain(e);
        if let Some(HeapObj::List(l)) = self.heap.try_get(c.notes) { l.borrow_mut().push(note); return Ok(()); }
        let notes = self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(vec![note]))))?;
        self.with_roots([notes], |vm| vm.set_exc_chain(e, Chain { notes, ..c }))
    }

    /* The notes of `e`, each on a line of its own after its message. */
    pub(crate) fn notes_text(&self, e: Val) -> String {
        let mut out = String::new();
        if let Some(HeapObj::List(l)) = self.heap.try_get(self.exc_chain(e).notes) {
            for &n in l.borrow().iter() { out.push('\n'); out.push_str(&self.display(n)); }
        }
        out
    }

    /* The exceptions the uncaught one was raised from, oldest first, each with how the next came. */
    pub fn render_chain(&self, src: &str, path: Option<&str>) -> String {
        let Some(mut cur) = self.pending.exc_val else { return String::new() };
        let mut links: Vec<(Val, Option<u32>, &str)> = Vec::new();
        for _ in 0..CHAIN_MAX {
            let c = self.exc_chain(cur);
            let (next, at, how) = if c.suppress { (c.cause, c.cause_at, "The above exception was the direct cause of the following exception:") }
                else { (c.context, c.context_at, "During handling of the above exception, another exception occurred:") };
            if next.is_none() || links.iter().any(|l| l.0.0 == next.0) { break; }
            links.push((next, at, how));
            cur = next;
        }
        let mut out = String::new();
        for &(exc, at, how) in links.iter().rev() {
            let name = self.exc_type_name(exc);
            let text = self.display(exc);
            let notes = self.notes_text(exc);
            let msg = if text.is_empty() { crate::s!(str &name, str &notes) } else { crate::s!(str &name, ": ", str &text, str &notes) };
            match at {
                Some(p) => out.push_str(&crate::parser::Diagnostic { start: p as usize, end: p as usize, msg }.render(src, path)),
                None => { out.push_str("error: "); out.push_str(&msg); out.push('\n'); }
            }
            out.push('\n');
            out.push_str(how);
            out.push_str("\n\n");
        }
        out
    }
}
