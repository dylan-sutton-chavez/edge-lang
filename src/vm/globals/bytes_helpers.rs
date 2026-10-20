use crate::s;

use super::super::VM;
use super::super::types::*;

impl<'a> VM<'a> {




    // `import_module(name)`, fetch an already-imported module by alias, returns its `HeapObj::Module` Val.
    pub fn call_import_module(&mut self) -> Result<(), VmErr> {
        let spec = self.pop()?;
        let name = self.str_of(spec, "import_module() argument must be a string")?;
        // A name read at run time may be a builtin the program never wrote, give it its slot first.
        self.register_builtin(&name);
        let val = self.global(&name)
            .ok_or_else(|| VmErr::Raised(s!("NameError: module '", str &name, "' not imported in this scope")))?;
        if !matches!(self.heap.try_get(val), Some(HeapObj::Module(..))) {
            return Err(VmErr::TypeMsg(s!("'", str &name, "' is not a module")));
        }
        self.push(val); Ok(())
    }
}
