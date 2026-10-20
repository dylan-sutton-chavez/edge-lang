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
pub(crate) const EVAL_NESTING: usize = 2;

/* What a run of text keeps until its code ends, its chunk, where its bindings go, how its value comes back, the stack lengths and traceback. */
pub(crate) struct SourceRun {
    at: usize,
    scratch: Option<Scratch>,
    globals: Option<Val>,
    owner: Option<usize>,
    mode: CodeMode,
    frames: usize,
    depths: [usize; 7],
    caller: *const SSAChunk,
    // Where the call that ran it lies in its caller, and whether it is `eval`, for the frame an error in it notes.
    call_ip: Option<u32>,
    eval: bool,
    // The builtin's step or job that called the text, which takes what it gives.
    feed: Option<Fed>,
    // Where the loop carries on once that took it, when not past the instruction that ran it, which then runs again.
    back: Option<usize>,
}

/* How a floor's run ended, the value its instruction takes, or where the loop carries on once the builtin callback that called it took the result. */
pub(crate) enum Ended { Value(Val), At(usize) }

/* What called a run of text as a builtin's callback, which takes what the run gives. */
pub(crate) enum Fed {
    // The step of a builtin `map` or `filter` that called it, with its iterator, the item a `filter` tests, and how the step takes the result.
    Feed(Val, Val, crate::vm::jobs::FeedKind, bool),
    // A job calling it as its key, the key, `eval` or `exec`, and the job.
    Key(Val, alloc::boxed::Box<crate::vm::jobs::Job>),
    // The `for` at that index of the running code stepping its own `map` or `filter`, taking the result as its item or as the test of the item.
    For(usize, Option<Val>),
    // A `min` or `max` whose key it is.
    Keys(alloc::boxed::Box<KeysRun>),
}

/* A `min` or `max` with an `eval` or `exec` key, each key a floor of the loop, with the key, items, keys back so far and whether it takes the least. */
pub(crate) struct KeysRun { pub(crate) key: Val, pub(crate) items: Vec<Val>, pub(crate) keys: Vec<Val>, pub(crate) is_min: bool }

/* What a class body run as a floor of the loop keeps until it ends, its bases, which names are members and the class's name. */
pub(crate) struct ClassRun {
    pub(crate) bases: Vec<Val>,
    pub(crate) member: Vec<bool>,
    pub(crate) name: String,
    // The keyword pairs of its statement, handed to `__init_subclass__`.
    pub(crate) keywords: Vec<Val>,
}

/* What a floor runs, a run of text, a class body or the top level of a module a run of text imports, by its spec. */
pub(crate) enum FloorRun { Source(alloc::boxed::Box<SourceRun>), Class(ClassRun), Import(String) }

/* The floor a run of text, class body or import set aside, which the loop takes back once that code ends, its code, chunk, depth and bases. */
pub(crate) struct Floored {
    pub(crate) code: Option<Rc<crate::vm::lower::Code>>,
    pub(crate) once: bool,
    pub(crate) chunk: *const SSAChunk,
    pub(crate) pool: usize,
    pub(crate) depth: usize,
    pub(crate) bases: [usize; 3],
    pub(crate) safe: bool,
    pub(crate) ip: usize,
    pub(crate) rip: usize,
    pub(crate) slots: Vec<Val>,
    pub(crate) cost: usize,
    pub(crate) run: FloorRun,
}

impl Floored {
    /* A floor of `chunk` starting from `slots` that runs `run`, counting `cost` toward the depth. */
    pub(crate) fn new(chunk: *const SSAChunk, slots: Vec<Val>, cost: usize, run: FloorRun) -> Self {
        Self { code: None, once: false, chunk, pool: 0, depth: 0, bases: [0; 3], safe: false, ip: 0, rip: 0, slots, cost, run }
    }

    /* Every value it holds, which a collection keeps. */
    pub(crate) fn for_each_val(&self, f: &mut impl FnMut(Val)) {
        for &v in &self.slots { f(v); }
        match &self.run {
            FloorRun::Source(r) => {
                if let Some(g) = r.globals { f(g); }
                match &r.feed {
                    Some(Fed::Feed(t, i, ..)) => { f(*t); f(*i); }
                    Some(Fed::Key(k, job)) => { f(*k); job.for_each_val(f); }
                    Some(Fed::For(_, Some(i))) => f(*i),
                    Some(Fed::For(_, None)) => {}
                    Some(Fed::Keys(r)) => { f(r.key); for &v in r.items.iter().chain(&r.keys) { f(v) } }
                    None => {}
                }
                if let Some(sc) = &r.scratch {
                    for &v in sc.globals.iter().chain(&sc.locals).chain(&sc.default_name) { f(v); }
                    for &(_, v) in &sc.names { f(v); }
                }
            }
            FloorRun::Class(c) => for &v in c.bases.iter().chain(&c.keywords) { f(v) },
            FloorRun::Import(_) => {}
        }
    }
}

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
        // Called where a frame may open in place, the text runs in the very loop that called it.
        let floor = core::mem::take(&mut self.job_ok);
        let args = self.pop_n(operand as usize)?;
        if floor { return self.source_floor(&args, CodeMode::Eval, "eval", chunk); }
        Err(self.unfloored("eval"))
    }

    /* `exec(source, globals, locals)`, statements run for their bindings. */
    pub fn call_exec(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let floor = core::mem::take(&mut self.job_ok);
        let args = self.pop_n(operand as usize)?;
        if floor { return self.source_floor(&args, CodeMode::Exec, "exec", chunk); }
        Err(self.unfloored("exec"))
    }

    /* Hands the run of the source in `args` to the loop that called exec or eval, which runs its chunk as its own floor, ended by `source_end`. */
    fn source_floor(&mut self, args: &[Val], asked: CodeMode, name: &str, caller: &SSAChunk) -> Result<(), VmErr> {
        let chunk = self.source_begin(args, asked, name, caller)?;
        // Its imports run first, and it nests as deep as a run of its own would.
        let mut pending = Vec::new();
        let ready = if self.depth + EVAL_NESTING >= self.max_calls { Err(cold_depth()) } else {
            let mut in_progress = crate::util::hash::FxHashSet::default();
            self.pending_modules(chunk, &mut in_progress, &mut pending)
        };
        if let Err(e) = ready { return self.source_end(Err(e), false).map(|_| ()); }
        let slots = self.source_slots(chunk);
        if let Some(f) = self.floors.last_mut() { f.slots = slots; }
        // The modules it imports run first, each a floor of the loop, the first on top.
        for (spec, sub) in pending.into_iter().rev() {
            // SAFETY the chunk a resolved import compiled to lives as long as the chunk that imports it.
            let slots = self.module_slots(&spec, unsafe { &*sub })?;
            self.floors.push(Floored::new(sub, slots, 1, FloorRun::Import(spec)));
        }
        (self.floor_req, self.yielded) = (true, true);
        Ok(())
    }

    /* The slots a compiled chunk starts from, every builtin it names unless it runs under builtins of its own. */
    fn source_slots(&self, chunk: &SSAChunk) -> Vec<Val> {
        if self.scopes[self.chunk_module_id(chunk)].closed { alloc::vec![Val::undef(); chunk.names.len()] } else { self.fill_builtins(&chunk.names) }
    }

    /* `compile(source, filename, mode)`, a code object whose source already parsed. */
    #[inline(never)]
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

    /* The internal error of an `eval` or `exec` called where no floor of the running loop takes its text, which debug builds stop at. */
    #[cold]
    #[inline(never)]
    fn unfloored(&self, name: &str) -> VmErr {
        debug_assert!(false, "{name} called where no floor of the running loop takes its text");
        VmErr::Runtime("internal error: eval or exec called where no floor of the running loop takes its text")
    }

    /* Compiles the source in `args` as `eval` or `exec` asks and readies its scope, what its run keeps going on top of the floors set aside. */
    fn source_begin(&mut self, args: &[Val], asked: CodeMode, name: &str, caller: &SSAChunk) -> Result<&'a SSAChunk, VmErr> {
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
        let run = SourceRun { at, scratch, globals, owner, mode, frames, depths, caller: caller as *const SSAChunk, call_ip, eval: name == "eval", feed: None, back: None };
        self.floors.push(Floored::new(chunk, Vec::new(), EVAL_NESTING + 1, FloorRun::Source(alloc::boxed::Box::new(run))));
        Ok(chunk)
    }

    /* Ends the top floor whose code gave `ran` (`yielded` if it suspended), giving its instruction's value, or for a callback's run where `code` goes. */
    #[inline(never)]
    pub(crate) fn floor_end(&mut self, ran: Result<Val, VmErr>, yielded: bool, at: Option<(usize, &crate::vm::lower::Code)>) -> Result<Ended, VmErr> {
        let fed = if at.is_some() { self.floor_feed() } else { None };
        let ran = match self.floors.last().map(|f| &f.run) {
            Some(FloorRun::Class(_)) => self.class_end(ran, yielded, at.is_some()),
            Some(FloorRun::Import(_)) => self.import_end(ran, yielded),
            _ => self.source_end(ran, yielded),
        };
        match (fed, at) {
            (Some((fed, again)), Some((back, code))) => self.feed_end(fed, ran, again.unwrap_or(back), code).map(Ended::At),
            _ => ran.map(Ended::Value),
        }
    }

    /* Whether `f` is `eval` or `exec`, whose call where a frame may open runs its text as a floor of the loop. */
    pub(crate) fn runs_text(&self, f: Val) -> bool { matches!(self.heap.try_get(f), Some(HeapObj::NativeFn(NativeFnId::Eval | NativeFnId::Exec))) }

    /* Marks the run of text last set aside as the callback of what `fed` names, which takes what it gives. */
    pub(crate) fn feed_floor(&mut self, fed: Fed) {
        if let Some(FloorRun::Source(r)) = self.floors.iter_mut().rev().map(|f| &mut f.run).find(|r| matches!(r, FloorRun::Source(_))) { r.feed = Some(fed); }
    }

    /* What called the run of text on top as a callback, taken off it as it ends, and where the loop carries on after. */
    pub(crate) fn floor_feed(&mut self) -> Option<(Fed, Option<usize>)> {
        match self.floors.last_mut().map(|f| &mut f.run) { Some(FloorRun::Source(r)) => r.feed.take().map(|f| (f, r.back.take())), _ => None }
    }

    /* Has the run of text last set aside as a callback end with the instruction at `rip` run again. */
    pub(crate) fn floor_back(&mut self, rip: usize) {
        if let Some(FloorRun::Source(r)) = self.floors.iter_mut().rev().map(|f| &mut f.run).find(|r| matches!(r, FloorRun::Source(_))) { r.back = Some(rip); }
    }

    /* Calls `key`, `eval` or `exec`, on `arg` for `job`, its text set aside as the floor the loop takes next, whose end hands the job the result. */
    #[inline(never)]
    pub(crate) fn key_floor(&mut self, key: Val, arg: Val, job: crate::vm::jobs::Job) -> Result<(), VmErr> {
        self.text_floor(key, arg, Fed::Key(key, alloc::boxed::Box::new(job)))
    }

    /* Calls `f`, `eval` or `exec`, on `arg`, its text set aside as the floor the loop takes next, whose end hands `fed` the result. */
    fn text_floor(&mut self, f: Val, arg: Val, fed: Fed) -> Result<(), VmErr> {
        let chunk = self.chunk;
        self.push(f);
        self.push(arg);
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (true, self.frame_safe, true);
        let called = self.exec_call_n(1, 0, chunk);
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (false, false, false);
        called?;
        if !self.floor_req { return Err(cold_runtime("a callback's text opened no floor")); }
        self.feed_floor(fed);
        Ok(())
    }

    /* `min` or `max` of `items` by `key`, `eval` or `exec`, each key a floor of the loop, the winner given once the last is back. */
    #[inline(never)]
    pub(crate) fn keys_floor(&mut self, key: Val, items: Vec<Val>, is_min: bool) -> Result<(), VmErr> {
        let first = items[0];
        let keys = Vec::with_capacity(items.len());
        self.text_floor(key, first, Fed::Keys(alloc::boxed::Box::new(KeysRun { key, items, keys, is_min })))
    }

    /* Calls `f`, `eval` or `exec`, on the next items of `its` for the `for` at `i` stepping its own `map` as the next floor, Ok(false) once one ends. */
    #[inline(never)]
    pub(crate) fn text_for(&mut self, f: Val, its: &[Val], i: usize, filter: bool) -> Result<bool, VmErr> {
        let chunk = self.chunk;
        let base = self.stack.len();
        self.push(f);
        for &c in its {
            match self.next_of(c, chunk) {
                Ok(Some(x)) => self.push(x),
                // One spent ends the step, which meets it spent again and the items taken before it.
                Ok(None) => {
                    let taken = self.stack.split_off(base + 1);
                    self.stack.truncate(base);
                    self.give_back(&its[..taken.len()], &taken);
                    self.pend.push((c, Val::undef()));
                    return Ok(false);
                }
                Err(e) => { self.stack.truncate(base); return Err(e); }
            }
        }
        // A `filter` keeps the item it tests, which the call takes off the stack.
        let item = filter.then(|| self.stack.get(base + 1).copied().unwrap_or(Val::undef()));
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (true, self.frame_safe, true);
        let called = self.exec_call_n(its.len(), 0, chunk);
        (self.inline_call, self.pending_exec_safe, self.job_ok) = (false, false, false);
        called?;
        if !self.floor_req { return Err(cold_runtime("a for's text opened no floor")); }
        self.feed_floor(Fed::For(i, item));
        Ok(true)
    }

    /* Hands `fed`, what called a run of text as a callback, the result `ran` of the floor that ended at `back` of `code`, giving where the loop goes. */
    #[inline(never)]
    pub(crate) fn feed_end(&mut self, fed: Fed, ran: Result<Val, VmErr>, back: usize, code: &crate::vm::lower::Code) -> Result<usize, VmErr> {
        use crate::vm::jobs::Step;
        let stop = |e: &VmErr| matches!(e, VmErr::Raised(m) if m == "StopIteration" || m.starts_with("StopIteration:"));
        match fed {
            Fed::For(i, item) => match ran {
                Ok(v) => match item {
                    None => { self.hand_to_for(code, i, v); Ok(i + 1) }
                    // A false test steps the `for` again.
                    Some(item) => {
                        let chunk = self.chunk;
                        self.push(v);
                        let keep = self.truthy_op(v, chunk);
                        self.pop()?;
                        if !keep? { return Ok(i); }
                        self.hand_to_for(code, i, item);
                        Ok(i + 1)
                    }
                },
                // A StopIteration ends the loop, as one out of the function a `map` calls does.
                Err(e) if stop(&e) => {
                    (self.error_byte_pos, self.yield_from_value) = (None, self.swallow_stop(None));
                    self.call_stack.clear();
                    self.iter_stack.pop();
                    Ok(code.ins.get(i).map_or(i, |ins| ins.a as usize))
                }
                Err(e) => Err(e),
            },
            Fed::Feed(target, item, kind, drive) => {
                match ran {
                    Ok(v) => { let chunk = self.chunk; self.feed_take(target, item, Val::undef(), kind, v, chunk)?; }
                    Err(e) if stop(&e) => {
                        self.pend.push((target, Val::undef()));
                        (self.error_byte_pos, _) = (None, self.swallow_stop(None));
                        self.call_stack.clear();
                    }
                    Err(e) => return Err(e),
                }
                if !drive { return Ok(back); }
                let next = self.gather_drive()?;
                Ok(self.after_drive(next, back, crate::vm::act::NO_DEST))
            }
            Fed::Key(key, job) => {
                let v = ran?;
                self.jobs.push(*job);
                let step = self.job_take(v);
                let Some(job) = self.jobs.pop() else { return Err(cold_runtime("a key's job gone")) };
                match step? {
                    // The next key runs as the next floor, which ends here again.
                    Step::Again(next) => { self.key_floor(key, next, job)?; Ok(back) }
                    Step::Give(r) => Ok(self.deliver(r, back, crate::vm::act::NO_DEST)),
                    _ => Err(cold_runtime("a key's job took a step it has none of")),
                }
            }
            Fed::Keys(mut r) => {
                r.keys.push(ran?);
                // The next key runs as the next floor, which ends here again.
                if let Some(&next) = r.items.get(r.keys.len()) { self.text_floor(r.key, next, Fed::Keys(r))?; return Ok(back); }
                let (chunk, KeysRun { items, keys, is_min, .. }) = (self.chunk, *r);
                let roots = if self.any_instance(&keys) { items.len() + keys.len() } else { 0 };
                let best = self.with_roots(items.iter().chain(&keys).copied().take(roots), |vm| vm.extreme_index(&keys, is_min, chunk))?;
                Ok(self.deliver(items[best], back, crate::vm::act::NO_DEST))
            }
        }
    }

    /* Whether the top floor waits for the loop to take it, a module a run of text imports after the one that just ended, or the text itself. */
    pub(crate) fn floor_pending(&self, above: usize) -> bool { self.floors.len() > above && self.floors.last().is_some_and(|f| f.code.is_none()) }

    /* Ends the top module floor whose top level gave `ran`, binding the module for the importing text, or on error ending it and its pending imports. */
    fn import_end(&mut self, ran: Result<Val, VmErr>, yielded: bool) -> Result<Val, VmErr> {
        let Some(f) = self.floors.pop() else { return ran };
        let FloorRun::Import(spec) = f.run else { return ran };
        let ran = if yielded { Err(VmErr::Runtime("a module's top level cannot suspend while a run of text imports it")) } else { ran };
        match ran {
            Ok(_) => {
                // SAFETY the VM keeps every chunk it runs until it drops.
                let sub: &SSAChunk = unsafe { &*f.chunk };
                let module = self.chunk_module_id(sub);
                let attrs = crate::vm::init::collect_module_attrs(sub, &self.scopes[module]);
                let val = self.heap.alloc(HeapObj::Module(spec.clone(), attrs))?;
                self.module_table.insert(spec, val);
                Ok(Val::none())
            }
            Err(e) => {
                // The modules still to import go, and the text ends with the error.
                while self.floors.last().is_some_and(|f| f.code.is_none() && matches!(f.run, FloorRun::Import(_))) { self.floors.pop(); }
                self.source_end(Err(e), false)
            }
        }
    }

    /* Ends the top run of text whose code gave `ran` (`yielded` if it suspended), handing its bindings to the dicts that asked and giving its value. */
    pub(crate) fn source_end(&mut self, mut ran: Result<Val, VmErr>, yielded: bool) -> Result<Val, VmErr> {
        let Some(FloorRun::Source(run)) = self.floors.last_mut().map(|f| &mut f.run) else { return ran };
        let SourceRun { at, globals, owner, mode, frames, depths, caller, call_ip, eval, .. } = **run;
        let scratch = run.scratch.take();
        self.floors.truncate(self.floors.len() - 1);
        // An error in the text keeps the frames inside it, then the call that ran it, which the error now points at.
        if ran.is_err() {
            self.error_byte_pos = None;
            // SAFETY the chunk that called lives as long as the VM.
            let caller: &SSAChunk = unsafe { &*caller };
            let call_byte_pos = call_ip.and_then(|ip| caller.resolve_call(ip).or_else(|| caller.resolve(ip))).unwrap_or(0);
            let fi = if eval { crate::value::EVAL_FRAME } else { crate::value::EXEC_FRAME };
            if self.call_stack.len() >= frames {
                self.call_stack.push(CallFrame { fi, call_byte_pos, caller_source: Some(caller.source.clone()), caller_path: Some(caller.path.clone()), repeats: 0 });
            }
        }
        // A native call holds the run, so a suspension inside it has nowhere to resume and its frames end here.
        if yielded {
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
            // SAFETY the chunk that called lives as long as the VM.
            let shown = self.repr_op(value, unsafe { &*caller })?;
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

    /* Gives the function `fi` of a link the code its first call runs as a frame, as `cold_code` gives any plain function. */
    pub(crate) fn linked_code(&mut self, fi: usize) -> bool {
        let pool = self.fn_pool[fi];
        if self.pools[pool].code.is_none() || self.pools[pool].cold {
            // A link keeps its functions off the paths that never sync the dict, which a call that does takes as any other.
            self.call_facts[fi].in_loop = true;
            self.cold_code(fi);
            self.call_facts[fi].in_loop = false;
        }
        self.pools[pool].code.is_some()
    }

    /* Opens function `fi` of link `at` as a frame of the loop, its scope read from the dict when no call into it runs, its end handing the dict back. */
    pub(crate) fn enter_linked(&mut self, fi: usize, callee: Val, at: usize) -> Result<(), VmErr> {
        self.enter_link(at);
        if let Err(e) = self.enter_frame(fi, callee) { let _ = self.leave_link(at); return Err(e); }
        let h = self.head_at(self.top_pool);
        self.regs[h] = Val::int(self.regs[h].as_int() | crate::vm::act::TASK << 40);
        Ok(())
    }

    /* Whether the running frame is a function exec defined in a dict, or a helper a coroutine waits inside with more kept under it. */
    #[inline(always)]
    pub(crate) fn top_linked(&self) -> bool {
        use crate::vm::act::{GEN, TASK};
        (!self.linked.is_empty() || !self.chain.chains.is_empty()) && self.acts != 0 && self.head(self.head_at(self.top_pool)).as_int() >> 40 & (GEN | TASK) == TASK
    }

    /* Hands the dict of the link of the running frame back what its scope bound, once, as the frame ends. */
    #[cold]
    #[inline(never)]
    pub(crate) fn unlink_top(&mut self) -> Result<(), VmErr> {
        // A helper parking with its coroutine leaves the ones kept under it as they are.
        if self.chain_top().is_some() { return Ok(()); }
        let h = self.head_at(self.top_pool);
        self.regs[h] = Val::int(self.regs[h].as_int() & !(crate::vm::act::TASK << 40));
        let fi = (self.head(h + 1).as_int() >> 24) as usize;
        match self.link_of(fi) { Some(at) => self.leave_link(at), None => Ok(()) }
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
