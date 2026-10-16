/* The hooks a user class defines and the `object` methods they end in. */

use super::VM;
use super::types::*;
use super::opcodes::attr_lookup::AttrLookup;
use crate::parser::SSAChunk;
use crate::s;
use alloc::string::String;

impl<'a> VM<'a> {
    /* `property 'x' of 'A' object has no setter`, a store or delete the property has no function for. */
    #[cold]
    pub(crate) fn property_err(&self, obj: Val, name: &str, part: &str) -> VmErr {
        VmErr::Attribute(s!("property '", str name, "' of '", str self.type_name(obj), "' object has no ", str part))
    }

    /* Whether `v` is an instance whose class defines `hook`, such as a descriptor method. */
    pub(crate) fn has_hook(&self, v: Val, hook: &str) -> bool {
        self.instance_class(v).is_some_and(|c| self.lookup_class_member(c, hook).is_some())
    }

    /* A member that takes stores and deletes ahead of the instance dict, a property or a descriptor with `__set__` or `__delete__`. */
    pub(crate) fn data_descriptor(&self, member: Val) -> bool {
        matches!(self.heap.try_get(member), Some(HeapObj::Property(..))) || self.has_hook(member, "__set__") || self.has_hook(member, "__delete__")
    }

    /* Whether `class` reaches a data descriptor named `name`. */
    pub(crate) fn class_data_descriptor(&self, class: Val, name: &str) -> bool {
        self.lookup_class_member(class, name).is_some_and(|(m, _)| self.data_descriptor(m))
    }

    /* Calls method `hook` of instance `obj` with `args` and returns what it returned. */
    #[inline(never)]
    pub(crate) fn call_hook(&mut self, obj: Val, hook: &str, args: &[Val], chunk: &SSAChunk) -> Result<Val, VmErr> {
        let found = self.instance_class(obj).and_then(|c| self.lookup_class_member(c, hook));
        let Some((func, defining)) = found else {
            return Err(VmErr::Attribute(s!("'", str self.type_name(obj), "' object has no attribute '", str hook, "'")));
        };
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.pending.method_binding = Some((defining, obj));
        self.call_with(func, Some(obj), args, &[], chunk)?;
        self.pop()
    }

    /* A store through the class of `obj`, a property setter or descriptor `__set__`, false when the instance takes it. */
    #[inline(never)]
    pub(crate) fn store_through_class(&mut self, obj: Val, member: Val, name: &str, value: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if let Some(&HeapObj::Property(_, setter, _)) = self.heap.try_get(member) {
            if setter.is_none() { return Err(self.property_err(obj, name, "setter")); }
            if self.depth >= self.max_calls { return Err(cold_depth()); }
            self.call_with(setter, None, &[obj, value], &[], chunk)?;
            self.pop()?;
            return Ok(true);
        }
        if self.has_hook(member, "__set__") { self.call_hook(member, "__set__", &[obj, value], chunk)?; return Ok(true); }
        if self.has_hook(member, "__delete__") { return Err(VmErr::Attribute("__set__".into())); }
        Ok(false)
    }

    /* A delete through the class of `obj`, a property deleter or descriptor `__delete__`, false when the instance takes it. */
    #[inline(never)]
    pub(crate) fn delete_through_class(&mut self, obj: Val, member: Val, name: &str, chunk: &SSAChunk) -> Result<bool, VmErr> {
        if let Some(&HeapObj::Property(_, _, deleter)) = self.heap.try_get(member) {
            if deleter.is_none() { return Err(self.property_err(obj, name, "deleter")); }
            if self.depth >= self.max_calls { return Err(cold_depth()); }
            self.call_with(deleter, None, &[obj], &[], chunk)?;
            self.pop()?;
            return Ok(true);
        }
        if self.has_hook(member, "__delete__") { self.call_hook(member, "__delete__", &[obj], chunk)?; return Ok(true); }
        if self.has_hook(member, "__set__") { return Err(VmErr::Attribute("__delete__".into())); }
        Ok(false)
    }

    /* The members of a class, or the methods of a builtin type unbound, as a fresh dict. */
    #[inline(never)]
    pub(crate) fn members_dict(&mut self, cls: Val) -> Result<Val, VmErr> {
        let mut pairs: alloc::vec::Vec<(&str, Val)> = alloc::vec::Vec::new();
        let members = match self.heap.get(cls) { HeapObj::Class(_, _, m) => m.borrow().clone(), _ => alloc::vec::Vec::new() };
        let ty = match self.heap.get(cls) { HeapObj::Type(n) => Some(n.clone()), _ => None };
        // An engine name starts with `#` and stays out.
        for (n, v) in members.iter().filter(|(n, _)| !n.starts_with('#')) { pairs.push((n, *v)); }
        let mut dm = DictMap::with_capacity(pairs.len());
        for (n, v) in pairs {
            let key = self.heap.intern_str(n)?;
            dm.insert(key, v, &self.heap);
        }
        if let Some(n) = ty {
            for id in crate::vm::methods::methods_of(&n) {
                let (key, v) = (self.heap.intern_str(id.name())?, self.heap.alloc(HeapObj::BoundMethod(Val::undef(), id))?);
                dm.insert(key, v, &self.heap);
            }
        }
        self.heap.alloc(HeapObj::Dict(alloc::rc::Rc::new(core::cell::RefCell::new(dm))))
    }

    /* A method of `object` that `super()` or `object.name(...)` reaches, `recv` the instance or class it acts on. */
    #[inline(never)]
    pub(crate) fn object_method(&mut self, recv: Val, name: &str, pos: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        if !kw.is_empty() {
            let owner = match self.heap.try_get(recv) { Some(HeapObj::Class(n, ..)) => n.clone(), _ => String::from(self.type_name(recv)) };
            return Err(VmErr::TypeMsg(s!(str &owner, ".", str name, "() takes no keyword arguments")));
        }
        let v = match name {
            "__init_subclass__" => Val::none(),
            "__new__" => self.object_new(recv, pos, chunk)?,
            "__init__" if !pos.is_empty() => return Err(cold_type("object.__init__() takes exactly one argument (the instance to initialize)")),
            "__init__" => Val::none(),
            "__repr__" => { let text = self.display(recv); self.heap.alloc(HeapObj::Str(text))? }
            "__str__" => { let text = self.repr_op(recv, chunk)?; self.heap.alloc(HeapObj::Str(text))? }
            "__eq__" => if recv.0 == pos[0].0 { Val::bool(true) } else { self.not_implemented()? },
            "__ne__" => match self.try_call_dunder(recv, "__eq__", &[pos[0]], chunk)? {
                Some(r) => Val::bool(!self.truthy(r)),
                None if recv.0 == pos[0].0 => Val::bool(false),
                None => self.not_implemented()?,
            },
            "__hash__" => { let h = crate::vm::globals::identity::identity_hash(recv); self.int_to_val(Some(h as i128))? }
            "__format__" => {
                if !matches!(self.heap.try_get(pos[0]), Some(HeapObj::Str(s)) if s.is_empty()) {
                    return Err(VmErr::TypeMsg(s!("unsupported format string passed to ", str self.type_name(recv), ".__format__")));
                }
                let text = self.display_op(recv, chunk)?;
                self.heap.alloc(HeapObj::Str(text))?
            }
            "__setattr__" => {
                let attr = self.str_of(pos[0], "attribute name must be string")?;
                self.store_attr_raw(recv, &attr, pos[1], chunk)?;
                Val::none()
            }
            "__delattr__" => {
                let attr = self.str_of(pos[0], "attribute name must be string")?;
                self.delete_attr_raw(recv, &attr, chunk)?;
                Val::none()
            }
            "__getattribute__" => {
                let attr = self.str_of(pos[0], "attribute name must be string")?;
                let lookup = self.resolve_attr(recv, &attr)?;
                return self.push_lookup(lookup, recv, chunk);
            }
            _ => return Err(VmErr::Attribute(s!("type object 'object' has no attribute '", str name, "'"))),
        };
        self.push(v);
        Ok(())
    }

    /* Whether instances of `cls` need the general constructor, built by a `__new__` of its own or held to `__slots__`. */
    #[inline(never)]
    pub(crate) fn special_class(&self, cls: Val) -> bool {
        self.lookup_class_member(cls, "__new__").is_some() || self.class_slots(cls).is_some()
    }

    /* The attribute names instances of `cls` may hold, None when some class on its order keeps a `__dict__`. */
    #[inline(never)]
    pub(crate) fn class_slots(&self, cls: Val) -> Option<alloc::vec::Vec<String>> {
        if !self.slotted { return None; }
        let order = self.mro_cache.get(&cls.0)?.clone();
        let mut names = alloc::vec::Vec::new();
        for &c in order.iter() {
            let HeapObj::Class(_, _, members) = self.heap.get(c) else { continue };
            let slots = members.borrow().iter().find(|(n, _)| n == "__slots__").map(|&(_, v)| v)?;
            for n in self.slot_names(slots) {
                if n == "__dict__" { return None; }
                names.push(n);
            }
        }
        Some(names)
    }

    /* The names a `__slots__` value lists, one string or an iterable of them. */
    pub(crate) fn slot_names(&self, slots: Val) -> alloc::vec::Vec<String> {
        let text = |v: Val| match self.heap.try_get(v) { Some(HeapObj::Str(s)) => Some(s.clone()), _ => None };
        match self.heap.try_get(slots) {
            Some(HeapObj::Str(s)) => alloc::vec![s.clone()],
            Some(HeapObj::Tuple(items)) => items.iter().filter_map(|&v| text(v)).collect(),
            Some(HeapObj::List(items)) => items.borrow().iter().filter_map(|&v| text(v)).collect(),
            Some(HeapObj::Dict(d) | HeapObj::FrozenDict(d)) => d.borrow().keys().filter_map(text).collect(),
            _ => alloc::vec::Vec::new(),
        }
    }

    /* Refuses a store of `name` on `obj` when its class holds instances to `__slots__` without it. */
    #[inline]
    pub(crate) fn check_slot(&self, obj: Val, name: &str) -> Result<(), VmErr> {
        if !self.slotted { return Ok(()); }
        self.check_slot_named(obj, name)
    }

    #[inline(never)]
    fn check_slot_named(&self, obj: Val, name: &str) -> Result<(), VmErr> {
        let Some(&HeapObj::Instance(cls, _)) = self.heap.try_get(obj) else { return Ok(()) };
        match self.class_slots(cls) {
            Some(names) if !names.iter().any(|n| n == name) => Err(VmErr::Attribute(s!("'", str self.type_name(obj), "' object has no attribute '", str name, "' and no __dict__ for setting new attributes"))),
            _ => Ok(()),
        }
    }

    /* `cls(*args)` for a special class, `__new__` makes the object and `__init__` runs when it is an instance of `cls`. */
    #[inline(never)]
    pub(crate) fn construct_special(&mut self, cls: Val, init: Option<(Val, Val)>, pos: &[Val], kw: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        let obj = match self.lookup_class_member(cls, "__new__") {
            Some((new_fn, defining)) => {
                let new_fn = match self.heap.try_get(new_fn) { Some(&HeapObj::StaticMethod(f)) => f, _ => new_fn };
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                let mut args = alloc::vec::Vec::with_capacity(pos.len() + 1);
                args.push(cls);
                args.extend_from_slice(pos);
                self.pending.method_binding = Some((defining, cls));
                self.call_with(new_fn, None, &args, kw, chunk)?;
                self.pop()?
            }
            None => self.object_new(cls, &[], chunk)?,
        };
        // `__init__` runs only on what `__new__` made of this class.
        let made = self.instance_class(obj).is_some_and(|c| self.heap.is_subclass(c, cls));
        if made && let Some(init) = self.instance_class(obj).and_then(|c| self.lookup_class_member(c, "__init__")).or(init) {
            self.with_roots([obj], |vm| {
                if vm.depth >= vm.max_calls { return Err(cold_depth()); }
                vm.pending.method_binding = Some((init.1, obj));
                vm.call_with(init.0, Some(obj), pos, kw, chunk)?;
                vm.pop()
            })?;
        }
        self.push(obj);
        Ok(())
    }

    /* `object.__new__(cls)`, a bare instance of `cls`, extra arguments refused unless `__init__` takes them. */
    #[inline(never)]
    pub(crate) fn object_new(&mut self, cls: Val, extra: &[Val], chunk: &SSAChunk) -> Result<Val, VmErr> {
        let name = match self.heap.try_get(cls) {
            Some(HeapObj::Class(n, ..)) => n.clone(),
            Some(HeapObj::Type(n)) if n == "object" => n.clone(),
            // `int.__new__(int, v)` is the plain builtin call.
            Some(HeapObj::Type(n)) if super::opcodes::attr_lookup::NATIVE_BASES.contains(&n.as_str()) => {
                self.call_with(cls, None, extra, &[], chunk)?;
                return self.pop();
            }
            Some(HeapObj::Type(n)) => return Err(VmErr::TypeMsg(s!("object.__new__(", str n, ") is not safe, use ", str n, ".__new__()"))),
            _ => return Err(VmErr::TypeMsg(s!("object.__new__(X): X is not a type object (", str self.type_name(cls), ")"))),
        };
        // A class over a builtin type takes its value from the arguments.
        if let Some(native) = self.native_base(cls) {
            self.new_classed(cls, native, None, extra, &[], chunk)?;
            return self.pop();
        }
        // An exception keeps what it gets as `args`.
        if !extra.is_empty() && self.exc_base(cls).is_none() {
            if self.lookup_class_member(cls, "__new__").is_some() { return Err(cold_type("object.__new__() takes exactly one argument (the type to instantiate)")); }
            if self.lookup_class_member(cls, "__init__").is_none() { return Err(VmErr::TypeMsg(s!(str &name, "() takes no arguments"))); }
        }
        if matches!(self.heap.get(cls), HeapObj::Type(_)) {
            return self.heap.alloc(HeapObj::Instance(cls, alloc::rc::Rc::new(core::cell::RefCell::new(DictMap::new()))));
        }
        let obj = self.new_instance(cls, 0)?;
        if self.exc_base(cls).is_some() { self.set_exc_args(obj, extra.to_vec())?; }
        // A slotted instance never takes a shape, so no store site appends a name it lacks.
        if self.class_slots(cls).is_some() && let HeapObj::Instance(_, attrs) = self.heap.get(obj) { attrs.borrow_mut().set_shape(crate::value::NO_SHAPE); }
        Ok(obj)
    }

    /* The `__hash__` instances of `cls` answer with, None for the identity hash and Some(None) when they are unhashable. */
    pub(crate) fn hash_hook(&self, cls: Val) -> Option<Option<Val>> {
        let order = self.mro_cache.get(&cls.0)?;
        for &c in order.iter() {
            let Some(HeapObj::Class(_, _, m)) = self.heap.try_get(c) else { continue };
            let m = m.borrow();
            if let Some(&(_, h)) = m.iter().find(|(n, _)| n == "__hash__") { return Some((!h.is_none()).then_some(h)); }
            // A class defining `__eq__` alone leaves its instances unhashable.
            if m.iter().any(|(n, _)| n == "__eq__") { return Some(None); }
        }
        None
    }

    /* Asks the user `__hash__` of `v`, or of the instances inside a tuple `v`, before a table probes with it. */
    #[inline]
    pub(crate) fn keyed(&mut self, v: Val) -> Result<(), VmErr> {
        if !self.user_hash || !v.is_heap() { return Ok(()); }
        self.key_slow(v, 0)
    }

    #[cold]
    #[inline(never)]
    fn key_slow(&mut self, v: Val, depth: usize) -> Result<(), VmErr> {
        if depth > crate::value::EQ_DEPTH_MAX { return Ok(()); }
        let cls = match self.heap.try_get(v) {
            Some(HeapObj::Tuple(items)) => {
                let items = items.clone();
                for x in items.into_iter().filter(Val::is_heap) { self.key_slow(x, depth + 1)?; }
                return Ok(());
            }
            Some(&HeapObj::Instance(cls, _)) => cls,
            _ => return Ok(()),
        };
        let Some(Some(hook)) = self.hash_hook(cls) else { return Ok(()) };
        let chunk = self.chunk;
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let r = self.with_roots([v], |vm| { vm.call_with(hook, Some(v), &[], &[], chunk)?; vm.pop() })?;
        if self.as_i128(r).is_none() { return Err(cold_type("__hash__ method should return an integer")); }
        let hash = self.py_hash(r, 0)?;
        let dict = crate::value::hash_val_with_heap(self.heap.int(hash as i128)?, &self.heap);
        if let Some(old) = self.heap.user_key(v) && old.dict == dict { return Ok(()); }
        // A key joins the first class of equal keys under its hash, or starts one.
        let reps = self.key_reps.get(&dict).cloned().unwrap_or_default();
        let mut class = None;
        for r in reps {
            if r.0 == v.0 || self.heap.user_key(r).is_none_or(|k| k.dict != dict) { continue; }
            if self.with_roots([v, r], |vm| vm.values_eq(v, r, chunk))? { class = self.heap.user_key(r).map(|k| k.class); break; }
        }
        let class = match class {
            Some(c) => c,
            None => {
                self.key_classes += 1;
                self.key_reps.entry(dict).or_default().push(v);
                self.key_classes
            }
        };
        self.heap.set_user_key(v, crate::value::UserKey { hash, dict, class });
        Ok(())
    }

    /* Drops the keys a sweep freed from the classes of equal keys. */
    pub(crate) fn prune_key_reps(&mut self) {
        if self.key_reps.is_empty() { return; }
        let heap = &self.heap;
        self.key_reps.retain(|&dict, reps| { reps.retain(|&r| heap.user_key(r).is_some_and(|k| k.dict == dict)); !reps.is_empty() });
    }

    /* The `co_` attributes of the code of function `fi`, the parameters named first and the locals after. */
    pub(crate) fn code_attr(&self, fi: usize, name: &str) -> Option<AttrLookup> {
        use crate::vm::ParamKind;
        let (params, body, _, _) = self.functions.get(fi).copied()?;
        let kinds = self.param_slots.get(fi)?;
        let count = |k: ParamKind| kinds.iter().filter(|(x, _)| *x == k).count() as i64;
        let base = |p: &String| alloc::string::String::from(crate::parser::types::param_base_name(p));
        let names = || -> alloc::vec::Vec<alloc::string::String> {
            // Positional, then keyword-only, then `*args` and `**kwargs`, then every other local in order.
            let mut out: alloc::vec::Vec<alloc::string::String> = params.iter().filter(|p| !p.starts_with('*') && !p.starts_with('~')).map(base).collect();
            out.extend(params.iter().filter(|p| p.starts_with('~')).map(base));
            out.extend(params.iter().filter(|p| p.starts_with('*') && !p.starts_with("**")).map(base));
            out.extend(params.iter().filter(|p| p.starts_with("**")).map(base));
            let scope = self.fn_scope.get(fi);
            for (i, n) in body.names.iter().enumerate() {
                let bare = crate::parser::ssa_strip(n);
                let local = scope.is_some_and(|s| s.kinds.get(i) == Some(&crate::vm::scope::Kind::Local));
                if local && !bare.starts_with('#') && !out.iter().any(|o| o == bare) { out.push(bare.into()); }
            }
            out
        };
        let int = |n: i64| Some(AttrLookup::ClassMember(Val::int(n)));
        match name {
            "co_name" => Some(AttrLookup::Name(self.function_names.get(fi).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| "<lambda>".into()))),
            "co_qualname" => Some(AttrLookup::Name(self.function_qualnames.get(fi).cloned().unwrap_or_default())),
            "co_filename" => Some(AttrLookup::Name(body.path.as_str().into())),
            "co_firstlineno" => int(body.first_line as i64),
            "co_argcount" => int(count(ParamKind::Normal)),
            "co_posonlyargcount" => int(body.posonly as i64),
            "co_kwonlyargcount" => int(count(ParamKind::KwOnly)),
            "co_nlocals" => int(names().len() as i64),
            "co_varnames" => Some(AttrLookup::Names(names())),
            "co_flags" => {
                // Optimized and new locals always, then varargs, varkeywords, nested, generator and coroutine.
                let mut f = 0x3;
                if count(ParamKind::Star) > 0 { f |= 0x4; }
                if count(ParamKind::DoubleStar) > 0 { f |= 0x8; }
                if self.function_parents.get(fi).copied().flatten().is_some() { f |= 0x10; }
                let co = self.is_async_fn(fi);
                if body.is_generator { f |= if co { 0x200 } else { 0x20 }; } else if co { f |= 0x80; }
                int(f)
            }
            _ => None,
        }
    }

    /* The member `name` a class or function holds itself, never one of a base. */
    pub(crate) fn own_member(&self, owner: Val, name: &str) -> Option<Val> {
        match self.heap.try_get(owner)? {
            HeapObj::Class(_, _, m) | HeapObj::Func(_, _, _, m) => m.borrow().iter().find(|(n, _)| n == name).map(|&(_, v)| v),
            _ => None,
        }
    }

    /* `owner.__annotations__`, made once by calling its `__annotate__` with format 1 and kept, an empty dict without one. */
    pub(crate) fn annotations(&mut self, owner: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        if let Some(d) = self.own_member(owner, "__annotations__") { return Ok(d); }
        let d = match self.own_member(owner, "__annotate__").filter(|f| !f.is_none()) {
            Some(f) => {
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.call_with(f, None, &[Val::int(1)], &[], chunk)?;
                self.pop()?
            }
            None => self.heap.alloc(HeapObj::Dict(alloc::rc::Rc::new(core::cell::RefCell::new(DictMap::new()))))?,
        };
        if let Some(HeapObj::Class(_, _, m) | HeapObj::Func(_, _, _, m)) = self.heap.try_get(owner) { crate::value::set_member(m, "__annotations__", d); }
        Ok(d)
    }

    /* The `NotImplemented` singleton. */
    fn not_implemented(&mut self) -> Result<Val, VmErr> { self.heap.alloc(HeapObj::NotImplemented) }
}
