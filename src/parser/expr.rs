
use super::Parser;
use super::types::{OpCode, Value, MAX_EXPR_DEPTH, Instruction};

use super::types::{parse_string, parse_bytes_literal};
use crate::lexer::{Token, TokenType};

use alloc::{string::ToString, vec::Vec, string::String};

/* What ends a trailer chain, in an expression, a statement or an assignment target. */
#[derive(Clone, Copy)]
pub(super) enum Tail { Expr, Stmt, Target }

/* The comparison operators, all at one precedence and all chaining, `not` here only opens `not in`. */
fn is_comparison(tok: TokenType) -> bool {
    matches!(tok, TokenType::EqEqual | TokenType::NotEqual | TokenType::Less | TokenType::Greater
        | TokenType::LessEqual | TokenType::GreaterEqual | TokenType::In | TokenType::Is | TokenType::Not)
}

impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {

    /* Entry, Pratt parse, then optional ternary. Recursion is bounded inside `expr_bp`. */
    pub(super) fn expr(&mut self) {
        self.saw_newline = false;
        let val_start = self.chunk.instructions.len();
        self.expr_bp(0);
        self.ternary_tail(val_start);
    }

    /* Ternary, value was emitted first (single-pass), so reorder `[value][cond]` into `[cond][JumpIfFalse][value][Jump][else]` for Python evaluation order. */
    pub(super) fn ternary_tail(&mut self, val_start: usize) {
        if self.saw_newline || !matches!(self.peek_same_line(), Some(TokenType::If)) { return; }
        self.advance();
        let cond_start = self.chunk.instructions.len();
        self.expr_bp(0);

        let cond_ins: Vec<Instruction> = self.chunk.instructions.drain(cond_start..).collect();
        let val_ins: Vec<Instruction> = self.chunk.instructions.drain(val_start..).collect();
        let calls_from = self.chunk.call_byte_pos.partition_point(|&(ip, _)| (ip as usize) < val_start);
        let cond_delta = val_start as i64 - cond_start as i64;

        self.push_shifted(cond_ins, cond_delta);
        let jf = self.emit_jump(OpCode::JumpIfFalse);
        let val_delta = self.chunk.instructions.len() as i64 - val_start as i64;
        self.push_shifted(val_ins, val_delta);

        // Remap recorded call ips, re-sort so resolve_call's binary search holds.
        for e in &mut self.chunk.call_byte_pos[calls_from..] {
            let d = if (e.0 as usize) < cond_start { val_delta } else { cond_delta };
            e.0 = (e.0 as i64 + d) as u32;
        }
        self.chunk.call_byte_pos[calls_from..].sort_unstable_by_key(|&(ip, _)| ip);

        let jmp = self.emit_jump(OpCode::Jump);
        self.patch(jf);
        self.eat(TokenType::Else);
        // A conditional chained in the else-branch nests one level deeper.
        self.nested(Self::expr);
        self.patch(jmp);
    }

    /* Re-append drained instructions, shifting internal jump targets by `delta`. */
    pub(super) fn push_shifted(&mut self, ins: Vec<Instruction>, delta: i64) {
        for i in ins {
            let operand = if i.opcode.is_jump() { (i.operand as i64 + delta) as u16 } else { i.operand };
            self.chunk.instructions.push(Instruction { opcode: i.opcode, operand });
        }
    }

    pub(super) fn expr_tails(&mut self, start: usize) {
        self.postfix_tail(Tail::Expr);
        self.infix_bp(0, start);
        self.ternary_tail(start);
    }

    /* Pratt parser, unary prefix then infix loop via `binding_power` table. Bounds every recursive descent (prefix `-`/`+`/`~`/`await`/`not`, right-associative `**`, infix right operands) so deep chains raise instead of overflowing the native/WASM stack. */
    pub(super) fn expr_bp(&mut self, min_bp: u8) {
        self.expr_depth += 1;
        if self.expr_depth > MAX_EXPR_DEPTH {
            self.expr_depth -= 1;
            self.error("expression too deeply nested");
            return;
        }
        let start = self.chunk.instructions.len();
        match self.peek() {
            Some(TokenType::Not) => {
                self.advance();
                self.expr_bp(5);
                self.chunk.emit(OpCode::Not, 0);
            }
            _ => self.parse_unary(),
        }
        self.infix_bp(min_bp, start);
        self.expr_depth -= 1;
    }

    // An operator never continues past the logical line, so `@deco` on the next line stays a decorator, `left` is where the first operand starts.
    pub(super) fn infix_bp(&mut self, min_bp: u8, mut left: usize) {
        // The open jumps of a comparison chain, each operand pointing at the jump before, as a patch list.
        let mut chain = u16::MAX;
        while let Some(tok) = self.peek_same_line() {
            if is_comparison(tok) {
                if 7 < min_bp { break; }
                let op_start = self.tokens.peek().map_or(self.last_end, |t| t.start);
                let op = self.comparison_op();
                let right = self.chunk.instructions.len();
                self.expr_bp(8);
                if matches!(op, OpCode::Is | OpCode::IsNot) && !self.singleton(left, right) && !self.singleton(right, self.chunk.instructions.len()) {
                    self.reject(op_start, self.last_end, "'is' compares only with None, True, False, ... or NotImplemented, compare other values with '=='");
                }
                // `a < b in c` tests `b` again and stops at the first false, the tail holds no `and` or `or`.
                if self.peek_same_line().is_some_and(is_comparison) {
                    let ver = self.increment_version(super::SSA_TMP_CMP);
                    let tmp = self.push_ssa_name(super::SSA_TMP_CMP, ver);
                    self.chunk.emit(OpCode::StoreName, tmp);
                    self.chunk.emit(OpCode::LoadName, tmp);
                    self.chunk.emit(op, 0);
                    let jmp = self.emit_jump(OpCode::JumpIfFalseOrPop);
                    if let Some(i) = self.chunk.instructions.get_mut(jmp) { i.operand = chain; }
                    chain = jmp as u16;
                    left = self.chunk.instructions.len();
                    self.chunk.emit(OpCode::LoadName, tmp);
                } else {
                    self.chunk.emit(op, 0);
                    while chain != u16::MAX {
                        let jmp = chain as usize;
                        chain = self.chunk.instructions.get(jmp).map_or(u16::MAX, |i| i.operand);
                        self.patch(jmp);
                    }
                }
                continue;
            }

            let Some((l_bp, r_bp, op)) = Self::binding_power(&tok) else { break };
            if l_bp < min_bp { break; }
            self.advance();

            if matches!(op, OpCode::And | OpCode::Or) {
                let jump_op = if op == OpCode::And { OpCode::JumpIfFalseOrPop } else { OpCode::JumpIfTrueOrPop };
                let jmp = self.emit_jump(jump_op);
                self.expr_bp(r_bp);
                self.patch(jmp);
                continue;
            }

            self.expr_bp(r_bp);
            self.chunk.emit(op, 0);
        }
    }

    /* Whether the code in `from..to` loads one singleton, the only operand whose identity `is` can answer. */
    fn singleton(&self, from: usize, to: usize) -> bool {
        let [i] = self.chunk.instructions[from..to] else { return false };
        match i.opcode {
            OpCode::LoadNone | OpCode::LoadTrue | OpCode::LoadFalse | OpCode::LoadEllipsis => true,
            OpCode::LoadConst => matches!(self.chunk.constants.get(i.operand as usize), Some(Value::Bool(_) | Value::None)),
            OpCode::LoadName => self.chunk.names.get(i.operand as usize).is_some_and(|n| super::types::ssa_strip(n) == "NotImplemented"),
            _ => false,
        }
    }

    /* Consumes one comparison operator, `not in` and `is not` included. */
    fn comparison_op(&mut self) -> OpCode {
        match self.advance().kind {
            TokenType::Is if self.eat_if(TokenType::Not) => OpCode::IsNot,
            TokenType::Is => OpCode::Is,
            TokenType::Not => { self.eat(TokenType::In); OpCode::NotIn }
            TokenType::In => OpCode::In,
            TokenType::EqEqual => OpCode::Eq,
            TokenType::NotEqual => OpCode::NotEq,
            TokenType::Less => OpCode::Lt,
            TokenType::Greater => OpCode::Gt,
            TokenType::LessEqual => OpCode::LtEq,
            _ => OpCode::GtEq,
        }
    }

    pub(super) fn binding_power(tok: &TokenType) -> Option<(u8, u8, OpCode)> {
        match tok {
            TokenType::Or => Some((1, 2, OpCode::Or)),
            TokenType::And => Some((3, 4, OpCode::And)),
            TokenType::Vbar => Some((9, 10, OpCode::BitOr)),
            TokenType::Circumflex => Some((11, 12, OpCode::BitXor)),
            TokenType::Amper => Some((13, 14, OpCode::BitAnd)),
            TokenType::LeftShift => Some((15, 16, OpCode::Shl)),
            TokenType::RightShift => Some((15, 16, OpCode::Shr)),
            TokenType::Plus => Some((17, 18, OpCode::Add)),
            TokenType::Minus => Some((17, 18, OpCode::Sub)),
            TokenType::Star => Some((19, 20, OpCode::Mul)),
            TokenType::Slash => Some((19, 20, OpCode::Div)),
            TokenType::Percent => Some((19, 20, OpCode::Mod)),
            TokenType::DoubleSlash => Some((19, 20, OpCode::FloorDiv)),
            TokenType::At => Some((19, 20, OpCode::MatMul)),
            TokenType::DoubleStar => Some((22, 21, OpCode::Pow)),
            _ => None,
        }
    }

    pub(super) fn parse_unary(&mut self) {
        match self.peek() {
            Some(TokenType::Minus) => {
                self.advance();
                self.expr_bp(21);
                self.chunk.emit(OpCode::Minus, 0);
            }
            Some(TokenType::Plus) => {
                // Unary plus calls `__pos__` and coerces bool to int.
                self.advance();
                self.expr_bp(21);
                self.chunk.emit(OpCode::Pos, 0);
            }
            Some(TokenType::Tilde) => {
                self.advance();
                self.expr_bp(21);
                self.chunk.emit(OpCode::BitNot, 0);
            }
            Some(TokenType::Await) => {
                self.advance();
                self.expr_bp(21);
                self.chunk.emit(OpCode::Await, 0);
            }
            _ => self.parse_atom()
        }
    }

    /* Atoms, literals, names, numbers, strings, f-strings, containers. */
    pub(super) fn parse_atom(&mut self) {
        let errs_before = self.errors.len();
        let t = self.advance();
        match t.kind {
            TokenType::Name | TokenType::Underscore => self.name(t),
            TokenType::String | TokenType::FstringStart => self.string_group(t),
            TokenType::Bytes => {
                // Adjacent bytes literals concat, mixing with str surfaces a diagnostic.
                let mut buf = parse_bytes_literal(self.lexeme(&t));
                while matches!(self.peek_same_line(), Some(TokenType::Bytes)) {
                    let t = self.advance();
                    buf.extend_from_slice(&parse_bytes_literal(self.lexeme(&t)));
                }
                self.emit_const(Value::Bytes(buf));
            }
            TokenType::Int | TokenType::Float => {
                self.parse_number(self.lexeme(&t), t.kind);
            }
            TokenType::True => self.chunk.emit(OpCode::LoadTrue, 0),
            TokenType::False => self.chunk.emit(OpCode::LoadFalse, 0),
            TokenType::None => self.chunk.emit(OpCode::LoadNone, 0),
            TokenType::Ellipsis => self.chunk.emit(OpCode::LoadEllipsis, 0),
            TokenType::Lbrace => self.brace_literal(),
            TokenType::Lsqb => self.list_literal(),
            TokenType::Lpar => self.paren_atom(),
            // `yield` / `yield from` as an expression value (keyword already consumed).
            TokenType::Yield => self.emit_yield(),
            TokenType::Lambda => self.parse_lambda(),
            // Caret at consumed token, skip if `advance()` already reported the error.
            _ => {
                if self.errors.len() == errs_before {
                    self.error_at(t.start, t.end, "expected expression");
                }
            }
        }
        self.postfix_tail(Tail::Expr);
    }

    /* After `(`, an empty tuple, a generator expression, a tuple or a parenthesized expression. */
    pub(super) fn paren_atom(&mut self) {
        if matches!(self.peek(), Some(TokenType::Rpar)) {
            self.advance();
            self.chunk.emit(OpCode::BuildTuple, 0);
        } else if self.at_comp() {
            self.comprehension(super::literals::Comp::Gen);
            self.eat(TokenType::Rpar);
        } else {
            // A leading `*it` starts a tuple.
            let star = matches!(self.peek(), Some(TokenType::Star));
            if !star { self.expr(); }
            if star || matches!(self.peek(), Some(TokenType::Comma)) {
                self.tuple_rest(!star as u16, |s| matches!(s.peek(), Some(TokenType::Rpar) | None));
            }
            self.eat(TokenType::Rpar);
        }
    }

    /* Adjacent str/f-string literals concat into one value. */
    fn string_group(&mut self, first: Token) {
        let mut parts = 0u16;
        let mut fstrings = 0u32;
        let mut lit = String::new();
        let mut tok = first;
        loop {
            if tok.kind == TokenType::String {
                lit.push_str(&parse_string(self.lexeme(&tok)));
            } else {
                // Flush pending literal text so part order holds.
                if !lit.is_empty() {
                    self.emit_const(Value::Str(core::mem::take(&mut lit)));
                    parts += 1;
                }
                parts += self.fstring(tok.start, tok.end);
                fstrings += 1;
            }
            match self.peek_same_line() {
                Some(TokenType::String | TokenType::FstringStart) => tok = self.advance(),
                _ => break,
            }
        }
        // Pure literals fold to a single constant.
        if fstrings == 0 {
            self.emit_const(Value::Str(lit));
            return;
        }
        if !lit.is_empty() {
            self.emit_const(Value::Str(lit));
            parts += 1;
        }
        if parts == 0 {
            self.emit_const(Value::Str(String::new()));
        } else {
            self.chunk.emit(OpCode::BuildString, parts);
        }
    }

    /* A name-led operand with its trailers, for callers that took the name token themselves. */
    pub(super) fn name_operand(&mut self, t: Token) {
        self.name(t);
        self.postfix_tail(Tail::Expr);
    }

    /* Name, walrus `:=`, call, or plain load. */
    pub(super) fn name(&mut self, t: Token) {
        let name = self.lexeme(&t);
        match self.peek_same_line() {
            // Walrus stores like an assignment, an enclosing `global` included, and leaves the value.
            Some(TokenType::ColonEqual) => {
                self.advance();
                self.expr();
                if self.in_comp && !self.globals_decl.contains(name) && !self.chunk.nonlocals.iter().any(|n| n == name) { self.chunk.nonlocals.push(name.to_string()); }
                self.store_name(name);
                self.emit_load_ssa(name);
            }
            Some(TokenType::Lpar) => {
                // A void call (`print(...)`) in value position must still leave a value, materialise its None.
                if !self.call(name) { self.emit_const(Value::None); }
            }
            _ => self.emit_load_ssa(name),
        }
    }

    fn parse_int_prefix(s: &str) -> (&str, u32) {
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) { (h, 16) }
        else if let Some(o) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) { (o, 8) }
        else if let Some(b) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) { (b, 2) }
        else { (s, 10) }
    }

    pub(super) fn parse_number(&mut self, raw: &str, kind: TokenType) {
        // The lexer already refused a `j` suffix, so the digits before it parse without a second error.
        let raw = raw.trim_end_matches(['j', 'J']);
        if kind == TokenType::Float {
            // A malformed float (e.g. empty exponent `1e`) is a syntax error, not 0.0.
            match Self::float_literal(raw) {
                Some(f) => self.emit_const(Value::Float(f)),
                None => self.error("invalid float literal"),
            }
            return;
        }
        match Self::int_literal(raw) {
            Ok(v) => self.emit_const(v),
            Err(m) => self.error(m),
        }
    }

    /* A float literal with `_` separators, copied only when it has one. */
    pub(super) fn float_literal(raw: &str) -> Option<f64> {
        if raw.contains('_') { raw.replace('_', "").parse().ok() } else { raw.parse().ok() }
    }

    /* An int literal in any base with `_` separators, the value or the syntax error it raises. */
    pub(super) fn int_literal(raw: &str) -> Result<Value, &'static str> {
        const TOO_LARGE: &str = "integer literal too large to represent (max ±2^127)";
        let (digits, base) = Self::parse_int_prefix(raw);
        let mut digits = digits.bytes().filter(|&b| b != b'_').peekable();
        // No leading zeros, all-zero runs still valid.
        if base == 10 && digits.peek() == Some(&b'0') && digits.clone().any(|b| b != b'0') {
            return Err("leading zeros in decimal integer literals are not permitted");
        }
        // u64 first, wasm emulates the u128 arithmetic only past it.
        let (mut narrow, mut wide, mut any) = (0u64, None::<u128>, false);
        for b in digits {
            let d = (b as char).to_digit(base).ok_or(TOO_LARGE)?;
            any = true;
            match wide {
                None => match narrow.checked_mul(base as u64).and_then(|v| v.checked_add(d as u64)) {
                    Some(v) => narrow = v,
                    None => wide = Some(narrow as u128 * base as u128 + d as u128),
                },
                Some(w) => wide = Some(w.checked_mul(base as u128).and_then(|v| v.checked_add(d as u128)).ok_or(TOO_LARGE)?),
            }
        }
        // The lexer already reported a literal without digits, so a zero stands in.
        if !any { return Ok(Value::Int(0)); }
        match wide {
            None if narrow <= i64::MAX as u64 => Ok(Value::Int(narrow as i64)),
            None => Ok(Value::LongInt(narrow as i128)),
            Some(w) => i128::try_from(w).map(Value::LongInt).map_err(|_| TOO_LARGE),
        }
    }

    /* Subscript after `[`, comma-separated items build one tuple key. Eats the closing `]`, returns true for a lone slice. */
    pub(super) fn parse_subscript(&mut self) -> bool {
        let slice = self.subscript_item();
        if !matches!(self.peek(), Some(TokenType::Comma)) {
            self.eat(TokenType::Rsqb);
            return slice;
        }
        let mut n = 1u16;
        while self.eat_if(TokenType::Comma) && !matches!(self.peek(), Some(TokenType::Rsqb)) {
            self.subscript_item();
            n += 1;
        }
        self.eat(TokenType::Rsqb);
        self.chunk.emit(OpCode::BuildTuple, n);
        false
    }

    /* One index or `a:b:c` slice with None defaults, emits BuildSlice and returns true for a slice. */
    #[inline]
    fn subscript_item(&mut self) -> bool {
        if matches!(self.peek(), Some(TokenType::Colon)) {
            self.chunk.emit(OpCode::LoadNone, 0);
        } else {
            self.expr();
        }
        if !self.eat_if(TokenType::Colon) { return false; }
        self.slice_bound();
        let parts = if self.eat_if(TokenType::Colon) { self.slice_bound(); 3 } else { 2 };
        self.chunk.emit(OpCode::BuildSlice, parts);
        true
    }

    // An omitted bound loads None.
    fn slice_bound(&mut self) {
        if matches!(self.peek(), Some(TokenType::Colon | TokenType::Comma | TokenType::Rsqb)) {
            self.chunk.emit(OpCode::LoadNone, 0);
        } else {
            self.expr();
        }
    }

    /* Same-line `.attr`, `[i]` and `(args)` trailers, true when the chain ends in a store. */
    pub(super) fn postfix_tail(&mut self, tail: Tail) -> bool {
        loop {
            match self.peek_same_line() {
                Some(TokenType::Lsqb) => {
                    self.advance();
                    self.parse_subscript();
                    if self.store_trailer(tail, None) { return true; }
                    self.chunk.emit(OpCode::GetItem, 0);
                }
                Some(TokenType::Dot) => {
                    self.advance();
                    // Inside brackets the name may sit on the line after the dot, a statement end leaves none.
                    if self.peek_same_line().is_none() { self.diag_at_peek("expected an attribute name after '.'"); return false; }
                    let t = self.advance();
                    let source = self.source;
                    let name = &source[t.start..t.end];
                    if self.store_trailer(tail, Some(name)) { return true; }
                    // LoadAttr adjacent to Call lets `fuse_method_calls` collapse them.
                    let idx = self.chunk.push_name(name);
                    self.chunk.emit(OpCode::LoadAttr, idx);
                }
                Some(TokenType::Lpar) => {
                    // Call after any trailer.
                    let call_pos = self.last_end as u32;
                    self.advance();
                    self.call_rest(call_pos);
                }
                _ => return false,
            }
        }
    }

    /* Ends a trailer in a store, at a target end or by a statement `op=` or annotation. */
    fn store_trailer(&mut self, tail: Tail, attr: Option<&str>) -> bool {
        let (load, store) = if attr.is_some() { (OpCode::LoadAttr, OpCode::StoreAttr) } else { (OpCode::GetItem, OpCode::StoreItem) };
        match tail {
            Tail::Expr => return false,
            Tail::Target => {
                if !self.at_target_end() { return false; }
                // The value compiled first, so it moves above the object and key.
                self.chunk.emit(if attr.is_some() { OpCode::Swap } else { OpCode::Rot3 }, 0);
            }
            Tail::Stmt if matches!(self.peek_same_line(), Some(TokenType::Colon)) => {
                self.advance();
                // A bare annotation evaluates the target and stores nothing.
                self.skip_annotation();
                self.chunk.emit(OpCode::PopTop, 0);
                if attr.is_none() { self.chunk.emit(OpCode::PopTop, 0); }
                return true;
            }
            Tail::Stmt => {
                let Some(op) = self.peek_same_line().and_then(|t| Self::augmented_op(&t)) else { return false };
                self.advance();
                let idx = attr.map_or(0, |a| self.chunk.push_name(a));
                self.chunk.emit(if attr.is_some() { OpCode::Dup } else { OpCode::Dup2 }, 0);
                self.chunk.emit(load, idx);
                self.rhs_tuple();
                self.emit_inplace(op);
            }
        }
        let idx = attr.map_or(0, |a| self.chunk.push_name(a));
        self.chunk.emit(store, idx);
        true
    }

    /* lambda, fresh chunk, compiles body to Return, emits MakeFunction. */
    pub(super) fn parse_lambda(&mut self) {
        let (params, defaults) = self.param_list(TokenType::Colon, false);
        self.eat(TokenType::Colon);
        let body = self.expr_body(&params);
        self.push_function(params, body, defaults, None, OpCode::MakeFunction);
    }

    /* A body returning one expression, for lambdas and type aliases, the outer names visible and `params` shadowing them. */
    pub(super) fn expr_body(&mut self, params: &[String]) -> super::types::SSAChunk {
        let outer_versions = self.ssa_versions.clone();
        let mut body = self.with_fresh_chunk(|s| {
            s.ssa_versions = outer_versions;
            // Base name shadows the enclosing scope, prefix/`=` marker must be stripped.
            for p in params { s.bind_param(p); }
            s.in_function = true;
            s.expr();
            s.chunk.emit(OpCode::ReturnValue, 0);
        });
        // A lambda that yields is a generator, as a def is.
        body.is_generator = body.instructions.iter().any(|i| i.opcode == OpCode::Yield);
        body
    }
}
