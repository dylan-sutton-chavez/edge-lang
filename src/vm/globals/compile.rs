use alloc::{format, rc::Rc, string::{String, ToString}, sync::Arc, vec::Vec};
use core::ops::Range;

use crate::lexer::lex;
use crate::parser::{Diagnostic, Parser, SSAChunk};
use crate::value::CallFrame;
use crate::s;
use crate::util::hash::{FxHashMap, FxHasher};
use core::hash::Hasher;

use super::super::VM;
use super::super::types::*;

/* A chunk compiled while the program runs, kept while the VM lives since the functions it defines point into it. */
pub(crate) struct Compiled {
    pub(crate) source: String,
    pub(crate) form: Form,
    // The module its names bind into, its caller or a scratch one, none for the entry module.
    pub(crate) spec: Option<String>,
    scope: usize,
    // A scratch scope in use by a run, which a nested run of the same source may not share.
    busy: bool,
    // The functions the chunk added to the table.
    fns: Range<usize>,
    chunk: Rc<SSAChunk>,
}

/* How a text compiles, its mode and whether it runs under a `__builtins__` of its own, which loads every builtin by name. */
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Form {
    pub(crate) mode: CodeMode,
    pub(crate) closed: bool,
}

/* A scratch scope whose functions read and write a dict, refreshed from it by each outermost call into them. */
pub(crate) struct Linked {
    pub(crate) dict: Val,
    pub(crate) scope: usize,
    pub(crate) fns: Range<usize>,
    // Calls into the functions running now, the outermost one syncing the dict.
    active: u32,
}

impl Linked {
    pub(crate) fn new(dict: Val, scope: usize, fns: Range<usize>) -> Self { Self { dict, scope, fns, active: 0 } }
}

impl Compiled {
    /* The chunk, which lives as long as the VM keeps it. */
    pub(crate) fn chunk(&self) -> &SSAChunk { &self.chunk }
}

/* Every chunk compiled at run time in the order it was made, found by the hash of its text. */
#[derive(Default)]
pub(crate) struct CompiledSet { list: Vec<Compiled>, by_text: FxHashMap<u64, Vec<usize>> }

impl CompiledSet {
    fn push(&mut self, c: Compiled) -> usize {
        self.by_text.entry(text_hash(&c.source)).or_default().push(self.list.len());
        self.list.push(c);
        self.list.len() - 1
    }

    /* The first chunk compiled from `source` that `fits` accepts. */
    fn find(&self, source: &str, fits: impl Fn(&Compiled) -> bool) -> Option<usize> {
        self.by_text.get(&text_hash(source))?.iter().copied().find(|&at| self.list[at].source == source && fits(&self.list[at]))
    }
}

impl core::ops::Deref for CompiledSet {
    type Target = [Compiled];
    fn deref(&self) -> &[Compiled] { &self.list }
}

impl core::ops::DerefMut for CompiledSet {
    fn deref_mut(&mut self) -> &mut [Compiled] { &mut self.list }
}

fn text_hash(source: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(source.as_bytes());
    h.finish()
}

// The extra nesting an eval or exec run counts, since its loop takes about three times the host stack of a nested call.
const EVAL_NESTING: usize = 2;

/* What a run starts from in a scratch scope and where its bindings go once it ends. */
struct Scratch {
    names: Vec<(String, Val)>,
    // The `__name__` a globals dict without one reads, which never lands in the dict.
    default_name: Option<Val>,
    // The dict `globals` named, which takes every binding back.
    globals: Option<Val>,
    // The dict `locals` named, which takes the bindings that changed.
    locals: Option<Val>,
}

impl<'a> VM<'a> {
    /* `eval(source, globals, locals)`, the value of an expression or the run of compiled code. */
    pub fn call_eval(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let args = self.pop_n(operand as usize)?;
        let value = self.run_source(&args, CodeMode::Eval, "eval", chunk)?;
        self.push(value);
        Ok(())
    }

    /* `exec(source, globals, locals)`, statements run for their bindings. */
    pub fn call_exec(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let args = self.pop_n(operand as usize)?;
        self.run_source(&args, CodeMode::Exec, "exec", chunk)?;
        self.push(Val::none());
        Ok(())
    }

    /* `compile(source, filename, mode)`, a code object whose source already parsed. */
    pub fn call_compile(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let args = self.pop_n(operand as usize)?;
        let source = self.source_text(args[0], "compile")?;
        let filename = match self.heap.try_get(args[1]) {
            Some(HeapObj::Str(s)) => s.clone(),
            _ => return Err(cold_type("compile() filename must be a string")),
        };
        let mode = match self.heap.try_get(args[2]) {
            Some(HeapObj::Str(s)) if s == "exec" => CodeMode::Exec,
            Some(HeapObj::Str(s)) if s == "eval" => CodeMode::Eval,
            Some(HeapObj::Str(s)) if s == "single" => CodeMode::Single,
            _ => return Err(cold_value("compile() mode must be 'exec', 'eval' or 'single'")),
        };
        // The flag asking for a syntax tree needs the ast module, which compiles apart.
        if args.get(3).is_some_and(|f| f.is_int() && f.as_int() & 0x400 != 0) {
            return Err(cold_value("compile() makes code, a syntax tree needs the ast module"));
        }
        let source = if mode == CodeMode::Eval { strip_eval(&source) } else { source };
        // A syntax error raises here, the code keeps only its text and compiles again where it runs.
        self.charge_steps(source.len())?;
        self.parse_compiled(&source, Form { mode, closed: false }, &filename, &chunk.path)?;
        let code = self.heap.alloc(HeapObj::Code(Rc::new(CodeObj { source, filename, mode })))?;
        self.push(code);
        Ok(())
    }

    /* The text of a source argument, a str, utf-8 bytes, or the source of a code object. */
    fn source_text(&self, v: Val, name: &str) -> Result<String, VmErr> {
        match self.heap.try_get(v) {
            Some(HeapObj::Str(s)) => Ok(s.clone()),
            Some(HeapObj::Bytes(b)) => String::from_utf8(b.clone()).map_err(|_| cold_value("source is not valid utf-8")),
            Some(HeapObj::Code(c)) => Ok(c.source.clone()),
            _ => Err(VmErr::TypeMsg(s!(str name, "() arg 1 must be a string, bytes or code object"))),
        }
    }

    /* Runs the source in `args` the way `eval` or `exec` asks, the value an expression gave back. */
    fn run_source(&mut self, args: &[Val], asked: CodeMode, name: &str, caller: &SSAChunk) -> Result<Val, VmErr> {
        let call_ip = self.pending.call_ip;
        let (source, mode, filename) = match self.heap.try_get(args[0]) {
            Some(HeapObj::Code(c)) => (c.source.clone(), c.mode, c.filename.clone()),
            _ => {
                let source = self.source_text(args[0], name)?;
                (if asked == CodeMode::Eval { strip_eval(&source) } else { source }, asked, String::from("<string>"))
            }
        };
        let dict = |vm: &Self, v: Option<&Val>, what: &str| -> Result<Option<Val>, VmErr> {
            match v.copied().filter(|v| !v.is_none()) {
                None => Ok(None),
                Some(d) if matches!(vm.heap.try_get(d), Some(HeapObj::Dict(_))) => Ok(Some(d)),
                Some(_) => Err(VmErr::TypeMsg(s!(str what, " must be a dict"))),
            }
        };
        let globals = dict(self, args.get(1), "globals")?;
        let locals = dict(self, args.get(2), "locals")?;
        // The dict `globals()` made for a module stands for that module, so text given it alone runs there.
        let owner = globals.filter(|_| locals.is_none()).and_then(|g| self.globals_dicts.iter().find(|(_, d)| d.0 == g.0).map(|&(m, _)| m));
        let module = owner.unwrap_or_else(|| self.chunk_module_id(caller));
        let in_body = self.body_to_fi.contains_key(&(caller as *const SSAChunk)) || self.class_chunks.contains(&(caller as *const SSAChunk));
        // Module code with no dicts runs in its own module, any other run in a scratch scope it fills first.
        let mut scratch = (owner.is_none() && (globals.is_some() || locals.is_some() || in_body)).then(|| {
            let mut names = match globals {
                Some(g) => self.dict_names(g),
                None => self.scopes[module].iter().filter(|(n, _)| !n.starts_with('#')).map(|(n, v)| (n.to_string(), v)).collect(),
            };
            // A function sees its own locals over the module, and its own bindings stay as they were, like Python does.
            if globals.is_none() && in_body { names.extend(self.frame_locals(caller)); }
            if let Some(l) = locals { names.extend(self.dict_names(l)); }
            Scratch { names, default_name: None, globals, locals }
        });
        let mut closed = false;
        if let Some(s) = &mut scratch && let Some(g) = s.globals {
            // A globals dict without `__name__` reads the name of the builtins, as Python does.
            if !s.names.iter().any(|(n, _)| n == "__name__") {
                let name = self.default_name()?;
                s.names.push((String::from("__name__"), name));
                s.default_name = Some(name);
            }
            // A dict of its own under `__builtins__` holds the only builtins the text finds.
            if let Some(own) = self.builtins_of(g)? {
                closed = true;
                s.names.extend(self.dict_names(own));
            }
        }
        let form = Form { mode, closed };
        let at = self.compiled_for(&source, form, &filename, if scratch.is_some() { None } else { Some(module) }, &caller.path)?;
        let scope = self.compiled[at].scope;
        if let Some(scratch) = &scratch {
            self.compiled[at].busy = true;
            self.scopes[scope].unbind_all();
            self.scopes[scope].closed = closed;
            for (n, v) in &scratch.names { self.scopes[scope].set(n, *v); }
        }
        // SAFETY the VM keeps every compiled chunk until it drops, and lends none of them past that.
        let chunk: &'a SSAChunk = unsafe { &*(self.compiled[at].chunk() as *const SSAChunk) };
        let depths = [self.stack.len(), self.iter_stack.len(), self.exception_stack.len(), self.unwind_stack.len(), self.with_stack.len(), self.pending_sync_frames.len(), self.bindings.len()];
        let frames = self.call_stack.len();
        let mut ran = self.run_compiled(chunk);
        // An error in the text keeps the frames inside it, then the call that ran it, which the error now points at.
        if ran.is_err() {
            self.error_byte_pos = None;
            let call_byte_pos = call_ip.and_then(|ip| caller.resolve_call(ip).or_else(|| caller.resolve(ip))).unwrap_or(0);
            let fi = if name == "eval" { crate::value::EVAL_FRAME } else { crate::value::EXEC_FRAME };
            if self.call_stack.len() >= frames {
                self.call_stack.push(CallFrame { fi, call_byte_pos, caller_source: Some(caller.source.clone()), caller_path: Some(caller.path.clone()), repeats: 0 });
            }
        }
        // A native call holds the run, so a suspension inside it has nowhere to resume and its frames end here.
        if core::mem::take(&mut self.yielded) {
            let [stack, iters, excs, unwinds, withs, syncs, bindings] = depths;
            self.stack.truncate(stack);
            self.iter_stack.truncate(iters);
            self.exception_stack.truncate(excs);
            self.unwind_stack.truncate(unwinds);
            self.with_stack.truncate(withs);
            self.pending_sync_frames.truncate(syncs);
            self.bindings.truncate(bindings);
            self.resume_ip = 0;
            self.pending.clear_waits();
            ran = Err(VmErr::Runtime("code that eval or exec runs cannot suspend, await it outside them"));
        }
        if let Some(scratch) = scratch {
            self.compiled[at].busy = false;
            if ran.is_ok() { self.keep_scratch(at, scratch)?; }
        }
        if let (Some(g), Some(m), true) = (globals, owner, ran.is_ok()) { self.sync_dict(g, m, None)?; }
        let value = ran?;
        if mode == CodeMode::Single && !value.is_none() {
            let shown = self.repr_op(value, caller)?;
            self.write_out(&s!(str &shown, "\n"));
        }
        Ok(if mode == CodeMode::Exec { Val::none() } else { value })
    }

    /* The compiled chunk for `source` bound into `module`, or into a scratch scope no run uses when `module` is none. */
    pub(crate) fn compiled_for(&mut self, source: &str, form: Form, filename: &str, module: Option<usize>, path: &Arc<String>) -> Result<usize, VmErr> {
        let fits = |c: &Compiled| c.form == form && c.chunk.path == *path
            && match module { Some(m) => c.scope == m, None => !c.busy && c.spec.as_ref().is_some_and(|s| s.starts_with('\u{0}')) && leaves_nothing(&c.chunk) };
        if let Some(at) = self.compiled.find(source, fits) { return Ok(at); }
        self.charge_steps(source.len())?;
        let chunk = self.parse_compiled(source, form, filename, path)?;
        let spec = match module {
            Some(m) => self.scope_ids.iter().find(|&(_, &i)| i == m).map(|(s, _)| s.clone()),
            None => Some(format!("\u{0}scratch {}", self.compiled.len())),
        };
        Ok(self.adopt(source, form, spec, chunk))
    }

    /* Makes `chunk` part of the program, its functions in the table and its names bound into module `spec`. */
    pub(crate) fn adopt(&mut self, source: &str, form: Form, spec: Option<String>, chunk: SSAChunk) -> usize {
        let chunk = Rc::new(chunk);
        // SAFETY the VM keeps every compiled chunk until it drops, and lends none of them past that.
        let body: &'a SSAChunk = unsafe { &*Rc::as_ptr(&chunk) };
        let start = self.functions.len();
        self.adopt_functions(body, spec.as_deref());
        let (scope, fns) = (self.chunk_module_id(body), start..self.functions.len());
        self.scopes[scope].closed = form.closed;
        self.compiled.push(Compiled { source: source.to_string(), form, spec, scope, busy: false, fns, chunk })
    }

    /* Parses `source` the way `mode` runs it, as code of the file at `path` whose package resolves its imports. */
    pub(crate) fn parse_compiled(&self, source: &str, form: Form, filename: &str, path: &Arc<String>) -> Result<SSAChunk, VmErr> {
        let (tokens, lex_errs) = lex(source);
        let resolver = match self.resolver_hook {
            Some(hook) => hook(&crate::modules::dir_of(path)),
            None => alloc::boxed::Box::new(crate::modules::NoopResolver),
        };
        // The caller path decides the package its system calls answer to, the file name only names its errors.
        let mut parser = Parser::with_resolver(source, tokens.into_iter(), resolver).at_path(path.clone());
        // Under builtins of its own the text calls each builtin through its name, which only they bind.
        if form.closed { parser = parser.unfusing(NativeFnId::ALL.iter().map(|id| id.name().to_string())); }
        for e in lex_errs { parser.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }); }
        // A single statement that is one expression echoes its value.
        let eval = form.mode == CodeMode::Eval || form.mode == CodeMode::Single && is_expression(source);
        let (mut chunk, errs) = if eval { parser.parse_eval() } else { parser.parse_exec() };
        match errs.first() {
            None => { crate::optimizer::constant_fold(&mut chunk); Ok(chunk) }
            Some(e) => {
                let line = source[..e.start.min(source.len())].matches('\n').count() + 1;
                Err(VmErr::Raised(s!("SyntaxError: ", str &e.msg, " (", str filename, ", line ", int line as i64, ")")))
            }
        }
    }

    /* Runs a compiled chunk as module code, its imports first, the value its last expression left. */
    pub(crate) fn run_compiled(&mut self, chunk: &'a SSAChunk) -> Result<Val, VmErr> {
        if self.depth + EVAL_NESTING >= self.max_calls { return Err(cold_depth()); }
        self.depth += EVAL_NESTING;
        let mut in_progress = crate::util::hash::FxHashSet::default();
        let ran = self.init_modules(chunk, &mut in_progress).and_then(|()| {
            // Under builtins of its own no slot starts from one.
            let mut slots = if self.scopes[self.chunk_module_id(chunk)].closed { alloc::vec![Val::undef(); chunk.names.len()] } else { self.fill_builtins(&chunk.names) };
            self.exec(chunk, &mut slots)
        });
        self.depth -= EVAL_NESTING;
        ran
    }

    /* Hands the bindings of a finished scratch run to the dicts that asked for them. */
    fn keep_scratch(&mut self, at: usize, scratch: Scratch) -> Result<(), VmErr> {
        let scope = self.compiled[at].scope;
        if let Some(l) = scratch.locals {
            let names = &self.scopes[scope];
            // A name the run deleted leaves the dict as well.
            let bound: Vec<(String, Option<Val>)> = names.iter().filter(|(n, _)| !n.starts_with('#')).map(|(n, v)| (n.to_string(), Some(v)))
                .chain(scratch.names.iter().filter(|(n, _)| names.get(n).is_none()).map(|(n, _)| (n.clone(), None)))
                .collect();
            let changed = |n: &str, v: Val| scratch.names.iter().rev().find(|(m, _)| m == n).is_none_or(|(_, old)| old.0 != v.0);
            for (n, v) in bound.iter().filter(|(n, v)| v.is_none_or(|v| changed(n, v))) { self.dict_bind(l, n, *v)?; }
        } else if let Some(g) = scratch.globals {
            self.sync_dict(g, scope, scratch.default_name)?;
            // A function the run defined keeps reading and writing the dict, so its calls sync with it.
            if !leaves_nothing(&self.compiled[at].chunk) { self.link_scope(Linked::new(g, scope, self.compiled[at].fns.clone())); }
        }
        Ok(())
    }

    /* Makes the functions of `link` sync with its dict on each call, never as frames of the loop calling them. */
    pub(crate) fn link_scope(&mut self, link: Linked) {
        for fi in link.fns.clone() { if let Some(f) = self.call_facts.get_mut(fi) { f.in_loop = false; } }
        self.linked.push(link);
    }

    /* The link whose functions hold `fi`, if a dict exec defined it. */
    #[inline]
    pub(crate) fn link_of(&self, fi: usize) -> Option<usize> {
        if self.linked.is_empty() { return None; }
        self.linked.iter().position(|l| l.fns.contains(&fi))
    }

    /* Refreshes the scope of a link from its dict on the outermost call into it. */
    pub(crate) fn enter_link(&mut self, at: usize) {
        let (dict, scope, active) = (self.linked[at].dict, self.linked[at].scope, self.linked[at].active);
        self.linked[at].active += 1;
        if active > 0 { return; }
        let names = self.dict_names(dict);
        self.scopes[scope].unbind_all();
        for (n, v) in &names { self.scopes[scope].set(n, *v); }
        if !names.iter().any(|(n, _)| n == "__name__") && let Ok(name) = self.default_name() { self.scopes[scope].set("__name__", name); }
    }

    /* Hands the scope of a link back to its dict once the outermost call into it ends. */
    pub(crate) fn leave_link(&mut self, at: usize) -> Result<(), VmErr> {
        self.linked[at].active = self.linked[at].active.saturating_sub(1);
        if self.linked[at].active > 0 { return Ok(()); }
        let (dict, scope) = (self.linked[at].dict, self.linked[at].scope);
        let name = self.default_name()?;
        self.sync_dict(dict, scope, Some(name))
    }

    /* The dict a globals dict holds under `__builtins__` when it is one of its own, the shared module put there when it has none. */
    fn builtins_of(&mut self, g: Val) -> Result<Option<Val>, VmErr> {
        let key = self.heap.intern_str("__builtins__")?;
        let held = match self.heap.try_get(g) { Some(HeapObj::Dict(rc)) => rc.borrow().get(&key, &self.heap).copied(), _ => None };
        match held {
            None => {
                let shared = self.shared_builtins()?;
                self.dict_bind(g, "__builtins__", Some(shared))?;
                Ok(None)
            }
            Some(b) if b.0 != self.builtins_module.0 && matches!(self.heap.try_get(b), Some(HeapObj::Dict(_))) => Ok(Some(b)),
            Some(_) => Ok(None),
        }
    }

    /* The `builtins` module, made once and shared by every globals dict that gets it, its builtins found on use. */
    fn shared_builtins(&mut self) -> Result<Val, VmErr> {
        if self.builtins_module.is_undef() { self.builtins_module = self.heap.alloc(HeapObj::Module(String::from("builtins"), Vec::new()))?; }
        Ok(self.builtins_module)
    }

    /* The `__name__` that text run with a globals dict reads when the dict has none. */
    fn default_name(&mut self) -> Result<Val, VmErr> { self.heap.intern_str("builtins") }

    /* Makes dict `d` hold every binding of scope `scope`, a name the scope lost leaving the dict too, and `default_name` left out while unchanged. */
    fn sync_dict(&mut self, d: Val, scope: usize, default_name: Option<Val>) -> Result<(), VmErr> {
        let kept = |n: &str, v: Val| !n.starts_with('#') && !(n == "__name__" && default_name.is_some_and(|d| d.0 == v.0));
        let bound: Vec<(String, Val)> = self.scopes[scope].iter().filter(|&(n, v)| kept(n, v)).map(|(n, v)| (n.to_string(), v)).collect();
        let gone: Vec<String> = self.dict_names(d).into_iter().filter(|(n, _)| self.scopes[scope].get(n).is_none()).map(|(n, _)| n).collect();
        for (n, v) in &bound { self.dict_bind(d, n, Some(*v))?; }
        for n in &gone { self.dict_bind(d, n, None)?; }
        Ok(())
    }

    /* The str keys of dict `d` with their values. */
    fn dict_names(&self, d: Val) -> Vec<(String, Val)> {
        let Some(HeapObj::Dict(rc)) = self.heap.try_get(d) else { return Vec::new() };
        rc.borrow().iter().filter_map(|(k, v)| match self.heap.try_get(k) {
            Some(HeapObj::Str(s)) if s != "__builtins__" => Some((s.clone(), v)),
            _ => None,
        }).collect()
    }

    /* Binds `name` in dict `d`, or drops it when `v` is none. */
    fn dict_bind(&mut self, d: Val, name: &str, v: Option<Val>) -> Result<(), VmErr> {
        let key = self.heap.intern_str(name)?;
        if let Some(HeapObj::Dict(rc)) = self.heap.try_get(d) {
            let mut dict = rc.borrow_mut();
            match v {
                Some(v) => { dict.insert(key, v, &self.heap); }
                None => { dict.remove(&key, &self.heap); }
            }
        }
        Ok(())
    }
}

/* Whether nothing `chunk` makes outlives its run, so a later run may refill its scratch scope. */
fn leaves_nothing(chunk: &SSAChunk) -> bool {
    chunk.classes.is_empty() && chunk.functions.iter().all(|(params, body, ..)| super::super::scope::is_comp(params) && !body.is_generator && leaves_nothing(body))
}

/* An `eval` source without its leading spaces and tabs, which Python drops. */
fn strip_eval(source: &str) -> String {
    source.trim_start_matches([' ', '\t']).to_string()
}

/* Whether `source` parses as one expression, the case `single` echoes. */
fn is_expression(source: &str) -> bool {
    let (tokens, lex_errs) = lex(source);
    lex_errs.is_empty() && Parser::new(source, tokens.into_iter()).parse_eval().1.is_empty()
}
