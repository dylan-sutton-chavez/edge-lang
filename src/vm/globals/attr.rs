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
    pub fn call_delattr(&mut self) -> Result<(), VmErr> {
        let name = self.expect_str_arg("delattr() name must be a string")?;
        let obj = self.pop()?;
        self.delete_attr_named(obj, &name)?;
        self.push(Val::none());
        Ok(())
    }

    /* `del obj.attr` opcode, pop the object, remove the named attribute in place. */
    pub fn exec_del_attr(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let obj = self.pop()?;
        let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("DelAttr: bad name index"))?.clone();
        self.delete_attr_named(obj, &name)
    }

    /* Shared attribute removal for `delattr()` and `del obj.attr`, AttributeError when absent. */
    fn delete_attr_named(&mut self, obj: Val, name: &str) -> Result<(), VmErr> {
        if matches!(self.heap.try_get(obj), Some(HeapObj::Class(..))) { self.class_epoch = self.class_epoch.wrapping_add(1); }
        let removed = match self.heap.try_get(obj) {
            Some(HeapObj::Class(_, _, members) | HeapObj::Func(_, _, _, members)) => {
                let mut m = members.borrow_mut();
                let before = m.len();
                m.retain(|(n, _)| n != name);
                m.len() < before
            }
            Some(HeapObj::Instance(_, attrs)) => {
                let key = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name)).map(|(k, _)| k);
                key.is_some_and(|k| attrs.borrow_mut().remove(&k, &self.heap).is_some())
            }
            _ => return Err(cold_type("delattr() target must be an instance or class")),
        };
        if removed { return Ok(()); }
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

    // Pops TOS and returns its String, or errors with `msg` if it isn't a heap string.
    fn expect_str_arg(&mut self, msg: &'static str) -> Result<String, VmErr> {
        let v = self.pop()?;
        self.str_of(v, msg)
    }

    /* `vars(obj)`, an Instance yields a copy of `__dict__`, a Module yields a dict from its attrs. No-arg form is unsupported. */
    pub fn call_vars(&mut self) -> Result<(), VmErr> {
        use alloc::vec::Vec;
        let obj = self.pop()?;
        if !obj.is_heap() {
            return Err(cold_type("vars() argument must have __dict__ attribute"));
        }
        // The `builtins` module makes its builtins on use, so listing them makes them all.
        if obj.0 == self.builtins_module.0 {
            let names = crate::parser::BUILTIN_TYPES.iter().copied().chain(NativeFnId::ALL.iter().map(|id| id.name()));
            let mut dm = DictMap::new();
            for name in names {
                let (key, v) = (self.heap.intern_str(name)?, self.builtin_value(name)?);
                dm.insert(key, v, &self.heap);
            }
            return self.alloc_and_push_dict(dm);
        }
        // An instance answers its attributes as a shared dict, an exception keeps its fields apart.
        if let &HeapObj::Instance(cls, ref attrs) = self.heap.get(obj) && self.exc_base(cls).is_none() {
            let attrs = attrs.clone();
            self.shadowed = true;
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

    /* `globals()`, a copy of the module's bindings, builtins kept apart. */
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
