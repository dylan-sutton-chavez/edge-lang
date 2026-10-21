use alloc::{string::{String, ToString}, vec, vec::Vec};
use crate::s;
use core::cmp::Ordering;
use super::*;
use super::render::{MAX_REPR_LEN, RENDER_DEPTH_MAX, at, format_bytes, quote_bytes, repr_str};

// What a builtin body reads past the heap so one body serves two hosts.
pub trait Host {
    fn heap(&self) -> &HeapPool;
    fn heap_mut(&mut self) -> &mut HeapPool;
    // Hands back the result of a call.
    fn push(&mut self, v: Val);
    // Pays native work against the op budget.
    fn charge(&mut self, n: usize) -> Result<(), VmErr>;
    // Items only the VM can step and none for a builtin container.
    fn vm_items(&mut self, _v: Val) -> Result<Option<Vec<Val>>, VmErr> { Ok(None) }
    fn type_name(&self, v: Val) -> &str { self.heap().type_name(v) }
    // Keeps `v` alive until unroot lets it go.
    fn root(&mut self, _v: Val) {}
    fn unroot(&mut self, _n: usize) {}
    // Runs `f` with `vals` kept alive.
    fn with_roots<R>(&mut self, vals: impl IntoIterator<Item = Val>, f: impl FnOnce(&mut Self) -> R) -> R where Self: Sized {
        let mut n = 0;
        for v in vals { self.root(v); n += 1; }
        let r = f(self);
        self.unroot(n);
        r
    }

    // Text of functions, classes, instances and coroutines only the VM makes.
    fn display_other(&self, v: Val, _seen: &mut Vec<u32>) -> String { s!("<", str self.type_name(v), " object", str &at(v), ">") }
    // A class with __eq__ and no __hash__ makes unhashable instances.
    fn eq_without_hash(&self, _cls: Val) -> bool { false }
    // Precomputes a user __hash__ which only the VM runs.
    fn keyed(&mut self, _v: Val) -> Result<(), VmErr> { Ok(()) }
    // KeyError whose text is the repr of the missing key.
    fn key_error(&mut self, key: Val) -> VmErr { VmErr::Raised(s!("KeyError: ", str &self.repr(key))) }
    // Repr of an exception instance which only the VM makes.
    fn repr_other(&self, _v: Val, _seen: &mut Vec<u32>) -> Option<String> { None }

    #[inline] fn truthy(&self, v: Val) -> bool { self.heap().truthy(v) }
    #[inline] fn as_i128(&self, v: Val) -> Option<i128> { super::as_i128(v, self.heap()) }
    // None for anything but a str.
    #[inline] fn text(&self, v: Val) -> Option<&str> { match self.heap().try_get(v) { Some(HeapObj::Str(s)) => Some(s), _ => None } }
    #[inline] fn alloc_and_push_str(&mut self, s: String) -> Result<(), VmErr> { let v = self.heap_mut().alloc(HeapObj::Str(s))?; self.push(v); Ok(()) }
    #[inline] fn alloc_list(&mut self, items: Vec<Val>) -> Result<Val, VmErr> { self.heap_mut().alloc(HeapObj::List(alloc::rc::Rc::new(core::cell::RefCell::new(items)))) }
    #[inline] fn alloc_and_push_list(&mut self, items: Vec<Val>) -> Result<(), VmErr> { let v = self.alloc_list(items)?; self.push(v); Ok(()) }
    #[inline] fn alloc_and_push_dict(&mut self, d: super::DictMap) -> Result<(), VmErr> { let v = self.heap_mut().alloc(HeapObj::Dict(alloc::rc::Rc::new(core::cell::RefCell::new(d))))?; self.push(v); Ok(()) }

    // TypeError for a non-iterable operand.
    fn not_iterable(&self, o: Val) -> VmErr {
        VmErr::TypeMsg(s!("'", str self.type_name(o), "' object is not iterable"))
    }

    /* What a dict view shows now, an items view pairing each key with its value. */
    fn view_items(&mut self, d: Val, kind: View) -> Result<Vec<Val>, VmErr> {
        let entries: Vec<(Val, Val)> = self.heap().dict_cell(d).map_or_else(Vec::new, |m| m.borrow().iter().collect());
        match kind {
            View::Keys => Ok(entries.into_iter().map(|(k, _)| k).collect()),
            View::Values => Ok(entries.into_iter().map(|(_, v)| v).collect()),
            View::Items => entries.into_iter().map(|(k, v)| self.heap_mut().alloc(HeapObj::Tuple(vec![k, v]))).collect(),
        }
    }

    fn str_to_char_vals(&mut self, s: &str) -> Result<Vec<Val>, VmErr> {
        // Per-char heap allocs scale with input, charge the budget so loops over this stay bounded.
        self.charge(s.len())?;
        s.chars().map(|c| self.heap_mut().alloc(HeapObj::Str(c.to_string()))).collect()
    }

    // Items charged as native work and refused as iter refuses them.
    fn extract_iter(&mut self, o: Val) -> Result<Vec<Val>, VmErr> {
        if !o.is_heap() {
            return Err(self.not_iterable(o));
        }
        if let Some(items) = self.vm_items(o)? { return Ok(items); }
        if let HeapObj::DictView(d, kind) = *self.heap().get(o) {
            let items = self.view_items(d, kind)?;
            self.charge(items.len())?;
            return Ok(items);
        }
        // A wide range steps its cursor, the count it would hold checked against the memory left.
        if let Some(&HeapObj::BigRange(cur, end, step)) = self.heap().try_get(o) {
            let n = crate::frontend::int::range_len(self.heap(), o).unwrap_or(Ok(0))?;
            if (n as u128).saturating_mul(VAL_BYTES as u128) > self.heap().room() as u128 { return Err(cold_heap()); }
            let (mut walk, mut out) = (IterFrame::BigRange { cur, end, step }, Vec::new());
            while let Some(x) = walk.next_item(self.heap_mut())? { out.push(x); }
            self.charge(out.len())?;
            return Ok(out);
        }
        // Snapshot the variant out so the &self borrow ends before any allocation.
        let snapshot = match self.heap().get(o) {
            HeapObj::List(v) => Some(v.borrow().clone()),
            HeapObj::Tuple(v) => Some(v.clone()),
            HeapObj::Set(v) => Some(v.borrow().iter().cloned().collect()),
            HeapObj::FrozenSet(v) => Some(v.iter().cloned().collect()),
            HeapObj::Range(s, e, st) => {
                let (mut cur, end, step) = (*s, *e, *st);
                // Materialised length is user-controlled, cap it against the memory left.
                let span = (end as i128 - cur as i128).unsigned_abs();
                let count = if step == 0 { 0 } else { span / (step as i128).unsigned_abs() };
                if count.saturating_mul(VAL_BYTES as u128) > self.heap().room() as u128 { return Err(cold_heap()); }
                let mut out = Vec::new();
                if step > 0 {
                    while cur < end {
                        out.push(self.heap_mut().int(cur as i128)?);
                        match cur.checked_add(step) { Some(n) => cur = n, None => break }
                    }
                } else {
                    while cur > end {
                        out.push(self.heap_mut().int(cur as i128)?);
                        match cur.checked_add(step) { Some(n) => cur = n, None => break }
                    }
                }
                Some(out)
            }
            HeapObj::Dict(d) | HeapObj::FrozenDict(d) => Some(d.borrow().keys().collect()),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => Some(b.iter().map(|&x| Val::int(x as i64)).collect()),
            HeapObj::MemoryView(_) => Some(self.heap().octets(o).unwrap_or_default().iter().map(|&x| Val::int(x as i64)).collect()),
            HeapObj::Str(_) => None, // handled below, needs heap allocation
            _ => return Err(self.not_iterable(o)),
        };
        if let Some(v) = snapshot {
            // Cost scales with element count, charge it so repeated materialisation stays bounded.
            self.charge(v.len())?;
            return Ok(v);
        }
        // Str path materialises one-char heap strings via the existing helper.
        if let HeapObj::Str(s) = self.heap().get(o) {
            let s = s.clone();
            return self.str_to_char_vals(&s);
        }
        unreachable!()
    }

    /* `str` of exception `name` from its args, a KeyError key shows as its repr. */
    fn exc_text(&self, name: &str, args: &[Val], seen: &mut Vec<u32>) -> String {
        match args {
            [] => String::new(),
            // A group reads as its message and how many members it holds.
            [msg, excs] if crate::vm::globals::is_group_type(name) => {
                let n = match self.heap().try_get(*excs) { Some(HeapObj::List(l)) => l.borrow().len(), Some(HeapObj::Tuple(t)) => t.len(), _ => 0 };
                let tail = if n == 1 { " sub-exception)" } else { " sub-exceptions)" };
                s!(str &self.display_d(*msg, seen), " (", int n as i64, str tail)
            }
            [a] if name == "KeyError" => self.repr_d(*a, seen),
            [a] => self.display_d(*a, seen),
            [no, msg, rest @ ..] if rest.len() <= 1 && crate::vm::globals::matches_exc_class(name, "OSError") => {
                let mut o = s!("[Errno ", str &self.display_d(*no, seen), "] ", str &self.display_d(*msg, seen));
                if let [f] = rest { o.push_str(": "); o.push_str(&self.repr_d(*f, seen)); }
                o
            }
            _ => { let mut o = s!(cap: 32; "("); self.append_reprs(&mut o, args.iter(), seen); o.push(')'); o }
        }
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

    fn display(&self, v: Val) -> String { self.display_d(v, &mut Vec::new()) }

    /* The text of a FOREIGN value where no error can surface, its type when the kind refuses. */
    fn foreign_text(&self, v: Val) -> String {
        match self.heap().foreign_repr(v) { Some(Ok(s)) => s, _ => s!("<", str self.type_name(v), " object", str &at(v), ">") }
    }

    /* Whether builtin method `id` over `recv` is unbound, read off its type, an operator dunder keeping that type as `recv`. */
    #[inline]
    fn method_unbound(&self, recv: Val, id: crate::vm::methods::BuiltinMethodId) -> bool {
        recv.is_undef() || id.ty() == "op" && matches!(self.heap().try_get(recv), Some(HeapObj::Type(_)))
    }

    /* The type an unbound builtin method belongs to. */
    fn method_owner(&self, recv: Val, id: crate::vm::methods::BuiltinMethodId) -> &str {
        match self.heap().try_get(recv) { Some(HeapObj::Type(n)) if !recv.is_undef() => n, _ => id.ty() }
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
        match self.heap().get(v) {
            &HeapObj::Boxed(n) => self.display_d(n, seen),
            HeapObj::Str(s) => s.clone(),
            HeapObj::Bytes(b) => format_bytes(b),
            HeapObj::ByteArray(b) => s!("bytearray(", str &quote_bytes(b, true), ")"),
            HeapObj::MemoryView(m) => s!(str if m.released { "<released memory" } else { "<memory" }, str &at(v), ">"),
            HeapObj::Type(name) => s!("<class '", str name, "'>"),
            HeapObj::Slice(s,e,st) => s!("slice(", str &self.display_d(*s, seen), ", ", str &self.display_d(*e, seen), ", ", str &self.display_d(*st, seen), ")"),
            HeapObj::Range(s,e,st) => if *st == 1 { s!("range(", int *s, ", ", int *e, ")") } else { s!("range(", int *s, ", ", int *e, ", ", int *st, ")") },
            HeapObj::BigRange(..) => crate::frontend::int::range_repr(self.heap(), v).and_then(Result::ok).unwrap_or_else(|| "range(...)".into()),
            HeapObj::List(l) => { let id = v.as_heap(); if seen.contains(&id) { return "[...]".into(); } seen.push(id); let mut o = s!(cap: 32; "["); self.append_reprs(&mut o, l.borrow().iter(), seen); o.push(']'); seen.pop(); o },
            HeapObj::Tuple(t) => { let id = v.as_heap(); if seen.contains(&id) { return "(...)".into(); } seen.push(id); let o = if t.len() == 1 { s!("(", str &self.repr_d(t[0], seen), ",)") } else { let mut o = s!(cap: 32; "("); self.append_reprs(&mut o, t.iter(), seen); o.push(')'); o }; seen.pop(); o },
            HeapObj::Dict(d) => self.dict_repr(v, &d.borrow(), seen),
            HeapObj::FrozenDict(d) if d.borrow().is_empty() => "frozendict()".into(),
            HeapObj::FrozenDict(d) => s!("frozendict(", str &self.dict_repr(v, &d.borrow(), seen), ")"),
            // `object` spells its slot wrappers without the type.
            &HeapObj::BoundMethod(recv, id) if self.method_unbound(recv, id) && is_dunder(id.name()) && id.ty() == "object" => s!("<slot wrapper '", str id.name(), "'>"),
            &HeapObj::BoundMethod(recv, id) if self.method_unbound(recv, id) && is_dunder(id.name()) => s!("<slot wrapper '", str id.name(), "' of '", str self.method_owner(recv, id), "' objects>"),
            &HeapObj::BoundMethod(recv, id) if self.method_unbound(recv, id) => s!("<method '", str id.name(), "' of '", str self.method_owner(recv, id), "' objects>"),
            &HeapObj::BoundMethod(recv, id) if is_dunder(id.name()) => s!("<method-wrapper '", str id.name(), "' of ", str self.type_name(recv), " object", str &at(recv), ">"),
            &HeapObj::BoundMethod(recv, id) => s!("<built-in method ", str id.name(), " of ", str self.type_name(recv), " object", str &at(recv), ">"),
            HeapObj::NativeFn(id) => s!("<built-in function ", str id.name(), ">"),
            &HeapObj::Super(cls, recv) => {
                let short = |c: Val| match self.heap().try_get(c) { Some(HeapObj::Class(n, ..) | HeapObj::Type(n)) => n.as_str(), _ => "?" };
                // A class bound reads as an object of that class, as an instance reads by its type.
                if recv.is_none() { return s!("<super: <class '", str short(cls), "'>, NULL>"); }
                let of = if matches!(self.heap().try_get(recv), Some(HeapObj::Class(..))) { short(recv) } else { self.type_name(recv) };
                s!("<super: <class '", str short(cls), "'>, <", str of, " object>>")
            }
            HeapObj::Property(..) => s!("<property object", str &at(v), ">"),
            HeapObj::PropertyPart(_, slot) => crate::s!("<property.", str ["getter", "setter", "deleter"][*slot as usize], ">"),
            &HeapObj::StaticMethod(f) => s!("<staticmethod(", str &self.repr_d(f, seen), ")>"),
            &HeapObj::ClassMethod(f) => s!("<classmethod(", str &self.repr_d(f, seen), ")>"),
            HeapObj::Iter(_, name) => s!("<", str name, " object", str &at(v), ">"),
            HeapObj::DictView(d, kind) => {
                let mut o = s!(str self.type_name(v), "([");
                if let Some(m) = self.heap().dict_cell(*d) {
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
            HeapObj::TypeAlias(name, _) => name.clone(),
            HeapObj::TypeVar(name) => name.trim_start_matches('*').into(),
            HeapObj::ExcInstance(name, args, _) => self.exc_text(name, args, seen),
            HeapObj::Set(s) => { let s = s.borrow(); self.set_repr(v, false, s.len(), s.iter(), seen) }
            HeapObj::FrozenSet(s) => self.set_repr(v, true, s.len(), s.iter(), seen),
            HeapObj::Ellipsis => "Ellipsis".into(),
            HeapObj::NotImplemented => "NotImplemented".into(),
            // A cell names what it holds by type, an unfilled one is empty.
            &HeapObj::Cell(c) if c.is_undef() => s!("<cell", str &at(v), ": empty>"),
            &HeapObj::Cell(c) => s!("<cell", str &at(v), ": ", str self.type_name(c), " object", str &at(c), ">"),
            HeapObj::Code(c) => s!("<code object <module>", str &at(v), ", file \"", str &c.filename, "\", line 1>"),

            _ => self.display_other(v, seen),
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

    fn repr(&self, v: Val) -> String { self.repr_d(v, &mut Vec::new()) }

    fn repr_d(&self, v: Val, seen: &mut Vec<u32>) -> String {
        if v.is_heap() {
            match self.heap().get(v) {
                HeapObj::Str(s) => return repr_str(s),
                // `repr(E("x"))` is the constructor call, `ValueError('x')`.
                HeapObj::ExcInstance(name, args, _) => {
                    let mut o = s!(cap: 32; str name, "(");
                    self.append_reprs(&mut o, args.iter(), seen);
                    o.push(')');
                    return o;
                }
                _ => if let Some(o) = self.repr_other(v, seen) { return o; },
            }
        }
        self.display_d(v, seen)
    }

    /* Reject mutable types (list/dict/set) used as dict/set keys, plus instances whose class defines `__eq__`. */
    fn require_hashable(&self, v: Val) -> Result<(), VmErr> { self.require_hashable_at(v, 0) }

    /* A set probe of a set looks up as the frozenset it equals, so only other values must hash. */
    fn require_set_probe(&self, v: Val) -> Result<(), VmErr> {
        if v.is_heap() && matches!(self.heap().get(v), HeapObj::Set(_)) { return Ok(()); }
        self.require_element(v)
    }

    /* `v` as a dict key, an unhashable one named for that use as well as for its type. */
    fn require_key(&self, v: Val) -> Result<(), VmErr> { self.require_hashable(v).map_err(|e| self.unhashable_as(v, &e, "a dict key")) }

    /* `v` as a set element, an unhashable one named for that use as well as for its type. */
    fn require_element(&self, v: Val) -> Result<(), VmErr> { self.require_hashable(v).map_err(|e| self.unhashable_as(v, &e, "a set element")) }

    #[cold]
    fn unhashable_as(&self, v: Val, e: &VmErr, what: &str) -> VmErr {
        VmErr::TypeMsg(crate::s!("cannot use '", str self.type_name(v), "' as ", str what, " (", str &e.message(), ")"))
    }

    fn require_hashable_at(&self, v: Val, depth: usize) -> Result<(), VmErr> {
        if v.is_heap() && depth <= EQ_DEPTH_MAX {
            match self.heap().get(v) {
                HeapObj::List(_) => return Err(cold_type("unhashable type: 'list'")),
                HeapObj::ByteArray(_) => return Err(cold_type("unhashable type: 'bytearray'")),
                HeapObj::MemoryView(m) if m.released => return Err(cold_value("operation forbidden on released memoryview object")),
                HeapObj::MemoryView(m) if matches!(self.heap().try_get(m.obj), Some(HeapObj::ByteArray(_))) => return Err(cold_value("cannot hash writable memoryview object")),
                HeapObj::Dict(_) => return Err(cold_type("unhashable type: 'dict'")),
                HeapObj::Set(_) => return Err(cold_type("unhashable type: 'set'")),
                HeapObj::DictView(..) => return Err(VmErr::TypeMsg(crate::s!("unhashable type: '", str self.type_name(v), "'"))),
                // A tuple hashes through its items as deep as the hash looks, so each must be hashable too.
                HeapObj::Tuple(items) => for &item in items { self.require_hashable_at(item, depth + 1)?; },
                // A frozendict hashes through its values too, its keys hashable already.
                HeapObj::FrozenDict(d) => for (_, item) in d.borrow().iter() { self.require_hashable_at(item, depth + 1)?; },
                // A `__eq__` without `__hash__` would let two equal keys hash apart.
                HeapObj::Instance(cls, _) if self.eq_without_hash(*cls) => return Err(VmErr::TypeMsg(crate::s!("unhashable type: '", str self.type_name(v), "'"))),
                _ => {}
            }
        }
        Ok(())
    }

    /* `v` as one byte, the int `bytearray` items must be. */
    fn byte_value(&self, v: Val) -> Result<u8, VmErr> {
        let v = crate::value::unboxed(v, self.heap());
        let Some(n) = self.as_i128(v).filter(|_| !v.is_float()) else {
            return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(v), "' object cannot be interpreted as an integer")));
        };
        u8::try_from(n).map_err(|_| cold_value("byte must be in range(0, 256)"))
    }

    /* KeyError carrying the text `msg`, as the methods that find nothing to remove raise. */
    fn text_key_error(&mut self, msg: &str) -> VmErr {
        match self.heap_mut().alloc(HeapObj::Str(msg.into())) { Ok(k) => self.key_error(k), Err(e) => e }
    }

    /* A set of `items`, each one hashable. */
    fn valset_of(&mut self, items: &[Val]) -> Result<ValSet, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; self.keyed(v)?; }
        Ok(ValSet::from_vals(items, self.heap()))
    }

    /* A dict of `pairs` in order, a later equal key overwriting the value and keeping the first key. */
    fn dictmap_of(&mut self, pairs: Vec<(Val, Val)>) -> Result<DictMap, VmErr> {
        for &(k, _) in pairs.iter().filter(|p| p.0.is_heap()) { self.require_key(k)?; self.keyed(k)?; }
        Ok(DictMap::from_pairs(pairs, self.heap()))
    }

    /* Alloc a set-algebra result, frozen picks frozenset (left-operand type rule). */
    fn alloc_set_result(&mut self, items: Vec<Val>, frozen: bool) -> Result<Val, VmErr> {
        let s = ValSet::from_vals(&items, self.heap());
        if frozen { self.heap_mut().alloc(HeapObj::FrozenSet(Rc::new(s))) }
        else { self.heap_mut().alloc(HeapObj::Set(Rc::new(RefCell::new(s)))) }
    }

    /* Wrap an i128 into the narrowest Val, None->Overflow, 48-bit->inline, else boxed. */
    #[inline]
    fn int_to_val(&mut self, r: Option<i128>) -> Result<Val, VmErr> {
        self.heap_mut().int(r.ok_or_else(cold_overflow)?)
    }

    // Allocate a Set from `items` (deduped by content) and push. Mirrors `alloc_and_push_list`.
    fn alloc_and_push_set(&mut self, items: Vec<Val>) -> Result<(), VmErr> {
        let v = self.alloc_set_result(items, false)?;
        self.push(v); Ok(())
    }

    /* The view `v` holds, refused once released. */
    fn view(&self, v: Val) -> Option<Result<MemView, VmErr>> {
        match self.heap().try_get(v)? {
            HeapObj::MemoryView(m) if m.released => Some(Err(released())),
            &HeapObj::MemoryView(m) => Some(Ok(m)),
            _ => None,
        }
    }

    /* `m[i]` of a view, the byte as an int. */
    fn view_item(&self, m: MemView, i: Val) -> Result<Val, VmErr> {
        let Some(i) = self.as_i128(i).filter(|_| !i.is_float()) else {
            return Err(VmErr::TypeMsg(s!("memoryview: invalid slice key")));
        };
        let at = crate::vm::memview::at(&m, i)?;
        let byte = self.heap().bytes_of(m.obj).and_then(|b| b.get(at)).copied().ok_or_else(|| cold_index("index out of bounds on dimension 1"))?;
        Ok(Val::int(byte as i64))
    }

    /* `m[a:b:c]`, a view of part of the view. */
    fn view_slice(&mut self, m: MemView, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> {
        let clip = |v: Val| crate::frontend::int::clip_index(self.heap(), v);
        let (s, e, st) = slice_bounds(clip(start), clip(stop), clip(step), m.len as i64)?;
        let len = if st > 0 { ((e - s).max(0) + st - 1) / st } else { ((s - e).max(0) - st - 1) / -st } as usize;
        let start = (m.start as isize + s as isize * m.step).max(0) as usize;
        self.heap_mut().alloc(HeapObj::MemoryView(MemView { obj: m.obj, start, len, step: m.step * st as isize, released: false }))
    }

    /* No-dunder indexing path. Used by callers without a bytecode frame (FFI re-entry) and as the post-dunder fallback inside `get_item`. */
    fn get_item_builtin(&mut self, obj: Val, idx: Val) -> Result<(), VmErr> {
        if idx.is_heap()
            && let &HeapObj::Slice(start, stop, step) = self.heap().get(idx) {
                let v = self.slice_val(obj, start, stop, step)?;
                self.push(v);
                return Ok(());
        }

        let ascii = idx.is_int() && self.heap_mut().str_is_ascii(obj);
        if obj.is_heap() && idx.is_int()
            && let HeapObj::Str(s) = self.heap().get(obj) {
                let i = idx.as_int();
                let one: String = if ascii {
                    let b = s.as_bytes();
                    let c = *b.get(normalize_index(i, b.len())).ok_or_else(|| cold_index("string index out of range"))?;
                    (c as char).to_string()
                } else {
                    let ui = normalize_index(i, s.chars().count());
                    s.chars().nth(ui).ok_or_else(|| cold_index("string index out of range"))?.to_string()
                };
                let val = self.heap_mut().alloc(HeapObj::Str(one))?;
                self.push(val);
                return Ok(());
        }

        // `bytes[i]` returns the byte as int (`0..=255`), unlike `str[i]` (length-1 str).
        if obj.is_heap() && idx.is_int()
            && let HeapObj::Bytes(b) | HeapObj::ByteArray(b) = self.heap().get(obj) {
                let i = idx.as_int();
                let ui = normalize_index(i, b.len());
                let what = if matches!(self.heap().get(obj), HeapObj::ByteArray(_)) { "bytearray index out of range" } else { "index out of range" };
                let byte = *b.get(ui).ok_or_else(|| cold_index(what))?;
                self.push(Val::int(byte as i64));
                return Ok(());
        }

        let v = self.getitem_val(obj, idx)?;
        self.push(v);
        Ok(())
    }

    fn slice_val(&mut self, obj: Val, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> {
        if !obj.is_heap() { return Err(cold_type("slice requires a sequence")); }
        if let Some(m) = self.view(obj) { return self.view_slice(m?, start, stop, step); }

        // Item count without materialising the source.
        let ascii = self.heap_mut().str_is_ascii(obj);
        let len: i64 = match self.heap().get(obj) {
            HeapObj::List(v) => v.borrow().len() as i64,
            HeapObj::Tuple(v) => v.len() as i64,
            HeapObj::Str(s) => if ascii { s.len() as i64 } else { s.chars().count() as i64 },
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b.len() as i64,
            // A range slices into another range, its bounds resolved at any width.
            HeapObj::Range(..) | HeapObj::BigRange(..) => return crate::frontend::int::range_slice(self.heap_mut(), obj, start, stop, step).unwrap_or(Err(VmErr::Overflow)),
            _ => return Err(cold_type("object is not sliceable")),
        };
        // A boxed index lies past either end of any sequence, so it clamps like the inline extreme.
        let clip = |v: Val| crate::frontend::int::clip_index(self.heap(), v);
        let (s, e, st) = slice_bounds(clip(start), clip(stop), clip(step), len)?;

        // Step 1 copies one contiguous range.
        let contiguous = st == 1;
        let (lo, hi) = (s.max(0) as usize, e.max(s).max(0) as usize);
        let mut indices = Vec::new();
        if !contiguous {
            let mut cur = s;
            if st > 0 { while cur < e { indices.push(cur as usize); cur += st; } }
            else { while cur > e { indices.push(cur as usize); cur += st; } }
        }
        let pick = |v: &[Val]| -> Vec<Val> {
            if contiguous { return v[lo.min(v.len())..hi.min(v.len())].to_vec(); }
            indices.iter().filter_map(|&i| v.get(i).copied()).collect()
        };
        let pick_bytes = |b: &[u8]| -> Vec<u8> {
            if contiguous { return b[lo.min(b.len())..hi.min(b.len())].to_vec(); }
            indices.iter().filter_map(|&i| b.get(i).copied()).collect()
        };

        // Pick while borrowing the source, allocate after.
        enum Out { List(Vec<Val>), Tuple(Vec<Val>), Str(String), Bytes(Vec<u8>), ByteArray(Vec<u8>) }
        let out = match self.heap().get(obj) {
            HeapObj::List(v) => Out::List(pick(&v.borrow())),
            HeapObj::Tuple(v) => Out::Tuple(pick(v)),
            HeapObj::Str(text) if ascii => Out::Str(String::from_utf8(pick_bytes(text.as_bytes())).unwrap_or_default()),
            HeapObj::Str(text) => {
                let chars: Vec<char> = text.chars().collect();
                Out::Str(if contiguous { chars[lo.min(chars.len())..hi.min(chars.len())].iter().collect() }
                    else { indices.iter().filter_map(|&i| chars.get(i)).collect() })
            }
            HeapObj::Bytes(buf) => Out::Bytes(pick_bytes(buf)),
            HeapObj::ByteArray(buf) => Out::ByteArray(pick_bytes(buf)),
            _ => return Err(cold_type("object is not sliceable")),
        };
        match out {
            Out::List(v) => self.heap_mut().alloc(HeapObj::List(Rc::new(RefCell::new(v)))),
            Out::Tuple(v) => self.heap_mut().alloc(HeapObj::Tuple(v)),
            Out::Str(s) => self.heap_mut().alloc(HeapObj::Str(s)),
            Out::Bytes(b) => self.heap_mut().alloc(HeapObj::Bytes(b)),
            Out::ByteArray(b) => self.heap_mut().alloc(HeapObj::ByteArray(b)),
        }
    }

    fn getitem_val(&mut self, obj: Val, idx: Val) -> Result<Val, VmErr> {
        if !obj.is_heap() { return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(obj), "' object is not subscriptable"))); }
        let bad = |vm: &Self, kind: &str| if crate::frontend::int::is_big(vm.heap(), idx) { cold_index("cannot fit 'int' into an index-sized integer") }
            // A string words it without slices and quotes the type.
            else if kind == "string" { VmErr::TypeMsg(crate::s!("string indices must be integers, not '", str vm.type_name(idx), "'")) }
            else { VmErr::TypeMsg(crate::s!(str kind, " indices must be integers or slices, not ", str vm.type_name(idx))) };
        if self.heap().dict_cell(obj).is_some() { self.keyed(idx)?; }
        match self.heap().get(obj) {
            HeapObj::List(v) => {
                if !idx.is_int() { return Err(bad(self, "list")); }
                let b = v.borrow(); let i = idx.as_int();
                let ui = normalize_index(i, b.len());
                b.get(ui).copied().ok_or_else(|| cold_index("list index out of range"))
            }
            HeapObj::Tuple(v) => {
                if !idx.is_int() { return Err(bad(self, "tuple")); }
                let i = idx.as_int();
                let ui = normalize_index(i, v.len());
                v.get(ui).copied().ok_or_else(|| cold_index("tuple index out of range"))
            }
            HeapObj::Dict(p) | HeapObj::FrozenDict(p) => {
                let hit = p.borrow().get(&idx, self.heap()).copied();
                match hit {
                    Some(v) => Ok(v),
                    None => { self.require_key(idx)?; Err(self.key_error(idx)) }
                }
            }
            HeapObj::Str(_) => Err(bad(self, "string")),
            HeapObj::Bytes(_) => Err(bad(self, "byte")),
            HeapObj::ByteArray(_) => Err(bad(self, "bytearray")),
            HeapObj::MemoryView(_) => { let m = self.view(obj).unwrap_or(Err(cold_heap()))?; self.view_item(m, idx) }
            &HeapObj::BigRange(..) if idx.is_int() || crate::frontend::int::is_big(self.heap(), idx) => crate::frontend::int::range_item(self.heap_mut(), obj, idx).unwrap_or(Err(VmErr::Overflow)),
            HeapObj::BigRange(..) => Err(bad(self, "range")),
            // `range(n)[i]` is computed, never materialised.
            &HeapObj::Range(s, e, st) => {
                // An index past i64 lies outside any range.
                if crate::frontend::int::is_big(self.heap(), idx) { return Err(cold_index("range object index out of range")); }
                if !idx.is_int() { return Err(bad(self, "range")); }
                let (i, len) = (idx.as_int(), range_len(s, e, st) as i64);
                let i = if i < 0 { i + len } else { i };
                if !(0..len).contains(&i) { return Err(cold_index("range object index out of range")); }
                self.heap_mut().int((s + i * st) as i128)
            }
            // `list[int]` and `Pair[int]` build a generic alias, the args kept as one tuple.
            HeapObj::Type(n) if matches!(n.as_str(), "list" | "tuple" | "dict" | "set" | "frozenset" | "type") => self.generic_alias(obj, idx),
            HeapObj::TypeAlias(..) => self.generic_alias(obj, idx),
            _ => Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(obj), "' object is not subscriptable"))),
        }
    }

    fn generic_alias(&mut self, origin: Val, idx: Val) -> Result<Val, VmErr> {
        let args = if idx.is_heap() && matches!(self.heap().get(idx), HeapObj::Tuple(_)) { idx } else { self.heap_mut().alloc(HeapObj::Tuple(alloc::vec![idx]))? };
        self.heap_mut().alloc(HeapObj::GenericAlias(origin, args))
    }

    /* `len(o)` of a builtin container, shared with the plugin ABI. */
    fn builtin_len(&mut self, o: Val) -> Result<i128, VmErr> {
        let ascii = self.heap_mut().str_is_ascii(o);
        Ok(match self.heap().try_get(o) {
            Some(HeapObj::Str(s)) => (if ascii { s.len() } else { s.chars().count() }) as i128,
            Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => b.len() as i128,
            Some(HeapObj::MemoryView(m)) => if m.released { return Err(cold_value("operation forbidden on released memoryview object")) } else { m.len as i128 },
            Some(HeapObj::List(v)) => v.borrow().len() as i128,
            Some(HeapObj::Tuple(v)) => v.len() as i128,
            Some(HeapObj::Dict(v) | HeapObj::FrozenDict(v)) => v.borrow().len() as i128,
            Some(&HeapObj::DictView(d, _)) => return self.builtin_len(d),
            Some(HeapObj::Set(v)) => v.borrow().len() as i128,
            Some(HeapObj::FrozenSet(v)) => v.len() as i128,
            Some(&HeapObj::Range(_, _, 0)) => return Err(cold_value("range() arg 3 must not be zero")),
            Some(&HeapObj::Range(s, e, st)) => crate::value::eq::range_len(s, e, st),
            Some(HeapObj::BigRange(..)) => crate::frontend::int::range_len(self.heap(), o).unwrap_or(Ok(0))?,
            _ => return Err(VmErr::TypeMsg(s!("object of type '", str self.type_name(o), "' has no len()"))),
        })
    }

    /* Item presence in list/tuple/dict/set, or substring in string. Non-iterable container raises TypeError. */
    fn contains(&mut self, container: Val, item: Val) -> Result<bool, VmErr> {
        if container.is_heap() && matches!(self.heap().get(container), HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_)) { self.keyed(item)?; }
        if container.is_heap() {
            match self.heap().get(container) {
                // A scan counts what it may read as native work.
                HeapObj::List(v) => { self.heap().spend(v.borrow().len()); return Ok(v.borrow().iter().any(|x| eq_member(*x, item, self.heap()))) }
                HeapObj::Tuple(v) => { self.heap().spend(v.len()); return Ok(v.iter().any(|x| eq_member(*x, item, self.heap()))) }
                // Only a miss checks the item, an unhashable one is never inside so a hit needs no check.
                HeapObj::Dict(p) | HeapObj::FrozenDict(p) => { let hit = p.borrow().contains_key(&item, self.heap()); if !hit { self.require_key(item)?; } return Ok(hit); }
                HeapObj::Set(s) => { let hit = s.borrow().contains(item, self.heap()); if !hit { self.require_set_probe(item)?; } return Ok(hit); }
                HeapObj::FrozenSet(s) => { let hit = s.contains(item, self.heap()); if !hit { self.require_set_probe(item)?; } return Ok(hit); }
                HeapObj::Str(s) => {
                    if let Some(HeapObj::Str(sub)) = self.heap().try_get(item) { self.heap().spend(s.len()); return Ok(s.contains(sub.as_str())); }
                    return Err(VmErr::TypeMsg(s!("'in <string>' requires string as left operand, not ", str self.type_name(item))));
                }
                HeapObj::Bytes(b) | HeapObj::ByteArray(b) => {
                    if let Some(sub) = self.heap().bytes_of(item) {
                        self.heap().spend(b.len());
                        return Ok(sub.is_empty() || b.windows(sub.len()).any(|w| w == sub));
                    }
                    let n = match item { i if i.is_int() => i.as_int(), i if i.is_bool() => i.as_bool() as i64, _ => return Err(cold_type("a bytes-like object is required")) };
                    if !(0..=255).contains(&n) { return Err(cold_value("byte must be in range(0, 256)")); }
                    return Ok(b.contains(&(n as u8)));
                }
                HeapObj::BigRange(..) => return Ok(crate::frontend::int::range_has(self.heap(), container, item).unwrap_or(false)),
                HeapObj::Range(s, e, st) => {
                    let (s, e, st) = (*s as i128, *e as i128, *st as i128);
                    // O(1) range membership via bounds + step, integral floats match too.
                    let x = match as_i128(item, self.heap()) {
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

    // None for an instance or a slice that may hold one.
    #[inline]
    fn plain_index(&self, cont: Val, idx: Val) -> Option<Val> {
        // `xs[True]` indexes like `xs[1]`, a dict keeps the bool key.
        let keyed = self.heap().dict_cell(cont).is_some();
        if idx.is_bool() && !keyed { return Some(Val::int(idx.as_bool() as i64)); }
        if !idx.is_heap() || keyed { return Some(idx); }
        // An int of a class deriving from int indexes as the int.
        if let &HeapObj::Boxed(n) = self.heap().get(idx) && (n.is_int() || n.is_foreign()) { return Some(n); }
        None
    }

    /* `C.__float__() must return a float, not int`, what a conversion dunder of `obj` that returned `got` raises. */
    #[cold]
    #[inline(never)]
    fn returned_wrong(&self, obj: Val, dunder: &str, want: &str, got: Val) -> VmErr {
        VmErr::TypeMsg(crate::s!(str self.type_name(obj), ".", str dunder, "() must return ", str want, ", not ", str self.type_name(got)))
    }

    /* `bad operand type for <what>: '<type>'`, what an operator or builtin of one operand says of a value it cannot take. */
    #[cold]
    #[inline(never)]
    fn bad_operand(&self, what: &str, v: Val) -> VmErr { VmErr::TypeMsg(crate::s!("bad operand type for ", str what, ": '", str self.type_name(v), "'")) }

    /* Whether `v` is an instance of a user class. */
    #[inline(always)]
    fn is_instance(&self, v: Val) -> bool { v.is_heap() && matches!(self.heap().get(v), HeapObj::Instance(..)) }

    /* Class of an Instance or of a dict or list a user class made, `None` for built-in operands. */
    #[inline]
    fn instance_class(&self, v: Val) -> Option<Val> { crate::vm::opcodes::dunder::class_of(self.heap(), v) }

    /* A builtin iterator named `name` over `frame`. */
    fn alloc_iterator(&mut self, frame: IterFrame, name: &'static str) -> Result<Val, VmErr> {
        self.heap_mut().alloc(HeapObj::Iter(alloc::rc::Rc::new(core::cell::RefCell::new(frame)), name))
    }

    /* Pushes a builtin iterator named `name` over `frame`. */
    fn push_iterator(&mut self, frame: IterFrame, name: &'static str) -> Result<(), VmErr> {
        let it = self.alloc_iterator(frame, name)?;
        self.push(it);
        Ok(())
    }

    /* A live walk over dict `d` showing the part `kind` names. */
    fn dict_walk(&self, d: Val, kind: View) -> IterFrame {
        let len = self.heap().dict_cell(d).map_or(0, |m| m.borrow().len());
        IterFrame::DictWalk { of: d, idx: 0, left: len, len, kind }
    }

    /* A frame over `items` read from `o`, watching a dict, dict view or set for a change of size. */
    fn watched(&self, o: Val, items: Vec<Val>) -> IterFrame {
        let of = match self.heap().try_get(o) {
            Some(HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_)) => o,
            Some(&HeapObj::DictView(d, _)) => d,
            _ => return IterFrame::Seq { items: items.into(), idx: 0 },
        };
        IterFrame::Watched { items: items.into(), idx: 0, of, len: crate::value::watched_len(self.heap(), of) }
    }

    // Interned keyword name behind a Val, or None when not a string.
    fn kw_name(&self, k: Val) -> Option<&str> {
        match self.heap().try_get(k) {
            Some(HeapObj::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn add_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        // Inline-int fast path, overflow falls through to the i128 slow path.
        if a.is_int() && b.is_int()
            && let Some(r) = a.as_int().checked_add(b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, self.heap()) { let (af, bf) = p?; return Ok(Val::float(af + bf)); }
        if let (Some(ai), Some(bi)) = (as_i128(a, self.heap()), as_i128(b, self.heap())) && let Some(r) = ai.checked_add(bi) { return self.heap_mut().int(r); }
        if let Some(r) = self.wide_op(BinOp::Add, a, b)? { return Ok(r); }
        if a.is_heap() && b.is_heap() {
            // Charge the copy cost so growing concatenation in a loop stays bounded (avoids O(n^2)).
            let copy_cost = match (self.heap().get(a), self.heap().get(b)) {
                (HeapObj::Str(sa), HeapObj::Str(sb)) => Some(sa.len() + sb.len()),
                (HeapObj::Bytes(ba) | HeapObj::ByteArray(ba), HeapObj::Bytes(bb) | HeapObj::ByteArray(bb)) => Some(ba.len() + bb.len()),
                (HeapObj::List(va), HeapObj::List(vb)) => Some(va.borrow().len() + vb.borrow().len()),
                (HeapObj::Tuple(va), HeapObj::Tuple(vb)) => Some(va.len() + vb.len()),
                _ => None,
            };
            if let Some(n) = copy_cost { self.charge(n)?; }
            match (self.heap().get(a), self.heap().get(b)) {
                (HeapObj::Str(sa), HeapObj::Str(sb)) => {
                    let mut r = String::with_capacity(sa.len() + sb.len());
                    r.push_str(sa); r.push_str(sb);
                    return self.heap_mut().alloc(HeapObj::Str(r));
                }
                // Each result is sized once, so no operand is copied twice.
                (HeapObj::List(va), HeapObj::List(vb)) => {
                    let lst = [&va.borrow()[..], &vb.borrow()[..]].concat();
                    return self.heap_mut().alloc(HeapObj::List(Rc::new(RefCell::new(lst))));
                }
                (HeapObj::Tuple(va), HeapObj::Tuple(vb)) => {
                    let tup = [&va[..], &vb[..]].concat();
                    return self.heap_mut().alloc(HeapObj::Tuple(tup));
                }
                // The result takes the type of the left side.
                (left @ (HeapObj::Bytes(ba) | HeapObj::ByteArray(ba)), HeapObj::Bytes(bb) | HeapObj::ByteArray(bb)) => {
                    let (r, array) = ([&ba[..], &bb[..]].concat(), matches!(left, HeapObj::ByteArray(_)));
                    return self.heap_mut().alloc(if array { HeapObj::ByteArray(r) } else { HeapObj::Bytes(r) });
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
        match self.heap().try_get(a)? {
            HeapObj::Str(_) | HeapObj::List(_) | HeapObj::Tuple(_) => Some(VmErr::TypeMsg(s!("can only concatenate ", str left, " (not \"", str right, "\") to ", str left))),
            HeapObj::Bytes(_) | HeapObj::ByteArray(_) => Some(VmErr::TypeMsg(s!("can't concat ", str right, " to ", str left))),
            _ => None,
        }
    }

    /* TypeError for a binary operator no operand type supports. */
    #[cold]
    fn unsupported(&self, op: &str, a: Val, b: Val) -> VmErr {
        VmErr::TypeMsg(s!("unsupported operand type(s) for ", str op, ": '", str self.type_name(a), "' and '", str self.type_name(b), "'"))
    }

    /* `op` through the kind of a FOREIGN operand, None when nothing takes it. */
    fn wide_op(&mut self, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        if !a.is_foreign() && !b.is_foreign() { return Ok(None); }
        self.heap_mut().foreign_binop(op, a, b)
    }

    fn lt_vals(&self, a: Val, b: Val) -> Result<bool, VmErr> {
        self.lt_vals_d(a, b, 0)
    }

    /* Depth-tracked `<`, past the cap surface RecursionError instead of overflowing the stack. */
    fn lt_vals_d(&self, a: Val, b: Val, depth: usize) -> Result<bool, VmErr> {
        if depth > CMP_DEPTH_MAX { return Err(cold_depth()); }
        let a = if a.is_bool() { Val::int(a.as_bool() as i64) } else { a };
        let b = if b.is_bool() { Val::int(b.as_bool() as i64) } else { b };
        if a.is_int() && b.is_int() { return Ok(a.as_int() < b.as_int()); }
        if (a.is_foreign() || b.is_foreign()) && let Some(o) = self.heap().foreign_cmp(a, b) { return Ok(o == Ordering::Less); }
        if let Some(Ok((af, bf))) = coerce_floats(a, b, self.heap()) { return Ok(af < bf); }
        if a.is_heap() && b.is_heap() {
            match (self.heap().get(a), self.heap().get(b)) {
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
        let (x, y) = (unboxed(a, self.heap()), unboxed(b, self.heap()));
        if x.0 != a.0 || y.0 != b.0 { return self.lt_vals_d(x, y, depth); }
        Err(VmErr::TypeMsg(s!("'<' not supported between instances of '", str self.type_name(a), "' and '", str self.type_name(b), "'")))
    }

    /* Lexicographic `<` for sequences, first differing element decides, otherwise the shorter is less. Recurses through `lt_vals`, so nested sequences and mixed element types are handled (and rejected) consistently. */
    fn seq_lt_d(&self, xs: &[Val], ys: &[Val], depth: usize) -> Result<bool, VmErr> {
        if depth > CMP_DEPTH_MAX { return Err(cold_depth()); }
        for (&x, &y) in xs.iter().zip(ys.iter()) {
            if eq_member(x, y, self.heap()) { continue; }
            return self.lt_vals_d(x, y, depth + 1);
        }
        Ok(xs.len() < ys.len())
    }

    /* `a // b` or `a % b`, floats when either side is one, signed like the divisor. */
    fn floor_op(&mut self, op: BinOp, a: Val, b: Val, sym: &str) -> Result<Val, VmErr> {
        let modulo = op == BinOp::Mod;
        if a.is_float() || b.is_float() {
            let (Some(af), Some(bf)) = (num_f64(a, self.heap()), num_f64(b, self.heap())) else { return Err(self.unsupported(sym, a, b)); };
            let (af, bf) = (af?, bf?);
            if bf == 0.0 { return Err(VmErr::ZeroDiv); }
            let (q, r) = float_divmod(af, bf);
            return Ok(Val::float(if modulo { r } else { q }));
        }
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) && y != 0 && let Some((q, r)) = int_divmod(x, y) {
            return self.heap_mut().int(if modulo { r } else { q });
        }
        self.heap_mut().foreign_binop(op, a, b)?.ok_or_else(|| self.unsupported(sym, a, b))
    }

    /* Two-arg power for `pow()` and `**`, exact for ints and a float for floats or negative exponents. */
    fn pow_vals(&mut self, a: Val, b: Val, err_msg: &'static str) -> Result<Val, VmErr> {
        if let (Some(ai), Some(exp)) = (self.as_i128(a), self.as_i128(b).and_then(|e| i64::try_from(e).ok())) {
            if exp >= 0 {
                if let Some(r) = checked_pow(ai, exp as u64) { return self.heap_mut().int(r); }
                return self.heap_mut().foreign_binop(BinOp::Pow, a, b)?.ok_or_else(cold_overflow);
            }
            if ai == 0 { return Err(VmErr::Raised(String::from("ZeroDivisionError: zero to a negative power"))); }
            // Clamp instead of wrapping so a huge negative exponent underflows to 0.0, base ±1 keeps its parity.
            let r = match ai {
                1 => 1.0,
                -1 => if exp % 2 == 0 { 1.0 } else { -1.0 },
                _ => libm::pow(ai as f64, exp as f64),
            };
            return Ok(Val::float(r));
        }
        if let Some(r) = self.wide_op(BinOp::Pow, a, b)? { return Ok(r); }
        let (Some(fa), Some(fb)) = (num_f64(a, self.heap()), num_f64(b, self.heap())) else { return Err(cold_type(err_msg)); };
        let (fa, fb) = (fa?, fb?);
        if fa == 0.0 && fb < 0.0 { return Err(VmErr::Raised(String::from("ZeroDivisionError: zero to a negative power"))); }
        // A negative base under a fractional power has only a complex result.
        if crate::value::math::complex_power(fa, fb) { return crate::frontend::complex::power(self.heap_mut(), fa, fb); }
        fpowf(fa, fb).map(Val::float).ok_or_else(|| VmErr::Raised(String::from("OverflowError: (34, 'Numerical result out of range')")))
    }


    /* True when a comparison over `vals` can reach a user dunder. */
    fn any_instance(&self, vals: &[Val]) -> bool {
        vals.iter().any(|&v| matches!(self.heap().try_get(v), Some(HeapObj::Instance(..))))
    }

    /* Whether `f` is `eval` or `exec`, whose call where a frame may open runs its text as a floor of the loop. */
    fn runs_text(&self, f: Val) -> bool { matches!(self.heap().try_get(f), Some(HeapObj::NativeFn(NativeFnId::Eval | NativeFnId::Exec))) }

    #[inline] fn alloc_and_push_tuple(&mut self, items: Vec<Val>) -> Result<(), VmErr> { let v = self.heap_mut().alloc(HeapObj::Tuple(items))?; self.push(v); Ok(()) }
}

// Type name and truth read off the heap alone.

/* The kinds of value no builtin type names, coded past the end of `BUILTIN_TYPES` in this order by the names
   `type_name_by` gives them, the first five standing for a family: any instance of a class of the program, any function
   or method, any builtin iterator, and each kind of suspended body. */
pub const OTHER_KINDS: &[&str] = &[
    "instance", "function", "iterator", "generator", "coroutine", "async_generator", "module", "property", "staticmethod",
    "classmethod", "super", "cell", "code", "ellipsis", "NotImplementedType", "GenericAlias", "TypeAliasType", "Union",
    "TypeVar", "ParamSpec", "TypeVarTuple", "dict_keys", "dict_values", "dict_items",
];

impl HeapPool {
    pub fn truthy(&self, v: Val) -> bool {
        if v.is_none() || v.is_false() { return false; }
        if v.is_true() { return true; }
        if v.is_int() { return v.as_int() != 0; }
        if v.is_float() { return v.as_float() != 0.0; }
        if v.is_foreign() { return self.foreign_truthy(v).unwrap_or(true); }
        match self.get(v) {
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
            HeapObj::BigRange(..) => crate::frontend::int::range_empty(self, v) == Some(false),
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

    #[inline]
    pub fn type_name(&self, v: Val) -> &str { self.type_name_by(v, &|_| "coroutine") }

    // `coro` names a generator or coroutine by the body it runs.
    #[inline(always)]
    pub fn type_name_by<'h>(&'h self, v: Val, coro: &impl Fn(&super::Coro) -> &'h str) -> &'h str {
        let scalar = |v: Val| if v.is_bool() { Some("bool") }
            else if v.is_int() { Some("int") }
            else if v.is_float() { Some("float") }
            else if v.is_none() { Some("NoneType") }
            else if v.is_foreign() { Some(self.foreign_kind(v).map_or("object", |k| k.type_name)) }
            else { None };
        if let Some(n) = scalar(v) { n }
        else { match self.get(v) {
            // A number of a class over int or float takes the number's name.
            &HeapObj::Boxed(n) => scalar(n).unwrap_or("object"),
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
            // A dunder of a builtin type is a slot wrapper, unbound or bound, a named method a descriptor until bound.
            &HeapObj::BoundMethod(recv, id) if recv.is_undef() || id.ty() == "op" && matches!(self.try_get(recv), Some(HeapObj::Type(_))) => if is_dunder(id.name()) { "wrapper_descriptor" } else { "method_descriptor" },
            HeapObj::BoundMethod(_, id) if is_dunder(id.name()) => "method-wrapper",
            HeapObj::BoundMethod(..) | HeapObj::NativeFn(_) | HeapObj::Extern(_) => "builtin_function_or_method",
            HeapObj::BoundUserMethod(..) => "method",
            HeapObj::Super(..) => "super",
            HeapObj::Property(..) | HeapObj::PropertyPart(..) => "property",
            HeapObj::StaticMethod(..) => "staticmethod",
            HeapObj::ClassMethod(..) => "classmethod",
            HeapObj::Instance(cls, _) => {
                if cls.is_heap() && let HeapObj::Class(name, _, _) = self.get(*cls) {
                    return name;
                }
                "object"
            }
            HeapObj::Coroutine(c) => coro(c),
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
}

pub fn is_dunder(name: &str) -> bool { name.len() > 4 && name.starts_with("__") && name.ends_with("__") }

pub(crate) fn normalize_index(i: i64, len: usize) -> usize {
    (if i < 0 { len as i64 + i } else { i }) as usize
}

/* Slice bounds clamped to a sequence of `len` items, the triple `slice.indices(len)` returns. */
pub(crate) fn slice_bounds(start: Val, stop: Val, step: Val, len: i64) -> Result<(i64, i64, i64), VmErr> {
    let st = if step.is_none() { 1 } else if step.is_int() { step.as_int() } else {
        return Err(cold_type("slice step must be an integer"));
    };
    if st == 0 { return Err(cold_value("slice step cannot be zero")); }
    let clamp = |v: Val, def: i64| -> i64 {
        if v.is_none() { def }
        else if v.is_int() { let i = v.as_int(); if i < 0 { (len+i).max(0) } else { i.min(len) } }
        else { def }
    };
    // Negative step bounds at [-1, len-1], an underflowing index floors at -1, not 0.
    let clamp_neg = |v: Val, def: i64| -> i64 {
        if v.is_none() { def }
        else if v.is_int() { let i = v.as_int(); (if i < 0 { len + i } else { i }).clamp(-1, len - 1) }
        else { def }
    };
    Ok(if st > 0 { (clamp(start, 0), clamp(stop, len), st) } else { (clamp_neg(start, len - 1), clamp_neg(stop, -1), st) })
}

pub(crate) fn released() -> VmErr { cold_value("operation forbidden on released memoryview object") }

/* Same cap for `<` descent, self-referential sequences raise RecursionError. */
pub(crate) const CMP_DEPTH_MAX: usize = 100;

/* Coerce a numeric pair to f64, returns None if neither operand is a float. */
pub(crate) fn coerce_floats(a: Val, b: Val, heap: &HeapPool) -> Option<Result<(f64, f64), VmErr>> {
    if !a.is_float() && !b.is_float() { return None; }
    let (x, y) = (num_f64(a, heap)?, num_f64(b, heap)?);
    Some(x.and_then(|x| Ok((x, y?))))
}

/* `base ** e` by squaring, None once a step leaves i128. */
pub(crate) fn checked_pow(mut base: i128, mut e: u64) -> Option<i128> {
    let mut acc: i128 = 1;
    while e > 0 {
        if e & 1 == 1 { acc = acc.checked_mul(base)?; }
        e >>= 1;
        if e > 0 { base = base.checked_mul(base)?; }
    }
    Some(acc)
}
