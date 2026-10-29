use crate::s;

use super::Parser;
use crate::{code::OpCode, syntax::{chunk::{NativeSite, Value, SSAChunk}, ssa::{COMP_ARG, SPREAD_BASES}}};

use crate::syntax::lexer::{Cond, Interp, Literal, Mode, Spelling, Token, TokenType};

use alloc::{string::{String, ToString}, vec::Vec};

// a bare generator expression beside another argument or a trailing comma
const GENEXP_BARE: &str = "a generator expression beside other arguments needs parentheses of its own";

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    const INTERP: Option<Interp> = S::SYNTAX.interp;
    const ECHO: u8 = match Self::INTERP { Some(Interp { echo: Some(b), .. }) => b, _ => 0 };
    const MARKS: (u8, &'static [(u8, Mode)]) = match Self::INTERP { Some(Interp { convert: Some(c), .. }) => c, _ => (0, &[]) };

    // whether the echo byte is the table's `=` so a field checks the token alone
    const ECHO_EQUAL: bool = Self::ECHO != 0 && S::SYNTAX.byte_of(TokenType::Equal) == Self::ECHO;

    // whether the next token is the byte that keeps a field's text beside its value
    #[inline]
    fn at_echo(&mut self) -> bool {
        if Self::ECHO_EQUAL { return matches!(self.peek(), Some(TokenType::Equal)); }
        Self::ECHO != 0 && self.peek().is_some() && self.tokens.peek().is_some_and(|t| t.end == t.start + 1 && self.source.as_bytes()[t.start] == Self::ECHO)
    }

    /* a dict or set display or comprehension always eating `}` so `bracket_stack` stays in sync */
    #[inline(never)]
    // kept out of the operand dispatch whose blocks every other operand enters
    pub(super) fn brace_literal(&mut self) {
        if matches!(self.peek(), Some(TokenType::BraceClose)) {
            self.advance();
            self.chunk.emit(OpCode::BuildDict, 0);
            return;
        }
        // a leading spread builds an empty container and merges into it
        if self.eat_if(TokenType::Pow) {
            if !Self::SPLAT { return self.spread_refused(); }
            self.chunk.emit(OpCode::BuildDict, 0);
            self.expr();
            self.chunk.emit(OpCode::DictUpdate, 0);
            self.dict_tail(0, true);
            return;
        }
        if self.eat_if(TokenType::Star) {
            if !Self::SPLAT { return self.spread_refused(); }
            self.chunk.emit(OpCode::BuildSet, 0);
            self.expr();
            self.chunk.emit(OpCode::SetUpdate, 0);
            self.set_tail(0, true);
            return;
        }
        if self.at_comp() {
            self.comprehension(Comp::Brace);
            self.eat(TokenType::BraceClose);
            return;
        }
        self.expr();
        if self.eat_if(TokenType::Colon) {
            self.expr();
            self.dict_tail(1, false);
        } else {
            self.set_tail(1, false);
        }
    }

    /* a list display or comprehension always eating `]` so `bracket_stack` stays in sync */
    pub(super) fn list_literal(&mut self) {
        if matches!(self.peek(), Some(TokenType::BracketClose)) {
            self.advance();
            self.chunk.emit(OpCode::BuildList, 0);
            return;
        }
        // a leading `*` builds an empty list and extends it
        if self.eat_if(TokenType::Star) {
            if !Self::SPLAT { return self.spread_refused(); }
            self.chunk.emit(OpCode::BuildList, 0);
            self.expr();
            self.chunk.emit(OpCode::ListExtend, 0);
            self.list_tail(0, true);
            return;
        }
        if self.at_comp() {
            self.comprehension(Comp::List);
            self.eat(TokenType::BracketClose);
            return;
        }
        self.expr();
        self.list_tail(1, false);
    }

    /* the star marker just taken where a table without spreads wants an operand */
    #[cold]
    #[inline(never)]
    fn spread_refused(&mut self) {
        let t = self.tokens.toks[self.tokens.pos - 1];
        self.error_at(t.start, t.end, "expected expression");
    }

    /* a display tail keeping items loose until a spread builds the container to merge into */
    #[allow(clippy::too_many_arguments)]
    fn container_tail(
        &mut self, mut count: u16, mut incremental: bool,
        close: TokenType, spread: TokenType,
        build: OpCode, update: OpCode, add: OpCode,
        elem: impl Fn(&mut Self),
    ) {
        while self.eat_if(TokenType::Comma) {
            if self.peek() == Some(close) { break; }
            if self.eat_if(spread) {
                if !Self::SPLAT { return self.spread_refused(); }
                if !incremental { self.chunk.emit(build, count); incremental = true; }
                self.expr();
                self.chunk.emit(update, 0);
            } else {
                elem(self);
                if incremental { self.chunk.emit(add, 0); } else { count += 1; }
            }
        }
        self.eat(close);
        if !incremental { self.chunk.emit(build, count); }
    }

    fn dict_tail(&mut self, pairs: u16, incremental: bool) {
        self.container_tail(pairs, incremental, TokenType::BraceClose, TokenType::Pow,
            OpCode::BuildDict, OpCode::DictUpdate, OpCode::MapAdd,
            |s| { s.expr(); s.eat(TokenType::Colon); s.expr(); });
    }

    fn set_tail(&mut self, count: u16, incremental: bool) {
        self.container_tail(count, incremental, TokenType::BraceClose, TokenType::Star,
            OpCode::BuildSet, OpCode::SetUpdate, OpCode::SetAdd, |s| s.expr());
    }

    fn list_tail(&mut self, count: u16, incremental: bool) {
        self.container_tail(count, incremental, TokenType::BracketClose, TokenType::Star,
            OpCode::BuildList, OpCode::ListExtend, OpCode::ListAppend, |s| s.expr());
    }

    /* the clauses of a comprehension after its first with each `if` filtering the innermost loop */
    fn comp_clauses(&mut self, loop_starts: &mut Vec<u16>, for_iters: &mut Vec<usize>) {
        loop {
            if let Some(&ls) = loop_starts.last() && self.eat_if(TokenType::If) {
                self.expr_bp(1);
                self.chunk.emit(OpCode::JumpIfFalse, ls);
                continue;
            }
            let at = self.tokens.peek().map(|t| (t.start, t.end));
            let asynced = self.eat_if(TokenType::Async);
            if !self.eat_if(TokenType::For) {
                if asynced { self.diag_at_peek(Self::FOR_AFTER_ASYNC); }
                break;
            }
            if asynced && let Some((start, end)) = at { self.reject(start, end, Self::ASYNC_COMP_REFUSED); }
            let at = self.mark();
            let Some(shape) = self.tokens.target_list() else { return self.error("cannot assign to this expression") };
            self.eat(TokenType::In);
            self.expr_bp(1);
            self.chunk.emit(OpCode::GetIter, 0);
            loop_starts.push(self.chunk.instructions.len() as u16);
            for_iters.push(self.emit_jump(OpCode::ForIter));
            self.store_back(at, shape);
        }
    }

    /* whether the next token starts the element of a comprehension */
    pub(super) fn at_comp(&mut self) -> bool {
        Self::SHAPES.comprehensions && self.peek().is_some() && self.tokens.peek().is_some_and(|t| t.comp)
    }

    /* a comprehension compiles to a function over its first iterable called at once */
    pub(super) fn comprehension(&mut self, kind: Comp) {
        // the element runs innermost so the cursor compiles the clauses first and comes back
        let elem = self.tokens.pos;
        let elem_start = self.tokens.peek().map_or(self.last_end, |t| t.start) as u32;
        // a colon outside a lambda or question conditional makes a brace comprehension a dict one
        let (mut depth, mut lambdas, mut pair) = (0usize, 0usize, false);
        while let Some(t) = self.tokens.peek() {
            match t.kind {
                TokenType::For | TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose | TokenType::Eof if depth == 0 => break,
                TokenType::Async if depth == 0 && self.tokens.after_head() == TokenType::For => break,
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth -= 1,
                TokenType::FnExpr if depth == 0 => lambdas += 1,
                TokenType::Question if matches!(Self::SHAPES.cond, Cond::Question) && depth == 0 => lambdas += 1,
                TokenType::Colon if depth == 0 && lambdas > 0 => lambdas -= 1,
                TokenType::Colon if depth == 0 => pair = true,
                _ => {}
            }
            self.tokens.pos += 1;
        }
        let elem_end = self.tokens.pos;
        let names = &S::NATIVES.names().comprehensions;
        let (build, add, name) = match kind {
            Comp::Gen => (None, OpCode::Yield, names[0]),
            Comp::List => (Some(OpCode::BuildList), OpCode::ListAppend, names[1]),
            Comp::Brace if pair => (Some(OpCode::BuildDict), OpCode::MapAdd, names[2]),
            Comp::Brace => (Some(OpCode::BuildSet), OpCode::SetAdd, names[3]),
        };
        let async_at = self.tokens.peek().map(|t| (t.start, t.end));
        if self.eat_if(TokenType::Async) && let Some((start, end)) = async_at { self.reject(start, end, Self::ASYNC_COMP_REFUSED); }
        let in_async = self.in_async;
        self.eat(TokenType::For);
        let at = self.mark();
        let shape = self.tokens.target_list();
        if shape.is_none() { self.error("cannot assign to this expression"); }
        self.eat(TokenType::In);
        self.expr_bp(1);
        self.chunk.emit(OpCode::GetIter, super::ITER_VALUE);
        // a walrus binds where the comprehension stands and a `global` there counts too
        let globals = self.globals_decl.clone();
        let mut body = self.with_fresh_chunk(|s| {
            s.in_comp = true;
            s.in_async = in_async;
            s.globals_decl = globals;
            // a body error points at the element until a clause marks its own line
            s.chunk.stmt_pos.push((0, elem_start));
            s.bind_param(COMP_ARG);
            if let Some(op) = build { s.chunk.emit(op, 0); }
            let arg = s.push_ssa_name(COMP_ARG, 0);
            s.chunk.emit(OpCode::LoadName, arg);
            s.chunk.emit(OpCode::GetIter, super::ITER_AS_IS);
            let mut loop_starts = alloc::vec![s.chunk.instructions.len() as u16];
            let mut for_iters = alloc::vec![s.emit_jump(OpCode::ForIter)];
            if let Some(shape) = shape { s.store_back(at, shape); }
            s.mark_stmt();
            s.comp_clauses(&mut loop_starts, &mut for_iters);
            let rest = s.tokens.pos;
            s.tokens.pos = elem;
            s.mark_stmt();
            s.expr();
            if add == OpCode::MapAdd { s.eat(TokenType::Colon); s.expr(); }
            if s.tokens.pos != elem_end { s.error(Self::FOR_AFTER_ELEMENT); }
            s.tokens.pos = rest;
            s.chunk.emit(add, 0);
            if add == OpCode::Yield { s.chunk.emit(OpCode::PopTop, 0); }
            for i in (0..for_iters.len()).rev() {
                s.chunk.emit(OpCode::Jump, loop_starts[i]);
                s.patch(for_iters[i]);
            }
            if build.is_some() { s.chunk.emit(OpCode::ReturnValue, 0); }
        });
        body.is_generator = kind == Comp::Gen;
        // a comprehension that awaits runs as a coroutine the enclosing one awaits
        let awaits = body.instructions.iter().any(|i| i.opcode == OpCode::GetAwaitable);
        if awaits && kind == Comp::Gen { self.reject(elem_start as usize, self.last_end, Self::ASYNC_GEN_REFUSED); }
        self.push_function(alloc::vec![COMP_ARG.into()], body, 0, Some(name), if awaits { OpCode::MakeCoroutine } else { OpCode::MakeFunction });
        self.chunk.emit(OpCode::Swap, 0);
        self.chunk.emit(OpCode::Call, 1);
        if awaits && kind != Comp::Gen { self.emit_await(); }
    }

    /* a bare generator expression which must be the only argument of its call */
    fn genexp_arg(&mut self, first: bool) {
        let start = self.tokens.peek().map_or(self.last_end, |t| t.start);
        self.comprehension(Comp::Gen);
        if !first || matches!(self.peek(), Some(TokenType::Comma)) { self.reject(start, self.last_end, GENEXP_BARE); }
    }

    /* emits the text and field parts of an interpolated literal and returns their count */
    pub(super) fn fstring(&mut self, fs_start: usize, fs_end: usize) -> u16 {
        let mut parts = 0u16;
        let mut got_end = false;
        // a raw literal keeps backslashes and a plain one decodes escapes like a string
        let is_raw = crate::syntax::text::prefix_flags::<S>(&self.source[fs_start..fs_end]) & crate::syntax::lexer::RAW != 0;
        let doubled = matches!(Self::INTERP, Some(Interp { literal: Literal::Doubled, .. }));
        let (open, close) = match Self::INTERP { Some(i) => (i.open.as_bytes()[0] as char, i.close as char), None => ('{', '}') };
        if matches!(self.peek(), Some(TokenType::FormatEnd)) {
            self.advance();
            return 0;
        }
        loop {
            match self.peek() {
            Some(TokenType::FormatText) => {
                let t = self.advance();
                let raw = self.lexeme(&t);
                let mut unescaped = String::with_capacity(raw.len());
                // one pass reads a delimiter written as text before an escape could make one
                let mut chars = raw.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        _ if doubled && (c == open || c == close) && chars.peek() == Some(&c) => { chars.next(); unescaped.push(c); }
                        '\\' if !doubled && chars.peek().is_some_and(|&n| n == open || n == close) => unescaped.push(chars.next().unwrap_or(c)),
                        '\\' if !is_raw => crate::syntax::text::push_escape::<S>(&mut unescaped, &mut chars),
                        _ => unescaped.push(c),
                    }
                }
                self.emit_const(Value::Str(unescaped));
                parts += 1;
            }
                Some(TokenType::BraceOpen) => {
                    self.advance();
                    // an `=` field shows its text as written with its spaces
                    let field_start = self.last_end;
                    self.expr();
                    self.tuple_rest(1, |s| matches!(s.peek(), Some(TokenType::BraceClose | TokenType::Colon | TokenType::Convert) | None) || s.at_echo());
                    /* `FormatValue` flags where bit 0 marks a spec and bits 1 and 2 the conversion */
                    let mut flags = 0u16;
                    let mut debug_prefix: Option<String> = None;
                    if self.at_echo() {
                        self.advance();
                        let field_end = self.tokens.peek().map_or(self.last_end, |t| t.start);
                        debug_prefix = Some(self.source[field_start..field_end].to_string());
                    }
                    if matches!(self.peek(), Some(TokenType::Convert)) {
                        let bang = self.advance();
                        let conv_tok = self.advance();
                        let conv = self.lexeme(&conv_tok).as_bytes();
                        flags |= match Self::MARKS.1.iter().find(|&&(letter, _)| conv == [letter]) {
                            Some(&(_, mode)) => mode.flags(),
                            None => {
                                self.error_at(bang.start, conv_tok.end, Self::BAD_CONVERSION);
                                0
                            }
                        };
                    }
                    if debug_prefix.is_some() && (flags & 0b110) == 0 && !matches!(self.peek(), Some(TokenType::Colon)) {
                        flags |= Mode::Repr.flags(); // an echoed field without conversion or spec renders as repr
                    }
                    // the prefix slides under the value so the field keeps its jumps and call positions
                    if let Some(prefix) = debug_prefix.take() {
                        self.emit_const(Value::Str(prefix));
                        self.chunk.emit(OpCode::Swap, 0);
                        parts += 1;
                    }
                    if matches!(self.peek(), Some(TokenType::Colon)) {
                        self.advance();
                        self.fstring_spec();
                        flags |= 1;
                    }
                    self.chunk.emit(OpCode::FormatValue, flags);
                    parts += 1;
                    if matches!(self.peek(), Some(TokenType::BraceClose)) {
                        self.advance();
                    } else {
                        self.field_close_ahead();
                    }
                }
                Some(TokenType::FormatEnd) => {
                    self.advance();
                    got_end = true;
                    break;
                }
                _ => break
            }
        }
        if !got_end {
            self.error_at(fs_start, fs_end, Self::FORMAT_UNCLOSED);
        }
        parts
    }

    /* a field spec as one string with nested fields like `f"{x:{w}}"` formatted in */
    fn fstring_spec(&mut self) {
        let (mut pieces, mut lit_start) = (0u16, self.last_end);
        loop {
            let (kind, at) = self.tokens.peek().map_or((None, self.source.len()), |t| (Some(t.kind), t.start));
            if at > lit_start && matches!(kind, Some(TokenType::BraceClose | TokenType::BraceOpen) | None) {
                self.emit_const(Value::Str(self.source[lit_start..at].to_string()));
                pieces += 1;
            }
            match kind {
                Some(TokenType::BraceOpen) => {
                    self.advance();
                    self.expr();
                    self.chunk.emit(OpCode::FormatValue, 0);
                    pieces += 1;
                    if !self.eat_if(TokenType::BraceClose) && !self.field_close_ahead() { self.eat(TokenType::BraceClose); }
                    lit_start = self.last_end;
                }
                Some(TokenType::BraceClose) | None => break,
                _ => { self.tokens.next(); }
            }
        }
        match pieces {
            0 => self.emit_const(Value::Str(String::new())),
            1 => {}
            n => self.chunk.emit(OpCode::BuildString, n),
        }
    }

    /* reports what trails a field's expression and gives whether it took the field's close */
    #[cold]
    #[inline(never)]
    fn field_close_ahead(&mut self) -> bool {
        // a field the line leaves open is one its literal never closed
        if self.skipped_line_end() { return false; }
        let (mut depth, mut nested, mut close) = (0usize, 0usize, None);
        for (i, t) in self.tokens.toks.iter().enumerate().skip(self.tokens.pos) {
            match t.kind {
                TokenType::FormatStart => nested += 1,
                TokenType::FormatEnd if nested > 0 => nested -= 1,
                TokenType::FormatEnd | TokenType::Newline | TokenType::Eof => break,
                TokenType::BraceClose if depth == 0 && nested == 0 => { close = Some(i); break; }
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let Some(close) = close else { return false };
        let t = self.tokens.toks[self.tokens.pos];
        // an error the field already reported covers what follows its expression
        if !self.bracket_stack.last().is_some_and(|&(_, _, _, at)| self.errors.len() > at) {
            let got = if t.start == t.end { Self::shown(t.kind).to_string() } else { super::quoted(&self.source[t.start..t.end]) };
            let shut = match Self::INTERP { Some(i) => i.close, None => b'}' };
            self.reject(t.start, t.end, &s!("expected '", char shut as char, "', got ", str &got));
        }
        self.tokens.pos = close;
        self.advance();
        true
    }

    /* calls `name` by binding import or native and true when it keeps no value */
    pub(super) fn call(&mut self, name: &str, stmt: bool) -> bool {
        let call_pos = self.last_end as u32;
        // a builtin name the scope binds as a parameter or otherwise calls the binding
        if self.ssa_versions.contains(name) || self.globals_decl.contains(name) || self.unfused.iter().any(|u| u == name) {
            let i = self.push_ssa_name(name, self.current_version(name));
            if self.globals_decl.contains(name) {
                let gi = self.chunk.push_name(name);
                self.chunk.emit(OpCode::LoadGlobal, gi);
            } else {
                self.chunk.emit(OpCode::LoadName, i);
            }
            self.advance();
            self.call_rest(call_pos);
            return false;
        }
        // an imported native shadows the builtin of the same name
        if let Some(&extern_idx) = self.chunk.extern_index.get(name) {
            // a native call always opens a spread frame as its operand has no flag room
            self.chunk.emit(OpCode::BeginArgs, 0);
            self.advance();
            let (pos, kw, _) = self.args_body(false);
            if pos > 0xF || kw > 0xF { self.error("native calls take at most 15 positional and 15 keyword arguments"); }
            let encoded = (extern_idx << 8) | ((kw & 0xF) << 4) | (pos & 0xF);
            self.fused_externs.push(extern_idx);
            self.chunk.emit(OpCode::CallExtern, encoded);
            self.chunk.record_call_pos(call_pos);
            return false;
        }
        // a native the runtime gives under this name runs with no binding read
        if let Some(id) = S::NATIVES.function_id(name) {
            self.advance();
            let (from, (pos, kw, spread)) = (self.chunk.instructions.len(), self.args_body(true));
            self.note_called(id.0);
            return self.native_or_own(NativeSite { id: id.0, pos: pos as u8, kw: kw as u8, spread, pinned: false, fast: false, void: false }, name, from, call_pos, stmt);
        }
        let i = self.push_ssa_name(name, self.current_version(name));
        self.chunk.emit(OpCode::LoadName, i);
        self.advance();
        self.call_rest(call_pos);
        false
    }

    /* emits `site` in place or through the sites and true when it keeps no value */
    pub(super) fn call_native(&mut self, mut site: NativeSite, call_pos: u32, stmt: bool) -> bool {
        use crate::syntax::chunk::{SITE_KW1, SITE_VOID};
        let native = S::NATIVES.get(crate::lang::NativeId(site.id));
        // a statement's plain call keeps no value in place unless its native steps
        if stmt && site.kw <= 1 && !site.spread && site.pos < 8 && site.id < 0x100 && native.is_some_and(|n| !n.steps()) && self.ends_statement() {
            self.chunk.emit(OpCode::CallNative, site.id | u16::from(site.pos) << 9 | if site.kw == 1 { SITE_KW1 } else { 0 } | SITE_VOID);
            self.chunk.record_call_pos(call_pos);
            return true;
        }
        // one or two bare positionals its heap op answers run with no call
        site.fast = site.kw == 0 && !site.spread && native.is_some_and(|n| site.pos == 1 && n.fast1.is_some() || site.pos == 2 && n.fast2.is_some());
        // a call that ends its statement keeps no value unless its native steps
        site.void = stmt && native.is_some_and(|n| !n.steps()) && self.ends_statement();
        let operand = match site.in_place() {
            Some(op) => op,
            None => {
                let at = self.chunk.native_sites.len();
                if at >= crate::syntax::chunk::SITE_TABLE as usize { self.error("too many native calls in one body"); return false; }
                self.chunk.native_sites.push(site);
                crate::syntax::chunk::SITE_TABLE | at as u16
            }
        };
        self.chunk.emit(OpCode::CallNative, operand);
        self.chunk.record_call_pos(call_pos);
        site.void
    }

    #[inline]
    fn note_called(&mut self, id: u16) {
        if id < 128 { self.called[usize::from(id >> 6)] |= 1 << (id & 63); } else if !self.called_far[self.far_from..].contains(&id) { self.called_far.push(id); }
    }

    /* whether the statement ends at the next token so a call there keeps nothing */
    #[inline(always)]
    fn ends_statement(&mut self) -> bool {
        match self.tokens.peek().map(|t| t.kind) {
            Some(TokenType::Newline | TokenType::Eof | TokenType::Semi | TokenType::Dedent) | None => true,
            Some(TokenType::BlockClose) => Self::DELIMITED,
            Some(TokenType::LineBreak | TokenType::Comment) => matches!(self.peek_same_line(), None | Some(TokenType::Semi | TokenType::Dedent)),
            _ => false,
        }
    }

    /* builtins and native imports called before the body binds them which makes them locals */
    pub(super) fn called_then_bound(&self) -> Vec<String> {
        let mut late = Vec::new();
        let mut keep = |name: &str| if self.ssa_versions.contains(name) { late.push(String::from(name)); };
        for (w, &word) in self.called.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let id = (w as u16) << 6 | bits.trailing_zeros() as u16;
                bits &= bits - 1;
                if let Some(n) = S::NATIVES.get(crate::lang::NativeId(id)) { keep(n.name); }
            }
        }
        for &id in &self.called_far[self.far_from..] { if let Some(n) = S::NATIVES.get(crate::lang::NativeId(id)) { keep(n.name); } }
        for &i in &self.fused_externs { if let Some(f) = self.chunk.extern_table.get(i as usize) { keep(f.name.as_str()); } }
        late.dedup();
        late
    }

    /* the arguments past `(` then the call as CallSpread once a spread opened a frame */
    pub(super) fn call_rest(&mut self, call_pos: u32) {
        let (pos, kw, spread) = self.args_body(true);
        self.chunk.emit(if spread { OpCode::CallSpread } else { OpCode::Call }, super::pack_call(pos, kw));
        self.chunk.record_call_pos(call_pos);
    }

    // depth guarded since a name-led argument recurses through `call` without passing `expr_bp`
    pub(super) fn args_body(&mut self, lazy: bool) -> (u16, u16, bool) {
        self.expr_depth += 1;
        if self.expr_depth > crate::syntax::chunk::MAX_EXPR_DEPTH {
            self.expr_depth -= 1;
            self.error("expression too deeply nested");
            return (0, 0, false);
        }
        let mut pos = 0u16;
        let mut kw = 0u16;
        let mut spread = false;
        self.comma_list(|t| t == TokenType::ParenClose, |s| {
            let unpack = if !Self::SPLAT { None }
                else if s.eat_if(TokenType::Pow) { Some(2u16) }
                else if s.eat_if(TokenType::Star) { Some(1u16) }
                else { None };
            // a spread tuple or list display before any keyword passes its items as plain positionals
            if unpack == Some(1) && kw == 0 && let Some(close) = s.literal_spread() {
                s.advance();
                let mut items = 0u16;
                s.comma_list(|t| t == close, |s| { s.expr(); items += 1; });
                s.eat(close);
                pos = pos.saturating_add(items);
                return;
            }
            if let Some(kind) = unpack {
                if lazy && !spread { s.chunk.emit(OpCode::BeginArgs, 0); }
                spread = true;
                s.expr();
                // the high bits carry the keyword pairs before it so the VM keeps positionals together
                s.chunk.emit(OpCode::UnpackArgs, (kw << 2) | kind);
                pos = pos.saturating_add(1);
            } else if matches!(s.peek(), Some(TokenType::Name | TokenType::Underscore)) {
                let at = s.tokens.pos;
                let t = s.advance();
                if s.peek() == Some(Self::SHAPES.keyword_arg) {
                    let kw_name = s.lexeme(&t).to_string();
                    s.advance();
                    s.emit_const(Value::Str(kw_name));
                    s.expr();
                    kw = kw.saturating_add(1);
                } else if Self::SHAPES.comprehensions && t.comp {
                    s.tokens.pos = at;
                    s.genexp_arg(pos + kw == 0);
                    pos = pos.saturating_add(1);
                } else {
                    let elem_start = s.chunk.instructions.len();
                    s.name_operand(t);
                    s.infix_bp(0);
                    // a name-led argument bypasses `expr()` so its trailing ternary parses here
                    s.saw_newline = false;
                    s.ternary_tail(elem_start);
                    pos = pos.saturating_add(1);
                }
            } else {
                if s.at_comp() { s.genexp_arg(pos + kw == 0); } else { s.expr(); }
                pos = pos.saturating_add(1);
            }
        });
        self.eat(TokenType::ParenClose);
        self.expr_depth -= 1;
        if pos > 0xFF || kw > 0xFF { self.error("too many arguments in call (max 255 positional and 255 keyword)"); }
        (pos, kw, spread)
    }

    /* the closer of a tuple or list display that alone is the spread argument */
    fn literal_spread(&self) -> Option<TokenType> {
        let toks = &self.tokens.toks;
        let at = self.tokens.pos;
        let close = match toks.get(at)?.kind { TokenType::ParenOpen => TokenType::ParenClose, TokenType::BracketOpen => TokenType::BracketClose, _ => return None };
        let (mut depth, mut comma, mut items, mut prev) = (0usize, false, false, TokenType::ParenOpen);
        for k in at..toks.len() {
            let t = toks[k].kind;
            if matches!(t, TokenType::LineBreak | TokenType::Comment | TokenType::Newline) { continue; }
            match t {
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => {
                    depth -= 1;
                    if depth == 0 {
                        if t != close || close == TokenType::ParenClose && items && !comma { return None; }
                        let next = toks[k + 1..].iter().map(|t| t.kind).find(|t| !matches!(t, TokenType::LineBreak | TokenType::Comment | TokenType::Newline));
                        return matches!(next, Some(TokenType::Comma | TokenType::ParenClose)).then_some(close);
                    }
                }
                _ if depth == 1 => match t {
                    TokenType::For | TokenType::Async => return None,
                    TokenType::Star | TokenType::Pow if matches!(prev, TokenType::ParenOpen | TokenType::BracketOpen | TokenType::Comma) => return None,
                    TokenType::Comma => comma = true,
                    _ => items = true,
                },
                _ => {}
            }
            if depth == 1 && !matches!(t, TokenType::ParenOpen | TokenType::BracketOpen) || k == at { prev = t; }
            if depth == 1 && k != at && !matches!(t, TokenType::Comma) { items = true; }
        }
        None
    }

    /* the next name or a diagnostic and a synthetic name so parsing goes on */
    fn ident_or_missing(&mut self, msg: &str) -> &'src str {
        // `_` lexes as the `match` wildcard yet names a function or class like any identifier
        if matches!(self.peek(), Some(TokenType::Name | TokenType::Underscore)) {
            self.advance_text()
        } else {
            self.diag_at_peek(msg);
            "<missing>"
        }
    }

    /* one `Call(1)` per decorator from the innermost out on the previous result */
    fn emit_decorator_calls(&mut self, n: u16) {
        for _ in 0..n {
            let pos = self.last_end as u32;
            self.chunk.emit(OpCode::Call, 1);
            self.chunk.record_call_pos(pos);
        }
    }

    pub(super) fn class_def_with(&mut self, decorators: u16) {
        self.decorated_at = None;
        let cname = self.ident_or_missing("expected class name");
        self.refuse_type_params();
        self.class_value(cname);

        self.emit_decorator_calls(decorators);

        self.emit_store_new(cname);
    }

    /* the class `cname` names left on the stack after its bases and body compile */
    fn class_value(&mut self, cname: &'src str) {
        // bases push left to right for `MakeClass` to pop `num_bases` of them
        let (mut num_bases, mut spread) = (0u16, false);
        if self.eat_if(TokenType::ParenOpen) {
            while !matches!(self.peek(), Some(TokenType::ParenClose) | None) {
                // a class keyword parses and then fails as a class takes only its bases
                if matches!(self.peek(), Some(TokenType::Name)) {
                    let t = self.advance();
                    if self.eat_if(Self::SHAPES.keyword_arg) {
                        self.reject(t.start, t.end, Self::CLASS_KEYWORDS_REFUSED);
                        self.expr();
                    } else {
                        let start = self.chunk.instructions.len();
                        self.name_operand(t);
                        self.expr_tails(start);
                        if spread { self.chunk.emit(OpCode::ListAppend, 0); }
                        num_bases = num_bases.saturating_add(1);
                    }
                } else if Self::SPLAT && self.eat_if(TokenType::Star) {
                    // `class B(*bases)` gathers every base into one list that MakeClass unpacks
                    if !spread { self.chunk.emit(OpCode::BuildList, num_bases); spread = true; }
                    self.expr();
                    self.chunk.emit(OpCode::ListExtend, 0);
                } else {
                    self.expr();
                    if spread { self.chunk.emit(OpCode::ListAppend, 0); }
                    num_bases = num_bases.saturating_add(1);
                }
                if !self.eat_if(TokenType::Comma) { break; }
            }
            self.eat(TokenType::ParenClose);
        }
        if spread { num_bases = SPREAD_BASES; }

        self.header_end();

        let cut = self.enter_qual(cname, false);
        let body = self.with_fresh_chunk(|s| {
            s.in_class_body = true;
            s.compile_block();
        });
        self.leave_qual(cut);

        let Ok(ci) = u16::try_from(self.chunk.classes.len()) else { self.error("too many classes in this scope (limit 65535)"); return; };
        let mut body = body;
        // free names of its methods pass through the class to the function around it
        let around_function = self.in_function || self.around_fn;
        let mut stored = alloc::vec![false; body.names.len()];
        for ins in body.instructions.iter().filter(|i| matches!(i.opcode, OpCode::StoreName | OpCode::Phi)) {
            if let Some(m) = stored.get_mut(ins.operand as usize) { *m = true; }
        }
        for ((name, attr), own) in body.names.iter().zip(body.attr_only_names()).zip(stored) {
            let inner = body.functions.iter().any(|f| f.1.names.contains(name)) || body.classes.iter().any(|c| c.names.contains(name));
            if around_function && !attr && !own && !name.starts_with('#') && inner { self.chunk.push_name(name); }
        }
        body.bases = num_bases;
        body.qualname = self.qualified(cname);
        self.chunk.classes.push(body);
        self.chunk.emit(OpCode::MakeClass, ci);
    }

    pub(super) fn func_def_inner(&mut self, decorators: u16, is_async: bool) {
        // token lines count from zero
        let first_line = self.decorated_at.take().unwrap_or(self.last_line) as u32 + 1;
        let fname = self.ident_or_missing("expected function name");
        self.refuse_type_params();
        self.func_value(fname, first_line, is_async);

        self.emit_decorator_calls(decorators);

        self.emit_store_new(fname);
    }

    // the function named fname left on the stack for its decorators
    fn func_value(&mut self, fname: &'src str, first_line: u32, is_async: bool) {
        let (params, defaults) = self.parse_params();
        let posonly = self.posonly;
        let cut = self.enter_qual(fname, true);
        let mut body = self.compile_body(&params, is_async);
        self.leave_qual(cut);
        (body.posonly, body.first_line) = (posonly, first_line);

        // free names reach the parent chunk so nested defs capture grandparent variables
        self.push_function(params, body, defaults, Some(fname), if is_async { OpCode::MakeCoroutine } else { OpCode::MakeFunction });
    }

    // a type parameter list after a def or class name parses and then fails
    fn refuse_type_params(&mut self) {
        if !matches!(self.peek(), Some(TokenType::BracketOpen)) { return; }
        let start = self.advance().start;
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                Some(TokenType::BracketOpen | TokenType::ParenOpen | TokenType::BraceOpen) => depth += 1,
                Some(TokenType::BracketClose | TokenType::ParenClose | TokenType::BraceClose) => depth -= 1,
                None => break,
                _ => {}
            }
            self.advance();
        }
        self.reject(start, self.last_end, "generic parameters are not supported");
    }

    pub(super) fn parse_params(&mut self) -> (Vec<String>, u16) {
        // without `(` the colon is eaten anyway so the body starts at its indent
        if !matches!(self.peek(), Some(TokenType::ParenOpen)) {
            self.diag_at_peek(Self::PAREN_EXPECTED);
            self.eat_if(TokenType::Colon);
            return (Vec::new(), 0);
        }
        self.advance();
        let (params, defaults) = self.param_list(TokenType::ParenClose, true);
        self.eat(TokenType::ParenClose);
        // a return annotation ends at the token that opens the body in either spelling
        if self.eat_if(TokenType::Returns) { self.pass_annotation(|k| matches!(k, TokenType::Colon | TokenType::BlockOpen), false); }
        self.header_end();
        (params, defaults)
    }

    /* parameters up to `close` where a bare `*` makes the rest keyword-only */
    pub(super) fn param_list(&mut self, close: TokenType, annotated: bool) -> (Vec<String>, u16) {
        let mut params: Vec<String> = Vec::new();
        let mut defaults = 0u16;
        let mut kw_only = false;
        self.posonly = 0;
        // a block brace ends the header so broken lists never take the body as names
        let at_end = |s: &mut Self| matches!(s.peek(), Some(TokenType::Returns) | None) || s.peek() == Some(close) || (Self::DELIMITED && s.peek() == Some(TokenType::BlockOpen));
        while !at_end(self) {
            let prefix = if self.eat_if(TokenType::Slash) { self.posonly = params.len() as u16; None }
                else if Self::SPLAT && self.eat_if(TokenType::Pow) { Some("**") }
                else if Self::SPLAT && self.eat_if(TokenType::Star) {
                    let bare = matches!(self.peek(), Some(TokenType::Comma)) || at_end(self);
                    kw_only |= bare;
                    (!bare).then_some("*")
                }
                else { Some(if kw_only { "~" } else { "" }) };
            if let Some(prefix) = prefix {
                // the cursor mostly sits on the name so the full peek runs only past markers
                let named = |k: Option<TokenType>| matches!(k, Some(TokenType::Name | TokenType::Underscore));
                if !named(self.tokens.peek().map(|t| t.kind)) && !named(self.peek()) { self.bad_param(close); break; }
                let nm = self.advance_text();
                params.push(s!(str prefix, str &nm));
                // parameters after `*args` are keyword-only and a trailing `=` marks one with a default
                kw_only |= prefix == "*";
                if annotated { self.drain_annotation(); }
                if prefix.len() < 2 && self.eat_if(TokenType::Equal) {
                    if Self::PIPES && close == TokenType::BitOr { self.expr_bp(Self::BAR_BP); } else { self.expr(); }
                    defaults += 1;
                    if let Some(last) = params.last_mut() { last.push('='); }
                }
            }
            if !at_end(self) { self.eat(TokenType::Comma); }
        }
        (params, defaults)
    }

    /* reports a parameter that is no name and skips the list to its close */
    #[cold]
    #[inline(never)]
    fn bad_param(&mut self, close: TokenType) {
        self.diag_at_peek("expected a parameter name");
        let mut depth = 0usize;
        while let Some(k) = self.peek_same_line() {
            match k {
                _ if depth == 0 && (k == close || k == TokenType::Returns || (Self::DELIMITED && k == TokenType::BlockOpen)) => break,
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose if depth > 0 => depth -= 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => break,
                _ => {}
            }
            self.tokens.next();
        }
    }

    // parses a parameter annotation for its errors and drops the code it made
    pub(super) fn drain_annotation(&mut self) {
        if self.eat_if(TokenType::Colon) { self.pass_annotation(|k| matches!(k, TokenType::Comma | TokenType::ParenClose | TokenType::Equal), true); }
    }

    /* passes a plain annotation and parses any other for its errors dropping its code */
    pub(super) fn pass_annotation(&mut self, end: fn(TokenType) -> bool, starred: bool) {
        match self.tokens.plain_annotation(end) {
            Some(at) => while self.tokens.pos < at { self.advance(); },
            // a starred annotation unpacks into the `*args` it annotates
            None => self.discarded(|p| { if starred { p.eat_if(TokenType::Star); } p.expr() }),
        }
    }

    // runs `f` for the errors it reports and drops everything it added to the chunk
    pub(super) fn discarded(&mut self, f: impl FnOnce(&mut Self)) {
        let at = self.chunk.sizes();
        f(self);
        self.chunk.rewind(at);
    }

    fn body_of(&mut self, params: &[String], is_async: bool) {
        self.in_function = true;
        self.in_async = is_async;
        for p in params {
            // the base name without its markers shadows the enclosing scope
            self.bind_param(p);
            let _ = self.push_ssa_name(crate::syntax::ssa::param_base_name(p), 0);
        }
        self.compile_block();
    }

    pub(super) fn compile_body(&mut self, params: &[String], is_async: bool) -> SSAChunk {
        let restart = self.mark();
        let known = if self.late.is_empty() { None } else { self.late.get(&restart.pos).cloned() };
        let mut late = Vec::new();
        let mut body = self.with_fresh_chunk(|s| { s.unfused.extend(known.iter().flatten().cloned()); s.body_of(params, is_async); late = s.called_then_bound(); });
        // a builtin called before its binding is a local so the body compiles once more
        if known.is_none() && !late.is_empty() {
            self.rewind(restart);
            body = self.with_fresh_chunk(|s| { s.unfused.extend(late.iter().cloned()); s.body_of(params, is_async); });
            self.late.insert(restart.pos, late);
        }
        // computed once so `exec_call` never scans the body
        body.is_generator = body.instructions.iter().any(|i| matches!(
            i.opcode,
            OpCode::Yield
        ));
        body
    }
}

/* the bracket a comprehension sits in where a paren makes a generator expression */
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Comp { Gen, List, Brace }
