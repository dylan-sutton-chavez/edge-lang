use super::{Mark, Parser};
use crate::code::OpCode;
use crate::syntax::chunk::Value;
use crate::syntax::lexer::grammar::{Emit, Form, Grammar, Row, Slot, Test};
use crate::syntax::lexer::{Spelling, Token, TokenType};

use alloc::vec::Vec;

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    pub(super) const GRAMMAR: Option<&'static Grammar> = S::SYNTAX.shapes.grammar;

    // only reached once a table brings a grammar
    fn g() -> &'static Grammar { Self::GRAMMAR.unwrap_or(&Grammar::NONE) }

    fn row(&self, tok: TokenType, prefix: bool) -> Option<Row> {
        Self::g().modes.get(self.mode as usize)?.rows.iter().find(|r| r.tok == tok && (r.left == 0) == prefix).copied()
    }

    /* the text of the next token whatever kind the table lexed it as */
    fn word(&mut self) -> Option<&'src str> {
        self.peek()?;
        let t = *self.tokens.peek()?;
        Some(self.lexeme(&t))
    }

    fn eat_word(&mut self, w: &str) -> bool {
        let at = self.word() == Some(w);
        if at { self.advance(); }
        at
    }

    /* whether a bracket opens on a later line so it starts a value and continues none */
    pub(super) fn fresh(&mut self) -> bool { Self::GRAMMAR.is_some() && self.tokens.peek().is_some_and(|t| t.line > self.last_line) }

    /* an operand and the operators of the current mode after it from `min_bp` up */
    pub(super) fn grammar_bp(&mut self, min_bp: u8) {
        match self.peek().and_then(|t| self.row(t, true)) {
            Some(r) => {
                self.advance();
                self.expr_bp(r.right);
                self.emit_row(r.emit, 1);
            }
            None => self.parse_atom(),
        }
        self.grammar_infix(min_bp);
    }

    pub(super) fn grammar_infix(&mut self, min_bp: u8) {
        while let Some(tok) = self.peek_same_line() {
            let Some(r) = self.row(tok, false) else { break };
            if r.left < min_bp { break; }
            self.advance();
            if let Emit::Lazy(and) = r.emit {
                let jmp = self.emit_jump(if and { OpCode::JumpIfFalseOrPop } else { OpCode::JumpIfTrueOrPop });
                self.expr_bp(r.right);
                self.patch(jmp);
                continue;
            }
            self.expr_bp(r.right);
            self.emit_row(r.emit, 2);
        }
    }

    // a call rotates its function under the operands already stacked
    fn emit_row(&mut self, emit: Emit, n: u16) {
        match emit {
            Emit::Op(op) => self.chunk.emit(op, 0),
            Emit::Call(f) => {
                self.emit_load_ssa(f);
                if n == 2 { self.chunk.emit(OpCode::Rot3, 0); self.chunk.emit(OpCode::Rot3, 0); } else { self.chunk.emit(OpCode::Swap, 0); }
                self.call_of(n, self.last_end as u32);
            }
            Emit::Lazy(_) => {}
        }
    }

    fn call_of(&mut self, n: u16, at: u32) {
        self.chunk.emit(OpCode::Call, super::pack_call(n, 0));
        self.chunk.record_call_pos(at);
    }

    /* a form head or a word of the current mode compiled in place */
    pub(super) fn grammar_name(&mut self, t: Token) -> bool {
        let name = self.lexeme(&t);
        if let Some(f) = Self::g().forms.iter().find(|f| f.head == name) {
            self.form(f, t.start as u32);
            return true;
        }
        let word = Self::g().modes.get(self.mode as usize).is_some_and(|m| m.words.contains(&name));
        if word { self.emit_const(Value::Str(name.into())); }
        word
    }

    fn in_mode(&mut self, k: u8) {
        let saved = core::mem::replace(&mut self.mode, k);
        self.expr();
        self.mode = saved;
    }

    /* a form's slots in order as the arguments of its call */
    fn form(&mut self, f: &Form, at: u32) {
        self.emit_load_ssa(f.call);
        let mut bound = alloc::string::String::new();
        for slot in f.slots {
            match *slot {
                Slot::Word(w) => { if !self.eat_word(w) { self.diag_at_peek(&crate::s!("expected '", str w, "'")); } continue; }
                Slot::Name => { let first = self.advance(); let mut end = first.end; while self.peek() == Some(TokenType::Dot) { self.advance(); end = self.advance().end; } self.emit_const(Value::Str(self.source[first.start..end].into())); }
                Slot::Expr(k) => self.in_mode(k),
                Slot::Bind => { bound = self.advance_text().into(); continue; }
                Slot::Body(k) => {
                    let saved = core::mem::replace(&mut self.mode, k);
                    let cut = self.enter_qual(S::NATIVES.names().anonymous, true);
                    let params = alloc::vec![core::mem::take(&mut bound)];
                    let body = self.expr_body(&params);
                    self.leave_qual(cut);
                    self.mode = saved;
                    self.push_function(params, body, 0, None, OpCode::MakeFunction);
                }
                Slot::At => self.emit_const(Value::Int(at as i64)),
                Slot::Opt(w, k) => if self.eat_word(w) { self.in_mode(k) } else { self.emit_const(Value::None) },
                Slot::Tagged(tags, default) => {
                    self.eat(TokenType::BracketOpen);
                    let word = self.word();
                    let call = tags.iter().find(|t| Some(t.0) == word).map_or(default, |t| { self.advance(); t.1 });
                    self.emit_load_ssa(call);
                    self.in_mode(0);
                    self.eat(TokenType::BracketClose);
                    self.call_of(1, at);
                }
                Slot::Limits(w, names) => {
                    let mut n = 0;
                    while (n == 0 && self.eat_word(w)) || (n > 0 && self.peek() == Some(TokenType::Comma) && self.limit_after(names)) {
                        if n > 0 { self.advance(); }
                        let Some(&(words, name)) = names.iter().find(|l| l.0.split(' ').all(|x| self.eat_word(x))) else { self.diag_at_peek("expected a limit"); break };
                        let _ = words;
                        self.emit_const(Value::Str(name.into()));
                        // a limit no value follows on its line counts one
                        if matches!(self.peek_same_line(), None | Some(TokenType::Comma | TokenType::BraceClose | TokenType::BlockClose | TokenType::ParenClose | TokenType::BracketClose)) { self.emit_const(Value::Int(1)) } else { self.in_mode(0) }
                        self.chunk.emit(OpCode::BuildList, 2);
                        n += 1;
                    }
                    self.chunk.emit(OpCode::BuildList, n);
                }
            }
        }
        self.call_of(f.slots.iter().filter(|s| !matches!(s, Slot::Word(_) | Slot::Bind)).count() as u16, at);
    }

    // whether a limit word follows the comma at the cursor
    fn limit_after(&self, names: &[(&str, &str)]) -> bool {
        let t = self.tokens.toks.get(self.tokens.pos + 1);
        t.is_some_and(|t| names.iter().any(|l| l.0.split(' ').next() == Some(&self.source[t.start..t.end])))
    }

    /* the value forms a table with a grammar reads past their leading token */
    pub(super) fn grammar_atom(&mut self, t: Token) -> bool {
        match t.kind {
            TokenType::If => self.value_if(),
            TokenType::Match => self.value_match(),
            TokenType::Fn => {
                self.eat(TokenType::ParenOpen);
                let (params, defaults) = self.param_list(TokenType::ParenClose, true);
                self.eat(TokenType::ParenClose);
                self.eat(TokenType::Equal);
                self.function_value(params, defaults, None);
            }
            TokenType::BlockOpen | TokenType::BraceOpen => self.braced(t.kind),
            _ => return false,
        }
        true
    }

    fn function_value(&mut self, params: Vec<alloc::string::String>, defaults: u16, name: Option<&str>) {
        let cut = self.enter_qual(name.unwrap_or(S::NATIVES.names().anonymous), true);
        let body = self.expr_body(&params);
        self.leave_qual(cut);
        self.push_function(params, body, defaults, name, OpCode::MakeFunction);
    }

    fn close_of(open: TokenType) -> TokenType { if open == TokenType::BlockOpen { TokenType::BlockClose } else { TokenType::BraceClose } }

    /* after `{` a record of named values or a block of bindings ending in its value */
    fn braced(&mut self, open: TokenType) {
        let close = Self::close_of(open);
        let named = |s: &mut Self| s.peek() == Some(TokenType::Name) && s.tokens.after_head() == TokenType::Equal;
        if self.peek() == Some(close) || named(self) {
            let mut n = 0;
            while named(self) {
                let key = self.advance_text();
                self.emit_const(Value::Str(key.into()));
                self.advance();
                self.expr();
                n += 1;
                if !self.eat_if(TokenType::Comma) { break; }
            }
            self.eat(close);
            return self.chunk.emit(OpCode::BuildDict, n);
        }
        while self.peek() == Some(TokenType::Let) { self.let_item(); }
        self.expr();
        self.eat(close);
    }

    fn block_after(&mut self) {
        match self.peek() {
            Some(k @ (TokenType::BlockOpen | TokenType::BraceOpen)) => { self.advance(); self.braced(k); }
            _ => self.diag_at_peek("expected '{'"),
        }
    }

    /* each test then its block with the last block taken when every test failed */
    fn value_if(&mut self) {
        self.enter_block();
        let mut ends = Vec::new();
        loop {
            self.in_mode(0);
            let jf = self.emit_jump(OpCode::JumpIfFalse);
            self.block_after();
            ends.push(self.emit_jump(OpCode::Jump));
            self.mid_block();
            self.patch(jf);
            self.eat(TokenType::Else);
            if !self.eat_if(TokenType::If) { break; }
        }
        self.block_after();
        for e in ends { self.patch(e); }
        self.commit_block();
    }

    /* the first arm whose pattern takes the subject gives the value and none raises */
    fn value_match(&mut self) {
        let at = self.last_end;
        self.in_mode(0);
        let subj = self.pattern_slot();
        self.chunk.emit(OpCode::StoreName, subj);
        let Some(open @ (TokenType::BlockOpen | TokenType::BraceOpen)) = self.peek() else { return self.diag_at_peek("expected '{'") };
        self.advance();
        let close = Self::close_of(open);
        self.enter_block();
        let mut ends = Vec::new();
        while self.peek().is_some() && self.peek() != Some(close) {
            let mut fails = Vec::new();
            let from = self.tokens.pos;
            self.pattern(subj, &mut fails);
            self.eat(TokenType::Arm);
            self.expr();
            self.eat_if(TokenType::Comma);
            ends.push(self.emit_jump(OpCode::Jump));
            self.mid_block();
            for f in fails { self.patch(f); }
            if self.tokens.pos == from { self.tokens.next(); }
        }
        self.eat(close);
        self.emit_load_ssa(Self::g().unmatched);
        self.emit_const(Value::Int(at as i64));
        self.call_of(1, at as u32);
        for e in ends { self.patch(e); }
        self.commit_block();
    }

    /* the pattern at the cursor tested on `subj` jumping out where a test fails */
    pub(super) fn pattern(&mut self, subj: u16, fails: &mut Vec<usize>) {
        // a literal matches the value equal to it
        if matches!(self.peek(), Some(TokenType::Int | TokenType::String | TokenType::True | TokenType::False)) {
            self.chunk.emit(OpCode::LoadName, subj);
            self.parse_atom();
            self.chunk.emit(OpCode::Eq, 0);
            return fails.push(self.emit_jump(OpCode::JumpIfFalse));
        }
        let t = self.advance();
        let part = |s: &mut Self, i: u16| {
            s.chunk.emit(OpCode::LoadName, subj);
            s.emit_const(Value::Int(i as i64));
            s.chunk.emit(OpCode::GetItem, 0);
            let slot = s.pattern_slot();
            s.chunk.emit(OpCode::StoreName, slot);
            slot
        };
        match t.kind {
            TokenType::Underscore => {}
            TokenType::ParenOpen => {
                let mut i = 0;
                while self.peek() != Some(TokenType::ParenClose) && self.peek().is_some() {
                    let slot = part(self, i);
                    self.pattern(slot, fails);
                    i += 1;
                    if !self.eat_if(TokenType::Comma) { break; }
                }
                self.eat(TokenType::ParenClose);
            }
            TokenType::Name => {
                let name = self.lexeme(&t);
                let Some(p) = Self::g().patterns.iter().find(|p| p.word == name) else {
                    self.chunk.emit(OpCode::LoadName, subj);
                    return self.store_name(name);
                };
                self.chunk.emit(OpCode::LoadName, subj);
                match p.test {
                    Test::Nil | Test::NotNil => { self.emit_const(Value::None); self.chunk.emit(if matches!(p.test, Test::Nil) { OpCode::Is } else { OpCode::IsNot }, 0); }
                    Test::Item(i, b) => { self.emit_const(Value::Int(i as i64)); self.chunk.emit(OpCode::GetItem, 0); self.emit_const(Value::Bool(b)); self.chunk.emit(OpCode::Eq, 0); }
                    Test::Field(f, k) => { let i = self.chunk.push_name(f); self.chunk.emit(OpCode::LoadAttr, i); self.emit_const(Value::Int(k)); self.chunk.emit(OpCode::Eq, 0); }
                }
                fails.push(self.emit_jump(OpCode::JumpIfFalse));
                // a word with no part of its own binds the subject to the name after it
                match p.part { Some(i) => { let slot = part(self, i); self.pattern(slot, fails); } None if matches!(self.peek(), Some(TokenType::Name | TokenType::Underscore)) => self.pattern(subj, fails), None => {} }
            }
            _ => self.error_at(t.start, t.end, "expected a pattern"),
        }
    }

    /* whether the loop target at the cursor holds a pattern word or a wildcard */
    pub(super) fn pattern_target(&mut self) -> Option<Mark> {
        Self::GRAMMAR?;
        let at = self.mark();
        let mut i = self.tokens.pos;
        let mut patterned = false;
        while let Some(t) = self.tokens.toks.get(i).filter(|t| t.kind != TokenType::In) {
            patterned |= t.kind == TokenType::Underscore || (t.kind == TokenType::Name && Self::g().patterns.iter().any(|p| p.word == &self.source[t.start..t.end]));
            i += 1;
        }
        if !patterned { return None; }
        self.tokens.pos = i;
        Some(at)
    }

    /* the looped value bound through the pattern at `at` failing back to `start` */
    pub(super) fn bind_pattern(&mut self, at: Mark, start: u16) {
        let slot = self.pattern_slot();
        self.chunk.emit(OpCode::StoreName, slot);
        let back = self.mark();
        self.seek(at);
        let mut fails = Vec::new();
        self.pattern(slot, &mut fails);
        self.seek(back);
        for f in fails { self.patch_to(f, start); }
    }

    /* `let name = value` or `let _ = value` as a module or block binding */
    fn let_item(&mut self) {
        self.advance();
        let t = self.advance();
        self.eat(TokenType::Equal);
        self.expr();
        if t.kind == TokenType::Underscore { self.chunk.emit(OpCode::PopTop, 0); } else { self.store_name(self.lexeme(&t)); }
    }

    /* an item the grammar reads with its modifier words and none for the fixed statements */
    pub(super) fn grammar_item(&mut self) -> Option<bool> {
        let g = Self::GRAMMAR?;
        let mut wraps = Vec::new();
        while let Some(m) = self.word().and_then(|w| g.modifiers.iter().find(|m| m.0 == w)) {
            self.advance();
            wraps.extend(m.1);
        }
        match self.peek()? {
            TokenType::Let => self.let_item(),
            TokenType::Fn => {
                self.advance();
                let name = self.ident_or_missing("expected function name");
                let (params, defaults) = self.parse_params();
                self.eat(TokenType::Equal);
                self.function_value(params, defaults, Some(name));
                for w in wraps.iter().rev() {
                    self.emit_load_ssa(w);
                    self.chunk.emit(OpCode::Swap, 0);
                    self.call_of(1, self.last_end as u32);
                }
                self.emit_store_new(name);
            }
            // a type item checks nothing here so its tokens pass up to the next item
            TokenType::Type => {
                self.advance();
                let mut depth = 0usize;
                while let Some(k) = self.peek() {
                    if depth == 0 && (matches!(k, TokenType::Fn | TokenType::Let | TokenType::Type | TokenType::Import) || self.word().is_some_and(|w| g.modifiers.iter().any(|m| m.0 == w))) { break; }
                    match k {
                        TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen | TokenType::BlockOpen => depth += 1,
                        TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose | TokenType::BlockClose => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    self.advance();
                }
            }
            _ => return None,
        }
        Some(false)
    }

    /* an annotation passed over whole to the end it ends at outside brackets */
    pub(super) fn skip_typed(&mut self, end: fn(TokenType) -> bool) {
        let mut depth = 0usize;
        while let Some(k) = self.peek() {
            if depth == 0 && end(k) { break; }
            match k {
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose if depth > 0 => depth -= 1,
                _ => {}
            }
            self.advance();
        }
    }
}
