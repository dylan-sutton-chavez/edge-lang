use edge::lexer::{lex_as, opens_body, Spelling, Token, TokenType};

use std::marker::PhantomData;

use crate::Renames;

// why a program has no faithful spelling in the other table and where
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untranslatable {
    pub at: usize,
    pub why: String,
}

// `source` lexed in spelling `F` and printed token for token in spelling `T`
pub fn translate<F: Spelling, T: Spelling>(source: &str) -> Result<String, Untranslatable> {
    translate_renamed::<F, T>(source, Renames::NONE, true)
}

// the same with the names of `renames` taken forward or back
pub fn translate_renamed<F: Spelling, T: Spelling>(source: &str, renames: Renames, forward: bool) -> Result<String, Untranslatable> {
    let (tokens, errors) = lex_as::<F>(source);
    // a program its own spelling cannot lex has no faithful respelling
    if let Some(e) = errors.first() { return Err(Untranslatable { at: e.start, why: e.msg.to_string() }); }
    let out = Printer::<F, T>::new(source, &tokens, renames, forward).run()?;
    // the output must lex to the same tokens at the same block depths
    let (want, got) = (shape::<F>(source), shape::<T>(&out));
    let at = want.iter().zip(&got).position(|(w, g)| (w.0, w.1) != (g.0, g.1)).unwrap_or(want.len().min(got.len()));
    if at < want.len() || at < got.len() {
        let pos = want.get(at).map_or(source.len(), |w| w.2);
        let why = match (want.get(at), got.get(at)) {
            (Some(w), Some(g)) if w.0 != g.0 => format!("lexes as {} in {} where {} had {}", T::SYNTAX.describe(g.0), T::SYNTAX.name, F::SYNTAX.name, F::SYNTAX.describe(w.0)),
            (Some(_), Some(_)) => format!("lands in another block in {}", T::SYNTAX.name),
            _ => format!("lexes to a different number of tokens in {}", T::SYNTAX.name),
        };
        return Err(Untranslatable { at: pos, why });
    }
    Ok(out)
}

// each token but layout with its block depth and offset and `pass` left out
fn shape<S: Spelling>(source: &str) -> Vec<(TokenType, u32, usize)> {
    let (tokens, _) = lex_as::<S>(source);
    let colons = if S::SYNTAX.braces() { Vec::new() } else { block_colons(&tokens) };
    let (mut depth, mut inline, mut out) = (0u32, false, Vec::new());
    for (i, t) in tokens.iter().enumerate() {
        match t.kind {
            TokenType::Indent | TokenType::BlockOpen => depth += 1,
            TokenType::Dedent | TokenType::BlockClose => depth = depth.saturating_sub(1),
            TokenType::Newline if inline => { depth = depth.saturating_sub(1); inline = false; }
            TokenType::Colon if colons.get(i).copied().flatten().is_some() => {
                if colons[i] == Some(true) { depth += 1; inline = true; }
            }
            TokenType::Newline | TokenType::LineBreak | TokenType::Comment | TokenType::Eof | TokenType::Empty => {}
            k => out.push((k, depth, t.start)),
        }
    }
    out
}

// the block colons of an indentation spelling and whether code follows each on its line
fn block_colons(tokens: &[Token]) -> Vec<Option<bool>> {
    let mut colons = vec![None; tokens.len()];
    let (mut head, mut fresh, mut done, mut depth, mut lambdas) = (TokenType::Newline, true, false, 0usize, 0usize);
    for (i, t) in tokens.iter().enumerate() {
        match t.kind {
            TokenType::Newline => { (fresh, depth) = (true, 0); continue; }
            TokenType::Indent | TokenType::Dedent | TokenType::Semi => { fresh = true; continue; }
            TokenType::Comment | TokenType::LineBreak => continue,
            _ => {}
        }
        if fresh { (head, fresh, done, lambdas) = (t.kind, false, false, 0); }
        match t.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth = depth.saturating_sub(1),
            TokenType::FnExpr if depth == 0 => lambdas += 1,
            TokenType::Colon if depth == 0 && lambdas > 0 => lambdas -= 1,
            TokenType::Colon if depth == 0 && !done && opens_body(head) => {
                let next = tokens[i + 1..].iter().find(|t| t.kind != TokenType::Comment).map_or(TokenType::Eof, |t| t.kind);
                colons[i] = Some(!matches!(next, TokenType::Newline | TokenType::LineBreak | TokenType::Eof));
                (done, fresh) = (true, true);
            }
            _ => {}
        }
    }
    colons
}

// whether a clause such as `else` continues the statement its block closed
fn continues(kind: TokenType) -> bool {
    matches!(kind, TokenType::Else | TokenType::ElseIf | TokenType::Catch | TokenType::Finally)
}

fn word_char(c: char) -> bool { c.is_alphanumeric() || c == '_' || !c.is_ascii() }

// whether two characters written side by side would lex as one token
fn joins(a: char, b: char) -> bool { (word_char(a) && word_char(b)) || (a == '/' && b == '/') }

// the leading blanks of the last line of `text`
fn indent_of(text: &str) -> &str {
    let line = &text[text.rfind('\n').map_or(0, |n| n + 1)..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

// an f-string open in the walk with the state of its current replacement field
#[derive(Clone, Copy, Default)]
struct Field {
    open: bool,
    depth: u32,
    spec: bool,
    respelled: bool,
}

// a block the walk opened with the indentation of its header line
struct Open {
    indent: String,
    inline: bool,
    colon: usize,
    body: bool,
}

struct Printer<'a, F: Spelling, T: Spelling> {
    src: &'a str,
    toks: &'a [Token],
    out: String,
    // the source offset copied so far and the output length after the last code token
    at: usize,
    code_end: usize,
    fields: Vec<Field>,
    blocks: Vec<Open>,
    // whether the next token starts a statement or a line and the statement indentation
    fresh: bool,
    line_start: bool,
    stmt_indent: String,
    // a `not` spelled `!` takes the next operand without the blank between them
    squeeze: bool,
    respelled: bool,
    // a symbol spelled as a word wants a blank on each side
    pad: bool,
    nest: u32,
    // a line holding only a closing brace leaves no blank line behind
    drop_line: bool,
    // the output sits at a line the walk broke and indented already
    broke: bool,
    reindent: bool,
    // the names a respell renames and the way it reads their table
    renames: Renames,
    forward: bool,
    spellings: PhantomData<(F, T)>,
}

impl<'a, F: Spelling, T: Spelling> Printer<'a, F, T> {
    const INTO_BRACES: bool = !F::SYNTAX.braces() && T::SYNTAX.braces();
    const INTO_INDENT: bool = F::SYNTAX.braces() && !T::SYNTAX.braces();

    fn new(src: &'a str, toks: &'a [Token], renames: Renames, forward: bool) -> Self {
        Self {
            src, toks, out: String::with_capacity(src.len() + src.len() / 8), at: 0, code_end: 0,
            fields: Vec::new(), blocks: Vec::new(), fresh: true, line_start: true, stmt_indent: String::new(),
            squeeze: false, respelled: false, pad: false, nest: 0, drop_line: false, broke: false,
            reindent: Self::INTO_INDENT && (!consistent(toks, src) || (0..toks.len()).any(|i| toks[i].kind == TokenType::BlockOpen && inline_block(toks, i) == Some(false))),
            renames, forward,
            spellings: PhantomData,
        }
    }

    fn fail(&self, at: usize, why: String) -> Untranslatable { Untranslatable { at, why } }

    fn run(mut self) -> Result<String, Untranslatable> {
        let colons = if Self::INTO_BRACES { block_colons(self.toks) } else { Vec::new() };
        let mut i = 0;
        while i < self.toks.len() {
            let t = self.toks[i];
            match t.kind {
                TokenType::Dedent if Self::INTO_BRACES => { i = self.dedents(i); continue; }
                TokenType::Colon if colons.get(i).copied().flatten().is_some() => self.open_brace(i, colons[i] == Some(true))?,
                TokenType::BlockOpen if Self::INTO_INDENT => self.open_colon(i)?,
                TokenType::BlockClose if Self::INTO_INDENT => self.close_brace(i)?,
                TokenType::Newline if Self::INTO_BRACES && self.blocks.last().is_some_and(|b| b.inline) => {
                    self.blocks.pop();
                    self.close_after_code();
                    self.layout(t);
                }
                TokenType::Indent | TokenType::Dedent => { self.gap(t.start, false); self.at = t.end; }
                TokenType::Newline | TokenType::LineBreak => self.layout(t),
                TokenType::Semi => { self.token(i)?; self.fresh = true; }
                TokenType::Comment => self.comment(t),
                TokenType::Eof => {
                    while Self::INTO_BRACES && self.blocks.pop().is_some() { self.close_after_code(); }
                    self.gap(t.start, false);
                }
                _ => self.token(i)?,
            }
            i += 1;
        }
        Ok(self.out)
    }

    // copies the blanks before `to` or the indentation a reindented line takes instead
    fn gap(&mut self, to: usize, indent: bool) {
        let gap = self.src.get(self.at..to).unwrap_or("");
        if (self.squeeze && gap == " ") || (self.broke && !gap.contains('\n')) { self.at = to; return; }
        if indent && self.reindent && self.line_start && !gap.contains('\n') {
            let depth = self.blocks.iter().filter(|b| !b.inline).count();
            self.out.push_str(&"    ".repeat(depth));
        } else {
            self.out.push_str(gap);
        }
        self.at = to;
    }

    // a line break or a blank line copied as written
    fn layout(&mut self, t: Token) {
        let dropped = core::mem::take(&mut self.drop_line) && matches!(t.kind, TokenType::Newline | TokenType::LineBreak);
        if dropped { self.at = t.end; } else {
            self.gap(t.start, false);
            self.out.push_str(&self.src[t.start..t.end]);
            self.at = t.end;
        }
        (self.squeeze, self.pad) = (false, false);
        (self.fresh, self.line_start) = (self.fresh || t.kind == TokenType::Newline, self.nest == 0);
    }

    fn comment(&mut self, t: Token) {
        self.gap(t.start, true);
        let text = &self.src[t.start..t.end];
        // a line comment takes the target's first marker and any other comment stays as written
        let line = F::SYNTAX.comments.line.iter().find_map(|m| text.strip_prefix(m));
        let mark = T::SYNTAX.comments.line.first().copied().filter(|_| line.is_some()).unwrap_or("");
        if let (Some(a), Some(b)) = (self.out.chars().next_back(), mark.chars().next().or(text.chars().next())) && joins(a, b) { self.out.push(' '); }
        self.out.push_str(mark);
        self.out.push_str(if mark.is_empty() { text } else { line.unwrap_or(text) });
        (self.at, self.squeeze, self.line_start, self.drop_line) = (t.end, false, false, false);
    }

    // starts a fresh line at `indent` for what needs a line of its own
    fn newline(&mut self, indent: &str) {
        self.out.truncate(self.out.trim_end_matches([' ', '\t']).len());
        self.out.push('\n');
        self.out.push_str(indent);
        self.broke = true;
    }

    fn depth_indent(&self) -> String { "    ".repeat(self.blocks.iter().filter(|b| !b.inline).count()) }

    fn token(&mut self, i: usize) -> Result<(), Untranslatable> {
        let t = self.toks[i];
        let text = &self.src[t.start..t.end];
        // a compound statement after code on its line moves to a line of its own
        if Self::INTO_INDENT && self.fresh && opens_body(t.kind) && !self.line_start && !self.broke && self.nest == 0 {
            let indent = self.depth_indent();
            self.newline(&indent);
        }
        let in_spec = self.fields.last().is_some_and(|f| f.open && f.spec && f.depth == 0);
        let new = match t.kind {
            _ if in_spec => text,
            TokenType::Name => {
                let name = self.renames.name(text, self.forward).unwrap_or(text);
                if let Some(k) = T::SYNTAX.word(name) && !soft::<T>(k) {
                    return Err(self.fail(t.start, format!("the name '{name}' is a reserved word of {}", T::SYNTAX.name)));
                }
                name
            }
            TokenType::Int | TokenType::Float | TokenType::String | TokenType::Bytes
            | TokenType::FormatStart | TokenType::FormatText | TokenType::FormatEnd => text,
            k if F::SYNTAX.spelling(k) == T::SYNTAX.spelling(k) => text,
            k => T::SYNTAX.spelling(k).ok_or_else(|| self.fail(t.start, format!("{} has no spelling for {}", T::SYNTAX.name, F::SYNTAX.describe(k))))?,
        };
        let respelled = new != text;
        self.field(i, respelled)?;
        if let Some(b) = self.blocks.last_mut() { b.body = true; }
        let tight = self.at == t.start;
        self.gap(t.start, true);
        if self.fresh { self.stmt_indent = indent_of(&self.out).to_string(); }
        // only a respelled side can run into its neighbour since the source lexed them apart
        let touching = core::mem::replace(&mut self.respelled, respelled) || respelled;
        let last = self.out.chars().next_back();
        let worded = respelled && new.chars().next().is_some_and(word_char) && !text.chars().next().is_some_and(word_char);
        let pad = tight && (core::mem::take(&mut self.pad) && !new.starts_with([')', ']', '}', ',', ':'])
            || worded && t.kind != TokenType::Not && last.is_some_and(|c| !c.is_whitespace() && !matches!(c, '(' | '[' | '{')));
        let joined = touching && last.zip(new.chars().next()).is_some_and(|(a, b)| joins(a, b));
        if pad || joined { self.out.push(' '); }
        self.out.push_str(new);
        self.pad = worded;
        match t.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => self.nest += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => self.nest = self.nest.saturating_sub(1),
            _ => {}
        }
        self.squeeze = t.kind == TokenType::Not && self.fields.is_empty() && !new.chars().next_back().is_some_and(word_char);
        (self.at, self.code_end, self.fresh, self.line_start, self.drop_line, self.broke) = (t.end, self.out.len(), false, false, false, false);
        Ok(())
    }

    // tracks the replacement fields of f-strings so a format spec copies through unchanged
    fn field(&mut self, i: usize, respelled: bool) -> Result<(), Untranslatable> {
        let t = self.toks[i];
        if t.kind == TokenType::FormatStart { self.fields.push(Field::default()); return Ok(()); }
        if t.kind == TokenType::FormatEnd { self.fields.pop(); return Ok(()); }
        let next = self.toks.get(i + 1).map_or(TokenType::Eof, |t| t.kind);
        let Some(f) = self.fields.last_mut() else { return Ok(()) };
        if !f.open {
            if t.kind == TokenType::BraceOpen { *f = Field { open: true, ..Field::default() }; }
            return Ok(());
        }
        match t.kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => f.depth += 1,
            TokenType::BraceClose if f.depth == 0 => *f = Field::default(),
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => f.depth = f.depth.saturating_sub(1),
            TokenType::Colon if f.depth == 0 => f.spec = true,
            // `f"{x=}"` keeps the field text as a constant so a respelled field changes the program
            TokenType::Equal if f.depth == 0 && !f.spec && matches!(next, TokenType::BraceClose | TokenType::Convert | TokenType::Colon) && f.respelled => {
                return Err(self.fail(t.start, format!("the text of an `=` field spells differently in {}", T::SYNTAX.name)));
            }
            _ => {}
        }
        f.respelled |= respelled;
        Ok(())
    }

    fn close_after_code(&mut self) {
        self.out.insert_str(self.code_end, " }");
        self.code_end += 2;
    }

    fn open_brace(&mut self, i: usize, inline: bool) -> Result<(), Untranslatable> {
        let t = self.toks[i];
        let mut rest = self.toks[i + 1..].iter().filter(|t| !matches!(t.kind, TokenType::Comment | TokenType::LineBreak));
        let next = rest.next().map_or(TokenType::Eof, |t| t.kind);
        // braces would give a meaning to what the indentation spelling rejects
        if inline && opens_body(next) {
            return Err(self.fail(t.start, format!("{} rejects a compound statement after a colon on its line", F::SYNTAX.name)));
        }
        if !inline && !(next == TokenType::Newline && rest.next().is_some_and(|t| t.kind == TokenType::Indent)) {
            return Err(self.fail(t.start, format!("{} expects an indented block after this colon", F::SYNTAX.name)));
        }
        let gap = &self.src[self.at..t.start];
        if gap.is_empty() && !self.out.ends_with([' ', '\t']) { self.out.push(' '); } else { self.out.push_str(gap); }
        self.out.push('{');
        self.blocks.push(Open { indent: self.stmt_indent.clone(), inline, colon: self.out.len(), body: false });
        (self.at, self.code_end, self.fresh, self.line_start, self.squeeze) = (t.end, self.out.len(), true, false, false);
        Ok(())
    }

    // a run of dedents closes each block after its code or before a continuing clause
    fn dedents(&mut self, from: usize) -> usize {
        let mut end = from;
        while self.toks.get(end).is_some_and(|t| t.kind == TokenType::Dedent) { end += 1; }
        let next = self.toks.get(end).map_or(TokenType::Eof, |t| t.kind);
        for k in from..end {
            let Some(open) = self.blocks.pop() else { continue };
            if k + 1 == end && continues(next) && !open.inline {
                self.gap(self.toks[k].start, false);
                self.out.push_str("} ");
                self.code_end = self.out.len();
            } else {
                self.close_after_code();
            }
        }
        end
    }

    // a `{` turns into the header colon and moves a body down when it must
    fn open_colon(&mut self, i: usize) -> Result<(), Untranslatable> {
        let t = self.toks[i];
        let gap = &self.src[self.at..t.start];
        if !gap.trim_matches([' ', '\t']).is_empty() { self.out.push_str(gap); }
        self.out.push(':');
        let shared = inline_block(self.toks, i);
        if let Some(b) = self.blocks.last_mut() { b.body = true; }
        self.blocks.push(Open { indent: self.stmt_indent.clone(), inline: shared == Some(true), colon: self.out.len(), body: false });
        (self.at, self.code_end, self.fresh, self.line_start, self.squeeze, self.broke) = (t.end, self.out.len(), true, false, false, false);
        if shared == Some(false) {
            let indent = self.depth_indent();
            self.newline(&indent);
        }
        Ok(())
    }

    // a `}` leaves nothing but the line break a clause continuing its statement needs
    fn close_brace(&mut self, i: usize) -> Result<(), Untranslatable> {
        let t = self.toks[i];
        let open = self.blocks.pop().unwrap_or(Open { indent: String::new(), inline: false, colon: self.out.len(), body: true });
        if !open.body {
            self.out.insert_str(open.colon, " pass");
            if self.code_end >= open.colon { self.code_end += 5; }
        }
        let alone = self.line_start;
        let next = self.toks.get(i + 1).copied();
        let kind = next.map_or(TokenType::Eof, |n| n.kind);
        if let Some(n) = next && continues(kind) && n.line == t.line {
            if alone { self.out.push_str(&open.indent); self.broke = true; } else { self.newline(&open.indent); }
            (self.at, self.fresh) = (n.start, true);
            self.stmt_indent = open.indent;
            return Ok(());
        }
        if !matches!(kind, TokenType::Newline | TokenType::LineBreak | TokenType::Comment | TokenType::Eof | TokenType::BlockClose) && next.is_some_and(|n| n.line == t.line) {
            return Err(self.fail(t.start, "a statement shares the line after a closing brace".to_string()));
        }
        self.at = t.end;
        self.drop_line = alone && matches!(kind, TokenType::Newline | TokenType::LineBreak | TokenType::BlockClose | TokenType::Eof);
        self.fresh = true;
        Ok(())
    }
}

// whether code follows the `{` at `i` and then whether it all fits one line
fn inline_block(toks: &[Token], i: usize) -> Option<bool> {
    let line = toks[i].line;
    let next = toks[i + 1..].iter().find(|t| t.kind != TokenType::Comment)?;
    if matches!(next.kind, TokenType::Newline | TokenType::LineBreak | TokenType::Eof) { return None; }
    let mut depth = 0u32;
    for t in &toks[i + 1..] {
        match t.kind {
            TokenType::BlockOpen => return Some(false),
            TokenType::BlockClose if depth == 0 => return Some(t.line == line),
            TokenType::Newline | TokenType::LineBreak | TokenType::Eof if depth == 0 => return Some(false),
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => depth += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Some(false)
}

// a reserved word a name may still be where the grammar wants no keyword
fn soft<S: Spelling>(kind: TokenType) -> bool {
    kind == TokenType::Underscore || S::SYNTAX.is_soft(kind)
}

// whether a braces source indents every block deeper than its header and alike throughout
fn consistent(toks: &[Token], src: &str) -> bool {
    let mut levels: Vec<Option<&str>> = vec![Some("")];
    let (mut line_start, mut nest) = (true, 0u32);
    for (i, t) in toks.iter().enumerate() {
        let first = core::mem::replace(&mut line_start, matches!(t.kind, TokenType::Newline | TokenType::LineBreak)) && nest == 0;
        let layout = matches!(t.kind, TokenType::Comment | TokenType::Newline | TokenType::LineBreak | TokenType::Eof | TokenType::BlockClose);
        // a clause after `}` takes its indentation from the brace
        if first && !layout && !(continues(t.kind) && i > 0 && toks[i - 1].kind == TokenType::BlockClose) {
            let ind = indent_of(&src[..t.start]);
            let outer = levels.iter().rev().skip(1).find_map(|l| *l).unwrap_or("");
            match levels.last_mut() {
                Some(Some(level)) if *level != ind => return false,
                Some(slot @ None) if ind.len() > outer.len() && ind.starts_with(outer) => *slot = Some(ind),
                Some(None) => return false,
                _ => {}
            }
        }
        match t.kind {
            TokenType::BlockOpen => levels.push(None),
            TokenType::BlockClose => { levels.pop(); }
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => nest += 1,
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => nest = nest.saturating_sub(1),
            _ => {}
        }
    }
    true
}
