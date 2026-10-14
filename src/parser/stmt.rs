use super::{Mark, Parser, Shape};
use super::expr::Tail;
use super::types::OpCode;

use crate::lexer::{Token, TokenType};

use alloc::{string::ToString, vec::Vec};

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {

    /* Records ip->source offset before any emit, so `resolve()` maps what follows to the next token. */
    pub(super) fn mark_stmt(&mut self) {
        let ip = self.chunk.instructions.len() as u32;
        let pos = self.tokens.peek().map(|t| t.start as u32).unwrap_or(self.last_end as u32);
        self.chunk.stmt_pos.push((ip, pos));
    }

    /* Statement dispatch, returns true if a value is left on the stack for caller to PopTop. */
    pub(super) fn stmt(&mut self) -> bool {
        self.mark_stmt();

        match self.peek() {
            Some(TokenType::If) => {
                self.if_stmt();
                false
            }
            Some(TokenType::For) => {
                self.for_stmt_inner(false);
                false
            }
            Some(TokenType::Def) => {
                self.advance();
                self.func_def_inner(0, false);
                false
            }
            Some(TokenType::With) => {
                self.with_stmt_inner(false);
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
                self.advance();
                match self.peek() {
                    Some(TokenType::Def) => {
                        self.advance();
                        self.func_def_inner(0, true);
                    }
                    Some(TokenType::For) => { self.for_stmt_inner(true); }
                    Some(TokenType::With) => { self.with_stmt_inner(true); }
                    _ => {}
                }
                false
            }
            Some(TokenType::Await) => {
                self.advance();
                self.expr();
                self.chunk.emit(OpCode::Await, 0);
                true
            }
            Some(TokenType::At) => {
                let mut count = 0u16;
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
            Some(TokenType::Pass) => {
                self.advance();
                false
            }
            // `type X = v` binds a lazy alias, `v` runs on `X.__value__`.
            Some(TokenType::Type) => {
                self.advance();
                let name = self.advance_text();
                self.type_params();
                self.eat(TokenType::Equal);
                let body = self.expr_body(&[]);
                self.push_function(Vec::new(), body, 0, None, OpCode::MakeFunction);
                let idx = self.chunk.push_name(name);
                self.chunk.emit(OpCode::MakeTypeAlias, idx);
                self.store_name(name);
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
            Some(TokenType::Nonlocal) => {
                self.emit_name_list(OpCode::Nonlocal);
                false
            }
            Some(TokenType::Assert) => {
                self.advance();
                self.expr();
                if self.eat_if(TokenType::Comma) {
                    // `assert cond, msg` desugars to lazy `if not cond: raise AssertionError(msg)`.
                    let to_raise = self.emit_jump(OpCode::JumpIfFalse);
                    let to_end = self.emit_jump(OpCode::Jump);
                    self.patch(to_raise);
                    let call_pos = self.last_end as u32;
                    let idx = self.chunk.push_name("AssertionError");
                    self.chunk.emit(OpCode::LoadName, idx);
                    self.expr(); // message, only evaluated when the assertion fails
                    self.chunk.emit(OpCode::Call, 1);
                    self.chunk.record_call_pos(call_pos);
                    self.chunk.emit(OpCode::Raise, 0);
                    self.patch(to_end);
                } else {
                    self.chunk.emit(OpCode::Assert, 0);
                }
                false
            }
            Some(TokenType::Del) => {
                self.advance();
                loop {
                    self.parse_del_target();
                    if !self.eat_if(TokenType::Comma) { break; }
                }
                false
            }
            Some(TokenType::Raise) => {
                self.advance();
                // A line end or semicolon ends a bare `raise`, and `from` must share its line.
                if !matches!(self.peek_same_line(), None | Some(TokenType::Semi | TokenType::Dedent)) {
                    self.expr();
                    if matches!(self.peek_same_line(), Some(TokenType::From)) {
                        self.advance();
                        self.expr();
                        self.chunk.emit(OpCode::RaiseFrom, 0);
                    } else {
                        self.chunk.emit(OpCode::Raise, 0);
                    }
                } else {
                    // Bare `raise`, operand 1 tells the VM to re-raise the active exception.
                    self.chunk.emit(OpCode::Raise, 1);
                }
                false
            }
            Some(TokenType::Break) => {
                self.advance();
                match self.loops.last() {
                    None => self.error("'break' outside loop"),
                    Some(l) => {
                        let is_for = l.is_for;
                        self.emit_loop_unwind();
                        // For-loop, PopIter before break so `iter_stack` stays clean.
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
                    self.error("'continue' outside loop");
                }
                false
            }
            Some(TokenType::Return) => {
                self.advance();
                if !self.in_function { self.error("'return' outside function"); }
                // A bare `return` ends at the line, the next statement is never its value.
                if matches!(self.peek_same_line(), None | Some(TokenType::Semi | TokenType::Dedent)) {
                    self.chunk.emit(OpCode::LoadNone, 0);
                } else {
                    self.expr_or_tuple(|s| s.peek_same_line().is_none());
                }
                self.chunk.emit(OpCode::ReturnValue, 0);
                false
            }
            Some(TokenType::Name | TokenType::Underscore) => {
                if self.assignment() { return false; }
                let t = self.advance();
                self.name_stmt(t)
            }
            // Dangling Indent from a prior error, skip the entire block silently.
            Some(TokenType::Indent) => {
                self.tokens.next();
                let mut depth = 1u32;
                while depth > 0 {
                    match self.tokens.peek().map(|t| t.kind) {
                        Some(TokenType::Indent) => { self.tokens.next(); depth += 1; }
                        Some(TokenType::Dedent) => { self.tokens.next(); depth -= 1; }
                        None | Some(TokenType::Endmarker) => break,
                        _ => { self.tokens.next(); }
                    }
                }
                false
            }
            // Stray Dedent from error recovery, skip silently.
            Some(TokenType::Dedent) => {
                self.tokens.next();
                false
            }
            _ => {
                if self.assignment() { return false; }
                self.expr();
                if matches!(self.peek_same_line(), Some(TokenType::Comma)) { self.tuple_rest(1, |s| s.peek_same_line().is_none()); }
                self.diag_stray_colon();
                true
            }
        }
    }

    /* Runs the finally/with blocks a break/continue crosses before its jump. */
    fn emit_loop_unwind(&mut self) {
        let base = self.loops.last().map_or(0, |l| l.cleanup_base);
        let count = self.cleanup_count - base;
        if count > 0 {
            self.chunk.emit(OpCode::UnwindFinally, count as u16);
        }
    }

    /* Emits Global/Nonlocal opcodes for a comma-separated name list. Global registers each name so loads/stores route through LoadGlobal/StoreGlobal, Nonlocal records it in `chunk.nonlocals`. */
    pub(super) fn emit_name_list(&mut self, op: OpCode) {
        self.advance();
        loop {
            let name = self.advance_text();
            let idx = self.chunk.push_name(name);
            self.chunk.emit(op, idx);
            match op {
                OpCode::Global => { self.globals_decl.insert(name.to_string()); }
                OpCode::Nonlocal if !self.chunk.nonlocals.iter().any(|n| n == name) => self.chunk.nonlocals.push(name.to_string()),
                _ => {}
            }
            if !self.eat_if(TokenType::Comma) { break; }
        }
    }

    /* Assignment RHS, one expression or a comma tuple, ends at the line boundary. */
    pub(super) fn rhs_tuple(&mut self) {
        self.expr_or_tuple(|s| s.peek_same_line().is_none());
    }

    /* One expression, or a tuple once a comma or a leading `*` shows up. Ends at `stop`. */
    pub(super) fn expr_or_tuple(&mut self, stop: impl Fn(&mut Self) -> bool) {
        if matches!(self.peek(), Some(TokenType::Star)) { return self.tuple_rest(0, stop); }
        self.expr();
        if matches!(self.peek_same_line(), Some(TokenType::Comma)) { self.tuple_rest(1, stop); }
    }

    /* Elements after the `count` emitted ones, `*it` unpacks, and a comma builds the tuple. Ends at `stop`. */
    pub(super) fn tuple_rest(&mut self, mut count: u16, stop: impl Fn(&mut Self) -> bool) {
        let (mut list, mut comma) = (false, false);
        loop {
            if count > 0 || list {
                if !self.eat_if(TokenType::Comma) { break; }
                comma = true;
                if stop(self) { break; }
            }
            if self.eat_if(TokenType::Star) {
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
            self.chunk.emit(OpCode::CallTuple, 1);
        } else if comma {
            self.chunk.emit(OpCode::BuildTuple, count);
        }
    }

    // `expr:` at statement level, suggest the missing keyword.
    fn diag_stray_colon(&mut self) {
        if matches!(self.peek(), Some(TokenType::Colon)) {
            let t = self.advance();
            self.error_at(
                t.start, t.end,
                "unexpected ':' (missing 'if', 'while', 'for', or other statement keyword?)",
            );
        }
    }

    pub(super) fn compile_block(&mut self) { self.compile_block_inner(false); }
    pub(super) fn compile_block_body(&mut self) { self.compile_block_inner(true); }

    /* Compiles Indent/Dedent block, is_body=true stops after ReturnValue to skip dead code. */
    fn compile_block_inner(&mut self, is_body: bool) {
        let next_line = self.peek_same_line().is_none();
        let indented = self.eat_if(TokenType::Indent);
        // A body that starts on the next line must be indented.
        if !indented && next_line { return self.error("expected an indented block"); }
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
            // A compound statement never shares the line of the colon that opens a body.
            let produced_value = if !indented && self.peek().is_some_and(crate::lexer::opens_body) { self.error("a compound statement must start its own line"); false } else { self.stmt() };
            self.end_statement(errors_before);
            if self.tokens.pos == at { self.tokens.next(); }
            if produced_value {
                self.chunk.emit(OpCode::PopTop, 0);
            }
            if indented { continue; }
            if is_body {
                let just_returned = self.chunk.instructions.last().is_some_and(|i| i.opcode == OpCode::ReturnValue);
                if just_returned || !matches!(self.peek(), Some(TokenType::Semi)) { break; }
            } else if !matches!(self.peek(), Some(TokenType::Semi)) { break; }
        }
    }

    /* Annotation discards tokens up to `=`, Edge Python is dynamically typed. Returns true when an assignment follows. */
    pub(super) fn skip_annotation(&mut self) -> bool {
        while !matches!(self.peek_same_line(), Some(TokenType::Equal) | None) {
            self.advance();
        }
        matches!(self.peek_same_line(), Some(TokenType::Equal))
    }

    /* Stores the value on the stack into the target list at `at`, the cursor back where it stood. */
    pub(super) fn store_back(&mut self, at: Mark, (n, star, comma): Shape) {
        let back = self.mark();
        self.seek(at);
        self.store_list(n, star, comma);
        self.seek(back);
    }

    /* `t1 = t2 = value` in Python order, the value first, then targets left to right. */
    fn assignment(&mut self) -> bool {
        let Some(&head) = self.tokens.peek() else { return false };
        // A lone name stores nothing it must evaluate, so it compiles in source order.
        if head.assigns <= 1 && matches!(head.kind, TokenType::Name | TokenType::Underscore) && self.tokens.after_head() == TokenType::Equal {
            let name = self.advance_text();
            self.check_member(name, head.end);
            self.advance();
            self.rhs_tuple();
            self.store_name(name);
            return true;
        }
        if head.assigns == 0 { return false; }
        let count = head.assigns;
        let start = self.mark();
        // A chain keeps its later targets in `more`, so a lone target allocates nothing.
        let (mut first, mut more) = (None, Vec::new());
        for _ in 0..count {
            let at = self.tokens.pos;
            let shape = self.tokens.target_list();
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
                // What follows is the value, and the statement end reports the `=` left in it.
                self.tokens.pos = at;
                break;
            };
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

    /* Stores the stacked value into `n` targets from the cursor, unpacked first when `unpack`. */
    fn store_list(&mut self, n: usize, star: Option<usize>, unpack: bool) {
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

    /* Stores the stacked value into the target at the cursor, a name, `a.b[i]` chain or group. */
    fn store_target(&mut self) {
        self.eat_if(TokenType::Star);
        self.peek_same_line();
        let open = self.tokens.pos;
        let t = self.advance();
        match t.kind {
            TokenType::Name | TokenType::Underscore if self.at_target_end() => {
                let name = self.lexeme(&t);
                self.check_member(name, t.end);
                return self.store_name(name);
            }
            TokenType::Name | TokenType::Underscore => self.name(t),
            _ => match self.tokens.group_target(open) {
                (close, true) => return self.nested(|s| s.store_group(t.kind, close)),
                _ if t.kind == TokenType::Lpar => self.paren_atom(),
                _ => self.list_literal(),
            },
        }
        if !self.postfix_tail(Tail::Target) {
            self.reject(t.start, self.last_end, "cannot assign to this expression");
        }
    }

    /* A bracketed target list closing at `close`, `[a]` and `()` unpack, `(a)` stays bare. */
    fn store_group(&mut self, open: TokenType, close: usize) {
        let at = self.tokens.pos;
        let shape = self.tokens.target_list();
        self.tokens.pos = at;
        match shape {
            Some((1, None, false)) if open == TokenType::Lpar => self.store_target(),
            Some((n, star, comma)) => self.store_list(n, star, comma || n == 0 || open == TokenType::Lsqb),
            None => self.diag_at_peek("cannot assign to this expression"),
        }
        self.tokens.pos = close;
        self.eat(if open == TokenType::Lpar { TokenType::Rpar } else { TokenType::Rsqb });
    }

    /* Name-led statement besides assignment, a bare annotation, augmented op, trailer, call or tuple. */
    pub(super) fn name_stmt(&mut self, t: Token) -> bool {
        let name = self.lexeme(&t);
        let start = self.chunk.instructions.len();
        self.saw_newline = false;

        // `x: T` annotates, a colon on the next line belongs to another statement.
        if matches!(self.peek_same_line(), Some(TokenType::Colon)) {
            self.check_member(name, t.end);
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
            Some(TokenType::Lsqb | TokenType::Dot) => {
                self.emit_load_ssa(name);
                self.stmt_tails(start)
            }
            Some(TokenType::Comma) => {
                self.emit_load_ssa(name);
                self.tuple_rest(1, |s| s.peek_same_line().is_none());
                true
            }
            // `name(...)` at statement level, allow postfix chains like `super().__init__(x)`.
            Some(TokenType::Lpar) => self.call(name) && self.stmt_tails(start),
            _ => {
                self.emit_load_ssa(name);
                self.expr_tails(start);
                self.diag_stray_colon();
                true
            }
        }
    }

    /* Trailers and operators after a statement head, true when a value is left on the stack. */
    fn stmt_tails(&mut self, start: usize) -> bool {
        if self.postfix_tail(Tail::Stmt) { return false; }
        self.infix_bp(0, start);
        self.ternary_tail(start);
        if matches!(self.peek_same_line(), Some(TokenType::Comma)) { self.tuple_rest(1, |s| s.peek_same_line().is_none()); }
        true
    }

    /* `op=` in its in-place form, so list `+=` and set `|=`/`&=`/`^=`/`-=` mutate the shared object and `__iop__` answers first. */
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
            TokenType::MinEqual => Some(OpCode::Sub),
            TokenType::StarEqual => Some(OpCode::Mul),
            TokenType::SlashEqual => Some(OpCode::Div),
            TokenType::DoubleSlashEqual => Some(OpCode::FloorDiv),
            TokenType::AtEqual => Some(OpCode::MatMul),
            TokenType::PercentEqual => Some(OpCode::Mod),
            TokenType::DoubleStarEqual => Some(OpCode::Pow),
            TokenType::AmperEqual => Some(OpCode::BitAnd),
            TokenType::VbarEqual => Some(OpCode::BitOr),
            TokenType::CircumflexEqual => Some(OpCode::BitXor),
            TokenType::LeftShiftEqual => Some(OpCode::Shl),
            TokenType::RightShiftEqual => Some(OpCode::Shr),
            _ => None,
        }
    }

    /* Parses one `del` target, name, subscript, attribute, or a parenthesized group. */
    fn parse_del_target(&mut self) {
        // A bare name unbinds its current version.
        if matches!(self.peek(), Some(TokenType::Name)) {
            let t = self.advance();
            if !matches!(self.peek_same_line(), Some(TokenType::Dot | TokenType::Lsqb | TokenType::Lpar)) {
                let name = self.lexeme(&t);
                let idx = self.push_ssa_name(name, self.current_version(name));
                self.chunk.emit(OpCode::Del, idx);
                return;
            }
            self.name_operand(t);
        } else {
            self.expr();
        }
        // Parsed as an expression, the trailing access becomes its delete form.
        match self.chunk.instructions.last().map(|i| i.opcode) {
            Some(OpCode::GetItem) => self.chunk.instructions.last_mut().unwrap().opcode = OpCode::DelItem,
            Some(OpCode::LoadAttr) => self.chunk.instructions.last_mut().unwrap().opcode = OpCode::DelAttr,
            Some(OpCode::LoadName) => self.chunk.instructions.last_mut().unwrap().opcode = OpCode::Del,
            // `del (a, b)` / `del [a, b]`, a target group unbinds each plain name.
            Some(OpCode::BuildTuple | OpCode::BuildList) => self.del_group_targets(),
            _ => self.error("cannot delete this expression"),
        }
    }

    /* Rewrites a just-built tuple/list of name loads into individual unbinds. */
    fn del_group_targets(&mut self) {
        let last = self.chunk.instructions.len() - 1;
        let n = self.chunk.instructions[last].operand as usize;
        if n >= 1 && last >= n
            && self.chunk.instructions[last - n..last].iter().all(|i| i.opcode == OpCode::LoadName) {
            self.chunk.instructions.truncate(last);
            for ins in &mut self.chunk.instructions[last - n..] { ins.opcode = OpCode::Del; }
        }
    }

    /* Emit `yield` / `yield from` / bare `yield`, leaving the produced value on the stack so it works in both statement and expression position. Assumes the `yield` keyword was already consumed. */
    pub(super) fn emit_yield(&mut self) {
        // A comprehension runs as a scope of its own, which has no frame a `yield` could suspend.
        if self.in_comp { return self.error("'yield' inside a comprehension"); }
        if !self.in_function { return self.error("'yield' outside function"); }
        // No value when a line boundary or a closing token follows (`(yield)`, `f(yield)`).
        let bare = matches!(
            self.peek_same_line(),
            None | Some(TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace
                | TokenType::Comma | TokenType::Colon)
        );
        if bare {
            self.chunk.emit(OpCode::LoadNone, 0);
            self.chunk.emit(OpCode::Yield, 0);
        } else if self.eat_if(TokenType::From) {
            // `yield from`, GetIter+ForIter+Yield loop, LoadYieldFrom pushes the subiterator's return value.
            self.expr();
            self.chunk.emit(OpCode::GetIter, 0);
            let loop_start = self.chunk.instructions.len() as u16;
            let fi = self.emit_jump(OpCode::ForIter);
            self.chunk.emit(OpCode::Yield, 0);
            self.chunk.emit(OpCode::PopTop, 0);
            self.chunk.emit(OpCode::Jump, loop_start);
            self.patch(fi);
            self.chunk.emit(OpCode::LoadYieldFrom, 0);
        } else {
            // `yield a, b` yields one tuple, a closer or the line end stops it.
            self.expr_or_tuple(|s| matches!(s.peek_same_line(), None | Some(TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace)));
            self.chunk.emit(OpCode::Yield, 0);
        }
    }

}
