use crate::s;

use super::Parser;
use crate::code::OpCode;

use crate::syntax::lexer::{Spelling, Token, TokenType};

use alloc::vec::Vec;

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {

    pub(super) fn if_stmt(&mut self) {
        self.advance();
        self.enter_block();
        self.if_body();
        self.commit_block();
    }

    /* the arms of a chain in turn so no `else if` length nests the parser */
    fn if_body(&mut self) {
        // the jumps out of the arms linked through their operands as a patch list
        let mut ends = u16::MAX;
        loop {
            self.header_test(Self::expr);
            let jf = self.emit_jump(OpCode::JumpIfFalse);

            self.header_end();
            self.compile_block();

            let Some(kind @ (TokenType::ElseIf | TokenType::Else)) = self.peek() else {
                self.patch(jf);
                break;
            };
            self.advance();
            let jmp = self.emit_jump(OpCode::Jump);
            // a frozen chunk hands back a stale index which never joins the list
            if !self.chunk.overflow { self.patch_to(jmp, ends); ends = jmp as u16; }
            self.mid_block();
            self.patch(jf);
            if kind == TokenType::Else {
                self.header_end();
                self.compile_block();
                break;
            }
        }
        while ends != u16::MAX {
            let jmp = ends as usize;
            ends = self.chunk.instructions.get(jmp).map_or(u16::MAX, |i| i.operand);
            self.patch(jmp);
        }
    }

    /* the expression of a header inside the parentheses a table may want around it */
    fn header_test(&mut self, f: impl FnOnce(&mut Self)) {
        if !Self::SHAPES.header_parens { return f(self); }
        self.eat(TokenType::ParenOpen);
        f(self);
        self.eat(TokenType::ParenClose);
    }

    /* a fresh slot for a value a sub-pattern matches against */
    pub(super) fn pattern_slot(&mut self) -> u16 {
        let ver = self.increment_version(crate::syntax::ssa::SSA_TMP_MATCH_ITEM);
        self.chunk.push_name(&s!(str crate::syntax::ssa::SSA_TMP_MATCH_ITEM, int ver))
    }

    /* opens a loop at the next instruction and returns where it starts */
    fn enter_loop(&mut self, is_for: bool) -> u16 {
        let start = self.chunk.instructions.len() as u16;
        self.loops.push(crate::syntax::ssa::LoopCtx { start, breaks: Vec::new(), is_for, cleanup_base: self.cleanup_count });
        start
    }

    /* closes the innermost loop and returns its `break` jumps to patch */
    fn exit_loop(&mut self) -> Vec<usize> {
        self.loops.pop().map(|l| l.breaks).unwrap_or_default()
    }

    pub(super) fn while_stmt(&mut self) {
        self.advance();
        self.enter_block();

        let loop_start = self.enter_loop(false);

        self.header_test(Self::expr);
        let jf = self.emit_jump(OpCode::JumpIfFalse);

        self.header_end();
        self.compile_block();

        self.chunk.emit(OpCode::Jump, loop_start);
        self.patch(jf);

        // the loop closes before the else so its break or continue targets the outer loop
        let breaks = self.exit_loop();

        if self.eat_if(TokenType::Else) {
            self.header_end();
            self.compile_block();
        }

        // breaks land past the else clause
        for pos in breaks { self.patch(pos); }

        self.commit_block();
    }

    pub(super) fn for_stmt(&mut self) {
        self.advance();

        // a table with header parentheses holds the targets and the iterable in them
        if Self::SHAPES.header_parens { self.eat(TokenType::ParenOpen); }
        // targets store inside the loop so the cursor skips them and returns for each item
        let at = self.mark();
        let Some(shape) = self.tokens.target_list() else { return self.error("cannot assign to this expression") };
        self.eat(TokenType::In);
        self.expr_or_tuple(|s| s.peek().is_none_or(|k| k == Self::OPEN || (Self::SHAPES.header_parens && k == TokenType::ParenClose)));
        if Self::SHAPES.header_parens { self.eat(TokenType::ParenClose); }
        self.chunk.emit(OpCode::GetIter, 0);

        self.enter_block();

        let loop_start = self.enter_loop(true);

        let fi = self.emit_jump(OpCode::ForIter);

        self.store_back(at, shape);

        self.header_end();
        self.compile_block();

        self.chunk.emit(OpCode::Jump, loop_start);
        self.patch(fi);

        // the loop closes before the else so its break or continue targets the outer loop
        let breaks = self.exit_loop();

        if self.eat_if(TokenType::Else) {
            self.header_end();
            self.compile_block();
        }

        // breaks land past the else clause
        for pos in breaks { self.patch(pos); }

        self.commit_block();
    }

    pub(super) fn try_stmt(&mut self) {
        self.advance();
        self.header_end();

        // the outer cleanup frame runs the finally on every exit including return break and raise
        let fin_setup = self.emit_jump(OpCode::SetupFinally);
        self.cleanup_count += 1;
        let setup = self.emit_jump(OpCode::SetupExcept);

        self.enter_block();
        self.compile_block();

        self.chunk.emit(OpCode::PopExcept, 0);
        let success_jump = self.emit_jump(OpCode::Jump);

        self.mid_block();

        self.patch(setup);

        let mut end_jumps: Vec<usize> = Vec::new();
        let mut next_arm_jump: Option<usize> = None;
        let mut had_bare = false;
        let mut had_except = false;

        while matches!(self.peek(), Some(TokenType::Catch)) {
            let at = self.advance().start;
            if self.eat_if(TokenType::Star) { self.reject(at, self.last_end, Self::CATCH_STAR_REFUSED); }
            had_except = true;
            if let Some(j) = next_arm_jump.take() { self.patch(j); }
            if had_bare {
                self.error(Self::DEFAULT_CATCH);
                break;
            }

            // the header runs its own code so an error in its type reports its line
            self.mark_stmt();
            if self.peek() == Some(Self::OPEN) {
                had_bare = true;
                self.chunk.emit(OpCode::PopTop, 0);
            } else {
                self.chunk.emit(OpCode::Dup, 0);
                self.expr();
                let match_pos = self.last_end as u32;
                self.chunk.emit(OpCode::MatchExc, 0);
                self.chunk.record_call_pos(match_pos);
                next_arm_jump = Some(self.emit_jump(OpCode::JumpIfFalse));

                let named = self.eat_if(TokenType::As);
                if named && matches!(self.peek_same_line(), Some(TokenType::Name | TokenType::Underscore)) {
                    let n = self.advance_text();
                    self.store_name(n);
                } else {
                    // a target that is no name is reported and passed so the header still ends
                    if named {
                        self.diag_at_peek("expected a name");
                        if self.peek_same_line().is_some_and(|k| k != Self::OPEN) { self.tokens.next(); }
                    }
                    self.chunk.emit(OpCode::PopTop, 0);
                }
            }
            self.header_end();
            self.compile_block();

            // handled arms jump past the `else` block while a bare last arm falls through
            let more = matches!(
                self.peek(),
                Some(TokenType::Catch | TokenType::Else | TokenType::Finally)
            );
            if !had_bare || more {
                end_jumps.push(self.emit_jump(OpCode::Jump));
            }
        }

        if let Some(j) = next_arm_jump {
            self.patch(j);
            self.chunk.emit(OpCode::Raise, 0);
        } else if !had_except {
            // without an except the handler re-raises so the outer finally still runs
            self.chunk.emit(OpCode::Raise, 0);
        }

        // the success path falls into `else` and handled arms skip it
        self.patch(success_jump);
        if self.eat_if(TokenType::Else) {
            self.header_end();
            self.compile_block();
        }
        for j in end_jumps {
            self.patch(j);
        }

        // normal exits pop the frame and mark the entry while unwinds jump to the body
        self.cleanup_count -= 1;
        self.chunk.emit(OpCode::PopExcept, 0);
        self.chunk.emit(OpCode::BeginFinally, 0);
        let fin_body = self.chunk.instructions.len() as u16;
        self.patch_to(fin_setup, fin_body);
        if self.eat_if(TokenType::Finally) {
            self.header_end();
            self.compile_block();
        }
        self.chunk.emit(OpCode::EndFinally, 0);

        self.commit_block();
    }

    /* with refused by its caller and parsed only for the errors of its parts */
    pub(super) fn with_stmt(&mut self) {
        self.advance();
        // the items sit in brackets only when the block opener follows the group
        let grouped = matches!(self.peek_same_line(), Some(TokenType::ParenOpen)) && self.tokens.group_then(Self::OPEN);
        if grouped { self.advance(); }
        loop {
            self.discarded(|p| { p.expr(); if p.eat_if(TokenType::As) { p.store_target(); } });
            let more = self.eat_if(TokenType::Comma);
            if grouped && (!more || matches!(self.peek(), Some(TokenType::ParenClose))) { self.eat(TokenType::ParenClose); break; }
            if !more { break; }
        }
        self.header_end();
        self.compile_block();
    }

}
