/* The `Did you mean` an uncaught NameError or AttributeError gains as its traceback renders, found the way Python
   finds it, among the names in scope or the attributes of the object. */

use alloc::{string::String, vec::Vec};
use crate::s;
use crate::util::suggest::nearest;
use super::VM;
use super::types::*;

// Past this many candidates no name is suggested.
const MOST: usize = 750;
const LONGEST: usize = 40;

/* The names and slots of the locals of a body, as a missed name measures them. */
pub(crate) type Locals = alloc::rc::Rc<[(String, usize)]>;

/* What an error that may still go uncaught missed, for the hint its traceback adds. */
#[derive(Clone)]
pub(crate) enum Missed {
    // A name not in scope, with the locals of its body, those its frame bound while it was alive, and its module.
    Name(String, Locals, Vec<bool>, usize),
    // An attribute the object lacks, the hint found only if the error is rendered.
    Attr(Val, String),
}

/* The hint of an attribute another language or a near type names, ahead of any near spelling. */
fn crossed(ty: &str, name: &str) -> Option<String> {
    let method = |m: &str| Some(s!("Did you mean '.", str m, "'?"));
    let raw = |h: &str| Some(String::from(h));
    match (name, ty) {
        ("push", "list") => method("append"),
        ("concat" | "addAll", "list") => method("extend"),
        ("contains", "list") => raw("Use 'x in list'."),
        ("add", "list" | "frozenset") | ("remove" | "discard" | "update", "frozenset") => raw("Did you mean to use a 'set' object?"),
        ("toUpperCase", "str") => method("upper"),
        ("toLowerCase", "str") => method("lower"),
        ("trimStart", "str") => method("lstrip"),
        ("trimEnd", "str") => method("rstrip"),
        ("keySet", "dict") => method("keys"),
        ("entrySet" | "entries", "dict") => method("items"),
        ("putAll", "dict") => method("update"),
        ("put", "dict") => raw("Use d[k] = v."),
        ("append" | "extend" | "insert" | "remove", "tuple") => raw("Did you mean to use a 'list' object?"),
        ("__or__" | "__and__" | "__xor__" | "__lshift__" | "__rshift__", "float") => raw("Did you mean to use an 'int' object? Bitwise operators are not supported by 'float'."),
        _ => None,
    }
}

/* The methods of builtin type `ty`, its own and the operator dunders it takes. */
fn builtin_names(ty: &str) -> impl Iterator<Item = String> + '_ {
    let ops = super::methods::methods_of("op").filter(move |id| super::methods::operator::applies(ty, id.name()));
    super::methods::methods_of(ty).chain(ops).map(|id| String::from(id.name()))
}

/* The candidate nearest to `name` when the candidates are few enough to measure. */
fn near<'c>(name: &str, candidates: &[&'c str]) -> Option<&'c str> {
    if candidates.len() > MOST || name.len() > LONGEST { return None; }
    nearest(name, candidates.iter().copied())
}

impl<'a> VM<'a> {
    /* Notes that `obj` lacks `name`, for the hint of the AttributeError if it goes uncaught. */
    #[cold]
    pub(crate) fn note_missing_attr(&mut self, obj: Val, name: &str) { self.missed = Some(Missed::Attr(obj, name.into())); }

    /* Notes a name `chunk` could not find, with what its frame binds, for the hint measured only if the error is rendered. */
    #[cold]
    pub(crate) fn note_missing_name(&mut self, name: &str, chunk: &crate::parser::SSAChunk) {
        if matches!(&self.missed, Some(Missed::Name(n, ..)) if n == name) { return; }
        let key = (chunk as *const crate::parser::SSAChunk as usize, chunk.names.len());
        let names = match self.local_names.get(&key) {
            Some(n) => n.clone(),
            None => { let n: Locals = self.local_slots(chunk).into(); self.local_names.insert(key, n.clone()); n }
        };
        let bound = names.iter().map(|&(_, slot)| self.regs.get(self.base + slot).is_some_and(|&v| !self.deref(v).is_undef())).collect();
        self.missed = Some(Missed::Name(name.into(), names, bound, self.chunk_module_id(chunk)));
    }

    /* `Did you mean: 'x'?` for a missing name, among the locals of its frame, its module and the builtins. */
    fn name_hint(&self, name: &str, locals: &[(String, usize)], bound: &[bool], module: usize) -> Option<String> {
        let mut names: Vec<&str> = locals.iter().zip(bound).filter(|(_, b)| **b).map(|((n, _), _)| n.as_str()).collect();
        names.extend(self.scopes[module].iter().map(|(n, _)| n).filter(|n| !n.starts_with('#')));
        names.extend(self.builtins.keys().copied());
        names.extend(super::NativeFnId::ALL.iter().map(|id| id.name()));
        names.extend(crate::parser::BUILTIN_TYPES.iter().copied());
        names.extend(["None", "True", "False", "Ellipsis", "NotImplemented"]);
        near(name, &names).map(|c| s!("Did you mean: '", str c, "'?"))
    }

    /* The names a misspelt attribute is measured against, the attributes and methods without the shared dunders. */
    fn attr_names(&self, obj: Val) -> Vec<String> {
        let members = |vm: &Self, cls: Val, out: &mut Vec<String>| for c in vm.mro_of(cls) {
            if let Some(HeapObj::Class(_, _, m)) = vm.heap.try_get(c) { out.extend(m.borrow().iter().map(|(n, _)| n.clone())); }
        };
        let mut out = Vec::new();
        match self.heap.try_get(obj) {
            Some(&HeapObj::Instance(cls, ref attrs)) => {
                out.extend(attrs.borrow().iter().filter_map(|(k, _)| match self.heap.try_get(k) { Some(HeapObj::Str(s)) => Some(s.clone()), _ => None }));
                members(self, cls, &mut out);
            }
            Some(HeapObj::Class(..)) => members(self, obj, &mut out),
            Some(HeapObj::Module(_, attrs)) => out.extend(attrs.iter().map(|(n, _)| n.clone())),
            _ => out.extend(super::methods::methods_of(self.type_name(obj)).map(|id| String::from(id.name()))),
        }
        out
    }

    /* The names `dir(obj)` lists. */
    pub(crate) fn dir_names(&self, obj: Val) -> Vec<String> {
        let members = |vm: &Self, cls: Val, out: &mut Vec<String>| for c in vm.mro_of(cls) {
            match vm.heap.try_get(c) {
                Some(HeapObj::Class(_, _, m)) => out.extend(m.borrow().iter().map(|(n, _)| n.clone()).filter(|n| !n.starts_with('#'))),
                Some(HeapObj::Type(t)) => out.extend(builtin_names(t)),
                _ => {}
            }
        };
        let object = || builtin_names("object").chain(["__class__", "__doc__", "__module__", "__dict__"].map(String::from));
        let mut out = Vec::new();
        match self.heap.try_get(obj) {
            Some(&HeapObj::Instance(cls, ref attrs)) => {
                out.extend(attrs.borrow().iter().filter_map(|(k, _)| match self.heap.try_get(k) { Some(HeapObj::Str(s)) if !s.starts_with('#') => Some(s.clone()), _ => None }));
                members(self, cls, &mut out);
                out.extend(object());
            }
            Some(HeapObj::Class(..)) => { members(self, obj, &mut out); out.extend(object()); }
            Some(HeapObj::Module(_, attrs)) => out.extend(attrs.iter().map(|(n, _)| n.clone()).chain(["__name__"].map(String::from))),
            Some(HeapObj::Type(t)) => { out.extend(builtin_names(t)); out.extend(builtin_names("object")); out.push("__class__".into()); out.push("__doc__".into()); }
            _ => { out.extend(builtin_names(self.type_name(obj))); out.extend(builtin_names("object")); out.push("__class__".into()); out.push("__doc__".into()); }
        }
        out
    }

    /* The hint the error `e` renders with, when it is the error the last miss was noted for. */
    pub(crate) fn hint_for(&self, e: &VmErr) -> Option<String> {
        match (self.missed.as_ref()?, e) {
            (Missed::Name(n, locals, bound, module), VmErr::Name(m)) if n == m => self.name_hint(n, locals, bound, *module),
            (Missed::Attr(obj, name), VmErr::Attribute(msg)) if msg.ends_with(&s!("'", str name, "'")) => {
                if let Some(h) = crossed(self.type_name(*obj), name) { return Some(h); }
                // An underscored name is measured against underscored ones only when it starts with one itself.
                let names = self.attr_names(*obj);
                let shown: Vec<&str> = names.iter().map(String::as_str).filter(|n| name.starts_with('_') || !n.starts_with('_')).collect();
                near(name, &shown).map(|c| s!("Did you mean '.", str c, "' instead of '.", str name, "'?"))
            }
            _ => None,
        }
    }
}
