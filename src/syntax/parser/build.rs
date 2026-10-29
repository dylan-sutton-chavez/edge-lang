use super::{quoted, Body, ModuleCache, Parser};
use crate::code::OpCode;
use crate::syntax::{chunk::{SSAChunk, Value}, diag::Diagnostic, ssa::{param_base_name, LoopCtx}};
use crate::lang::{Natives, ParseFn};
use crate::syntax::lexer::{lex_as, Spelling, Syntax, Token};
use crate::deps::Resolver;
use crate::s;

use alloc::{boxed::Box, rc::Rc, string::{String, ToString}, sync::Arc, vec::Vec};
use core::marker::PhantomData;
use core::ops::Range;

/* a chunk ready to run or every diagnostic that kept the source from making one */
pub type Parsed = Result<Rc<SSAChunk>, Vec<Diagnostic>>;

/* what a source parses as with where its imports resolve and how they parse */
pub struct Unit {
    pub(crate) resolver: Box<dyn Resolver>,
    // the file the code runs as where none keeps the chunk's own
    pub(crate) path: Option<Arc<String>>,
    pub(crate) body: Body,
    // the modules parsed so far shared with every unit an import makes
    pub(crate) cache: Option<ModuleCache>,
    pub(crate) modules: ParseFn,
}

impl Unit {
    pub(crate) fn new(resolver: Box<dyn Resolver>, body: Body, modules: ParseFn) -> Unit {
        Unit { resolver, path: None, body, cache: None, modules }
    }
}

/* the value free names read fields of with the runtime's natives keeping their own names */
pub(crate) struct Subject {
    name: String,
    natives: &'static Natives,
}

// a table whose parser lets free names read a subject which no other parser compiles
pub(crate) struct Subjected<S>(PhantomData<S>);

impl<S: Spelling> Spelling for Subjected<S> {
    const SYNTAX: Syntax = S::SYNTAX;
    const SUBJECT: bool = true;
    const NATIVES: &'static Natives = S::NATIVES;
    const OPS: &'static crate::lang::OpSet = S::OPS;
}

type Inner<'src, S> = Parser<'src, core::iter::Empty<Token>, Subjected<S>>;

/* builds a chunk through the parser's own emission for a front end of a runtime */
pub struct Builder<'b, 'src, S: Spelling> {
    p: &'b mut Inner<'src, S>,
    // where each line of the source starts for the lines tokens and functions carry
    starts: &'b [usize],
}

/* the chunk `f` builds for `source` as `unit` asks with every diagnostic raised */
pub fn build<S: Spelling>(source: &str, unit: Unit, f: impl FnOnce(&mut Builder<'_, '_, S>)) -> Parsed {
    let starts: Vec<usize> = core::iter::once(0).chain(source.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let Unit { resolver, path, body, cache, modules } = unit;
    let mut p = Inner::<S>::with_parts(source, core::iter::empty(), resolver, cache, modules);
    if let Some(path) = path { p.chunk.path = path; }
    f(&mut Builder { p: &mut p, starts: &starts });
    p.close(body);
    if p.errors.is_empty() { Ok(Rc::new(p.chunk)) } else { Err(p.errors) }
}

impl<'b, 'src, S: Spelling> Builder<'b, 'src, S> {
    /* the zero based line `offset` sits on */
    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    /* anchors a statement at byte `offset` so its errors and calls point there */
    pub fn at(&mut self, offset: usize) -> &mut Self {
        let offset = self.p.source.floor_char_boundary(offset);
        (self.p.last_end, self.p.last_line) = (offset, self.line(offset));
        self.p.mark_stmt();
        self
    }

    /* reports `msg` over `range` of the source which leaves the chunk empty */
    pub fn error(&mut self, range: Range<usize>, msg: &str) -> &mut Self {
        let src = self.p.source;
        self.p.reject(src.floor_char_boundary(range.start), src.ceil_char_boundary(range.end.max(range.start)), msg);
        self
    }

    /* pushes `v` the way a literal of it compiles */
    pub fn constant(&mut self, v: Value) -> &mut Self {
        match v {
            Value::Bool(true) => self.p.chunk.emit(OpCode::LoadTrue, 0),
            Value::Bool(false) => self.p.chunk.emit(OpCode::LoadFalse, 0),
            Value::None => self.p.chunk.emit(OpCode::LoadNone, 0),
            v => self.p.emit_const(v),
        }
        self
    }

    /* pushes the value bound to `name` whatever subject is set */
    pub fn load(&mut self, name: &str) -> &mut Self {
        let subject = self.p.subject.take();
        self.p.emit_load_ssa(name);
        self.p.subject = subject;
        self
    }

    /* binds the value on top to `name` */
    pub fn store(&mut self, name: &str) -> &mut Self {
        self.p.store_name(name);
        self
    }

    /* replaces the values on top with what runtime opcode `op` gives of them */
    pub fn op(&mut self, op: u8) -> &mut Self {
        match S::OPS.ops.get(op as usize) {
            Some(own) => self.p.chunk.emit(OpCode::RuntimeOp, u16::from(op) | u16::from(own.pops) << 8),
            None => self.p.error("the runtime has no opcode of that number"),
        }
        self
    }

    /* replaces the value on top with its field `name` */
    pub fn field(&mut self, name: &str) -> &mut Self {
        let i = self.p.chunk.push_name(name);
        self.p.chunk.emit(OpCode::LoadAttr, i);
        self
    }

    /* replaces a value and the key above it with its item at that key */
    pub fn item(&mut self) -> &mut Self {
        self.p.chunk.emit(OpCode::GetItem, 0);
        self
    }

    /* replaces the top `n` values with a list of them */
    pub fn list(&mut self, n: u16) -> &mut Self {
        self.p.chunk.emit(OpCode::BuildList, n);
        self
    }

    /* replaces the top `n` key and value pairs with a map of them */
    pub fn map(&mut self, n: u16) -> &mut Self {
        self.p.chunk.emit(OpCode::BuildDict, n);
        self
    }

    /* calls the value under `pos` arguments and `kw` keywords pushed each as name then value */
    pub fn call(&mut self, pos: u16, kw: u16) -> &mut Self {
        if pos > 0xFF || kw > 0xFF {
            let at = self.p.last_end;
            self.p.reject(at, at, "too many arguments in call (max 255 positional and 255 keyword)");
        }
        self.p.chunk.emit(OpCode::Call, super::pack_call(pos, kw));
        self.p.chunk.record_call_pos(self.p.last_end as u32);
        self
    }

    /* replaces the two values on top with what binary operator `op` makes of them */
    pub fn binary(&mut self, op: OpCode) -> &mut Self {
        if !op.is_binary() {
            let at = self.p.last_end;
            self.p.reject(at, at, "a builder applies only a binary operator");
        }
        self.p.chunk.emit(op, 0);
        self
    }

    /* drops the value on top */
    pub fn pop(&mut self) -> &mut Self {
        self.p.chunk.emit(OpCode::PopTop, 0);
        self
    }

    /* raises the value on top */
    pub fn throw(&mut self) -> &mut Self {
        self.p.chunk.emit(OpCode::Raise, 0);
        self
    }

    /* returns the value on top from the function being built */
    pub fn ret(&mut self) -> &mut Self {
        if !self.p.in_function {
            let at = self.p.last_end;
            self.p.reject(at, at, Inner::<'src, S>::RETURN_OUTSIDE);
        }
        self.p.chunk.emit(OpCode::ReturnValue, 0);
        self
    }

    /* binds the module `spec` resolves to under `alias` */
    pub fn import(&mut self, spec: &str, alias: &str) -> &mut Self {
        let at = self.p.last_end;
        self.p.resolve_and_bind_all(spec, (at, at), alias);
        self
    }

    /* free names in later expressions read fields of what `name` binds unless `natives` defines them */
    pub fn subject(&mut self, name: &str, natives: &'static Natives) -> &mut Self {
        self.p.subject = Some(Box::new(Subject { name: name.into(), natives }));
        self
    }

    /* free names in later expressions read their own bindings again */
    pub fn no_subject(&mut self) -> &mut Self {
        self.p.subject = None;
        self
    }

    /* one expression read from `range` of the source through table `S` */
    pub fn expr(&mut self, range: Range<usize>) -> &mut Self {
        if self.fragment(range) {
            let errors = self.p.errors.len();
            self.p.expr();
            if self.p.errors.len() == errors && self.p.peek().is_some() && let Some(&t) = self.p.tokens.peek() {
                let msg = s!("expected the end of the expression, got ", str &quoted(&self.p.source[t.start..t.end]));
                self.p.reject(t.start, t.end, &msg);
            }
        }
        self.p.tokens.toks.clear();
        self
    }

    /* the statements read from `range` of the source through table `S` leaving nothing */
    pub fn stmts(&mut self, range: Range<usize>) -> &mut Self {
        if self.fragment(range) { self.p.statements(); }
        self.p.tokens.toks.clear();
        self
    }

    /* lexes `range` without its blanks into the next tokens placed where it sits */
    fn fragment(&mut self, range: Range<usize>) -> bool {
        let src = self.p.source;
        let (start, end) = (range.start.min(src.len()), range.end.min(src.len()));
        let Some(text) = src.get(start..end.max(start)) else {
            // the refusal covers the characters the range cuts so its own span stays whole
            self.p.reject(src.floor_char_boundary(start), src.ceil_char_boundary(end.max(start)), "a fragment must start and end between characters");
            return false;
        };
        let lead = text.len() - text.trim_start_matches([' ', '\t']).len();
        let (start, text) = (start + lead, text[lead..].trim_end_matches([' ', '\t', '\r', '\n']));
        let line = self.line(start);
        let (mut toks, lex_errs) = lex_as::<S>(text);
        for e in lex_errs { self.p.errors.push(Diagnostic { start: start + e.start, end: start + e.end, msg: e.msg.into() }); }
        for t in &mut toks { (t.start, t.end, t.line) = (t.start + start, t.end + start, t.line + line); }
        let p = &mut *self.p;
        (p.tokens.toks, p.tokens.pos) = (toks, 0);
        (p.last_end, p.last_line) = (start, line);
        p.bracket_stack.clear();
        true
    }

    /* opens a chain of arms where only the first whose condition holds runs */
    pub fn arms(&mut self) -> Arms<'_, 'b, 'src, S> {
        self.p.enter_block();
        Arms { b: self, open: None, ends: Vec::new() }
    }

    /* runs `then` when `cond` leaves a true value and `other` when it does not */
    pub fn if_else(&mut self, cond: impl FnOnce(&mut Self), then: impl FnOnce(&mut Self), other: impl FnOnce(&mut Self)) -> &mut Self {
        self.arms().arm(cond, then).other(other);
        self
    }

    /* runs `body` for each item of the iterable on top with `targets` bound to it */
    pub fn for_each(&mut self, targets: &[&str], body: impl FnOnce(&mut Self)) -> &mut Self {
        let p = &mut *self.p;
        p.chunk.emit(OpCode::GetIter, 0);
        p.enter_block();
        let start = p.chunk.instructions.len() as u16;
        p.loops.push(LoopCtx { start, breaks: Vec::new(), is_for: true, cleanup_base: p.cleanup_count });
        let next = p.emit_jump(OpCode::ForIter);
        match targets {
            [one] => p.store_name(one),
            many => {
                p.chunk.emit(OpCode::UnpackSequence, many.len() as u16);
                for name in many { p.store_name(name); }
            }
        }
        body(self);
        let p = &mut *self.p;
        p.chunk.emit(OpCode::Jump, start);
        p.patch(next);
        for at in p.loops.pop().map(|l| l.breaks).unwrap_or_default() { p.patch(at); }
        p.commit_block();
        self
    }

    /* binds `name` to a function of `params` where `x=` takes a default pushed before */
    pub fn function(&mut self, name: &str, params: &[&str], mut body: impl FnMut(&mut Builder<'_, 'src, S>)) -> &mut Self {
        let (params, defaults) = signature(params);
        let starts = self.starts;
        let p = &mut *self.p;
        let first_line = p.last_line as u32 + 1;
        let cut = p.enter_qual(name, true);
        let restart = p.mark();
        let mut late = Vec::new();
        let mut chunk = p.with_fresh_chunk(|p| {
            open_body(p, &params);
            body(&mut Builder { p, starts });
            late = p.called_then_bound();
        });
        // a builtin called before the body binds it is local so the body builds twice
        if !late.is_empty() {
            p.rewind(restart);
            chunk = p.with_fresh_chunk(|p| {
                p.unfused.extend(late.iter().cloned());
                open_body(p, &params);
                body(&mut Builder { p, starts });
            });
        }
        chunk.is_generator = chunk.instructions.iter().any(|i| i.opcode == OpCode::Yield);
        p.leave_qual(cut);
        (chunk.posonly, chunk.first_line) = (0, first_line);
        p.push_function(params, chunk, defaults, Some(name), OpCode::MakeFunction);
        p.emit_store_new(name);
        self
    }

    /* pushes a function of `params` returning what `body` leaves on top */
    pub fn lambda(&mut self, params: &[&str], body: impl FnOnce(&mut Builder<'_, 'src, S>)) -> &mut Self {
        let (params, defaults) = signature(params);
        let starts = self.starts;
        let p = &mut *self.p;
        let cut = p.enter_qual(S::NATIVES.names().anonymous, true);
        let mut outer = core::mem::take(&mut p.ssa_versions);
        let mut chunk = p.with_fresh_chunk(|p| {
            let held = p.lend_versions(&mut outer, &params);
            p.in_function = true;
            body(&mut Builder { p, starts });
            p.chunk.emit(OpCode::ReturnValue, 0);
            p.return_versions(&mut outer, held);
        });
        p.ssa_versions = outer;
        chunk.is_generator = chunk.instructions.iter().any(|i| i.opcode == OpCode::Yield);
        p.leave_qual(cut);
        p.push_function(params, chunk, defaults, None, OpCode::MakeFunction);
        self
    }
}

/* the parameters as a chunk keeps them with how many take a default */
fn signature(params: &[&str]) -> (Vec<String>, u16) {
    let defaults = params.iter().filter(|p| p.ends_with('=')).count() as u16;
    (params.iter().map(|p| p.to_string()).collect(), defaults)
}

/* binds each parameter at version zero so the body reads it before any store */
fn open_body<S: Spelling>(p: &mut Inner<'_, S>, params: &[String]) {
    p.in_function = true;
    p.in_async = false;
    for param in params {
        p.bind_param(param);
        let _ = p.push_ssa_name(param_base_name(param), 0);
    }
}

/* conditional arms whose join closes on drop so no front end leaves one open */
pub struct Arms<'a, 'b, 'src, S: Spelling> {
    b: &'a mut Builder<'b, 'src, S>,
    // the false jump of the last arm until another arm or the end lands it
    open: Option<usize>,
    ends: Vec<usize>,
}

impl<'b, 'src, S: Spelling> Arms<'_, 'b, 'src, S> {
    /* an arm whose body runs when `cond` holds and no arm before it ran */
    pub fn arm(mut self, cond: impl FnOnce(&mut Builder<'b, 'src, S>), body: impl FnOnce(&mut Builder<'b, 'src, S>)) -> Self {
        self.split();
        cond(self.b);
        self.open = Some(self.b.p.emit_jump(OpCode::JumpIfFalse));
        body(self.b);
        self
    }

    /* the body that runs when no arm ran which ends the chain */
    pub fn other(mut self, body: impl FnOnce(&mut Builder<'b, 'src, S>)) {
        self.split();
        body(self.b);
    }

    // the arm before jumps past the chain and its false jump lands on what follows
    fn split(&mut self) {
        let Some(open) = self.open.take() else { return };
        self.ends.push(self.b.p.emit_jump(OpCode::Jump));
        self.b.p.mid_block();
        self.b.p.patch(open);
    }
}

impl<S: Spelling> Drop for Arms<'_, '_, '_, S> {
    fn drop(&mut self) {
        if let Some(open) = self.open.take() { self.b.p.patch(open); }
        for at in core::mem::take(&mut self.ends) { self.b.p.patch(at); }
        self.b.p.commit_block();
    }
}

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    /* reads free name `name` as a field of the subject or is false */
    pub(super) fn subject_field(&mut self, name: &str) -> bool {
        let Some(subject) = self.subject.take() else { return false };
        let natives = subject.natives;
        let free = name != subject.name && !self.globals_decl.contains(name) && !self.ssa_versions.contains(name)
            && natives.find(name).is_none() && !natives.types.contains(&name);
        if free {
            self.emit_load_ssa(&subject.name);
            // the field reads as the table reads a name after a dot
            if Self::KEYS {
                self.emit_const(Value::Str(name.to_string()));
                self.chunk.emit(OpCode::GetItem, 0);
            } else {
                let i = self.chunk.push_name(name);
                self.chunk.emit(OpCode::LoadAttr, i);
            }
        }
        self.subject = Some(subject);
        free
    }
}
