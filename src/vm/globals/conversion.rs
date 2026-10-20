use super::super::VM;
use super::super::types::*;
use crate::vm::natives::Builtins;

impl<'a> VM<'a> {



    pub fn call_type(&mut self, argc: u16) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        if argc == 3 { return self.call_type_new(ok); }
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

    /* `type(name, bases, dict)`, a class built as a class statement builds one, its hooks frames of the loop when `frames`, its job giving the class. */
    fn call_type_new(&mut self, frames: bool) -> Result<(), VmErr> {
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
        let cls = self.finish_class(name, None, bases, members, &[], frames)?;
        if !(frames && self.entered) { self.push(cls); }
        Ok(())
    }

}
