use crate::s;

use super::Parser;
use super::types::builtin;
use super::types::{OpCode, Value, SSAChunk, Instruction, class_dunder, ssa_strip, COMP_ARG};

use crate::lexer::{Token, TokenType};

use alloc::{string::{String, ToString}, vec::Vec};

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
            if let Some(&ls) = loop_starts.last() && self.eat_if(TokenType::If) {
                self.expr_bp(1);
                self.chunk.emit(OpCode::JumpIfFalse, ls);
                continue;
            }
            if !self.eat_if(TokenType::For) { break; }
            let (targets, star, comma) = self.target_list(|s| matches!(s.peek(), Some(TokenType::In)));
            self.eat(TokenType::In);
            self.expr_bp(1);
            self.chunk.emit(OpCode::GetIter, 0);
            loop_starts.push(self.chunk.instructions.len() as u16);
            for_iters.push(self.emit_jump(OpCode::ForIter));
            self.store_targets(&targets, star, comma);
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
        self.eat(TokenType::For);
        let (targets, star, comma) = self.target_list(|s| matches!(s.peek(), Some(TokenType::In)));
        self.eat(TokenType::In);
        self.expr_bp(1);
        self.chunk.emit(OpCode::GetIter, super::ITER_VALUE);
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
            s.chunk.emit(OpCode::GetIter, super::ITER_AS_IS);
            let mut loop_starts = alloc::vec![s.chunk.instructions.len() as u16];
            let mut for_iters = alloc::vec![s.emit_jump(OpCode::ForIter)];
            s.store_targets(&targets, star, comma);
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
                // Single pass so `{{` is seen in the raw text before any escape can produce a brace.
                let mut chars = raw.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '{' if chars.peek() == Some(&'{') => { chars.next(); unescaped.push('{'); }
                        '}' if chars.peek() == Some(&'}') => { chars.next(); unescaped.push('}'); }
                        '\\' if !is_raw => super::types::push_escape(&mut unescaped, &mut chars),
                        _ => unescaped.push(c),
                    }
                }
                self.emit_const(Value::Str(unescaped));
                parts += 1;
            }
                Some(TokenType::Lbrace) => {
                    self.advance();
                    // Capture span for `f"{expr=}"` debug prefix.
                    let expr_start_byte = self.tokens.peek().map(|t| t.start).unwrap_or(0);
                    let insn_start = self.chunk.instructions.len();
                    self.expr();
                    // Bare tuple in a replacement field, `f"{1,}"` builds (1,).
                    self.tuple_rest(1, |s| matches!(s.peek(), Some(TokenType::Rbrace | TokenType::Colon | TokenType::Exclamation | TokenType::Equal) | None));
                    let expr_end_byte = self.last_end;
                    /* FormatValue operand, bit0=has-spec, bits1-2=conversion (0=none,1=!r,2=!s,3=!a). */
                    let mut flags = 0u16;
                    // `=` debug emits "expr=" prefix, defaults to !r when no conv/spec given.
                    let mut debug_prefix: Option<String> = None;
                    if matches!(self.peek(), Some(TokenType::Equal)) {
                        self.advance();
                        let raw = &self.source[expr_start_byte..expr_end_byte];
                        debug_prefix = Some(s!(str raw, "="));
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
                    // Drain expr bytecode, emit prefix const, re-emit expr so `stack=[prefix, value]`.
                    if let Some(prefix) = debug_prefix.take() {
                        let drained: Vec<Instruction> = self.chunk.instructions
                            .drain(insn_start..)
                            .collect();
                        self.emit_const(Value::Str(prefix));
                        parts += 1;
                        self.chunk.instructions.extend(drained);
                    }
                    if matches!(self.peek(), Some(TokenType::Colon)) {
                        self.advance();
                        self.fstring_spec();
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
    fn fstring_spec(&mut self) {
        let (mut pieces, mut lit_start) = (0u16, self.last_end);
        loop {
            let (kind, at) = self.tokens.peek().map_or((None, self.source.len()), |t| (Some(t.kind), t.start));
            if at > lit_start && matches!(kind, Some(TokenType::Rbrace | TokenType::Lbrace) | None) {
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
    pub(super) fn call(&mut self, name: String) -> bool {
        let call_pos = self.last_end as u32;
        // A builtin name the scope binds, a parameter included, must call the binding, not the fused opcode.
        if self.ssa_versions.contains_key(&name) || self.globals_decl.contains(&name) || self.unfused.contains(&name) {
            let i = self.push_ssa_name(&name, self.current_version(&name));
            if self.globals_decl.contains(&name) {
                let gi = self.chunk.push_name(&name);
                self.chunk.emit(OpCode::LoadGlobal, gi);
            } else {
                self.chunk.emit(OpCode::LoadName, i);
            }
            self.advance();
            self.call_rest(call_pos);
            return true;
        }
        self.note_removed(&name);
        if name == "print" {
            // Same packed layout as Call so the VM can split sep/end kwargs from positionals.
            let operand = self.fused_args(&name, true);
            self.note_fused(OpCode::CallPrint);
            self.chunk.emit(OpCode::CallPrint, operand);
            self.chunk.record_call_pos(call_pos);
            return false;
        }

        if name == "range" {
            let operand = self.fused_args(&name, false);
            if operand & super::KEYWORDS != 0 && (operand >> 8) & 0x3F != 0 { self.error("range() takes no keyword arguments"); }
            self.note_fused(OpCode::CallRange);
            self.chunk.emit(OpCode::CallRange, operand);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        // Imported natives shadow builtins, matching Python `from x import *` rebinding.
        if let Some(&extern_idx) = self.chunk.extern_index.get(&name) {
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
        if let Some(op) = match name.as_str() {
            "dict" => Some(OpCode::CallDict),
            "min" => Some(OpCode::CallMin),
            "max" => Some(OpCode::CallMax),
            "enumerate" => Some(OpCode::CallEnumerate),
            _ => None,
        } {
            let operand = self.fused_args(&name, true);
            self.note_fused(op);
            self.chunk.emit(op, operand);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        if let Some(op) = builtin(name.as_str()) {
            let operand = self.fused_args(&name, false);
            self.note_fused(op);
            self.chunk.emit(op, operand);
            self.chunk.record_call_pos(call_pos);
            return true;
        }

        let i = self.push_ssa_name(&name, self.current_version(&name));
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
        let mut late: Vec<String> = fused.chain(externs).filter(|n| self.ssa_versions.contains_key(*n)).map(String::from).collect();
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
            } else if matches!(s.peek(), Some(TokenType::Name)) {
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
    fn ident_or_missing(&mut self, msg: &str) -> String {
        if matches!(self.peek(), Some(TokenType::Name)) {
            self.advance_text()
        } else {
            self.diag_at_peek(msg);
            "<missing>".to_string()
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
        // Missing name, non-syncing diagnostic + synthetic name so body still parses.
        let cname = self.ident_or_missing("expected class name");
        let params = self.type_params();

        // Bases are pushed left-to-right, `MakeClass` pops `num_bases` and stores them in the Class.
        let mut num_bases: u16 = 0;
        if self.eat_if(TokenType::Lpar) {
            while !matches!(self.peek(), Some(TokenType::Rpar) | None) {
                // `metaclass=` and every other class keyword parse and then fail, the class model has none.
                if matches!(self.peek(), Some(TokenType::Name)) {
                    let t = self.advance();
                    if self.eat_if(TokenType::Equal) {
                        self.reject(t.start, t.end, "class keywords such as 'metaclass' are not supported");
                        self.expr();
                    } else {
                        let start = self.chunk.instructions.len();
                        self.name_operand(t);
                        self.expr_tails(start);
                        num_bases = num_bases.saturating_add(1);
                    }
                } else {
                    self.expr();
                    num_bases = num_bases.saturating_add(1);
                }
                if !self.eat_if(TokenType::Comma) { break; }
            }
            self.eat(TokenType::Rpar);
        }

        self.eat(TokenType::Colon);

        let body = self.with_fresh_chunk(|s| {
            s.in_class_body = true;
            // `class Box[T]` keeps its parameters in `__type_params__`, which makes `Box[int]` an alias.
            if !params.is_empty() {
                for p in &params {
                    let idx = s.chunk.push_name(p);
                    s.chunk.emit(OpCode::MakeTypeVar, idx);
                }
                s.chunk.emit(OpCode::BuildTuple, params.len() as u16);
                s.store_name("__type_params__".into());
            }
            s.compile_block();
        });

        let ci = self.chunk.classes.len() as u16;
        // Operand packs `(num_bases << 8) | class_idx`, each field is one byte to keep the dispatch decode cheap.
        if ci > 0xFF { self.error("too many classes in this scope (limit 255)"); return; }
        if num_bases > 0xFF { self.error("too many base classes (limit 255)"); return; }
        self.chunk.classes.push(body);
        self.chunk.emit(OpCode::MakeClass, (num_bases << 8) | ci);

        // Each decorator Calls with the previous result, same as for functions.
        self.emit_decorator_calls(decorators);

        self.emit_store_new(&cname);
    }

    /* def/async def parses signature, compiles body, emits MakeFunction/MakeCoroutine+decorators+StoreName. */
    pub(super) fn func_def_inner(&mut self, decorators: u16, is_async: bool) {
        // Missing name, non-syncing diagnostic + synthetic name so signature+body still parse.
        let fname = self.ident_or_missing("expected function name");
        self.check_member(&fname, self.last_end);
        self.type_params();
        let (params, defaults) = self.parse_params();
        let body = self.compile_body(&params);

        // Propagate free names to parent chunk so nested defs capture grandparent vars.
        self.push_function(params, body, defaults, Some(&fname), if is_async { OpCode::MakeCoroutine } else { OpCode::MakeFunction });

        self.emit_decorator_calls(decorators);

        self.emit_store_new(&fname);
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

    /* Names of a `[T, *Ts, **P]` type parameter list, bounds and defaults skipped since the engine is dynamically typed. */
    pub(super) fn type_params(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        if !self.eat_if(TokenType::Lsqb) { return names; }
        let (mut depth, mut expect_name) = (1, true);
        while depth > 0 {
            match self.peek() {
                Some(TokenType::Lsqb | TokenType::Lpar | TokenType::Lbrace) => depth += 1,
                Some(TokenType::Rsqb | TokenType::Rpar | TokenType::Rbrace) => depth -= 1,
                Some(TokenType::Comma) if depth == 1 => expect_name = true,
                Some(TokenType::Name) if depth == 1 && expect_name => { names.push(self.advance_text()); expect_name = false; continue; }
                None => return names,
                _ => {}
            }
            self.advance();
        }
        names
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
        let at_end = |s: &mut Self| matches!(s.peek(), Some(TokenType::Rarrow) | None) || s.peek() == Some(close);
        while !at_end(self) {
            let prefix = if self.eat_if(TokenType::Slash) { None }
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
                if annotated { self.drain_annotation(); }
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

    /* Drains annotation via `advance_raw` (keeps bracket_stack clean), breaks on Rarrow to avoid infinite drain. */
    pub(super) fn drain_annotation(&mut self) {
        if self.eat_if(TokenType::Colon) {
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
    }

    fn body_of(&mut self, params: &[String]) {
        for p in params {
            // Base name shadows the enclosing scope, prefix/`=` marker must be stripped.
            self.bind_param(p);
            let _ = self.push_ssa_name(super::types::param_base_name(p), 0);
        }
        self.compile_block_body();
    }

    pub(super) fn compile_body(&mut self, params: &[String]) -> SSAChunk {
        let restart = self.mark();
        let mut late = Vec::new();
        let mut body = self.with_fresh_chunk(|s| { s.body_of(params); late = s.called_then_bound(); });
        // A builtin called before the body binds it is a local all along, so the body compiles again calling the local.
        if !late.is_empty() {
            self.rewind(restart);
            body = self.with_fresh_chunk(|s| { s.unfused = late; s.body_of(params); });
        }
        body.is_pure = !body.instructions.iter().any(|i| matches!(
            i.opcode,
            OpCode::CallPrint
            | OpCode::StoreItem
            | OpCode::DelItem
            | OpCode::DelAttr
            | OpCode::StoreAttr
            | OpCode::CallInput
            | OpCode::Global
            | OpCode::Nonlocal
            | OpCode::Raise
            | OpCode::RaiseFrom
            | OpCode::Yield
        ));
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
