use crate::s;

use super::Parser;
use crate::code::OpCode;

use crate::syntax::lexer::{Spelling, Token, TokenType};

use alloc::{vec::Vec, string::ToString};

/* positions of `kind` outside any bracket in `toks` */
fn depth0(toks: &[Token], kind: TokenType) -> Vec<usize> {
    let mut depth = 0i32;
    toks.iter().enumerate().filter_map(|(i, t)| {
        match t.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth -= 1,
            _ => {}
        }
        (depth == 0 && t.kind == kind).then_some(i)
    }).collect()
}

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

    /* each case tests the subject against its pattern and guard then jumps to the end */
    pub(super) fn match_stmt(&mut self) {
        self.advance();
        self.header_test(Self::expr);

        let ver = self.increment_version(crate::syntax::ssa::SSA_TMP_MATCH);
        let subj = self.chunk.push_name(&s!(str crate::syntax::ssa::SSA_TMP_MATCH, int ver));
        self.chunk.emit(OpCode::StoreName, subj);

        if Self::DELIMITED { self.eat(TokenType::BlockOpen); } else {
            self.eat(TokenType::Colon);
            let indented = self.eat_if(TokenType::Indent);
            // arms end where their block dedents so they need one
            if Self::SHAPES.arms && !indented { return self.error("expected an indented block"); }
        }

        let mut end_jumps = Vec::new();

        while self.at_case() {
            let start = self.tokens.pos;
            if !Self::SHAPES.arms { self.advance(); }

            let mut fail_jumps: Vec<usize> = Vec::new();
            self.parse_pattern(subj, &mut fail_jumps);

            // a failed guard joins the pattern failures at the next case
            if self.eat_if(TokenType::If) {
                self.expr();
                fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
            }

            if Self::SHAPES.arms {
                self.eat(TokenType::Arm);
                self.arm_body();
            } else {
                self.header_end();
                self.compile_block();
            }

            // the last case falls through to the end
            if self.at_case() { end_jumps.push(self.emit_jump(OpCode::Jump)); }

            for j in fail_jumps { self.patch(j); }
            // an arm that took nothing skips a token so the loop always ends
            if Self::SHAPES.arms && self.tokens.pos == start { self.tokens.next(); }
        }

        // the dedent that ends the cases completes the statement as a block's own does
        if !Self::DELIMITED { if self.eat_if(TokenType::Dedent) { self.block_closed = true; } } else if !self.eat_if(TokenType::BlockClose) {
            // anything but a case reports once and the rest of the block is skipped
            self.eat(TokenType::BlockClose);
            self.skip_block();
        }

        for pos in end_jumps { self.patch(pos); }
    }

    /* whether a case starts here or with arms anything but the end of the match */
    fn at_case(&mut self) -> bool {
        if Self::SHAPES.arms { return !matches!(self.peek(), None | Some(TokenType::Dedent | TokenType::BlockClose)); }
        matches!(self.peek(), Some(TokenType::Case))
    }

    /* an arm's body as a block or with delimited blocks the statements on its line */
    fn arm_body(&mut self) {
        if !Self::DELIMITED || self.peek_same_line().is_none_or(|k| k == TokenType::BlockOpen) { return self.compile_block(); }
        self.block_body(false);
    }

    /* the expression of a header inside the parentheses a table may want around it */
    fn header_test(&mut self, f: impl FnOnce(&mut Self)) {
        if !Self::SHAPES.header_parens { return f(self); }
        self.eat(TokenType::ParenOpen);
        f(self);
        self.eat(TokenType::ParenClose);
    }

    /* one case pattern which a top-level comma or star turns into a sequence */
    pub(super) fn parse_pattern(&mut self, subj: u16, fail_jumps: &mut Vec<usize>) {
        let end = if Self::SHAPES.arms { TokenType::Arm } else { Self::OPEN };
        let toks = self.pattern_tokens(Vec::new(), |k| k == end || k == TokenType::If);
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

    /* buffers tokens to `end` at depth 0 so a sequence counts its items before emitting */
    fn pattern_tokens(&mut self, mut toks: Vec<crate::syntax::lexer::Token>, end: impl Fn(TokenType) -> bool) -> Vec<crate::syntax::lexer::Token> {
        let mut depth = 0i32;
        while let Some(k) = self.peek_same_line() {
            if depth == 0 && end(k) { break; }
            match k {
                TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
                TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth -= 1,
                _ => {}
            }
            toks.push(self.advance());
        }
        toks
    }

    /* checks the length then matches each item with a star taking the middle slice */
    fn sequence_items(&mut self, toks: &[crate::syntax::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        // items split at depth 0 commas where a trailing comma adds none
        let mut items: Vec<(bool, usize, usize)> = Vec::new();
        let mut lo = 0;
        for hi in depth0(toks, TokenType::Comma).into_iter().chain(core::iter::once(toks.len())) {
            if lo < hi {
                let star = Self::SPLAT && toks[lo].kind == TokenType::Star;
                items.push((star, lo + star as usize, hi));
            }
            lo = hi + 1;
        }
        let stars = items.iter().filter(|it| it.0).count();
        if stars > 1 {
            self.error_at(toks[0].start, toks.last().unwrap().end, "multiple stars in sequence pattern");
        }
        let n = items.len() as i64;

        // a subject that is no sequence fails the pattern instead of erroring on its length
        self.chunk.emit(OpCode::LoadName, subj);
        self.chunk.emit(OpCode::MatchSeq, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));

        // the length must match exactly without a star and cover the other items with one
        self.chunk.emit(OpCode::LoadName, subj);
        self.chunk.emit(OpCode::Len, 0);
        self.emit_const(crate::syntax::chunk::Value::Int(n - (stars > 0) as i64));
        self.chunk.emit(if stars > 0 { OpCode::GtEq } else { OpCode::Eq }, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));

        let item_subj = self.pattern_slot();

        let mut seen_star = false;
        for (k, &(star, lo, hi)) in items.iter().enumerate() {
            let k = k as i64;
            if star {
                seen_star = true;
                // the star takes the slice between the leading and trailing items as a list
                self.chunk.emit(OpCode::BuildList, 0);
                self.chunk.emit(OpCode::LoadName, subj);
                self.emit_const(crate::syntax::chunk::Value::Int(k));
                self.chunk.emit(OpCode::LoadName, subj);
                self.chunk.emit(OpCode::Len, 0);
                self.emit_const(crate::syntax::chunk::Value::Int(n - k - 1));
                self.chunk.emit(OpCode::Sub, 0);
                self.chunk.emit(OpCode::Cut, 3);
                self.chunk.emit(OpCode::ListExtend, 0);
            } else {
                // items after the star index from the end
                self.chunk.emit(OpCode::LoadName, subj);
                self.emit_const(crate::syntax::chunk::Value::Int(if seen_star { k - n } else { k }));
                self.chunk.emit(OpCode::GetItem, 0);
            }
            self.chunk.emit(OpCode::StoreName, item_subj);
            self.sub_pattern(&toks[lo..hi], item_subj, fail_jumps);
        }
    }

    /* brackets make a sequence but parentheses without a comma or star only group */
    fn bracket_pattern(&mut self, paren: bool, inner: &[crate::syntax::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        let group = paren && !inner.is_empty()
            && depth0(inner, TokenType::Comma).is_empty() && depth0(inner, TokenType::Star).is_empty();
        if group { self.sub_pattern(inner, subj, fail_jumps); } else { self.sequence_items(inner, subj, fail_jumps); }
    }

    fn sub_pattern(&mut self, toks: &[crate::syntax::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        self.nested(|s| s.one_pattern(toks, subj, fail_jumps));
    }

    /* matches one buffered pattern of any kind against `subj` */
    fn one_pattern(&mut self, toks: &[crate::syntax::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        // `p as name` binds the subject only after `p` with its alternatives matched
        if let Some(&at) = depth0(toks, TokenType::As).last() {
            // a bare `as` names no pattern to bind after
            if at == 0 { return self.error_at(toks[0].start, toks[0].end, "expected a pattern"); }
            let [t] = &toks[at + 1..] else { return self.error_at(toks[at].start, toks.last().unwrap().end, "invalid pattern target") };
            if t.kind != TokenType::Name { return self.error_at(t.start, t.end, "invalid pattern target"); }
            self.sub_pattern(&toks[..at], subj, fail_jumps);
            let name = &self.source[t.start..t.end];
            self.chunk.emit(OpCode::LoadName, subj);
            self.emit_store_new(name);
            return;
        }
        // each alternative of `p | q` sends its failure on to the next one
        let bars = depth0(toks, TokenType::BitOr);
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
                (TokenType::BracketOpen, TokenType::BracketClose) | (TokenType::ParenOpen, TokenType::ParenClose) =>
                    return self.bracket_pattern(first.kind == TokenType::ParenOpen, &toks[1..toks.len() - 1], subj, fail_jumps),
                (TokenType::BraceOpen, TokenType::BraceClose) => return self.mapping_pattern(&toks[1..toks.len() - 1], subj, fail_jumps),
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
        // `a.b.c` is a value pattern and `a.b(...)` a class pattern
        let path = toks.iter().enumerate().take_while(|&(i, t)| t.kind == if i % 2 == 0 { TokenType::Name } else { TokenType::Dot }).count();
        if path % 2 == 1 && path == toks.len() {
            self.chunk.emit(OpCode::LoadName, subj);
            self.emit_dotted(&toks[..path], false);
            self.chunk.emit(OpCode::Eq, 0);
            fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
            return;
        }
        if path % 2 == 1 && toks.get(path).is_some_and(|t| t.kind == TokenType::ParenOpen) && toks.last().is_some_and(|t| t.kind == TokenType::ParenClose) {
            return self.class_pattern(&toks[..path], &toks[path + 1..toks.len() - 1], subj, fail_jumps);
        }
        let Some(v) = self.pattern_literal(toks) else {
            self.error_at(toks[0].start, toks.last().unwrap().end, "unsupported pattern (use literals, names, _, sequences, mappings or classes)");
            return;
        };
        // `None` and booleans match by identity so `case True` rejects 1
        let op = if matches!(v, crate::syntax::chunk::Value::Bool(_) | crate::syntax::chunk::Value::None) { OpCode::Is } else { OpCode::Eq };
        self.chunk.emit(OpCode::LoadName, subj);
        self.emit_const(v);
        self.chunk.emit(op, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
    }

    /* a number after an optional minus or joined strings or bytes or a singleton */
    fn pattern_literal(&self, toks: &[crate::syntax::lexer::Token]) -> Option<crate::syntax::chunk::Value> {
        use crate::syntax::{chunk::Value, text::{parse_string, parse_bytes_literal}};
        let neg = toks.first()?.kind == TokenType::Minus;
        let toks = &toks[neg as usize..];
        let text = |t: &crate::syntax::lexer::Token| &self.source[t.start..t.end];
        let kind = toks.first()?.kind;
        // adjacent string or bytes literals join into one as in an expression
        if !neg && matches!(kind, TokenType::String | TokenType::Bytes) && toks.iter().all(|t| t.kind == kind) && (toks.len() == 1 || S::SYNTAX.strings.adjacent) {
            return Some(if kind == TokenType::String { Value::Str(toks.iter().map(|t| parse_string::<S>(text(t))).collect()) }
                else { Value::Bytes(toks.iter().flat_map(|t| parse_bytes_literal::<S>(text(t))).collect()) });
        }
        let [t] = toks else { return None };
        Some(match (t.kind, neg) {
            (TokenType::Int, _) => match Self::int_literal(text(t)).ok()? {
                Value::Int(i) if neg => Value::Int(-i),
                Value::Wide { neg: n, base, digits } if neg => Value::Wide { neg: !n, base, digits },
                v => v,
            },
            (TokenType::Float, _) => {
                let f = Self::float_literal(text(t))?;
                Value::Float(if neg { -f } else { f })
            }
            (TokenType::True, false) => Value::Bool(true),
            (TokenType::False, false) => Value::Bool(false),
            (TokenType::Nil, false) => Value::None,
            _ => return None,
        })
    }

    /* loads a dotted name like `Color.RED` where `call` keeps the last name an attribute */
    fn emit_dotted(&mut self, path: &[crate::syntax::lexer::Token], call: bool) {
        self.emit_load_ssa(&self.source[path[0].start..path[0].end]);
        for (i, t) in path.iter().enumerate().skip(2).step_by(2) {
            // a table that reads keys at dots reads one here as an expression would
            if Self::KEYS && !(call && i + 1 == path.len()) {
                self.emit_const(crate::syntax::chunk::Value::Str(self.source[t.start..t.end].to_string()));
                self.chunk.emit(OpCode::GetItem, 0);
                continue;
            }
            let idx = self.chunk.push_name(&self.source[t.start..t.end]);
            self.chunk.emit(OpCode::LoadAttr, idx);
        }
    }

    /* a fresh slot for a value a sub-pattern matches against */
    fn pattern_slot(&mut self) -> u16 {
        let ver = self.increment_version(crate::syntax::ssa::SSA_TMP_MATCH_ITEM);
        self.chunk.push_name(&s!(str crate::syntax::ssa::SSA_TMP_MATCH_ITEM, int ver))
    }

    /* a class pattern asks `MatchClass` for the attribute values then matches each sub-pattern */
    fn class_pattern(&mut self, path: &[crate::syntax::lexer::Token], inner: &[crate::syntax::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        let (mut positional, mut keywords) = (Vec::new(), Vec::new());
        let mut lo = 0;
        for hi in depth0(inner, TokenType::Comma).into_iter().chain(core::iter::once(inner.len())) {
            let item = &inner[lo..hi];
            match item {
                [] => {}
                [k, eq, rest @ ..] if k.kind == TokenType::Name && eq.kind == Self::SHAPES.keyword_arg => keywords.push((&self.source[k.start..k.end], rest)),
                _ => positional.push(item),
            }
            lo = hi + 1;
        }
        self.chunk.emit(OpCode::LoadName, subj);
        self.emit_dotted(path, true);
        for &(k, _) in &keywords { self.emit_const(crate::syntax::chunk::Value::Str(k.to_string())); }
        self.chunk.emit(OpCode::BuildTuple, keywords.len() as u16);
        self.chunk.emit(OpCode::MatchClass, positional.len() as u16);
        let values = self.pattern_slot();
        self.chunk.emit(OpCode::StoreName, values);
        self.chunk.emit(OpCode::LoadName, values);
        self.chunk.emit(OpCode::LoadNone, 0);
        self.chunk.emit(OpCode::IsNot, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
        let subs: Vec<&[crate::syntax::lexer::Token]> = positional.into_iter().chain(keywords.into_iter().map(|(_, p)| p)).collect();
        for (i, sub) in subs.into_iter().enumerate() {
            self.chunk.emit(OpCode::LoadName, values);
            self.emit_const(crate::syntax::chunk::Value::Int(i as i64));
            self.chunk.emit(OpCode::GetItem, 0);
            let item = self.pattern_slot();
            self.chunk.emit(OpCode::StoreName, item);
            self.sub_pattern(sub, item, fail_jumps);
        }
    }

    /* a mapping pattern wants a dict with every literal key and each value matching */
    fn mapping_pattern(&mut self, inner: &[crate::syntax::lexer::Token], subj: u16, fail_jumps: &mut Vec<usize>) {
        self.chunk.emit(OpCode::LoadName, subj);
        self.chunk.emit(OpCode::MatchMap, 0);
        fail_jumps.push(self.emit_jump(OpCode::JumpIfFalse));
        let (mut keys, mut rest) = (Vec::new(), None);
        let mut lo = 0;
        for hi in depth0(inner, TokenType::Comma).into_iter().chain(core::iter::once(inner.len())) {
            let item = &inner[lo..hi];
            lo = hi + 1;
            if item.is_empty() { continue; }
            if let [star, name] = item && Self::SPLAT && star.kind == TokenType::Pow {
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
        // `**rest` binds a copy without the matched keys
        if let Some(name) = rest {
            self.chunk.emit(OpCode::BuildDict, 0);
            self.chunk.emit(OpCode::LoadName, subj);
            self.chunk.emit(OpCode::DictUpdate, 0);
            let slot = self.emit_store_new(&name);
            let load = if self.globals_decl.contains(&name) { OpCode::LoadGlobal } else { OpCode::LoadName };
            for ki in keys {
                self.chunk.emit(load, slot);
                self.chunk.emit(OpCode::LoadConst, ki);
                self.chunk.emit(OpCode::DelItem, 0);
            }
        }
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
            let mut as_name: Option<&str> = None;
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
                    as_name = Some(n);
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
            // the `as` name of a handler is unbound after its block
            if let Some(n) = as_name {
                let idx = self.push_ssa_name(n, self.current_version(n));
                self.chunk.emit(OpCode::Del, idx);
            }

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
