use alloc::{string::String, vec::Vec};
use crate::s;
use crate::lexer::TokenType;
use crate::util::suggest::nearest;
use super::VM;
use crate::value::*;

// past this many candidates no name is suggested
const MOST: usize = 750;
const LONGEST: usize = 40;

/* names and slots of a body's locals which a missed name is measured against */
pub(crate) type Locals = alloc::rc::Rc<[(String, usize)]>;

/* what an error that may go uncaught missed so its traceback can add a hint */
#[derive(Clone)]
pub(crate) enum Missed {
    // missing name and body locals flagged if bound while the frame lived and its module
    Name(String, Locals, Vec<bool>, usize),
    // an attribute the object lacks whose hint is found only if the error is rendered
    Attr(Val, String),
}

/* the candidate nearest to `name` when the candidates are few enough to measure */
fn near<'c>(name: &str, candidates: &[&'c str]) -> Option<&'c str> {
    if candidates.len() > MOST || name.len() > LONGEST { return None; }
    nearest(name, candidates.iter().copied())
}

impl<'a> VM<'a> {
    /* notes that `obj` lacks `name` for the hint an uncaught attribute error gains */
    #[cold]
    pub(crate) fn note_missing_attr(&mut self, obj: Val, name: &str) { self.missed = Some(Missed::Attr(obj, name.into())); }

    /* notes a name `chunk` missed with its frame's bindings and defers the hint until rendering */
    #[cold]
    pub(crate) fn note_missing_name(&mut self, name: &str, chunk: &crate::syntax::chunk::SSAChunk) {
        if matches!(&self.missed, Some(Missed::Name(n, ..)) if n == name) { return; }
        let key = (chunk.id.get() as usize, chunk.names.len());
        let names = match self.local_names.get(&key) {
            Some(n) => n.clone(),
            None => { let n: Locals = self.local_slots(chunk).into(); self.local_names.insert(key, n.clone()); n }
        };
        let bound = names.iter().map(|&(_, slot)| self.regs.get(self.base + slot).is_some_and(|&v| !self.deref(v).is_undef())).collect();
        self.missed = Some(Missed::Name(name.into(), names, bound, self.chunk_module_id(chunk)));
    }

    /* suggests a missing name's nearest match among its frame's locals and its module and builtins */
    fn name_hint(&self, name: &str, locals: &[(String, usize)], bound: &[bool], module: usize) -> Option<String> {
        let mut names: Vec<&str> = locals.iter().zip(bound).filter(|(_, b)| **b).map(|((n, _), _)| n.as_str()).collect();
        names.extend(self.scopes[module].iter().map(|(n, _)| n).filter(|n| !n.starts_with('#')));
        names.extend(self.builtins.keys().copied());
        names.extend(self.natives.functions().iter().map(|n| n.name));
        names.extend(self.natives.types.iter().copied());
        // the words a program writes for nil and the booleans then the ellipsis builtin
        if let Some(sx) = self.rt.front.and_then(|f| f.syntax) { names.extend([TokenType::Nil, TokenType::True, TokenType::False].into_iter().filter_map(|k| sx.spelling(k))); }
        names.push(self.natives.names().ellipsis);
        near(name, &names).map(|c| s!("Did you mean: '", str c, "'?"))
    }

    /* the attributes and methods a misspelt attribute is measured against */
    fn attr_names(&self, obj: Val) -> Vec<String> {
        let members = |vm: &Self, cls: Val, out: &mut Vec<String>| for c in vm.mro_of(cls) {
            out.extend(vm.heap.member_names(c));
        };
        let mut out = Vec::new();
        match self.heap.try_get(obj) {
            Some(&HeapObj::Object { meta: cls, slots: ref attrs }) if cls.is_heap() => {
                out.extend(attrs.borrow().iter().filter_map(|(k, _)| match self.heap.try_get(k) { Some(HeapObj::Str(s)) => Some(s.clone()), _ => None }));
                members(self, cls, &mut out);
            }
            _ if self.heap.is_class(obj) => members(self, obj, &mut out),
            Some(HeapObj::Module(_, attrs)) => out.extend(attrs.iter().map(|(n, _)| n.clone())),
            _ => out.extend(self.natives.kind_methods(crate::lang::kind_of(&self.heap, obj)).iter().map(|n| String::from(n.name))),
        }
        out
    }

    /* the hint `e` renders with when the last noted miss belongs to it */
    pub(crate) fn hint_for(&self, e: &VmErr) -> Option<String> {
        match (self.missed.as_ref()?, e) {
            // the miss read off the error or off the runtime's words for it
            (Missed::Name(n, locals, bound, module), e) if e.missing_name() == Some(n.as_str()) || matches!(e, VmErr::Of(ErrKind::Name, m) if m.contains(&s!("'", str n, "'"))) => self.name_hint(n, locals, bound, *module),
            (Missed::Attr(obj, name), VmErr::Attribute(msg) | VmErr::Of(ErrKind::Attribute, msg)) if msg.ends_with(&s!("'", str name, "'")) => {
                // underscored names are candidates only when the missed name starts with an underscore
                let names = self.attr_names(*obj);
                let shown: Vec<&str> = names.iter().map(String::as_str).filter(|n| name.starts_with('_') || !n.starts_with('_')).collect();
                near(name, &shown).map(|c| s!("Did you mean '.", str c, "' instead of '.", str name, "'?"))
            }
            _ => None,
        }
    }
}
