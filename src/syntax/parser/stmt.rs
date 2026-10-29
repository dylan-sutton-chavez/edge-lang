use super::{Mark, Parser, Shape};
use super::expr::Tail;
use crate::{code::OpCode, syntax::diag::Diagnostic};
use crate::s;

use crate::syntax::lexer::{Spelling, Token, TokenType};

use alloc::{string::ToString, vec::Vec};

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {

    /* records the offset of the next token before any emit so `resolve` maps what follows */
    pub(super) fn mark_stmt(&mut self) {
        let ip = self.chunk.instructions.len() as u32;
        let pos = self.tokens.peek().map(|t| t.start as u32).unwrap_or(self.last_end as u32);
        self.chunk.stmt_pos.push((ip, pos));
    }

    /* one statement and true when it leaves a value for the caller to pop */
    pub(super) fn stmt(&mut self) -> bool {
        self.mark_stmt();

        match self.peek() {
            Some(TokenType::If) => {
                self.if_stmt();
                false
            }
            Some(TokenType::For) => {
                self.for_stmt();
                false
            }
            Some(TokenType::Fn) => {
                self.advance();
                self.func_def_inner(0, false);
                false
            }
            Some(TokenType::With) => {
                if let Some(t) = self.tokens.peek() { let (s, e) = (t.start, t.end); self.reject(s, e, Self::WITH_REFUSED); }
                self.with_stmt();
                false
            }
            Some(TokenType::While) => {
                self.while_stmt();
                false
            }
            Some(TokenType::Match) => {
                self.match_stmt();
                false
            }
            Some(TokenType::Yield) => {
                self.advance();
                self.emit_yield();
                true
            }
            Some(TokenType::Async) => {
                let t = self.advance();
                match self.peek() {
                    Some(TokenType::Fn) => {
                        self.advance();
                        self.func_def_inner(0, true);
                    }
                    Some(TokenType::For) => {
                        self.reject(t.start, t.end, Self::ASYNC_FOR_REFUSED);
                        self.for_stmt();
                    }
                    Some(TokenType::With) => {
                        self.reject(t.start, t.end, Self::ASYNC_WITH_REFUSED);
                        self.with_stmt();
                    }
                    _ => {}
                }
                false
            }
            Some(TokenType::Await) => {
                let t = self.advance();
                self.check_await(t.start, t.end);
                self.expr();
                self.emit_await();
                true
            }
            Some(TokenType::At) if Self::SHAPES.decorators => {
                let mut count = 0u16;
                self.decorated_at = self.tokens.peek().map(|t| t.line);
                while self.eat_if(TokenType::At) {
                    self.expr();
                    count += 1;
                }
                if self.eat_if(TokenType::Async) {
                    self.advance();
                    self.func_def_inner(count, true);
                } else if matches!(self.peek(), Some(TokenType::Class)) {
                    self.advance();
                    self.class_def_with(count);
                } else {
                    self.advance();
                    self.func_def_inner(count, false);
                }
                false
            }
            Some(TokenType::Class) => {
                self.advance();
                self.class_def_with(0);
                false
            }
            Some(TokenType::Empty) => {
                self.advance();
                false
            }
            Some(TokenType::Type) => {
                let t = self.advance();
                self.error_at(t.start, t.end, Self::TYPE_ALIAS_REFUSED);
                false
            }
            Some(TokenType::Try) => {
                self.try_stmt();
                false
            }
            Some(TokenType::Import) => {
                self.import_stmt();
                false
            }
            Some(TokenType::From) => {
                self.parse_from_stmt();
                false
            }
            Some(TokenType::Global) => {
                self.emit_name_list(OpCode::Global);
                false
            }
            Some(TokenType::Outer) => {
                self.emit_name_list(OpCode::Outer);
                false
            }
            Some(TokenType::Assert) => {
                self.advance();
                self.expr();
                if self.eat_if(TokenType::Comma) {
                    // the runtime's assert class raises and the message evaluates only on failure
                    let to_raise = self.emit_jump(OpCode::JumpIfFalse);
                    let to_end = self.emit_jump(OpCode::Jump);
                    self.patch(to_raise);
                    let call_pos = self.last_end as u32;
                    let idx = self.chunk.push_name(crate::lang::ASSERT_CLASS);
                    self.chunk.emit(OpCode::LoadName, idx);
                    self.expr();
                    self.chunk.emit(OpCode::Call, 1);
                    self.chunk.record_call_pos(call_pos);
                    self.chunk.emit(OpCode::Raise, 0);
                    self.patch(to_end);
                } else {
                    self.chunk.emit(OpCode::Assert, 0);
                }
                false
            }
            Some(TokenType::Delete) => {
                self.advance();
                loop {
                    self.parse_del_target();
                    // a trailing comma ends the list where the statement ends
                    if !self.eat_if(TokenType::Comma) || self.at_del_end() { break; }
                }
                false
            }
            Some(TokenType::Throw) => {
                self.advance();
                // a line end or semicolon ends a bare `raise` and `from` must share its line
                if !matches!(self.peek_same_line(), None | Some(TokenType::Semi | TokenType::Dedent)) {
                    self.expr();
                    if matches!(self.peek_same_line(), Some(TokenType::From)) {
                        let at = self.advance().start;
                        self.discarded(|p| p.expr());
                        self.reject(at, self.last_end, Self::THROW_FROM_REFUSED);
                    }
                    self.chunk.emit(OpCode::Raise, 0);
                } else {
                    // operand 1 tells the VM to raise the active exception again
                    self.chunk.emit(OpCode::Raise, 1);
                }
                false
            }
            Some(TokenType::Break) => {
                self.advance();
                match self.loops.last() {
                    None => self.error(Self::BREAK_OUTSIDE),
                    Some(l) => {
                        let is_for = l.is_for;
                        self.emit_loop_unwind();
                        // a for loop pops its iterator before the break so `iter_stack` stays clean
                        if is_for { self.chunk.emit(OpCode::PopIter, 0); }
                        let j = self.emit_jump(OpCode::Jump);
                        if let Some(l) = self.loops.last_mut() { l.breaks.push(j); }
                    }
                }
                false
            }
            Some(TokenType::Continue) => {
                self.advance();
                if let Some(start) = self.loops.last().map(|l| l.start) {
                    self.emit_loop_unwind();
                    self.chunk.emit(OpCode::Jump, start);
                } else {
                    self.error(Self::CONTINUE_OUTSIDE);
                }
                false
            }
            Some(TokenType::Return) => {
                self.advance();
                if !self.in_function { self.error(Self::RETURN_OUTSIDE); }
                // a bare `return` ends at the line so the next statement is never its value
                if matches!(self.peek_same_line(), None | Some(TokenType::Semi | TokenType::Dedent)) {
                    self.chunk.emit(OpCode::LoadNone, 0);
                } else {
                    self.expr_or_tuple(|s| s.peek_same_line().is_none());
                }
                self.chunk.emit(OpCode::ReturnValue, 0);
                false
            }
            Some(TokenType::Name | TokenType::Underscore) => {
                if self.assignment(false) { return false; }
                let t = self.advance();
                self.name_stmt(t)
            }
            Some(TokenType::Let) if Self::BINDS => {
                self.let_stmt();
                false
            }
            // an indent left by an earlier error skips its whole block silently
            Some(TokenType::Indent) => {
                self.tokens.next();
                let mut depth = 1u32;
                while depth > 0 {
                    match self.tokens.peek().map(|t| t.kind) {
                        Some(TokenType::Indent) => { self.tokens.next(); depth += 1; }
                        Some(TokenType::Dedent) => { self.tokens.next(); depth -= 1; }
                        None | Some(TokenType::Eof) => break,
                        _ => { self.tokens.next(); }
                    }
                }
                false
            }
            // a stray dedent from error recovery is skipped silently
            Some(TokenType::Dedent) => {
                self.tokens.next();
                false
            }
            // a block no header took skips whole since the statement before it already failed
            Some(TokenType::BlockOpen) if Self::DELIMITED => {
                if self.errors.is_empty() { self.diag_at_peek(Self::STRAY_BLOCK); }
                self.tokens.next();
                self.skip_block();
                false
            }
            // a block end with no block open goes the same way
            Some(TokenType::BlockClose) if Self::DELIMITED => {
                if self.errors.is_empty() { self.diag_at_peek(Self::STRAY_CLOSE); }
                self.tokens.next();
                false
            }
            _ => {
                if self.assignment(false) { return false; }
                self.expr();
                if matches!(self.peek_same_line(), Some(TokenType::Comma)) { self.tuple_rest(1, |s| s.peek_same_line().is_none()); }
                self.diag_stray_colon();
                true
            }
        }
    }

    /* runs the cleanup blocks a break or continue crosses before its jump */
    fn emit_loop_unwind(&mut self) {
        let base = self.loops.last().map_or(0, |l| l.cleanup_base);
        let count = self.cleanup_count - base;
        if count > 0 {
            self.chunk.emit(OpCode::UnwindFinally, count as u16);
        }
    }

    /* declares each listed name `global` or `nonlocal` so later loads and stores route accordingly */
    pub(super) fn emit_name_list(&mut self, op: OpCode) {
        self.advance();
        loop {
            if !matches!(self.peek_same_line(), Some(TokenType::Name | TokenType::Underscore)) { return self.error("expected a name"); }
            let name = self.advance_text();
            let idx = self.chunk.push_name(name);
            self.chunk.emit(op, idx);
            match op {
                OpCode::Global => {
                    // a parameter or a name stored earlier in the scope cannot turn global
                    if !self.globals_decl.contains(name) && let Some(v) = self.ssa_versions.get(name) {
                        let how = if v == 0 { Self::PARAM_GLOBAL } else { Self::ASSIGNED_GLOBAL };
                        self.errors.push(Diagnostic { start: self.last_end - name.len(), end: self.last_end, msg: s!("name '", str name, str how) });
                    }
                    self.globals_decl.insert(name.to_string());
                }
                OpCode::Outer => self.declare_nonlocal(name),
                _ => {}
            }
            if !self.eat_if(TokenType::Comma) { break; }
        }
    }

    /* records a nonlocal name an enclosing function must bind by the end of the module */
    fn declare_nonlocal(&mut self, name: &str) {
        let (start, end) = (self.last_end - name.len(), self.last_end);
        let msg = if !self.in_function && !self.in_class_body {
            s!(str Self::OUTER_AT_MODULE)
        } else if self.ssa_versions.get(name) == Some(0) {
            s!("name '", str name, str Self::PARAM_OUTER)
        } else {
            if !self.chunk.nonlocals.iter().any(|n| n == name) { self.chunk.nonlocals.push(name.to_string()); }
            self.unbound_nonlocals.push((name.to_string(), start, end));
            return;
        };
        self.errors.push(Diagnostic { start, end, msg });
    }

    /* an assignment value or tuple running to the line end */
    pub(super) fn rhs_tuple(&mut self) {
        self.expr_or_tuple(|s| s.peek_same_line().is_none());
    }

    /* one expression or a tuple once a comma or leading `*` shows up until `stop` */
    pub(super) fn expr_or_tuple(&mut self, stop: impl Fn(&mut Self) -> bool) {
        if Self::SPLAT && matches!(self.peek(), Some(TokenType::Star)) { return self.tuple_rest(0, stop); }
        self.expr();
        if matches!(self.peek_same_line(), Some(TokenType::Comma)) { self.tuple_rest(1, stop); }
    }

    /* items after the `count` emitted ones up to `stop` with `*it` unpacked into the tuple */
    pub(super) fn tuple_rest(&mut self, mut count: u16, stop: impl Fn(&mut Self) -> bool) {
        let (mut list, mut comma) = (false, false);
        loop {
            if count > 0 || list {
                if !self.eat_if(TokenType::Comma) { break; }
                comma = true;
                if stop(self) { break; }
            }
            if Self::SPLAT && self.eat_if(TokenType::Star) {
                if !list { self.chunk.emit(OpCode::BuildList, count); list = true; }
                self.expr();
                self.chunk.emit(OpCode::ListExtend, 0);
            } else {
                self.expr();
                if list { self.chunk.emit(OpCode::ListAppend, 0); } else { count += 1; }
            }
        }
        if list {
            if !comma { self.error("cannot use starred expression here"); }
            self.chunk.emit(OpCode::ListToTuple, 0);
        } else if comma {
            self.chunk.emit(OpCode::BuildTuple, count);
        }
    }

    // a stray colon after an expression statement suggests the missing keyword
    fn diag_stray_colon(&mut self) {
        if matches!(self.peek(), Some(TokenType::Colon)) {
            let t = self.advance();
            self.error_at(t.start, t.end, Self::STRAY_COLON);
        }
    }

    /* compiles an indented block or the statements on the line of its colon */
    pub(super) fn compile_block(&mut self) {
        if Self::DELIMITED { return self.brace_block(); }
        let next_line = self.peek_same_line().is_none();
        let indented = self.eat_if(TokenType::Indent);
        // a body that starts on the next line must be indented
        if !indented && next_line { return self.error("expected an indented block"); }
        self.block_body(indented);
    }

    /* the statements of an indented block or those on the line of its header */
    pub(super) fn block_body(&mut self, indented: bool) {
        loop {
            while self.eat_if(TokenType::Semi) {}
            if self.at_end() { break; }
            if matches!(self.peek(), Some(TokenType::Dedent)) {
                self.advance();
                self.block_closed = true;
                break;
            }
            self.block_closed = false;
            self.bracket_stack.clear();
            let (errors_before, at) = (self.errors.len(), self.tokens.pos);
            // a compound statement never shares the line of the colon that opens a body
            let produced_value = if !indented && !Self::DELIMITED && self.peek().is_some_and(crate::syntax::lexer::opens_body) { self.error("a compound statement must start its own line"); false } else { self.stmt() };
            self.end_statement(errors_before);
            if self.tokens.pos == at { self.tokens.next(); }
            if produced_value {
                self.chunk.emit(OpCode::PopTop, 0);
            }
            // a body on its header line runs to the line end even past a `return`
            if !indented && !matches!(self.peek(), Some(TokenType::Semi)) { break; }
        }
    }

    // moves past the `}` of the block the cursor sits in
    pub(super) fn skip_block(&mut self) {
        let mut depth = 1u32;
        while depth > 0 {
            match self.tokens.peek().map(|t| t.kind) {
                Some(TokenType::BlockOpen) => { self.tokens.next(); depth += 1; }
                Some(TokenType::BlockClose) => { self.tokens.next(); depth -= 1; }
                None | Some(TokenType::Eof) => break,
                _ => { self.tokens.next(); }
            }
        }
    }

    // the statements between a `{` and its `}` each ending at a newline or semicolon
    fn brace_block(&mut self) {
        if !self.eat_if(TokenType::BlockOpen) {
            self.eat(TokenType::BlockOpen);
            // a header that failed still hands its block the statements inside
            if !self.eat_if(TokenType::BlockOpen) { return; }
        }
        // the caret sits on the last character of the opener whatever its width
        let end = self.last_end;
        let open = self.source.floor_char_boundary(end.saturating_sub(1));
        loop {
            while self.eat_if(TokenType::Semi) {}
            match self.peek() {
                Some(TokenType::BlockClose) => { self.advance(); return; }
                // only the innermost block the source leaves open reports it
                None if !self.errors.iter().any(|e| e.msg == Self::UNCLOSED_BLOCK || e.msg == Self::UNCLOSED_LINE) => {
                    // a block a header's line end opened names the word that closes it
                    let spelled = self.source.as_bytes().get(..end).is_some_and(|s| s.ends_with(Self::b(TokenType::BlockOpen)));
                    return self.reject(open, end, if spelled { Self::UNCLOSED_BLOCK } else { Self::UNCLOSED_LINE })
                }
                None => return,
                _ => {}
            }
            self.bracket_stack.clear();
            let (errors_before, at) = (self.errors.len(), self.tokens.pos);
            let produced_value = self.stmt();
            self.end_statement(errors_before);
            if self.tokens.pos == at { self.tokens.next(); }
            if produced_value { self.chunk.emit(OpCode::PopTop, 0); }
        }
    }

    /* parses an annotation for its errors and drops its code giving whether an assignment follows */
    pub(super) fn skip_annotation(&mut self) -> bool {
        self.pass_annotation(|k| matches!(k, TokenType::Equal | TokenType::Semi | TokenType::Comment | TokenType::Newline | TokenType::Eof | TokenType::BlockClose), false);
        matches!(self.peek_same_line(), Some(TokenType::Equal))
    }

    /* stores the stacked value into the target list at `at` and puts the cursor back */
    pub(super) fn store_back(&mut self, at: Mark, (n, star, comma): Shape) {
        let back = self.mark();
        self.seek(at);
        self.store_list(n, star, comma);
        self.seek(back);
    }

    /* a chained assignment evaluates the value first then stores the targets left to right */
    fn assignment(&mut self, binds: bool) -> bool {
        let Some(&head) = self.tokens.peek() else { return false };
        // a lone name target has nothing to evaluate so it compiles in source order
        if head.assigns <= 1 && matches!(head.kind, TokenType::Name | TokenType::Underscore) && self.tokens.after_head() == TokenType::Equal {
            if Self::REQUIRED && !binds { self.declared(head); }
            let name = self.advance_text();
            self.advance();
            self.rhs_tuple();
            self.store_name(name);
            return true;
        }
        if head.assigns == 0 { return false; }
        let count = head.assigns;
        let start = self.mark();
        // a chain keeps its later targets in `more` so a lone target allocates nothing
        let (mut first, mut more) = (None, Vec::new());
        for _ in 0..count {
            let at = self.tokens.pos;
            let shape = self.tokens.target_list();
            let end = self.tokens.pos;
            if let Some(shape) = shape && matches!(self.peek_same_line(), Some(TokenType::Colon)) {
                if count > 1 || shape != (1, None, false) {
                    self.error("only a single target can be annotated");
                    return true;
                }
                self.advance();
                self.skip_annotation();
            }
            let Some(shape) = shape.filter(|_| matches!(self.peek_same_line(), Some(TokenType::Equal))) else {
                if first.is_none() {
                    self.error("cannot assign to this expression");
                    return true;
                }
                // what follows is the value and the statement end reports the `=` left in it
                self.tokens.pos = at;
                break;
            };
            // a binding word takes names alone and where it is required no name goes unbound
            if Self::BINDS && (binds || Self::REQUIRED) { self.check_targets(at, end, binds); }
            let target = (self.tokens.pos, shape);
            if first.is_none() { first = Some(target); } else { more.push(target); }
            self.advance();
        }
        let Some(first) = first else { return false };
        self.rhs_tuple();
        let end = self.mark();
        self.seek(start);
        let last = more.len();
        for (i, (eq, (n, star, comma))) in core::iter::once(first).chain(more).enumerate() {
            if i < last { self.chunk.emit(OpCode::Dup, 0); }
            self.store_list(n, star, comma);
            self.tokens.pos = eq + 1;
        }
        self.seek(end);
        true
    }

    /* a binding word then the assignment of the names it binds */
    fn let_stmt(&mut self) {
        let word = self.advance();
        // the lexer counted the targets' `=` on the word so the first target takes them
        if self.peek_same_line().is_some() && let Some(t) = self.tokens.toks.get_mut(self.tokens.pos) { t.assigns = word.assigns; }
        if word.assigns > 0 {
            self.assignment(true);
            return;
        }
        // a binding with no value reports where its `=` belongs
        if self.tokens.target_list().is_some() { return self.eat(TokenType::Equal); }
        let msg = s!(str Self::shown(TokenType::Let), " binds names only");
        self.error(&msg);
    }

    /* refuses targets after a binding word that are no names and stores to unbound names */
    fn check_targets(&mut self, from: usize, to: usize, binds: bool) {
        let (names, whole) = self.bound_names(from, to);
        if binds && !whole {
            let (start, end) = (self.tokens.toks[from].start, self.tokens.toks[to.saturating_sub(1).max(from)].end);
            let msg = s!(str Self::shown(TokenType::Let), " binds names only");
            return self.reject(start, end, &msg);
        }
        if !binds { for t in names { self.declared(t); } }
    }

    /* the names the targets from `from` to `to` bind whole and whether only names stand */
    fn bound_names(&self, from: usize, to: usize) -> (Vec<Token>, bool) {
        let toks: Vec<Token> = self.tokens.toks[from..to].iter().copied().filter(|t| !matches!(t.kind, TokenType::LineBreak | TokenType::Comment)).collect();
        let (mut names, mut whole, mut inside) = (Vec::new(), true, 0usize);
        for (i, t) in toks.iter().enumerate() {
            let prev = if i == 0 { TokenType::Comma } else { toks[i - 1].kind };
            let next = toks.get(i + 1).map_or(TokenType::Equal, |n| n.kind);
            // brackets right after an operand hold a call or an index whose names bind nothing
            let trailer = matches!(prev, TokenType::Name | TokenType::Underscore | TokenType::ParenClose | TokenType::BracketClose);
            match t.kind {
                TokenType::ParenOpen | TokenType::BracketOpen if inside > 0 || trailer => { inside += 1; whole = false; }
                TokenType::ParenClose | TokenType::BracketClose if inside > 0 => inside -= 1,
                _ if inside > 0 => {}
                TokenType::Name | TokenType::Underscore if prev != TokenType::Dot && !matches!(next, TokenType::Dot | TokenType::ParenOpen | TokenType::BracketOpen) => names.push(*t),
                TokenType::Comma | TokenType::Star | TokenType::ParenOpen | TokenType::BracketOpen | TokenType::ParenClose | TokenType::BracketClose => {}
                _ => whole = false,
            }
        }
        (names, whole)
    }

    /* reports a store to an unbound name where the table requires its binding word */
    fn declared(&mut self, t: Token) {
        let name = self.lexeme(&t);
        if t.kind == TokenType::Underscore || self.ssa_versions.contains(name) || self.globals_decl.contains(name) || self.chunk.nonlocals.iter().any(|n| n == name) { return; }
        let msg = s!("name '", str name, "' is not bound, bind it with ", str Self::shown(TokenType::Let));
        self.reject(t.start, t.end, &msg);
    }

    /* stores the stacked value into `n` targets at the cursor unpacking it first when `unpack` */
    fn store_list(&mut self, n: usize, star: Option<usize>, unpack: bool) {
        // a table without the star markers gathers no rest into a target
        if !Self::SPLAT && star.is_some() { return self.diag_at_peek("cannot assign to this expression"); }
        match star {
            Some(i) if unpack => self.chunk.emit(OpCode::UnpackEx, ((i as u16) << 8) | (n - i - 1) as u16),
            Some(_) => self.diag_at_peek("starred assignment target must be in a list or tuple"),
            None if unpack => self.chunk.emit(OpCode::UnpackSequence, n as u16),
            None => {}
        }
        for _ in 0..n {
            self.store_target();
            self.eat_if(TokenType::Comma);
        }
    }

    /* stores the stacked value into the name or `a.b[i]` chain or group at the cursor */
    pub(super) fn store_target(&mut self) {
        self.eat_if(TokenType::Star);
        self.peek_same_line();
        let open = self.tokens.pos;
        let t = self.advance();
        match t.kind {
            TokenType::Name | TokenType::Underscore if self.at_target_end() => {
                let name = self.lexeme(&t);
                return self.store_name(name);
            }
            TokenType::Name | TokenType::Underscore => self.name(t),
            _ => match self.tokens.group_target(open) {
                (close, true) => return self.nested(|s| s.store_group(t.kind, close)),
                _ if t.kind == TokenType::ParenOpen => self.paren_atom(),
                _ => self.list_literal(),
            },
        }
        if !self.postfix_tail(Tail::Target) {
            self.reject(t.start, self.last_end, "cannot assign to this expression");
        }
    }

    /* a target list closing at `close` where `[a]` and `()` unpack but `(a)` stays bare */
    fn store_group(&mut self, open: TokenType, close: usize) {
        let at = self.tokens.pos;
        let shape = self.tokens.target_list();
        self.tokens.pos = at;
        match shape {
            Some((1, None, false)) if open == TokenType::ParenOpen => self.store_target(),
            Some((n, star, comma)) => self.store_list(n, star, comma || n == 0 || open == TokenType::BracketOpen),
            None => self.diag_at_peek("cannot assign to this expression"),
        }
        self.tokens.pos = close;
        self.eat(if open == TokenType::ParenOpen { TokenType::ParenClose } else { TokenType::BracketClose });
    }

    /* a name-led statement other than an assignment */
    pub(super) fn name_stmt(&mut self, t: Token) -> bool {
        let name = self.lexeme(&t);
        let start = self.chunk.instructions.len();
        self.saw_newline = false;

        // `x: T` annotates while a colon on the next line belongs to another statement
        if matches!(self.peek_same_line(), Some(TokenType::Colon)) {
            self.advance();
            self.skip_annotation();
            return false;
        }

        match self.peek_same_line() {
            Some(t) if Self::augmented_op(&t).is_some() => {
                let op = Self::augmented_op(&t).unwrap();
                self.advance();
                self.emit_load_ssa(name);
                self.expr();
                self.emit_inplace(op);
                self.store_name(name);
                false
            }
            Some(TokenType::BracketOpen | TokenType::Dot) => {
                self.emit_load_ssa(name);
                self.stmt_tails(start)
            }
            Some(TokenType::Comma) => {
                self.emit_load_ssa(name);
                self.tuple_rest(1, |s| s.peek_same_line().is_none());
                true
            }
            // a call at statement level may continue with trailers like `f().g(x)`
            Some(TokenType::ParenOpen) => {
                // a native call that ends its statement keeps no value and the others leave theirs
                if self.call(name, true) { return false; }
                match self.peek_same_line() {
                    None | Some(TokenType::Semi | TokenType::Dedent) => true,
                    _ => self.stmt_tails(start),
                }
            }
            _ => {
                self.emit_load_ssa(name);
                self.expr_tails(start);
                self.diag_stray_colon();
                true
            }
        }
    }

    /* trailers and operators after a statement head and true when a value is left */
    fn stmt_tails(&mut self, start: usize) -> bool {
        if self.postfix_tail(Tail::Stmt) { return false; }
        self.infix_bp(0);
        self.ternary_tail(start);
        if matches!(self.peek_same_line(), Some(TokenType::Comma)) { self.tuple_rest(1, |s| s.peek_same_line().is_none()); }
        true
    }

    /* `op=` in its in-place form so list `+=` and set operators mutate the shared object */
    pub(super) fn emit_inplace(&mut self, op: OpCode) {
        let op = match op {
            OpCode::Add => OpCode::InPlaceAdd,
            OpCode::Sub => OpCode::InPlaceSub,
            OpCode::BitOr => OpCode::InPlaceBitOr,
            OpCode::BitAnd => OpCode::InPlaceBitAnd,
            OpCode::BitXor => OpCode::InPlaceBitXor,
            other => return self.chunk.emit(other, super::INPLACE),
        };
        self.chunk.emit(op, 0);
    }

    pub(super) fn augmented_op(tok: &TokenType) -> Option<OpCode> {
        match tok {
            TokenType::PlusEqual => Some(OpCode::Add),
            TokenType::MinusEqual => Some(OpCode::Sub),
            TokenType::StarEqual => Some(OpCode::Mul),
            TokenType::SlashEqual => Some(OpCode::Div),
            TokenType::FloorDivEqual => Some(OpCode::FloorDiv),
            TokenType::AtEqual => Some(OpCode::MatMul),
            TokenType::PercentEqual => Some(OpCode::Mod),
            TokenType::PowEqual => Some(OpCode::Pow),
            TokenType::BitAndEqual => Some(OpCode::BitAnd),
            TokenType::BitOrEqual => Some(OpCode::BitOr),
            TokenType::BitXorEqual => Some(OpCode::BitXor),
            TokenType::LeftShiftEqual => Some(OpCode::Shl),
            TokenType::RightShiftEqual => Some(OpCode::Shr),
            _ => None,
        }
    }

    /* one `del` target as a name or an access chain or a group of targets */
    fn parse_del_target(&mut self) {
        self.peek_same_line();
        let open = self.tokens.pos;
        let Some(&t) = self.tokens.peek() else { return self.error("cannot delete this expression") };
        match t.kind {
            TokenType::Name | TokenType::Underscore => {
                self.advance();
                if self.at_del_end() {
                    let name = self.lexeme(&t);
                    // a bare name unbinds its current version or its module binding when declared `global`
                    let idx = if self.globals_decl.contains(name) { self.chunk.push_name(name) } else { self.push_ssa_name(name, self.current_version(name)) };
                    return self.chunk.emit(OpCode::Del, idx);
                }
                self.name(t);
            }
            TokenType::ParenOpen | TokenType::BracketOpen => {
                self.advance();
                let (close, _) = self.tokens.group_target(open);
                if self.del_ends_after(close) { return self.nested(|s| s.del_group(t.kind, close)); }
                if t.kind == TokenType::ParenOpen { self.paren_atom(); } else { self.list_literal(); }
            }
            TokenType::String | TokenType::FormatStart | TokenType::Bytes | TokenType::Int | TokenType::Float | TokenType::True
            | TokenType::False | TokenType::Nil | TokenType::Ellipsis | TokenType::BraceOpen => {
                // a literal's chain compiles as an operand whose last access becomes its delete
                self.parse_atom();
                let del = match self.chunk.instructions.last().map(|i| i.opcode) {
                    Some(OpCode::GetItem) => Some(OpCode::DelItem),
                    Some(OpCode::Cut) => Some(OpCode::DelCut),
                    Some(OpCode::LoadAttr) => Some(OpCode::DelAttr),
                    _ => None,
                };
                match del.filter(|_| self.at_del_end()) {
                    Some(op) => if let Some(ins) = self.chunk.instructions.last_mut() { ins.opcode = op; },
                    None => self.error_at(t.start, self.last_end, "cannot delete this expression"),
                }
                return;
            }
            _ => return self.error("cannot delete this expression"),
        }
        // only an attribute or a subscript ends a chain a delete reaches
        if !self.del_tail() { self.error_at(t.start, self.last_end, "cannot delete this expression"); }
    }

    /* the trailers of a `del` chain whose last attribute or subscript turns into its delete */
    fn del_tail(&mut self) -> bool {
        loop {
            match self.peek_same_line() {
                Some(TokenType::BracketOpen) => {
                    self.advance();
                    let cut = self.parse_subscript();
                    if self.at_del_end() { self.emit_del_item(cut); return true; }
                    self.emit_get_item(cut);
                }
                Some(TokenType::Dot) => {
                    self.advance();
                    if !matches!(self.peek_same_line(), Some(TokenType::Name | TokenType::Underscore)) { self.diag_at_peek(Self::ATTR_AFTER_DOT); return false; }
                    let t = self.advance();
                    let name = self.lexeme(&t);
                    // a table that reads keys at dots deletes a key unless a call follows
                    if Self::KEYS && !matches!(self.peek_same_line(), Some(TokenType::ParenOpen)) {
                        self.emit_const(crate::syntax::chunk::Value::Str(name.to_string()));
                        if self.at_del_end() { self.chunk.emit(OpCode::DelItem, 0); return true; }
                        self.chunk.emit(OpCode::GetItem, 0);
                        continue;
                    }
                    let idx = self.chunk.push_name(name);
                    if self.at_del_end() { self.chunk.emit(OpCode::DelAttr, idx); return true; }
                    self.chunk.emit(OpCode::LoadAttr, idx);
                }
                Some(TokenType::ParenOpen) => {
                    let call_pos = self.last_end as u32;
                    self.advance();
                    self.call_rest(call_pos);
                }
                _ => return false,
            }
        }
    }

    /* deletes each target of a group closing at `close` where an empty one deletes nothing */
    fn del_group(&mut self, open: TokenType, close: usize) {
        let shut = if open == TokenType::ParenOpen { TokenType::ParenClose } else { TokenType::BracketClose };
        while self.tokens.pos < close && !matches!(self.peek(), Some(k) if k == shut) {
            self.parse_del_target();
            if !self.eat_if(TokenType::Comma) { break; }
        }
        self.tokens.pos = close;
        self.eat(shut);
    }

    /* whether a `del` target ends at the cursor */
    pub(super) fn at_del_end(&mut self) -> bool {
        matches!(self.peek_same_line(), None | Some(TokenType::Semi | TokenType::Comma | TokenType::ParenClose | TokenType::BracketClose))
    }

    /* whether a `del` target ends right after the group closing at `close` */
    fn del_ends_after(&self, close: usize) -> bool {
        let next = self.tokens.toks.get(close + 1..).and_then(|rest| rest.iter().find(|t| !matches!(t.kind, TokenType::LineBreak | TokenType::Comment)));
        next.is_none_or(|t| matches!(t.kind, TokenType::Newline | TokenType::Eof | TokenType::Dedent | TokenType::Semi | TokenType::Comma | TokenType::ParenClose | TokenType::BracketClose)
            || (Self::DELIMITED && t.kind == TokenType::BlockClose))
    }

    /* a yield past its keyword leaves a value so it serves as statement or expression */
    pub(super) fn emit_yield(&mut self) {
        // a comprehension is a scope of its own with no frame a `yield` could suspend
        if self.in_comp { return self.error(Self::YIELD_IN_COMP); }
        if !self.in_function { return self.error(Self::YIELD_OUTSIDE); }
        if self.in_async { return self.error(Self::ASYNC_GEN_REFUSED); }
        let bare = matches!(
            self.peek_same_line(),
            None | Some(TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose
                | TokenType::Comma | TokenType::Colon)
        );
        if bare {
            self.chunk.emit(OpCode::LoadNone, 0);
            self.chunk.emit(OpCode::Yield, 0);
        } else if self.eat_if(TokenType::From) {
            self.expr();
            self.chunk.emit(OpCode::GetIter, 0);
            let loop_start = self.chunk.instructions.len() as u16;
            let fi = self.emit_jump(OpCode::ForIter);
            // the marked yield lets `send` and `throw` and `close` reach the subiterator first
            self.chunk.emit(OpCode::Yield, super::YIELD_FROM);
            self.chunk.emit(OpCode::PopTop, 0);
            self.chunk.emit(OpCode::Jump, loop_start);
            self.patch(fi);
            self.chunk.emit(OpCode::LoadYieldFrom, 0);
        } else {
            // several values yield one tuple that a closer or the line end stops
            self.expr_or_tuple(|s| matches!(s.peek_same_line(), None | Some(TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose)));
            self.chunk.emit(OpCode::Yield, 0);
        }
    }

    /* refuses the `await` spanning `start..end` outside an `async def` */
    pub(super) fn check_await(&mut self, start: usize, end: usize) {
        if self.in_async { return; }
        let msg = if self.in_comp { Self::AWAIT_IN_COMP } else if self.in_function { Self::AWAIT_OUTSIDE_ASYNC } else { Self::AWAIT_OUTSIDE };
        self.reject(start, end, msg);
    }

    /* awaits the stacked value by stepping its awaitable in place until it returns */
    pub(super) fn emit_await(&mut self) {
        self.chunk.emit(OpCode::GetAwaitable, 0);
        self.chunk.emit(OpCode::Await, 0);
    }

}
