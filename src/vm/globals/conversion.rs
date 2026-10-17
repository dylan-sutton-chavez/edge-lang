use super::super::VM;
use super::super::types::*;
use crate::alloc::string::ToString;

impl<'a> VM<'a> {

    pub fn call_str(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 { return self.alloc_and_push_str(alloc::string::String::new()); }
        if argc >= 2 {
            // `str(bytes, encoding[, errors])` decodes, mirroring `bytes.decode`.
            let args = self.pop_n(argc as usize)?;
            for (&a, name) in args[1..].iter().zip(["encoding", "errors"]) {
                if !matches!(self.heap.try_get(a), Some(HeapObj::Str(_))) {
                    return Err(VmErr::TypeMsg(crate::s!("str() argument '", str name, "' must be str, not ", str self.type_name(a))));
                }
            }
            if self.heap.bytes_of(args[0]).is_some() {
                return crate::vm::methods::bytes::decode(self, args[0], &args[1..]);
            }
            return Err(VmErr::TypeMsg(crate::s!("decoding to str: need a bytes-like object, ", str self.type_name(args[0]), " found")));
        }
        let o = self.pop()?;
        let s = self.display_op(o, chunk)?;
        self.alloc_and_push_str(s)
    }

    pub fn call_bool(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 { self.push(Val::bool(false)); return Ok(()); }
        let o = self.pop()?;
        let t = self.truthy_op(o, chunk)?;
        self.push(Val::bool(t));
        Ok(())
    }

    pub fn call_type(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 3 { return self.call_type_new(chunk); }
        if argc != 1 { return Err(cold_type("type() takes 1 or 3 arguments")); }
        let o = self.pop()?;
        // A user instance's type is its own class object, so `type(x) is Cls` holds.
        if let Some(cls) = self.instance_class(o) {
            self.push(cls);
            return Ok(());
        }
        let name = self.type_repr_name(o);
        let t = self.heap.alloc(HeapObj::Type(name))?;
        self.push(t);
        Ok(())
    }

    /* `type(name, bases, dict)`, a class built from its parts as a class statement builds one. */
    fn call_type_new(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let (ns, bases, name) = (self.pop()?, self.pop()?, self.pop()?);
        let name = match self.heap.try_get(name) { Some(HeapObj::Str(s)) => s.clone(), _ => return Err(VmErr::TypeMsg(crate::s!("type.__new__() argument 1 must be str, not ", str self.type_name(name)))) };
        let wrong = |vm: &Self, n: &str, want: &str, v: Val| VmErr::TypeMsg(crate::s!("type.__new__() argument ", str n, " must be ", str want, ", not ", str vm.type_name(v)));
        let Some(HeapObj::Tuple(items)) = self.heap.try_get(bases) else { return Err(wrong(self, "2", "tuple", bases)) };
        let mut bases = items.clone();
        let Some(HeapObj::Dict(ns)) = self.heap.try_get(ns) else { return Err(wrong(self, "3", "dict", ns)) };
        let pairs: alloc::vec::Vec<(Val, Val)> = ns.borrow().iter().collect();
        let mut members = alloc::vec::Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            members.push((self.str_of(k, "type() dict keys must be strings")?, v));
        }
        self.check_bases(&mut bases)?;
        let cls = self.finish_class(name, None, bases, members, &[], chunk)?;
        self.push(cls);
        Ok(())
    }

    /* The name `type(x)` reports, a user instance's own class, an exception's concrete class, else the builtin type name. */
    pub(crate) fn type_repr_name(&self, o: Val) -> alloc::string::String {
        if o.is_heap() {
            match self.heap.get(o) {
                // Exception instances report their concrete class (e.g. `ZeroDivisionError`).
                HeapObj::ExcInstance(n, ..) => return n.clone(),
                _ => if let Some(cls) = self.instance_class(o) && let HeapObj::Class(n, _, _) = self.heap.get(cls) {
                    return n.clone();
                },
            }
        }
        self.type_name(o).to_string() // interned, so shares the `set`/`int` singleton
    }
}
