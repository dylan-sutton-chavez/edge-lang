use alloc::string::String;

use super::super::VM;
use super::super::types::*;

impl<'a> VM<'a> {

    /* `print(*args, sep=' ', end='\n')` joins args with `sep`, appends `end`, streams exact bytes via `print_hook` or buffers. Leaves no value (statement-shaped), value uses get a None pushed by the parser / the generic Call path. */
    #[inline(never)]
    pub fn call_print(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        let (positional, kw_flat) = self.parse_call_args(op)?;
        // One container or instance printed alone runs its `__repr__` or `__str__` as frames of the loop, a container writing its items with repr.
        if ok && kw_flat.is_empty() && let [o] = positional[..] && o.is_heap() && self.render_job(o, true, chunk)? { return Ok(()); }
        if ok && kw_flat.is_empty() && let [o] = positional[..] && self.is_instance(o) && self.str_job(o, true) { return Ok(()); }
        // The defaults are borrowed, so a print with no keyword makes no string for them.
        let (mut sep, mut end) = (alloc::borrow::Cow::Borrowed(" "), alloc::borrow::Cow::Borrowed("\n"));
        for pair in kw_flat.as_chunks::<2>().0 {
            // Own the name so the heap borrow ends before the &mut self kwarg calls.
            let kname = String::from(self.kw_name(pair[0]).unwrap_or(""));
            match kname.as_str() {
                "sep" => sep = alloc::borrow::Cow::Owned(self.print_str_kwarg(pair[1], " ", "sep")?),
                "end" => end = alloc::borrow::Cow::Owned(self.print_str_kwarg(pair[1], "\n", "end")?),
                // No file/stdout selection or buffering in the sandbox, accept and ignore.
                "file" | "flush" => {}
                _ => return Err(VmErr::TypeMsg(crate::s!("print() got an unexpected keyword argument '", str &kname, "'"))),
            }
        }
        let mut body = String::new();
        for (i, v) in positional.iter().enumerate() {
            if i > 0 { body.push_str(&sep); }
            // each arg goes through `display_op` so user `__str__` / `__repr__` are honoured.
            let s = self.display_op(*v, chunk)?;
            body.push_str(&s);
        }
        body.push_str(&end);
        self.write_out(&body);
        Ok(())
    }

    /* Hands the exact bytes of `text` to the print hook or the buffered output, no newline added or removed. */
    pub(crate) fn write_out(&mut self, text: &str) {
        match self.print_hook {
            Some(hook) => hook(text),
            None => self.emit_buffered_output(text),
        }
    }

    /* `sep`/`end` accept a string or None (None keeps the default). */
    fn print_str_kwarg(&self, v: Val, default: &str, name: &'static str) -> Result<String, VmErr> {
        if v.is_none() { return Ok(String::from(default)); }
        match self.heap.try_get(v) {
            Some(HeapObj::Str(s)) => Ok(s.clone()),
            _ => Err(VmErr::TypeMsg(crate::s!(str name, " must be None or a string, not ", str self.type_name(v)))),
        }
    }

    /* Append `text` to the line-buffered output, '\n' closes a line, text without one leaves the line open so a later `print(end="")` continues it. */
    fn emit_buffered_output(&mut self, text: &str) {
        let mut rest = text;
        loop {
            match rest.find('\n') {
                Some(i) => {
                    self.append_open_line(&rest[..i]);
                    self.output_open = false;
                    rest = &rest[i + 1..];
                }
                None => {
                    if !rest.is_empty() { self.append_open_line(rest); }
                    break;
                }
            }
        }
    }

    fn append_open_line(&mut self, s: &str) {
        if self.output_open && let Some(last) = self.output.last_mut() {
            last.push_str(s);
            return;
        }
        self.output.push(String::from(s));
        self.output_open = true;
    }

    /* Pops a buffered host line, raises in strict mode and WASM, native non-strict reads stdin. */
    pub fn call_input(&mut self) -> Result<(), VmErr> {
        let s = if !self.input_buffer.is_empty() {
            self.input_buffer.remove(0)
        } else if self.strict_input {
            // Host-driven mode, no blocking stdin read (also keeps headless/fuzz runs from hanging), so the input has ended.
            return Err(eof());
        } else {
            #[cfg(feature = "std")]
            {
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 { return Err(eof()); }
                while line.ends_with('\n') || line.ends_with('\r') { line.pop(); }
                line
            }
            #[cfg(not(feature = "std"))]
            {
                return Err(eof());
            }
        };
        let val = self.heap.alloc(HeapObj::Str(s))?;
        self.push(val); Ok(())
    }

    // `format(value [, spec])`.
    pub fn call_format(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if op != 1 && op != 2 {
            return Err(cold_type("format() takes 1 or 2 arguments"));
        }
        let spec_val = if op == 2 { Some(self.pop()?) } else { None };
        let val = self.pop()?;
        let spec = match spec_val.map(|sv| self.heap.try_get(sv)) {
            None => String::new(),
            Some(Some(HeapObj::Str(s))) => s.clone(),
            Some(_) => return Err(VmErr::TypeMsg(crate::s!("format() argument 2 must be str, not ", str self.type_name(spec_val.unwrap_or_else(Val::none))))),
        };
        let result = self.format_op(val, &spec, chunk)?;
        self.alloc_and_push_str(result)
    }
}

/* What `input()` raises once no line is left to read. */
#[cold]
fn eof() -> VmErr { VmErr::Raised(String::from("EOFError: EOF when reading a line")) }
