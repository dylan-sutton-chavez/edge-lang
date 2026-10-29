mod stmt;
mod control;
mod expr;
mod literals;
mod imports;
mod build;

pub use build::{build, Arms, Builder, Parsed, Unit};

use crate::s;
use crate::code::OpCode;
use crate::syntax::{chunk::{SSAChunk, Value, MAX_EXPR_DEPTH}, diag::Diagnostic, ssa::{param_base_name, JoinNode, LoopCtx, Versions}};
use crate::syntax::lexer::{Ends, Spelling, Token, TokenType};
use crate::util::hash::FxHashMap as HashMap;
use crate::deps::Resolver;

use alloc::{boxed::Box, string::{String, ToString}, vec::Vec};
use core::marker::PhantomData;

// the opener of a bracket given either side
#[inline]
pub(super) const fn bracket_open(k: TokenType) -> TokenType {
    match k {
        TokenType::ParenOpen | TokenType::ParenClose => TokenType::ParenOpen,
        TokenType::BracketOpen | TokenType::BracketClose => TokenType::BracketOpen,
        TokenType::BraceOpen | TokenType::BraceClose => TokenType::BraceOpen,
        _ => k,
    }
}

// the closer of a bracket given either side
#[inline]
pub(super) const fn bracket_close(k: TokenType) -> TokenType {
    match k {
        TokenType::ParenOpen | TokenType::ParenClose => TokenType::ParenClose,
        TokenType::BracketOpen | TokenType::BracketClose => TokenType::BracketClose,
        TokenType::BraceOpen | TokenType::BraceClose => TokenType::BraceClose,
        _ => k,
    }
}

// a call operand with the keyword count high and the positional count low
#[inline]
pub(super) const fn pack_call(pos: u16, kw: u16) -> u16 {
    ((kw & 0xFF) << 8) | (pos & 0xFF)
}

// the positional and keyword counts of a call operand
pub const fn unpack_call(operand: u16) -> (u16, u16) { (operand & 0xFF, operand >> 8) }

// marks a binary op written as `op=` which may update its left side in place
pub const INPLACE: u16 = 1;

// a GetIter operand leaving the iterator a comprehension receives on the stack
pub const ITER_VALUE: u16 = 2;

// a GetIter operand looping over an iterator without asking it for one again
pub const ITER_AS_IS: u16 = 3;

// the Yield operand of the loop a `yield from` compiles to
pub const YIELD_FROM: u16 = 1;

// the compiled chunk of each spec in a scanned Vec to avoid a hashbrown monomorphization
pub(crate) type ModuleCache = alloc::rc::Rc<core::cell::RefCell<Vec<(String, Option<alloc::rc::Rc<SSAChunk>>)>>>;

// diagnostic texts a table words at compile time from its own spellings
macro_rules! worded {
    ($($text:ident $name:ident = [$($part:expr),* $(,)?];)*) => { $(
        const $text: &'static ([u8; 128], usize) = &crate::syntax::lexer::join(&[$($part),*]);
        pub(super) const $name: &'static str = crate::syntax::lexer::joined(Self::$text);
    )* };
}

/* every token behind a cursor so a comprehension can compile its element after its clauses */
pub(super) struct Tokens<I: Iterator<Item = Token>, S: Spelling> {
    toks: Vec<Token>,
    pub(super) pos: usize,
    from: PhantomData<(I, S)>,
}

impl<I: Iterator<Item = Token>, S: Spelling> Tokens<I, S> {
    #[inline]
    pub(super) fn peek(&self) -> Option<&Token> { self.toks.get(self.pos) }

    #[inline]
    pub(super) fn next(&mut self) -> Option<Token> {
        let t = self.toks.get(self.pos).copied();
        self.pos += t.is_some() as usize;
        t
    }

    /* the kind of the next token past bracket line breaks and comments */
    fn ahead(&mut self) -> TokenType {
        while self.peek().is_some_and(|t| matches!(t.kind, TokenType::LineBreak | TokenType::Comment)) { self.pos += 1; }
        self.peek().map_or(TokenType::Eof, |t| t.kind)
    }

    /* the index where a plain annotation of names dots subscripts and unions meets `end` */
    pub(super) fn plain_annotation(&self, end: fn(TokenType) -> bool) -> Option<usize> {
        let (mut i, mut depth, mut atom) = (self.pos, 0u32, true);
        loop {
            match self.toks.get(i)?.kind {
                TokenType::Name | TokenType::Nil | TokenType::Ellipsis | TokenType::String | TokenType::Int if atom => atom = false,
                TokenType::Dot if !atom && self.toks.get(i + 1).is_some_and(|t| t.kind == TokenType::Name) => i += 1,
                TokenType::BracketOpen if !atom => (depth, atom) = (depth + 1, true),
                TokenType::BracketClose if !atom && depth > 0 => depth -= 1,
                TokenType::Comma if !atom && depth > 0 => atom = true,
                TokenType::BitOr if !atom => atom = true,
                k if !atom && depth == 0 && end(k) => return Some(i),
                _ => return None,
            }
            i += 1;
        }
    }

    /* the kind of the token after the one at the cursor */
    pub(super) fn after_head(&self) -> TokenType {
        self.toks.get(self.pos + 1).map_or(TokenType::Eof, |t| t.kind)
    }

    /* moves past the bracket group the cursor opens */
    fn skip_group(&mut self) {
        let mut depth = 0usize;
        while let Some(t) = self.next() {
            match t.kind {
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose if depth > 1 => depth -= 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose | TokenType::Newline | TokenType::Eof => return,
                _ => {}
            }
        }
    }

    /* whether `k` follows the bracket group at the cursor which stays where it was */
    pub(super) fn group_then(&mut self, k: TokenType) -> bool {
        let at = self.pos;
        self.skip_group();
        let then = self.ahead() == k;
        self.pos = at;
        then
    }

    /* moves past one target such as `*a` or `b.c[i]` uncompiled and is false if invalid */
    fn skip_target(&mut self) -> bool {
        if self.ahead() == TokenType::Star { self.pos += 1; }
        match self.ahead() {
            TokenType::Name | TokenType::Underscore => self.pos += 1,
            TokenType::ParenOpen | TokenType::BracketOpen => self.skip_group(),
            _ => return false,
        }
        loop {
            match self.ahead() {
                TokenType::Dot => {
                    self.pos += 1;
                    if !matches!(self.ahead(), TokenType::Name | TokenType::Underscore) { return false; }
                    self.pos += 1;
                }
                TokenType::ParenOpen | TokenType::BracketOpen => self.skip_group(),
                _ => return true,
            }
        }
    }

    /* moves past a target list returning its shape or None if invalid */
    pub(super) fn target_list(&mut self) -> Option<Shape> {
        let (mut n, mut star, mut comma) = (0, None, false);
        // an empty group as in `() = []`
        if matches!(self.ahead(), TokenType::ParenClose | TokenType::BracketClose) { return Some((n, star, comma)); }
        loop {
            if self.ahead() == TokenType::Star {
                if star.is_some() { return None; }
                star = Some(n);
            }
            if !self.skip_target() { return None; }
            n += 1;
            if self.ahead() != TokenType::Comma { return Some((n, star, comma)); }
            self.pos += 1;
            comma = true;
            // a trailing comma ends the list only where a target may end
            let next = self.ahead();
            if !matches!(next, TokenType::Star | TokenType::Name | TokenType::Underscore | TokenType::ParenOpen | TokenType::BracketOpen) {
                return ends_target::<S>(next).then_some((n, star, comma));
            }
        }
    }

    /* where the group opening at `open` closes and whether a target ends right after it */
    pub(super) fn group_target(&mut self, open: usize) -> (usize, bool) {
        let at = core::mem::replace(&mut self.pos, open);
        self.skip_group();
        let close = self.pos - 1;
        let ends = ends_target::<S>(self.ahead());
        self.pos = at;
        (close, ends)
    }
}

/* the length and starred item of a target list and whether it had a comma */
pub(super) type Shape = (usize, Option<usize>, bool);

/* whether `k` ends an assignment or `for` target */
pub(super) fn ends_target<S: Spelling>(k: TokenType) -> bool {
    matches!(k, TokenType::Equal | TokenType::In | TokenType::Comma | TokenType::Colon | TokenType::ParenClose | TokenType::BracketClose)
        || (S::SYNTAX.delimited() && k == TokenType::BlockOpen)
}

/* `text` quoted and cut after 40 chars so a huge token never fills a message */
fn quoted(text: &str) -> String {
    let cut = text.char_indices().nth(40).map_or(text.len(), |(i, _)| i);
    s!("'", str &text[..cut], str if cut < text.len() { "...'" } else { "'" })
}

/* cursor and diagnostics state so a statement can compile its tokens out of source order */
#[derive(Clone, Copy)]
pub(super) struct Mark {
    pos: usize,
    errors: usize,
    unbound: usize,
    line: usize,
    end: usize,
    closed: bool,
    newline: bool,
}

pub struct Parser<'src, I: Iterator<Item = Token>, S: Spelling> {
    pub(super) source: &'src str,
    pub(super) tokens: Tokens<I, S>,
    pub(crate) chunk: SSAChunk,
    pub(super) ssa_versions: Versions,
    /* names declared `global` in the current function body whose loads and stores go to `self.globals` */
    pub(super) globals_decl: crate::util::hash::FxHashSet<String>,
    pub(super) join_stack: Vec<JoinNode>,
    /* each store inside an open block as its slot and replaced version for the joins */
    pub(super) trail: Vec<(u32, u32)>,
    /* set once a block takes its dedent since the statement it closed is complete */
    pub(super) block_closed: bool,
    pub(super) last_line: usize,
    /* the end of the last token which anchors diagnostics once `peek()` skipped a Newline */
    pub(super) last_end: usize,
    pub(super) loops: Vec<LoopCtx>,
    /* the finally blocks open in the current function that break and continue cross */
    pub(super) cleanup_count: usize,
    pub(super) expr_depth: usize,
    pub(super) saw_newline: bool,
    /* unclosed brackets with the error count at opening for "never closed" and dropping cascades */
    pub(super) bracket_stack: Vec<(TokenType, usize, usize, usize)>,
    /* true while the statements of a class body compile but not those of its methods */
    pub(super) in_class_body: bool,
    /* the scopes around the compiling code which name its definitions in full */
    pub(super) qual: String,
    // whether a function encloses the scope the path ends in
    pub(super) around_fn: bool,
    /* the parameters before a `/` just parsed and the line of a decorator `@` */
    pub(super) posonly: u16,
    pub(super) decorated_at: Option<usize>,
    /* true while a comprehension body compiles where a walrus binds in the enclosing scope */
    pub(super) in_comp: bool,
    /* true inside a def or lambda body as the only place `return` and `yield` belong */
    pub(super) in_function: bool,
    /* true while an `async def` body compiles as the only place `await` belongs */
    pub(super) in_async: bool,
    /* the natives the body calls as bits in two words with those past 128 listed */
    pub(super) called: [u64; 2],
    pub(super) called_far: Vec<u16>,
    // where this body's own far natives start in `called_far` past its enclosing bodies
    pub(super) far_from: usize,
    pub(super) fused_externs: Vec<u16>,
    /* builtin names this body or an enclosing function binds so their calls load the binding */
    pub(super) unfused: Vec<String>,
    /* builtins each body calls before binding them by start token so no body compiles thrice */
    pub(super) late: HashMap<usize, Vec<String>>,
    /* names declared `nonlocal` here or deeper that no function up to here binds */
    pub(super) unbound_nonlocals: Vec<(String, usize, usize)>,
    pub errors: Vec<Diagnostic>,
    /* the host resolver the imports of the source go through */
    pub(super) resolver: Box<dyn Resolver>,
    // sub-parsers inherit this cache made at the first import so each spec parses once
    pub(super) module_cache: Option<ModuleCache>,
    /* how an imported module of the same front end parses */
    pub(super) modules: crate::lang::ParseFn,
    /* the value whose fields free names read in what a builder parses */
    pub(super) subject: Option<Box<build::Subject>>,
    spelling: PhantomData<S>,
}

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    // whether block tokens bound each block and the token each compound header ends at
    pub(super) const DELIMITED: bool = S::SYNTAX.delimited();
    pub(super) const OPEN: TokenType = S::SYNTAX.opener();
    // whether a line end may end a statement
    pub(super) const LINES: bool = !matches!(S::SYNTAX.ends, Ends::Semicolon);

    // each token's text for a diagnostic quoted and bare at compile time
    const SHOWN: &'static crate::syntax::lexer::Shown = &S::SYNTAX.shown(true);
    const QUOTED: &'static [&'static str; 128] = &Self::SHOWN.texts();
    const BARE_SHOWN: &'static crate::syntax::lexer::Shown = &S::SYNTAX.shown(false);
    const BARE: &'static [&'static str; 128] = &Self::BARE_SHOWN.texts();

    // `kind` quoted as the table writes it for a diagnostic
    pub(super) const fn shown(kind: TokenType) -> &'static str { Self::QUOTED[kind as usize & 127] }

    // `kind` bare as the table writes it or its role when it has no text
    pub(super) const fn bare(kind: TokenType) -> &'static str { Self::BARE[kind as usize & 127] }

    const fn q(kind: TokenType) -> &'static [u8] { Self::shown(kind).as_bytes() }

    const fn b(kind: TokenType) -> &'static [u8] { Self::bare(kind).as_bytes() }

    const NAME: &'static [u8] = match S::SYNTAX.interp { Some(i) => i.name.as_bytes(), None => b"interpolated string" };
    const COLON_IF_INDENT: &'static [u8] = if Self::DELIMITED { b"" } else { Self::b(TokenType::Colon) };
    // the conversions a field may name listed as the message writes them
    const MARK_LIST: &'static ([u8; 128], usize) = &{
        let (mark, list) = match S::SYNTAX.interp { Some(crate::syntax::lexer::Interp { convert: Some(c), .. }) => c, _ => (0, &[] as &[(u8, crate::syntax::lexer::Mode)]) };
        let mut out = [0u8; 128];
        let (mut n, mut i) = (0, 0);
        while i < list.len() {
            let gap: &[u8] = if i == 0 { b"" } else if i + 1 == list.len() { if list.len() > 2 { b", or " } else { b" or " } } else { b", " };
            let mut j = 0;
            while j < gap.len() { out[n] = gap[j]; n += 1; j += 1; }
            out[n] = mark;
            out[n + 1] = list[i].0;
            n += 2;
            i += 1;
        }
        (out, n)
    };
    const MARKS_TEXT: &'static [u8] = Self::MARK_LIST.0.split_at(Self::MARK_LIST.1).0;

    worded! {
        BREAK_OUTSIDE_TEXT BREAK_OUTSIDE = [Self::q(TokenType::Break), b" outside loop"];
        CONTINUE_OUTSIDE_TEXT CONTINUE_OUTSIDE = [Self::q(TokenType::Continue), b" outside loop"];
        RETURN_OUTSIDE_TEXT RETURN_OUTSIDE = [Self::q(TokenType::Return), b" outside function"];
        YIELD_IN_COMP_TEXT YIELD_IN_COMP = [Self::q(TokenType::Yield), b" inside a comprehension"];
        YIELD_OUTSIDE_TEXT YIELD_OUTSIDE = [Self::q(TokenType::Yield), b" outside function"];
        AWAIT_OUTSIDE_ASYNC_TEXT AWAIT_OUTSIDE_ASYNC = [Self::q(TokenType::Await), b" outside ", Self::b(TokenType::Async), b" function"];
        AWAIT_OUTSIDE_TEXT AWAIT_OUTSIDE = [Self::q(TokenType::Await), b" outside function"];
        AWAIT_IN_COMP_TEXT AWAIT_IN_COMP = [Self::q(TokenType::Await), b" in a comprehension outside ", Self::b(TokenType::Async), b" function"];
        STRAY_COLON_TEXT STRAY_COLON = [b"unexpected ", Self::q(TokenType::Colon), b" (missing ", Self::q(TokenType::If), b", ", Self::q(TokenType::While), b", ", Self::q(TokenType::For), b", or other statement keyword?)"];
        UNCLOSED_BLOCK_TEXT UNCLOSED_BLOCK = [Self::q(TokenType::BlockOpen), b" was never closed"];
        UNCLOSED_LINE_TEXT UNCLOSED_LINE = [b"the block this line opens was never closed, close it with ", Self::q(TokenType::BlockClose)];
        STRAY_BLOCK_TEXT STRAY_BLOCK = [b"unexpected ", Self::q(TokenType::BlockOpen)];
        STRAY_CLOSE_TEXT STRAY_CLOSE = [b"unexpected ", Self::q(TokenType::BlockClose)];
        FOR_AFTER_ASYNC_TEXT FOR_AFTER_ASYNC = [b"expected ", Self::q(TokenType::For), b" after ", Self::q(TokenType::Async)];
        FOR_AFTER_ELEMENT_TEXT FOR_AFTER_ELEMENT = [b"expected ", Self::q(TokenType::For), b" after the element of a comprehension"];
        PAREN_EXPECTED_TEXT PAREN_EXPECTED = [b"expected ", Self::q(TokenType::ParenOpen)];
        MODULE_AFTER_DOT_TEXT MODULE_AFTER_DOT = [b"expected a module name after ", Self::q(TokenType::Dot)];
        ATTR_AFTER_DOT_TEXT ATTR_AFTER_DOT = [b"expected an attribute name after ", Self::q(TokenType::Dot)];
        WITH_REFUSED_TEXT WITH_REFUSED = [Self::b(TokenType::With), b" is not supported"];
        ASYNC_FOR_REFUSED_TEXT ASYNC_FOR_REFUSED = [Self::b(TokenType::Async), b" ", Self::b(TokenType::For), b" is not supported"];
        ASYNC_WITH_REFUSED_TEXT ASYNC_WITH_REFUSED = [Self::b(TokenType::Async), b" ", Self::b(TokenType::With), b" is not supported"];
        ASYNC_COMP_REFUSED_TEXT ASYNC_COMP_REFUSED = [Self::b(TokenType::Async), b" comprehensions are not supported"];
        ASYNC_GEN_REFUSED_TEXT ASYNC_GEN_REFUSED = [Self::b(TokenType::Async), b" generators are not supported"];
        CATCH_STAR_REFUSED_TEXT CATCH_STAR_REFUSED = [Self::b(TokenType::Catch), Self::b(TokenType::Star), b" is not supported"];
        THROW_FROM_REFUSED_TEXT THROW_FROM_REFUSED = [Self::b(TokenType::Throw), b" ", Self::b(TokenType::From), b" is not supported"];
        CLASS_KEYWORDS_REFUSED_TEXT CLASS_KEYWORDS_REFUSED = [Self::b(TokenType::Class), b" keywords are not supported"];
        TYPE_ALIAS_REFUSED_TEXT TYPE_ALIAS_REFUSED = [Self::b(TokenType::Type), b" aliases are not supported"];
        OUTER_AT_MODULE_TEXT OUTER_AT_MODULE = [Self::b(TokenType::Outer), b" declaration not allowed at module level"];
        DEFAULT_CATCH_TEXT DEFAULT_CATCH = [b"default '", Self::b(TokenType::Catch), Self::COLON_IF_INDENT, b"' must be last"];
        BAD_CONVERSION_TEXT BAD_CONVERSION = [b"invalid ", Self::NAME, b" conversion (expected ", Self::MARKS_TEXT, b")"];
        FORMAT_UNCLOSED_TEXT FORMAT_UNCLOSED = [Self::NAME, b" was never closed"];
        PARAM_GLOBAL_TEXT PARAM_GLOBAL = [b"' is parameter and ", Self::b(TokenType::Global)];
        ASSIGNED_GLOBAL_TEXT ASSIGNED_GLOBAL = [b"' is assigned to before ", Self::b(TokenType::Global), b" declaration"];
        PARAM_OUTER_TEXT PARAM_OUTER = [b"' is parameter and ", Self::b(TokenType::Outer)];
    }

    pub(super) fn current_version(&self, name: &str) -> u32 {
        self.ssa_versions.get(name).unwrap_or(0)
    }

    /* allocates only when `name_ver` overflows `buf` */
    pub(super) fn ssa_name<'a>(name: &'a str, ver: u32, buf: &'a mut [u8; 128]) -> alloc::borrow::Cow<'a, str> {
        versioned(name, ver, buf)
    }

    pub(super) fn increment_version(&mut self, name: &str) -> u32 {
        let (slot, old) = self.ssa_versions.bump(name);
        if !self.join_stack.is_empty() { self.trail.push((slot, old)); }
        old + 1
    }

    /* version zero before any block opens lets the body read a parameter before any store */
    pub(super) fn bind_param(&mut self, p: &str) {
        self.ssa_versions.set(param_base_name(p), 0);
    }

    pub(super) fn push_ssa_name(&mut self, name: &str, ver: u32) -> u16 {
        let mut buf = [0u8; 128];
        self.chunk.push_name(&Self::ssa_name(name, ver, &mut buf))
    }

    pub(super) fn emit_load_ssa(&mut self, name: &str) {
        // only a builder's parser reads a subject so other parsers compile none of this
        if S::SUBJECT && self.subject_field(name) { return; }
        if self.globals_decl.contains(name) {
            let i = self.chunk.push_name(name);
            self.chunk.emit(OpCode::LoadGlobal, i);
            return;
        }
        let i = self.push_ssa_name(name, self.current_version(name));
        self.chunk.emit(OpCode::LoadName, i);
    }

    pub(super) fn emit_const(&mut self, v: Value) {
        let i = self.chunk.push_const(v);
        self.chunk.emit(OpCode::LoadConst, i);
    }

    /* emits a jump with a placeholder operand and returns its index for patching */
    pub(super) fn emit_jump(&mut self, op: OpCode) -> usize {
        self.chunk.emit(op, 0);
        self.chunk.instructions.len() - 1
    }

    pub(super) fn store_name(&mut self, name: &str) {
        if self.globals_decl.contains(name) {
            let i = self.chunk.push_name(name);
            self.chunk.emit(OpCode::StoreGlobal, i);
            return;
        }
        let ver = self.increment_version(name);
        let i = self.push_ssa_name(name, ver);
        self.chunk.emit(OpCode::StoreName, i);
    }

    /* moves the free names of the body up then registers and makes the function */
    pub(super) fn push_function(&mut self, params: Vec<String>, mut body: SSAChunk, defaults: u16, fname: Option<&str>, op: OpCode) {
        // a parameter slot is its base name at version zero
        let param_slot = |name: &str| name.strip_suffix("_0").is_some_and(|bare| params.iter().any(|p| param_base_name(p) == bare));
        for (name, attr) in body.names.iter().zip(body.attr_only_names()) {
            if !attr && !param_slot(name) {
                self.chunk.push_name(name);
            }
        }
        let fi = self.chunk.functions.len() as u16;
        body.qualname = self.qualified(fname.unwrap_or(S::NATIVES.names().anonymous));
        let name_slot = match fname {
            Some(f) => self.push_ssa_name(f, self.current_version(f) + 1),
            None => u16::MAX,
        };
        self.chunk.functions.push((params, body, defaults, name_slot));
        self.chunk.emit(op, fi);
    }

    /* `name` after the classes and functions around it and their scope pieces */
    pub(super) fn qualified(&self, name: &str) -> String {
        if self.qual.is_empty() { return name.into(); }
        crate::s!(str &self.qual, ".", str name)
    }

    /* enters class or function `name` and returns what `leave_qual` restores */
    pub(super) fn enter_qual(&mut self, name: &str, function: bool) -> (usize, bool) {
        let (len, locals) = (self.qual.len(), S::NATIVES.names().locals);
        // one reservation for the whole name keeps the scope text from growing twice
        self.qual.reserve(name.len() + if function { locals.len() + 2 } else { 1 });
        if len > 0 { self.qual.push('.'); }
        self.qual.push_str(name);
        if function { self.qual.push('.'); self.qual.push_str(locals); }
        let around = self.around_fn;
        self.around_fn |= function;
        (len, around)
    }

    /* the scope path as it was before `enter_qual` gave `cut` */
    pub(super) fn leave_qual(&mut self, (len, around): (usize, bool)) {
        self.qual.truncate(len);
        self.around_fn = around;
    }

    /* binds `name` to a new version or to the module binding a `global` names */
    pub(super) fn emit_store_new(&mut self, name: &str) -> u16 {
        if self.globals_decl.contains(name) {
            let i = self.chunk.push_name(name);
            self.chunk.emit(OpCode::StoreGlobal, i);
            return i;
        }
        let ver = self.increment_version(name);
        let i = self.push_ssa_name(name, ver);
        self.chunk.emit(OpCode::StoreName, i);
        i
    }

    /* a nested body built by `f` over a chunk sharing the parent texts */
    pub(super) fn with_fresh_chunk(&mut self, f: impl FnOnce(&mut Self)) -> SSAChunk {
        // the shared texts spare the two allocations a default chunk makes each way
        let fresh = SSAChunk::over(self.chunk.source.clone(), self.chunk.path.clone(), None);
        let saved_chunk = core::mem::replace(&mut self.chunk, fresh);
        let saved_ver = core::mem::take(&mut self.ssa_versions);
        let saved_globals = core::mem::take(&mut self.globals_decl);
        let saved_joins = (core::mem::take(&mut self.join_stack), core::mem::take(&mut self.trail));
        let saved_loops = (core::mem::take(&mut self.loops), core::mem::replace(&mut self.cleanup_count, 0));
        let saved_class = core::mem::replace(&mut self.in_class_body, false);
        let saved_comp = core::mem::replace(&mut self.in_comp, false);
        let saved_function = core::mem::replace(&mut self.in_function, false);
        let saved_async = core::mem::replace(&mut self.in_async, false);
        let saved_unbound = core::mem::take(&mut self.unbound_nonlocals);
        // a nested body keeps what enclosing scopes left unfused and hands up what it fused
        let saved_fused = (core::mem::take(&mut self.called), core::mem::take(&mut self.fused_externs), self.unfused.len(), core::mem::replace(&mut self.far_from, self.called_far.len()));
        // a copy of the parent externs lets nested bodies call imported natives without leaking up
        self.chunk.extern_table = saved_chunk.extern_table.clone();
        self.chunk.extern_index = saved_chunk.extern_index.clone();
        f(self);
        // a function binds what its locals hold unless it declared the name nonlocal itself
        let mut unbound = core::mem::replace(&mut self.unbound_nonlocals, saved_unbound);
        if self.in_function {
            unbound.retain(|(name, ..)| !self.ssa_versions.contains(name) || self.chunk.nonlocals.contains(name));
        }
        self.unbound_nonlocals.append(&mut unbound);
        let body = core::mem::replace(&mut self.chunk, saved_chunk);
        self.ssa_versions = saved_ver;
        self.globals_decl = saved_globals;
        (self.join_stack, self.trail) = saved_joins;
        (self.loops, self.cleanup_count) = saved_loops;
        self.in_class_body = saved_class;
        self.in_comp = saved_comp;
        self.in_function = saved_function;
        self.in_async = saved_async;
        let outer = self.chunk.extern_table.len();
        let inner = core::mem::replace(&mut self.fused_externs, saved_fused.1);
        self.fused_externs.extend(inner.into_iter().filter(|&i| (i as usize) < outer));
        self.called = [self.called[0] | saved_fused.0[0], self.called[1] | saved_fused.0[1]];
        self.far_from = saved_fused.3;
        self.unfused.truncate(saved_fused.2);
        body
    }
}

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    pub(super) fn enter_block(&mut self) {
        self.join_stack.push(JoinNode { start: self.trail.len(), mid: None });
    }

    /* versions only grow so the next branch starts from them and only the split moves */
    pub(super) fn mid_block(&mut self) {
        if let Some(j) = self.join_stack.last_mut() { j.mid = Some(self.trail.len()); }
    }

    pub(super) fn commit_block(&mut self) {
        let Some(j) = self.join_stack.pop() else { return };
        // the names stored since the split sorted by name each once with its version there
        let from = j.mid.unwrap_or(j.start);
        let versions = &self.ssa_versions;
        self.trail[from..].sort_by(|a, b| versions.name(a.0).cmp(versions.name(b.0)));
        let mut end = from;
        for i in from..self.trail.len() {
            if end == from || self.trail[end - 1].0 != self.trail[i].0 {
                self.trail[end] = self.trail[i];
                end += 1;
            }
        }
        self.trail.truncate(end);

        // each entry keeps the pre-split version so an enclosing join needs none for the Phi
        for i in from..end {
            let (slot, old) = self.trail[i];
            let now = self.ssa_versions.at(slot);
            if old == now { continue; }
            let (va, vb) = if j.mid.is_some() { (old, now) } else { (now, old) };
            let name = self.ssa_versions.name(slot);
            let mut ba = [0u8; 128];
            let mut bb = [0u8; 128];
            let mut bx = [0u8; 128];
            let ia = self.chunk.push_name(&Self::ssa_name(name, va, &mut ba));
            let ib = self.chunk.push_name(&Self::ssa_name(name, vb, &mut bb));
            let ix = self.chunk.push_name(&Self::ssa_name(name, now + 1, &mut bx));
            self.ssa_versions.set_at(slot, now + 1);

            self.chunk.phi_sources.push((ia, ib));
            self.chunk.emit(OpCode::Phi, ix);
        }
        if self.join_stack.is_empty() { self.trail.clear(); }
    }
}

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {

    pub(super) fn advance(&mut self) -> Token {
        let tok = self.tokens.next().unwrap_or(Token {
            kind: TokenType::Eof,
            line: 0, start: 0, end: 0, comp: false, assigns: 0,
        });
        self.last_line = tok.line;
        if tok.end > 0 { self.last_end = tok.end; }
        match tok.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => {
                self.bracket_stack.push((tok.kind, tok.start, tok.end, self.errors.len()));
            }
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => {
                let want_open = bracket_open(tok.kind);
                /* reports the brackets left open above the matching opener or an orphan closer */
                if let Some(idx) = self.bracket_stack.iter().rposition(|&(k, _, _, _)| k == want_open) {
                    while self.bracket_stack.len() > idx + 1 {
                        let (k, st, en, ov) = self.bracket_stack.pop().unwrap();
                        self.errors.truncate(ov);
                        self.errors.push(Diagnostic {
                            start: st, end: en,
                            msg: s!(str &Self::shown(k), " was never closed"),
                        });
                    }
                    self.bracket_stack.pop();
                } else if let Some(&(top_k, _, _, _)) = self.bracket_stack.last() {
                    self.errors.push(Diagnostic {
                        start: tok.start, end: tok.end,
                        msg: s!(str &Self::shown(bracket_close(tok.kind)), " does not match ", str &Self::shown(top_k), ", expected ", str &Self::shown(bracket_close(top_k))),
                    });
                    self.bracket_stack.pop();
                } else {
                    self.errors.push(Diagnostic {
                        start: tok.start, end: tok.end,
                        msg: s!("unexpected ", str &Self::shown(bracket_close(tok.kind)), ", no matching opener"),
                    });
                }
            }
            _ => {}
        }
        tok
    }

    /* a diagnostic without resync over a span that parsed fine but is not allowed */
    pub(super) fn reject(&mut self, start: usize, end: usize, msg: &str) {
        self.errors.push(Diagnostic { start, end, msg: msg.to_string() });
    }

    /* a diagnostic without resync at the next token for when parsing must go on */
    pub(super) fn diag_at_peek(&mut self, msg: &str) {
        let n = self.source.len();
        let (start, end) = match self.tokens.peek() {
            Some(t) if t.line > self.last_line && self.last_end > 0 => (self.last_end, self.last_end),
            Some(t) => (t.start, t.end),
            None => (n, n),
        };
        self.errors.push(Diagnostic { start, end, msg: msg.to_string() });
    }

    /* a diagnostic at the next token then a resync at the next statement boundary */
    pub(super) fn error(&mut self, msg: &str) {
        let n = self.source.len();
        let (start, end) = self.tokens.peek().map(|t| (t.start, t.end)).unwrap_or((n, n));
        self.error_at(start, end, msg);
    }

    /* runs `f` one level deeper or reports the limit so no input exhausts the stack */
    pub(super) fn nested<T: Default>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        if self.expr_depth >= MAX_EXPR_DEPTH {
            self.error("expression too deeply nested");
            return T::default();
        }
        self.expr_depth += 1;
        let out = f(self);
        self.expr_depth -= 1;
        out
    }

    /* a diagnostic at a span then resync to a statement end or closing bracket */
    pub(super) fn error_at(&mut self, start: usize, end: usize, msg: &str) {
        self.errors.push(Diagnostic { start, end, msg: msg.to_string() });
        let in_brackets = !self.bracket_stack.is_empty();
        let mut depth: i32 = 0;
        loop {
            let kind = self.tokens.peek().map(|t| t.kind);
            match kind {
                None | Some(TokenType::Newline | TokenType::Dedent | TokenType::Eof) => break,
                // a block brace bounds a statement the way a line does
                Some(TokenType::BlockOpen | TokenType::BlockClose) if Self::DELIMITED => break,
                Some(TokenType::Comma) if in_brackets && depth == 0 => break,
                Some(TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose) => {
                    if in_brackets && depth == 0 { break; }
                    depth -= 1;
                    self.tokens.next();
                }
                Some(TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen) => {
                    depth += 1;
                    self.tokens.next();
                }
                _ => { self.tokens.next(); }
            }
        }
    }

    pub(super) fn mark(&self) -> Mark {
        Mark {
            pos: self.tokens.pos, errors: self.errors.len(), unbound: self.unbound_nonlocals.len(),
            line: self.last_line, end: self.last_end, closed: self.block_closed, newline: self.saw_newline,
        }
    }

    /* puts the cursor back at `m` keeping what compiled since */
    pub(super) fn seek(&mut self, m: Mark) {
        self.tokens.pos = m.pos;
        (self.last_line, self.last_end, self.block_closed, self.saw_newline) = (m.line, m.end, m.closed, m.newline);
    }

    /* puts the cursor back at `m` and drops the diagnostics raised since */
    pub(super) fn rewind(&mut self, m: Mark) {
        self.seek(m);
        self.errors.truncate(m.errors);
        self.unbound_nonlocals.truncate(m.unbound);
    }

    pub(super) fn at_target_end(&mut self) -> bool { self.peek_same_line().is_some_and(ends_target::<S>) }

    pub(super) fn at_end(&mut self) -> bool { self.peek().is_none() }

    pub(super) fn lexeme(&self, t: &Token) -> &'src str { &self.source[t.start..t.end] }

    pub(super) fn advance_text(&mut self) -> &'src str {
        let t = self.advance();
        self.lexeme(&t)
    }

    /* skips line breaks and comments and latches `saw_newline` for ternary detection */
    pub(super) fn peek(&mut self) -> Option<TokenType> {
        loop {
            match self.tokens.peek().map(|t| t.kind) {
                Some(TokenType::Newline) => {
                    self.saw_newline = true;
                    self.tokens.next();
                }
                Some(TokenType::LineBreak | TokenType::Comment) => { self.tokens.next(); }
                Some(TokenType::Eof) | None => return None,
                Some(k) => return Some(k),
            }
        }
    }

    /* `peek` that stops at a `Newline` so postfix trailers never bind across statements */
    pub(super) fn peek_same_line(&mut self) -> Option<TokenType> {
        loop {
            match self.tokens.peek().map(|t| t.kind) {
                Some(TokenType::LineBreak | TokenType::Comment) => { self.tokens.next(); }
                Some(TokenType::Newline) | Some(TokenType::Eof) | None => return None,
                // the `}` of a block ends its last statement as a line end does
                Some(TokenType::BlockClose) if Self::DELIMITED => return None,
                Some(k) => return Some(k),
            }
        }
    }

    pub(super) fn patch(&mut self, pos: usize) {
        let target = self.chunk.instructions.len() as u16;
        // error recovery can leave a placeholder that was never emitted
        if let Some(ins) = self.chunk.instructions.get_mut(pos) { ins.operand = target; }
    }

    /* bounds checked since a chunk frozen at `MAX_INSTRUCTIONS` can leave a stale index */
    pub(super) fn patch_to(&mut self, pos: usize, target: u16) {
        if let Some(ins) = self.chunk.instructions.get_mut(pos) { ins.operand = target; }
    }

    /* consumes `kind` or reports it with a missing closer anchored at its opener */
    pub(super) fn eat(&mut self, kind: TokenType) {
        if matches!(self.peek(), Some(k) if k == kind) {
            self.advance();
            return;
        }
        if matches!(kind, TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose) {
            let peeked = self.tokens.peek().map(|t| t.kind);
            // a wrong closer is left to `advance()` and a missing one reports at its opener
            if matches!(peeked, Some(TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose)) && peeked != Some(kind)
            {
                return;
            }
            let want_open = bracket_open(kind);
            if let Some(&(opener_kind, start, end, errors_at_open)) = self.bracket_stack.last()
                && opener_kind == want_open
            {
                self.errors.truncate(errors_at_open);
                self.errors.push(Diagnostic {
                    start, end,
                    msg: s!(str &Self::shown(opener_kind), " was never closed"),
                });
                self.bracket_stack.pop();
                return;
            }
        }
        let label: alloc::string::String = match self.tokens.peek() {
            Some(t) if t.kind == TokenType::Eof => "EOF".to_string(),
            Some(t) if t.kind == TokenType::Newline || t.kind == TokenType::LineBreak => "newline".to_string(),
            Some(t) if t.kind == TokenType::Indent => "indent".to_string(),
            Some(t) if t.kind == TokenType::Dedent => "dedent".to_string(),
            Some(t) if t.start == t.end => Self::shown(t.kind).to_string(),
            Some(t) => quoted(&self.source[t.start..t.end]),
            None => "EOF".to_string(),
        };
        let msg = s!("expected ", str &Self::shown(kind), ", got ", str &label);
        // a token on a later line anchors at the end of the previous one
        if let Some(t) = self.tokens.peek()
            && t.line > self.last_line
            && self.last_end > 0
        {
            let p = self.last_end;
            self.error_at(p, p, &msg);
        } else {
            self.error(&msg);
        }
    }

    pub(super) fn eat_if(&mut self, kind: TokenType) -> bool {
        if matches!(self.peek(), Some(k) if k == kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /* a simple statement ends at its line or a `;` and never at another statement */
    pub(super) fn end_statement(&mut self, errors_before: usize) {
        if self.block_closed || self.errors.len() > errors_before { return; }
        if !Self::LINES { return self.end_by_semicolon(); }
        let Some(t) = self.tokens.peek() else { return };
        if matches!(t.kind, TokenType::Semi | TokenType::Newline | TokenType::LineBreak | TokenType::Comment | TokenType::Dedent | TokenType::Eof) { return; }
        if Self::DELIMITED && t.kind == TokenType::BlockClose { return; }
        if self.skipped_line_end() { return; }
        // reported without skipping so the rest of the line still reports its own errors
        let (start, end) = (t.start, t.end);
        if self.close_held() { return; }
        let msg = s!("expected newline, got ", str &quoted(&self.source[start..end]));
        self.errors.push(Diagnostic { start, end, msg });
    }

    /* a wrong closer an inner bracket left reports against the opener still held */
    fn close_held(&mut self) -> bool {
        let closer = self.tokens.peek().is_some_and(|t| matches!(t.kind, TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose));
        if !closer || self.bracket_stack.is_empty() { return false; }
        self.advance();
        true
    }

    // whether a peek skipped a line end after the last token the statement took
    fn skipped_line_end(&self) -> bool {
        let taken = &self.tokens.toks[..self.tokens.pos.min(self.tokens.toks.len())];
        taken.iter().rev().take_while(|t| matches!(t.kind, TokenType::Newline | TokenType::LineBreak | TokenType::Comment)).any(|t| t.kind == TokenType::Newline)
    }

    // where line ends end nothing a statement wants a semicolon or its block's end
    fn end_by_semicolon(&mut self) {
        // the last token the statement took past the line breaks a peek skipped
        let last = self.tokens.toks[..self.tokens.pos.min(self.tokens.toks.len())].iter().rev().find(|t| !matches!(t.kind, TokenType::LineBreak | TokenType::Comment | TokenType::Newline));
        let ended = last.is_some_and(|t| matches!(t.kind, TokenType::Semi | TokenType::BlockClose));
        if ended || matches!(self.peek(), None | Some(TokenType::Semi | TokenType::BlockClose)) { return; }
        let Some(t) = self.tokens.peek() else { return };
        let (start, end) = (t.start, t.end);
        if self.close_held() { return; }
        let msg = s!("expected ", str &Self::shown(TokenType::Semi), ", got ", str &quoted(&self.source[start..end]));
        self.errors.push(Diagnostic { start, end, msg });
    }

    // the colon a header ends with where an indented block follows and nothing before braces
    #[inline]
    pub(super) fn header_end(&mut self) {
        if !Self::DELIMITED { self.eat(TokenType::Colon); }
    }

    // a comma list that stops when an element makes no progress
    pub(super) fn comma_list(&mut self, is_end: impl Fn(TokenType) -> bool, mut elem: impl FnMut(&mut Self)) {
        while !matches!(self.peek(), Some(t) if is_end(t)) && self.peek().is_some() {
            let progress = self.last_end;
            elem(self);
            // a missing comma between items is an error and not two items
            if !matches!(self.peek(), Some(t) if is_end(t)) { self.eat(TokenType::Comma); }
            if self.last_end == progress { break; }
        }
    }
}

/* what a source parses as a program or statements a caller runs */
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Body { Program, Exec }

/* a token table's front end lexing `source` through `S` and parsing it as asked */
pub(crate) fn spelled<S: Spelling>(source: &str, unit: Unit) -> Parsed {
    let (tokens, lex_errs) = crate::syntax::lexer::lex_as::<S>(source);
    let Unit { resolver, path, body, cache, modules } = unit;
    // built in place and parsed by reference since fuel charges every byte a move copies
    let mut p = Parser::<_, S>::with_parts(source, tokens.into_iter(), resolver, cache, modules);
    if let Some(path) = path { p.chunk.path = path; }
    for e in lex_errs { p.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }); }
    p.parse_body(body);
    if p.errors.is_empty() { Ok(alloc::rc::Rc::new(p.chunk)) } else { Err(p.errors) }
}

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    // the parser of tokens that `lex_as` lexed in spelling `S`
    pub fn spelled(source: &'src str, iter: I, resolver: Box<dyn Resolver>) -> Self {
        Self::with_parts(source, iter, resolver, None, spelled::<S>)
    }

    /* the parser for code that runs as if written in the file at `path` */
    pub fn at_path(mut self, path: alloc::sync::Arc<String>) -> Self {
        self.chunk.path = path;
        self
    }

    /* the one constructor with the cache a parent shares and how its imports parse */
    fn with_parts(source: &'src str, iter: I, resolver: Box<dyn Resolver>, module_cache: Option<ModuleCache>, modules: crate::lang::ParseFn) -> Self {
        let chunk = SSAChunk::over(alloc::sync::Arc::new(source.into()), alloc::sync::Arc::default(), Some(S::NATIVES));
        Self {
            source,
            tokens: Tokens { toks: iter.collect(), pos: 0, from: PhantomData },
            chunk,
            ssa_versions: Versions::default(),
            globals_decl: crate::util::hash::FxHashSet::default(),
            join_stack: Vec::new(),
            trail: Vec::new(),
            block_closed: false,
            loops: Vec::new(),
            cleanup_count: 0,
            saw_newline: false,
            expr_depth: 0,
            last_line: 0,
            last_end: 0,
            bracket_stack: Vec::new(),
            in_class_body: false,
            qual: String::new(),
            around_fn: false,
            posonly: 0,
            decorated_at: None,
            in_comp: false,
            in_function: false,
            in_async: false,
            called: [0; 2],
            called_far: Vec::new(),
            far_from: 0,
            fused_externs: Vec::new(),
            unfused: Vec::new(),
            late: HashMap::default(),
            unbound_nonlocals: Vec::new(),
            errors: Vec::new(),
            resolver,
            module_cache,
            modules,
            subject: None,
            spelling: PhantomData,
        }
    }

    pub fn parse(mut self) -> (SSAChunk, Vec<Diagnostic>) { let errors = self.parse_in_place(); (self.chunk, errors) }

    /* parses statements that run under a caller so the chunk returns None like a function */
    pub fn parse_exec(self) -> (SSAChunk, Vec<Diagnostic>) { self.parse_as(Body::Exec) }

    /* the chunk of a source parsed as `body` returning the last value on the stack */
    fn parse_as(mut self, body: Body) -> (SSAChunk, Vec<Diagnostic>) {
        self.parse_body(body);
        (self.chunk, self.errors)
    }

    /* `parse` that leaves the chunk in `self.chunk` for the caller and returns the diagnostics */
    pub(crate) fn parse_in_place(&mut self) -> Vec<Diagnostic> {
        self.parse_body(Body::Program);
        core::mem::take(&mut self.errors)
    }

    fn parse_body(&mut self, body: Body) {
        self.statements();
        self.close(body);
    }

    /* every statement left in the tokens each popping the value it leaves */
    #[inline(always)]
    pub(super) fn statements(&mut self) {
        while !self.at_end() {
            while self.eat_if(TokenType::Semi) {}
            if self.at_end() { break; }

            self.block_closed = false;
            // a statement starts outside every bracket whatever a failed one left open
            self.bracket_stack.clear();
            let (errors_before, at) = (self.errors.len(), self.tokens.pos);
            let produced_value = self.stmt();
            self.end_statement(errors_before);
            // a failed statement that consumed nothing skips a token so the loop always ends
            if self.tokens.pos == at { self.tokens.next(); }
            if produced_value { self.chunk.emit(OpCode::PopTop, 0); }
        }
    }

    /* ends a module chunk once its statements are in */
    #[inline(always)]
    pub(super) fn close(&mut self, body: Body) {
        if body == Body::Exec { self.emit_const(Value::None); }

        for (name, start, end) in core::mem::take(&mut self.unbound_nonlocals) {
            self.errors.push(Diagnostic { start, end, msg: s!("no binding for nonlocal '", str &name, "' found") });
        }

        if self.chunk.overflow {
            let n = self.source.len();
            self.errors.push(Diagnostic {
                start: n, end: n,
                msg: "program too large: exceeded the 65535 instruction, name, or constant limit".to_string(),
            });
        }

        if !self.errors.is_empty() {
            // cleared so `finalize_prev_slots` never reads stale phi sources
            self.chunk.instructions.clear();
            self.chunk.constants.clear();
            self.chunk.names.clear();
            self.chunk.phi_sources.clear();
            self.chunk.phi_map.clear();
            self.chunk.functions.clear();
            self.chunk.classes.clear();
            self.chunk.native_sites.clear();
            self.chunk.name_index.clear();
            self.chunk.name_marks = 0;
            self.chunk.nonlocals.clear();
            self.chunk.stmt_pos.clear();
            self.loops.clear();
        }

        self.chunk.emit(OpCode::ReturnValue, 0);
        self.chunk.finalize_prev_slots();
    }
}

/* `name` and an underscore and `ver` in `buf` or on the heap past its end */
fn versioned<'a>(name: &'a str, ver: u32, buf: &'a mut [u8; 128]) -> alloc::borrow::Cow<'a, str> {
    let mut tmp = itoa::Buffer::new();
    let digits = tmp.format(ver);
    let n = name.len() + 1 + digits.len();
    if n > buf.len() {
        return alloc::borrow::Cow::Owned(crate::s!(str name, "_", str digits));
    }
    buf[..name.len()].copy_from_slice(name.as_bytes());
    buf[name.len()] = b'_';
    buf[name.len() + 1..n].copy_from_slice(digits.as_bytes());
    // the name is copied whole and the underscore and digits are ascii
    alloc::borrow::Cow::Borrowed(crate::r#unsafe::whole(&buf[..n]))
}
