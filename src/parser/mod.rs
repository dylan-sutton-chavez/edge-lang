pub(super) mod types;

mod stmt;
mod control;
mod expr;
mod literals;
mod imports;

pub use types::*;

use crate::s;
use crate::lexer::{Token, TokenType};
use crate::util::hash::FxHashMap as HashMap;
use crate::modules::{Resolver, NoopResolver};

use alloc::{boxed::Box, string::{String, ToString}, vec::Vec};
use core::marker::PhantomData;

// Bracket table for either side, (opener, open str, close str).
#[inline]
pub(super) const fn bracket_pair(k: TokenType) -> (TokenType, &'static str, &'static str) {
    match k {
        TokenType::Lpar | TokenType::Rpar => (TokenType::Lpar, "'('", "')'"),
        TokenType::Lsqb | TokenType::Rsqb => (TokenType::Lsqb, "'['", "']'"),
        TokenType::Lbrace | TokenType::Rbrace => (TokenType::Lbrace, "'{'", "'}'"),
        _ => (k, "bracket", "bracket"),
    }
}
#[inline]
pub(super) const fn open_str(k: TokenType) -> &'static str { bracket_pair(k).1 }
#[inline]
pub(super) const fn close_str(k: TokenType) -> &'static str { bracket_pair(k).2 }

// Call operand, high byte keyword count, low byte positional count.
#[inline]
pub(super) const fn pack_call(pos: u16, kw: u16) -> u16 {
    ((kw & 0xFF) << 8) | (pos & 0xFF)
}

// Operand of a binary op written as `op=`, so the VM asks `__iop__` first.
pub const INPLACE: u16 = 1;

// GetIter operand that leaves `iter(x)` on the stack, the iterator a comprehension receives.
pub const ITER_VALUE: u16 = 2;

// GetIter operand that loops over an iterator without asking it for one again.
pub const ITER_AS_IS: u16 = 3;

// Marks a fused builtin call given `*` or `**`, whose packed operand the VM runs as a plain call.
pub const SPREAD_ARGS: u16 = 0x8000;

// Marks a fused builtin run as a plain call, for uncounted keywords or a bad count.
pub const KEYWORDS: u16 = 0x4000;

// Shared spec -> compiled-chunk cache, a Vec (linear scan) avoids a hashbrown monomorphization.
pub(crate) type ModuleCache = alloc::rc::Rc<core::cell::RefCell<Vec<(String, alloc::rc::Rc<SSAChunk>)>>>;

/* Every token behind a cursor, so a comprehension can compile its element after its clauses. */
pub(super) struct Tokens<I: Iterator<Item = Token>> {
    toks: Vec<Token>,
    pub(super) pos: usize,
    from: PhantomData<I>,
}

impl<I: Iterator<Item = Token>> Tokens<I> {
    #[inline]
    pub(super) fn peek(&self) -> Option<&Token> { self.toks.get(self.pos) }

    #[inline]
    pub(super) fn next(&mut self) -> Option<Token> {
        let t = self.toks.get(self.pos).copied();
        self.pos += t.is_some() as usize;
        t
    }

    /* Kind of the next token, moving past bracket line breaks and comments. */
    fn ahead(&mut self) -> TokenType {
        while self.peek().is_some_and(|t| matches!(t.kind, TokenType::Nl | TokenType::Comment)) { self.pos += 1; }
        self.peek().map_or(TokenType::Endmarker, |t| t.kind)
    }

    /* Kind of the token after the one at the cursor. */
    pub(super) fn after_head(&self) -> TokenType {
        self.toks.get(self.pos + 1).map_or(TokenType::Endmarker, |t| t.kind)
    }

    /* Moves past the bracket group the cursor opens. */
    fn skip_group(&mut self) {
        let mut depth = 0usize;
        while let Some(t) = self.next() {
            match t.kind {
                TokenType::Lpar | TokenType::Lsqb | TokenType::Lbrace => depth += 1,
                TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace if depth > 1 => depth -= 1,
                TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace | TokenType::Newline | TokenType::Endmarker => return,
                _ => {}
            }
        }
    }

    /* Moves past one target such as `*a`, `b.c[i]` or `(d, e)` uncompiled, false if invalid. */
    fn skip_target(&mut self) -> bool {
        if self.ahead() == TokenType::Star { self.pos += 1; }
        match self.ahead() {
            TokenType::Name | TokenType::Underscore => self.pos += 1,
            TokenType::Lpar | TokenType::Lsqb => self.skip_group(),
            _ => return false,
        }
        loop {
            match self.ahead() {
                TokenType::Dot => {
                    self.pos += 1;
                    if !matches!(self.ahead(), TokenType::Name | TokenType::Underscore) { return false; }
                    self.pos += 1;
                }
                TokenType::Lpar | TokenType::Lsqb => self.skip_group(),
                _ => return true,
            }
        }
    }

    /* Moves past a target list, returns its length, starred item and comma, None if invalid. */
    pub(super) fn target_list(&mut self) -> Option<(usize, Option<usize>, bool)> {
        let (mut n, mut star, mut comma) = (0, None, false);
        // An empty group, as in `() = []`.
        if matches!(self.ahead(), TokenType::Rpar | TokenType::Rsqb) { return Some((n, star, comma)); }
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
            // A trailing comma ends the list only where a target may end.
            let next = self.ahead();
            if !matches!(next, TokenType::Star | TokenType::Name | TokenType::Underscore | TokenType::Lpar | TokenType::Lsqb) {
                return ends_target(next).then_some((n, star, comma));
            }
        }
    }

    /* Where the bracket group opening at `open` closes, and whether a target ends right after it. */
    pub(super) fn group_target(&mut self, open: usize) -> (usize, bool) {
        let at = core::mem::replace(&mut self.pos, open);
        self.skip_group();
        let close = self.pos - 1;
        let ends = ends_target(self.ahead());
        self.pos = at;
        (close, ends)
    }
}

/* Whether `k` ends an assignment target, `=`, a comma, an annotation colon or a closer. */
pub(super) fn ends_target(k: TokenType) -> bool {
    matches!(k, TokenType::Equal | TokenType::Comma | TokenType::Colon | TokenType::Rpar | TokenType::Rsqb)
}

/* Cursor and diagnostics state, so a statement compiles its tokens out of source order. */
#[derive(Clone, Copy)]
pub(super) struct Mark {
    pos: usize,
    errors: usize,
    removed: usize,
    line: usize,
    end: usize,
    closed: bool,
    newline: bool,
}

pub struct Parser<'src, I: Iterator<Item = Token>> {
    pub(super) source: &'src str,
    pub(super) tokens: Tokens<I>,
    pub(super) chunk: SSAChunk,
    pub(super) ssa_versions: HashMap<String, u32>,
    /* Names declared `global` in the current function body, redirects load/store to `self.globals`. */
    pub(super) globals_decl: crate::util::hash::FxHashSet<String>,
    pub(super) join_stack: Vec<JoinNode>,
    /* Set once a block takes its dedent, the statement it closed is complete. */
    pub(super) block_closed: bool,
    pub(super) last_line: usize,
    /* Last token's end offset, anchors diagnostics when `peek()` already skipped a Newline. */
    pub(super) last_end: usize,
    pub(super) loops: Vec<LoopCtx>,
    /* Open finally/with cleanup blocks in the current function, break/continue cross these. */
    pub(super) cleanup_count: usize,
    pub(super) expr_depth: usize,
    pub(super) saw_newline: bool,
    /* Unclosed brackets with error count at open, anchors "never closed" and drops cascade errors. */
    pub(super) bracket_stack: Vec<(TokenType, usize, usize, usize)>,
    /* True while the statements of a class body compile, not those of its methods. */
    pub(super) in_class_body: bool,
    /* True while a comprehension body compiles, where a walrus binds in the enclosing scope. */
    pub(super) in_comp: bool,
    /* Fused builtin calls of the body, one bit per opcode, and the native imports it called by index. */
    pub(super) fused: u128,
    pub(super) fused_externs: Vec<u16>,
    /* Names the body binds after calling them, compiled as its locals when the body compiles again. */
    pub(super) unfused: Vec<String>,
    /* One bit per entry of `REMOVED_BUILTINS` the module binds in any scope, so that name stays its own. */
    pub(super) bound: u8,
    /* Spans that use a removed builtin, by its index in `REMOVED_BUILTINS`, reported at the end unless the module binds the name. */
    pub(super) removed_uses: Vec<(usize, usize, usize)>,
    pub errors: Vec<Diagnostic>,
    /* Host resolver defaulting to NoopResolver so import-free call sites work unchanged. */
    pub(super) resolver: Box<dyn Resolver>,
    // Shared module cache, sub-parsers inherit the Rc so each spec parses once.
    pub(super) module_cache: ModuleCache,
}

// SSA versioning helpers for version tracking and suffixed name emission.

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {
    pub(super) fn current_version(&self, name: &str) -> u32 {
        self.ssa_versions.get(name).copied().unwrap_or(0)
    }

    /* Allocates only when `name_ver` overflows `buf`. */
    pub(super) fn ssa_name<'a>(name: &'a str, ver: u32, buf: &'a mut [u8; 128]) -> alloc::borrow::Cow<'a, str> {
        let mut tmp = itoa::Buffer::new();
        let digits = tmp.format(ver);
        let n = name.len() + 1 + digits.len();
        if n > buf.len() {
            return alloc::borrow::Cow::Owned(s!(str name, "_", str digits));
        }
        buf[..name.len()].copy_from_slice(name.as_bytes());
        buf[name.len()] = b'_';
        buf[name.len() + 1..n].copy_from_slice(digits.as_bytes());
        // SAFETY holds because the name is copied whole and the suffix is ascii.
        alloc::borrow::Cow::Borrowed(unsafe { core::str::from_utf8_unchecked(&buf[..n]) })
    }

    pub(super) fn increment_version(&mut self, name: &str) -> u32 {
        let cur = self.current_version(name);
        let new = cur + 1;
        self.ssa_versions.insert(name.to_string(), new);
        self.note_bound(name);
        new
    }

    /* Binds a parameter at version zero, so the body reads it before any store. */
    pub(super) fn bind_param(&mut self, p: &str) {
        let name = types::param_base_name(p);
        self.ssa_versions.insert(name.to_string(), 0);
        self.note_bound(name);
    }

    #[inline]
    fn note_bound(&mut self, name: &str) {
        if let Some(i) = removed_builtin(name) { self.bound |= 1 << i; }
    }

    /* Notes a use of `name` when it is a removed builtin the scope does not bind. */
    #[inline]
    pub(super) fn note_removed(&mut self, name: &str) {
        if let Some(i) = removed_builtin(name) { self.note_removed_at(name, i); }
    }

    #[cold]
    fn note_removed_at(&mut self, name: &str, i: usize) {
        if self.ssa_versions.contains_key(name) || self.globals_decl.contains(name) || self.chunk.extern_index.contains_key(name) { return; }
        self.removed_uses.push((self.last_end.saturating_sub(name.len()), self.last_end, i));
    }

    pub(super) fn push_ssa_name(&mut self, name: &str, ver: u32) -> u16 {
        let mut buf = [0u8; 128];
        self.chunk.push_name(&Self::ssa_name(name, ver, &mut buf))
    }

    pub(super) fn emit_load_ssa(&mut self, name: String) {
        self.note_removed(&name);
        if self.globals_decl.contains(&name) {
            let i = self.chunk.push_name(&name);
            self.chunk.emit(OpCode::LoadGlobal, i);
            return;
        }
        let i = self.push_ssa_name(&name, self.current_version(&name));
        self.chunk.emit(OpCode::LoadName, i);
    }

    pub(super) fn emit_const(&mut self, v: Value) {
        let i = self.chunk.push_const(v);
        self.chunk.emit(OpCode::LoadConst, i);
    }

    /* Emit a jump with a placeholder operand, returns its instruction index for later patching. */
    pub(super) fn emit_jump(&mut self, op: OpCode) -> usize {
        self.chunk.emit(op, 0);
        self.chunk.instructions.len() - 1
    }

    pub(super) fn store_name(&mut self, name: String) {
        if self.globals_decl.contains(&name) {
            self.note_bound(&name);
            let i = self.chunk.push_name(&name);
            self.chunk.emit(OpCode::StoreGlobal, i);
            return;
        }
        let ver = self.increment_version(&name);
        let i = self.push_ssa_name(&name, ver);
        self.chunk.emit(OpCode::StoreName, i);
    }

    /* Emit UnpackEx (starred) or UnpackSequence then store each target, a leading `*` on a starred name is stripped. */
    pub(super) fn emit_unpack_stores(&mut self, targets: &[String], star_pos: Option<usize>) {
        if let Some(sp) = star_pos {
            let before = sp as u16;
            let after = (targets.len() - sp - 1) as u16;
            self.chunk.emit(OpCode::UnpackEx, (before << 8) | after);
        } else {
            self.chunk.emit(OpCode::UnpackSequence, targets.len() as u16);
        }
        for target in targets { self.store_name(target.trim_start_matches('*').to_string()); }
    }

    /* Propagate the body's free names to the parent chunk, register the function, and emit its make-op. `fname` is None for lambdas (anonymous, sentinel slot), Some(name) registers a store slot for a def. Order matches the historic def path of free-name propagation, then slot, then push. */
    pub(super) fn push_function(&mut self, params: Vec<String>, body: SSAChunk, defaults: u16, fname: Option<&str>, op: OpCode) {
        let param_slots: crate::util::hash::FxHashSet<String> = params.iter()
            .map(|p| s!(str types::param_base_name(p), "_0")).collect();
        for (name, attr) in body.names.iter().zip(body.attr_only_names()) {
            if !attr && !param_slots.contains(name.as_str()) {
                self.chunk.push_name(name);
            }
        }
        let fi = self.chunk.functions.len() as u16;
        let name_slot = match fname {
            Some(f) => self.push_ssa_name(f, self.current_version(f) + 1),
            None => u16::MAX,
        };
        self.chunk.functions.push((params, body, defaults, name_slot));
        self.chunk.emit(op, fi);
    }

    /* Raw version-bump + StoreName, returns the slot. NOT `store_name` since it deliberately skips `globals_decl` routing. */
    pub(super) fn emit_store_new(&mut self, name: &str) -> u16 {
        let ver = self.increment_version(name);
        let i = self.push_ssa_name(name, ver);
        self.chunk.emit(OpCode::StoreName, i);
        i
    }

    pub(super) fn with_fresh_chunk(&mut self, f: impl FnOnce(&mut Self)) -> SSAChunk {
        let saved_chunk = core::mem::take(&mut self.chunk);
        let saved_ver = core::mem::take(&mut self.ssa_versions);
        let saved_globals = core::mem::take(&mut self.globals_decl);
        // Nested body owns its loops and block stack, isolate, then restore the enclosing ones.
        let saved_loops = (core::mem::take(&mut self.loops), core::mem::replace(&mut self.cleanup_count, 0));
        let saved_class = core::mem::replace(&mut self.in_class_body, false);
        let saved_comp = core::mem::replace(&mut self.in_comp, false);
        let saved_fused = (core::mem::take(&mut self.fused), core::mem::take(&mut self.fused_externs), core::mem::take(&mut self.unfused));
        // Copy parent externs so nested def bodies can call imported natives, extras don't leak up.
        self.chunk.extern_table = saved_chunk.extern_table.clone();
        self.chunk.extern_index = saved_chunk.extern_index.clone();
        // Inherit source/path for consistent traceback file context.
        self.chunk.source = saved_chunk.source.clone();
        self.chunk.path = saved_chunk.path.clone();
        f(self);
        let body = core::mem::take(&mut self.chunk);
        self.chunk = saved_chunk;
        self.ssa_versions = saved_ver;
        self.globals_decl = saved_globals;
        (self.loops, self.cleanup_count) = saved_loops;
        self.in_class_body = saved_class;
        self.in_comp = saved_comp;
        (self.fused, self.fused_externs, self.unfused) = saved_fused;
        body
    }
}

// SSA join points, Phi emission at control-flow merges.

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {
    pub(super) fn enter_block(&mut self) {
        self.join_stack.push(JoinNode {
            backup: self.ssa_versions.clone(),
            then: None,
        });
    }

    pub(super) fn mid_block(&mut self) {
        let Some(j) = self.join_stack.last_mut() else { return };
        // Save then-branch versions before restoring else baseline.
        j.then = Some(self.ssa_versions.clone());
        let mut restored = j.backup.clone();
        for (name, &v) in &self.ssa_versions {
            let e = restored.entry(name.clone()).or_insert(0);
            *e = (*e).max(v);
        }
        self.ssa_versions = restored;
    }

    pub(super) fn commit_block(&mut self) {
        let Some(j) = self.join_stack.pop() else { return };
        let post = self.ssa_versions.clone();

        let (a, b) = match j.then {
            Some(t) => (t, post),
            None => (post, j.backup.clone()),
        };

        let mut divergent: Vec<&String> = a
            .keys()
            .chain(b.keys())
            .filter(|name| a.get(*name).unwrap_or(&0) != b.get(*name).unwrap_or(&0))
            .collect();

        // Sort for deterministic Phi order, dedup chain() duplicates from shared vars.
        divergent.sort();
        divergent.dedup();

        for name in divergent {
            let va = *a.get(name).unwrap_or(&0);
            let vb = *b.get(name).unwrap_or(&0);
            let mut ba = [0u8; 128];
            let mut bb = [0u8; 128];
            let mut bx = [0u8; 128];
            let ia = self.chunk.push_name(&Self::ssa_name(name, va, &mut ba));
            let ib = self.chunk.push_name(&Self::ssa_name(name, vb, &mut bb));
            let v = self.increment_version(name);
            let ix = self.chunk.push_name(&Self::ssa_name(name, v, &mut bx));

            self.chunk.phi_sources.push((ia, ib));
            self.chunk.emit(OpCode::Phi, ix);
        }
    }
}

// Token-stream utilities for advance, peek, eat, diagnostics.

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {
    /* Advance without bracket_stack tracking, for recovery consumers with their own balance bookkeeping. */
    pub(super) fn advance_raw(&mut self) -> Token {
        let tok = self.tokens.next().unwrap_or(Token {
            kind: TokenType::Endmarker,
            line: 0, start: 0, end: 0, comp: false, assigns: 0,
        });
        self.last_line = tok.line;
        if tok.end > 0 { self.last_end = tok.end; }
        tok
    }

    pub(super) fn advance(&mut self) -> Token {
        let tok = self.tokens.next().unwrap_or(Token {
            kind: TokenType::Endmarker,
            line: 0, start: 0, end: 0, comp: false, assigns: 0,
        });
        self.last_line = tok.line;
        if tok.end > 0 { self.last_end = tok.end; }
        match tok.kind {
            TokenType::Lpar | TokenType::Lsqb | TokenType::Lbrace => {
                self.bracket_stack.push((tok.kind, tok.start, tok.end, self.errors.len()));
            }
            TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace => {
                let want_open = bracket_pair(tok.kind).0;
                /* Find matching opener, report unclosed brackets above it, handle orphan closers. */
                if let Some(idx) = self.bracket_stack.iter().rposition(|&(k, _, _, _)| k == want_open) {
                    while self.bracket_stack.len() > idx + 1 {
                        let (k, st, en, ov) = self.bracket_stack.pop().unwrap();
                        self.errors.truncate(ov);
                        self.errors.push(Diagnostic {
                            start: st, end: en,
                            msg: s!(str open_str(k), " was never closed"),
                        });
                    }
                    self.bracket_stack.pop();
                } else if let Some(&(top_k, _, _, _)) = self.bracket_stack.last() {
                    self.errors.push(Diagnostic {
                        start: tok.start, end: tok.end,
                        msg: s!(str close_str(tok.kind), " does not match ", str open_str(top_k), ", expected ", str close_str(top_k)),
                    });
                    self.bracket_stack.pop();
                } else {
                    self.errors.push(Diagnostic {
                        start: tok.start, end: tok.end,
                        msg: s!("unexpected ", str close_str(tok.kind), ", no matching opener"),
                    });
                }
            }
            _ => {}
        }
        tok
    }

    /* Non-syncing diagnostic over a span that parsed fine but is not allowed. */
    pub(super) fn reject(&mut self, start: usize, end: usize, msg: &str) {
        self.errors.push(Diagnostic { start, end, msg: msg.to_string() });
    }

    /* Non-syncing diagnostic at peek, used when flow must continue (e.g. missing class name). */
    pub(super) fn diag_at_peek(&mut self, msg: &str) {
        let n = self.source.len();
        let (start, end) = match self.tokens.peek() {
            Some(t) if t.line > self.last_line && self.last_end > 0 => (self.last_end, self.last_end),
            Some(t) => (t.start, t.end),
            None => (n, n),
        };
        self.errors.push(Diagnostic { start, end, msg: msg.to_string() });
    }

    /* Diagnostic at next token + panic-mode sync to next statement boundary. */
    pub(super) fn error(&mut self, msg: &str) {
        let n = self.source.len();
        let (start, end) = self.tokens.peek().map(|t| (t.start, t.end)).unwrap_or((n, n));
        self.error_at(start, end, msg);
    }

    /* Diagnostic at caller span + panic-mode sync, stops at statement boundary or bracket close. */
    pub(super) fn error_at(&mut self, start: usize, end: usize, msg: &str) {
        self.errors.push(Diagnostic { start, end, msg: msg.to_string() });
        let in_brackets = !self.bracket_stack.is_empty();
        let mut depth: i32 = 0;
        loop {
            let kind = self.tokens.peek().map(|t| t.kind);
            match kind {
                None | Some(TokenType::Newline | TokenType::Dedent | TokenType::Endmarker) => break,
                Some(TokenType::Comma) if in_brackets && depth == 0 => break,
                Some(TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace) => {
                    if in_brackets && depth == 0 { break; }
                    depth -= 1;
                    self.tokens.next();
                }
                Some(TokenType::Lpar | TokenType::Lsqb | TokenType::Lbrace) => {
                    depth += 1;
                    self.tokens.next();
                }
                _ => { self.tokens.next(); }
            }
        }
    }

    pub(super) fn mark(&self) -> Mark {
        Mark {
            pos: self.tokens.pos, errors: self.errors.len(), removed: self.removed_uses.len(),
            line: self.last_line, end: self.last_end, closed: self.block_closed, newline: self.saw_newline,
        }
    }

    /* Puts the cursor back at `m`, keeping what compiled since. */
    pub(super) fn seek(&mut self, m: Mark) {
        self.tokens.pos = m.pos;
        (self.last_line, self.last_end, self.block_closed, self.saw_newline) = (m.line, m.end, m.closed, m.newline);
    }

    /* Puts the cursor back at `m` and drops the diagnostics raised since. */
    pub(super) fn rewind(&mut self, m: Mark) {
        self.seek(m);
        self.errors.truncate(m.errors);
        self.removed_uses.truncate(m.removed);
    }

    /* Whether the cursor sits where an assignment target ends. */
    pub(super) fn at_target_end(&mut self) -> bool { self.peek_same_line().is_some_and(ends_target) }

    pub(super) fn at_end(&mut self) -> bool { self.peek().is_none() }

    pub(super) fn lexeme(&self, t: &Token) -> &'src str { &self.source[t.start..t.end] }

    /* Consume the next token and return its source text as a String. */
    pub(super) fn advance_text(&mut self) -> String {
        let t = self.advance();
        self.lexeme(&t).to_string()
    }

    /* Skips Newline/Nl/Comment, maps Endmarker->None, latches saw_newline for ternary detection. */
    pub(super) fn peek(&mut self) -> Option<TokenType> {
        loop {
            match self.tokens.peek().map(|t| t.kind) {
                Some(TokenType::Newline) => {
                    self.saw_newline = true;
                    self.tokens.next();
                }
                Some(TokenType::Nl | TokenType::Comment) => { self.tokens.next(); }
                Some(TokenType::Endmarker) | None => return None,
                Some(k) => return Some(k),
            }
        }
    }

    /* Like `peek` but stops at the logical-line boundary where a `Newline` yields `None` (unconsumed) so postfix trailers don't bind across statements. `Nl`/`Comment` (bracket-internal) still skip, so multiline `()`/`[]`/`{}` are unaffected. */
    pub(super) fn peek_same_line(&mut self) -> Option<TokenType> {
        loop {
            match self.tokens.peek().map(|t| t.kind) {
                Some(TokenType::Nl | TokenType::Comment) => { self.tokens.next(); }
                Some(TokenType::Newline) | Some(TokenType::Endmarker) | None => return None,
                Some(k) => return Some(k),
            }
        }
    }

    pub(super) fn patch(&mut self, pos: usize) {
        let target = self.chunk.instructions.len() as u16;
        // Error recovery can leave a stale placeholder, skip if it was never emitted.
        if let Some(ins) = self.chunk.instructions.get_mut(pos) { ins.operand = target; }
    }

    /* Patch a jump to an explicit target, bounds-safe so instruction overflow (`emit` freezes at MAX_INSTRUCTIONS) can't panic on a stale index. */
    pub(super) fn patch_to(&mut self, pos: usize, target: u16) {
        if let Some(ins) = self.chunk.instructions.get_mut(pos) { ins.operand = target; }
    }

    /* Consumes kind or emits diagnostic, for missing closers anchors at opener and drops cascade. */
    pub(super) fn eat(&mut self, kind: TokenType) {
        if matches!(self.peek(), Some(k) if k == kind) {
            self.advance();
            return;
        }
        if matches!(kind, TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace) {
            let peeked = self.tokens.peek().map(|t| t.kind);
            // Wrong closer, bail silently since advance() will report the mismatch.
            if matches!(peeked, Some(TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace)) && peeked != Some(kind)
            {
                return;
            }
            // Missing closer, anchor diagnostic at the opener, not at the next-line token.
            let want_open = bracket_pair(kind).0;
            if let Some(&(opener_kind, start, end, errors_at_open)) = self.bracket_stack.last()
                && opener_kind == want_open
            {
                self.errors.truncate(errors_at_open);
                self.errors.push(Diagnostic {
                    start, end,
                    msg: s!(str open_str(opener_kind), " was never closed"),
                });
                self.bracket_stack.pop();
                return;
            }
        }
        let label: alloc::string::String = match self.tokens.peek() {
            Some(t) if t.kind == TokenType::Endmarker => "EOF".to_string(),
            Some(t) if t.kind == TokenType::Newline || t.kind == TokenType::Nl => "newline".to_string(),
            Some(t) if t.kind == TokenType::Indent => "indent".to_string(),
            Some(t) if t.kind == TokenType::Dedent => "dedent".to_string(),
            Some(t) if t.start == t.end => t.kind.as_str().to_string(),
            Some(t) => {
                let mut s = alloc::string::String::with_capacity(t.end - t.start + 2);
                s.push('\''); s.push_str(&self.source[t.start..t.end]); s.push('\''); s
            }
            None => "EOF".to_string(),
        };
        let msg = s!("expected ", str kind.as_str(), ", got ", str &label);
        // Token on later line, anchor at end-of-prev-line (common case is missing `:` in header).
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

    /* A simple statement ends at its line or a `;`, not at another statement. */
    pub(super) fn end_statement(&mut self, errors_before: usize) {
        // A statement that already failed reports nothing more, its own error comes first.
        if self.block_closed || self.errors.len() > errors_before { return; }
        let Some(t) = self.tokens.peek() else { return };
        if t.line != self.last_line || matches!(t.kind, TokenType::Semi | TokenType::Newline | TokenType::Nl | TokenType::Comment | TokenType::Dedent | TokenType::Endmarker) { return; }
        // Reported without skipping, so the rest of the line still reports its own errors.
        let (start, end) = (t.start, t.end);
        let msg = s!("expected newline, got '", str &self.source[start..end], "'");
        self.errors.push(Diagnostic { start, end, msg });
    }

    // Comma list with forward-progress guard.
    pub(super) fn comma_list(&mut self, is_end: impl Fn(TokenType) -> bool, mut elem: impl FnMut(&mut Self)) {
        while !matches!(self.peek(), Some(t) if is_end(t)) && self.peek().is_some() {
            let progress = self.last_end;
            elem(self);
            // A missing comma between items is an error, not two items.
            if !matches!(self.peek(), Some(t) if is_end(t)) { self.eat(TokenType::Comma); }
            if self.last_end == progress { break; }
        }
    }
}

// Parser constructors and parse entry point.

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {
    pub fn new(source: &'src str, iter: I) -> Self {
        Self::with_resolver(source, iter, Box::new(NoopResolver))
    }

    /* Parser with explicit resolver, required for `from X import` support. */
    pub fn with_resolver(source: &'src str, iter: I, resolver: Box<dyn Resolver>) -> Self {
        Self::with_shared_cache(source, iter, resolver,
            alloc::rc::Rc::new(core::cell::RefCell::new(Vec::new())))
    }

    /* Shared-cache constructor, sub-parsers inherit it so each spec parses once. */
    pub(crate) fn with_shared_cache(source: &'src str,iter: I,resolver: Box<dyn Resolver>, module_cache: ModuleCache) -> Self {
        let chunk = SSAChunk {
            source: alloc::sync::Arc::new(source.into()),
            ..Default::default()
        };
        Self {
            source,
            tokens: Tokens { toks: iter.collect(), pos: 0, from: PhantomData },
            chunk,
            ssa_versions: HashMap::default(),
            globals_decl: crate::util::hash::FxHashSet::default(),
            join_stack: Vec::new(),
            block_closed: false,
            loops: Vec::new(),
            cleanup_count: 0,
            saw_newline: false,
            expr_depth: 0,
            last_line: 0,
            last_end: 0,
            bracket_stack: Vec::new(),
            in_class_body: false,
            in_comp: false,
            fused: 0,
            fused_externs: Vec::new(),
            unfused: Vec::new(),
            bound: 0,
            removed_uses: Vec::new(),
            errors: Vec::new(),
            resolver,
            module_cache,
        }
    }

    pub fn parse(mut self) -> (SSAChunk, Vec<Diagnostic>) {
        while !self.at_end() {
            while self.eat_if(TokenType::Semi) {}
            if self.at_end() { break; }

            self.block_closed = false;
            let errors_before = self.errors.len();
            let produced_value = self.stmt();
            self.end_statement(errors_before);
            // Pop expression-statement results, chunk's implicit ReturnValue expects empty stack.
            if produced_value { self.chunk.emit(OpCode::PopTop, 0); }
        }

        for (start, end, i) in core::mem::take(&mut self.removed_uses) {
            if self.bound & 1 << i == 0 { self.errors.push(Diagnostic { start, end, msg: REMOVED_BUILTINS[i].1.to_string() }); }
        }

        if self.chunk.overflow {
            let n = self.source.len();
            self.errors.push(Diagnostic {
                start: n, end: n,
                msg: "program too large: exceeded the 65535 instruction, name, or constant limit".to_string(),
            });
        }

        if !self.errors.is_empty() {
            // Clear bytecode state so `finalize_prev_slots` doesn't use stale phi sources.
            self.chunk.instructions.clear();
            self.chunk.constants.clear();
            self.chunk.names.clear();
            self.chunk.phi_sources.clear();
            self.chunk.phi_map.clear();
            self.chunk.functions.clear();
            self.chunk.classes.clear();
            self.chunk.name_index.clear();
            self.chunk.nonlocals.clear();
            self.chunk.stmt_pos.clear();
            self.loops.clear();
        }

        self.chunk.emit(OpCode::ReturnValue, 0);
        self.chunk.finalize_prev_slots();
        (self.chunk, self.errors)
    }
}
