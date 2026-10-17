
use super::super::VM;
use super::super::types::*;
use super::matches_exc_class;
use alloc::vec::Vec;
use crate::value::eq::{hash_float, hash_int};

impl<'a> VM<'a> {

    /* `property(fget, fset, fdel, doc)`, the functions attribute access, stores and deletes run, each optional. */
    pub fn call_property(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        if args.len() > 4 { return Err(VmErr::TypeMsg(crate::s!("property() takes at most 4 arguments (", int args.len() as i64, " given)"))); }
        let part = |i: usize| args.get(i).copied().unwrap_or(Val::none());
        let prop = self.heap.alloc(HeapObj::Property(part(0), part(1), part(2)))?;
        self.push(prop);
        Ok(())
    }

    // `staticmethod(func)` wraps a function so the class chain returns it unbound, with no `self`.
    pub fn call_staticmethod(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let [func] = args.as_slice() else {
            return Err(cold_type("staticmethod() takes exactly one argument"));
        };
        let wrapped = self.heap.alloc(HeapObj::StaticMethod(*func))?;
        self.push(wrapped);
        Ok(())
    }

    // `classmethod(func)` wraps a function so attribute lookup binds the class.
    pub fn call_classmethod(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let [func] = args.as_slice() else {
            return Err(cold_type("classmethod() takes exactly one argument"));
        };
        let wrapped = self.heap.alloc(HeapObj::ClassMethod(*func))?;
        self.push(wrapped);
        Ok(())
    }

    // `super()` reads the running method's `(class, self)` off the top frame, `super(C, obj)` names both, and either returns a Super proxy.
    pub fn call_super(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let (class, recv) = match *args.as_slice() {
            [class, recv] => {
                if !matches!(self.heap.try_get(class), Some(HeapObj::Class(..) | HeapObj::Type(_))) {
                    return Err(VmErr::TypeMsg(crate::s!("super() argument 1 must be a type, not ", str self.type_name(class))));
                }
                // The object is an instance of the class or a class under it.
                let of = self.instance_class(recv).or_else(|| matches!(self.heap.try_get(recv), Some(HeapObj::Class(..))).then_some(recv));
                if !of.is_some_and(|c| c.0 == class.0 || self.heap.is_subclass(c, class)) {
                    return Err(VmErr::TypeMsg(crate::s!("super(type, obj): obj (instance of ", str self.type_name(recv), ") is not an instance or subtype of type (", str match self.heap.get(class) { HeapObj::Class(n, ..) | HeapObj::Type(n) => n.as_str(), _ => "?" }, ").")));
                }
                (class, recv)
            }
            // `super(C)` is unbound, its object None until `__get__` gives it one.
            [class] => {
                if !matches!(self.heap.try_get(class), Some(HeapObj::Class(..) | HeapObj::Type(_))) {
                    return Err(VmErr::TypeMsg(crate::s!("super() argument 1 must be a type, not ", str self.type_name(class))));
                }
                (class, Val::none())
            }
            _ => {
                // Only the binding of the running frame counts, a plain function a method called has none.
                let binding = self.bindings.last().filter(|b| b.0 == self.level()).map(|b| (b.1, b.2));
                let Some(found) = binding else { return Err(VmErr::Runtime("super(): no arguments")) };
                found
            }
        };
        let proxy = self.heap.alloc(HeapObj::Super(class, recv))?;
        self.push(proxy);
        Ok(())
    }

    /* `super(C).__get__(obj)` binds an unbound super to `obj`, a bound one or a None object gives it back as it is. */
    pub(crate) fn super_get(&mut self, sup: Val, pos: &[Val]) -> Result<(), VmErr> {
        let Some(&HeapObj::Super(class, recv)) = self.heap.try_get(sup) else { return Err(cold_type("descriptor '__get__' requires a 'super' object")) };
        if !recv.is_none() || pos[0].is_none() { self.push(sup); return Ok(()); }
        self.push(class);
        self.push(pos[0]);
        self.call_super(2)
    }

    pub fn call_repr(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        let s = self.repr_op(o, chunk)?;
        self.alloc_and_push_str(s)
    }

    /* `ascii(x)`, the repr of `x` with every non-ASCII character escaped. */
    pub fn call_ascii(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        let s = self.repr_op(o, chunk)?;
        self.alloc_and_push_str(crate::vm::format_spec::ascii_escape(&s))
    }

    /* `sentinel(name, *, repr=None)`, a new object equal only to itself. */
    pub fn call_sentinel(&mut self, op: u16) -> Result<(), VmErr> {
        let (positional, kw_flat) = self.parse_call_args(op)?;
        let text = |vm: &Self, v: Val| match vm.heap.try_get(v) { Some(HeapObj::Str(s)) => Some(s.clone()), _ => None };
        let name = text(self, positional[0]).ok_or_else(|| VmErr::TypeMsg(crate::s!("sentinel() argument 1 must be str, not ", str self.type_name(positional[0]))))?;
        let mut repr = None;
        for pair in kw_flat.as_chunks::<2>().0 {
            match self.kw_name(pair[0]).unwrap_or("") {
                "repr" if pair[1].is_none() => {}
                "repr" => repr = Some(text(self, pair[1]).ok_or_else(|| VmErr::TypeMsg(crate::s!("sentinel() argument 'repr' must be str or None, not ", str self.type_name(pair[1]))))?),
                other => {
                    let hint = crate::util::suggest::did_you_mean(other, ["repr"]);
                    return Err(VmErr::TypeMsg(crate::s!("sentinel() got an unexpected keyword argument '", str other, "'", str &hint)));
                }
            }
        }
        let v = crate::frontend::sentinel::make(&mut self.heap, name, repr)?;
        self.push(v);
        Ok(())
    }

    pub fn call_callable(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        let result = self.is_callable(o);
        self.push(Val::bool(result));
        Ok(())
    }

    /* Whether `o` can be called, an instance or a value a class made only when its class defines `__call__`. */
    pub(crate) fn is_callable(&self, o: Val) -> bool {
        if let Some(cls) = self.instance_class(o) { return self.lookup_class_member(cls, "__call__").is_some(); }
        matches!(self.heap.try_get(o), Some(HeapObj::Func(..) | HeapObj::BoundMethod(..) | HeapObj::Type(_) | HeapObj::NativeFn(_)
            | HeapObj::Class(..) | HeapObj::BoundUserMethod(..) | HeapObj::Extern(_) | HeapObj::PropertyPart(..)
            | HeapObj::StaticMethod(_) | HeapObj::GenericAlias(..)))
    }

    /* Type-name based isinstance check. Accepts Type / NativeFn (builtin types) / user Class on the right, allows int<->bool aliasing and walks user inheritance via `is_subclass`. */
    pub fn call_isinstance(&mut self) -> Result<(), VmErr> {
        let (arg2, obj) = (self.pop()?, self.pop()?);
        let result = self.check_classinfo(arg2, |t| self.is_instance_of(obj, t))?;
        self.push(Val::bool(result));
        Ok(())
    }

    /* `isinstance(obj, t)` for one class, builtins by type name and user classes along the bases. */
    fn is_instance_of(&self, obj: Val, t: Val) -> Result<bool, VmErr> {
        let bad = || VmErr::Type("isinstance() arg 2 must be a type, a tuple of types, or a union");
        // A user instance is named "object" so a class named like a builtin never matches.
        let (obj_ty, obj_class) = match self.heap.try_get(obj) {
            Some(&HeapObj::Instance(cls, _)) => ("object", Some(cls)),
            _ => (self.type_name(obj), self.heap.classed(obj).map(|e| e.0)),
        };
        // An exception instance or a type object also matches through the exception tree by its own name.
        let exc_name = match self.heap.try_get(obj) { Some(HeapObj::Type(n) | HeapObj::ExcInstance(n, ..)) => Some(n.as_str()), _ => None };
        match self.heap.try_get(t).ok_or_else(bad)? {
            HeapObj::Type(name) => Ok(name == "object" || matches_exc_class(obj_ty, name) || (obj_ty == "bool" && name == "int")
                || exc_name.is_some_and(|n| matches_exc_class(n, name))
                || obj_class.and_then(|c| self.exc_base(c)).is_some_and(|b| matches_exc_class(b, name))),
            HeapObj::NativeFn(id) if matches!(id.name(), "int" | "str" | "bytes" | "float" | "bool" | "list" | "tuple" | "dict" | "set") =>
                Ok(id.name() == obj_ty || (obj_ty == "bool" && id.name() == "int")),
            HeapObj::Class(..) => Ok(obj_class.is_some_and(|c| self.heap.is_subclass(c, t))),
            _ => Err(bad()),
        }
    }

    /* `isinstance(obj, classinfo)`, a type or a tuple or union of them. */
    pub(crate) fn instance_of(&self, obj: Val, classinfo: Val) -> Result<bool, VmErr> {
        self.check_classinfo(classinfo, |t| self.is_instance_of(obj, t))
    }

    /* Shared `isinstance`/`issubclass` classinfo dispatch, a tuple or union matches if any member does, else a single check. */
    fn check_classinfo(&self, arg2: Val, single: impl Fn(Val) -> Result<bool, VmErr>) -> Result<bool, VmErr> {
        // `int | str` checks like the tuple of its members.
        let arg2 = match self.heap.try_get(arg2) { Some(&HeapObj::Union(args)) => args, _ => arg2 };
        if let Some(HeapObj::Tuple(items)) = self.heap.try_get(arg2) {
            // A non-class member raises instead of being skipped.
            for &t in items { if single(t)? { return Ok(true); } }
            return Ok(false);
        }
        single(arg2)
    }

    /* `issubclass(C, B)`, both are classes (B may be a tuple). Walks the exception hierarchy for built-ins and the inheritance chain for user classes. Unlike `isinstance`, arg 1 must itself be a class. */
    pub fn call_issubclass(&mut self) -> Result<(), VmErr> {
        let (arg2, sub) = (self.pop()?, self.pop()?);
        // arg 1 must be a built-in/exception `Type` or a user `Class`.
        let sub_name = match self.heap.try_get(sub) {
            Some(HeapObj::Type(n)) => Some(n.as_str()),
            Some(HeapObj::Class(..)) => None,
            _ => return Err(VmErr::Type("issubclass() arg 1 must be a class")),
        };
        let result = self.check_classinfo(arg2, |t| match self.heap.try_get(t) {
            // A user class reaches a builtin type only through an exception, dict or list base.
            // Every class and type derives from `object`.
            Some(HeapObj::Type(name)) if name == "object" => Ok(true),
            Some(HeapObj::Type(name)) => Ok(sub_name.or_else(|| self.exc_base(sub)).is_some_and(|n| matches_exc_class(n, name) || (n == "bool" && name == "int"))
                || sub_name.is_none() && self.native_base(sub).is_some_and(|c| matches!(self.heap.get(c), HeapObj::Type(n) if n == name))),
            Some(HeapObj::Class(..)) => Ok(sub_name.is_none() && self.heap.is_subclass(sub, t)),
            _ => Err(VmErr::Type("issubclass() arg 2 must be a class, a tuple of classes, or a union")),
        })?;
        self.push(Val::bool(result));
        Ok(())
    }
}

impl<'a> VM<'a> {
    /* `id(x)`, the boxed word itself, unique while the object lives and kept across a snapshot. */
    pub fn call_id(&mut self) -> Result<(), VmErr> {
        let v = self.pop()?;
        let id = self.int_to_val(Some(v.0 as i128))?;
        self.push(id);
        Ok(())
    }

    /* `hash(x)`, by value for numbers, strings and tuples and by identity for the rest. */
    pub fn call_hash(&mut self) -> Result<(), VmErr> {
        let v = self.pop()?;
        self.require_hashable(v)?;
        self.keyed(v)?;
        let h = self.py_hash(v, 0)?;
        let h = self.int_to_val(Some(h as i128))?;
        self.push(h);
        Ok(())
    }

    pub(crate) fn py_hash(&self, v: Val, depth: usize) -> Result<i64, VmErr> {
        if depth > 200 { return Err(VmErr::CallDepth); }
        let v = crate::value::unboxed(v, &self.heap);
        if v.is_bool() { return Ok(v.as_bool() as i64); }
        if v.is_float() { let f = v.as_float(); return Ok(if f.is_nan() { identity_hash(v) } else { hash_float(f) }); }
        if let Some(n) = self.as_i128(v) { return Ok(hash_int(n)); }
        if let Some(h) = self.heap.foreign_hash(v) { return Ok(h as i64); }
        if let Some(k) = self.heap.user_key(v) { return Ok(k.hash); }
        Ok(match self.heap.try_get(v) {
            // A tuple mixes the hashes of its items in order.
            Some(HeapObj::Tuple(items)) => {
                let lanes: Result<Vec<i64>, VmErr> = items.iter().map(|&item| self.py_hash(item, depth + 1)).collect();
                tuple_hash(&lanes?)
            }
            // A range hashes as its length, start and step, the parts that tell two ranges apart.
            Some(&HeapObj::Range(start, stop, step)) => {
                let (len, none) = (crate::value::eq::range_len(start, stop, step), self.py_hash(Val::none(), depth + 1)?);
                let (start, step) = (if len == 0 { none } else { hash_int(start as i128) }, if len <= 1 { none } else { hash_int(step as i128) });
                tuple_hash(&[hash_int(len), start, step])
            }
            Some(HeapObj::BigRange(..)) => tuple_hash(&crate::frontend::int::range_hashes(&self.heap, v, self.py_hash(Val::none(), depth + 1)?).unwrap_or_default()),
            // A frozenset mixes the hashes of its items in no order, and a frozendict hashes as the frozenset of its items.
            Some(HeapObj::FrozenSet(items)) => set_hash(&items.iter().map(|&item| self.py_hash(item, depth + 1)).collect::<Result<Vec<_>, _>>()?),
            Some(HeapObj::FrozenDict(d)) => {
                let pairs: Vec<(Val, Val)> = d.borrow().iter().collect();
                set_hash(&pairs.into_iter().map(|(k, v)| Ok(tuple_hash(&[self.py_hash(k, depth + 2)?, self.py_hash(v, depth + 2)?]))).collect::<Result<Vec<_>, VmErr>>()?)
            }
            Some(HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::Slice(..) | HeapObj::MemoryView(_)) | None => {
                let h = hash_val_with_heap(v, &self.heap) as i64;
                if h == -1 { -2 } else { h }
            }
            Some(_) => identity_hash(v),
        })
    }
}

/* The hash of a frozenset from the hashes of its items, in any order. */
fn set_hash(hashes: &[i64]) -> i64 {
    let mut acc = (hashes.len() as u64 + 1).wrapping_mul(1_927_868_237);
    for &h in hashes { let h = h as u64; acc ^= (h ^ 89_869_747 ^ (h << 16)).wrapping_mul(3_644_798_167); }
    let acc = (acc ^ (acc >> 11) ^ (acc >> 25)).wrapping_mul(69_069).wrapping_add(907_133_923);
    if acc == u64::MAX { 590_923_713 } else { acc as i64 }
}

/* The hash of a tuple from the hashes of its items. */
fn tuple_hash(lanes: &[i64]) -> i64 {
    const P1: u64 = 11_400_714_785_074_694_791;
    const P2: u64 = 14_029_467_366_897_019_727;
    const P5: u64 = 2_870_177_450_012_600_261;
    let acc = lanes.iter().fold(P5, |acc, &lane| acc.wrapping_add((lane as u64).wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1));
    let acc = acc.wrapping_add(lanes.len() as u64 ^ (P5 ^ 3_527_539));
    if acc == u64::MAX { 1_546_275_796 } else { acc as i64 }
}

/* The hash of an object compared by identity, its word with the fixed low bits rotated away. */
pub(crate) fn identity_hash(v: Val) -> i64 {
    let h = v.0.rotate_right(4) as i64;
    if h == -1 { -2 } else { h }
}
