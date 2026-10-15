
use super::super::VM;
use super::super::types::*;
use super::matches_exc_class;
use alloc::vec::Vec;

impl<'a> VM<'a> {

    /* `property(fget)` / `property(fget, fset)`, captures the descriptor pair the class chain hands to `LoadAttr` / `StoreAttr`. The `@x.setter` decorator builds the second form via `PropertySetter`. */
    pub fn call_property(&mut self, argc: u16) -> Result<(), VmErr> {
        let args = self.pop_n(argc as usize)?;
        let (getter, setter) = match args.as_slice() {
            [g] => (*g, Val::none()),
            [g, s] => (*g, *s),
            _ => return Err(cold_type("property() takes 1 or 2 arguments")),
        };
        let prop = self.heap.alloc(HeapObj::Property(getter, setter))?;
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

    // `super()` zero-arg reads the running method's `(class, self)` off the top frame and returns a Super proxy.
    pub fn call_super(&mut self) -> Result<(), VmErr> {
        // Only the binding of the running frame counts, a plain function a method called has none.
        let binding = self.bindings.last().filter(|b| b.0 == self.level()).map(|b| (b.1, b.2));
        let Some((class, recv)) = binding else {
            return Err(VmErr::Runtime("super() must be called inside a method"));
        };
        let proxy = self.heap.alloc(HeapObj::Super(class, recv))?;
        self.push(proxy);
        Ok(())
    }

    pub fn call_repr(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        let s = self.repr_op(o, chunk)?;
        self.alloc_and_push_str(s)
    }

    pub fn call_callable(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        let result = if o.is_heap() {
            match self.heap.get(o) {
                HeapObj::Func(..) | HeapObj::BoundMethod(..)
                | HeapObj::Type(_) | HeapObj::NativeFn(_)
                | HeapObj::Class(..) | HeapObj::BoundUserMethod(..)
                | HeapObj::Extern(_) => true,
                // instance is callable iff its class chain defines `__call__`.
                HeapObj::Instance(cls, _) => self.lookup_class_member(*cls, "__call__").is_some(),
                _ => false,
            }
        } else { false };
        self.push(Val::bool(result));
        Ok(())
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
        let bad = || VmErr::Type("isinstance() arg 2 must be a type or tuple of types");
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
            Some(HeapObj::Type(name)) => Ok(sub_name.or_else(|| self.exc_base(sub)).is_some_and(|n| matches_exc_class(n, name) || (n == "bool" && name == "int"))
                || sub_name.is_none() && self.container_base(sub).is_some_and(|c| matches!(self.heap.get(c), HeapObj::Type(n) if n == name))),
            Some(HeapObj::Class(..)) => Ok(sub_name.is_none() && self.heap.is_subclass(sub, t)),
            _ => Err(VmErr::Type("issubclass() arg 2 must be a class or tuple of classes")),
        })?;
        self.push(Val::bool(result));
        Ok(())
    }
}

// Numbers hash by their value modulo this prime, so equal ints, floats and bools hash alike.
const MODULUS: u64 = (1 << 61) - 1;

/* The hash of `n`, its value modulo the prime with its sign, -1 kept for errors. */
fn hash_int(n: i128) -> i64 {
    let h = (n.unsigned_abs() % MODULUS as u128) as i64;
    let h = if n < 0 { -h } else { h };
    if h == -1 { -2 } else { h }
}

/* The hash of a finite float, equal to the hash of an int of the same value. */
fn hash_float(v: f64) -> i64 {
    if v.is_infinite() { return if v > 0.0 { 314_159 } else { -314_159 }; }
    let (mut m, mut e) = frexp(v.abs());
    let mut x: u64 = 0;
    while m != 0.0 {
        x = ((x << 28) & MODULUS) | x >> (61 - 28);
        m *= 268_435_456.0;
        e -= 28;
        let y = m as u64;
        m -= y as f64;
        x += y;
        if x >= MODULUS { x -= MODULUS; }
    }
    let e = if e >= 0 { e % 61 } else { 61 - 1 - ((-1 - e) % 61) } as u32;
    x = ((x << e) & MODULUS) | x >> (61 - e);
    let h = if v < 0.0 { x.wrapping_neg() } else { x } as i64;
    if h == -1 { -2 } else { h }
}

/* The mantissa in [0.5, 1) and the exponent of a positive float. */
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 { return (0.0, 0); }
    let exp = ((x.to_bits() >> 52) & 0x7ff) as i32;
    if exp == 0 { let (m, e) = frexp(x * 18_014_398_509_481_984.0); return (m, e - 54); }
    (f64::from_bits((x.to_bits() & !(0x7ff << 52)) | (1022 << 52)), exp - 1022)
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
        let h = self.py_hash(v, 0)?;
        let h = self.int_to_val(Some(h as i128))?;
        self.push(h);
        Ok(())
    }

    pub(crate) fn py_hash(&self, v: Val, depth: usize) -> Result<i64, VmErr> {
        if depth > 200 { return Err(VmErr::CallDepth); }
        if v.is_bool() { return Ok(v.as_bool() as i64); }
        if v.is_float() { let f = v.as_float(); return Ok(if f.is_nan() { identity_hash(v) } else { hash_float(f) }); }
        if let Some(n) = self.as_i128(v) { return Ok(hash_int(n)); }
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
            // A frozenset mixes the hashes of its items in no order.
            Some(HeapObj::FrozenSet(items)) => {
                let mut acc = (items.len() as u64 + 1).wrapping_mul(1_927_868_237);
                for &item in items.iter() { let h = self.py_hash(item, depth + 1)? as u64; acc ^= (h ^ 89_869_747 ^ (h << 16)).wrapping_mul(3_644_798_167); }
                let acc = (acc ^ (acc >> 11) ^ (acc >> 25)).wrapping_mul(69_069).wrapping_add(907_133_923);
                if acc == u64::MAX { 590_923_713 } else { acc as i64 }
            }
            Some(HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::Slice(..)) | None => {
                let h = hash_val_with_heap(v, &self.heap) as i64;
                if h == -1 { -2 } else { h }
            }
            Some(_) => identity_hash(v),
        })
    }
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
fn identity_hash(v: Val) -> i64 {
    let h = v.0.rotate_right(4) as i64;
    if h == -1 { -2 } else { h }
}
