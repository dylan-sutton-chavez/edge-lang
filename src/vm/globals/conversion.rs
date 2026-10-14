use super::super::VM;
use super::super::types::*;
use crate::alloc::string::ToString;

impl<'a> VM<'a> {

    pub fn call_str(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 { return self.alloc_and_push_str(alloc::string::String::new()); }
        if argc >= 2 {
            // `str(bytes, encoding[, errors])` decodes, mirroring `bytes.decode`.
            let args = self.pop_n(argc as usize)?;
            if args[0].is_heap() && matches!(self.heap.get(args[0]), HeapObj::Bytes(_)) {
                return crate::vm::methods::bytes::decode(self, args[0], &args[1..]);
            }
            return Err(cold_type("decoding to str: need a bytes-like object"));
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

    pub fn call_type(&mut self) -> Result<(), VmErr> {
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
