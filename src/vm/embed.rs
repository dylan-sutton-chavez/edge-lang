use crate::s;
use alloc::{string::String, vec::Vec};

use super::VM;
use crate::value::*;
use crate::lang::{Cx, Mode};

// the shared bodies run on the VM's own heap and budget
impl Host for VM<'_> {
    #[inline(always)] fn heap(&self) -> &HeapPool { &self.heap }
    #[inline(always)] fn heap_mut(&mut self) -> &mut HeapPool { &mut self.heap }
    #[inline] fn charge(&mut self, n: usize) -> Result<(), VmErr> { self.charge_steps(n) }
    #[inline(always)] fn vm_items(&mut self, v: Val) -> Result<Option<Vec<Val>>, VmErr> { VM::vm_items(self, v) }
    #[inline] fn type_name(&self, v: Val) -> &str { VM::type_name(self, v) }
    #[inline] fn natives(&self) -> &'static crate::lang::Natives { self.natives }
    fn key_error(&mut self, key: Val) -> VmErr { VM::key_error(self, key) }
    fn fail(&mut self, e: crate::lang::EngineError) -> VmErr { VM::fail(self, e) }
    #[inline] fn root(&mut self, v: Val) { self.temp_roots.push(v); }
    #[inline] fn unroot(&mut self, n: usize) { self.temp_roots.truncate(self.temp_roots.len() - n); }
}

impl<'a> VM<'a> {
    /* `e` with the facts of an engine error worded by the runtime */
    #[inline]
    pub(crate) fn word(&mut self, e: VmErr) -> VmErr {
        use crate::lang::EngineError as E;
        match e {
            VmErr::Engine(b) => self.fail(b.into_inner()),
            // the bare errors the engine raises with no facts past their kind
            VmErr::ZeroDiv => self.fail(E::ZeroDiv),
            VmErr::Overflow => self.fail(E::Overflow),
            VmErr::Name(n) => self.fail(E::Undefined(n)),
            VmErr::CallDepth => self.fail(E::Recursion),
            e => e,
        }
    }

    /* engine error `e` worded by the runtime under the class its kind names */
    #[cold]
    #[inline(never)]
    pub(crate) fn fail(&mut self, e: crate::lang::EngineError) -> VmErr {
        match e.kind() {
            Some(kind) => VmErr::Of(kind, (self.rt.message)(&mut Cx::new(self), &e)),
            None => match e {
                crate::lang::EngineError::Budget => VmErr::Budget,
                crate::lang::EngineError::HostYield(s) => VmErr::HostYield(s),
                crate::lang::EngineError::HostCallDeferred => VmErr::HostCallDeferred,
                crate::lang::EngineError::Redo => VmErr::Redo,
                _ => cold_runtime("a raised value reached the words"),
            },
        }
    }

    /* the ABI kind a plugin sees for `e` with raised values classed by the runtime */
    pub fn error_kind(&mut self, e: &VmErr) -> crate::abi::ErrorKind {
        if matches!(e, VmErr::Raised(_)) && let Some(exc) = self.pending.exc_val { return (self.rt.error_kind)(&Cx::new(self), exc); }
        crate::lang::abi_kind(e, self.natives.names())
    }

    /* byte offset of the last propagating error and None on success or before `run()` */
    pub fn error_pos(&self) -> Option<usize> { self.error_byte_pos.map(|p| p as usize) }



    /* uncaught e reported where it rose in whichever text then the calls it left */
    pub fn render_error(&self, e: &VmErr, src: &str, path: Option<&str>) -> String {
        let names = self.natives.names();
        let mut head = e.render(names);
        if let Some(hint) = self.hint_for(e) { head.push_str(". "); head.push_str(&hint); }
        let mut out = match &self.error_inner {
            Some((s, p, pos)) => crate::value::err::render_text_at(head, s, Some(*pos as usize), Some(if p.is_empty() { names.compiled } else { p })),
            None => crate::value::err::render_text_at(head, src, self.error_pos(), path),
        };
        crate::value::err::render_calls(&mut out, names, &self.call_stack, &self.function_names, path, Some(self.chunk.source.as_str()));
        out
    }

    /* `e` in the runtime's words as its class then its message */
    pub fn error_text(&self, e: &VmErr) -> String { e.render(self.natives.names()) }

    /* frees at a pause what nothing holds as a host may before it saves */
    pub fn sweep(&mut self) { self.collect(); }

    /* uncaught `e` in its runtime's words or the engine's report where it gives none */
    #[inline]
    pub fn report_error(&self, e: &VmErr, src: &str, path: Option<&str>) -> String {
        let Some(words) = self.rt.report else { return self.render_error(e, src, path) };
        let report = self.render_error(e, src, path);
        // only a report placed in its source whose head names a class is reworded
        let head = report.lines().next().and_then(|l| l.strip_prefix("error: ")).filter(|h| h.split_once(": ").is_some_and(|(class, _)| !class.is_empty() && class.chars().all(|c| c.is_alphanumeric() || c == '_')));
        match head {
            Some(head) => words(src, head, if self.error_inner.is_none() { self.error_pos() } else { None }),
            None => report,
        }
    }

    /* the exit code of an uncaught exit class with an int or no argument */
    pub fn system_exit_code(&self) -> Option<i64> {
        let exc = self.pending.exc_val?;
        let (name, args) = self.heap.error_parts(exc)?;
        if name != self.natives.names().error(crate::lang::ErrKind::Exit) { return None; }
        match args.first() {
            None => Some(0),
            Some(a) if a.is_none() => Some(0),
            Some(a) if a.is_int() => Some(a.as_int()),
            _ => None,
        }
    }

    pub fn call_stack_frames(&self) -> &[CallFrame] { &self.call_stack }
    pub fn function_names_ref(&self) -> &[String] { &self.function_names }

    /* the lines the next `input()` calls read one per call from the first */
    pub fn set_input(&mut self, lines: Vec<String>) {
        self.input_buffer = lines;
        self.input_at = 0;
    }

    /* read-only heap access for embedders reading result values */
    pub fn heap(&self) -> &HeapPool { &self.heap }

    /* mutable heap access for embedders building argument values */
    pub fn heap_mut(&mut self) -> &mut HeapPool { &mut self.heap }

    /* faithful buffered output with lines rejoined and a trailing newline unless the last is open */
    pub fn output_text(&self) -> String {
        let mut s = self.output.join("\n");
        if !self.output.is_empty() && !self.output_open { s.push('\n'); }
        s
    }

    /* calls a top-level function by name with positional args and `run()` must bind it first */
    pub fn call_export(&mut self, name: &str, args: &[Val]) -> Result<Val, VmErr> {
        let _paying = self.paying()?;
        if args.len() > 255 {
            return Err(self.fail(crate::lang::EngineError::Call(crate::lang::CallFault::TooMany { name: name.into(), got: args.len() as u32 })));
        }
        // a builtin the program never wrote gets its slot before the name is looked up
        self.register_builtin(name);
        let Some(callee) = self.global(name) else { return Err(self.fail(crate::lang::EngineError::Undefined(name.into()))) };
        let chunk: &crate::syntax::chunk::SSAChunk = self.chunk;
        match self.call_with(callee, None, args, &[], chunk) {
            Ok(v) => v.ok_or(VmErr::Runtime("call_export: callable left no result")),
            Err(e) => Err(self.word(e)),
        }
    }

    /* moves the levels iterators and handlers above the bases into a suspended frame */
    pub(crate) fn save_frames(&mut self, lb: usize, ib: usize, eb: usize, levels: &mut Vec<Val>, iters: &mut Vec<IterFrame>, excs: &mut Vec<ExceptionFrame>) {
        if self.levels.len() > lb { levels.extend(self.levels.drain(lb..)); }
        match self.iter_stack.len().saturating_sub(ib) {
            0 => {}
            1 => iters.extend(self.iter_stack.pop()),
            _ => iters.extend(self.iter_stack.drain(ib..)),
        }
        if self.exception_stack.len() <= eb { return; }
        let at = excs.len();
        excs.extend(self.exception_stack.drain(eb..));
        for f in &mut excs[at..] {
            f.levels = f.levels.saturating_sub(lb);
            f.iter_depth = f.iter_depth.saturating_sub(ib);
        }
    }

    /* puts a suspended frame on the live stacks returning their bases and emptying its buffers */
    pub(crate) fn restore_into(&mut self, levels: &mut Vec<Val>, iters: &mut Vec<IterFrame>, excs: &mut Vec<ExceptionFrame>) -> (usize, usize, usize) {
        let bases = (self.levels.len(), self.iter_stack.len(), self.exception_stack.len());
        if !levels.is_empty() { self.levels.append(levels); }
        match iters.len() {
            0 => {}
            1 => self.iter_stack.extend(iters.pop()),
            _ => self.iter_stack.append(iters),
        }
        if excs.is_empty() { return bases; }
        for f in excs.iter_mut() {
            f.levels += bases.0;
            f.iter_depth += bases.1;
        }
        self.exception_stack.append(excs);
        bases
    }

    /* runs `f` with `vals` as GC roots since its user code can run a collection */
    pub(crate) fn with_roots<R>(&mut self, vals: impl IntoIterator<Item = Val>, f: impl FnOnce(&mut Self) -> R) -> R {
        let base = self.temp_roots.len();
        self.temp_roots.extend(vals);
        let r = f(self);
        self.temp_roots.truncate(base);
        r
    }

    /* `f(*row)` over the first `n` rows of `cols` rooting args and results while `f` runs */
    pub(crate) fn call_rows(&mut self, f: Val, cols: &[Vec<Val>], n: usize, chunk: &crate::syntax::chunk::SSAChunk) -> Result<Vec<Val>, VmErr> {
        self.with_roots(core::iter::once(f).chain(cols.iter().flatten().copied()), |vm| {
            let mut out = Vec::with_capacity(n);
            let mut row = Vec::with_capacity(cols.len());
            for i in 0..n {
                row.clear();
                row.extend(cols.iter().map(|c| c[i]));
                let r = vm.call_with(f, None, &row, &[], chunk)?;
                let r = vm.got(r)?;
                vm.temp_roots.push(r);
                out.push(r);
            }
            Ok(out)
        })
    }
}

impl<'a> VM<'a> {

    #[inline] pub fn truthy(&self, v: Val) -> bool { self.heap.truthy(v) }

    #[inline(never)]
    pub fn type_name(&self, v: Val) -> &str {
        let names = self.natives.names();
        self.heap.type_name_by(v, &|c| match c.body {
            BodyRef::Fn(fi) if !self.is_async_fn(fi) => names.kind(crate::lang::KindId::Coro),
            _ => names.coroutine,
        })
    }

    /* `v` as text in `mode` by the runtime's render hook with ints and strs inline */
    pub(crate) fn render(&mut self, v: Val, mode: Mode) -> Result<String, VmErr> {
        if v.is_int() { return Ok(String::from(itoa::Buffer::new().format(v.as_int()))); }
        if mode == Mode::Str && let Some(HeapObj::Str(s)) = self.heap.try_get(v) { return Ok(s.clone()); }
        (self.rt.show)(&mut Cx::new(self), v, mode)
    }

    /* the text an error shows of `v` which names its kind where rendering fails */
    pub(crate) fn shown(&mut self, v: Val, mode: Mode) -> String {
        match self.render(v, mode) { Ok(text) => text, Err(_) => s!("<", str &self.type_named(v), ">") }
    }

    /* the name the runtime's `type_name` gives the type of `v` */
    pub(crate) fn type_named(&mut self, v: Val) -> String { String::from((self.rt.type_name)(&Cx::new(self), v)) }

    /* `str(v)` as the runtime renders it with the program's budget left as it was */
    pub fn display(&mut self, v: Val) -> String { self.unbilled(|vm| vm.shown(v, Mode::Str)) }

    /* `repr(v)` as the runtime renders it with the program's budget left as it was */
    pub fn repr(&mut self, v: Val) -> String { self.unbilled(|vm| vm.shown(v, Mode::Repr)) }

    /* runs `f` for an embedder with no work limit and the budget put back after */
    fn unbilled<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let budget = self.budget;
        self.budget = u64::MAX;
        self.heap.set_budget(u64::MAX);
        let r = f(self);
        self.budget = budget;
        self.heap.set_budget(budget);
        r
    }

    /* hands `text` to the print hook or buffered output with no newline added or removed */
    pub(crate) fn write_out(&mut self, text: &str) {
        match self.print_hook {
            Some(hook) => hook(text),
            None => self.emit_buffered_output(text),
        }
    }

    /* appends `text` to the buffered lines leaving a tail without newline open for later output */
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
}
