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
    // `str.lower` on the type, the call takes its receiver as the first argument, an operator dunder keeping the type it was read off.
    UnboundMethod(BuiltinMethodId, Val),
    // `e.args` on ExcInstance, caller picks between LoadAttr materialising the tuple and CallMethod erroring.
    ExcArgs(Vec<Val>),
    // Property descriptor on an instance, `LoadAttr` invokes `getter(recv)`.
    PropertyGet { recv: Val, getter: Val },
    // `prop.setter` and its siblings, `LoadAttr` makes the `PropertyPart` of that slot.
    PropertyPart(Val, u8),
    // `__name__` on a function, type, or class, and `LoadAttr` materialises the str.
    Name(String),
    // A tuple of names, such as `co_varnames`.
    Names(alloc::vec::Vec<String>),
    // `x.__class__` of a builtin value, `LoadAttr` materialises the type named here.
    TypeOf(String),
    // `X.__value__` of a type alias, `LoadAttr` calls the zero-argument function that evaluates it.
    Thunk(Val),
    // `obj.__dict__`, the attributes of the instance as a dict that shares them.
    Dict(Rc<RefCell<DictMap>>),
    // `__mro__` of a class or type, `LoadAttr` linearizes it with the builtin parents.
    Mro(Val),
    // `__bases__` of a class or type, `LoadAttr` makes the tuple.
    Bases(Val),
    // `__dict__` of a class or type, `LoadAttr` copies the members into a dict.
    Members(Val),
    // `__annotations__` of a function or class, `LoadAttr` calls its `__annotate__` once and keeps the dict.
    Annotations(Val),
    // `f.__code__`, `LoadAttr` makes the code object of function `fi`.
    Code(usize),
    // `f.__kwdefaults__`, `LoadAttr` makes the dict of each keyword-only name and its default.
    KwDefaults(Vec<(String, Val)>),
    // `range.start` and its siblings, `LoadAttr` makes the int.
    Int(i64),
    // A member whose class defines `__get__`, `LoadAttr` calls it with the instance, None through the class, and the owner.
    DescGet { desc: Val, recv: Val, owner: Val },
}

/* The builtin types a class may derive from, whose instances are values of that type the class marks. */
pub(crate) const NATIVE_BASES: [&str; 9] = ["dict", "list", "set", "frozenset", "str", "bytes", "tuple", "int", "float"];

impl<'a> VM<'a> {
    // The cached C3 linearization of `cls`, or `[cls]` when uncached (native classes, or an inconsistent hierarchy that `c3_merge` declined to cache).
    pub(crate) fn mro_of(&self, c: Val) -> alloc::vec::Vec<Val> {
        match self.mro_cache.get(&c.0) {
            Some(r) => (**r).clone(),
            None => alloc::vec![c],
        }
    }

    /* C3 merge of the bases' linearizations plus the bases list itself, the tail of `L[cls] = cls :: merge(...)`. `cls` is prepended by the caller (it isn't allocated yet at validation time). Errs on an inconsistent hierarchy, matching Python's `TypeError` at class creation. */
    pub(crate) fn c3_merge(&self, bases: &[Val]) -> Result<alloc::vec::Vec<Val>, VmErr> {
        let mut seqs: alloc::vec::Vec<alloc::vec::Vec<Val>> = bases.iter().map(|&b| self.mro_of(b)).collect();
        if !bases.is_empty() { seqs.push(bases.to_vec()); }
        c3(seqs).map_err(|heads| self.mro_conflict(&heads))
    }

    /* The error of a hierarchy C3 cannot order, naming once each class the merge was left choosing between. */
    #[cold]
    fn mro_conflict(&self, heads: &[Val]) -> VmErr {
        let mut names: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
        for &h in heads { if let Some(HeapObj::Class(n, ..) | HeapObj::Type(n)) = self.heap.try_get(h) && !names.contains(&n.as_str()) { names.push(n); } }
        VmErr::TypeMsg(crate::s!("Cannot create a consistent method resolution order (MRO) for bases ", str &names.join(", ")))
    }

    /* The bases a class or type lists, `object` for a class listing none and for a builtin type without a parent. */
    fn direct_bases(&mut self, cls: Val) -> Result<alloc::vec::Vec<Val>, VmErr> {
        let (listed, parents) = match self.heap.try_get(cls) {
            Some(HeapObj::Class(_, bases, _)) => (bases.clone(), alloc::vec::Vec::new()),
            Some(HeapObj::Type(n)) if n == "object" => return Ok(alloc::vec::Vec::new()),
            Some(HeapObj::Type(n)) => (alloc::vec::Vec::new(), crate::vm::globals::type_parents(n)),
            _ => (alloc::vec::Vec::new(), alloc::vec::Vec::new()),
        };
        if !listed.is_empty() { return Ok(listed); }
        if parents.is_empty() { return Ok(alloc::vec![self.builtin_value("object")?]); }
        parents.into_iter().map(|p| self.builtin_value(p)).collect()
    }

    /* Linearization of `cls` through builtin parents too, `object` left out so the caller puts it last. */
    fn linearize(&mut self, cls: Val) -> Result<alloc::vec::Vec<Val>, VmErr> {
        let object = self.builtin_value("object")?;
        self.linearize_in(cls, object, &mut crate::util::hash::FxHashMap::default())
    }

    // Each class is linearized once per call, so a diamond shared by many paths costs no more than one.
    fn linearize_in(&mut self, cls: Val, object: Val, done: &mut crate::util::hash::FxHashMap<u64, alloc::vec::Vec<Val>>) -> Result<alloc::vec::Vec<Val>, VmErr> {
        if cls.0 == object.0 { return Ok(alloc::vec::Vec::new()); }
        if let Some(l) = done.get(&cls.0) { return Ok(l.clone()); }
        let bases: alloc::vec::Vec<Val> = self.direct_bases(cls)?.into_iter().filter(|b| b.0 != object.0).collect();
        let mut seqs = alloc::vec::Vec::with_capacity(bases.len() + 1);
        for &b in &bases { seqs.push(self.linearize_in(b, object, done)?); }
        if !bases.is_empty() { seqs.push(bases); }
        let mut out = alloc::vec![cls];
        out.extend(c3(seqs).map_err(|heads| self.mro_conflict(&heads))?);
        done.insert(cls.0, out.clone());
        Ok(out)
    }
}

/* C3 merge of linearizations, each step takes the first head found in no tail, Err with the heads when none is. */
fn c3(mut seqs: alloc::vec::Vec<alloc::vec::Vec<Val>>) -> Result<alloc::vec::Vec<Val>, alloc::vec::Vec<Val>> {
    let mut out = alloc::vec::Vec::new();
    loop {
        seqs.retain(|s| !s.is_empty());
        if seqs.is_empty() { break; }
        // A valid head appears in no tail, so the first such across the sequences comes next.
        let mut head = None;
        for s in &seqs {
            let h = s[0];
            let in_tail = seqs.iter().any(|t| t.len() > 1 && t[1..].iter().any(|&x| x.0 == h.0));
            if !in_tail { head = Some(h); break; }
        }
        let Some(h) = head else { return Err(seqs.iter().map(|s| s[0]).collect()) };
        out.push(h);
        for s in &mut seqs { s.retain(|&x| x.0 != h.0); }
    }
    Ok(out)
}

impl<'a> VM<'a> {
    /* Bind a resolved MRO member `mv` to `recv`, mapping Property to getter, staticmethod to unbound, function to descriptor-bound. Plain data is returned as-is. Guards is_heap before heap.get so a non-heap data member is never read as a pointer. */
    /* Whether a class member is a method or property, which an instance attribute could hide. */
    pub(crate) fn hides(&self, member: Val) -> bool {
        matches!(self.heap.try_get(member), Some(HeapObj::Func(..) | HeapObj::Property(..))) || self.has_hook(member, "__get__")
    }

    fn bind_member(&self, mv: Val, recv: Val, defining: Val) -> AttrLookup {
        if mv.is_heap() {
            match self.heap.get(mv) {
                HeapObj::Property(getter, ..) => return AttrLookup::PropertyGet { recv, getter: *getter },
                HeapObj::StaticMethod(func) => return AttrLookup::ClassMember(*func),
                // A descriptor computes the value from the instance and its class.
                HeapObj::Instance(..) if self.has_hook(mv, "__get__") => return AttrLookup::DescGet { desc: mv, recv, owner: self.instance_class(recv).unwrap_or(defining) },
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
        self.builtin_base(cls, &|n| crate::vm::globals::is_exc_type(n)).and_then(|t| match self.heap.try_get(t) { Some(HeapObj::Type(n)) => Some(n.as_str()), _ => None })
    }

    /* The builtin type whose values a class makes, a container, text or number, None for a plain class. */
    pub(crate) fn native_base(&self, cls: Val) -> Option<Val> { self.builtin_base(cls, &|n| NATIVE_BASES.contains(&n)) }

    /* The first builtin type `pick` takes in the order of `cls`, read off the cached order so a diamond is walked once. */
    fn builtin_base(&self, cls: Val, pick: &dyn Fn(&str) -> bool) -> Option<Val> {
        let is = |c: Val| matches!(self.heap.try_get(c), Some(HeapObj::Type(n)) if pick(n));
        match self.heap.try_get(cls)? {
            HeapObj::Type(_) => is(cls).then_some(cls),
            HeapObj::Class(_, bases, _) if bases.is_empty() => None,
            HeapObj::Class(_, bases, _) => match self.mro_cache.get(&cls.0) {
                Some(mro) => mro.iter().copied().find(|&c| is(c)),
                None => bases.iter().find_map(|&b| self.builtin_base(b, pick)),
            },
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
            // A number of a class over int or float runs the methods of the number.
            return lookup_method(self.type_name(obj), name).map(|id| AttrLookup::BoundBuiltin(crate::value::unboxed(obj, &self.heap), id))
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
                _ => self.chain_attr(obj, name).map(AttrLookup::ClassMember).or_else(|| self.group_attr(obj, name))
                    .or_else(|| lookup_method("BaseException", name).map(AttrLookup::BuiltinMethod)).ok_or_else(missing),
            },
            // Bound methods expose their receiver, user methods also their function.
            Some(&HeapObj::BoundUserMethod(recv, _, _)) if name == "__self__" => return Ok(AttrLookup::ClassMember(recv)),
            Some(&HeapObj::BoundUserMethod(_, func, _)) if name == "__func__" => return Ok(AttrLookup::ClassMember(func)),
            Some(&HeapObj::StaticMethod(func) | &HeapObj::ClassMethod(func)) if matches!(name, "__func__" | "__wrapped__") => return Ok(AttrLookup::ClassMember(func)),
            Some(&HeapObj::BoundMethod(recv, id)) if name == "__self__" && !self.method_unbound(recv, id) => return Ok(AttrLookup::ClassMember(recv)),
            // A builtin function or method is named by its id.
            Some(&HeapObj::NativeFn(id)) if matches!(name, "__name__" | "__qualname__") => return Ok(AttrLookup::Name(id.name().into())),
            // A builtin function lives in `builtins`, a method of a builtin value in no module.
            Some(HeapObj::NativeFn(_)) if name == "__module__" => return Ok(AttrLookup::Name("builtins".into())),
            Some(HeapObj::BoundMethod(..)) if name == "__module__" => return Ok(AttrLookup::ClassMember(Val::none())),
            Some(&HeapObj::BoundMethod(_, id)) if name == "__name__" => return Ok(AttrLookup::Name(id.name().into())),
            // A dunder of a value is named by the type of the value, any other method by its group.
            Some(&HeapObj::BoundMethod(recv, id)) if name == "__qualname__" => {
                let owner = if self.method_unbound(recv, id) { self.method_owner(recv, id) } else if id.ty() == "op" { self.type_name(recv) } else { id.ty() };
                return Ok(AttrLookup::Name(crate::s!(str owner, ".", str id.name())));
            }
            // Function attributes, a stored attr wins over the derived `__name__`.
            Some(HeapObj::Func(fi, _, _, attrs)) => {
                if let Some(&(_, v)) = attrs.borrow().iter().find(|(n, _)| n == name) { return Ok(AttrLookup::ClassMember(v)); }
                if name == "__name__" && let Some(n) = self.function_names.get(*fi) { return Ok(AttrLookup::Name(if n.is_empty() { "<lambda>".into() } else { n.clone() })); }
                if name == "__qualname__" && let Some(n) = self.function_qualnames.get(*fi) { return Ok(AttrLookup::Name(n.clone())); }
                if name == "__annotations__" { return Ok(AttrLookup::Annotations(obj)); }
                if name == "__annotate__" { return Ok(AttrLookup::ClassMember(Val::none())); }
                if name == "__code__" { return Ok(AttrLookup::Code(*fi)); }
                if let Some(found) = self.fn_attr(obj, name) { return Ok(found); }
            }
            // A closure cell holds one value, and reading an unfilled one fails.
            Some(&HeapObj::Cell(c)) if name == "cell_contents" => return if c.is_undef() { Err(cold_value("Cell is empty")) } else { Ok(AttrLookup::ClassMember(c)) },
            Some(&HeapObj::FnCode(fi)) => return self.code_attr(fi, name).ok_or_else(missing),
            // Class attr, `MyClass.method` returns the unbound function (no `self` prepended).
            Some(HeapObj::Class(cls_name, _, _)) => {
                if name == "__name__" { return Ok(AttrLookup::Name(cls_name.clone())); }
                // A docstring belongs to its own class, never inherited.
                if name == "__doc__" { return Ok(AttrLookup::ClassMember(self.own_member(obj, name).unwrap_or(Val::none()))); }
                if name == "__mro__" { return Ok(AttrLookup::Mro(obj)); }
                if name == "__bases__" { return Ok(AttrLookup::Bases(obj)); }
                if name == "__dict__" { return Ok(AttrLookup::Members(obj)); }
                // A class has the annotations of its own body, never those of a base.
                if name == "__annotations__" { return Ok(AttrLookup::Annotations(obj)); }
                if name == "__annotate__" { return Ok(AttrLookup::ClassMember(self.own_member(obj, name).unwrap_or(Val::none()))); }
                if let Some((v, defining)) = self.lookup_class_member(obj, name) {
                    return Ok(match self.heap.try_get(v) {
                        // `staticmethod` accessed on the class itself unwraps to the plain function.
                        Some(&HeapObj::StaticMethod(func)) => AttrLookup::ClassMember(func),
                        // `classmethod` binds the accessed class, derived included.
                        Some(&HeapObj::ClassMethod(func)) => AttrLookup::InstanceMethod { recv: obj, func, class: defining },
                        // `__init_subclass__` is a classmethod without saying so.
                        Some(HeapObj::Func(..)) if name == "__init_subclass__" => AttrLookup::InstanceMethod { recv: obj, func: v, class: defining },
                        // A descriptor read through the class gets None for the instance.
                        Some(HeapObj::Instance(..)) if self.has_hook(v, "__get__") => AttrLookup::DescGet { desc: v, recv: Val::none(), owner: obj },
                        _ => AttrLookup::ClassMember(v),
                    });
                }
                // A class defining `__eq__` alone has no hash.
                if name == "__hash__" && self.hash_hook(obj) == Some(None) { return Ok(AttrLookup::ClassMember(Val::none())); }
                // A class reads the `object` methods unbound, `__init_subclass__` bound to itself.
                if name.starts_with("__") && let Some(id) = lookup_method("object", name) { return Ok(if name == "__init_subclass__" { AttrLookup::BoundBuiltin(obj, id) } else { AttrLookup::UnboundMethod(id, Val::undef()) }); }
                if name == "__qualname__" { return Ok(AttrLookup::Name(self.class_qualnames.get(&obj.0).unwrap_or(cls_name).clone())); }
                if name == "__module__" { return Ok(AttrLookup::Name("__main__".into())); }
                if name == "__class__" { return Ok(AttrLookup::TypeOf("type".into())); }
                return Err(VmErr::Attribute(s!("type object '", str cls_name, "' has no attribute '", str name, "'")));
            }
            // Instance attribute lookup, check `__dict__` first, then the class chain (direct + bases).
            Some(HeapObj::Instance(cls_val, attrs)) => {
                let found = attrs.borrow().iter()
                    .find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name))
                    .map(|(_, v)| v);
                // A property, or a class `args` of an exception, outranks a field of the same name.
                let hidden = || (self.shadowed && self.class_data_descriptor(*cls_val, name)) || (name == "args" && self.exc_base(*cls_val).is_some() && self.lookup_class_member(*cls_val, name).is_some());
                if let Some(v) = found && !hidden() { return Ok(AttrLookup::InstanceField(v)); }
                if name == "__dict__" && self.exc_base(*cls_val).is_none() && self.class_slots(*cls_val).is_none() { return Ok(AttrLookup::Dict(attrs.clone())); }
                // An instance reads the docstring of its own class, which a base never passes on.
                if name == "__doc__" { return Ok(AttrLookup::ClassMember(self.own_member(*cls_val, name).unwrap_or(Val::none()))); }
                if let Some((mv, defining)) = self.lookup_class_member(*cls_val, name) { return Ok(self.bind_member(mv, obj, defining)); }
                // An exception falls back to the `BaseException` methods, a group to its own first.
                if let Some(l) = self.group_attr(obj, name) { return Ok(l); }
                if self.exc_base(*cls_val).is_some() && let Some(id) = lookup_method("BaseException", name) { return Ok(AttrLookup::BuiltinMethod(id)); }
                if self.exc_base(*cls_val).is_some() && let Some(v) = self.chain_attr(obj, name) { return Ok(AttrLookup::ClassMember(v)); }
                if name == "__class__" { return Ok(AttrLookup::ClassMember(*cls_val)); }
                if name == "__hash__" && self.hash_hook(*cls_val) == Some(None) { return Ok(AttrLookup::ClassMember(Val::none())); }
                // Last comes `object`, its hooks bound to the instance or for `__init_subclass__` to the class.
                if name.starts_with("__") && let Some(id) = lookup_method("object", name) { return Ok(match name { "__new__" => AttrLookup::UnboundMethod(id, Val::undef()), "__init_subclass__" => AttrLookup::BoundBuiltin(*cls_val, id), _ => AttrLookup::BoundBuiltin(obj, id) }); }
                return Err(missing());
            }
            // `super().<name>` searches strictly above the proxy's stored class, and methods bind to the proxy's `recv`.
            Some(&HeapObj::Super(cls_val, recv)) => {
                // Its own fields, and an unbound super reaches nothing past them.
                match name {
                    "__thisclass__" => return Ok(AttrLookup::ClassMember(cls_val)),
                    "__self__" => return Ok(AttrLookup::ClassMember(recv)),
                    "__self_class__" if recv.is_none() => return Ok(AttrLookup::ClassMember(Val::none())),
                    "__self_class__" => return Ok(AttrLookup::ClassMember(self.instance_class(recv).unwrap_or(recv))),
                    "__get__" => if let Some(id) = lookup_method("super", name) { return Ok(AttrLookup::BuiltinMethod(id)); },
                    _ => {}
                }
                if recv.is_none() { return Err(VmErr::Attribute(s!("'super' object has no attribute '", str name, "'"))); }
                // C3 super walks the *instance type*'s MRO past the defining class, not just the defining class's bases.
                let derived = self.instance_class(recv).or_else(|| matches!(self.heap.try_get(recv), Some(HeapObj::Class(..))).then_some(recv)).unwrap_or(cls_val);
                if let Some((mv, defining)) = self.lookup_class_member_after(derived, cls_val, name) { return Ok(self.bind_member(mv, recv, defining)); }
                // Past the user classes of a builtin value sit the methods and operators of its type.
                if self.heap.classed(recv).is_some() {
                    let ty = self.type_name(recv);
                    let op = || crate::vm::methods::operator::applies(ty, name).then(|| lookup_method("op", name)).flatten();
                    if let Some(id) = lookup_method(ty, name).or_else(op) { return Ok(AttrLookup::BoundBuiltin(recv, id)); }
                }
                // Past the user classes of an exception sits `BaseException`.
                if self.exc_base(derived).is_some() && let Some(id) = lookup_method("BaseException", name) { return Ok(AttrLookup::BoundBuiltin(recv, id)); }
                // Last comes `object`, its `__new__` static so the class goes in explicitly.
                if name.starts_with("__") && let Some(id) = lookup_method("object", name) { return Ok(if name == "__new__" { AttrLookup::UnboundMethod(id, Val::undef()) } else { AttrLookup::BoundBuiltin(recv, id) }); }
                return Err(VmErr::Attribute(s!("'super' object has no attribute '", str name, "'")));
            }
            // Every builtin iterator shares `__next__` and `__iter__`.
            Some(HeapObj::Iter(..)) => return lookup_method("iterator", name).map(AttrLookup::BuiltinMethod).ok_or_else(missing),
            // `prop.setter` produces a callable that re-builds the property with a new setter (powers `@x.setter`).
            Some(&HeapObj::Property(g, s, d)) => return match name {
                "getter" => Ok(AttrLookup::PropertyPart(obj, 0)),
                "setter" => Ok(AttrLookup::PropertyPart(obj, 1)),
                "deleter" => Ok(AttrLookup::PropertyPart(obj, 2)),
                "fget" => Ok(AttrLookup::ClassMember(g)),
                "fset" => Ok(AttrLookup::ClassMember(s)),
                "fdel" => Ok(AttrLookup::ClassMember(d)),
                _ => Err(missing()),
            },
            // A method off a builtin type stays unbound, a classmethod like `dict.fromkeys` binds the type.
            Some(HeapObj::Type(n)) => {
                if matches!(name, "__name__" | "__qualname__") { return Ok(AttrLookup::Name(n.clone())); }
                if name == "__module__" { return Ok(AttrLookup::Name("builtins".into())); }
                if name == "__mro__" { return Ok(AttrLookup::Mro(obj)); }
                if name == "__bases__" { return Ok(AttrLookup::Bases(obj)); }
                if name == "__dict__" { return Ok(AttrLookup::Members(obj)); }
                // `int.__new__(cls, v)` builds a value of the type, marked with `cls` when it derives from it.
                if name == "__new__" && let Some(id) = lookup_method("object", name) { return Ok(AttrLookup::UnboundMethod(id, Val::undef())); }
                // `Exception.__init__(self, msg)` reaches the shared exception methods.
                let owner = if crate::vm::globals::is_exc_type(n) { "BaseException" } else { n.as_str() };
                let op = || crate::vm::methods::operator::applies(n, name).then(|| lookup_method("op", name)).flatten();
                if let Some(id) = lookup_method(owner, name).or_else(op) {
                    let classmethod = matches!(name, "fromkeys" | "fromhex" | "from_bytes" | "maketrans" | "from_number");
                    // The operator dunders are shared by every type, so the unbound one keeps the type for its repr.
                    let of = if id.ty() == "op" { obj } else { Val::undef() };
                    return Ok(if classmethod { AttrLookup::BuiltinMethod(id) } else { AttrLookup::UnboundMethod(id, of) });
                }
            }
            // Plain fields, `slice.start`, `.stop` and `.step`, an alias `__origin__` and `__args__`, and the lazy `__value__`.
            Some(&HeapObj::Range(s, e, st)) if matches!(name, "start" | "stop" | "step") => return Ok(AttrLookup::Int(match name { "start" => s, "stop" => e, _ => st })),
            Some(&HeapObj::BigRange(s, e, st)) if matches!(name, "start" | "stop" | "step") => return Ok(AttrLookup::ClassMember(match name { "start" => s, "stop" => e, _ => st })),
            Some(&HeapObj::Slice(v, _, _)) if name == "start" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::Slice(_, v, _)) if name == "stop" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::Slice(_, _, v)) if name == "step" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::GenericAlias(v, _)) if name == "__origin__" => return Ok(AttrLookup::ClassMember(v)),
            Some(&HeapObj::GenericAlias(_, v) | &HeapObj::Union(v)) if name == "__args__" => return Ok(AttrLookup::ClassMember(v)),
            Some(HeapObj::TypeVar(n)) if name == "__name__" => return Ok(AttrLookup::Name(n.trim_start_matches('*').into())),
            Some(HeapObj::TypeAlias(q, _)) if name == "__name__" => return Ok(AttrLookup::Name(q.rsplit('.').next().unwrap_or(q).into())),
            Some(HeapObj::TypeAlias(q, _)) if name == "__qualname__" => return Ok(AttrLookup::Name(q.clone())),
            Some(&HeapObj::TypeAlias(_, f)) if name == "__value__" => return Ok(AttrLookup::Thunk(f)),
            _ => {}
        }
        if name == "__class__" { return Ok(AttrLookup::TypeOf(self.type_name(obj).into())); }
        if let Some(v) = crate::vm::methods::operator::number_attr(self, obj, name) { return Ok(AttrLookup::ClassMember(v)); }
        if let Some(s) = crate::frontend::sentinel::get(&self.heap, obj) {
            match name { "__name__" => return Ok(AttrLookup::Name(s.name.clone())), "__module__" => return Ok(AttrLookup::Name("__main__".into())), _ => {} }
        }
        if let Some(r) = self.view_attr(obj, name) { return r; }
        // A mutable container has no hash, its `__hash__` is None.
        if name == "__hash__" && matches!(self.type_name(obj), "list" | "dict" | "set") { return Ok(AttrLookup::ClassMember(Val::none())); }
        // Builtin type method, then the dunders its operators answer.
        let ty = self.type_name(obj);
        // A bool answers the methods of int, the type it derives from.
        lookup_method(ty, name).or_else(|| (ty == "bool").then(|| lookup_method("int", name)).flatten())
            .or_else(|| crate::vm::methods::operator::applies(ty, name).then(|| lookup_method("op", name)).flatten())
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
                self.note_missing_attr(obj, name);
                return Err(VmErr::Attribute(msg));
            }
            Err(other) => return Err(other),
        };
        self.push_lookup(lookup, obj, chunk)
    }

    /* Pushes the value `lookup` names for `obj`, a bound method made, a property or alias run. */
    #[inline]
    pub(crate) fn push_lookup(&mut self, lookup: AttrLookup, obj: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        let made = match lookup {
            AttrLookup::ModuleAttr(v) | AttrLookup::ClassMember(v) | AttrLookup::InstanceField(v) => { self.push(v); return Ok(()); }
            AttrLookup::Builtin(name) => { let v = self.builtin_value(name)?; self.push(v); return Ok(()); }
            AttrLookup::InstanceMethod { recv, func, class } => HeapObj::BoundUserMethod(recv, func, class),
            AttrLookup::BuiltinMethod(id) => HeapObj::BoundMethod(obj, id),
            AttrLookup::BoundBuiltin(recv, id) => HeapObj::BoundMethod(recv, id),
            AttrLookup::UnboundMethod(id, of) => HeapObj::BoundMethod(of, id),
            AttrLookup::ExcArgs(args) => HeapObj::Tuple(args),
            AttrLookup::PropertyPart(prop, slot) => HeapObj::PropertyPart(prop, slot),
            AttrLookup::Name(s) => HeapObj::Str(s),
            AttrLookup::TypeOf(s) => HeapObj::Type(s),
            // Writes through the dict skip the stores that note hidden methods, so sites check again.
            AttrLookup::Dict(attrs) => { self.shadowed = true; HeapObj::Dict(attrs) }
            AttrLookup::Mro(cls) => {
                let mut classes = self.linearize(cls)?;
                classes.push(self.builtin_value("object")?);
                HeapObj::Tuple(classes)
            }
            AttrLookup::Bases(cls) => HeapObj::Tuple(self.direct_bases(cls)?),
            AttrLookup::Members(cls) => { let d = self.members_dict(cls)?; self.push(d); return Ok(()); }
            AttrLookup::DescGet { desc, recv, owner } => { let v = self.call_hook(desc, "__get__", &[recv, owner], chunk)?; self.push(v); return Ok(()); }
            AttrLookup::Annotations(owner) => { let v = self.annotations(owner, chunk)?; self.push(v); return Ok(()); }
            AttrLookup::Code(fi) => HeapObj::FnCode(fi),
            AttrLookup::Int(n) => { let v = self.heap.int(n as i128)?; self.push(v); return Ok(()); }
            AttrLookup::KwDefaults(pairs) => {
                let mut dm = DictMap::with_capacity(pairs.len());
                for (n, v) in pairs { let key = self.heap.intern_str(&n)?; dm.insert(key, v, &self.heap); }
                HeapObj::Dict(Rc::new(RefCell::new(dm)))
            }
            AttrLookup::Names(names) => {
                let mut items = alloc::vec::Vec::with_capacity(names.len());
                for n in names { let v = self.heap.alloc(HeapObj::Str(n))?; self.temp_roots.push(v); items.push(v); }
                self.temp_roots.truncate(self.temp_roots.len() - items.len());
                HeapObj::Tuple(items)
            }
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

impl<'a> VM<'a> {
    /* `__module__`, `__doc__`, `__defaults__`, `__kwdefaults__` and `__closure__` of user function `f`. */
    fn fn_attr(&self, f: Val, name: &str) -> Option<AttrLookup> {
        let Some(HeapObj::Func(fi, defaults, captures, _)) = self.heap.try_get(f) else { return None };
        let params = &self.functions.get(*fi)?.0;
        // Defaults line up with the parameters marked `=`, keyword-only ones marked `~` too.
        let with_default = || params.iter().filter(|p| p.ends_with('=')).zip(defaults.iter().copied());
        Some(match name {
            "__module__" => match self.fn_module_id(*fi) {
                0 => AttrLookup::Name("__main__".into()),
                m => self.scopes[m].get("__name__").map_or_else(|| AttrLookup::Name("__main__".into()), AttrLookup::ClassMember),
            },
            "__doc__" => docstring(&self.functions[*fi].1).map_or(AttrLookup::ClassMember(Val::none()), AttrLookup::Name),
            "__defaults__" => {
                let pos: Vec<Val> = with_default().filter(|(p, _)| !p.starts_with('~')).map(|(_, v)| v).collect();
                if pos.is_empty() { AttrLookup::ClassMember(Val::none()) } else { AttrLookup::ExcArgs(pos) }
            }
            "__kwdefaults__" => {
                let kw: Vec<(String, Val)> = with_default().filter(|(p, _)| p.starts_with('~')).map(|(p, v)| (String::from(crate::parser::types::param_base_name(p)), v)).collect();
                if kw.is_empty() { AttrLookup::ClassMember(Val::none()) } else { AttrLookup::KwDefaults(kw) }
            }
            "__closure__" => {
                let cells: Vec<Val> = captures.iter().map(|&(_, c)| c).filter(|&c| matches!(self.heap.try_get(c), Some(HeapObj::Cell(_)))).collect();
                if cells.is_empty() { AttrLookup::ClassMember(Val::none()) } else { AttrLookup::ExcArgs(cells) }
            }
            _ => return None,
        })
    }
}

/* The docstring a body opens with, its indentation past the first line removed as the compiler removes it. */
pub(crate) fn docstring(body: &crate::parser::SSAChunk) -> Option<String> {
    use crate::parser::{OpCode, Value};
    let [first, second, ..] = body.instructions.as_slice() else { return None };
    if first.opcode != OpCode::LoadConst || second.opcode != OpCode::PopTop { return None; }
    let Some(Value::Str(text)) = body.constants.get(first.operand as usize) else { return None };
    let mut lines = text.split('\n');
    let blank = [' ', '\t'];
    let head = lines.next().unwrap_or("").trim_start_matches(blank);
    let rest: Vec<&str> = lines.collect();
    let indent = rest.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start_matches(blank).len()).min().unwrap_or(0);
    let mut out = String::from(head);
    for l in rest {
        out.push('\n');
        out.push_str(if l.trim().is_empty() { "" } else { &l[indent.min(l.len())..] });
    }
    Some(out)
}
