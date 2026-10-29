
use super::Parser;
use crate::code::OpCode;
use crate::syntax::chunk::{Value, MAX_EXPR_DEPTH, Instruction};
use crate::syntax::ssa::{param_base_name, JoinNode, Versions};

use crate::syntax::text::{parse_string, parse_bytes_literal};
use crate::syntax::lexer::{Binding, Cond, Dot, Lambda, Shapes, Spelling, Token, TokenType, Zeros};

use alloc::{string::ToString, vec::Vec, string::String};

/* whether a trailer chain sits in an expression or a statement or an assignment target */
#[derive(Clone, Copy)]
pub(super) enum Tail { Expr, Stmt, Target }

/* comparison operators chain where the table wants while `not` here only opens `not in` */
#[inline(always)]
fn is_comparison(tok: TokenType) -> bool {
    matches!(tok, TokenType::EqualEqual | TokenType::NotEqual | TokenType::Less | TokenType::Greater
        | TokenType::LessEqual | TokenType::GreaterEqual | TokenType::In | TokenType::Is | TokenType::Not
        | TokenType::IsNot | TokenType::NotIn)
}

// the opcode a binary operator compiles to where comparisons take theirs from `comparison_op`
const fn binary_op(kind: TokenType) -> Option<OpCode> {
    Some(match kind {
        TokenType::Or => OpCode::Or,
        TokenType::And => OpCode::And,
        TokenType::BitOr => OpCode::BitOr,
        TokenType::BitXor => OpCode::BitXor,
        TokenType::BitAnd => OpCode::BitAnd,
        TokenType::LeftShift => OpCode::Shl,
        TokenType::RightShift => OpCode::Shr,
        TokenType::Plus => OpCode::Add,
        TokenType::Minus => OpCode::Sub,
        TokenType::Star => OpCode::Mul,
        TokenType::Slash => OpCode::Div,
        TokenType::Percent => OpCode::Mod,
        TokenType::FloorDiv => OpCode::FloorDiv,
        TokenType::At => OpCode::MatMul,
        TokenType::Pow => OpCode::Pow,
        TokenType::Range | TokenType::RangeInclusive => OpCode::CallNative,
        k if is_comparison_kind(k) => OpCode::Eq,
        _ => return None,
    })
}

const fn is_comparison_kind(kind: TokenType) -> bool {
    matches!(kind, TokenType::EqualEqual | TokenType::NotEqual | TokenType::Less | TokenType::Greater
        | TokenType::LessEqual | TokenType::GreaterEqual | TokenType::In | TokenType::Is | TokenType::Not
        | TokenType::IsNot | TokenType::NotIn)
}

// each infix operator of a precedence table with its powers and opcode by token
const fn infix(rows: &[(TokenType, u8, u8)]) -> [Option<(u8, u8, OpCode)>; 128] {
    let mut t = [None; 128];
    let mut i = 0;
    while i < rows.len() {
        let (kind, left, right) = rows[i];
        if left > 0 && let Some(op) = binary_op(kind) { t[kind as usize & 127] = Some((left, right, op)); }
        i += 1;
    }
    t
}

// the right power of each prefix operator of a precedence table by token
const fn prefix(rows: &[(TokenType, u8, u8)]) -> [u8; 128] {
    let mut t = [0; 128];
    let mut i = 0;
    while i < rows.len() {
        let (kind, left, right) = rows[i];
        if left == 0 { t[kind as usize & 127] = right; }
        i += 1;
    }
    t
}

impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {
    const INFIX: &'static [Option<(u8, u8, OpCode)>; 128] = &infix(S::SYNTAX.precedence);

    // the powers and opcode of infix `tok` with each arm a constant of the table
    #[inline(always)]
    fn infix_of(tok: TokenType) -> Option<(u8, u8, OpCode)> {
        macro_rules! rows { ($($k:ident),*) => { match tok { $(TokenType::$k => Self::INFIX[TokenType::$k as usize],)* _ => None } } }
        rows!(Or, And, BitOr, BitXor, BitAnd, LeftShift, RightShift, Plus, Minus, Star, Slash, Percent, FloorDiv, At, Pow,
            EqualEqual, NotEqual, Less, Greater, LessEqual, GreaterEqual, In, Is, Not, IsNot, NotIn, Range, RangeInclusive)
    }
    const PREFIX: &'static [u8; 128] = &prefix(S::SYNTAX.precedence);

    // the shapes and kit pieces of the table as constants each branch folds
    pub(super) const SHAPES: Shapes = S::SYNTAX.shapes;
    pub(super) const SPLAT: bool = Self::SHAPES.splat;
    pub(super) const PIPES: bool = matches!(Self::SHAPES.lambda, Lambda::Pipes);
    pub(super) const KEYS: bool = matches!(Self::SHAPES.dot, Dot::Key);
    pub(super) const BINDS: bool = !matches!(Self::SHAPES.binding, Binding::None);
    pub(super) const REQUIRED: bool = matches!(Self::SHAPES.binding, Binding::Required);
    // the least power a closure's default takes so the bar after it closes the parameters
    pub(super) const BAR_BP: u8 = match Self::INFIX[TokenType::BitOr as usize] { Some((left, _, _)) => left + 1, None => 0 };
    const BAR: u8 = S::SYNTAX.byte_of(TokenType::BitOr);

    /* a full expression with its ternary tail and recursion bounded inside `expr_bp` */
    pub(super) fn expr(&mut self) {
        self.saw_newline = false;
        let val_start = self.chunk.instructions.len();
        self.expr_bp(0);
        self.ternary_tail(val_start);
    }

    /* reorders the value emitted first into `[cond][JumpIfFalse][value][Jump][else]` so the condition runs first */
    pub(super) fn ternary_tail(&mut self, val_start: usize) {
        if !matches!(Self::SHAPES.cond, Cond::Suffix) { return self.question_tail(); }
        if self.saw_newline || !matches!(self.peek_same_line(), Some(TokenType::If)) { return; }
        self.advance();
        self.chunk.set_ternaries(true);
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

        // the moved call positions are sorted again for the binary search of `resolve_call`
        for e in &mut self.chunk.call_byte_pos[calls_from..] {
            let d = if (e.0 as usize) < cond_start { val_delta } else { cond_delta };
            e.0 = (e.0 as i64 + d) as u32;
        }
        self.chunk.call_byte_pos[calls_from..].sort_unstable_by_key(|&(ip, _)| ip);

        let jmp = self.emit_jump(OpCode::Jump);
        self.patch(jf);
        self.eat(TokenType::Else);
        // a conditional chained in the else branch nests one level deeper
        self.nested(Self::expr);
        self.patch(jmp);
    }

    /* the two values after the question mark of a test that compiled first */
    fn question_tail(&mut self) {
        if !matches!(Self::SHAPES.cond, Cond::Question) || self.saw_newline || !matches!(self.peek_same_line(), Some(TokenType::Question)) { return; }
        self.advance();
        self.chunk.set_ternaries(true);
        let jf = self.emit_jump(OpCode::JumpIfFalse);
        // either value may hold a conditional of its own so each nests one level deeper
        self.nested(Self::expr);
        let jmp = self.emit_jump(OpCode::Jump);
        self.patch(jf);
        self.eat(TokenType::Colon);
        self.nested(Self::expr);
        self.patch(jmp);
    }

    /* appends drained instructions back with their jump targets shifted by `delta` */
    pub(super) fn push_shifted(&mut self, ins: Vec<Instruction>, delta: i64) {
        for i in ins {
            let operand = if i.opcode.is_jump() { (i.operand as i64 + delta) as u16 } else { i.operand };
            self.chunk.instructions.push(Instruction { opcode: i.opcode, operand });
        }
    }

    pub(super) fn expr_tails(&mut self, start: usize) {
        self.postfix_tail(Tail::Expr);
        self.infix_bp(0);
        self.ternary_tail(start);
    }

    /* a Pratt parse bounded in depth so deep chains raise instead of overflowing the stack */
    pub(super) fn expr_bp(&mut self, min_bp: u8) {
        self.expr_depth += 1;
        if self.expr_depth > MAX_EXPR_DEPTH {
            self.expr_depth -= 1;
            self.error("expression too deeply nested");
            return;
        }
        match self.peek() {
            Some(TokenType::Not) => {
                self.advance();
                self.expr_bp(Self::PREFIX[TokenType::Not as usize]);
                self.chunk.emit(OpCode::Not, 0);
            }
            _ => self.parse_unary(),
        }
        self.infix_bp(min_bp);
        self.expr_depth -= 1;
    }

    // an operator never continues past the logical line so a decorator below stays one
    pub(super) fn infix_bp(&mut self, min_bp: u8) {
        // the open jumps of a comparison chain linked through their operands as a patch list
        let mut chain = u16::MAX;
        while let Some(tok) = self.peek_same_line() {
            if is_comparison(tok) {
                let Some((l_bp, r_bp, _)) = Self::infix_of(tok) else { break };
                if l_bp < min_bp { break; }
                let op = self.comparison_op();
                self.expr_bp(r_bp);
                // `a < b in c` tests `b` again and stops at the first false
                if S::SYNTAX.chained && self.peek_same_line().is_some_and(is_comparison) {
                    let ver = self.increment_version(crate::syntax::ssa::SSA_TMP_CMP);
                    let tmp = self.push_ssa_name(crate::syntax::ssa::SSA_TMP_CMP, ver);
                    self.chunk.emit(OpCode::StoreName, tmp);
                    self.chunk.emit(OpCode::LoadName, tmp);
                    self.chunk.emit(op, 0);
                    let jmp = self.emit_jump(OpCode::JumpIfFalseOrPop);
                    if let Some(i) = self.chunk.instructions.get_mut(jmp) { i.operand = chain; }
                    chain = jmp as u16;
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

            let Some((l_bp, r_bp, op)) = Self::infix_of(tok) else { break };
            if l_bp < min_bp { break; }
            self.advance();

            if Self::SHAPES.ranges && op == OpCode::CallNative {
                self.range_tail(tok, r_bp);
                continue;
            }

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

    /* the range from the stacked value up to the bound or past it after `..=` */
    fn range_tail(&mut self, tok: TokenType, r_bp: u8) {
        let at = self.tokens.pos - 1;
        let call_pos = self.tokens.toks[at].start as u32;
        // a range from a literal zero calls the native with its bound alone
        let zero = self.tokens.toks[..at].iter().rev().find(|t| !matches!(t.kind, TokenType::LineBreak | TokenType::Comment)).is_some_and(|t| t.kind == TokenType::Int)
            && self.chunk.instructions.last().is_some_and(|i| i.opcode == OpCode::LoadConst && i.operand as usize + 1 == self.chunk.constants.len())
            && self.chunk.constants.last() == Some(&Value::Int(0));
        if zero {
            self.chunk.instructions.pop();
            self.chunk.constants.pop();
        }
        self.expr_bp(r_bp);
        if tok == TokenType::RangeInclusive {
            self.emit_const(Value::Int(1));
            self.chunk.emit(OpCode::Add, 0);
        }
        // the runtime's range builds it whatever a binding of its name holds
        let id = S::NATIVES.intrinsic(crate::lang::Intrinsic::Range).map_or(u16::MAX, |n| n.id.0);
        self.call_native(crate::syntax::chunk::NativeSite { id, pos: if zero { 1 } else { 2 }, kw: 0, spread: false, pinned: true, fast: false, void: false }, call_pos, false);
    }

    /* consumes one comparison operator including `not in` and `is not` */
    fn comparison_op(&mut self) -> OpCode {
        match self.advance().kind {
            TokenType::Is if self.eat_if(TokenType::Not) => OpCode::IsNot,
            TokenType::Is => OpCode::Is,
            TokenType::IsNot => OpCode::IsNot,
            TokenType::Not => { self.eat(TokenType::In); OpCode::NotIn }
            TokenType::NotIn => OpCode::NotIn,
            TokenType::In => OpCode::In,
            TokenType::EqualEqual => OpCode::Eq,
            TokenType::NotEqual => OpCode::NotEq,
            TokenType::Less => OpCode::Lt,
            TokenType::Greater => OpCode::Gt,
            TokenType::LessEqual => OpCode::LtEq,
            _ => OpCode::GtEq,
        }
    }

    pub(super) fn parse_unary(&mut self) {
        match self.peek() {
            Some(TokenType::Minus) => {
                self.advance();
                self.expr_bp(Self::PREFIX[TokenType::Minus as usize]);
                self.chunk.emit(OpCode::Minus, 0);
            }
            Some(TokenType::Plus) => {
                // unary plus emits an op since it turns a bool into an int
                self.advance();
                self.expr_bp(Self::PREFIX[TokenType::Plus as usize]);
                self.chunk.emit(OpCode::Pos, 0);
            }
            Some(TokenType::Tilde) => {
                self.advance();
                self.expr_bp(Self::PREFIX[TokenType::Tilde as usize]);
                self.chunk.emit(OpCode::BitNot, 0);
            }
            Some(TokenType::Await) => {
                let t = self.advance();
                self.check_await(t.start, t.end);
                self.expr_bp(Self::PREFIX[TokenType::Await as usize]);
                self.emit_await();
            }
            _ => self.parse_atom()
        }
    }

    pub(super) fn parse_atom(&mut self) {
        // a block brace starts no operand so it stays for the block it bounds
        if Self::DELIMITED && matches!(self.tokens.peek().map(|t| t.kind), Some(TokenType::BlockOpen | TokenType::BlockClose | TokenType::Newline | TokenType::LineBreak | TokenType::Comment)) && matches!(self.peek(), Some(TokenType::BlockOpen | TokenType::BlockClose)) {
            return self.diag_at_peek("expected expression");
        }
        let errs_before = self.errors.len();
        let t = self.advance();
        // a table without the lambda word reads its anonymous functions apart
        if !matches!(Self::SHAPES.lambda, Lambda::Word) && self.closure(t) {
            self.postfix_tail(Tail::Expr);
            return;
        }
        match t.kind {
            TokenType::Name | TokenType::Underscore => self.name(t),
            TokenType::String | TokenType::FormatStart => self.string_group(t),
            TokenType::Bytes => {
                // adjacent bytes literals join and mixing them with str reports a diagnostic
                let mut buf = parse_bytes_literal::<S>(self.lexeme(&t));
                while S::SYNTAX.strings.adjacent && matches!(self.peek_same_line(), Some(TokenType::Bytes)) {
                    let t = self.advance();
                    buf.extend_from_slice(&parse_bytes_literal::<S>(self.lexeme(&t)));
                }
                self.emit_const(Value::Bytes(buf));
            }
            TokenType::Int | TokenType::Float => {
                self.parse_number(self.lexeme(&t), t.kind);
            }
            TokenType::True => self.chunk.emit(OpCode::LoadTrue, 0),
            TokenType::False => self.chunk.emit(OpCode::LoadFalse, 0),
            TokenType::Nil => self.chunk.emit(OpCode::LoadNone, 0),
            TokenType::Ellipsis => self.chunk.emit(OpCode::LoadEllipsis, 0),
            TokenType::BraceOpen => self.brace_literal(),
            TokenType::BracketOpen => self.list_literal(),
            TokenType::ParenOpen => self.paren_atom(),
            // `yield` or `yield from` as a value with the keyword already consumed
            TokenType::Yield => self.emit_yield(),
            TokenType::FnExpr => self.parse_lambda(),
            // a closer its group still waits for goes back so the group ends there
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose if self.errors.len() == errs_before => return self.unread_closer(t),
            // the caret sits at the consumed token unless `advance` already reported the error
            _ => {
                if self.errors.len() == errs_before {
                    self.error_at(t.start, t.end, "expected expression");
                }
            }
        }
        self.postfix_tail(Tail::Expr);
    }

    /* reports the operand missing before closer `t` and hands `t` back to its opener */
    #[cold]
    #[inline(never)]
    fn unread_closer(&mut self, t: Token) {
        let at = self.tokens.pos - 1;
        let before = &self.tokens.toks[..at];
        // the opener `advance` popped is the nearest one before `t` left open
        let mut depth = 0usize;
        let opener = before.iter().rev().find(|o| match o.kind {
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => { depth += 1; false }
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen if depth > 0 => { depth -= 1; false }
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => true,
            _ => false,
        }).copied();
        if let Some(o) = opener.filter(|o| o.kind == super::bracket_open(t.kind)) {
            let last = before.iter().rev().find(|p| !matches!(p.kind, TokenType::Newline | TokenType::LineBreak | TokenType::Comment)).copied();
            self.tokens.pos = at;
            self.bracket_stack.push((o.kind, o.start, o.end, self.errors.len()));
            if let Some(p) = last { (self.last_line, self.last_end) = (p.line, p.end); }
        }
        self.error_at(t.start, t.end, "expected expression");
    }

    /* after `(` an empty tuple or a generator or a tuple or a grouped expression */
    pub(super) fn paren_atom(&mut self) {
        if matches!(self.peek(), Some(TokenType::ParenClose)) {
            self.advance();
            self.chunk.emit(OpCode::BuildTuple, 0);
        } else if self.at_comp() {
            self.comprehension(super::literals::Comp::Gen);
            self.eat(TokenType::ParenClose);
        } else {
            // a leading `*it` starts a tuple
            let star = matches!(self.peek(), Some(TokenType::Star)) && Self::SPLAT;
            if !star { self.expr(); }
            if star || matches!(self.peek(), Some(TokenType::Comma)) {
                self.tuple_rest(!star as u16, |s| matches!(s.peek(), Some(TokenType::ParenClose) | None));
            }
            self.eat(TokenType::ParenClose);
        }
    }

    /* adjacent string and f-string literals join into one value */
    fn string_group(&mut self, first: Token) {
        let mut parts = 0u16;
        let mut fstrings = 0u32;
        let mut lit = String::new();
        let mut tok = first;
        loop {
            if tok.kind == TokenType::String {
                lit.push_str(&parse_string::<S>(self.lexeme(&tok)));
            } else {
                // pending literal text goes out first so the parts keep their order
                if !lit.is_empty() {
                    self.emit_const(Value::Str(core::mem::take(&mut lit)));
                    parts += 1;
                }
                parts += self.fstring(tok.start, tok.end);
                fstrings += 1;
            }
            match self.peek_same_line() {
                Some(TokenType::String | TokenType::FormatStart) if S::SYNTAX.strings.adjacent => tok = self.advance(),
                _ => break,
            }
        }
        // plain literals fold into one constant
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

    /* a name-led operand with its trailers for callers that took the name token */
    pub(super) fn name_operand(&mut self, t: Token) {
        self.name(t);
        self.postfix_tail(Tail::Expr);
    }

    pub(super) fn name(&mut self, t: Token) {
        let name = self.lexeme(&t);
        match self.peek_same_line() {
            // a walrus stores like an assignment honoring an enclosing `global` and leaves the value
            Some(TokenType::ColonEqual) => {
                // a table without named expressions leaves the mark to fail after the name
                if !Self::SHAPES.named { return self.emit_load_ssa(name); }
                self.advance();
                self.expr();
                if self.in_comp && !self.globals_decl.contains(name) && !self.chunk.nonlocals.iter().any(|n| n == name) { self.chunk.nonlocals.push(name.to_string()); }
                self.store_name(name);
                self.emit_load_ssa(name);
            }
            Some(TokenType::ParenOpen) => { self.call(name, false); }
            _ => self.emit_load_ssa(name),
        }
    }

    // the digits after a radix prefix of the table and their radix
    fn parse_int_prefix(s: &str) -> (&str, u32) {
        let b = s.as_bytes();
        if b.len() > 1 && b[0] == b'0' && let Some(&(_, base)) = S::SYNTAX.numbers.radixes.iter().find(|(l, _)| l.eq_ignore_ascii_case(&b[1])) {
            return (&s[2..], base);
        }
        (s, 10)
    }

    // whether `b` separates digits in this table
    #[inline(always)]
    fn separates(b: u8) -> bool { S::SYNTAX.numbers.separator == Some(b) }

    pub(super) fn parse_number(&mut self, raw: &str, kind: TokenType) {
        if kind == TokenType::Float {
            // a malformed float such as `1e` is a syntax error and never 0.0
            match Self::float_literal(raw) {
                Some(f) => self.emit_const(Value::Float(f)),
                None => self.error("invalid float literal"),
            }
            return;
        }
        match Self::int_literal(raw) {
            Ok(v) => self.emit_const(v),
            Err(m) => self.error(&m),
        }
    }

    /* a float literal with its separators copied only when it has one */
    pub(super) fn float_literal(raw: &str) -> Option<f64> {
        match S::SYNTAX.numbers.separator {
            Some(sep) if raw.as_bytes().contains(&sep) => raw.bytes().filter(|&b| b != sep).map(char::from).collect::<String>().parse().ok(),
            _ => raw.parse().ok(),
        }
    }

    /* an int literal in any base with its separators or the syntax error it raises */
    pub(super) fn int_literal(raw: &str) -> Result<Value, String> {
        let (digits, mut base) = Self::parse_int_prefix(raw);
        let digits = digits.bytes().filter(|&b| !Self::separates(b));
        // a decimal with leading zeros fails or reads as octal unless every digit is zero
        if base == 10 && digits.clone().next() == Some(b'0') && digits.clone().any(|b| b != b'0') {
            match S::SYNTAX.numbers.zeros {
                Zeros::Refused => return Err("leading zeros in decimal integer literals are not permitted".into()),
                Zeros::Octal => base = 8,
                Zeros::Allowed => {}
            }
        }
        let mut narrow = 0u64;
        // this many digits of the base always fit so only longer literals check for overflow
        let fits = match base { 2 => 64, 8 => 21, 10 => 19, 16 => 16, _ => 64 / (32 - base.leading_zeros()) as usize };
        for (n, b) in digits.clone().enumerate() {
            let d = (b as char).to_digit(base).ok_or("invalid digit in integer literal")?;
            if n < fits { narrow = narrow * base as u64 + d as u64; continue; }
            match narrow.checked_mul(base as u64).and_then(|v| v.checked_add(d as u64)) {
                Some(v) => narrow = v,
                None => return Self::big_literal(digits, base),
            }
        }
        // the lexer already reported a literal without digits so a zero stands in
        if narrow <= i64::MAX as u64 { Ok(Value::Int(narrow as i64)) } else { Self::big_literal(digits, base) }
    }

    /* a literal past i64 refused beyond the decimal digit limit `int` enforces too */
    #[cold]
    fn big_literal(digits: impl Iterator<Item = u8>, base: u32) -> Result<Value, String> {
        // a buffer as long as the literal so boxing the digits mostly moves nothing
        let mut d = alloc::vec::Vec::with_capacity(digits.size_hint().1.unwrap_or(0));
        d.extend(digits.map(|b| (b as char).to_digit(base).unwrap_or(0) as u8));
        // leading zeros drop so one value keeps one form
        let zeros = d.iter().take_while(|&&x| x == 0).count();
        d.drain(..zeros);
        // decimal literals past the runtime's digit limit refuse so no conversion costs quadratic time
        let limit = S::NATIVES.policy.digits() as usize;
        if base == 10 && d.len() > limit {
            return Err(Self::past_digit_limit(d.len(), limit));
        }
        Ok(Value::Wide { neg: false, base: base as u8, digits: d.into_boxed_slice() })
    }

    /* why `n` decimal digits pass `limit` naming hex only where the table spells it */
    #[cold]
    #[inline(never)]
    fn past_digit_limit(n: usize, limit: usize) -> String {
        match S::SYNTAX.numbers.radixes.iter().find(|&&(_, radix)| radix == 16) {
            Some(&(letter, _)) => crate::s!("an integer literal of ", int n as i64, " digits is past the limit of ", int limit as i64, ", write it in hex after 0", char letter as char),
            None => crate::s!("an integer literal of ", int n as i64, " digits is past the limit of ", int limit as i64),
        }
    }

    /* several items make one tuple key and the result is true for a lone slice */
    pub(super) fn parse_subscript(&mut self) -> bool {
        if Self::SPLAT && matches!(self.peek(), Some(TokenType::Star)) { return self.starred_subscript(0); }
        let slice = self.subscript_item();
        if !matches!(self.peek(), Some(TokenType::Comma)) {
            self.eat(TokenType::BracketClose);
            return slice;
        }
        let mut n = 1u16;
        while self.eat_if(TokenType::Comma) && !matches!(self.peek(), Some(TokenType::BracketClose)) {
            if Self::SPLAT && matches!(self.peek(), Some(TokenType::Star)) { return self.starred_subscript(n); }
            self.subscript_item();
            n += 1;
        }
        self.eat(TokenType::BracketClose);
        self.chunk.emit(OpCode::BuildTuple, n);
        false
    }

    /* the rest of a key holding `*it` after `count` emitted items unpacked into one tuple */
    fn starred_subscript(&mut self, count: u16) -> bool {
        self.chunk.emit(OpCode::BuildList, count);
        loop {
            if self.eat_if(TokenType::Star) {
                self.expr();
                self.chunk.emit(OpCode::ListExtend, 0);
            } else {
                self.subscript_item();
                self.chunk.emit(OpCode::ListAppend, 0);
            }
            if !self.eat_if(TokenType::Comma) || matches!(self.peek(), Some(TokenType::BracketClose)) { break; }
        }
        self.eat(TokenType::BracketClose);
        self.chunk.emit(OpCode::ListToTuple, 0);
        false
    }

    /* one index or `a:b:c` slice with None defaults and true for a slice */
    #[inline]
    fn subscript_item(&mut self) -> bool {
        if Self::SHAPES.slices && matches!(self.peek(), Some(TokenType::Colon)) {
            self.chunk.emit(OpCode::LoadNone, 0);
        } else {
            self.expr();
        }
        if !Self::SHAPES.slices || !self.eat_if(TokenType::Colon) { return false; }
        self.slice_bound();
        let parts = if self.eat_if(TokenType::Colon) { self.slice_bound(); 3 } else { 2 };
        self.chunk.emit(OpCode::BuildSlice, parts);
        true
    }

    // an omitted bound loads None
    fn slice_bound(&mut self) {
        if matches!(self.peek(), Some(TokenType::Colon | TokenType::Comma | TokenType::BracketClose)) {
            self.chunk.emit(OpCode::LoadNone, 0);
        } else {
            self.expr();
        }
    }

    /* trailers on the same line and true when the chain ends in a store */
    pub(super) fn postfix_tail(&mut self, tail: Tail) -> bool {
        loop {
            match self.peek_same_line() {
                Some(TokenType::BracketOpen) => {
                    self.advance();
                    let cut = self.parse_subscript();
                    if self.store_trailer(tail, None) { return true; }
                    self.emit_get_item(cut);
                }
                Some(TokenType::Dot) => {
                    self.advance();
                    // in brackets the name may be on the next line unlike at a statement end
                    let Some(next) = self.peek_same_line() else { self.diag_at_peek(Self::ATTR_AFTER_DOT); return false };
                    if !matches!(next, TokenType::Name | TokenType::Underscore) { self.error(Self::ATTR_AFTER_DOT); return false; }
                    let t = self.advance();
                    let source = self.source;
                    let name = &source[t.start..t.end];
                    // a table that reads keys at dots keeps the attribute only for a call
                    if Self::KEYS && !matches!(self.peek_same_line(), Some(TokenType::ParenOpen)) {
                        self.emit_const(Value::Str(name.to_string()));
                        if self.store_trailer(tail, None) { return true; }
                        self.chunk.emit(OpCode::GetItem, 0);
                        continue;
                    }
                    if self.store_trailer(tail, Some(name)) { return true; }
                    // `LoadAttr` right before `Call` lets `fuse_method_calls` collapse the pair
                    let idx = self.chunk.push_name(name);
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

    /* a subscript read whose lone slice hands its bounds to one op */
    pub(super) fn emit_get_item(&mut self, cut: bool) { self.emit_item(cut, OpCode::GetItem, OpCode::Cut) }

    /* a subscript delete whose lone slice hands its bounds to one op */
    pub(super) fn emit_del_item(&mut self, cut: bool) { self.emit_item(cut, OpCode::DelItem, OpCode::DelCut) }

    /* `item` of a subscript or `cut` over the bounds of its lone slice */
    fn emit_item(&mut self, cut: bool, item: OpCode, cutting: OpCode) {
        match self.chunk.instructions.last() {
            Some(&Instruction { opcode: OpCode::BuildSlice, operand: n }) if cut => {
                self.chunk.instructions.pop();
                self.chunk.emit(cutting, n + 1);
            }
            _ => self.chunk.emit(item, 0),
        }
    }

    /* a store ending a trailer at a target end or a statement `op=` or annotation */
    fn store_trailer(&mut self, tail: Tail, attr: Option<&str>) -> bool {
        let (load, store) = if attr.is_some() { (OpCode::LoadAttr, OpCode::StoreAttr) } else { (OpCode::GetItem, OpCode::StoreItem) };
        match tail {
            Tail::Expr => return false,
            Tail::Target => {
                if !self.at_target_end() { return false; }
                // the value compiled first so it moves above the object and key
                self.chunk.emit(if attr.is_some() { OpCode::Swap } else { OpCode::Rot3 }, 0);
            }
            Tail::Stmt if matches!(self.peek_same_line(), Some(TokenType::Colon)) => {
                self.advance();
                // a bare annotation evaluates the target and stores nothing
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

    pub(super) fn parse_lambda(&mut self) {
        let (params, defaults) = self.param_list(TokenType::Colon, false);
        self.eat(TokenType::Colon);
        let cut = self.enter_qual(S::NATIVES.names().anonymous, true);
        let body = self.expr_body(&params);
        self.leave_qual(cut);
        self.push_function(params, body, defaults, None, OpCode::MakeFunction);
    }

    /* whether `t` opened a closure or a lambda word a table without the word refuses */
    fn closure(&mut self, t: Token) -> bool {
        if t.kind == TokenType::FnExpr {
            self.error_at(t.start, t.end, "expected expression");
            return true;
        }
        // two bars together open a closure without parameters
        let empty = t.kind == TokenType::Or && self.lexeme(&t).bytes().all(|b| b == Self::BAR);
        if !Self::PIPES || !(empty || t.kind == TokenType::BitOr) { return false; }
        let (params, defaults) = if empty { (Vec::new(), 0) } else {
            let list = self.param_list(TokenType::BitOr, false);
            self.eat(TokenType::BitOr);
            list
        };
        let cut = self.enter_qual(S::NATIVES.names().anonymous, true);
        let body = self.expr_body(&params);
        self.leave_qual(cut);
        self.push_function(params, body, defaults, None, OpCode::MakeFunction);
        true
    }

    // a lambda body returning one expression with the outer names visible and params shadowing them
    pub(super) fn expr_body(&mut self, params: &[String]) -> crate::syntax::chunk::SSAChunk {
        let mut outer = core::mem::take(&mut self.ssa_versions);
        let mut body = self.with_fresh_chunk(|s| {
            let held = s.lend_versions(&mut outer, params);
            s.in_function = true;
            // an error in the body points at its expression as a statement would
            s.mark_stmt();
            s.expr();
            s.chunk.emit(OpCode::ReturnValue, 0);
            s.return_versions(&mut outer, held);
        });
        self.ssa_versions = outer;
        // a lambda that yields is a generator like a def
        body.is_generator = body.instructions.iter().any(|i| i.opcode == OpCode::Yield);
        body
    }

    /* moves the outer versions into a lambda body and binds its params over them */
    pub(super) fn lend_versions(&mut self, outer: &mut Versions, params: &[String]) -> usize {
        self.ssa_versions = core::mem::take(outer);
        let held = self.ssa_versions.len();
        // a join that never commits puts each version the body bumps on the trail
        if held != 0 { self.join_stack.push(JoinNode { start: 0, mid: None }); }
        // each parameter takes its slot in order and shadows by its base name
        for p in params {
            let base = param_base_name(p);
            let (slot, old) = self.ssa_versions.set(base, 0);
            if (slot as usize) < held { self.trail.push((slot, old)); }
            let _ = self.push_ssa_name(base, 0);
        }
        held
    }

    /* gives the lent versions back as they stood before the lambda body began */
    pub(super) fn return_versions(&mut self, outer: &mut Versions, held: usize) {
        // an empty outer gets nothing back and the body's versions drop with its chunk
        if held == 0 { return; }
        // nonlocals the body binds leave the unbound list before its versions go back
        if self.in_function && !self.unbound_nonlocals.is_empty() {
            let (versions, own) = (&self.ssa_versions, &self.chunk.nonlocals);
            self.unbound_nonlocals.retain(|(name, ..)| !versions.contains(name) || own.contains(name));
        }
        self.join_stack.pop();
        let mut versions = core::mem::take(&mut self.ssa_versions);
        versions.truncate(held);
        // the earliest entry of a slot holds the version it had before the body
        for &(slot, old) in self.trail.iter().rev() {
            if (slot as usize) < held { versions.set_at(slot, old); }
        }
        *outer = versions;
    }

    /* a native call or the runtime opcode a plain call of it fuses into */
    pub(super) fn native_or_own(&mut self, site: crate::syntax::chunk::NativeSite, name: &str, from: usize, call_pos: u32, stmt: bool) -> bool {
        if S::OPS.ops.is_empty() || site.kw != 0 || site.spread { return self.call_native(site, call_pos, stmt); }
        let Some(op) = S::OPS.fused(name, site.pos) else { return self.call_native(site, call_pos, stmt) };
        // constant operands fold once the whole program shows no binding of the name
        if let Some(v) = crate::code::fold::own_value(&self.chunk, from, op, S::OPS) { self.chunk.folds.push((self.chunk.instructions.len() as u32, site.pos, v, S::OPS.ops[usize::from(op)].fuse.unwrap_or_default())); }
        self.chunk.emit(OpCode::RuntimeOp, u16::from(op) | u16::from(site.pos) << 8);
        self.chunk.record_call_pos(call_pos);
        false
    }
}
