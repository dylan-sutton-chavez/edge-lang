use crate::s;

use super::Parser;
use super::types::OpCode;

use crate::lexer::{Token, TokenType};

use alloc::{vec::Vec, string::ToString};

/* Positions of `kind` outside any bracket in `toks`. */
fn depth0(toks: &[Token], kind: TokenType) -> Vec<usize> {
    let mut depth = 0i32;
    toks.iter().enumerate().filter_map(|(i, t)| {
        match t.kind {
            TokenType::Lpar | TokenType::Lsqb | TokenType::Lbrace => depth += 1,
            TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace => depth -= 1,
            _ => {}
        }
        (depth == 0 && t.kind == kind).then_some(i)
    }).collect()
}

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {

    /* if/elif/else compiler, emits JumpIfFalse/Jump and patches branch join targets */

    pub(super) fn if_stmt(&mut self) {
        self.advance();
        self.enter_block();
        self.if_body();
        self.commit_block();
    }

    pub(super) fn if_body(&mut self) {
        self.expr();
        let jf = self.emit_jump(OpCode::JumpIfFalse);

        self.eat(TokenType::Colon);
        self.compile_block();

        match self.peek() {
            Some(TokenType::Elif) => {
                self.advance();
                let jmp = self.emit_jump(OpCode::Jump);
                self.mid_block();
                self.patch(jf);
                self.if_body();
                self.patch(jmp);
            }
            Some(TokenType::Else) => {
                self.advance();
                let jmp = self.emit_jump(OpCode::Jump);
                self.mid_block();
                self.patch(jf);
                self.eat(TokenType::Colon);
                self.compile_block();
                self.patch(jmp);
            }
            _ => {
                self.patch(jf);
            }
        }
    }

    /* match/case, literals, captures, wildcards, OR, guards, sequences, emits subject-load + pattern + guard + Jump-end. */
    pub(super) fn match_stmt(&mut self) {
        self.advance();
        self.expr();

        let ver = self.increment_version(super::SSA_TMP_MATCH);
        let subj = self.chunk.push_name(&s!(str super::SSA_TMP_MATCH, int ver));
        self.chunk.emit(OpCode::StoreName, subj);

        self.eat(TokenType::Colon);
        self.eat_if(TokenType::Indent);

        let mut end_jumps = Vec::new();

        while matches!(self.peek(), Some(TokenType::Case)) {
            self.advance();

            let mut fail_jumps: Vec<usize> = Vec::new();
            self.parse_pattern(subj, &mut fail_jumps);

            // Guard fail joins pattern fails, both land at the next case.
            if self.eat_if(TokenType::If) {
                self.expr();
                fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
            }

            self.eat(TokenType::Colon);
            self.compile_block();

            // The last case falls through to the end.
            if matches!(self.peek(), Some(TokenType::Case)) { end_jumps.push(self.emit_jump(OpCode::Jump)); }

            for j in fail_jumps { self.patch(j); }
        }

        self.eat_if(TokenType::Dedent);

        for pos in end_jumps { self.patch(pos); }
    }

    /* Buffers one case pattern up to its guard or colon, a top-level comma or star opens a sequence. */
    pub(super) fn parse_pattern(&mut self, subj: u16, fail_jumps: &mut Vec<usize>) {
        let toks = self.pattern_tokens(Vec::new(), |k| matches!(k, TokenType::Colon | TokenType::If));
        let Some(first) = toks.first() else {
            let at = self.tokens.peek().map_or(self.last_end, |t| t.start);
            return self.error_at(at, at, "expected a pattern");
        };
        if first.kind == TokenType::Star || !depth0(&toks, TokenType::Comma).is_empty() {
            self.sequence_items(&toks, subj, fail_jumps);
        } else {
            self.sub_pattern(&toks, subj, fail_jumps);
        }
    }

    /* Buffers tokens until `end` at depth 0 or the line end, so a sequence pattern counts its items before emitting. */
    fn pattern_tokens(&mut self, mut toks: Vec<crate::lexer::Token>, end: impl Fn(TokenType) -> bool) -> Vec<crate::lexer::Token> {
        let mut depth = 0i32;
        while let Some(k) = self.peek_same_line() {
            if depth == 0 && end(k) { break; }
            match k {
                TokenType::Lpar | TokenType::Lsqb | TokenType::Lbrace => depth += 1,
                TokenType::Rpar | TokenType::Rsqb | TokenType::Rbrace => depth -= 1,
                _ => {}
            }
            toks.push(self.advance());
        }
        toks
    }

    /* Sequence pattern length-checks subject, indexes items into fresh slots, star captures middle slice. */
    fn sequence_items(&mut self, toks: &[crate::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        // Items split at depth-0 commas as (starred, start, end), a trailing comma adds none.
        let mut items: Vec<(bool, usize, usize)> = Vec::new();
        let mut lo = 0;
        for hi in depth0(toks, TokenType::Comma).into_iter().chain(core::iter::once(toks.len())) {
            if lo < hi {
                let star = toks[lo].kind == TokenType::Star;
                items.push((star, lo + star as usize, hi));
            }
            lo = hi + 1;
        }
        let stars = items.iter().filter(|it| it.0).count();
        if stars > 1 {
            self.error_at(toks[0].start, toks.last().unwrap().end, "multiple stars in sequence pattern");
        }
        let n = items.len() as i64;

        // Type guard, a non-sequence subject fails the pattern instead of erroring on len().
        self.chunk.emit(OpCode::LoadName, subj);
        self.chunk.emit(OpCode::MatchSeq, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));

        // Length check, exact without star, >= (count-1) with star.
        self.chunk.emit(OpCode::LoadName, subj);
        self.chunk.emit(OpCode::CallLen, 1);
        self.emit_const(super::types::Value::Int(n - (stars > 0) as i64));
        self.chunk.emit(if stars > 0 { OpCode::GtEq } else { OpCode::Eq }, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));

        // Fresh slot to use as the per-item sub-subject.
        let item_subj = self.pattern_slot();

        let mut seen_star = false;
        for (k, &(star, lo, hi)) in items.iter().enumerate() {
            let k = k as i64;
            if star {
                seen_star = true;
                // Slice subj[k : len-suffix]
                self.chunk.emit(OpCode::LoadName, subj);
                self.emit_const(super::types::Value::Int(k));
                self.chunk.emit(OpCode::LoadName, subj);
                self.chunk.emit(OpCode::CallLen, 1);
                self.emit_const(super::types::Value::Int(n - k - 1));
                self.chunk.emit(OpCode::Sub, 0);
                self.chunk.emit(OpCode::LoadNone, 0);
                self.chunk.emit(OpCode::BuildSlice, 3);
                self.chunk.emit(OpCode::GetItem, 0);
                self.chunk.emit(OpCode::CallList, 1);
            } else {
                // Negative index for items after the star.
                self.chunk.emit(OpCode::LoadName, subj);
                self.emit_const(super::types::Value::Int(if seen_star { k - n } else { k }));
                self.chunk.emit(OpCode::GetItem, 0);
            }
            self.chunk.emit(OpCode::StoreName, item_subj);
            self.sub_pattern(&toks[lo..hi], item_subj, fail_jumps);
        }
    }

    /* `[p, ...]` or `(p, ...)` is a sequence, parentheses without a comma only group. */
    fn bracket_pattern(&mut self, paren: bool, inner: &[crate::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        let group = paren && !inner.is_empty()
            && depth0(inner, TokenType::Comma).is_empty() && depth0(inner, TokenType::Star).is_empty();
        if group { self.sub_pattern(inner, subj, fail_jumps); } else { self.sequence_items(inner, subj, fail_jumps); }
    }

    fn sub_pattern(&mut self, toks: &[crate::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        self.nested(|s| s.one_pattern(toks, subj, fail_jumps));
    }

    /* One buffered pattern against `subj`, an `as` capture, an OR, a nested sequence, a wildcard, a capture, or a literal. */
    fn one_pattern(&mut self, toks: &[crate::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        // `p as name` binds the subject once `p`, OR included, matched.
        if let Some(&at) = depth0(toks, TokenType::As).last() {
            let [t] = &toks[at + 1..] else { return self.error_at(toks[at].start, toks.last().unwrap().end, "invalid pattern target") };
            if t.kind != TokenType::Name { return self.error_at(t.start, t.end, "invalid pattern target"); }
            self.sub_pattern(&toks[..at], subj, fail_jumps);
            let name = &self.source[t.start..t.end];
            self.chunk.emit(OpCode::LoadName, subj);
            self.emit_store_new(name);
            return;
        }
        // Each alternative of `p | q` sends its failure on to the next one.
        let bars = depth0(toks, TokenType::Vbar);
        if !bars.is_empty() {
            let (mut lo, mut succ) = (0, Vec::new());
            for hi in bars.into_iter().chain(core::iter::once(toks.len())) {
                if hi == toks.len() { self.sub_pattern(&toks[lo..], subj, fail_jumps); break; }
                let mut alt_fails = Vec::new();
                self.sub_pattern(&toks[lo..hi], subj, &mut alt_fails);
                succ.push(self.emit_jump(OpCode::Jump));
                for j in alt_fails { self.patch(j); }
                lo = hi + 1;
            }
            for j in succ { self.patch(j); }
            return;
        }
        if let (Some(first), Some(last)) = (toks.first(), toks.last()) {
            match (first.kind, last.kind) {
                (TokenType::Lsqb, TokenType::Rsqb) | (TokenType::Lpar, TokenType::Rpar) =>
                    return self.bracket_pattern(first.kind == TokenType::Lpar, &toks[1..toks.len() - 1], subj, fail_jumps),
                (TokenType::Lbrace, TokenType::Rbrace) => return self.mapping_pattern(&toks[1..toks.len() - 1], subj, fail_jumps),
                _ => {}
            }
        }
        if toks.is_empty() || (toks.len() == 1 && toks[0].kind == TokenType::Underscore) { return; }
        if toks.len() == 1 && toks[0].kind == TokenType::Name {
            let name = &self.source[toks[0].start..toks[0].end];
            self.chunk.emit(OpCode::LoadName, subj);
            self.emit_store_new(name);
            return;
        }
        // `a.b.c` is a value pattern, `a.b(...)` a class pattern.
        let path = toks.iter().enumerate().take_while(|&(i, t)| t.kind == if i % 2 == 0 { TokenType::Name } else { TokenType::Dot }).count();
        if path % 2 == 1 && path == toks.len() {
            self.chunk.emit(OpCode::LoadName, subj);
            self.emit_dotted(&toks[..path]);
            self.chunk.emit(OpCode::Eq, 0);
            fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
            return;
        }
        if path % 2 == 1 && toks.get(path).is_some_and(|t| t.kind == TokenType::Lpar) && toks.last().is_some_and(|t| t.kind == TokenType::Rpar) {
            return self.class_pattern(&toks[..path], &toks[path + 1..toks.len() - 1], subj, fail_jumps);
        }
        let Some(v) = self.pattern_literal(toks) else {
            self.error_at(toks[0].start, toks.last().unwrap().end, "unsupported pattern (use literals, names, _, sequences, mappings or classes)");
            return;
        };
        // `None`, `True` and `False` match by identity, so `case True` rejects 1.
        let op = if matches!(v, super::types::Value::Bool(_) | super::types::Value::None) { OpCode::Is } else { OpCode::Eq };
        self.chunk.emit(OpCode::LoadName, subj);
        self.emit_const(v);
        self.chunk.emit(op, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
    }

    /* Warns when `break` or `continue` at `at` leaves the `finally` body it sits in. */
    pub(super) fn leaves_finally(&mut self, at: usize, word: &str) {
        if self.finally_loops.is_some_and(|depth| self.loops.len() <= depth as usize) { self.warn(at, crate::s!("'", str word, "' in a 'finally' block")); }
    }

    /* A literal pattern, a number after an optional minus, adjacent strings or bytes, or a singleton. */
    fn pattern_literal(&self, toks: &[crate::lexer::Token]) -> Option<super::types::Value> {
        use super::types::{Value, parse_string, parse_bytes_literal};
        let neg = toks.first()?.kind == TokenType::Minus;
        let toks = &toks[neg as usize..];
        let text = |t: &crate::lexer::Token| &self.source[t.start..t.end];
        let kind = toks.first()?.kind;
        // Adjacent string or bytes literals join into one, as in an expression.
        if !neg && matches!(kind, TokenType::String | TokenType::Bytes) && toks.iter().all(|t| t.kind == kind) {
            return Some(if kind == TokenType::String { Value::Str(toks.iter().map(|t| parse_string(text(t))).collect()) }
                else { Value::Bytes(toks.iter().flat_map(|t| parse_bytes_literal(text(t))).collect()) });
        }
        let [t] = toks else { return None };
        Some(match (t.kind, neg) {
            (TokenType::Int, _) => match Self::int_literal(text(t)).ok()? {
                Value::Int(i) if neg => Value::Int(-i),
                Value::BigInt(b) if neg => Value::BigInt(b.neg()),
                v => v,
            },
            (TokenType::Float, _) => {
                let f = Self::float_literal(text(t))?;
                Value::Float(if neg { -f } else { f })
            }
            (TokenType::True, false) => Value::Bool(true),
            (TokenType::False, false) => Value::Bool(false),
            (TokenType::None, false) => Value::None,
            _ => return None,
        })
    }

    /* Loads a dotted name such as `Color.RED` from its tokens. */
    fn emit_dotted(&mut self, path: &[crate::lexer::Token]) {
        self.emit_load_ssa(&self.source[path[0].start..path[0].end]);
        for t in path.iter().skip(2).step_by(2) {
            let idx = self.chunk.push_name(&self.source[t.start..t.end]);
            self.chunk.emit(OpCode::LoadAttr, idx);
        }
    }

    /* A fresh slot for a value a sub-pattern matches against. */
    fn pattern_slot(&mut self) -> u16 {
        let ver = self.increment_version(super::SSA_TMP_MATCH_ITEM);
        self.chunk.push_name(&s!(str super::SSA_TMP_MATCH_ITEM, int ver))
    }

    /* `C(p, k=q)` asks MatchClass for the attribute values, then matches each against its sub-pattern. */
    fn class_pattern(&mut self, path: &[crate::lexer::Token], inner: &[crate::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        let (mut positional, mut keywords) = (Vec::new(), Vec::new());
        let mut lo = 0;
        for hi in depth0(inner, TokenType::Comma).into_iter().chain(core::iter::once(inner.len())) {
            let item = &inner[lo..hi];
            match item {
                [] => {}
                [k, eq, rest @ ..] if k.kind == TokenType::Name && eq.kind == TokenType::Equal => keywords.push((&self.source[k.start..k.end], rest)),
                _ => positional.push(item),
            }
            lo = hi + 1;
        }
        self.chunk.emit(OpCode::LoadName, subj);
        self.emit_dotted(path);
        for &(k, _) in &keywords { self.emit_const(super::types::Value::Str(k.to_string())); }
        self.chunk.emit(OpCode::BuildTuple, keywords.len() as u16);
        self.chunk.emit(OpCode::MatchClass, positional.len() as u16);
        let values = self.pattern_slot();
        self.chunk.emit(OpCode::StoreName, values);
        self.chunk.emit(OpCode::LoadName, values);
        self.chunk.emit(OpCode::LoadNone, 0);
        self.chunk.emit(OpCode::IsNot, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
        let subs: Vec<&[crate::lexer::Token]> = positional.into_iter().chain(keywords.into_iter().map(|(_, p)| p)).collect();
        for (i, sub) in subs.into_iter().enumerate() {
            self.chunk.emit(OpCode::LoadName, values);
            self.emit_const(super::types::Value::Int(i as i64));
            self.chunk.emit(OpCode::GetItem, 0);
            let item = self.pattern_slot();
            self.chunk.emit(OpCode::StoreName, item);
            self.sub_pattern(sub, item, fail_jumps);
        }
    }

    /* `{"k": p, **rest}` needs a dict holding every literal key, each value matching its sub-pattern. */
    fn mapping_pattern(&mut self, inner: &[crate::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        self.chunk.emit(OpCode::LoadName, subj);
        self.chunk.emit(OpCode::MatchMap, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
        let (mut keys, mut rest) = (Vec::new(), None);
        let mut lo = 0;
        for hi in depth0(inner, TokenType::Comma).into_iter().chain(core::iter::once(inner.len())) {
            let item = &inner[lo..hi];
            lo = hi + 1;
            if item.is_empty() { continue; }
            if let [star, name] = item && star.kind == TokenType::DoubleStar {
                rest = Some(self.source[name.start..name.end].to_string());
                continue;
            }
            let Some(&colon) = depth0(item, TokenType::Colon).first() else {
                self.error_at(item[0].start, item.last().unwrap().end, "mapping pattern items are `key: pattern`");
                continue;
            };
            let Some(key) = self.pattern_literal(&item[..colon]) else {
                self.error_at(item[0].start, item[colon].end, "mapping pattern keys must be literals");
                continue;
            };
            let ki = self.chunk.push_const(key);
            self.chunk.emit(OpCode::LoadConst, ki);
            self.chunk.emit(OpCode::LoadName, subj);
            self.chunk.emit(OpCode::In, 0);
            fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
            self.chunk.emit(OpCode::LoadName, subj);
            self.chunk.emit(OpCode::LoadConst, ki);
            self.chunk.emit(OpCode::GetItem, 0);
            let slot = self.pattern_slot();
            self.chunk.emit(OpCode::StoreName, slot);
            self.sub_pattern(&item[colon + 1..], slot, fail_jumps);
            keys.push(ki);
        }
        // `**rest` binds a copy without the matched keys.
        if let Some(name) = rest {
            self.chunk.emit(OpCode::BuildDict, 0);
            self.chunk.emit(OpCode::LoadName, subj);
            self.chunk.emit(OpCode::DictUpdate, 0);
            let slot = self.emit_store_new(&name);
            for ki in keys {
                self.chunk.emit(OpCode::LoadName, slot);
                self.chunk.emit(OpCode::LoadConst, ki);
                self.chunk.emit(OpCode::DelItem, 0);
            }
        }
    }

    /* while, cond + body + back-edge, optional else when cond falsifies. */

    /* Opens a loop at the next instruction and returns where it starts. */
    fn enter_loop(&mut self, is_for: bool) -> u16 {
        let start = self.chunk.instructions.len() as u16;
        self.loops.push(super::types::LoopCtx { start, breaks: Vec::new(), is_for, cleanup_base: self.cleanup_count });
        start
    }

    /* Closes the innermost loop and returns its `break` jumps to patch. */
    fn exit_loop(&mut self) -> Vec<usize> {
        self.loops.pop().map(|l| l.breaks).unwrap_or_default()
    }

    pub(super) fn while_stmt(&mut self) {
        self.advance();
        self.enter_block();

        let loop_start = self.enter_loop(false);

        self.expr();
        let jf = self.emit_jump(OpCode::JumpIfFalse);

        self.eat(TokenType::Colon);
        self.compile_block();

        self.chunk.emit(OpCode::Jump, loop_start);
        self.patch(jf);

        // Pop loop state before the else so its break/continue target the enclosing loop.
        let breaks = self.exit_loop();

        if self.eat_if(TokenType::Else) {
            self.eat(TokenType::Colon);
            self.compile_block();
        }

        // Loop breaks land past the else clause.
        for pos in breaks { self.patch(pos); }

        self.commit_block();
    }

    /* for / async for, with optional tuple/star unpacking. */

    pub(super) fn for_stmt_inner(&mut self, is_async: bool) {
        self.advance();

        // The targets store inside the loop, so the cursor skips them and comes back on each item.
        let at = self.mark();
        let Some(shape) = self.tokens.target_list() else { return self.error("cannot assign to this expression") };
        self.eat(TokenType::In);
        // Unparenthesized tuple iterable, `for x in 1, 2:` iterates the tuple.
        self.expr_or_tuple(|s| matches!(s.peek(), Some(TokenType::Colon) | None));
        self.chunk.emit(OpCode::GetIter, if is_async { super::ITER_ASYNC } else { 0 });

        self.enter_block();

        let loop_start = self.enter_loop(true);

        let fi = self.emit_jump(OpCode::ForIter);

        self.store_back(at, shape);

        self.eat(TokenType::Colon);
        self.compile_block();

        self.chunk.emit(OpCode::Jump, loop_start);
        self.patch(fi);

        // Pop loop state before the else so its break/continue target the enclosing loop.
        let breaks = self.exit_loop();

        if !is_async && self.eat_if(TokenType::Else) {
            self.eat(TokenType::Colon);
            self.compile_block();
        }

        // Loop breaks land past the else clause.
        for pos in breaks { self.patch(pos); }

        self.commit_block();
    }

    /* try / except / else / finally with exception arm chaining. */

    pub(super) fn try_stmt(&mut self) {
        self.advance();
        self.eat(TokenType::Colon);

        // Outer cleanup frame, the finally runs on every exit (normal, return, break, raise).
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

        while self.eat_if(TokenType::Except) {
            if self.eat_if(TokenType::Star) {
                if had_except { self.diag_at_peek("cannot have both 'except' and 'except*' on the same 'try'"); }
                self.except_star(&mut end_jumps);
                had_except = true;
                break;
            }
            had_except = true;
            if let Some(j) = next_arm_jump.take() { self.patch(j); }
            if had_bare {
                self.error("default 'except:' must be last");
                break;
            }

            // The header runs code of its own, so an error in its type reports its line.
            self.mark_stmt();
            let mut as_name: Option<&str> = None;
            if matches!(self.peek(), Some(TokenType::Colon)) {
                had_bare = true;
                self.chunk.emit(OpCode::PopTop, 0);
            } else {
                self.chunk.emit(OpCode::Dup, 0);
                self.expr();
                let isinst_pos = self.last_end as u32;
                self.chunk.emit(OpCode::CallIsInstance, 2);
                self.chunk.record_call_pos(isinst_pos);
                next_arm_jump = Some(self.emit_jump(OpCode::JumpIfFalse));

                if self.eat_if(TokenType::As) {
                    let n = self.advance_text();
                    self.store_name(n);
                    as_name = Some(n);
                } else { self.chunk.emit(OpCode::PopTop, 0); }
            }
            self.eat(TokenType::Colon);
            self.compile_block();
            // Python implicitly unbinds the `except ... as e` name after the block.
            if let Some(n) = as_name {
                let idx = self.push_ssa_name(n, self.current_version(n));
                self.chunk.emit(OpCode::Del, idx);
            }

            // Handled arms jump past the `else` block, a bare last arm just falls through.
            let more = matches!(
                self.peek(),
                Some(TokenType::Except | TokenType::Else | TokenType::Finally)
            );
            if !had_bare || more {
                end_jumps.push(self.emit_jump(OpCode::Jump));
            }
        }

        if let Some(j) = next_arm_jump {
            self.patch(j);
            self.chunk.emit(OpCode::Raise, 0);
        } else if !had_except {
            // Pure try/finally, the handler just re-raises so the outer finally still runs.
            self.chunk.emit(OpCode::Raise, 0);
        }

        // Success path falls into `else`, handled-exception arms skip it.
        self.patch(success_jump);
        if self.eat_if(TokenType::Else) {
            self.eat(TokenType::Colon);
            self.compile_block();
        }
        for j in end_jumps {
            self.patch(j);
        }

        // Normal path pops the frame and marks the entry, unwind exits jump straight to the body.
        self.cleanup_count -= 1;
        self.chunk.emit(OpCode::PopExcept, 0);
        self.chunk.emit(OpCode::BeginFinally, 0);
        let fin_body = self.chunk.instructions.len() as u16;
        self.patch_to(fin_setup, fin_body);
        if self.eat_if(TokenType::Finally) {
            self.eat(TokenType::Colon);
            let outer = self.finally_loops.replace(self.loops.len() as u16);
            self.compile_block();
            self.finally_loops = outer;
        }
        self.chunk.emit(OpCode::EndFinally, 0);

        self.commit_block();
    }

    /* `break` or `continue` leaving the `except*` body it sits in, which Python refuses. */
    pub(super) fn leaves_star(&mut self) {
        if self.star_loops == Some(self.loops.len()) { self.error("'break', 'continue' and 'return' cannot appear in an except* block"); }
    }

    /* The `except*` clauses of a try, the first `except*` eaten, the caught exception on the stack. Each clause takes the part of the group it matches, its body runs with that part as the exception it handles, and what stays unhandled or was raised is raised at the end. */
    fn except_star(&mut self, end_jumps: &mut Vec<usize>) {
        let n = self.star_count;
        self.star_count += 1;
        let hidden = |what: &str| crate::s!("#", str what, int n as i64);
        let (state, part, raised, rest) = (hidden("eg"), hidden("part"), hidden("raised"), hidden("rest"));
        let call = |s: &mut Self, argc: u16| { let at = s.last_end as u32; s.chunk.emit(OpCode::Call, argc); s.chunk.record_call_pos(at); };
        self.emit_load_ssa("%star_start");
        self.chunk.emit(OpCode::Swap, 0);
        call(self, 1);
        self.store_name(&state);
        let mut first = true;
        while first || self.eat_if(TokenType::Except) {
            if !first && !self.eat_if(TokenType::Star) { self.diag_at_peek("cannot have both 'except' and 'except*' on the same 'try'"); }
            first = false;
            self.mark_stmt();
            self.emit_load_ssa("%star_split");
            self.emit_load_ssa(&state);
            if matches!(self.peek(), Some(TokenType::Colon)) { self.diag_at_peek("expected one or more exception types"); self.chunk.emit(OpCode::LoadNone, 0); }
            else { self.expr(); }
            call(self, 2);
            self.store_name(&part);
            let as_name = if self.eat_if(TokenType::As) { Some(self.advance_text()) } else { None };
            self.eat(TokenType::Colon);
            self.emit_load_ssa(&part);
            let skip = self.emit_jump(OpCode::JumpIfFalse);
            // The part is raised and caught, so a bare `raise` in the body raises it again.
            let outer = self.emit_jump(OpCode::SetupExcept);
            let catch = self.emit_jump(OpCode::SetupExcept);
            self.emit_load_ssa(&part);
            self.chunk.emit(OpCode::Raise, 0);
            self.patch(catch);
            match as_name { Some(name) => self.store_name(name), None => self.chunk.emit(OpCode::PopTop, 0) }
            let guard = self.emit_jump(OpCode::SetupExcept);
            let saved = self.star_loops.replace(self.loops.len());
            self.compile_block();
            self.star_loops = saved;
            self.chunk.emit(OpCode::PopExcept, 0);
            let done = self.emit_jump(OpCode::Jump);
            // What the body raises is kept for the end.
            self.patch(guard);
            self.store_name(&raised);
            self.emit_load_ssa("%star_raised");
            self.emit_load_ssa(&state);
            self.emit_load_ssa(&raised);
            self.emit_load_ssa(&part);
            call(self, 3);
            self.chunk.emit(OpCode::PopTop, 0);
            self.patch(done);
            self.chunk.emit(OpCode::PopExcept, 0);
            if let Some(name) = as_name {
                let idx = self.push_ssa_name(name, self.current_version(name));
                self.chunk.emit(OpCode::Del, idx);
            }
            let over = self.emit_jump(OpCode::Jump);
            // Bookkeeping that fails leaves through the outer frame.
            self.patch(outer);
            self.chunk.emit(OpCode::Raise, 1);
            self.patch(over);
            self.patch(skip);
        }
        self.emit_load_ssa("%star_end");
        self.emit_load_ssa(&state);
        call(self, 1);
        self.store_name(&rest);
        self.emit_load_ssa(&rest);
        let clean = self.emit_jump(OpCode::JumpIfFalse);
        self.emit_load_ssa(&rest);
        self.chunk.emit(OpCode::Raise, 2);
        self.patch(clean);
        end_jumps.push(self.emit_jump(OpCode::Jump));
    }

    /* with / async with, each CM is a SetupFinally cleanup frame whose handler stages `WithExit`. Dunders run as staged plain Calls so host deferrals can park. */

    pub(super) fn with_stmt_inner(&mut self, is_async: bool) {
        self.advance();
        let operand = is_async as u16;
        let mut setups: Vec<usize> = Vec::new();
        // `with (a as x, b as y):` puts its items in brackets.
        let grouped = matches!(self.peek_same_line(), Some(TokenType::Lpar)) && self.tokens.group_then(TokenType::Colon);
        if grouped { self.advance(); }
        loop {
            self.expr();
            self.chunk.emit(OpCode::WithEnter, operand);
            self.chunk.emit(OpCode::Call, 1);
            if self.eat_if(TokenType::As) {
                self.store_target();
            } else {
                // Discard the unbound `__enter__` result.
                self.chunk.emit(OpCode::PopTop, 0);
            }
            setups.push(self.emit_jump(OpCode::SetupFinally));
            self.cleanup_count += 1;
            let more = self.eat_if(TokenType::Comma);
            if grouped && (!more || matches!(self.peek(), Some(TokenType::Rpar))) { self.eat(TokenType::Rpar); break; }
            if !more { break; }
        }
        self.eat(TokenType::Colon);
        self.compile_block();

        // Cleanups innermost-first, normal path pops the frame and marks the entry, then runs `__exit__`.
        for s in setups.into_iter().rev() {
            self.cleanup_count -= 1;
            self.chunk.emit(OpCode::PopExcept, 0);
            self.chunk.emit(OpCode::BeginFinally, 0);
            let h = self.chunk.instructions.len() as u16;
            self.patch_to(s, h);
            self.chunk.emit(OpCode::WithExit, operand);
            self.chunk.emit(OpCode::Call, 4);
            self.chunk.emit(OpCode::WithJudge, 0);
            self.chunk.emit(OpCode::EndFinally, 0);
        }
    }

}
