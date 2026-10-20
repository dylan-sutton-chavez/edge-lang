use crate::s;

use alloc::string::String;

use super::super::VM;
use super::super::types::*;

impl<'a> VM<'a> {

    // `getattr(obj, name [, default])` reads like `obj.name`, a default answers only an AttributeError.
    pub fn call_getattr(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if op != 2 && op != 3 {
            return Err(cold_type("getattr() takes 2 or 3 arguments"));
        }
        let default = if op == 3 { Some(self.pop()?) } else { None };
        let name = self.expect_str_arg("getattr() name must be a string")?;
        let obj = self.pop()?;
        match (self.with_roots(default, |vm| vm.load_attr(obj, &name, chunk)), default) {
            (Err(e), Some(d)) if self.absorb_attr_err(&e) => { self.push(d); Ok(()) }
            (r, _) => r,
        }
    }

    // `hasattr(obj, name)` is True when `getattr` would not raise AttributeError.
    pub fn call_hasattr(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let name = self.expect_str_arg("hasattr() name must be a string")?;
        let obj = self.pop()?;
        let found = match self.load_attr(obj, &name, chunk) {
            Ok(()) => { self.pop()?; true }
            Err(e) if self.absorb_attr_err(&e) => false,
            Err(e) => return Err(e),
        };
        self.push(Val::bool(found));
        Ok(())
    }

    // `setattr(obj, name, value)` writes like `obj.name = value`, property setters included.
    pub fn call_setattr(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let value = self.pop()?;
        let name = self.expect_str_arg("setattr() name must be a string")?;
        let obj = self.pop()?;
        self.store_attr(obj, &name, value, chunk)?;
        self.push(Val::none());
        Ok(())
    }

    /* `delattr(obj, name)`, remove an attribute from a user instance or class. */
    pub fn call_delattr(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let name = self.expect_str_arg("delattr() name must be a string")?;
        let obj = self.pop()?;
        self.delete_attr_named(obj, &name, chunk)?;
        self.push(Val::none());
        Ok(())
    }

    /* `del obj.attr` opcode, pop the object, remove the named attribute in place. */
    pub fn exec_del_attr(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let obj = self.pop()?;
        let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("DelAttr: bad name index"))?.clone();
        self.delete_attr_named(obj, &name, chunk)
    }

    /* Shared attribute removal for `delattr()` and `del obj.attr`, AttributeError when absent. */
    fn delete_attr_named(&mut self, obj: Val, name: &str, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if self.attr_hooks && self.has_hook(obj, "__delattr__") {
            let attr = self.heap.intern_str(name)?;
            let at = self.prop_at.take();
            if !self.hook_in_loop(obj, "__delattr__", &[attr], at)? { self.call_hook(obj, "__delattr__", &[attr], chunk)?; }
            return Ok(());
        }
        self.delete_attr_raw(obj, name, chunk)
    }

    /* `object.__delattr__`, the delete past any `__delattr__` hook. */
    pub(crate) fn delete_attr_raw(&mut self, obj: Val, name: &str, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if matches!(self.heap.try_get(obj), Some(HeapObj::Class(..))) { self.class_epoch = self.class_epoch.wrapping_add(1); }
        let attrs = match self.heap.try_get(obj) {
            Some(HeapObj::Class(_, _, members) | HeapObj::Func(_, _, _, members)) => {
                let mut m = members.borrow_mut();
                let before = m.len();
                m.retain(|(n, _)| n != name);
                if m.len() < before { return Ok(()); }
                None
            }
            Some(HeapObj::Instance(_, attrs)) => Some(attrs.clone()),
            _ => match self.heap.classed(obj) {
                Some((_, attrs)) => Some(attrs.clone()),
                None => return Err(cold_type("delattr() target must be an instance or class")),
            },
        };
        // The instance gives up its own attribute, else a property or descriptor of its class takes the delete.
        if let Some(attrs) = attrs {
            let key = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name)).map(|(k, _)| k);
            if key.is_some_and(|k| attrs.borrow_mut().remove(&k, &self.heap).is_some()) { return Ok(()); }
            let member = self.instance_class(obj).and_then(|c| self.lookup_class_member(c, name));
            if let Some((m, _)) = member && self.delete_through_class(obj, m, name, chunk)? { return Ok(()); }
        }
        Err(VmErr::Attribute(match self.heap.get(obj) {
            HeapObj::Class(n, ..) => s!("type object '", str n, "' has no attribute '", str name, "'"),
            _ => s!("'", str self.type_name(obj), "' object has no attribute '", str name, "'"),
        }))
    }

    // Returns v's String, or errors with `msg` when it isn't a heap string.
    pub(crate) fn str_of(&self, v: Val, msg: &'static str) -> Result<String, VmErr> {
        if v.is_heap() && let HeapObj::Str(s) = self.heap.get(v) { return Ok(s.clone()); }
        Err(cold_type(msg))
    }

    // Pops the attribute name a getattr-like builtin takes, which must be a str.
    fn expect_str_arg(&mut self, _: &'static str) -> Result<String, VmErr> {
        let v = self.pop()?;
        if v.is_heap() && let HeapObj::Str(s) = self.heap.get(v) { return Ok(s.clone()); }
        Err(VmErr::TypeMsg(crate::s!("attribute name must be string, not '", str self.type_name(v), "'")))
    }

    /* `vars(obj)`, an Instance yields its `__dict__`, a module, class or type a dict from its attrs, `vars()` the locals. */
    #[inline(never)]
    pub fn call_vars(&mut self) -> Result<(), VmErr> {
        use alloc::vec::Vec;
        let obj = self.pop()?;
        if !obj.is_heap() {
            return Err(cold_type("vars() argument must have __dict__ attribute"));
        }
        // The `builtins` module makes its builtins on use, so listing them makes them all.
        if obj.0 == self.builtins_module.0 {
            let names = crate::parser::BUILTIN_TYPES.iter().copied().chain(NativeFnId::ALL.iter().map(|id| id.name()).filter(|n| !n.starts_with('%')));
            let mut dm = DictMap::new();
            for name in names {
                let (key, v) = (self.heap.intern_str(name)?, self.builtin_value(name)?);
                dm.insert(key, v, &self.heap);
            }
            return self.alloc_and_push_dict(dm);
        }
        if matches!(self.heap.get(obj), HeapObj::Class(..) | HeapObj::Type(_)) {
            let d = self.members_dict(obj)?;
            self.push(d);
            return Ok(());
        }
        // An instance answers its attributes as a shared dict, an exception keeps its fields apart.
        // A value a user class made answers the attributes it got.
        if let Some((_, attrs)) = self.heap.classed(obj) {
            let attrs = attrs.clone();
            self.shadowed = true;
            let d = self.heap.alloc(HeapObj::Dict(attrs))?;
            self.push(d);
            return Ok(());
        }
        if let &HeapObj::Instance(cls, _) = self.heap.get(obj) && self.class_slots(cls).is_some() {
            return Err(cold_type("vars() argument must have __dict__ attribute"));
        }
        if let &HeapObj::Instance(cls, ref attrs) = self.heap.get(obj) && self.exc_base(cls).is_none() {
            let attrs = attrs.clone();
            self.shadow();
            let d = self.heap.alloc(HeapObj::Dict(attrs))?;
            self.push(d);
            return Ok(());
        }
        // Two passes, drop the heap borrow before `alloc()`. Modules materialise names as `Vec<String>` first.
        enum Source { Instance(Vec<(Val, Val)>), Module(Vec<(String, Val)>) }
        let src = match self.heap.get(obj) {
            // A `#` name is the engine's own, and the `args` of an exception is no entry of its dict.
            &HeapObj::Instance(cls, ref attrs) => {
                let exc = self.exc_base(cls).is_some();
                Source::Instance(attrs.borrow().iter().filter(|&(k, _)| !matches!(self.heap.try_get(k), Some(HeapObj::Str(n)) if n.starts_with('#') || exc && n == "args")).collect())
            }
            HeapObj::Module(_, attrs) => Source::Module(attrs.clone()),
            _ => return Err(cold_type("vars() argument must have __dict__ attribute")),
        };
        let entries: Vec<(Val, Val)> = match src {
            Source::Instance(e) => e,
            Source::Module(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (name, v) in items {
                    let key = self.heap.intern_str(&name)?;
                    out.push((key, v));
                }
                out
            }
        };
        let mut dm = DictMap::with_capacity(entries.len());
        for (k, v) in entries { dm.insert(k, v, &self.heap); }
        self.alloc_and_push_dict(dm)
    }

    /* `locals()`, the module bindings at module level, elsewhere a fresh dict of the bound locals in the order they were made. */
    pub fn call_locals(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let in_body = self.body_to_fi.contains_key(&(chunk as *const crate::parser::SSAChunk)) || self.class_chunks.contains(&(chunk as *const crate::parser::SSAChunk));
        if !in_body { return self.call_globals(chunk); }
        let bound = self.frame_locals(chunk);
        let mut dm = DictMap::with_capacity(bound.len());
        for (k, v) in bound {
            let key = self.heap.intern_str(&k)?;
            dm.insert(key, v, &self.heap);
        }
        self.alloc_and_push_dict(dm)
    }

    /* `dir(obj)`, the sorted names an attribute read can reach, a user `__dir__` deciding when the class has one, `dir()` the local names. */
    pub fn call_dir(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        // Called where a frame may open, an instance's `__dir__` runs as a frame of the loop.
        if ok && argc == 1 && let Some(&obj) = self.stack.last() && let Some(class) = self.instance_class(obj)
            && let Some((f, defining)) = self.lookup_class_member(class, "__dir__") && matches!(self.heap.try_get(f), Some(HeapObj::Func(..))) {
            self.pop()?;
            if let Some(v) = self.calls_run(alloc::boxed::Box::new(DirOf { obj, f, defining }), true)? { self.push(v); }
            return Ok(());
        }
        let names: alloc::vec::Vec<String> = if argc == 0 {
            let in_body = self.body_to_fi.contains_key(&(chunk as *const crate::parser::SSAChunk)) || self.class_chunks.contains(&(chunk as *const crate::parser::SSAChunk));
            if in_body { self.frame_locals(chunk).into_iter().map(|(n, _)| n).collect() }
            else { self.scopes[self.chunk_module_id(chunk)].iter().map(|(n, _)| String::from(n)).filter(|n| !n.starts_with('#')).chain(["__builtins__", "__name__"].map(String::from)).collect() }
        } else {
            let obj = self.pop()?;
            if self.instance_class(obj).is_some() && let Some(listed) = self.try_call_dunder(obj, "__dir__", &[], chunk)? {
                let names = self.dir_listed(listed, chunk)?;
                let list = self.dir_list(names, false)?;
                self.push(list);
                return Ok(());
            } else {
                self.dir_names(obj)
            }
        };
        let list = self.dir_list(names, true)?;
        self.push(list);
        Ok(())
    }

    /* The names what a `__dir__` gave holds. */
    fn dir_listed(&mut self, listed: Val, chunk: &crate::parser::SSAChunk) -> Result<alloc::vec::Vec<String>, VmErr> {
        let items = self.iterable_items(listed, chunk)?;
        let mut out = alloc::vec::Vec::with_capacity(items.len());
        for v in items { out.push(self.str_of(v, "__dir__() must return an iterable of str")?); }
        Ok(out)
    }

    /* The sorted list `dir` gives of `names`, each once when `once`, as many times as a `__dir__` gave it otherwise. */
    fn dir_list(&mut self, mut names: alloc::vec::Vec<String>, once: bool) -> Result<Val, VmErr> {
        names.sort_unstable();
        if once { names.dedup(); }
        let mut items = alloc::vec::Vec::with_capacity(names.len());
        let base = self.temp_roots.len();
        for n in &names { let v = self.heap.alloc(HeapObj::Str(n.clone()))?; self.temp_roots.push(v); items.push(v); }
        let list = self.heap.alloc(HeapObj::List(alloc::rc::Rc::new(core::cell::RefCell::new(items))));
        self.temp_roots.truncate(base);
        list
    }

    /* `globals()`, a copy of the module's bindings, builtins kept apart. */
    #[inline(never)]
    pub fn call_globals(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let module = self.chunk_module_id(chunk);
        let bound: alloc::vec::Vec<(String, Val)> = self.scopes[module].iter()
            .filter(|(n, _)| !n.starts_with('#'))
            .map(|(n, v)| (String::from(n), v))
            .collect();
        let mut dm = DictMap::with_capacity(bound.len());
        for (k, v) in bound {
            let key = self.heap.alloc(HeapObj::Str(k))?;
            dm.insert(key, v, &self.heap);
        }
        let dict = self.heap.alloc(HeapObj::Dict(alloc::rc::Rc::new(core::cell::RefCell::new(dm))))?;
        self.globals_dicts.retain(|&(m, _)| m != module);
        self.globals_dicts.push((module, dict));
        self.push(dict);
        Ok(())
    }
}

/* `dir(obj)` of an instance whose class's `__dir__` runs as a frame of a job, what it gives listed sorted. */
struct DirOf { obj: Val, f: Val, defining: Val }

impl crate::vm::jobs::Calls for DirOf {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<crate::vm::jobs::Call, VmErr> {
        let Some(r) = got else { return Ok(crate::vm::jobs::Call::Frame(self.f, self.defining, [self.obj, Val::undef(), Val::undef()])) };
        let chunk = vm.chunk;
        let names = vm.dir_listed(r?, chunk)?;
        Ok(crate::vm::jobs::Call::Give(vm.dir_list(names, false)?))
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { f(self.obj); f(self.f); f(self.defining); }
}
