use crate::s;

use super::types::*;
use crate::parser::types::OpCode;

use alloc::{string::String, vec::Vec, rc::Rc};
use core::cell::RefCell;
use core::cmp::Ordering;

/* Cap on nested-container rendering depth, stops self-referential prints from overflowing the stack. */
pub(crate) const RENDER_DEPTH_MAX: usize = 100;

/* Cap on total rendered output, bounds breadth the way RENDER_DEPTH_MAX bounds depth. */
pub(crate) const MAX_REPR_LEN: usize = 1_000_000;

/* Same cap for `<` descent, self-referential sequences raise RecursionError. */
const CMP_DEPTH_MAX: usize = 100;

/* Render `bytes` as `b'...'`, printable ASCII verbatim and the rest escaped, in double quotes when it holds a single quote and no double one. */
fn format_bytes(buf: &[u8]) -> String { quote_bytes(buf, false) }

/* `format_bytes`, the single quote escaped even inside double quotes when `escape_single` asks, as a bytearray shows. */
fn quote_bytes(buf: &[u8], escape_single: bool) -> String {
    let quote = if buf.contains(&b'\'') && !buf.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::with_capacity(buf.len() + 3);
    out.push('b');
    out.push(quote as char);
    for &b in buf {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\'' if quote == b'\'' || escape_single => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                out.push_str("\\x");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out.push(quote as char);
    out
}

/* `repr` of a str, quote selection (' unless the text has ' but not ") and backslash escapes for what is not printable, the rest verbatim. */
fn repr_str(s: &str) -> String {
    use core::fmt::Write;
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => { out.push('\\'); out.push(c); }
            c if !crate::util::uni::is_printable(c) => {
                let n = c as u32;
                if n <= 0xff { let _ = write!(out, "\\x{:02x}", n); }
                else if n <= 0xffff { let _ = write!(out, "\\u{:04x}", n); }
                else { let _ = write!(out, "\\U{:08x}", n); }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/* Coerce a numeric pair to f64, returns None if neither operand is a float. */
fn coerce_floats(a: Val, b: Val, heap: &HeapPool) -> Option<Result<(f64, f64), VmErr>> {
    if !a.is_float() && !b.is_float() { return None; }
    let (x, y) = (num_f64(a, heap)?, num_f64(b, heap)?);
    Some(x.and_then(|x| Ok((x, y?))))
}

use super::VM;

impl<'a> VM<'a> {
    pub fn truthy(&self, v: Val) -> bool {
        if v.is_none() || v.is_false() { return false; }
        if v.is_true() { return true; }
        if v.is_int() { return v.as_int() != 0; }
        if v.is_float() { return v.as_float() != 0.0; }
        if v.is_foreign() { return self.heap.foreign_truthy(v).unwrap_or(true); }
        match self.heap.get(v) {
            &HeapObj::Boxed(n) => self.truthy(n),
            HeapObj::Str(s) => !s.is_empty(),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => !b.is_empty(),
            HeapObj::MemoryView(m) => m.len > 0,
            HeapObj::FnCode(_) => true,
            HeapObj::List(l) => !l.borrow().is_empty(),
            HeapObj::Tuple(t) => !t.is_empty(),
            HeapObj::Dict(d) | HeapObj::FrozenDict(d) => !d.borrow().is_empty(),
            HeapObj::Set(s) => !s.borrow().is_empty(),
            HeapObj::FrozenSet(s) => !s.is_empty(),
            HeapObj::Range(s,e,st) => if *st > 0 { s < e } else { s > e },
            HeapObj::BigRange(..) => crate::frontend::int::range_empty(&self.heap, v) == Some(false),
            HeapObj::DictView(d, _) => self.truthy(*d),
            HeapObj::Type(_) | HeapObj::Func(..) | HeapObj::Slice(..) | HeapObj::BoundMethod(..)
            | HeapObj::NativeFn(_) | HeapObj::Class(..) | HeapObj::BoundUserMethod(..)
            | HeapObj::Super(..) | HeapObj::Property(..) | HeapObj::PropertyPart(..)
            | HeapObj::StaticMethod(..) | HeapObj::ClassMethod(..) | HeapObj::Instance(..) | HeapObj::Coroutine(..)
            | HeapObj::Module(..) | HeapObj::Extern(_) | HeapObj::ExcInstance(..)
            | HeapObj::Ellipsis | HeapObj::NotImplemented | HeapObj::GenericAlias(..) | HeapObj::TypeAlias(..)
            | HeapObj::Union(_) | HeapObj::TypeVar(_) | HeapObj::Iter(..) | HeapObj::Cell(_) | HeapObj::Code(_) => true,
        }
    }

    /* Clone the element set out of a `set` or `frozenset`, None for any other type. */
    pub(crate) fn clone_set_items(&self, v: Val) -> Option<ValSet> {
        if !v.is_heap() { return None; }
        match self.heap.get(v) {
            HeapObj::Set(s) => Some(s.borrow().clone()),
            HeapObj::FrozenSet(s) => Some((**s).clone()),
            _ => None,
        }
    }

    /* True for `set` or `frozenset` operands. */
    pub(crate) fn is_set_like(&self, v: Val) -> bool {
        v.is_heap() && matches!(self.heap.get(v), HeapObj::Set(_) | HeapObj::FrozenSet(_))
    }

    /* True for a keys or items view, which takes part in set operators. */
    pub(crate) fn is_set_view(&self, v: Val) -> bool {
        matches!(self.heap.try_get(v), Some(HeapObj::DictView(_, View::Keys | View::Items)))
    }

    /* Both operands as new sets when one is a keys or items view, `any` admitting any iterable. */
    pub(crate) fn view_set_operands(&mut self, a: Val, b: Val, any: bool, chunk: &crate::parser::SSAChunk) -> Result<Option<(Val, Val)>, VmErr> {
        let (va, vb) = (self.is_set_view(a), self.is_set_view(b));
        if !(va || vb) || !(any || (va || self.is_set_like(a)) && (vb || self.is_set_like(b))) { return Ok(None); }
        self.with_roots([a, b, Val::none()], |vm| {
            let x = vm.as_fresh_set(a, chunk)?;
            if let Some(slot) = vm.temp_roots.last_mut() { *slot = x; }
            let y = vm.as_fresh_set(b, chunk)?;
            Ok(Some((x, y)))
        })
    }

    fn as_fresh_set(&mut self, v: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        let items = match self.heap.try_get(v) {
            Some(&HeapObj::DictView(d, kind)) => self.view_items(d, kind)?,
            _ => self.iterable_items(v, chunk)?,
        };
        let s = self.valset_of(&items)?;
        self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))
    }

    /* Alloc a set-algebra result, frozen picks frozenset (left-operand type rule). */
    pub(crate) fn alloc_set_result(&mut self, items: Vec<Val>, frozen: bool) -> Result<Val, VmErr> {
        let s = ValSet::from_vals(&items, &self.heap);
        if frozen { self.heap.alloc(HeapObj::FrozenSet(Rc::new(s))) }
        else { self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s)))) }
    }

    /* Set binops (|, &, ^, -) over set/frozenset, result frozen iff `a` is frozen. */
    // Union/intersection/symmetric-diff/difference items, content membership lets alloc dedup distinct-handle equals.
    fn set_binop_items(&self, a: Val, b: Val, op: OpCode) -> Result<Vec<Val>, VmErr> {
        let (sa, sb) = match (self.clone_set_items(a), self.clone_set_items(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(cold_runtime("set_binop on non-set operands")),
        };
        Ok(match op {
            OpCode::BitOr => sa.iter().chain(sb.iter()).copied().collect(),
            OpCode::BitAnd => sa.iter().filter(|&&v| sb.contains(v, &self.heap)).copied().collect(),
            OpCode::BitXor => sa.iter().filter(|&&v| !sb.contains(v, &self.heap))
                .chain(sb.iter().filter(|&&v| !sa.contains(v, &self.heap))).copied().collect(),
            OpCode::Sub => sa.iter().filter(|&&v| !sb.contains(v, &self.heap)).copied().collect(),
            _ => return Err(cold_runtime("set_binop with unsupported opcode")),
        })
    }

    pub(crate) fn set_binop_and_push(&mut self, a: Val, b: Val, op: OpCode) -> Result<(), VmErr> {
        let items = self.set_binop_items(a, b, op)?;
        let frozen = matches!(self.heap.get(a), HeapObj::FrozenSet(_));
        let v = self.alloc_set_result(items, frozen)?;
        self.push(v); Ok(())
    }

    // Augmented set ops rewrite the left set in place (identity preserved), frozenset rebinds.
    pub(crate) fn set_iop_and_push(&mut self, a: Val, b: Val, op: OpCode) -> Result<(), VmErr> {
        if !matches!(self.heap.get(a), HeapObj::Set(_)) {
            return self.set_binop_and_push(a, b, op);
        }
        let items = self.set_binop_items(a, b, op)?;
        let s = ValSet::from_vals(&items, &self.heap);
        if let HeapObj::Set(rc) = self.heap.get(a) { *rc.borrow_mut() = s; }
        self.push(a); Ok(())
    }

    /* Set comparisons with subset/superset semantics over set/frozenset. */
    pub(crate) fn set_compare_and_push(&mut self, a: Val, b: Val, op: OpCode) -> Result<(), VmErr> {
        let (sa, sb) = match (self.clone_set_items(a), self.clone_set_items(b)) {
            (Some(x), Some(y)) => (x, y),
            _ => return Err(cold_runtime("set_compare on non-set operands")),
        };
        // Content-based so distinct-handle equal elements (tuples, long strings) compare correctly.
        let eq = eq_set(&sa, &sb, |a, b| eq_member(a, b, &self.heap));
        // Each member is looked up under its stored hash, so a subset test stays linear.
        let subset = |x: &ValSet, y: &ValSet| x.len() <= y.len() && x.iter().all(|&v| y.contains(v, &self.heap));
        let result = match op {
            OpCode::Eq => eq,
            OpCode::NotEq => !eq,
            OpCode::Lt => subset(&sa, &sb) && !eq,
            OpCode::LtEq => subset(&sa, &sb),
            OpCode::Gt => subset(&sb, &sa) && !eq,
            OpCode::GtEq => subset(&sb, &sa),
            _ => return Err(cold_runtime("set_compare with non-compare opcode")),
        };
        self.push(Val::bool(result));
        Ok(())
    }

    pub fn type_name(&self, v: Val) -> &str {
        if v.is_bool() { "bool" }
        else if v.is_int() { "int" }
        else if v.is_float() { "float" }
        else if v.is_none() { "NoneType" }
        else if v.is_foreign() { self.heap.foreign_kind(v).map_or("object", |k| k.type_name) }
        else { match self.heap.get(v) {
            &HeapObj::Boxed(n) => self.type_name(n),
            HeapObj::Str(_) => "str",
            HeapObj::Bytes(_) => "bytes",
            HeapObj::ByteArray(_) => "bytearray",
            HeapObj::MemoryView(_) => "memoryview",
            HeapObj::FnCode(_) => "code",
            HeapObj::List(_) => "list",
            HeapObj::Dict(_) => "dict",
            HeapObj::FrozenDict(_) => "frozendict",
            HeapObj::Set(_) => "set",
            HeapObj::FrozenSet(_) => "frozenset",
            HeapObj::Tuple(_) => "tuple",
            HeapObj::Func(..) => "function",
            HeapObj::Type(_) | HeapObj::Class(..) => "type",
            HeapObj::Range(..) | HeapObj::BigRange(..) => "range",
            HeapObj::Slice(..) => "slice",
            HeapObj::BoundMethod(..) | HeapObj::NativeFn(_) | HeapObj::Extern(_) => "builtin_function_or_method",
            HeapObj::BoundUserMethod(..) => "<bound method>",
            HeapObj::Super(..) => "super",
            HeapObj::Property(..) | HeapObj::PropertyPart(..) => "property",
            HeapObj::StaticMethod(..) => "staticmethod",
            HeapObj::ClassMethod(..) => "classmethod",
            HeapObj::Instance(cls, _) => {
                if cls.is_heap() && let HeapObj::Class(name, _, _) = self.heap.get(*cls) {
                    return name;
                }
                "object"
            }
            HeapObj::Coroutine(c) => match c.body {
                BodyRef::Fn(fi) if !self.is_async_fn(fi) => "generator",
                BodyRef::Fn(fi) if self.functions[fi].1.is_generator => "async_generator",
                BodyRef::Fn(fi) if self.is_anext_step(fi) => "async_generator_asend",
                _ => "coroutine",
            },
            HeapObj::Module(..) => "module",
            HeapObj::ExcInstance(..) => "exception",
            HeapObj::Ellipsis => "ellipsis",
            HeapObj::NotImplemented => "NotImplementedType",
            HeapObj::GenericAlias(..) => "GenericAlias",
            HeapObj::TypeAlias(..) => "TypeAliasType",
            HeapObj::Union(_) => "Union",
            // The stars a type parameter was declared with name its kind.
            HeapObj::TypeVar(n) if n.starts_with("**") => "ParamSpec",
            HeapObj::TypeVar(n) if n.starts_with('*') => "TypeVarTuple",
            HeapObj::TypeVar(_) => "TypeVar",
            HeapObj::Iter(_, name) => name,
            HeapObj::Cell(_) => "cell",
            HeapObj::Code(_) => "code",
            HeapObj::DictView(_, View::Keys) => "dict_keys",
            HeapObj::DictView(_, View::Values) => "dict_values",
            HeapObj::DictView(_, View::Items) => "dict_items",
        }}
    }

    /* `str` of exception `name` from its args, a KeyError key shows as its repr. */
    fn exc_text(&self, name: &str, args: &[Val], seen: &mut Vec<u32>) -> String {
        match args {
            [] => String::new(),
            // A group reads as its message and how many members it holds.
            [msg, excs] if super::globals::is_group_type(name) => {
                let n = match self.heap.try_get(*excs) { Some(HeapObj::List(l)) => l.borrow().len(), Some(HeapObj::Tuple(t)) => t.len(), _ => 0 };
                let tail = if n == 1 { " sub-exception)" } else { " sub-exceptions)" };
                s!(str &self.display_d(*msg, seen), " (", int n as i64, str tail)
            }
            [a] if name == "KeyError" => self.repr_d(*a, seen),
            [a] => self.display_d(*a, seen),
            [no, msg, rest @ ..] if rest.len() <= 1 && super::globals::matches_exc_class(name, "OSError") => {
                let mut o = s!("[Errno ", str &self.display_d(*no, seen), "] ", str &self.display_d(*msg, seen));
                if let [f] = rest { o.push_str(": "); o.push_str(&self.repr_d(*f, seen)); }
                o
            }
            _ => { let mut o = s!(cap: 32; "("); self.append_reprs(&mut o, args.iter(), seen); o.push(')'); o }
        }
    }

    /* The heap slot of an exception's `args` tuple, the key cycle checks use. */
    fn exc_args_slot(&self, inst: Val) -> Option<u32> {
        let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(inst) else { return None };
        let args = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == "args")).map(|(_, v)| v)?;
        matches!(self.heap.try_get(args), Some(HeapObj::Tuple(_))).then(|| args.as_heap())
    }

    /* The `args` an exception instance holds, empty for anything else. */
    pub(crate) fn exc_args(&self, inst: Val) -> Vec<Val> {
        let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(inst) else { return Vec::new() };
        let args = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == "args")).map(|(_, v)| v);
        match args.and_then(|a| self.heap.try_get(a)) { Some(HeapObj::Tuple(t)) => t.clone(), _ => Vec::new() }
    }

    /* Whether function `fi` is an `async def`. */
    #[inline]
    pub(crate) fn is_async_fn(&self, fi: usize) -> bool { self.is_async.get(fi).copied().unwrap_or(false) }

    /* Whether `v` is a coroutine or an async generator, which only `async for` and `await` step. */
    pub(crate) fn is_async_object(&self, v: Val) -> bool {
        matches!(self.heap.try_get(v), Some(HeapObj::Coroutine(c)) if !matches!(c.body, BodyRef::Fn(fi) if !self.is_async_fn(fi)))
    }

    /* TypeError for a binary operator no operand type supports. */
    #[cold]
    pub(crate) fn unsupported(&self, op: &str, a: Val, b: Val) -> VmErr {
        VmErr::TypeMsg(s!("unsupported operand type(s) for ", str op, ": '", str self.type_name(a), "' and '", str self.type_name(b), "'"))
    }

    /* `{a, b}` or `frozenset({a, b})` read in place, `...` for a cycle. */
    fn set_repr<'b>(&self, v: Val, frozen: bool, len: usize, items: impl Iterator<Item = &'b Val>, seen: &mut Vec<u32>) -> String {
        if len == 0 { return (if frozen { "frozenset()" } else { "set()" }).into(); }
        let mut out = String::from(if frozen { "frozenset({" } else { "{" });
        let id = v.as_heap();
        if seen.contains(&id) { out.push_str("..."); } else { seen.push(id); self.append_reprs(&mut out, items, seen); seen.pop(); }
        out.push_str(if frozen { "})" } else { "}" });
        out
    }

    fn append_reprs<'b>(&self, out: &mut String, it: impl Iterator<Item = &'b Val>, seen: &mut Vec<u32>) {
        let mut first = true;
        for v in it {
            // Bound breadth, a wide structure re-referencing one big child would render without limit.
            if out.len() > MAX_REPR_LEN { out.push_str(", ..."); break; }
            if !first { out.push_str(", "); }
            out.push_str(&self.repr_d(*v, seen));
            first = false;
        }
    }

    pub fn display(&self, v: Val) -> String { self.display_d(v, &mut Vec::new()) }

    /* The text of a FOREIGN value where no error can surface, its type when the kind refuses. */
    fn foreign_text(&self, v: Val) -> String {
        match self.heap.foreign_repr(v) { Some(Ok(s)) => s, _ => s!("<", str self.type_name(v), " object>") }
    }

    /* Cycle-aware display, `seen` tracks containers on the current path, so self-referential structures emit "..." instead of recursing forever (and its length bounds raw nesting depth). */
    fn display_d(&self, v: Val, seen: &mut Vec<u32>) -> String {
        if seen.len() > RENDER_DEPTH_MAX { return "...".into(); }
        if v.is_int() { let mut b = itoa::Buffer::new(); return b.format(v.as_int()).into(); }
        if v.is_float() {
            // Single source of truth for float text (Python repr rules, scientific + .0).
            return crate::util::fstr::format_f64(v.as_float());
        }
        if v.is_true() { return "True".into(); }
        if v.is_false() { return "False".into(); }
        if v.is_none() { return "None".into(); }
        if v.is_foreign() { return self.foreign_text(v); }
        match self.heap.get(v) {
            &HeapObj::Boxed(n) => self.display_d(n, seen),
            HeapObj::Str(s) => s.clone(),
            HeapObj::Bytes(b) => format_bytes(b),
            HeapObj::ByteArray(b) => s!("bytearray(", str &quote_bytes(b, true), ")"),
            HeapObj::MemoryView(m) => s!(str if m.released { "<released memory at 0x" } else { "<memory at 0x" }, str &alloc::format!("{:x}", v.0), ">"),
            &HeapObj::FnCode(fi) => {
                let name = self.function_names.get(fi).map_or("<lambda>", |n| if n.is_empty() { "<lambda>" } else { n });
                let body = self.functions.get(fi).map(|f| &f.1);
                s!("<code object ", str name, " at 0x", str &alloc::format!("{:x}", v.0), ", file \"", str body.map_or("", |b| b.path.as_str()), "\", line ", int body.map_or(0, |b| b.first_line) as i64, ">")
            }
            HeapObj::Type(name) => s!("<class '", str name, "'>"),
            HeapObj::Func(i, ..) => {
                // Empty name means lambda, same convention as the snapshot restore.
                let name = match self.function_names.get(*i) { Some(n) if !n.is_empty() => n.as_str(), _ => "<lambda>" };
                s!("<function ", str name, ">")
            }
            HeapObj::Slice(s,e,st) => s!("slice(", str &self.display_d(*s, seen), ", ", str &self.display_d(*e, seen), ", ", str &self.display_d(*st, seen), ")"),
            HeapObj::Range(s,e,st) => if *st == 1 { s!("range(", int *s, ", ", int *e, ")") } else { s!("range(", int *s, ", ", int *e, ", ", int *st, ")") },
            HeapObj::BigRange(..) => crate::frontend::int::range_repr(&self.heap, v).and_then(Result::ok).unwrap_or_else(|| "range(...)".into()),
            HeapObj::List(l) => { let id = v.as_heap(); if seen.contains(&id) { return "[...]".into(); } seen.push(id); let mut o = s!(cap: 32; "["); self.append_reprs(&mut o, l.borrow().iter(), seen); o.push(']'); seen.pop(); o },
            HeapObj::Tuple(t) => { let id = v.as_heap(); if seen.contains(&id) { return "(...)".into(); } seen.push(id); let o = if t.len() == 1 { s!("(", str &self.repr_d(t[0], seen), ",)") } else { let mut o = s!(cap: 32; "("); self.append_reprs(&mut o, t.iter(), seen); o.push(')'); o }; seen.pop(); o },
            HeapObj::Dict(d) => self.dict_repr(v, &d.borrow(), seen),
            HeapObj::FrozenDict(d) if d.borrow().is_empty() => "frozendict()".into(),
            HeapObj::FrozenDict(d) => s!("frozendict(", str &self.dict_repr(v, &d.borrow(), seen), ")"),
            HeapObj::BoundMethod(recv, id) if recv.is_undef() => s!("<method '", str id.name(), "' of '", str id.ty(), "' objects>"),
            HeapObj::BoundMethod(_, id) => s!("<built-in method ", str id.name(), ">"),
            HeapObj::NativeFn(id) => s!("<built-in function ", str id.name(), ">"),
            // User classes live in `__main__`, Python qualifies the repr with the module.
            HeapObj::Class(name, _, _) => crate::s!("<class '__main__.", str self.class_qualnames.get(&v.0).unwrap_or(name), "'>"),
            HeapObj::Instance(cls, _) => {
                // An exception reads as its message.
                if let Some(base) = self.exc_base(*cls) {
                    // An exception inside its own args reads as `E(...)` there, the way repr does.
                    let slot = self.exc_args_slot(v);
                    if slot.is_some_and(|s| seen.contains(&s)) { return s!(str self.type_name(v), "(...)"); }
                    seen.extend(slot);
                    let text = self.exc_text(base, &self.exc_args(v), seen);
                    if slot.is_some() { seen.pop(); }
                    return text;
                }
                if cls.is_heap() && let HeapObj::Class(name, _, _) | HeapObj::Type(name) = self.heap.get(*cls) { return crate::s!("<", str name, " instance>"); }
                "<instance>".into()
            }
            HeapObj::BoundUserMethod(..) => "<bound method>".into(),
            HeapObj::Super(..) => "<super object>".into(),
            HeapObj::Property(..) => "<property object>".into(),
            HeapObj::PropertyPart(_, slot) => crate::s!("<property.", str ["getter", "setter", "deleter"][*slot as usize], ">"),
            HeapObj::StaticMethod(..) => "<staticmethod object>".into(),
            HeapObj::ClassMethod(..) => "<classmethod object>".into(),
            HeapObj::Coroutine(c) if matches!(c.body, BodyRef::Fn(fi) if self.is_anext_step(fi)) => "<async_generator_asend object>".into(),
            HeapObj::Coroutine(c) => {
                let name = match c.body {
                    BodyRef::Fn(fi) => self.function_names.get(fi).map_or("<lambda>", |n| if n.is_empty() { "<lambda>" } else { n }),
                    BodyRef::Module => "<module>",
                };
                s!("<", str self.type_name(v), " object ", str name, ">")
            }
            HeapObj::Iter(_, name) => s!("<", str name, " object>"),
            HeapObj::DictView(d, kind) => {
                let mut o = s!(str self.type_name(v), "([");
                if let Some(m) = self.heap.dict_cell(*d) {
                    for (i, (k, val)) in m.borrow().iter().enumerate() {
                        if i > 0 { if o.len() > MAX_REPR_LEN { o.push_str(", ..."); break; } o.push_str(", "); }
                        match kind {
                            View::Keys => o.push_str(&self.repr_d(k, seen)),
                            View::Values => o.push_str(&self.repr_d(val, seen)),
                            View::Items => { o.push('('); o.push_str(&self.repr_d(k, seen)); o.push_str(", "); o.push_str(&self.repr_d(val, seen)); o.push(')'); }
                        }
                    }
                }
                o.push_str("])");
                o
            }
            HeapObj::Module(name, _) => s!("<module '", str name, "'>"),
            HeapObj::Extern(f) => s!("<extern function ", str &f.name, ">"),
            HeapObj::GenericAlias(origin, args) => {
                let mut o = self.alias_arg(*origin, seen);
                o.push('[');
                match self.heap.get(*args) {
                    HeapObj::Tuple(t) if !t.is_empty() => for (i, &a) in t.iter().enumerate() {
                        if i > 0 { o.push_str(", "); }
                        o.push_str(&self.alias_arg(a, seen));
                    },
                    _ => o.push_str("()"),
                }
                o.push(']');
                o
            }
            HeapObj::TypeAlias(name, _) => name.clone(),
            HeapObj::TypeVar(name) => name.trim_start_matches('*').into(),
            HeapObj::Union(args) => {
                let mut o = String::new();
                if let HeapObj::Tuple(t) = self.heap.get(*args) {
                    for (i, &m) in t.iter().enumerate() {
                        if i > 0 { o.push_str(" | "); }
                        // A union stores `None` as its type and spells it back as `None`.
                        let none = m.is_heap() && matches!(self.heap.get(m), HeapObj::Type(n) if n == "NoneType");
                        o.push_str(&if none { "None".into() } else { self.alias_arg(m, seen) });
                    }
                }
                o
            }
            HeapObj::ExcInstance(name, args, _) => self.exc_text(name, args, seen),
            HeapObj::Set(s) => { let s = s.borrow(); self.set_repr(v, false, s.len(), s.iter(), seen) }
            HeapObj::FrozenSet(s) => self.set_repr(v, true, s.len(), s.iter(), seen),
            HeapObj::Ellipsis => "Ellipsis".into(),
            HeapObj::NotImplemented => "NotImplemented".into(),
            HeapObj::Cell(_) => "<cell>".into(),
            HeapObj::Code(c) => s!("<code object <module>, file \"", str &c.filename, "\">"),
        }
    }

    /* `{k: v, ...}` of the entries of dict `v`, `{...}` where it holds itself. */
    fn dict_repr(&self, v: Val, d: &DictMap, seen: &mut Vec<u32>) -> String {
        let id = v.as_heap();
        if seen.contains(&id) { return "{...}".into(); }
        seen.push(id);
        let mut o = s!(cap: 32; "{");
        for (i, (k, val)) in d.iter().enumerate() {
            if i > 0 {
                if o.len() > MAX_REPR_LEN { o.push_str(", ..."); break; }
                o.push_str(", ");
            }
            o.push_str(&self.repr_d(k, seen));
            o.push_str(": ");
            o.push_str(&self.repr_d(val, seen));
        }
        o.push('}');
        seen.pop();
        o
    }

    pub fn repr(&self, v: Val) -> String { self.repr_d(v, &mut Vec::new()) }

    /* An alias argument as Python spells it, a type by its name and anything else by repr. */
    fn alias_arg(&self, v: Val, seen: &mut Vec<u32>) -> String {
        if v.is_heap() {
            match self.heap.get(v) {
                HeapObj::Type(n) => return n.clone(),
                HeapObj::TypeVar(n) => return n.trim_start_matches('*').into(),
                HeapObj::TypeAlias(q, _) => return s!("__main__.", str q),
                HeapObj::Class(n, ..) => return s!("__main__.", str self.class_qualnames.get(&v.0).unwrap_or(n)),
                HeapObj::Ellipsis => return "...".into(),
                _ => {}
            }
        }
        self.repr_d(v, seen)
    }

    pub(crate) fn repr_d(&self, v: Val, seen: &mut Vec<u32>) -> String {
        if v.is_heap() {
            match self.heap.get(v) {
                HeapObj::Str(s) => return repr_str(s),
                // `repr(E("x"))` is the constructor call, `ValueError('x')`.
                HeapObj::ExcInstance(name, args, _) => {
                    let mut o = s!(cap: 32; str name, "(");
                    self.append_reprs(&mut o, args.iter(), seen);
                    o.push(')');
                    return o;
                }
                // An exception instance reads as its constructor call, `E('x')`.
                &HeapObj::Instance(cls, _) if self.exc_base(cls).is_some() => {
                    let slot = self.exc_args_slot(v);
                    if slot.is_some_and(|s| seen.contains(&s)) { return s!(str self.type_name(v), "(...)"); }
                    seen.extend(slot);
                    let mut o = s!(cap: 32; str self.type_name(v), "(");
                    self.append_reprs(&mut o, self.exc_args(v).iter(), seen);
                    o.push(')');
                    if slot.is_some() { seen.pop(); }
                    return o;
                }
                _ => {}
            }
        }
        self.display_d(v, seen)
    }

    pub fn lt_vals(&self, a: Val, b: Val) -> Result<bool, VmErr> {
        self.lt_vals_d(a, b, 0)
    }

    /* Depth-tracked `<`, past the cap surface RecursionError instead of overflowing the stack. */
    fn lt_vals_d(&self, a: Val, b: Val, depth: usize) -> Result<bool, VmErr> {
        if depth > CMP_DEPTH_MAX { return Err(cold_depth()); }
        let a = if a.is_bool() { Val::int(a.as_bool() as i64) } else { a };
        let b = if b.is_bool() { Val::int(b.as_bool() as i64) } else { b };
        if a.is_int() && b.is_int() { return Ok(a.as_int() < b.as_int()); }
        if (a.is_foreign() || b.is_foreign()) && let Some(o) = self.heap.foreign_cmp(a, b) { return Ok(o == Ordering::Less); }
        if let Some(Ok((af, bf))) = coerce_floats(a, b, &self.heap) { return Ok(af < bf); }
        if a.is_heap() && b.is_heap() {
            match (self.heap.get(a), self.heap.get(b)) {
                (HeapObj::Str(x), HeapObj::Str(y)) => return Ok(x < y),
                (HeapObj::Bytes(x) | HeapObj::ByteArray(x), HeapObj::Bytes(y) | HeapObj::ByteArray(y)) => return Ok(x < y),
                // Sequences compare lexicographically, clone to drop the heap borrow before recursing.
                (HeapObj::List(x), HeapObj::List(y)) => {
                    let (x, y) = (x.borrow().clone(), y.borrow().clone());
                    return self.seq_lt_d(&x, &y, depth + 1);
                }
                (HeapObj::Tuple(x), HeapObj::Tuple(y)) => {
                    let (x, y) = (x.clone(), y.clone());
                    return self.seq_lt_d(&x, &y, depth + 1);
                }
                _ => {}
            }
        }
        // A number of a class over int or float orders as the number.
        let (x, y) = (unboxed(a, &self.heap), unboxed(b, &self.heap));
        if x.0 != a.0 || y.0 != b.0 { return self.lt_vals_d(x, y, depth); }
        Err(VmErr::TypeMsg(s!("'<' not supported between instances of '", str self.type_name(a), "' and '", str self.type_name(b), "'")))
    }

    /* Lexicographic `<` for sequences, first differing element decides, otherwise the shorter is less. Recurses through `lt_vals`, so nested sequences and mixed element types are handled (and rejected) consistently. */
    fn seq_lt_d(&self, xs: &[Val], ys: &[Val], depth: usize) -> Result<bool, VmErr> {
        if depth > CMP_DEPTH_MAX { return Err(cold_depth()); }
        for (&x, &y) in xs.iter().zip(ys.iter()) {
            if eq_member(x, y, &self.heap) { continue; }
            return self.lt_vals_d(x, y, depth + 1);
        }
        Ok(xs.len() < ys.len())
    }

    /* Item presence in list/tuple/dict/set, or substring in string. Non-iterable container raises TypeError. */
    pub fn contains(&mut self, container: Val, item: Val) -> Result<bool, VmErr> {
        if container.is_heap() && matches!(self.heap.get(container), HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_)) { self.keyed(item)?; }
        if container.is_heap() {
            match self.heap.get(container) {
                // A scan counts what it may read as native work.
                HeapObj::List(v) => { self.heap.spend(v.borrow().len()); return Ok(v.borrow().iter().any(|x| eq_member(*x, item, &self.heap))) }
                HeapObj::Tuple(v) => { self.heap.spend(v.len()); return Ok(v.iter().any(|x| eq_member(*x, item, &self.heap))) }
                // Only a miss checks the item, an unhashable one is never inside so a hit needs no check.
                HeapObj::Dict(p) | HeapObj::FrozenDict(p) => { let hit = p.borrow().contains_key(&item, &self.heap); if !hit { self.require_key(item)?; } return Ok(hit); }
                HeapObj::Set(s) => { let hit = s.borrow().contains(item, &self.heap); if !hit { self.require_set_probe(item)?; } return Ok(hit); }
                HeapObj::FrozenSet(s) => { let hit = s.contains(item, &self.heap); if !hit { self.require_set_probe(item)?; } return Ok(hit); }
                HeapObj::Str(s) => {
                    if let Some(HeapObj::Str(sub)) = self.heap.try_get(item) { self.heap.spend(s.len()); return Ok(s.contains(sub.as_str())); }
                    return Err(VmErr::TypeMsg(s!("'in <string>' requires string as left operand, not ", str self.type_name(item))));
                }
                HeapObj::Bytes(b) | HeapObj::ByteArray(b) => {
                    if let Some(sub) = self.heap.bytes_of(item) {
                        self.heap.spend(b.len());
                        return Ok(sub.is_empty() || b.windows(sub.len()).any(|w| w == sub));
                    }
                    let n = match item { i if i.is_int() => i.as_int(), i if i.is_bool() => i.as_bool() as i64, _ => return Err(cold_type("a bytes-like object is required")) };
                    if !(0..=255).contains(&n) { return Err(cold_value("byte must be in range(0, 256)")); }
                    return Ok(b.contains(&(n as u8)));
                }
                HeapObj::BigRange(..) => return Ok(crate::frontend::int::range_has(&self.heap, container, item).unwrap_or(false)),
                HeapObj::Range(s, e, st) => {
                    let (s, e, st) = (*s as i128, *e as i128, *st as i128);
                    // O(1) range membership via bounds + step, integral floats match too.
                    let x = match as_i128(item, &self.heap) {
                        Some(i) => Some(i),
                        None if item.is_float() => {
                            let v = item.as_float();
                            if v.is_finite() && v == libm::trunc(v) { Some(v as i128) } else { None }
                        }
                        None => None,
                    };
                    return Ok(match x {
                        Some(x) => {
                            let in_bounds = if st > 0 { x >= s && x < e } else { x <= s && x > e };
                            in_bounds && st != 0 && (x - s) % st == 0
                        }
                        None => false,
                    });
                }
                _ => {}
            }
        }
        Err(VmErr::TypeMsg(s!("argument of type '", str self.type_name(container), "' is not a container or iterable")))
    }
    pub fn add_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        // Inline-int fast path, overflow falls through to the i128 slow path.
        if a.is_int() && b.is_int()
            && let Some(r) = a.as_int().checked_add(b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, &self.heap) { let (af, bf) = p?; return Ok(Val::float(af + bf)); }
        if let (Some(ai), Some(bi)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && let Some(r) = ai.checked_add(bi) { return self.heap.int(r); }
        if let Some(r) = self.wide_op(BinOp::Add, a, b)? { return Ok(r); }
        if a.is_heap() && b.is_heap() {
            // Charge the copy cost so growing concatenation in a loop stays bounded (avoids O(n^2)).
            let copy_cost = match (self.heap.get(a), self.heap.get(b)) {
                (HeapObj::Str(sa), HeapObj::Str(sb)) => Some(sa.len() + sb.len()),
                (HeapObj::Bytes(ba) | HeapObj::ByteArray(ba), HeapObj::Bytes(bb) | HeapObj::ByteArray(bb)) => Some(ba.len() + bb.len()),
                (HeapObj::List(va), HeapObj::List(vb)) => Some(va.borrow().len() + vb.borrow().len()),
                (HeapObj::Tuple(va), HeapObj::Tuple(vb)) => Some(va.len() + vb.len()),
                _ => None,
            };
            if let Some(n) = copy_cost { self.charge_steps(n)?; }
            match (self.heap.get(a), self.heap.get(b)) {
                (HeapObj::Str(sa), HeapObj::Str(sb)) => {
                    let mut r = String::with_capacity(sa.len() + sb.len());
                    r.push_str(sa); r.push_str(sb);
                    return self.heap.alloc(HeapObj::Str(r));
                }
                // Each result is sized once, so no operand is copied twice.
                (HeapObj::List(va), HeapObj::List(vb)) => {
                    let lst = [&va.borrow()[..], &vb.borrow()[..]].concat();
                    return self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(lst))));
                }
                (HeapObj::Tuple(va), HeapObj::Tuple(vb)) => {
                    let tup = [&va[..], &vb[..]].concat();
                    return self.heap.alloc(HeapObj::Tuple(tup));
                }
                // The result takes the type of the left side.
                (left @ (HeapObj::Bytes(ba) | HeapObj::ByteArray(ba)), HeapObj::Bytes(bb) | HeapObj::ByteArray(bb)) => {
                    let r = [&ba[..], &bb[..]].concat();
                    return self.heap.alloc(if matches!(left, HeapObj::ByteArray(_)) { HeapObj::ByteArray(r) } else { HeapObj::Bytes(r) });
                }
                _ => {}
            }
        }
        Err(self.concat_err(a, b).unwrap_or_else(|| self.unsupported("+", a, b)))
    }

    /* The error a sequence on the left of `+` raises for a right side of another type. */
    #[cold]
    fn concat_err(&self, a: Val, b: Val) -> Option<VmErr> {
        let (left, right) = (self.type_name(a), self.type_name(b));
        match self.heap.try_get(a)? {
            HeapObj::Str(_) | HeapObj::List(_) | HeapObj::Tuple(_) => Some(VmErr::TypeMsg(s!("can only concatenate ", str left, " (not \"", str right, "\") to ", str left))),
            HeapObj::Bytes(_) | HeapObj::ByteArray(_) => Some(VmErr::TypeMsg(s!("can't concat ", str right, " to ", str left))),
            _ => None,
        }
    }

    pub fn sub_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        if a.is_int() && b.is_int()
            && let Some(r) = a.as_int().checked_sub(b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, &self.heap) { let (af, bf) = p?; return Ok(Val::float(af - bf)); }
        if let (Some(ai), Some(bi)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && let Some(r) = ai.checked_sub(bi) { return self.heap.int(r); }
        if let Some(r) = self.wide_op(BinOp::Sub, a, b)? { return Ok(r); }
        // Set / frozenset difference makes a fresh set of `a` elements not in `b`.
        if let (Some(sa), Some(sb)) = (self.clone_set_items(a), self.clone_set_items(b)) {
            let items: Vec<Val> = sa.iter().filter(|&&v| !sb.contains(v, &self.heap)).copied().collect();
            let frozen = matches!(self.heap.get(a), HeapObj::FrozenSet(_));
            return self.alloc_set_result(items, frozen);
        }
        Err(self.unsupported("-", a, b))
    }

    pub fn mul_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        if a.is_int() && b.is_int()
            && let Some(r) = super::registers::mul_exact(a.as_int(), b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, &self.heap) { let (af, bf) = p?; return Ok(Val::float(af * bf)); }
        // Numeric multiply wins over sequence repetition when either side is a boxed int.
        if let (Some(ai), Some(bi)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && let Some(r) = ai.checked_mul(bi) { return self.heap.int(r); }
        if let Some(r) = self.wide_op(BinOp::Mul, a, b)? { return Ok(r); }
        // Sequence repetition, str/bytes/list/tuple * int, bool counts as 0/1 (count clamped to i64).
        let rep_count = |v: Val| if v.is_int() { Some(v.as_int()) } else if v.is_bool() { Some(v.as_bool() as i64) } else { None };
        let (seq_val, count) = if a.is_heap() && let Some(n) = rep_count(b) {
            (a, n)
        } else if b.is_heap() && let Some(n) = rep_count(a) {
            (b, n)
        } else {
            let seq = |v: Val| matches!(self.heap.try_get(v), Some(HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_) | HeapObj::List(_) | HeapObj::Tuple(_)));
            if (seq(a) && b.is_foreign()) || (seq(b) && a.is_foreign()) { return Err(VmErr::Raised(String::from("OverflowError: cannot fit 'int' into an index-sized integer"))); }
            let other = if seq(a) { b } else if seq(b) { a } else { return Err(self.unsupported("*", a, b)) };
            return Err(VmErr::TypeMsg(s!("can't multiply sequence by non-int of type '", str self.type_name(other), "'")));
        };
        let n = count.max(0) as usize;
        // Charge the fill up front so repeated `[x]*n` is bounded by the op budget, not just heap.
        let fill_cost = match self.heap.get(seq_val) {
            HeapObj::Str(s) => s.len().checked_mul(n),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b.len().checked_mul(n),
            HeapObj::List(rc) => rc.borrow().len().checked_mul(n),
            HeapObj::Tuple(v) => v.len().checked_mul(n),
            _ => None,
        };
        if let Some(c) = fill_cost && c.saturating_mul(VAL_BYTES) <= self.heap.room() { self.charge_steps(c)?; }
        match self.heap.get(seq_val) {
            HeapObj::Str(s) => {
                let bytes = s.len().checked_mul(n).ok_or_else(cold_overflow)?;
                self.heap.reserve(bytes)?;
                let r = s.repeat(n);
                return self.heap.alloc(HeapObj::Str(r));
            }
            HeapObj::List(rc) => {
                let src = rc.borrow().clone();
                let out = self.repeat_seq(&src, n)?;
                return self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(out))));
            }
            HeapObj::Tuple(v) => {
                let src = v.clone();
                let out = self.repeat_seq(&src, n)?;
                return self.heap.alloc(HeapObj::Tuple(out));
            }
            seq @ (HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => {
                let bytes = b.len().checked_mul(n).ok_or_else(cold_overflow)?;
                self.heap.reserve(bytes)?;
                let r = b.repeat(n);
                return self.heap.alloc(if matches!(seq, HeapObj::ByteArray(_)) { HeapObj::ByteArray(r) } else { HeapObj::Bytes(r) });
            }
            _ => {}
        }
        Err(self.unsupported("*", a, b))
    }

    /* Repeat a sequence `n` times with the same budget/overflow/heap-limit guards as `seq * n`. */
    fn repeat_seq(&self, src: &[Val], n: usize) -> Result<Vec<Val>, VmErr> {
        // Empty source means result is empty for any n, so skip the n-iteration loop.
        if src.is_empty() { return Ok(Vec::new()); }
        let cap = src.len().checked_mul(n).ok_or_else(cold_overflow)?;
        self.heap.reserve(cap.saturating_mul(VAL_BYTES))?;
        let mut out = Vec::with_capacity(cap);
        for _ in 0..n { out.extend_from_slice(src); }
        Ok(out)
    }

    pub fn div_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        // Boxed ints that fit i128 divide without limbs, rounded the same way.
        if (a.is_foreign() || b.is_foreign()) && let (Some(x), Some(y)) = (as_i128(a, &self.heap), as_i128(b, &self.heap)) && y != 0 {
            return Ok(Val::float(crate::frontend::bigint::BigInt::div_i128(x, y)));
        }
        if let Some(r) = self.wide_op(BinOp::TrueDiv, a, b)? { return Ok(r); }
        let (Some(av), Some(bv)) = (num_f64(a, &self.heap), num_f64(b, &self.heap)) else { return Err(self.unsupported("/", a, b)) };
        let (av, bv) = (av?, bv?);
        if bv == 0.0 { return Err(VmErr::ZeroDiv); }
        Ok(Val::float(av / bv))
    }

    /* `op` through the kind of a FOREIGN operand, None when nothing takes it. */
    pub(crate) fn wide_op(&mut self, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        if !a.is_foreign() && !b.is_foreign() { return Ok(None); }
        self.heap.foreign_binop(op, a, b)
    }

    /* Method wrapper around the free `as_i128` for borrow-checker ergonomics. */
    #[inline]
    pub(crate) fn as_i128(&self, v: Val) -> Option<i128> {
        as_i128(v, &self.heap)
    }

    pub(crate) fn to_f64_coerce(&self, v: Val) -> Result<f64, VmErr> {
        num_as_f64(v, &self.heap).ok_or_else(|| cold_type("numeric operand required"))
    }

    /* Wrap an i128 into the narrowest Val, None->Overflow, 48-bit->inline, else boxed. */
    #[inline]
    pub(crate) fn int_to_val(&mut self, r: Option<i128>) -> Result<Val, VmErr> {
        self.heap.int(r.ok_or_else(cold_overflow)?)
    }
}
