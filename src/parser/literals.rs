use crate::s;

use super::{Mark, Parser};
use super::types::builtin;
use super::types::{OpCode, Value, SSAChunk, class_dunder, ssa_strip, COMP_ARG, SPREAD_BASES};

use crate::lexer::{Token, TokenType};

use alloc::{string::{String, ToString}, vec::Vec};

/* A type parameter as the scope binds it, without the stars of its kind. */
pub(super) fn param_name(p: &str) -> &str { p.trim_start_matches('*') }

// A bare generator expression beside another argument or a trailing comma.
const GENEXP_BARE: &str = "Generator expression must be parenthesized";

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {

    /* `{}` is a dict/set literal or comprehension, always eat(Rbrace) to keep `bracket_stack` in sync. */
    pub(super) fn brace_literal(&mut self) {
        if matches!(self.peek(), Some(TokenType::Rbrace)) {
            self.advance();
            self.chunk.emit(OpCode::BuildDict, 0);
            return;
        }
        // `{**m, ...}`, leading mapping-unpack => dict built incrementally.
        if self.eat_if(TokenType::DoubleStar) {
            self.chunk.emit(OpCode::BuildDict, 0);
            self.expr();
            self.chunk.emit(OpCode::DictUpdate, 0);
            self.dict_tail(0, true);
            return;
        }
        // `{*s, ...}`, leading iterable-unpack => set built incrementally.
        if self.eat_if(TokenType::Star) {
            self.chunk.emit(OpCode::BuildSet, 0);
            self.expr();
            self.chunk.emit(OpCode::SetUpdate, 0);
            self.set_tail(0, true);
            return;
        }
        if self.at_comp() {
            self.comprehension(Comp::Brace);
            self.eat(TokenType::Rbrace);
            return;
        }
        self.expr();
        if self.eat_if(TokenType::Colon) {
            self.expr();
            // First pair already emitted, dict_tail consolidates if a later `**` appears.
            self.dict_tail(1, false);
        } else {
            // First element already emitted, set_tail consolidates if a later `*` appears.
            self.set_tail(1, false);
        }
    }

    /* `[]` is a list literal or list-comp, always eat(Rsqb) to keep `bracket_stack` in sync. */
    pub(super) fn list_literal(&mut self) {
        if matches!(self.peek(), Some(TokenType::Rsqb)) {
            self.advance();
            self.chunk.emit(OpCode::BuildList, 0);
            return;
        }
        // `[*it, ...]`, leading iterable-unpack => list built incrementally.
        if self.eat_if(TokenType::Star) {
            self.chunk.emit(OpCode::BuildList, 0);
            self.expr();
            self.chunk.emit(OpCode::ListExtend, 0);
            self.list_tail(0, true);
            return;
        }
        if self.at_comp() {
            self.comprehension(Comp::List);
            self.eat(TokenType::Rsqb);
            return;
        }
        self.expr();
        // First element already emitted, list_tail consolidates if a later `*` appears.
        self.list_tail(1, false);
    }

    /* Shared tail for `{}`/`[]` displays after the first element. `count` = loose elems on the stack, `incremental` = container already on the stack. First spread consolidates loose elems with `build count`, then merges use `update`/`add`. */
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
        self.container_tail(pairs, incremental, TokenType::Rbrace, TokenType::DoubleStar,
            OpCode::BuildDict, OpCode::DictUpdate, OpCode::MapAdd,
            |s| { s.expr(); s.eat(TokenType::Colon); s.expr(); });
    }

    fn set_tail(&mut self, count: u16, incremental: bool) {
        self.container_tail(count, incremental, TokenType::Rbrace, TokenType::Star,
            OpCode::BuildSet, OpCode::SetUpdate, OpCode::SetAdd, |s| s.expr());
    }

    fn list_tail(&mut self, count: u16, incremental: bool) {
        self.container_tail(count, incremental, TokenType::Rsqb, TokenType::Star,
            OpCode::BuildList, OpCode::ListExtend, OpCode::ListAppend, |s| s.expr());
    }

    /* The `for` and `if` clauses of a comprehension after its first, each `if` filtering the innermost loop. */
    fn comp_clauses(&mut self, loop_starts: &mut Vec<u16>, for_iters: &mut Vec<usize>) {
        loop {
            self.glued_keyword();
            if let Some(&ls) = loop_starts.last() && self.eat_if(TokenType::If) {
                self.expr_bp(1);
                self.chunk.emit(OpCode::JumpIfFalse, ls);
                continue;
            }
            // `async for` steps its iterable the way `for` does.
            let asynced = self.eat_if(TokenType::Async);
            if !self.eat_if(TokenType::For) {
                if asynced { self.diag_at_peek("expected 'for' after 'async'"); }
                break;
            }
            let at = self.mark();
            let Some(shape) = self.tokens.target_list() else { return self.error("cannot assign to this expression") };
            self.eat(TokenType::In);
            self.expr_bp(1);
            self.chunk.emit(OpCode::GetIter, if asynced { super::ITER_ASYNC } else { 0 });
            loop_starts.push(self.chunk.instructions.len() as u16);
            for_iters.push(self.emit_jump(OpCode::ForIter));
            self.store_back(at, shape);
        }
    }

    /* Whether the next token starts the element of a comprehension. */
    pub(super) fn at_comp(&mut self) -> bool {
        self.peek().is_some() && self.tokens.peek().is_some_and(|t| t.comp)
    }

    /* A comprehension, a function over `iter()` of its first iterable called at once, a generator for `kind` Gen. */
    pub(super) fn comprehension(&mut self, kind: Comp) {
        // The element runs innermost, so the cursor skips it, compiles the clauses and comes back for it.
        let elem = self.tokens.pos;
        let elem_start = self.tokens.peek().map_or(self.last_end, |t| t.start) as u32;
        // A colon outside a lambda makes a brace comprehension a dict one.
        let (mut depth, mut lambdas, mut pair) = (0usize, 0usize, false);
        while let Some(t) = self.tokens.peek() {
            match t.kind {
                TokenType::For | TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace | TokenType::Endmarker if depth == 0 => break,
                TokenType::Async if depth == 0 && self.tokens.after_head() == TokenType::For => break,
                TokenType::Lpar | TokenType::Lsqb | TokenType::Lbrace => depth += 1,
                TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace => depth -= 1,
                TokenType::Lambda if depth == 0 => lambdas += 1,
                TokenType::Colon if depth == 0 && lambdas > 0 => lambdas -= 1,
                TokenType::Colon if depth == 0 => pair = true,
                _ => {}
            }
            self.tokens.pos += 1;
        }
        let elem_end = self.tokens.pos;
        let (build, add, name) = match kind {
            Comp::Gen => (None, OpCode::Yield, "<genexpr>"),
            Comp::List => (Some(OpCode::BuildList), OpCode::ListAppend, "<listcomp>"),
            Comp::Brace if pair => (Some(OpCode::BuildDict), OpCode::MapAdd, "<dictcomp>"),
            Comp::Brace => (Some(OpCode::BuildSet), OpCode::SetAdd, "<setcomp>"),
        };
        let asynced = self.eat_if(TokenType::Async);
        self.glued_keyword();
        self.eat(TokenType::For);
        let at = self.mark();
        let shape = self.tokens.target_list();
        if shape.is_none() { self.error("cannot assign to this expression"); }
        self.eat(TokenType::In);
        self.expr_bp(1);
        // An `async for` hands its iterable over as is, the body steps it the way `async for` does.
        if !asynced { self.chunk.emit(OpCode::GetIter, super::ITER_VALUE); }
        // A walrus binds where the comprehension stands, a `global` there included.
        let globals = self.globals_decl.clone();
        let mut body = self.with_fresh_chunk(|s| {
            s.in_comp = true;
            s.globals_decl = globals;
            // A body error points at the element until a clause marks its own line.
            s.chunk.stmt_pos.push((0, elem_start));
            s.bind_param(COMP_ARG);
            if let Some(op) = build { s.chunk.emit(op, 0); }
            let arg = s.push_ssa_name(COMP_ARG, 0);
            s.chunk.emit(OpCode::LoadName, arg);
            s.chunk.emit(OpCode::GetIter, if asynced { super::ITER_ASYNC } else { super::ITER_AS_IS });
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
            if s.tokens.pos != elem_end { s.error("expected 'for' after the element of a comprehension"); }
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
        self.push_function(alloc::vec![COMP_ARG.into()], body, 0, Some(name), OpCode::MakeFunction);
        self.chunk.emit(OpCode::Swap, 0);
        self.chunk.emit(OpCode::Call, 1);
    }

    /* A generator expression passed bare, which must be the only argument of its call. */
    fn genexp_arg(&mut self, first: bool) {
        let start = self.tokens.peek().map_or(self.last_end, |t| t.start);
        self.comprehension(Comp::Gen);
        if !first || matches!(self.peek(), Some(TokenType::Comma)) { self.reject(start, self.last_end, GENEXP_BARE); }
    }

    /* f-string emits literal+expr parts until FstringEnd, returns the count, caller wraps in BuildString. `fs_start/fs_end` anchor unclosed-string errors. */
    pub(super) fn fstring(&mut self, fs_start: usize, fs_end: usize) -> u16 {
        let mut parts = 0u16;
        let mut got_end = false;
        // Raw f-strings (`rf"..."`) keep backslashes literal, plain ones decode escapes like a normal string.
        let is_raw = super::types::has_raw_prefix(&self.source[fs_start..fs_end]);
        if matches!(self.peek(), Some(TokenType::FstringEnd)) {
            self.advance();
            return 0;
        }
        loop {
            match self.peek() {
            Some(TokenType::FstringMiddle) => {
                let t = self.advance();
                let raw = self.lexeme(&t);
                let mut unescaped = String::with_capacity(raw.len());
                let mut bad = false;
                // Single pass so `{{` is seen in the raw text before any escape can produce a brace.
                let mut chars = raw.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '{' if chars.peek() == Some(&'{') => { chars.next(); unescaped.push('{'); }
                        '}' if chars.peek() == Some(&'}') => { chars.next(); unescaped.push('}'); }
                        '\\' if !is_raw => bad |= super::types::push_escape(&mut unescaped, &mut chars),
                        _ => unescaped.push(c),
                    }
                }
                if bad { self.escape_at(&self.source[t.start..t.end], t.start, false); }
                self.emit_const(Value::Str(unescaped));
                parts += 1;
            }
                Some(TokenType::Lbrace) => {
                    self.advance();
                    // The `f"{expr=}"` prefix keeps the field text as written, its spaces included.
                    let field_start = self.last_end;
                    self.expr();
                    // Bare tuple in a replacement field, `f"{1,}"` builds (1,).
                    self.tuple_rest(1, |s| matches!(s.peek(), Some(TokenType::Rbrace | TokenType::Colon | TokenType::Exclamation | TokenType::Equal) | None));
                    /* FormatValue operand, bit0=has-spec, bits1-2=conversion (0=none,1=!r,2=!s,3=!a). */
                    let mut flags = 0u16;
                    // `=` debug emits "expr=" prefix, defaults to !r when no conv/spec given.
                    let mut debug_prefix: Option<String> = None;
                    if matches!(self.peek(), Some(TokenType::Equal)) {
                        self.advance();
                        let field_end = self.tokens.peek().map_or(self.last_end, |t| t.start);
                        debug_prefix = Some(self.source[field_start..field_end].to_string());
                    }
                    if matches!(self.peek(), Some(TokenType::Exclamation)) {
                        let bang = self.advance();
                        let conv_tok = self.advance();
                        let conv = self.lexeme(&conv_tok);
                        flags |= match conv {
                            "r" => 1 << 1,
                            "s" => 2 << 1,
                            "a" => 3 << 1,
                            _ => {
                                self.error_at(bang.start, conv_tok.end,
                                    "invalid f-string conversion (expected !r, !s, or !a)");
                                0
                            }
                        };
                    }
                    if debug_prefix.is_some() && (flags & 0b110) == 0 && !matches!(self.peek(), Some(TokenType::Colon)) {
                        flags |= 1 << 1; // default !r when `=` has no explicit conv/spec
                    }
                    // The prefix slides under the value, so the field code keeps its jumps and call positions.
                    if let Some(prefix) = debug_prefix.take() {
                        self.emit_const(Value::Str(prefix));
                        self.chunk.emit(OpCode::Swap, 0);
                        parts += 1;
                    }
                    if matches!(self.peek(), Some(TokenType::Colon)) {
                        self.advance();
                        self.fstring_spec(is_raw);
                        flags |= 1;
                    }
                    self.chunk.emit(OpCode::FormatValue, flags);
                    parts += 1;
                    if matches!(self.peek(), Some(TokenType::Rbrace)) {
                        self.advance();
                    }
                }
                Some(TokenType::FstringEnd) => {
                    self.advance();
                    got_end = true;
                    break;
                }
                _ => break
            }
        }
        if !got_end {
            self.error_at(fs_start, fs_end, "f-string was never closed");
        }
        parts
    }

    /* A replacement field spec as one string, raw text with nested `{expr}` fields formatted in, `f"{x:{w}}"`. */
    fn fstring_spec(&mut self, raw: bool) {
        let (mut pieces, mut lit_start) = (0u16, self.last_end);
        loop {
            let (kind, at) = self.tokens.peek().map_or((None, self.source.len()), |t| (Some(t.kind), t.start));
            if at > lit_start && matches!(kind, Some(TokenType::Rbrace | TokenType::Lbrace) | None) {
                let text = &self.source[lit_start..at];
                if !raw && text.contains('\\') { self.escape_at(text, lit_start, false); }
                self.emit_const(Value::Str(self.source[lit_start..at].to_string()));
                pieces += 1;
            }
            match kind {
                Some(TokenType::Lbrace) => {
                    self.advance();
                    self.expr();
                    self.chunk.emit(OpCode::FormatValue, 0);
                    pieces += 1;
                    self.eat(TokenType::Rbrace);
                    lit_start = self.last_end;
                }
                Some(TokenType::Rbrace) | None => break,
                _ => { self.tokens.next(); }
            }
        }
        match pieces {
            0 => self.emit_const(Value::Str(String::new())),
            1 => {}
            n => self.chunk.emit(OpCode::BuildString, n),
        }
    }

    /* Dispatches call, print/range opcodes, imported natives (shadow builtins), builtins table, else LoadName+Call. */
    pub(super) fn call(&mut self, name: &str) -> bool {
        let call_pos = self.last_end as u32;
        // A builtin name the scope binds, a parameter included, must call the binding, not the fused opcode.
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
            return true;
        }
        self.note_removed(name);
        if name == "print" {
            // Same packed layout as Call so the VM can split sep/end kwargs from positionals.
            let operand = self.fused_args(name, true);
            self.note_fused(OpCode::CallPrint);
            self.chunk.emit(OpCode::CallPrint, operand);
            self.chunk.record_call_pos(call_pos);
            return false;
        }

        if name == "range" {
            let operand = self.fused_args(name, false);
            if operand & super::KEYWORDS != 0 && (operand >> 8) & 0x3F != 0 { self.error("range() takes no keyword arguments"); }
            self.note_fused(OpCode::CallRange);
            self.chunk.emit(OpCode::CallRange, operand);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        // Imported natives shadow builtins, matching Python `from x import *` rebinding.
        if let Some(&extern_idx) = self.chunk.extern_index.get(name) {
            // A native call always opens its spread frame, the operand has no room for a flag.
            self.chunk.emit(OpCode::BeginArgs, 0);
            self.advance();
            let (pos, kw, _) = self.args_body(false);
            if pos > 0xF || kw > 0xF { self.error("native calls take at most 15 positional and 15 keyword arguments"); }
            // Operand packs extern_idx<<8 | kw<<4 | pos, same layout as Call.
            let encoded = (extern_idx << 8) | ((kw & 0xF) << 4) | (pos & 0xF);
            self.fused_externs.push(extern_idx);
            self.chunk.emit(OpCode::CallExtern, encoded);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        // dict()/min()/max()/enumerate() take keywords (`default=`/`key=`/`start=`), so keep positional and keyword counts distinct via the packed operand.
        if let Some(op) = match name {
            "dict" => Some(OpCode::CallDict),
            "min" => Some(OpCode::CallMin),
            "max" => Some(OpCode::CallMax),
            "enumerate" => Some(OpCode::CallEnumerate),
            _ => None,
        } {
            let operand = self.fused_args(name, true);
            self.note_fused(op);
            self.chunk.emit(op, operand);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        if let Some(op) = builtin(name) {
            let operand = self.fused_args(name, false);
            self.note_fused(op);
            self.chunk.emit(op, operand);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        let i = self.push_ssa_name(name, self.current_version(name));
        self.chunk.emit(OpCode::LoadName, i);
        self.advance();
        self.call_rest(call_pos);
        true
    }

    #[inline]
    fn note_fused(&mut self, op: OpCode) { self.fused |= 1u128 << (op as u8 & 127); }

    /* The builtins and native imports the body called before binding them, so they were locals all along. */
    fn called_then_bound(&self) -> Vec<String> {
        let mut bits = self.fused;
        let fused = core::iter::from_fn(|| {
            if bits == 0 { return None; }
            let bit = bits.trailing_zeros() as u8;
            bits &= bits - 1;
            Some(super::types::FUSED.iter().find(|&&op| op as u8 == bit).and_then(|&op| super::types::fused_native(op)).map(|id| id.name()))
        }).flatten();
        let externs = self.fused_externs.iter().filter_map(|&i| self.chunk.extern_table.get(i as usize)).map(|f| f.name.as_str());
        let mut late: Vec<String> = fused.chain(externs).filter(|n| self.ssa_versions.contains(n)).map(String::from).collect();
        late.dedup();
        late
    }

    /* Args after `(` and the call, `CallSpread` once a spread opened its frame. */
    pub(super) fn call_rest(&mut self, call_pos: u32) {
        let (pos, kw, spread) = self.args_body(true);
        self.chunk.emit(if spread { OpCode::CallSpread } else { OpCode::Call }, super::pack_call(pos, kw));
        self.chunk.record_call_pos(call_pos);
    }

    /* Parses the args of fused builtin `name` into its operand, `packed` keeps the keyword count. */
    fn fused_args(&mut self, name: &str, packed: bool) -> u16 {
        self.advance();
        let (pos, kw, spread) = self.args_body(true);
        if kw > 0x3F { self.error("too many keyword arguments in call (max 63)"); }
        self.fused_operand(name, pos, kw, spread, packed)
    }

    /* Flags a spread, uncounted keywords or a count outside the arity, so the VM runs a plain call. */
    fn fused_operand(&self, name: &str, pos: u16, kw: u16, spread: bool, packed: bool) -> u16 {
        let fits = crate::value::NativeFnId::from_name(name).is_none_or(|id| id.takes(pos));
        if spread { super::pack_call(pos, kw) | super::SPREAD_ARGS }
        else if !fits || (!packed && kw > 0) { super::pack_call(pos, kw) | super::KEYWORDS }
        else if packed { super::pack_call(pos, kw) }
        else { pos }
    }

    // Parse args after `(` already consumed. Depth-guarded since the name-led arg path recurses through `name`/`call` without passing `expr_bp`.
    pub(super) fn args_body(&mut self, lazy: bool) -> (u16, u16, bool) {
        self.expr_depth += 1;
        if self.expr_depth > super::types::MAX_EXPR_DEPTH {
            self.expr_depth -= 1;
            self.error("expression too deeply nested");
            return (0, 0, false);
        }
        let mut pos = 0u16;
        let mut kw = 0u16;
        let mut spread = false;
        self.comma_list(|t| t == TokenType::Rpar, |s| {
            let unpack = if s.eat_if(TokenType::DoubleStar) { Some(2u16) }
                else if s.eat_if(TokenType::Star) { Some(1u16) }
                else { None };
            if let Some(kind) = unpack {
                // The first spread opens the frame its call closes, a native call opened one up front.
                if lazy && !spread { s.chunk.emit(OpCode::BeginArgs, 0); }
                spread = true;
                s.expr();
                // High bits carry preceding kw-pair count so the VM keeps positionals contiguous.
                s.chunk.emit(OpCode::UnpackArgs, (kw << 2) | kind);
                pos = pos.saturating_add(1);
            } else if matches!(s.peek(), Some(TokenType::Name | TokenType::Underscore)) {
                let at = s.tokens.pos;
                let t = s.advance();
                if matches!(s.peek(), Some(TokenType::Equal)) {
                    let kw_name = s.lexeme(&t).to_string();
                    s.advance();
                    s.emit_const(Value::Str(kw_name));
                    s.expr();
                    kw = kw.saturating_add(1);
                } else if t.comp {
                    s.tokens.pos = at;
                    s.genexp_arg(pos + kw == 0);
                    pos = pos.saturating_add(1);
                } else {
                    let elem_start = s.chunk.instructions.len();
                    s.name_operand(t);
                    s.infix_bp(0, elem_start);
                    // Name-led arg bypasses expr(), parse a trailing ternary here too.
                    s.saw_newline = false;
                    s.ternary_tail(elem_start);
                    pos = pos.saturating_add(1);
                }
            } else {
                if s.at_comp() { s.genexp_arg(pos + kw == 0); } else { s.expr(); }
                pos = pos.saturating_add(1);
            }
        });
        self.eat(TokenType::Rpar);
        self.expr_depth -= 1;
        if pos > 0xFF || kw > 0xFF { self.error("too many arguments in call (max 255 positional and 255 keyword)"); }
        (pos, kw, spread)
    }

    /* Consume the next Name, or emit a non-syncing diagnostic and return a synthetic name so parsing continues. */
    fn ident_or_missing(&mut self, msg: &str) -> &'src str {
        // `_` lexes as the wildcard of `match`, yet names a function or class like any identifier.
        if matches!(self.peek(), Some(TokenType::Name | TokenType::Underscore)) {
            self.advance_text()
        } else {
            self.diag_at_peek(msg);
            "<missing>"
        }
    }

    /* Emit one `Call(1)` per decorator, innermost first, each applied to the previous result. */
    fn emit_decorator_calls(&mut self, n: u16) {
        for _ in 0..n {
            let pos = self.last_end as u32;
            self.chunk.emit(OpCode::Call, 1);
            self.chunk.record_call_pos(pos);
        }
    }

    /* class compiles body into fresh chunk, emits MakeClass+decorators+StoreName. */
    pub(super) fn class_def_with(&mut self, decorators: u16) {
        self.decorated_at = None;
        // Missing name, non-syncing diagnostic + synthetic name so body still parses.
        let cname = self.ident_or_missing("expected class name");
        let params = self.type_params();
        if params.is_empty() { self.class_value(cname, &params); } else { self.generic_scope(&params, cname, |s| s.class_value(cname, &params)); }

        // Each decorator Calls with the previous result, same as for functions.
        self.emit_decorator_calls(decorators);

        self.emit_store_new(cname);
    }

    /* The class `cname` names on the stack, its bases and body compiled in order. */
    fn class_value(&mut self, cname: &'src str, params: &[String]) {
        // Bases are pushed left-to-right, `MakeClass` pops `num_bases` and stores them in the Class.
        let (mut num_bases, mut num_kw, mut spread) = (0u16, 0u16, false);
        if self.eat_if(TokenType::Lpar) {
            while !matches!(self.peek(), Some(TokenType::Rpar) | None) {
                // A class keyword goes to `__init_subclass__` as a name and value pair, `metaclass=` parses and then fails.
                if matches!(self.peek(), Some(TokenType::Name)) {
                    let t = self.advance();
                    if self.eat_if(TokenType::Equal) {
                        let name = self.lexeme(&t).to_string();
                        if name == "metaclass" { self.reject(t.start, t.end, "metaclasses are not supported"); }
                        self.emit_const(Value::Str(name));
                        self.expr();
                        num_kw = num_kw.saturating_add(1);
                    } else {
                        if num_kw > 0 { self.reject(t.start, t.end, "positional argument follows keyword argument"); }
                        let start = self.chunk.instructions.len();
                        self.name_operand(t);
                        self.expr_tails(start);
                        if spread { self.chunk.emit(OpCode::ListAppend, 0); }
                        num_bases = num_bases.saturating_add(1);
                    }
                } else if self.eat_if(TokenType::Star) {
                    // `class B(*bases)` gathers every base into one list, which MakeClass unpacks.
                    if num_kw > 0 { self.diag_at_peek("iterable argument unpacking follows keyword argument unpacking"); }
                    if !spread { self.chunk.emit(OpCode::BuildList, num_bases); spread = true; }
                    self.expr();
                    self.chunk.emit(OpCode::ListExtend, 0);
                } else {
                    if num_kw > 0 { self.diag_at_peek("positional argument follows keyword argument"); }
                    self.expr();
                    if spread { self.chunk.emit(OpCode::ListAppend, 0); }
                    num_bases = num_bases.saturating_add(1);
                }
                if !self.eat_if(TokenType::Comma) { break; }
            }
            self.eat(TokenType::Rpar);
        }
        if spread { num_bases = SPREAD_BASES; }

        self.eat(TokenType::Colon);

        let cut = self.enter_qual(cname, false);
        let body = self.with_fresh_chunk(|s| {
            s.in_class_body = true;
            let outer = core::mem::take(&mut s.class_anns);
            // `class Box[T]` keeps its parameters in `__type_params__`, which makes `Box[int]` an alias.
            if !params.is_empty() {
                for p in params { s.emit_load_ssa(param_name(p)); }
                s.chunk.emit(OpCode::BuildTuple, params.len() as u16);
                s.store_name("__type_params__");
            }
            s.compile_block();
            let anns = core::mem::replace(&mut s.class_anns, outer);
            if !anns.is_empty() { s.emit_annotate(&anns); s.store_name("__annotate__"); }
        });
        self.qual.truncate(cut);

        // The operand is the class index, the body keeps how many bases its MakeClass pops.
        let Ok(ci) = u16::try_from(self.chunk.classes.len()) else { self.error("too many classes in this scope (limit 65535)"); return; };
        let mut body = body;
        // The free names of its methods pass through the class to the function around it, its own members staying behind.
        let around_function = self.in_function || self.qual.contains("<locals>");
        let mut stored = alloc::vec![false; body.names.len()];
        for ins in body.instructions.iter().filter(|i| matches!(i.opcode, OpCode::StoreName | OpCode::Phi)) {
            if let Some(m) = stored.get_mut(ins.operand as usize) { *m = true; }
        }
        for ((name, attr), own) in body.names.iter().zip(body.attr_only_names()).zip(stored) {
            let inner = body.functions.iter().any(|f| f.1.names.contains(name)) || body.classes.iter().any(|c| c.names.contains(name));
            if around_function && !attr && !own && !name.starts_with('#') && inner { self.chunk.push_name(name); }
        }
        (body.bases, body.class_kw) = (num_bases, num_kw);
        body.qualname = self.qualified(cname);
        self.chunk.classes.push(body);
        self.chunk.emit(OpCode::MakeClass, ci);
    }

    /* def/async def parses signature, compiles body, emits MakeFunction/MakeCoroutine+decorators+StoreName. */
    pub(super) fn func_def_inner(&mut self, decorators: u16, is_async: bool) {
        // Token lines count from zero.
        let first_line = self.decorated_at.take().unwrap_or(self.last_line) as u32 + 1;
        // Missing name, non-syncing diagnostic + synthetic name so signature+body still parse.
        let fname = self.ident_or_missing("expected function name");
        self.check_member(fname, self.last_end);
        let tparams = self.type_params();
        if tparams.is_empty() { self.func_value(fname, first_line, is_async); } else {
            self.generic_scope(&tparams, fname, |s| {
                s.func_value(fname, first_line, is_async);
                s.chunk.emit(OpCode::Dup, 0);
                for p in &tparams { s.emit_load_ssa(param_name(p)); }
                s.chunk.emit(OpCode::BuildTuple, tparams.len() as u16);
                let i = s.chunk.push_name("__type_params__");
                s.chunk.emit(OpCode::StoreAttr, i);
            });
        }

        self.emit_decorator_calls(decorators);

        self.emit_store_new(fname);
    }

    /* The function `fname` names on the stack, its annotations waiting in `__annotate__`. */
    fn func_value(&mut self, fname: &'src str, first_line: u32, is_async: bool) {
        let (params, defaults) = self.parse_params();
        let anns = core::mem::take(&mut self.param_anns);
        let posonly = self.posonly;
        let cut = self.enter_qual(fname, true);
        let mut body = self.compile_body(&params);
        self.qual.truncate(cut);
        (body.posonly, body.first_line) = (posonly, first_line);

        // Propagate free names to parent chunk so nested defs capture grandparent vars.
        self.push_function(params, body, defaults, Some(fname), if is_async { OpCode::MakeCoroutine } else { OpCode::MakeFunction });
        // Annotations wait in `__annotate__`, which the first read of `__annotations__` calls.
        if !anns.is_empty() {
            self.chunk.emit(OpCode::Dup, 0);
            self.emit_annotate(&anns);
            let i = self.chunk.push_name("__annotate__");
            self.chunk.emit(OpCode::StoreAttr, i);
        }
    }

    /* Rejects a dunder a class body binds that the engine never calls, `end` closing its name. */
    pub(super) fn check_member(&mut self, name: &str, end: usize) {
        if self.in_class_body && name.len() > 4 && name.starts_with("__") && name.ends_with("__") && !class_dunder(name) {
            self.reject(end - name.len(), end, &s!("'", str name, "' is not supported in a class body"));
        }
    }

    /* Whether the code from `from` builds a list, dict or set, by display, comprehension or constructor. */
    fn mutable_from(&self, from: usize) -> bool {
        let ins = &self.chunk.instructions[from..];
        let built = |op: OpCode| matches!(op, OpCode::BuildList | OpCode::BuildDict | OpCode::BuildSet | OpCode::CallList | OpCode::CallDict | OpCode::CallSet);
        let display = ins.last().is_some_and(|i| built(i.opcode));
        let comp = |fi: u16| self.chunk.functions.get(fi as usize).and_then(|f| self.chunk.names.get(f.3 as usize)).is_some_and(|n| matches!(ssa_strip(n), "<listcomp>" | "<setcomp>" | "<dictcomp>"));
        let comprehension = matches!(ins, [.., m, s, c] if m.opcode == OpCode::MakeFunction && s.opcode == OpCode::Swap && c.opcode == OpCode::Call && comp(m.operand));
        display || comprehension
    }

    /* A `[T, *Ts, **P]` type parameter list, each name with the stars of its kind, bounds and defaults skipped since the engine is dynamically typed. */
    pub(super) fn type_params(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        if !self.eat_if(TokenType::Lsqb) { return names; }
        let (mut depth, mut expect_name, mut stars) = (1, true, "");
        while depth > 0 {
            match self.peek() {
                Some(TokenType::Lsqb | TokenType::Lpar | TokenType::Lbrace) => depth += 1,
                Some(TokenType::Rsqb | TokenType::Rpar | TokenType::Rbrace) => depth -= 1,
                Some(TokenType::Comma) if depth == 1 => expect_name = true,
                Some(TokenType::Star) if depth == 1 && expect_name => stars = "*",
                Some(TokenType::DoubleStar) if depth == 1 && expect_name => stars = "**",
                Some(TokenType::Name) if depth == 1 && expect_name => {
                    let name = self.advance_text();
                    names.push(s!(str core::mem::take(&mut stars), str name));
                    expect_name = false;
                    continue;
                }
                None => return names,
                _ => {}
            }
            self.advance();
        }
        names
    }

    /* Compiles `build` inside a scope that binds each type parameter, called at once so the definition it leaves comes back. */
    pub(super) fn generic_scope(&mut self, params: &[String], name: &str, build: impl FnOnce(&mut Self)) {
        let body = self.with_fresh_chunk(|s| {
            s.in_function = true;
            for p in params {
                // A hidden name, so the parameter is never read as the variable that binds it.
                let idx = s.chunk.push_name(&s!("#", str p));
                s.chunk.emit(OpCode::MakeTypeVar, idx);
                s.store_name(param_name(p));
            }
            build(s);
            s.chunk.emit(OpCode::ReturnValue, 0);
        });
        self.push_function(Vec::new(), body, 0, None, OpCode::MakeFunction);
        if let Some(f) = self.chunk.functions.last_mut() { f.1.qualname = s!("<generic parameters of ", str name, ">"); }
        self.chunk.emit(OpCode::Call, 0);
    }

    pub(super) fn parse_params(&mut self) -> (Vec<String>, u16) {
        // No `(`, diagnostic, consume `:` so compile_body starts at Indent correctly.
        if !matches!(self.peek(), Some(TokenType::Lpar)) {
            self.diag_at_peek("expected '('");
            self.eat_if(TokenType::Colon);
            return (Vec::new(), 0);
        }
        self.advance();
        let (params, defaults) = self.param_list(TokenType::Rpar, true);
        self.eat(TokenType::Rpar);
        if self.eat_if(TokenType::Rarrow) {
            self.param_anns.push(("return".into(), self.mark()));
            while !matches!(self.peek(), Some(TokenType::Colon) | None) { self.advance(); }
        }
        self.eat(TokenType::Colon);
        (params, defaults)
    }

    /* Parameters up to `close`, a bare `*` makes the rest keyword-only and `/` only separates. */
    pub(super) fn param_list(&mut self, close: TokenType, annotated: bool) -> (Vec<String>, u16) {
        let mut params: Vec<String> = Vec::new();
        let mut defaults = 0u16;
        let mut kw_only = false;
        self.posonly = 0;
        let at_end = |s: &mut Self| matches!(s.peek(), Some(TokenType::Rarrow) | None) || s.peek() == Some(close);
        while !at_end(self) {
            let prefix = if self.eat_if(TokenType::Slash) { self.posonly = params.len() as u16; None }
                else if self.eat_if(TokenType::DoubleStar) { Some("**") }
                else if self.eat_if(TokenType::Star) {
                    let bare = matches!(self.peek(), Some(TokenType::Comma)) || at_end(self);
                    kw_only |= bare;
                    (!bare).then_some("*")
                }
                else { Some(if kw_only { "~" } else { "" }) };
            if let Some(prefix) = prefix {
                let nm = self.advance_text();
                params.push(s!(str prefix, str &nm));
                // What follows `*args` is keyword-only, as after a bare `*`.
                kw_only |= prefix == "*";
                if annotated && let Some(at) = self.drain_annotation() { self.param_anns.push((nm.into(), at)); }
                // Trailing `=` marks a param carrying a default value.
                if prefix.len() < 2 && self.eat_if(TokenType::Equal) {
                    let (from, at) = (self.chunk.instructions.len(), self.tokens.peek().map_or(self.last_end, |t| t.start));
                    self.expr();
                    if self.mutable_from(from) { self.reject(at, self.last_end, "a list, dict or set default is shared by every call, default to None and build it in the body"); }
                    defaults += 1;
                    if let Some(last) = params.last_mut() { last.push('='); }
                }
            }
            if !at_end(self) { self.eat(TokenType::Comma); }
        }
        (params, defaults)
    }

    /* Drains annotation via `advance_raw` (keeps bracket_stack clean), breaks on Rarrow to avoid infinite drain, and returns where it starts. */
    pub(super) fn drain_annotation(&mut self) -> Option<Mark> {
        let at = self.eat_if(TokenType::Colon).then(|| self.mark());
        if at.is_some() {
            let mut depth = 0u32;
            loop {
                match self.peek() {
                    None => break,
                    Some(TokenType::Rarrow) => break,
                    Some(TokenType::Lsqb | TokenType::Lpar | TokenType::Lbrace) => {
                        depth += 1;
                        self.advance_raw();
                    }
                    Some(TokenType::Rsqb | TokenType::Rpar | TokenType::Rbrace) => {
                        if depth == 0 { break; }
                        depth -= 1;
                        self.advance_raw();
                    }
                    Some(TokenType::Equal | TokenType::Comma) if depth == 0 => break,
                    _ => { self.advance_raw(); }
                }
            }
        }
        at
    }

    /* `__annotate__`, a function of `format` that evaluates each annotation in order and returns them by name, compiled from where each one starts. */
    pub(super) fn emit_annotate(&mut self, anns: &[(String, Mark)]) {
        let back = self.mark();
        let outer = self.ssa_versions.clone();
        let params = alloc::vec![String::from("format")];
        let body = self.with_fresh_chunk(|s| {
            s.ssa_versions = outer;
            s.bind_param("format");
            s.in_function = true;
            for (name, at) in anns {
                s.emit_const(Value::Str(name.clone()));
                s.seek(*at);
                // `*args: *Ts` takes the one item its annotation unpacks to.
                if s.eat_if(TokenType::Star) { s.expr(); s.chunk.emit(OpCode::UnpackSequence, 1); } else { s.expr(); }
            }
            s.chunk.emit(OpCode::BuildDict, anns.len() as u16);
            s.chunk.emit(OpCode::ReturnValue, 0);
        });
        self.seek(back);
        self.push_function(params, body, 0, Some("__annotate__"), OpCode::MakeFunction);
    }

    fn body_of(&mut self, params: &[String]) {
        self.in_function = true;
        for p in params {
            // Base name shadows the enclosing scope, prefix/`=` marker must be stripped.
            self.bind_param(p);
            let _ = self.push_ssa_name(super::types::param_base_name(p), 0);
        }
        self.compile_block();
    }

    pub(super) fn compile_body(&mut self, params: &[String]) -> SSAChunk {
        let restart = self.mark();
        let known = if self.late.is_empty() { None } else { self.late.get(&restart.pos).cloned() };
        let mut late = Vec::new();
        let mut body = self.with_fresh_chunk(|s| { s.unfused.extend(known.iter().flatten().cloned()); s.body_of(params); late = s.called_then_bound(); });
        // A builtin the body calls before binding it is a local all along, so the body compiles once more and remembers that.
        if known.is_none() && !late.is_empty() {
            self.rewind(restart);
            body = self.with_fresh_chunk(|s| { s.unfused.extend(late.iter().cloned()); s.body_of(params); });
            self.late.insert(restart.pos, late);
        }
        // Pre-compute is_generator to avoid O(n) scan per `exec_call`.
        body.is_generator = body.instructions.iter().any(|i| matches!(
            i.opcode,
            OpCode::Yield
        ));
        body
    }
}

/* The bracket a comprehension sits in, a paren making a generator expression. */
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Comp { Gen, List, Brace }
