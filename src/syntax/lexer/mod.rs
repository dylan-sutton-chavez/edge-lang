pub mod tables;
pub use tables::utf8_char_len;

pub mod syntax;
pub use syntax::{index, join, joined, operators, role, Binding, Blocks, Comments, Cond, Continuation, Dot, Ends, Escapes, Interp, Keywords, Lambda, Literal, Mode, Numbers, Quote, Shapes, Shown, Spelling, Strings, Suffix, Syntax, Unicode, Unknown, Zeros, BYTES, FORMAT, RAW};

mod scan;

use scan::{Lead, Scanner};
use alloc::vec::Vec;

const MAX_SOURCE_SIZE: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct Token {
    pub kind: TokenType,
    pub line: usize,
    pub start: usize,
    pub end: usize,
    /* starts the element of a comprehension so the parser compiles its clauses first */
    pub comp: bool,
    /* counts the `=` ending the targets of its statement so the value compiles first */
    pub assigns: u8,
}

/* a static message suffices since the table words every lex error at compile time */
#[derive(Debug)]
pub struct LexError {
    pub start: usize,
    pub end: usize,
    pub msg: &'static str,
}

/* the dots a token puts in a relative import spec where a Dot counts one */
pub(crate) fn dots(kind: TokenType, text: &str) -> usize {
    match kind {
        TokenType::Dot => 1,
        // a range or ellipsis spelled with dots alone counts each of them
        TokenType::Range | TokenType::Ellipsis if !text.is_empty() && text.bytes().all(|b| b == b'.') => text.len(),
        _ => 0,
    }
}

// a token by its role in the grammar whatever a table spells it as
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TokenType {
    // reserved words
    False, Nil, True, And, As, Assert, Async, Await, Break, Class, Continue, Fn, Delete,
    ElseIf, Else, Catch, Finally, For, From, Global, If, Import, In, Is, FnExpr, Outer,
    Not, Or, Empty, Throw, Return, Try, While, With, Yield,
    // words reserved only where the grammar wants them
    Case, Match, Type, Underscore,
    // operators of three bytes in the reference tables
    PowEqual, FloorDivEqual, LeftShiftEqual, RightShiftEqual,
    // operators of two bytes in the reference tables
    NotEqual, PercentEqual, BitAndEqual, Pow, StarEqual, PlusEqual, MinusEqual,
    Returns, Ellipsis, FloorDiv, SlashEqual, ColonEqual, LeftShift, LessEqual,
    EqualEqual, GreaterEqual, RightShift, AtEqual, BitXorEqual, BitOrEqual,
    // the conversion of an interpolated field then the operators of one byte
    Convert, Percent, BitAnd, Star, Plus, Minus, Dot, Slash, Less, Equal, Greater,
    At, BitXor, BitOr, Tilde, Comma, Colon, Semi,
    // brackets
    ParenOpen, ParenClose, BracketOpen, BracketClose, BraceOpen, BraceClose,
    // literals
    Name, Float, Int, String, Bytes,
    // the parts of an interpolated string
    FormatStart, FormatText, FormatEnd,
    // layout
    Comment, Newline, Indent, Dedent, LineBreak, Eof,
    // the bounds of a block in a table that delimits its blocks
    BlockOpen, BlockClose,
    // comparisons a table spells as one word or phrase or symbol
    IsNot, NotIn,
    // the word and marks of the kit pieces a table switches on
    Let, Arm, Range, RangeInclusive, Question,
}

// the tokens of `source` in spelling `S` and the lex errors the caller folds in
pub fn lex_as<S: Spelling>(source: &str) -> (Vec<Token>, Vec<LexError>) {
    let (mut tokens, errors) = scanned::<S>(source);
    // scans back past this much work leave the rest to the forward pass
    let budget = 16 * tokens.len() + 1024;
    let end = settle::<S>(&mut tokens, budget);
    tokens.truncate(end);
    (tokens, errors)
}

/* the tokens of `source` settled by the forward pass alone for tests to compare */
#[doc(hidden)]
pub fn lex_forward<S: Spelling>(source: &str) -> (Vec<Token>, Vec<LexError>) {
    let (mut tokens, errors) = scanned::<S>(source);
    let end = walk::<S>(&mut tokens, 0);
    tokens.truncate(end);
    (tokens, errors)
}

/* the tokens the scanner gives before any soft word or flag is settled */
#[inline]
fn scanned<S: Spelling>(source: &str) -> (Vec<Token>, Vec<LexError>) {
    let bytes = source.as_bytes();
    let len = source.len();
    let mut scanner = Scanner::<S>::new(bytes);
    // a UTF-8 BOM is skipped so it never fuses into the first identifier
    if S::SYNTAX.bom && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { scanner.pos = 3; }

    if len > MAX_SOURCE_SIZE {
        scanner.errors.push(LexError {
            start: 0, end: 0,
            msg: "source file exceeds maximum size (10 MiB)",
        });
        return (
            alloc::vec![Token { kind: TokenType::Eof, line: 0, start: len, end: len, comp: false, assigns: 0 }],
            scanner.errors,
        );
    }

    // about two bytes of source per token so most sources never grow the buffer
    let mut tokens: Vec<Token> = Vec::with_capacity(len / 2 + 8);
    let token = |(kind, line, start, end)| Token { kind, line, start, end, comp: false, assigns: 0 };
    let mut lead = Lead::NEW;
    while let Some(t) = scanner.next_token(&tokens, &mut lead) {
        if Scanner::<S>::WORDS { scanner.word_blocks(&mut tokens, t); } else { tokens.push(token(t)); }
    }
    // dangling indents drain at the end so every block closes
    while scanner.indent_stack.pop().is_some() {
        tokens.push(token((TokenType::Dedent, scanner.line, len, len)));
    }
    tokens.push(token((TokenType::Eof, scanner.line, len, len)));
    (tokens, scanner.errors)
}

/* settles the tokens in place by short scans back giving how many stand */
#[inline]
fn settle<S: Spelling>(tokens: &mut [Token], budget: usize) -> usize {
    // the tokens before `i` are final and those after it as scanned
    let (mut i, mut stmt, mut spent) = (0, 0, 0);
    while i < tokens.len() {
        match tokens[i].kind {
            TokenType::Eof => return i + 1,
            // scans back stop at these so each costs the statement read so far
            TokenType::Newline | TokenType::Indent | TokenType::Dedent | TokenType::Semi => stmt = i + 1,
            TokenType::BlockOpen | TokenType::BlockClose if S::SYNTAX.delimited() => stmt = i + 1,
            // `match` and `case` stay keywords only in a header and `type` only in an alias
            TokenType::Match | TokenType::Case if Scanner::<S>::SOFT_HEADS => {
                let (header, read) = if starts_stmt::<S>(tokens, i) { opens_header::<S>(tokens, i) } else { (false, 0) };
                spent += i - stmt + read;
                if !header { tokens[i].kind = TokenType::Name; }
            }
            TokenType::Type if Scanner::<S>::SOFT_TYPE => {
                spent += i - stmt;
                if !(starts_stmt::<S>(tokens, i) && tokens.get(i + 1).is_some_and(|t| t.kind == TokenType::Name)) { tokens[i].kind = TokenType::Name; }
            }
            TokenType::Fn if Scanner::<S>::SOFT_FN => {
                spent += i - stmt;
                if !defines::<S>(tokens, i) { tokens[i].kind = TokenType::Name; }
            }
            TokenType::For => {
                spent += i - stmt;
                if let Some(first) = comp_start::<S>(&tokens[..i]) { tokens[first].comp = true; }
            }
            TokenType::Equal => {
                spent += i - stmt;
                if let Some(head) = assign_head::<S>(&tokens[..i]) { tokens[head].assigns = tokens[head].assigns.saturating_add(1); }
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 1;
        // scans that grow with what a statement repeats hand the rest to the linear pass
        if spent > budget { return walk::<S>(tokens, i); }
    }
    i
}

/* settles the tokens from `at` in one forward pass once the levels catch up */
fn walk<S: Spelling>(tokens: &mut [Token], at: usize) -> usize {
    let mut pass = Pass::new();
    for i in 0..at { pass.shape::<S>(tokens, i, tokens[i].kind); }
    let mut i = at;
    while i < tokens.len() {
        let tok = tokens[i].kind;
        pass.step::<S>(tokens, i);
        i += 1;
        if tok == TokenType::Eof { break; }
    }
    i
}

/* the first token of the item a `for` ends inside brackets or None */
fn comp_start<S: Spelling>(tokens: &[Token]) -> Option<usize> {
    // commas between a lambda and its colon separate its parameters and not items
    let (mut depth, mut item, mut params) = (0usize, None, false);
    for (i, t) in tokens.iter().enumerate().rev() {
        match t.kind {
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth += 1,
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen if depth > 0 => depth -= 1,
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => { item = item.or(Some(i + 1)); break; }
            TokenType::Colon if depth == 0 => params = true,
            TokenType::FnExpr if depth == 0 => params = false,
            // the bars of a closure hold commas between its parameters
            TokenType::BitOr if matches!(S::SYNTAX.shapes.lambda, Lambda::Pipes) && depth == 0 => params = !params,
            TokenType::Comma if depth == 0 && !params => item = item.or(Some(i + 1)),
            TokenType::Newline | TokenType::Indent | TokenType::Dedent | TokenType::Semi => return None,
            TokenType::BlockOpen | TokenType::BlockClose if S::SYNTAX.delimited() => return None,
            _ => {}
        }
        if i == 0 { return None; }
    }
    (item?..tokens.len()).find(|&j| !matches!(tokens[j].kind, TokenType::LineBreak | TokenType::Comment))
}

/* the statement head whose targets an `=` ends and None for keyword arguments and defaults */
fn assign_head<S: Spelling>(tokens: &[Token]) -> Option<usize> {
    // the first colon outside brackets ends a header when a compound keyword leads the line
    let (mut depth, mut colon, mut bars) = (0usize, None, 0u32);
    let mut at = 0;
    for (i, t) in tokens.iter().enumerate().rev() {
        match t.kind {
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth += 1,
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen if depth == 0 => return None,
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth -= 1,
            TokenType::FnExpr if depth == 0 => return None,
            TokenType::Colon if depth == 0 => colon = Some(i),
            // each bar after the header opens or closes the parameters of a closure
            TokenType::BitOr if matches!(S::SYNTAX.shapes.lambda, Lambda::Pipes) && depth == 0 && colon.is_none() => bars += 1,
            // an arm ends its pattern as a line ends a statement
            TokenType::Arm if S::SYNTAX.shapes.arms && depth == 0 => { at = i + 1; break; }
            TokenType::Newline | TokenType::Semi | TokenType::Indent | TokenType::Dedent => { at = i + 1; break; }
            TokenType::BlockOpen | TokenType::BlockClose if S::SYNTAX.delimited() => { at = i + 1; break; }
            _ => {}
        }
    }
    // an `=` between the bars of a closure gives a parameter its default
    if bars % 2 == 1 { return None; }
    let first = |from: usize| (from..tokens.len()).find(|&j| !matches!(tokens[j].kind, TokenType::LineBreak | TokenType::Comment));
    let head = first(at)?;
    match colon {
        Some(c) if opens_body(tokens[head].kind) => first(header_colon::<S>(tokens, head, c) + 1),
        _ => Some(head),
    }
}

/* the colon ending the header led at `head` past the colons of its question conditionals */
fn header_colon<S: Spelling>(tokens: &[Token], head: usize, first: usize) -> usize {
    if !matches!(S::SYNTAX.shapes.cond, Cond::Question) { return first; }
    let (mut depth, mut open) = (0usize, 0usize);
    for (i, t) in tokens.iter().enumerate().skip(head) {
        match t.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth = depth.saturating_sub(1),
            TokenType::Question if depth == 0 => open += 1,
            TokenType::Colon if depth == 0 && open > 0 => open -= 1,
            TokenType::Colon if depth == 0 => return i,
            _ => {}
        }
    }
    first
}

/* whether a statement led by `kind` ends its header in a colon a body follows */
pub fn opens_body(kind: TokenType) -> bool {
    matches!(kind, TokenType::If | TokenType::ElseIf | TokenType::Else | TokenType::While | TokenType::For | TokenType::With
        | TokenType::Try | TokenType::Catch | TokenType::Finally | TokenType::Fn | TokenType::Class | TokenType::Async
        | TokenType::Match | TokenType::Case)
}

/* calls `f` with each token that starts a statement among the tokens a lex resolved */
pub(crate) fn statements<S: Spelling>(tokens: &[Token], mut f: impl FnMut(usize)) {
    let mut pass = Pass::new();
    for (i, t) in tokens.iter().enumerate() {
        if pass.starts { f(i); }
        pass.shape::<S>(tokens, i, t.kind);
    }
}

fn starts_stmt<S: Spelling>(tokens: &[Token], i: usize) -> bool {
    i == 0 || matches!(tokens[i - 1].kind, TokenType::Newline | TokenType::Indent | TokenType::Dedent | TokenType::Semi)
        || (S::SYNTAX.delimited() && matches!(tokens[i - 1].kind, TokenType::BlockOpen | TokenType::BlockClose))
        || (S::SYNTAX.indent() && tokens[i - 1].kind == TokenType::Colon && ends_header(tokens, i - 1))
}

/* whether the colon at `c` closes a header whose body may share its line */
fn ends_header(tokens: &[Token], c: usize) -> bool {
    let mut depth = 0usize;
    let mut j = c;
    while j > 0 {
        j -= 1;
        match tokens[j].kind {
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth += 1,
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen if depth == 0 => return false,
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth -= 1,
            // a line end ends the statement however its brackets stand
            TokenType::Newline | TokenType::Indent | TokenType::Dedent | TokenType::Semi => return opens_body(tokens[j + 1].kind),
            // the statement the colon ends starts past the last header colon before it
            TokenType::Colon if depth == 0 => return opens_body(tokens[j + 1].kind),
            _ => {}
        }
    }
    opens_body(tokens[0].kind)
}

/* whether an unbracketed colon precedes `=` or a line end and the tokens read */
fn opens_header<S: Spelling>(tokens: &[Token], i: usize) -> (bool, usize) {
    let mut depth = 0usize;
    for (j, t) in tokens.iter().enumerate().skip(i + 1) {
        match t.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth = depth.saturating_sub(1),
            TokenType::Colon if S::SYNTAX.indent() && depth == 0 => return (j > i + 1, j - i),
            TokenType::BlockOpen if S::SYNTAX.delimited() && depth == 0 => return (j > i + 1, j - i),
            TokenType::BlockOpen | TokenType::BlockClose if S::SYNTAX.delimited() => return (false, j - i),
            TokenType::Newline | TokenType::Semi | TokenType::Eof => return (false, j - i),
            TokenType::Equal if depth == 0 => return (false, j - i),
            _ => {}
        }
    }
    (false, tokens.len() - i)
}

// whether a soft definition word starts a statement right before a name
fn defines<S: Spelling>(tokens: &[Token], i: usize) -> bool {
    let after_async = i > 0 && tokens[i - 1].kind == TokenType::Async;
    (starts_stmt::<S>(tokens, i) || after_async)
        && tokens.get(i + 1).is_some_and(|t| matches!(t.kind, TokenType::Name | TokenType::Underscore | TokenType::Match | TokenType::Case | TokenType::Type | TokenType::Fn))
}

// a position no token holds in the fields of a pass
const NONE: u32 = u32::MAX;

// the later of two positions where `NONE` holds none
#[inline]
fn later(a: u32, b: u32) -> u32 {
    if a == NONE { b } else if b == NONE { a } else { a.max(b) }
}

/* the first token from `*at` before `end` that is no line break or comment */
fn first_sig(tokens: &[Token], at: &mut u32, end: usize) -> Option<usize> {
    let mut p = *at as usize;
    while p < end && matches!(tokens[p].kind, TokenType::LineBreak | TokenType::Comment) { p += 1; }
    // the skipped run is never scanned again by a later ask
    *at = p as u32;
    (p < end).then_some(p)
}

/* what a bracket level or the statement around it holds as the pass goes */
#[derive(Clone, Copy)]
struct Level {
    // the token after the opener where the first item of a bracket starts
    first: u32,
    // where items start after commas sorted by the lambda marks after them
    plain: u32,
    barred: u32,
    kept: u32,
    // where an `=` here finds its head and none in a bracket before an arm
    start: u32,
    // the first colon since the start and the header colon past any question marks
    colon: u32,
    header: u32,
    // the token after the last colon or line end here which a header colon reads
    seg: u32,
    // where the body after the header colon was last looked for and what was found
    body_at: u32,
    body: u32,
    // the question marks still open and the bars since the last colon
    open: u16,
    bars: u16,
    // the header colon and open marks counted from the statement start past any arm
    header_s: u32,
    open_s: u16,
    // whether a lambda word sits here since the start so an `=` gives its default
    lambda: bool,
}

impl Level {
    const fn statement(at: u32) -> Level {
        Level { first: NONE, plain: NONE, barred: NONE, kept: NONE, start: at, colon: NONE, header: NONE, seg: at, body_at: NONE, body: NONE, open: 0, bars: 0, header_s: NONE, open_s: 0, lambda: false }
    }

    const fn bracket(open: u32) -> Level {
        Level { first: open + 1, start: NONE, seg: NONE, ..Level::statement(NONE) }
    }
}

/* the forward pass over the scanned tokens with a level per open bracket */
struct Pass {
    // the levels the open brackets suspend below the one the cursor sits in
    outer: Vec<Level>,
    top: Level,
    // where the statement the cursor sits in starts
    stmt: u32,
    // whether the next token starts a statement
    starts: bool,
    // soft heads waiting on the token that settles them in groups of one depth
    depth: i32,
    heads: Vec<u32>,
    groups: Vec<(i32, u32)>,
}

impl Pass {
    fn new() -> Pass {
        Pass { outer: Vec::new(), top: Level::statement(0), stmt: 0, starts: true, depth: 0, heads: Vec::new(), groups: Vec::new() }
    }

    /* resolves the token at `i` and moves the levels past it */
    #[inline]
    fn step<S: Spelling>(&mut self, tokens: &mut [Token], i: usize) {
        let raw = tokens[i].kind;
        if Scanner::<S>::SOFT_HEADS && !self.heads.is_empty() { self.settle::<S>(tokens, i, raw); }
        // `match` and `case` stay keywords only in a header and `type` only in an alias
        let kind = match raw {
            TokenType::Match | TokenType::Case if Scanner::<S>::SOFT_HEADS => {
                if self.starts { self.wait(i); raw } else { TokenType::Name }
            }
            TokenType::Type if Scanner::<S>::SOFT_TYPE && !(self.starts && tokens.get(i + 1).is_some_and(|t| t.kind == TokenType::Name)) => TokenType::Name,
            TokenType::Fn if Scanner::<S>::SOFT_FN && !self.defines(tokens, i) => TokenType::Name,
            _ => raw,
        };
        tokens[i].kind = kind;
        match kind {
            TokenType::For => if let Some(first) = self.item(tokens, i) { tokens[first].comp = true; },
            TokenType::Equal => if let Some(head) = self.head::<S>(tokens, i) { tokens[head].assigns = tokens[head].assigns.saturating_add(1); },
            _ => {}
        }
        self.shape::<S>(tokens, i, kind);
    }

    /* moves the levels past the token at `i` of final kind `kind` */
    #[inline]
    fn shape<S: Spelling>(&mut self, tokens: &[Token], i: usize, kind: TokenType) {
        let at = i as u32;
        if matches!(kind, TokenType::Newline | TokenType::Indent | TokenType::Dedent | TokenType::Semi)
            || (S::SYNTAX.delimited() && matches!(kind, TokenType::BlockOpen | TokenType::BlockClose)) {
            return self.cut(at + 1);
        }
        let Pass { outer, top, stmt, starts, .. } = self;
        let mut ends = false;
        match kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => {
                outer.push(*top);
                *top = Level::bracket(at);
            }
            // an unmatched closer leaves the statement level fresh but for its header count
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => *top = match outer.pop() {
                Some(level) => level,
                // a question table finds its header colon from the head whatever closers come between
                None if matches!(S::SYNTAX.shapes.cond, Cond::Question) => Level { header: top.header_s, open: top.open_s, header_s: top.header_s, open_s: top.open_s, ..Level::statement(*stmt) },
                None => Level::statement(*stmt),
            },
            TokenType::Comma => top.plain = at + 1,
            TokenType::Colon => {
                (top.kept, top.plain, top.barred) = (later(top.kept, top.barred), NONE, NONE);
                if top.colon == NONE { top.colon = at; }
                if matches!(S::SYNTAX.shapes.cond, Cond::Question) && top.open > 0 { top.open -= 1; } else if top.header == NONE { top.header = at; }
                if matches!(S::SYNTAX.shapes.cond, Cond::Question) && top.open_s > 0 { top.open_s -= 1; } else if top.header_s == NONE { top.header_s = at; }
                top.bars = 0;
                // a colon ends a header when the segment it closes opens with a body word
                ends = S::SYNTAX.indent() && top.seg != NONE && tokens.get(top.seg as usize).is_some_and(|t| opens_body(t.kind));
                top.seg = at + 1;
            }
            TokenType::FnExpr => (top.kept, top.plain, top.barred, top.lambda) = (later(top.kept, top.plain), NONE, NONE, true),
            TokenType::BitOr if matches!(S::SYNTAX.shapes.lambda, Lambda::Pipes) => {
                (top.plain, top.barred) = (top.barred, top.plain);
                top.bars = top.bars.wrapping_add(1);
            }
            TokenType::Arm if S::SYNTAX.shapes.arms => *top = Level { start: at + 1, colon: NONE, header: NONE, body_at: NONE, open: 0, bars: 0, lambda: false, ..*top },
            TokenType::Question if matches!(S::SYNTAX.shapes.cond, Cond::Question) => (top.open, top.open_s) = (top.open.saturating_add(1), top.open_s.saturating_add(1)),
            _ => {}
        }
        *starts = ends;
    }

    // a statement starts at `at` with every bracket before it forgotten
    fn cut(&mut self, at: u32) {
        self.outer.clear();
        (self.top, self.stmt, self.starts) = (Level::statement(at), at, true);
    }

    /* the first token of the item a `for` at `i` ends in the innermost bracket */
    fn item(&mut self, tokens: &[Token], i: usize) -> Option<usize> {
        let top = &mut self.top;
        if top.first == NONE { return None; }
        // commas a lambda word passed alone separate items while its parameters hold the others
        let slot = if top.plain != NONE && (top.kept == NONE || top.plain > top.kept) { &mut top.plain } else if top.kept != NONE { &mut top.kept } else { &mut top.first };
        first_sig(tokens, slot, i)
    }

    /* the head whose targets an `=` at `i` ends and none for keyword arguments */
    fn head<S: Spelling>(&mut self, tokens: &[Token], i: usize) -> Option<usize> {
        let top = &mut self.top;
        if top.lambda || top.start == NONE || (matches!(S::SYNTAX.shapes.lambda, Lambda::Pipes) && top.bars % 2 == 1) { return None; }
        let head = first_sig(tokens, &mut top.start, i)?;
        if top.colon == NONE || !opens_body(tokens[head].kind) { return Some(head); }
        // a compound header's `=` counts on the first token of the body on its line
        let from = if top.header == NONE { top.colon } else { top.header } + 1;
        if top.body_at != from { (top.body_at, top.body) = (from, from); }
        first_sig(tokens, &mut top.body, i)
    }

    // whether a soft definition word at `i` starts a statement right before a name
    fn defines(&self, tokens: &[Token], i: usize) -> bool {
        let after_async = i > 0 && tokens[i - 1].kind == TokenType::Async;
        (self.starts || after_async)
            && tokens.get(i + 1).is_some_and(|t| matches!(t.kind, TokenType::Name | TokenType::Underscore | TokenType::Match | TokenType::Case | TokenType::Type | TokenType::Fn))
    }

    // a soft head at `i` waits at depth zero for the token that settles it
    fn wait(&mut self, i: usize) {
        if self.heads.is_empty() { self.depth = 0; self.groups.clear(); }
        if self.groups.last().is_none_or(|&(base, _)| base != self.depth) { self.groups.push((self.depth, self.heads.len() as u32)); }
        self.heads.push(i as u32);
    }

    /* settles the soft heads the token at `j` ends and tracks the others' depth */
    fn settle<S: Spelling>(&mut self, tokens: &mut [Token], j: usize, kind: TokenType) {
        match kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => self.depth += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => {
                self.depth -= 1;
                // heads that would sink below depth zero stay there and join the group under them
                let n = self.groups.len();
                if n > 0 && self.groups[n - 1].0 > self.depth {
                    self.groups[n - 1].0 = self.depth;
                    if n > 1 && self.groups[n - 2].0 == self.depth { self.groups.pop(); }
                }
            }
            TokenType::Newline | TokenType::Semi | TokenType::Eof => self.settle_all(tokens),
            TokenType::BlockClose if S::SYNTAX.delimited() => self.settle_all(tokens),
            TokenType::BlockOpen if S::SYNTAX.delimited() => { self.settle_top(tokens, j, true); self.settle_all(tokens); }
            TokenType::Colon if S::SYNTAX.indent() => self.settle_top(tokens, j, true),
            TokenType::Equal => self.settle_top(tokens, j, false),
            _ => {}
        }
    }

    // heads at depth zero stay words when `keeps` and a token sits before the colon
    fn settle_top(&mut self, tokens: &mut [Token], j: usize, keeps: bool) {
        let Some(&(base, from)) = self.groups.last() else { return };
        if base != self.depth { return; }
        for &q in &self.heads[from as usize..] {
            if !(keeps && j > q as usize + 1) { tokens[q as usize].kind = TokenType::Name; }
        }
        self.heads.truncate(from as usize);
        self.groups.pop();
    }

    // every head still waiting reads as a name since no header colon came
    fn settle_all(&mut self, tokens: &mut [Token]) {
        for &q in &self.heads { tokens[q as usize].kind = TokenType::Name; }
        self.heads.clear();
        self.groups.clear();
    }
}
