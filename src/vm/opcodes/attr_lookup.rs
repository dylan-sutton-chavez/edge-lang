use super::*;
use crate::s;

pub use crate::vm::methods::BuiltinMethodId;
use crate::vm::methods::lookup_method;

// `resolve_attr` result, every shape LoadAttr / CallMethod dispatches on. Built-in method bodies live in `vm/methods/`.
pub(crate) enum AttrLookup {
    ModuleAttr(Val),
    // A builtin read off the `builtins` module, made the first time it is asked for.
    Builtin(&'static str),
    ClassMember(Val),
    InstanceField(Val),
    // `class` is where `func` was found, and the called frame needs it so `super()` knows where to resume.
    InstanceMethod { recv: Val, func: Val, class: Val },
    BuiltinMethod(BuiltinMethodId),
    // A builtin method bound to a receiver other than the accessed object, `super().__init__` of an exception.
    BoundBuiltin(Val, BuiltinMethodId),
    // `str.lower` on the type, the call takes its receiver as the first argument.
    UnboundMethod(BuiltinMethodId),
    // `e.args` on ExcInstance, caller picks between LoadAttr materialising the tuple and CallMethod erroring.
    ExcArgs(Vec<Val>),
    // Property descriptor on an instance, `LoadAttr` invokes `getter(recv)`.
    PropertyGet { recv: Val, getter: Val },
    // `prop.setter` access, `LoadAttr` materialises a `PropertySetter` value bound to the source property.
    PropertySetterRef(Val),
    // `__name__` on a function, type, or class, and `LoadAttr` materialises the str.
    Name(String),
    // `x.__class__` of a builtin value, `LoadAttr` materialises the type named here.
    TypeOf(String),
    // `X.__value__` of a type alias, `LoadAttr` calls the zero-argument function that evaluates it.
    Thunk(Val),
    // `obj.__dict__`, the attributes of the instance as a dict that shares them.
    Dict(Rc<RefCell<DictMap>>),
}

impl<'a> VM<'a> {
    // The cached C3 linearization of `cls`, or `[cls]` when uncached (native classes, or an inconsistent hierarchy that `c3_merge` declined to cache).
    fn mro_of(&self, c: Val) -> alloc::vec::Vec<Val> {
        match self.mro_cache.get(&c.0) {
            Some(r) => (**r).clone(),
            None => alloc::vec![c],
        }
    }

    /* C3 merge of the bases' linearizations plus the bases list itself, the tail of `L[cls] = cls :: merge(...)`. `cls` is prepended by the caller (it isn't allocated yet at validation time). Errs on an inconsistent hierarchy, matching Python's `TypeError` at class creation. */
    pub(crate) fn c3_merge(&self, bases: &[Val]) -> Result<alloc::vec::Vec<Val>, VmErr> {
        let mut seqs: alloc::vec::Vec<alloc::vec::Vec<Val>> = bases.iter().map(|&b| self.mro_of(b)).collect();
        if !bases.is_empty() { seqs.push(bases.to_vec()); }
        let mut out = alloc::vec::Vec::new();
        loop {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() { break; }
            // A valid head appears in no sequence's tail, so take the first such across sequences (C3 order).
            let mut head = None;
            for s in &seqs {
                let h = s[0];
                let in_tail = seqs.iter().any(|t| t.len() > 1 && t[1..].iter().any(|&x| x.0 == h.0));
                if !in_tail { head = Some(h); break; }
            }
            let Some(h) = head else {
                return Err(cold_type("Cannot create a consistent method resolution order (MRO) for bases"));
            };
            out.push(h);
            for s in &mut seqs { s.retain(|&x| x.0 != h.0); }
        }
        Ok(out)
    }

    /* Bind a resolved MRO member `mv` to `recv`, mapping Property to getter, staticmethod to unbound, function to descriptor-bound. Plain data is returned as-is. Guards is_heap before heap.get so a non-heap data member is never read as a pointer. */
    /* Whether a class member is a method or property, which an instance attribute could hide. */
    pub(crate) fn hides(&self, member: Val) -> bool {
        matches!(self.heap.try_get(member), Some(HeapObj::Func(..) | HeapObj::Property(..)))
    }

    /* Whether `class` reaches a property named `name`. */
    pub(crate) fn class_property(&self, class: Val, name: &str) -> bool {
        self.lookup_class_member(class, name).is_some_and(|(m, _)| matches!(self.heap.try_get(m), Some(HeapObj::Property(..))))
    }

    fn bind_member(&self, mv: Val, recv: Val, defining: Val) -> AttrLookup {
        if mv.is_heap() {
            match self.heap.get(mv) {
                HeapObj::Property(getter, _) => return AttrLookup::PropertyGet { recv, getter: *getter },
                HeapObj::StaticMethod(func) => return AttrLookup::ClassMember(*func),
                // Native-class methods take self as their first argument.
                HeapObj::Extern(_) => return AttrLookup::InstanceMethod { recv, func: mv, class: defining },
                HeapObj::ClassMethod(func) => {
                    // Bind the receiver's class, not the instance.
                    let cls = self.instance_class(recv).unwrap_or(recv);
                    return AttrLookup::InstanceMethod { recv: cls, func: *func, class: defining };
                }
                HeapObj::Func(..) => return AttrLookup::InstanceMethod { recv, func: mv, class: defining },
                _ => {}
            }
        }
        AttrLookup::ClassMember(mv)
    }

    // Member lookup along the C3 MRO, first hit wins. Falls back to a direct-then-DFS walk for uncached classes (native classes have no bases, so DFS = own members). Returns `(value, defining_class)` so callers building `BoundUserMethod` / `InstanceMethod` record where the method came from for `super()`.
    pub(crate) fn lookup_class_member(&self, cls: Val, name: &str) -> Option<(Val, Val)> {
        if !cls.is_heap() { return None; }
        let HeapObj::Class(_, bases, members) = self.heap.get(cls) else { return None; };
        if let Some(mro) = self.mro_cache.get(&cls.0) {
            for &c in mro.iter() {
                if let HeapObj::Class(_, _, m) = self.heap.get(c)
                    && let Some(&(_, v)) = m.borrow().iter().find(|(n, _)| n == name) {
                        return Some((v, c));
                    }
            }
            return None;
        }
        if let Some(&(_, v)) = members.borrow().iter().find(|(n, _)| n == name) { return Some((v, cls)); }
        for &b in bases {
            if let Some(found) = self.lookup_class_member(b, name) { return Some(found); }
        }
        None
    }

    /* The builtin exception a class derives from through its bases, None for a plain class. */
    pub(crate) fn exc_base(&self, cls: Val) -> Option<&str> {
        match self.heap.try_get(cls)? {
            HeapObj::Type(n) => crate::vm::globals::matches_exc_class(n, "BaseException").then_some(n.as_str()),
            HeapObj::Class(_, bases, _) => bases.iter().find_map(|&b| self.exc_base(b)),
            _ => None,
        }
    }

    /* The builtin `dict` or `list` a class derives from through its bases, None for a plain class. */
    pub(crate) fn container_base(&self, cls: Val) -> Option<Val> {
        match self.heap.try_get(cls)? {
            HeapObj::Type(n) if matches!(n.as_str(), "dict" | "list") => Some(cls),
            HeapObj::Class(_, bases, _) => bases.iter().find_map(|&b| self.container_base(b)),
            _ => None,
        }
    }

    /* Sets the `args` of exception instance `inst`, what its constructor and `__init__` received. */
    pub(crate) fn set_exc_args(&mut self, inst: Val, args: Vec<Val>) -> Result<(), VmErr> {
        let tuple = self.heap.alloc(HeapObj::Tuple(args))?;
        let key = self.heap.alloc(HeapObj::Str("args".into()))?;
        if let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(inst) { attrs.borrow_mut().insert(key, tuple, &self.heap); }
        Ok(())
    }

    /* `super()` lookup walks `derived`'s C3 MRO strictly past `after`, so a diamond resolves to the next class in the instance's linearization (not just `after`'s own bases). Falls back to a DFS over `after`'s bases when `derived` has no cached MRO. */
    pub(crate) fn lookup_class_member_after(&self, derived: Val, after: Val, name: &str) -> Option<(Val, Val)> {
        if let Some(mro) = self.mro_cache.get(&derived.0) {
            let mut past = false;
            for &c in mro.iter() {
                if past
                    && let HeapObj::Class(_, _, m) = self.heap.get(c)
                    && let Some(&(_, v)) = m.borrow().iter().find(|(n, _)| n == name) {
                        return Some((v, c));
                    }
                if c.0 == after.0 { past = true; }
            }
            return None;
        }
        // Fallback searches strictly above `after` via its own bases.
        if !after.is_heap() { return None; }
        let HeapObj::Class(_, bases, _) = self.heap.get(after) else { return None; };
        for &b in bases {
            if let Some(found) = self.lookup_class_member(b, name) { return Some(found); }
        }
        None
    }

    // `obj.<name>` for LoadAttr and CallMethod, other kinds fall through to builtin methods.
    pub(crate) fn resolve_attr(&self, obj: Val, name: &str) -> Result<AttrLookup, VmErr> {
        let missing = || VmErr::Attribute(s!("'", str self.type_name(obj), "' object has no attribute '", str name, "'"));
        // A dict or list a user class made reads its own attributes and class first, then the builtin methods.
        if let Some((cls, attrs)) = self.heap.classed(obj) {
            let found = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name)).map(|(_, v)| v);
            if let Some(v) = found { return Ok(AttrLookup::InstanceField(v)); }
            if let Some((mv, defining)) = self.lookup_class_member(*cls, name) { return Ok(self.bind_member(mv, obj, defining)); }
            if name == "__class__" { return Ok(AttrLookup::ClassMember(*cls)); }
            return lookup_method(self.type_name(obj), name).map(AttrLookup::BuiltinMethod)
                .ok_or_else(|| VmErr::Attribute(s!("'", str &self.type_repr_name(obj), "' object has no attribute '", str name, "'")));
        }
        match self.heap.try_get(obj) {
            // Module attr lookup is a linear scan, the table is sized for around 30 entries.
            Some(HeapObj::Module(mod_name, attrs)) => {
                if let Some(&(_, v)) = attrs.iter().find(|(n, _)| n == name) { return Ok(AttrLookup::ModuleAttr(v)); }
                // The `builtins` module holds every builtin without making them up front.
                let builtin = (mod_name == "builtins").then(|| crate::parser::builtin_type(name).or_else(|| crate::vm::NativeFnId::from_name(name).map(|id| id.name()))).flatten();
                return builtin.map(AttrLookup::Builtin).ok_or_else(|| VmErr::Attribute(s!("module '", str mod_name, "' has no attribute '", str name, "'")));
            }
            // ExcInstance attrs, `e.args` and the chain the exception was raised in.
            Some(HeapObj::ExcInstance(n, args, _)) => return match name {
                "args" => Ok(AttrLookup::ExcArgs(args.clone())),
                "value" if n == "StopIteration" => Ok(AttrLookup::ClassMember(args.first().copied().unwrap_or(Val::none()))),
                "__class__" => Ok(AttrLookup::TypeOf(n.clone())),
                _ => self.chain_attr(obj, name).map(AttrLookup::ClassMember)
                    .or_else(|| lookup_method("BaseException", name).map(AttrLookup::BuiltinMethod)).ok_or_else(missing),
            },
            // Bound methods expose their receiver, user methods also their function.
            Some(&HeapObj::BoundUserMethod(recv, _, _)) if name == "__self__" => return Ok(AttrLookup::ClassMember(recv)),
            Some(&HeapObj::BoundUserMethod(_, func, _)) if name == "__func__" => return Ok(AttrLookup::ClassMember(func)),
            Some(&HeapObj::BoundMethod(recv, _)) if name == "__self__" && !recv.is_undef() => return Ok(AttrLookup::ClassMember(recv)),
            // Function attributes, a stored attr wins over the derived `__name__`.
            Some(HeapObj::Func(fi, _, _, attrs)) => {
                if let Some(&(_, v)) = attrs.borrow().iter().find(|(n, _)| n == name) { return Ok(AttrLookup::ClassMember(v)); }
                if name == "__name__" && let Some(n) = self.function_names.get(*fi) { return Ok(AttrLookup::Name(n.clone())); }
            }
            // Class attr, `MyClass.method` returns the unbound function (no `self` prepended).
            Some(HeapObj::Class(cls_name, _, _)) => {
                if name == "__name__" { return Ok(AttrLookup::Name(cls_name.clone())); }
                if let Some((v, defining)) = self.lookup_class_member(obj, name) {
                    return Ok(match self.heap.try_get(v) {
                        // `staticmethod` accessed on the class itself unwraps to the plain function.
                        Some(&HeapObj::StaticMethod(func)) => AttrLookup::ClassMember(func),
                        // `classmethod` binds the accessed class, derived included.
                        Some(&HeapObj::ClassMethod(func)) => AttrLookup::InstanceMethod { recv: obj, func, class: defining },
                        _ => AttrLookup::ClassMember(v),
                    });
                }
                return Err(VmErr::Attribute(s!("type object '", str cls_name, "' has no attribute '", str name, "'")));
            }
            // Instance attribute lookup, check `__dict__` first, then the class chain (direct + bases).
            Some(HeapObj::Instance(cls_val, attrs)) => {
                let found = attrs.borrow().iter()
                    .find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name))
                    .map(|(_, v)| v);
                // A property, or a class `args` of an exception, outranks a field of the same name.
                let hidden = || (self.shadowed && self.class_property(*cls_val, name)) || (name == "args" && self.exc_base(*cls_val).is_some() && self.lookup_class_member(*cls_val, name).is_some());
                if let Some(v) = found && !hidden() { return Ok(AttrLookup::InstanceField(v)); }
                if name == "__dict__" && self.exc_base(*cls_val).is_none() { return Ok(AttrLookup::Dict(attrs.clone())); }
                if let Some((mv, defining)) = self.lookup_class_member(*cls_val, name) { return Ok(self.bind_member(mv, obj, defining)); }
                // An exception falls back to the `BaseException` methods.
                if self.exc_base(*cls_val).is_some() && let Some(id) = lookup_method("BaseException", name) { return Ok(AttrLookup::BuiltinMethod(id)); }
                if self.exc_base(*cls_val).is_some() && let Some(v) = self.chain_attr(obj, name) { return Ok(AttrLookup::ClassMember(v)); }
                if name == "__class__" { return Ok(AttrLookup::ClassMember(*cls_val)); }
                return Err(missing());
            }
            // `super().<name>` searches strictly above the proxy's stored class, and methods bind to the proxy's `recv`.
            Some(&HeapObj::Super(cls_val, recv)) => {
                // C3 super walks the *instance type*'s MRO past the defining class, not just the defining class's bases.
                let derived = self.instance_class(recv).unwrap_or(cls_val);
                if let Some((mv, defining)) = self.lookup_class_member_after(derived, cls_val, name) { return Ok(self.bind_member(mv, recv, defining)); }
                // Past the user classes of a dict or list sit its builtin methods.
                if self.heap.classed(recv).is_some() && let Some(id) = lookup_method(self.type_name(recv), name) { return Ok(AttrLookup::BoundBuiltin(recv, id)); }
                // Past the user classes of an exception sits `BaseException`.
                if self.exc_base(derived).is_some() && let Some(id) = lookup_method("BaseException", name) { return Ok(AttrLookup::BoundBuiltin(recv, id)); }
                return Err(VmErr::Attribute(s!("'super' object has no attribute '", str name, "'")));
            }
            // Every builtin iterator shares `__next__` and `__iter__`.
            Some(HeapObj::Iter(..)) => return lookup_method("iterator", name).map(AttrLookup::BuiltinMethod).ok_or_else(missing),
            // `prop.setter` produces a callable that re-builds the property with a new setter (powers `@x.setter`).
            Some(HeapObj::Property(..)) if name == "setter" => return Ok(AttrLookup::PropertySetterRef(obj)),
            // A method off a builtin type stays unbound, a classmethod like `dict.fromkeys` binds the type.
            Some(HeapObj::Type(n)) => {
                if name == "__name__" { return Ok(AttrLookup::Name(n.clone())); }
                // `Exception.__init__(self, msg)` reaches the shared exception methods.
                let owner = if crate::vm::globals::matches_exc_class(n, "BaseException") { "BaseException" } else { n.as_str() };
                let op = || crate::vm::methods::operator::applies(n, name).then(|| lookup_method("op", name)).flatten();
                if let Some(id) = lookup_method(owner, name).or_else(op) {
                    let classmethod = matches!(name, "fromkeys" | "fromhex" | "from_bytes");
                    return Ok(if classmethod { AttrLookup::BuiltinMethod(id) } else { AttrLookup::UnboundMethod(id) });
                }
            }
            // Plain fields, `slice.start`, `.stop` and `.step`, an alias `__origin__` and `__args__`, and the lazy `__value__`.
            Some(&HeapObj::Slice(v, _, _)) if name == "start" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::Slice(_, v, _)) if name == "stop" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::Slice(_, _, v)) if name == "step" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::GenericAlias(v, _)) if name == "__origin__" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::GenericAlias(_, v) | &HeapObj::Union(v)) if name == "__args__" => return Ok(AttrLookup::ClassMember(v)),
            Some(HeapObj::TypeAlias(n, _)) if name == "__name__" => return Ok(AttrLookup::Name(n.clone())),
            Some(&HeapObj::TypeAlias(_, f)) if name == "__value__" => return Ok(AttrLookup::Thunk(f)),
            _ => {}
        }
        if name == "__class__" { return Ok(AttrLookup::TypeOf(self.type_name(obj).into())); }
        if let Some(v) = crate::vm::methods::operator::number_attr(self, obj, name) { return Ok(AttrLookup::ClassMember(v)); }
        // A mutable container has no hash, its `__hash__` is None.
        if name == "__hash__" && matches!(self.type_name(obj), "list" | "dict" | "set") { return Ok(AttrLookup::ClassMember(Val::none())); }
        // Builtin type method, then the dunders its operators answer.
        let ty = self.type_name(obj);
        lookup_method(ty, name).or_else(|| crate::vm::methods::operator::applies(ty, name).then(|| lookup_method("op", name)).flatten())
            .map(AttrLookup::BuiltinMethod).ok_or_else(missing)
    }

    /* `case C(p, k=q)` checks `isinstance(subj, C)`, then pushes a tuple of the values its sub-patterns match, or None on a miss. */
    pub(crate) fn match_class(&mut self, npos: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        let names = self.pop()?;
        let cls = self.pop()?;
        let subj = self.pop()?;
        self.push(subj);
        self.push(cls);
        self.call_isinstance()?;
        if !self.pop()?.as_bool() { self.push(Val::none()); return Ok(()); }
        let mut attrs: Vec<String> = Vec::new();
        let base = self.stack.len();
        // A builtin type matches its one positional sub-pattern against the subject itself.
        let self_match = matches!(self.heap.get(cls), HeapObj::Type(n)
            if matches!(n.as_str(), "bool" | "bytes" | "dict" | "float" | "frozenset" | "int" | "list" | "set" | "str" | "tuple"));
        if self_match && npos > 0 {
            if npos > 1 { return Err(VmErr::TypeMsg(s!(str self.type_name(subj), "() accepts 1 positional sub-pattern"))); }
            self.push(subj);
        } else if npos > 0 {
            let order = self.lookup_class_member(cls, "__match_args__").map(|(v, _)| v);
            let Some(HeapObj::Tuple(order)) = order.and_then(|v| self.heap.try_get(v)) else {
                return Err(VmErr::TypeMsg("class pattern accepts no positional sub-patterns without __match_args__".into()));
            };
            if npos > order.len() { return Err(VmErr::TypeMsg("class pattern got more positional sub-patterns than __match_args__".into())); }
            for &n in &order[..npos] { attrs.push(self.display(n)); }
        }
        if let HeapObj::Tuple(kw) = self.heap.get(names) { for &n in kw { attrs.push(self.display(n)); } }
        for a in &attrs {
            match self.load_attr(subj, a, chunk) {
                Ok(()) => {}
                // A missing attribute fails the pattern instead of raising.
                Err(e) if self.absorb_attr_err(&e) => { self.stack.truncate(base); self.push(Val::none()); return Ok(()); }
                Err(e) => return Err(e),
            }
        }
        let values = self.stack.split_off(base);
        let t = self.heap.alloc(HeapObj::Tuple(values))?;
        self.push(t);
        Ok(())
    }

    /* True for an AttributeError, native, raised or a user subclass, which then counts as handled. */
    pub(crate) fn absorb_attr_err(&mut self, e: &VmErr) -> bool {
        let user = match self.pending.exc_val.and_then(|v| self.heap.try_get(v)) { Some(&HeapObj::Instance(c, _)) => self.exc_base(c), _ => None };
        let hit = matches!(e, VmErr::Raised(_) | VmErr::Attribute(_)) && crate::vm::globals::matches_exc_class(user.unwrap_or(&e.class_name()), "AttributeError");
        if hit { self.pending.exc_val = None; }
        hit
    }

    /* instance fallback via `__getattr__(name)`. Called by `LoadAttr` / `CallMethod` after the normal lookup raises `AttributeError`. */
    pub(crate) fn try_getattr_fallback(&mut self, obj: Val, name: &str, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        if self.instance_class(obj).is_none() { return Ok(None); }
        let name_val = self.heap.intern_str(name)?;
        self.try_call_dunder(obj, "__getattr__", &[name_val], chunk)
    }

    pub(crate) fn handle_load_attr(&mut self, name_idx: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        // Borrow, don't clone, `chunk` outlives every `&mut self` call below.
        let Some(name) = chunk.names.get(name_idx as usize) else { return Err(VmErr::Runtime("LoadAttr: bad name index")) };
        let obj = self.pop()?;
        self.load_attr(obj, name, chunk)
    }

    /* Pushes `obj.name`, shared by LoadAttr and class patterns. */
    pub(crate) fn load_attr(&mut self, obj: Val, name: &str, chunk: &SSAChunk) -> Result<(), VmErr> {
        let lookup = match self.resolve_attr(obj, name) {
            Ok(l) => l,
            Err(VmErr::Attribute(msg)) => {
                if let Some(v) = self.try_getattr_fallback(obj, name, chunk)? {
                    self.push(v);
                    return Ok(());
                }
                return Err(VmErr::Attribute(msg));
            }
            Err(other) => return Err(other),
        };
        // Bound values materialise as a heap object, a property or alias value runs its function.
        let made = match lookup {
            AttrLookup::ModuleAttr(v) | AttrLookup::ClassMember(v) | AttrLookup::InstanceField(v) => { self.push(v); return Ok(()); }
            AttrLookup::Builtin(name) => { let v = self.builtin_value(name)?; self.push(v); return Ok(()); }
            AttrLookup::InstanceMethod { recv, func, class } => HeapObj::BoundUserMethod(recv, func, class),
            AttrLookup::BuiltinMethod(id) => HeapObj::BoundMethod(obj, id),
            AttrLookup::BoundBuiltin(recv, id) => HeapObj::BoundMethod(recv, id),
            AttrLookup::UnboundMethod(id) => HeapObj::BoundMethod(Val::undef(), id),
            AttrLookup::ExcArgs(args) => HeapObj::Tuple(args),
            AttrLookup::PropertySetterRef(prop) => HeapObj::PropertySetter(prop),
            AttrLookup::Name(s) => HeapObj::Str(s),
            AttrLookup::TypeOf(s) => HeapObj::Type(s),
            // Writes through the dict skip the stores that note hidden methods, so sites check again.
            AttrLookup::Dict(attrs) => { self.shadowed = true; HeapObj::Dict(attrs) }
            AttrLookup::PropertyGet { recv, getter } => {
                // Inline getter call, matches `BoundUserMethod` dispatch (push func, push self, call).
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.push(getter);
                self.push(recv);
                return self.exec_call(1, chunk);
            }
            AttrLookup::Thunk(f) => {
                self.push(f);
                return self.exec_call(0, chunk);
            }
        };
        let v = self.heap.alloc(made)?;
        self.push(v);
        Ok(())
    }
}
