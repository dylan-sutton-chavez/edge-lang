use super::{Token, TokenType, LexError, opens_body};
use super::tables::*;
use super::syntax::{join, joined, Blocks, Ends, Interp, Literal, Phrase, Quote, Spelling, Suffix, Unicode, Unknown, MAX_PAIRS, MAX_PHRASES, BYTES, FORMAT, RAW};

use alloc::vec::Vec;
use core::marker::PhantomData;

const MAX_INDENT_DEPTH: usize = 100;
const MAX_FIELD_DEPTH: usize = 200;

// a token as its kind and line and byte span
pub(super) type Tok = (TokenType, usize, usize, usize);

// a field of an interpolated literal the scan sits inside
#[derive(Clone, Copy)]
struct Field {
    quote: u8,
    triple: bool,
    decodes: bool,
    // the bracket nesting at which the field's close byte ends it
    nesting: u32,
    // whether the field sits in the spec of the field under it
    in_spec: bool,
}

// what a table of block words keeps between tokens
struct Words {
    // the word leading a header whose block has not opened yet
    header: Option<TokenType>,
    // the word that led the header of each open block innermost last
    clauses: Vec<TokenType>,
    // conditional expressions of the statement still waiting for their else
    conditions: u32,
    // whether the next token starts a statement
    fresh: bool,
    depth: u32,
}

pub(super) struct Scanner<'a, S: Spelling> {
    pub src: &'a [u8],
    pub pos: usize,
    /* a stack of tokens for states that emit several at one position like dedents */
    pub pending: Vec<Tok>,
    pub indent_stack: Vec<usize>,
    pub nesting: u32,
    pub line: usize,
    // each open field of an interpolated literal innermost last
    fields: Vec<Field>,
    /* lex errors kept beside the tokens so the parser folds them into one report */
    pub errors: Vec<LexError>,
    // a bit per open brace under a leading one set where a block opened
    braces: usize,
    // the full words of brace bits under the newest
    spilled: Vec<usize>,
    words: Words,
    // the tokens the continuation check read and the last one not a break or comment
    sig: (usize, Option<TokenType>),
    spelling: PhantomData<S>,
}

// how many tokens the last lead search covered and the lead it found
#[derive(Clone, Copy)]
pub(super) struct Lead {
    seen: usize,
    at: usize,
}

impl Lead {
    pub(super) const NEW: Lead = Lead { seen: 0, at: usize::MAX };

    // the first kept token of the statement searching only tokens added since the last search
    fn of(&mut self, tokens: &[Token]) -> TokenType {
        if let Some(j) = tokens[self.seen..].iter().rposition(|t| ends_stmt(t.kind)) {
            let from = self.seen + j + 1;
            self.at = (from..tokens.len()).find(|&i| !matches!(tokens[i].kind, TokenType::Comment | TokenType::LineBreak)).unwrap_or(usize::MAX);
        } else if self.at == usize::MAX {
            self.at = (self.seen..tokens.len()).find(|&i| !matches!(tokens[i].kind, TokenType::Comment | TokenType::LineBreak)).unwrap_or(usize::MAX);
        }
        self.seen = tokens.len();
        tokens.get(self.at).map_or(TokenType::Newline, |t| t.kind)
    }
}

// the roles a byte the scan sets apart may play
const COMMENT: u8 = 1;
const BRACE: u8 = 2;
const CONVERT: u8 = 4;
const CLOSE: u8 = 8;
const PAIR: u8 = 16;

// a table of flags by byte from a list of bytes
const fn flags(bytes: &[u8]) -> [bool; 256] {
    let mut t = [false; 256];
    let mut i = 0;
    while i < bytes.len() { t[bytes[i] as usize] = true; i += 1; }
    t
}

impl<'a, S: Spelling> Scanner<'a, S> {
    // a table that cannot work fails to compile here
    const CHECKED: () = S::SYNTAX.check();
    const INDENT: bool = S::SYNTAX.indent();
    // a braces table keeps `{` a lone symbol so braces skip the operator table
    const BRACES: bool = {
        let (row, e) = (S::SYNTAX.operators[b'{' as usize], TokenType::Eof as u8);
        let lone = row[0] as u8 == TokenType::BraceOpen as u8 && row[1] as u8 == e && row[2] as u8 == e && row[3] as u8 == e && !Self::PAIRED[b'{' as usize];
        assert!(lone || !S::SYNTAX.braces(), "a braces table keeps its open brace a symbol of its own");
        S::SYNTAX.braces()
    };
    pub(super) const WORDS: bool = matches!(S::SYNTAX.blocks, Blocks::Words { .. });
    const IMPLICIT: bool = matches!(S::SYNTAX.blocks, Blocks::Words { implicit: true });
    // whether a line end outside brackets may end a statement
    const LINES: bool = !matches!(S::SYNTAX.ends, Ends::Semicolon);
    const PHRASES: bool = S::SYNTAX.phrases();
    // names scan for phrases and bytes for pair symbols only where one may start
    const HEADS: &'static [bool; 128] = &S::SYNTAX.heads();
    const PAIRED: &'static [bool; 128] = &S::SYNTAX.pair_starts();
    const PHRASE_LIST: &'static ([Phrase; MAX_PHRASES], usize) = &S::SYNTAX.phrase_list();
    // the pair symbols as bytes and length grouped by first byte longest first
    const PAIR_GROUPS: &'static [([u8; 3], u8, TokenType); MAX_PAIRS] = &{
        let (list, n) = S::SYNTAX.pair_list();
        let mut t = [([0u8; 3], 0u8, TokenType::Name); MAX_PAIRS];
        let (mut k, mut b) = (0, 0);
        while b < 128 {
            let mut i = 0;
            while i < n {
                let s = list[i].0;
                if s[0] as usize & 127 == b {
                    let mut bytes = [0u8; 3];
                    let mut j = 0;
                    while j < s.len() { bytes[j] = s[j]; j += 1; }
                    t[k] = (bytes, s.len() as u8, list[i].1);
                    k += 1;
                }
                i += 1;
            }
            b += 1;
        }
        t
    };
    // the second byte every symbol outside the operator table that starts with a byte shares
    const PAIR_NEXT: &'static [u8; 128] = &{
        let (list, n) = S::SYNTAX.pair_list();
        let mut t = [0u8; 128];
        let mut i = 0;
        while i < n {
            let s = list[i].0;
            let b = s[0] as usize & 127;
            // a byte whose symbols differ in their second byte has every one tried
            t[b] = if t[b] == 0 || t[b] == s[1] { s[1] } else { 0xFF };
            i += 1;
        }
        t
    };
    // where each first byte's group begins among the pair groups and its length
    const PAIR_AT: &'static [(u8, u8); 128] = &{
        let list = *Self::PAIR_GROUPS;
        let n = S::SYNTAX.pair_list().1;
        let mut t = [(0u8, 0u8); 128];
        let mut i = 0;
        while i < n {
            let b = list[i].0[0] as usize & 127;
            if t[b].1 == 0 { t[b].0 = i as u8; }
            t[b].1 += 1;
            i += 1;
        }
        t
    };
    const LINE_MARKS: &'static [&'static str] = S::SYNTAX.comments.line;
    const BLOCK_MARKS: Option<(&'static str, &'static str)> = S::SYNTAX.comments.block;
    const COMMENT_START: &'static [bool; 256] = &{
        let mut t = [false; 256];
        let mut i = 0;
        while i < Self::LINE_MARKS.len() { t[Self::LINE_MARKS[i].as_bytes()[0] as usize] = true; i += 1; }
        if let Some((open, _)) = Self::BLOCK_MARKS { t[open.as_bytes()[0] as usize] = true; }
        t
    };
    const INTERP: Option<Interp> = S::SYNTAX.interp;
    const CONVERT: u8 = match Self::INTERP { Some(Interp { convert: Some((b, _)), .. }) => b, _ => 0 };
    // a conversion byte that also spells a symbol has to be told apart from it
    const CONVERT_SHARED: bool = S::SYNTAX.convert_shared();
    const OPEN: &'static [u8] = match Self::INTERP { Some(i) => i.open.as_bytes(), None => b"{" };
    const CLOSE: u8 = match Self::INTERP { Some(i) => i.close, None => b'}' };
    const DOUBLED: bool = matches!(Self::INTERP, Some(Interp { literal: Literal::Doubled, .. }));
    const SHORT: u8 = match Self::INTERP { Some(Interp { short: Some(b), .. }) => b, _ => 0 };
    const SPEC: u8 = match Self::INTERP { Some(Interp { spec: Some(b), .. }) => b, _ => 0 };
    // a field close the `}` path never sees
    const CLOSE_APART: bool = Self::INTERP.is_some() && Self::CLOSE != b'}';
    // bytes that may start a comment pair symbol block brace conversion or field close
    const SPECIAL: &'static [u8; 256] = &{
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < 256 { if Self::COMMENT_START[i] { t[i] |= COMMENT; } i += 1; }
        if Self::BRACES { t[b'{' as usize] |= BRACE; }
        if Self::CONVERT_SHARED { t[Self::CONVERT as usize] |= CONVERT; }
        if Self::CLOSE_APART { t[Self::CLOSE as usize] |= CLOSE; }
        let mut i = 0;
        while i < 128 { if Self::PAIRED[i] { t[i] |= PAIR; } i += 1; }
        t
    };
    const SLOW: bool = {
        let (mut i, mut any) = (0, false);
        while i < 256 { any |= Self::SPECIAL[i] != 0; i += 1; }
        any
    };
    // whether some doubled symbol has no `=` form so `&&=` lexes as `&&` then `=`
    const GAPS: bool = {
        let (t, mut b, mut gaps) = (S::SYNTAX.operators, 0, false);
        while b < 128 {
            gaps |= t[b][2] as u8 != TokenType::Eof as u8 && t[b][3] as u8 == TokenType::Eof as u8;
            b += 1;
        }
        gaps
    };
    // one more than the index of each quote byte among the table's quotes
    const QUOTE: &'static [u8; 256] = &{
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < S::SYNTAX.strings.quotes.len() { t[S::SYNTAX.strings.quotes[i].byte as usize] = i as u8 + 1; i += 1; }
        t
    };
    const FORMAT_QUOTE: &'static [bool; 256] = &match Self::INTERP { Some(i) => flags(i.quotes), None => [false; 256] };
    const HASHES: bool = S::SYNTAX.strings.hashes;
    // whether no quote or every quote or only some take the long form of three
    const TRIPLES: u8 = {
        let (q, mut i, mut some, mut all) = (S::SYNTAX.strings.quotes, 0, false, true);
        while i < q.len() { some |= q[i].triple; all &= q[i].triple; i += 1; }
        if all { 2 } else if some { 1 } else { 0 }
    };
    // whether some single quote literal of the table may hold line ends
    const MULTILINE: bool = {
        let (q, mut i, mut any) = (S::SYNTAX.strings.quotes, 0, false);
        while i < q.len() { any |= q[i].multiline; i += 1; }
        any
    };
    // one more than the bits of each one letter prefix in either case or zero
    const PREFIX1: &'static [u8; 256] = &{
        let (p, any) = (S::SYNTAX.strings.prefixes, S::SYNTAX.strings.any_case);
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < p.len() {
            let b = p[i].0.as_bytes();
            if b.len() == 1 {
                t[b[0] as usize] = p[i].1 + 1;
                if any { t[b[0].to_ascii_uppercase() as usize] = p[i].1 + 1; }
            }
            i += 1;
        }
        t
    };
    const REFUSED: &'static [Option<&'static str>; 128] = &{
        let r = S::SYNTAX.strings.escapes.refused;
        let mut t = [None; 128];
        let mut i = 0;
        while i < r.len() { t[r[i].0 as usize & 127] = Some(r[i].1); i += 1; }
        t
    };
    const REFUSES: bool = !S::SYNTAX.strings.escapes.refused.is_empty();
    const STRICT: bool = matches!(S::SYNTAX.strings.escapes.unknown, Unknown::Refuse);
    const BRACED: bool = matches!(S::SYNTAX.strings.escapes.unicode, Unicode::Braced);
    // the escapes a strict table knows besides its quotes and the backslash
    const KNOWN: &'static [bool; 256] = &{
        let e = S::SYNTAX.strings.escapes;
        let mut t = [false; 256];
        let mut i = 0;
        while i < e.simple.len() { t[e.simple[i].0 as usize] = true; i += 1; }
        if e.hex { t[b'x' as usize] = true; }
        if !matches!(e.unicode, Unicode::None) { t[b'u' as usize] = true; }
        if matches!(e.unicode, Unicode::Fixed) { t[b'U' as usize] = true; }
        if e.octal { let mut d = b'0'; while d <= b'7' { t[d as usize] = true; d += 1; } }
        if e.line_join { t[b'\n' as usize] = true; t[b'\r' as usize] = true; }
        t[b'\\' as usize] = true;
        let mut q = 0;
        while q < S::SYNTAX.strings.quotes.len() { t[S::SYNTAX.strings.quotes[q].byte as usize] = true; q += 1; }
        t
    };
    // the radix each letter after a leading zero names in either case
    const RADIX: &'static [u8; 256] = &{
        let r = S::SYNTAX.numbers.radixes;
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < r.len() {
            t[r[i].0.to_ascii_lowercase() as usize] = r[i].1 as u8;
            t[r[i].0.to_ascii_uppercase() as usize] = r[i].1 as u8;
            i += 1;
        }
        t
    };
    const SEP: u8 = match S::SYNTAX.numbers.separator { Some(b) => b, None => 0 };
    const HAS_SEP: bool = S::SYNTAX.numbers.separator.is_some();
    const LEADING_DOT: bool = S::SYNTAX.numbers.leading_dot;
    const TRAILING_DOT: bool = S::SYNTAX.numbers.trailing_dot;
    const EXPONENT: bool = S::SYNTAX.numbers.exponent;
    const SUFFIX: Suffix = S::SYNTAX.numbers.suffix;
    const JOIN: u8 = match S::SYNTAX.continuation.join { Some(b) => b, None => 0 };
    const HAS_JOIN: bool = S::SYNTAX.continuation.join.is_some();
    const TRAIL_OP: bool = S::SYNTAX.continuation.operator;
    // the tokens with a binding power on their left
    const BINARY: &'static [bool; 128] = &{
        let p = S::SYNTAX.precedence;
        let mut t = [false; 128];
        let mut i = 0;
        while i < p.len() { if p[i].1 > 0 { t[p[i].0 as usize & 127] = true; } i += 1; }
        t
    };
    // the messages the table words with its own spellings at compile time
    const NAME: &'static [u8] = match Self::INTERP { Some(i) => i.name.as_bytes(), None => b"interpolated string" };
    const UNTERMINATED_TEXT: &'static ([u8; 96], usize) = &join(&[b"unterminated ", Self::NAME, b" literal"]);
    const UNTERMINATED: &'static str = joined(Self::UNTERMINATED_TEXT);
    const SINGLE_CLOSE_TEXT: &'static ([u8; 96], usize) = &join(&[Self::NAME, b": single '", &[Self::CLOSE], b"' is not allowed"]);
    const SINGLE_CLOSE: &'static str = joined(Self::SINGLE_CLOSE_TEXT);
    const TOO_DEEP_TEXT: &'static ([u8; 96], usize) = &join(&[Self::NAME, b" nesting depth exceeds maximum (200)"]);
    const TOO_DEEP: &'static str = joined(Self::TOO_DEEP_TEXT);
    const MISPLACED_TEXT: &'static ([u8; 64], usize) = &join(&[b"invalid '", &[Self::SEP], b"' in numeric literal"]);
    const MISPLACED: &'static str = joined(Self::MISPLACED_TEXT);
    const DOUBLED_SEP_TEXT: &'static ([u8; 64], usize) = &join(&[b"consecutive '", &[Self::SEP], b"' in numeric literal"]);
    const DOUBLED_SEP: &'static str = joined(Self::DOUBLED_SEP_TEXT);
    pub(super) const SOFT_HEADS: bool = S::SYNTAX.is_soft(TokenType::Match) || S::SYNTAX.is_soft(TokenType::Case);
    pub(super) const SOFT_TYPE: bool = S::SYNTAX.is_soft(TokenType::Type);
    pub(super) const SOFT_FN: bool = S::SYNTAX.is_soft(TokenType::Fn);

    pub fn new(src: &'a [u8]) -> Self {
        let () = Self::CHECKED;
        Self {
            src, pos: 0,
            pending: Vec::new(),
            indent_stack: Vec::new(),
            nesting: 0, line: 0,
            fields: Vec::new(),
            errors: Vec::new(),
            braces: 1,
            spilled: Vec::new(),
            words: Words { header: None, clauses: Vec::new(), conditions: 0, fresh: true, depth: 0 },
            sig: (0, None),
            spelling: PhantomData,
        }
    }

    // a `{` outside brackets opens a block after a header operand or a block word
    fn opens_block(&self, tokens: &[Token], lead: &mut Lead, after: usize) -> bool {
        // the tokens before the brace back to front without comments or blank lines
        let mut kept = tokens.iter().rev().map(|t| t.kind).filter(|&k| !matches!(k, TokenType::Comment | TokenType::LineBreak));
        let Some(prev) = kept.next() else { return false };
        let mut head = || kept.next().is_none_or(ends_stmt);
        match prev {
            TokenType::Name | TokenType::Int | TokenType::Float | TokenType::String | TokenType::Bytes | TokenType::FormatEnd
            | TokenType::True | TokenType::False | TokenType::Nil | TokenType::Ellipsis | TokenType::Underscore
            | TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose | TokenType::Try | TokenType::Finally | TokenType::Catch
            | TokenType::Arm => true,
            // a statement `else` takes a block and a conditional `else` an operand
            TokenType::Else => head(),
            // a soft word leading a statement keeps its keyword and anywhere else names a value
            TokenType::Match | TokenType::Case | TokenType::Type | TokenType::Fn => !head(),
            // a header ending in a comma opens its block where the brace ends the line
            TokenType::Comma => self.line_ends(after) && opens_body(lead.of(tokens)),
            _ => false,
        }
    }

    // whether only blanks and a line comment follow `p` on its line
    fn line_ends(&self, mut p: usize) -> bool {
        while matches!(self.src.get(p), Some(b' ' | b'\t')) { p += 1; }
        matches!(self.src.get(p), None | Some(b'\n' | b'\r')) || self.line_comment_at(p)
    }

    // whether a comment running to the line end starts at `p`
    #[inline(always)]
    fn line_comment_at(&self, p: usize) -> bool {
        let mut i = 0;
        while i < Self::LINE_MARKS.len() {
            let m = Self::LINE_MARKS[i].as_bytes();
            if self.src.get(p) == Some(&m[0]) && (m.len() == 1 || self.src[p..].starts_with(m)) { return true; }
            i += 1;
        }
        false
    }

    // whether the line from `p` holds nothing but comments
    fn blank_from(&self, mut p: usize) -> bool {
        loop {
            if matches!(self.src.get(p), Some(b'\n' | b'\r')) || self.line_comment_at(p) { return true; }
            let Some((open, close)) = Self::BLOCK_MARKS else { return false };
            if !self.src[p.min(self.src.len())..].starts_with(open.as_bytes()) { return false; }
            // a block comment counts as blank only when it closes on its own line
            let Some(end) = self.block_end(p, open.as_bytes(), close.as_bytes()) else { return false };
            if self.src[p..end].contains(&b'\n') { return false; }
            p = end;
            while matches!(self.src.get(p), Some(b' ' | b'\t')) { p += 1; }
        }
    }

    // where the block comment opening at `p` ends past its close if it closes
    fn block_end(&self, p: usize, open: &[u8], close: &[u8]) -> Option<usize> {
        let (mut q, mut depth) = (p + open.len(), 1usize);
        while q < self.src.len() {
            if self.src[q..].starts_with(close) {
                q += close.len();
                depth -= 1;
                if depth == 0 { return Some(q); }
            } else if S::SYNTAX.comments.nested && self.src[q..].starts_with(open) {
                q += open.len();
                depth += 1;
            } else {
                q += 1;
            }
        }
        None
    }

    #[inline(always)]
    fn line_comment(&mut self, start: usize, line: usize) -> Tok {
        let (src, mut p) = (self.src, self.pos);
        while p < src.len() && src[p] != b'\n' { p += 1; }
        self.pos = p;
        (TokenType::Comment, line, start, p)
    }

    // the block comment opening at `start` queued as one token and whether one opens there
    #[inline(never)]
    fn block_comment(&mut self, start: usize, line: usize) -> bool {
        let Some((open, close)) = Self::BLOCK_MARKS else { return false };
        if !self.src[start..].starts_with(open.as_bytes()) { return false; }
        match self.block_end(start, open.as_bytes(), close.as_bytes()) {
            Some(end) => {
                self.line += self.src[start..end].iter().filter(|&&b| b == b'\n').count();
                self.pos = end;
            }
            None => {
                self.report(start, start + open.len(), "unterminated comment");
                self.pos = self.src.len();
            }
        }
        self.pending.push((TokenType::Comment, line, start, self.pos));
        true
    }

    // a `{` of a braces table as a block opener or a display bracket
    #[inline(always)]
    fn open_brace(&mut self, tokens: &[Token], lead: &mut Lead, start: usize) -> TokenType {
        self.pos = start + 1;
        let block = self.fields.is_empty() && self.nesting == 0 && self.opens_block(tokens, lead, self.pos);
        self.push_brace(block);
        if !block {
            self.nesting += 1;
            return TokenType::BraceOpen;
        }
        if self.depth() > MAX_INDENT_DEPTH {
            if self.depth() == MAX_INDENT_DEPTH + 1 { self.report(start, self.pos, "block nesting depth exceeds maximum (100)"); }
            return TokenType::Eof;
        }
        TokenType::BlockOpen
    }

    // a word of brace bits spills once its leading one reaches the top bit
    fn push_brace(&mut self, block: bool) {
        if self.braces >> (usize::BITS - 1) != 0 {
            self.spilled.push(self.braces);
            self.braces = 1;
        }
        self.braces = self.braces << 1 | block as usize;
    }

    // the newest brace closes and an emptied word takes back the last full one
    fn pop_brace(&mut self) {
        if self.braces > 1 { self.braces >>= 1; }
        if self.braces == 1 && let Some(word) = self.spilled.pop() { self.braces = word; }
    }

    // how many braces are open
    fn depth(&self) -> usize {
        self.spilled.len() * (usize::BITS as usize - 1) + (usize::BITS - 1 - self.braces.leading_zeros()) as usize
    }

    // the token and end of a phrase like `else if` that `word` begins
    fn phrase(&self, word: &[u8]) -> Option<(TokenType, usize)> {
        let (list, n) = Self::PHRASE_LIST;
        for p in &list[..*n] {
            if p.first != word { continue; }
            let mut q = self.pos;
            while matches!(self.src.get(q), Some(b' ' | b'\t')) { q += 1; }
            let end = q + p.rest.len();
            if self.src[q..].starts_with(p.rest) && self.src.get(end).is_none_or(|&b| BYTE_CLASS[b as usize] & ID_CONT == 0) {
                return Some((p.kind, end));
            }
        }
        None
    }

    // the symbol at the cursor where a pair symbol wins over the operators it starts
    #[inline(always)]
    fn long_symbol(&self, b: u8) -> (TokenType, usize) {
        let next = self.at(1);
        let lead = Self::PAIR_NEXT[b as usize & 127];
        if (lead == 0xFF || next == Some(lead)) && let Some(p) = self.pair(b, next) { return p; }
        self.symbol(b, next)
    }

    // the longest symbol at the cursor that the operator table has no column for
    #[inline(always)]
    fn pair(&self, b: u8, next: Option<u8>) -> Option<(TokenType, usize)> {
        let (from, n) = Self::PAIR_AT[b as usize & 127];
        for &(s, len, kind) in &Self::PAIR_GROUPS[from as usize..(from + n) as usize] {
            if next == Some(s[1]) && (len == 2 || self.at(2) == Some(s[2])) { return Some((kind, len as usize)); }
        }
        None
    }

    // whether the byte at `at` converts a field as in `f"{x!r}"` and spells no symbol
    fn converts(&self, at: usize) -> bool {
        // only a field of the innermost literal converts so every other byte stops here
        if !self.fields.last().is_some_and(|f| f.nesting == self.nesting) { return false; }
        let before = self.src[..at].iter().rev().find(|&&c| c != b' ');
        let mut end = at + 1;
        while self.src.get(end).is_some_and(|&c| BYTE_CLASS[c as usize] & ID_CONT != 0) { end += 1; }
        let last = Self::OPEN[Self::OPEN.len() - 1];
        before != Some(&last) && end > at + 1 && self.src.get(end).is_some_and(|&c| c == Self::CLOSE || (Self::SPEC != 0 && c == Self::SPEC))
    }

    // the operator the bytes at the cursor spell as its token and length
    #[inline(always)]
    fn symbol(&self, b: u8, next: Option<u8>) -> (TokenType, usize) {
        let [alone, with_eq, doubled, doubled_eq] = S::SYNTAX.operators[b as usize & 127];
        if next == Some(b) && doubled != TokenType::Eof {
            if self.at(2) == Some(b'=') && (!Self::GAPS || doubled_eq != TokenType::Eof) { (doubled_eq, 3) } else { (doubled, 2) }
        } else if next == Some(b'=') && with_eq != TokenType::Eof {
            (with_eq, 2)
        } else {
            (alone, 1)
        }
    }

    #[inline(never)]
    #[cold]
    fn report(&mut self, start: usize, end: usize, msg: &'static str) {
        self.errors.push(LexError { start, end, msg });
    }

    /* the inner scan loops test bytes through `BYTE_CLASS` loads instead of branches */
    #[inline]
    fn scan_while(&mut self, pred: impl Fn(u8) -> bool) {
        while self.pos < self.src.len() && pred(self.src[self.pos]) {
            self.pos += 1;
        }
    }

    pub fn skip_whitespace(&mut self) {
        loop {
            self.scan_while(|b| BYTE_CLASS[b as usize] & SPACE != 0);
            // the join byte before a line end continues the line and both are consumed
            if Self::HAS_JOIN && self.pos + 1 < self.src.len() && self.src[self.pos] == Self::JOIN
                && matches!(self.src[self.pos + 1], b'\n' | b'\r') {
                self.pos += 1;
                if self.src[self.pos] == b'\r' { self.pos += 1; }
                if self.pos < self.src.len() && self.src[self.pos] == b'\n' { self.pos += 1; }
                self.line += 1;
            } else { break; }
        }
    }
    fn scan_id_rest(&mut self) {
        self.scan_while(|b| BYTE_CLASS[b as usize] & ID_CONT != 0);
    }
    fn scan_digits(&mut self) {
        self.scan_while(|b| BYTE_CLASS[b as usize] & DIGIT != 0 || (Self::HAS_SEP && b == Self::SEP));
    }

    /* separators sit only between digits and only `allow_empty` accepts an empty body */
    fn check_digits(&mut self, start: usize, end: usize, allow_empty: bool) {
        if start == end {
            if !allow_empty {
                self.report(start, end, "missing digits in numeric literal");
            }
            return;
        }
        if !Self::HAS_SEP { return; }
        let s = &self.src[start..end];
        if s[0] == Self::SEP || s[s.len() - 1] == Self::SEP {
            self.report(start, end, Self::MISPLACED);
        } else if s.windows(2).any(|w| w[0] == Self::SEP && w[1] == Self::SEP) {
            self.report(start, end, Self::DOUBLED_SEP);
        }
    }
    // one separator may follow the radix prefix as in `0x_FF`
    fn check_prefixed(&mut self, b: usize) {
        let skip = usize::from(Self::HAS_SEP && self.src.get(b) == Some(&Self::SEP) && b + 1 < self.pos);
        self.check_digits(b + skip, self.pos, false);
    }

    // the digits of `base` with any separators between them
    fn scan_radix_digits(&mut self, base: u8) {
        match base {
            16 => self.scan_while(|b| b.is_ascii_hexdigit() || (Self::HAS_SEP && b == Self::SEP)),
            8 => self.scan_while(|b| matches!(b, b'0'..=b'7') || (Self::HAS_SEP && b == Self::SEP)),
            2 => self.scan_while(|b| matches!(b, b'0' | b'1') || (Self::HAS_SEP && b == Self::SEP)),
            _ => self.scan_while(|b| (b as char).is_digit(base as u32) || (Self::HAS_SEP && b == Self::SEP)),
        }
    }

    #[inline(always)]
    fn at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    // whether an exponent follows where the table reads one and the scan past it
    #[inline(always)]
    fn scan_exponent(&mut self) -> bool {
        if !Self::EXPONENT || !(self.pos < self.src.len() && matches!(self.src[self.pos], b'e' | b'E')) { return false; }
        self.exponent();
        true
    }

    #[inline(never)]
    fn exponent(&mut self) {
        self.pos += 1;
        if self.pos < self.src.len() && matches!(self.src[self.pos], b'+' | b'-') {
            self.pos += 1;
        }
        let body_start = self.pos;
        self.scan_digits();
        // format specs reuse `e` as in `.2e` so the float parser reports an empty exponent
        self.check_digits(body_start, self.pos, true);
    }

    // a number and where its token ends before any suffix the table drops
    fn scan_number(&mut self, start: usize) -> (TokenType, usize) {
        if self.src[start] == b'0' && self.pos < self.src.len() {
            let base = Self::RADIX[self.src[self.pos] as usize];
            if base != 0 {
                self.pos += 1;
                self.scan_radix_digits(base);
                self.check_prefixed(start + 2);
                return (TokenType::Int, self.suffix());
            }
        }
        self.scan_digits();
        self.check_digits(start, self.pos, false);
        let mut is_float = false;
        if self.pos < self.src.len() && self.src[self.pos] == b'.' && self.at(1) != Some(b'.')
            && (Self::TRAILING_DOT || self.at(1).is_some_and(|c| BYTE_CLASS[c as usize] & DIGIT != 0)) {
            is_float = true;
            self.pos += 1;
            let frac_start = self.pos;
            self.scan_digits();
            // a trailing dot as in `5.` is valid so only a non-empty fraction is checked
            self.check_digits(frac_start, self.pos, true);
        }
        is_float |= self.scan_exponent();
        let end = self.suffix();
        (if is_float { TokenType::Float } else { TokenType::Int }, end)
    }

    fn scan_dot_number(&mut self) -> usize {
        let body_start = self.pos;
        self.scan_digits();
        self.check_digits(body_start, self.pos, false);
        self.scan_exponent();
        self.suffix()
    }

    // letters right after a number as the table reads them and where the number ends
    #[inline(always)]
    fn suffix(&mut self) -> usize {
        let end = self.pos;
        if matches!(Self::SUFFIX, Suffix::Apart) || !self.src.get(end).is_some_and(|&b| BYTE_CLASS[b as usize] & ID_CONT != 0) { return end; }
        self.suffixed(end)
    }

    // the letters glued to a number that ends at `end`
    #[inline(never)]
    #[cold]
    fn suffixed(&mut self, end: usize) -> usize {
        self.scan_id_rest();
        if matches!(Self::SUFFIX, Suffix::Refused) { self.report(end, self.pos, "invalid suffix on numeric literal"); }
        end
    }

    // whether `b` is a quote of the table as one compare per quote
    #[inline(always)]
    fn is_quote(b: u8) -> bool {
        let quotes = S::SYNTAX.strings.quotes;
        let mut i = 0;
        while i < quotes.len() {
            if quotes[i].byte == b { return true; }
            i += 1;
        }
        false
    }

    // the quote of the table that `b` is
    #[inline(always)]
    fn quote_of(b: u8) -> Quote { S::SYNTAX.strings.quotes[Self::QUOTE[b as usize] as usize - 1] }

    // the RAW BYTES and FORMAT bits prefix `s` sets or None for any other name
    #[inline(always)]
    fn prefix(s: &[u8]) -> Option<u8> {
        if s.len() == 1 { return Self::PREFIX1[s[0] as usize].checked_sub(1); }
        let strings = S::SYNTAX.strings;
        strings.prefixes.iter().find(|(p, _)| {
            p.len() == s.len() && p.bytes().zip(s).all(|(a, &b)| a == b || (strings.any_case && a == b.to_ascii_lowercase()))
        }).map(|&(_, f)| f)
    }

    // scans the literal a prefix opens and gives its kind unless hashes lack a quote
    #[inline(always)]
    fn prefixed(&mut self, start: usize, flags: u8) -> Option<TokenType> {
        let at = self.pos;
        if Self::HASHES && self.src[at] == b'#' {
            let hashes = self.src[at..].iter().take_while(|&&b| b == b'#').count();
            let q = at + hashes;
            if flags & RAW == 0 || self.src.get(q).is_none_or(|&b| Self::QUOTE[b as usize] == 0) { return None; }
            self.scan_hashed(self.src[q], q, hashes);
        } else if flags & FORMAT != 0 {
            // the parts of an interpolated literal wait on the pending stack
            self.start_format(start, at, flags & RAW == 0);
            return Some(TokenType::FormatStart);
        } else {
            self.pos = at + 1;
            self.scan_string(self.src[at], at, flags & (RAW | BYTES) == 0);
        }
        Some(if flags & BYTES != 0 { TokenType::Bytes } else { TokenType::String })
    }

    // a raw literal closed by its quote and as many hashes as opened it
    fn scan_hashed(&mut self, quote: u8, q: usize, hashes: usize) {
        let mut p = q + 1;
        while p < self.src.len() {
            if self.src[p] == quote && self.src.len() - p > hashes && self.src[p + 1..p + 1 + hashes].iter().all(|&b| b == b'#') {
                self.pos = p + 1 + hashes;
                return;
            }
            if self.src[p] == b'\n' { self.line += 1; }
            p += 1;
        }
        self.report(q, q + 1, "unterminated string literal");
        self.pos = p;
    }

    /* `decodes` is false for a raw or bytes literal where refused escapes stay plain text */
    fn scan_string(&mut self, quote: u8, start: usize, decodes: bool) {
        let triple = match Self::TRIPLES { 0 => false, 2 => true, _ => Self::quote_of(quote).triple };
        if triple && self.at(0) == Some(quote) && self.at(1) == Some(quote) {
            self.pos += 2;
            self.scan_triple_string(quote, start, decodes);
        } else {
            self.scan_single_string(quote, start, decodes, Self::MULTILINE && Self::quote_of(quote).multiline);
        }
    }

    // reports an escape at `p` the table refuses
    #[inline]
    fn check_escape(&mut self, p: usize, decodes: bool) {
        if !decodes || !(Self::REFUSES || Self::STRICT) { return; }
        let Some(&e) = self.src.get(p + 1) else { return };
        if Self::REFUSES && e < 128 && let Some(msg) = Self::REFUSED[e as usize] {
            let end = p + 2 + usize::from(self.src.get(p + 2) == Some(&b'{'));
            self.report(p, end, msg);
        } else if Self::STRICT && !(Self::KNOWN[e as usize] && self.digits_hold(p)) {
            // the span covers the whole character after the backslash
            self.report(p, p + 1 + utf8_char_len(e), "invalid escape in string literal");
        }
    }

    /* whether a code point escape at `p` holds exactly the hex digits its table wants */
    fn digits_hold(&self, p: usize) -> bool {
        let hex = |n: usize| self.src.get(p + 2..p + 2 + n).is_some_and(|d| d.iter().all(u8::is_ascii_hexdigit));
        let e = &S::SYNTAX.strings.escapes;
        match self.src[p + 1] {
            b'x' if e.hex => hex(2),
            b'u' if matches!(e.unicode, Unicode::Fixed) => hex(4),
            b'U' if matches!(e.unicode, Unicode::Fixed) => hex(8),
            b'u' if matches!(e.unicode, Unicode::Braced) => braced_len(&self.src[p + 2..]).is_some(),
            _ => true,
        }
    }

    fn scan_single_string(&mut self, quote: u8, start: usize, decodes: bool, multiline: bool) {
        if Self::MULTILINE && multiline { return self.scan_lines_string(quote, start, decodes); }
        let (src, mut p) = (self.src, self.pos);
        while p < src.len() {
            let b = src[p];
            if b == quote { self.pos = p + 1; return; }
            if b == b'\n' {
                // the error anchors at the opener so the caret lands on the quote
                self.pos = p;
                return self.report(start, start + 1, "unterminated string literal");
            }
            if b == b'\\' && p + 1 < src.len() {
                self.check_escape(p, decodes);
                // an escaped line end is a line of the source like any other
                if src[p + 1] == b'\n' { self.line += 1; }
                p += 2;
                continue;
            }
            p += 1;
        }
        self.pos = p;
        self.report(start, start + 1, "unterminated string literal");
    }

    // a literal of one quote that may hold line ends
    fn scan_lines_string(&mut self, quote: u8, start: usize, decodes: bool) {
        let (src, mut p) = (self.src, self.pos);
        while p < src.len() {
            let b = src[p];
            if b == quote { self.pos = p + 1; return; }
            if b == b'\\' && p + 1 < src.len() { self.check_escape(p, decodes); p += 1; }
            if src[p] == b'\n' { self.line += 1; }
            p += 1;
        }
        self.pos = p;
        self.report(start, start + 1, "unterminated string literal");
    }

    fn scan_triple_string(&mut self, quote: u8, start: usize, decodes: bool) {
        let (src, mut p) = (self.src, self.pos);
        while p < src.len() {
            let b = src[p];
            if b == quote && src.get(p + 1) == Some(&quote) && src.get(p + 2) == Some(&quote) {
                self.pos = p + 3; return;
            }
            if b == b'\\' && p + 1 < src.len() { self.check_escape(p, decodes); p += 1; }
            if src[p] == b'\n' { self.line += 1; }
            p += 1;
        }
        self.pos = p;
        self.report(start, start + 3, "unterminated triple-quoted string literal");
    }

    // whether the field opener of the table starts at `p`
    #[inline(always)]
    fn opens_field(&self, p: usize) -> bool {
        self.src[p] == Self::OPEN[0] && (Self::OPEN.len() == 1 || self.src.get(p + 1) == Some(&Self::OPEN[1]))
    }

    // whether the quote of a literal closes it at `p`
    #[inline(always)]
    fn closes_at(&self, p: usize, quote: u8, triple: bool) -> bool {
        if triple { p + 2 < self.src.len() && self.src[p] == quote && self.src[p + 1] == quote && self.src[p + 2] == quote } else { self.src[p] == quote }
    }

    // opens the interpolated literal whose quote sits at `at` and queues its first parts
    #[inline(never)]
    fn start_format(&mut self, start: usize, at: usize, decodes: bool) {
        let quote = self.src[at];
        let triple = Self::quote_of(quote).triple && self.src.get(at + 1) == Some(&quote) && self.src.get(at + 2) == Some(&quote);
        self.pos = at + if triple { 3 } else { 1 };
        let body = self.pos;
        self.format_body(quote, triple, decodes, body);
        // the opener takes the line its first text ended on and pops first
        self.pending.push((TokenType::FormatStart, self.line, start, body));
    }

    // queues literal text up to a field or the end so parts pop in order
    fn format_body(&mut self, quote: u8, triple: bool, decodes: bool, body_start: usize) {
        let ql: usize = if triple { 3 } else { 1 };
        let multiline = triple || Self::quote_of(quote).multiline;
        let (mut pos, mut text) = (self.pos, body_start);
        // the short fields met so far in order which queue after what ends the scan
        let mut shorts: Vec<(TokenType, usize, usize)> = Vec::new();
        while pos < self.src.len() {
            if self.closes_at(pos, quote, triple) {
                self.pending.push((TokenType::FormatEnd, self.line, pos, pos + ql));
                if pos > text { self.pending.push((TokenType::FormatText, self.line, text, pos)); }
                self.pos = pos + ql;
                return self.queue_shorts(shorts);
            }
            let c = self.src[pos];
            if self.opens_field(pos) {
                // a doubled opener is plain text
                if Self::DOUBLED && self.src.get(pos + 1) == Some(&c) { pos += 2; continue; }
                if self.fields.len() >= MAX_FIELD_DEPTH {
                    self.report(pos, pos + Self::OPEN.len(), Self::TOO_DEEP);
                    self.pos = pos + Self::OPEN.len();
                    return self.queue_shorts(shorts);
                }
                self.pending.push((TokenType::BraceOpen, self.line, pos, pos + Self::OPEN.len()));
                if pos > text { self.pending.push((TokenType::FormatText, self.line, text, pos)); }
                self.fields.push(Field { quote, triple, decodes, nesting: self.nesting, in_spec: false });
                self.pos = pos + Self::OPEN.len();
                return self.queue_shorts(shorts);
            }
            match c {
                b'\\' => pos = self.format_escape(pos, quote, decodes),
                b'\n' if !multiline => break,
                b'\n' => { self.line += 1; pos += 1; }
                _ if c == Self::CLOSE && Self::DOUBLED => {
                    if self.src.get(pos + 1) == Some(&c) { pos += 2; } else {
                        self.report(pos, pos + 1, Self::SINGLE_CLOSE);
                        pos += 1;
                    }
                }
                _ if Self::SHORT != 0 && c == Self::SHORT && self.src.get(pos + 1).is_some_and(|&b| BYTE_CLASS[b as usize] & ID_START != 0) => {
                    let mut end = pos + 1 + utf8_char_len(self.src[pos + 1]);
                    while self.src.get(end).is_some_and(|&b| BYTE_CLASS[b as usize] & ID_CONT != 0) { end += 1; }
                    if pos > text { shorts.push((TokenType::FormatText, text, pos)); }
                    shorts.extend([(TokenType::BraceOpen, pos, pos + 1), (TokenType::Name, pos + 1, end), (TokenType::BraceClose, end, end)]);
                    (self.pos, pos, text) = (end, end, end);
                }
                _ => pos += 1,
            }
        }
        // an end token where the literal stops keeps the structure balanced for the parser
        self.report(self.pos, pos, Self::UNTERMINATED);
        self.pending.push((TokenType::FormatEnd, self.line, pos, pos));
        if pos > text { self.pending.push((TokenType::FormatText, self.line, text, pos)); }
        self.pos = pos;
        self.queue_shorts(shorts);
    }

    // the short fields above what ended the scan so they pop first in source order
    #[inline(always)]
    fn queue_shorts(&mut self, shorts: Vec<(TokenType, usize, usize)>) {
        if shorts.is_empty() { return; }
        let line = self.line;
        self.pending.extend(shorts.into_iter().rev().map(|(kind, start, end)| (kind, line, start, end)));
    }

    // where the scan of literal text goes on past the backslash at `pos`
    fn format_escape(&mut self, pos: usize, quote: u8, decodes: bool) -> usize {
        let e = self.src.get(pos + 1).copied();
        let braced = self.src.get(pos + 2) == Some(&b'{');
        let refused = if decodes && Self::REFUSES && let Some(e) = e && e < 128 { Self::REFUSED[e as usize] } else { None };
        if let Some(msg) = refused { self.report(pos, pos + 2 + usize::from(braced), msg); }
        // the braces of such an escape stay text instead of opening a field
        if (refused.is_some() || (decodes && Self::BRACED && e == Some(b'u'))) && braced {
            if refused.is_none() && Self::STRICT && braced_len(&self.src[pos + 2..]).is_none() { self.report(pos, pos + 2, "invalid escape in string literal"); }
            // the braces end at the literal's quote or line end so its close still counts
            let stop = self.src[pos..].iter().position(|&c| c == b'}' || c == quote || c == b'\n');
            return stop.map_or(self.src.len(), |n| pos + n + usize::from(self.src[pos + n] == b'}'));
        }
        if refused.is_some() { return pos + 2; }
        // a raw backslash shields only a quote or backslash so a following opener still counts
        if !decodes && !matches!(e, Some(c) if c == quote || c == b'\\') { return pos + 1; }
        if decodes && Self::STRICT && let Some(e) = e && !(Self::KNOWN[e as usize] && self.digits_hold(pos)) && e != Self::OPEN[0] && e != Self::CLOSE {
            self.report(pos, pos + 1 + utf8_char_len(e), "invalid escape in string literal");
        }
        if e == Some(b'\n') { self.line += 1; }
        (pos + 2).min(self.src.len())
    }

    // the spec text of the top field up to a nested field or its close
    fn spec_text(&mut self) {
        let Some(&f) = self.fields.last() else { return };
        let multiline = f.triple || Self::quote_of(f.quote).multiline;
        let (from, mut p) = (self.pos, self.pos);
        while p < self.src.len() {
            let c = self.src[p];
            if self.closes_at(p, f.quote, f.triple) || (c == b'\n' && !multiline) { break; }
            if self.opens_field(p) {
                if self.fields.len() >= MAX_FIELD_DEPTH {
                    if p > from { self.pending.push((TokenType::FormatText, self.line, from, p)); }
                    self.report(p, p + Self::OPEN.len(), Self::TOO_DEEP);
                    self.pos = p + Self::OPEN.len();
                    return;
                }
                self.pending.push((TokenType::BraceOpen, self.line, p, p + Self::OPEN.len()));
                if p > from { self.pending.push((TokenType::FormatText, self.line, from, p)); }
                self.fields.push(Field { in_spec: true, nesting: self.nesting, ..f });
                self.pos = p + Self::OPEN.len();
                return;
            }
            if c == Self::CLOSE {
                // the close and what follows it queue first so the spec text pops first
                self.close_on(p);
                if p > from { self.pending.push((TokenType::FormatText, self.line, from, p)); }
                return;
            }
            if c == b'\n' { self.line += 1; }
            p += 1;
        }
        if p > from { self.pending.push((TokenType::FormatText, self.line, from, p)); }
        self.pos = p;
    }

    // closes the top field at `start` and queues what follows in its literal or spec
    fn close_on(&mut self, start: usize) {
        let Some(f) = self.fields.pop() else { return };
        self.pos = start + 1;
        if f.in_spec { self.spec_text(); } else { self.format_body(f.quote, f.triple, f.decodes, start + 1); }
        // the close takes the line the scan after it ended on and pops first
        self.pending.push((TokenType::BraceClose, self.line, start, start + 1));
    }

    // queues the tokens of a field close at `start` in the order they pop
    #[inline(never)]
    fn close_field(&mut self, start: usize) { self.close_on(start); }

    // queues the spec marker at `start` atop a field and the raw spec after it
    #[inline(never)]
    fn open_spec(&mut self, start: usize, line: usize) {
        self.pos = start + 1;
        self.spec_text();
        self.pending.push((TokenType::Colon, line, start, start + 1));
    }

    // the last kind no break or comment holds reading only the tokens added since
    fn last_sig(&mut self, tokens: &[Token]) -> Option<TokenType> {
        let (seen, last) = &mut self.sig;
        for t in &tokens[(*seen).min(tokens.len())..] {
            if !matches!(t.kind, TokenType::Comment | TokenType::LineBreak) { *last = Some(t.kind); }
        }
        *seen = tokens.len();
        *last
    }

    // the token a line end gives with any indents or dedents after it queued behind
    #[inline(always)]
    fn handle_newline(&mut self, start: usize, tokens: &[Token]) -> Tok {
        let current_line = self.line;
        self.line += 1;
        let at = self.pos;

        if self.nesting > 0 || !Self::LINES { return (TokenType::LineBreak, current_line, start, at); }

        // a line ending in a binary operator goes on in a table that wants it
        if Self::TRAIL_OP && self.last_sig(tokens).is_some_and(|k| Self::BINARY[k as usize & 127]) {
            return (TokenType::LineBreak, current_line, start, at);
        }

        // with delimited blocks no indentation counts and a blank next line ends no statement
        if !Self::INDENT {
            let mut p = at;
            while matches!(self.src.get(p), Some(b' ' | b'\t')) { p += 1; }
            let blank = self.blank_from(p);
            return (if blank { TokenType::LineBreak } else { TokenType::Newline }, current_line, start, at);
        }

        let mut p = at;
        // spaces are the common indentation so only a tab takes the careful count
        while self.src.get(p) == Some(&b' ') { p += 1; }
        if self.src.get(p) == Some(&b'\t') { return self.tabbed(start, current_line, p); }

        // the indentation is read once here so the next token starts past it
        self.pos = p;
        if self.blank_from(p) { return (TokenType::LineBreak, current_line, start, at); }
        let level = p - at;
        let current = *self.indent_stack.last().unwrap_or(&0);
        if level != current { self.indent_to(level, current, at, p); }
        (TokenType::Newline, current_line, start, at)
    }

    // an indentation holding a tab counted as blanks or refused when the two mix
    #[cold]
    #[inline(never)]
    fn tabbed(&mut self, start: usize, current_line: usize, from: usize) -> Tok {
        let at = self.pos;
        let mut p = from;
        let (mut has_space, mut has_tab) = (from > at, false);
        while let Some(&c) = self.src.get(p) && (c == b' ' || c == b'\t') {
            has_space |= c == b' ';
            has_tab |= c == b'\t';
            p += 1;
        }
        if has_space && has_tab {
            self.report(at, p, "inconsistent indentation: mixing tabs and spaces");
            self.pending.push((TokenType::Eof, current_line, start, at));
            return (TokenType::Newline, current_line, start, at);
        }
        self.pos = p;
        if self.blank_from(p) { return (TokenType::LineBreak, current_line, start, at); }
        let current = *self.indent_stack.last().unwrap_or(&0);
        if p - at != current { self.indent_to(p - at, current, at, p); }
        (TokenType::Newline, current_line, start, at)
    }

    // queues the indent or the dedents that take the block depth from `current` to `level`
    #[inline(never)]
    fn indent_to(&mut self, level: usize, current: usize, at: usize, p: usize) {
        if level > current {
            if self.indent_stack.len() >= MAX_INDENT_DEPTH {
                self.report(at, p, "indentation depth exceeds maximum (100)");
                self.pending.push((TokenType::Eof, self.line - 1, at - 1, at));
                return;
            }
            self.indent_stack.push(level);
            self.pending.push((TokenType::Indent, self.line, p, p));
            return;
        }
        while self.indent_stack.last().is_some_and(|&t| t > level) {
            self.indent_stack.pop();
            self.pending.push((TokenType::Dedent, self.line, p, p));
        }
        // after popping the new top must equal `level` exactly
        if *self.indent_stack.last().unwrap_or(&0) != level {
            self.report(at, p, "unindent does not match any outer indentation level");
        }
    }

    /* main dispatch draining the pending queue then routing each byte through the class tables */
    #[inline(always)]
    pub fn next_token(&mut self, tokens: &[Token], lead: &mut Lead) -> Option<Tok> {
        // a byte no token starts is reported and skipped and the scan goes on
        loop {
            if let Some(tok) = self.pending.pop() {
                return Some(tok);
            }

            self.skip_whitespace();
            if self.pos >= self.src.len() { return None; }

            let start = self.pos;
            let line_at_start = self.line;
            let b = self.src[self.pos];
            let cl = BYTE_CLASS[b as usize];

            if b == b'\n' {
                self.pos += 1;
                return Some(self.handle_newline(start, tokens));
            }

            // identifiers and keywords and the prefixes of strings
            if cl & ID_START != 0 {
                self.pos += if b >= 0x80 { utf8_char_len(b) } else { 1 };
                self.scan_id_rest();
                let slice = &self.src[start..self.pos];

                // a prefix opens a literal only right before a quote
                if let Some(&q) = self.src.get(self.pos)
                    && (Self::is_quote(q) || (Self::HASHES && q == b'#'))
                    && let Some(flags) = Self::prefix(slice)
                    && let Some(kind) = self.prefixed(start, flags)
                {
                    // a literal of one token skips the pending stack whose first push allocates
                    if kind == TokenType::FormatStart { return self.pending.pop(); }
                    return Some((kind, line_at_start, start, self.pos));
                }

                let mut kind = S::SYNTAX.keywords.find(slice).unwrap_or(TokenType::Name);
                if Self::PHRASES && Self::HEADS[kind as usize & 127] && let Some((phrase, end)) = self.phrase(slice) {
                    kind = phrase;
                    self.pos = end;
                }
                return Some((kind, line_at_start, start, self.pos));
            }

            if cl & DIGIT != 0 {
                self.pos += 1;
                let (kind, end) = self.scan_number(start);
                return Some((kind, line_at_start, start, end));
            }

            if Self::LEADING_DOT && b == b'.' && self.at(1).is_some_and(|c| BYTE_CLASS[c as usize] & DIGIT != 0) {
                self.pos += 1;
                let end = self.scan_dot_number();
                return Some((TokenType::Float, line_at_start, start, end));
            }

            if Self::is_quote(b) {
                if Self::FORMAT_QUOTE[b as usize] {
                    self.start_format(start, start, true);
                    return self.pending.pop();
                }
                self.pos += 1;
                self.scan_string(b, start, true);
                return Some((TokenType::String, line_at_start, start, self.pos));
            }

            if b == b'}' {
                // a block closes every bracket its body left open
                if Self::BRACES && self.fields.is_empty() && self.braces > 1 && self.braces & 1 == 1 {
                    self.pop_brace();
                    self.nesting = 0;
                    self.pos += 1;
                    return Some((TokenType::BlockClose, line_at_start, start, self.pos));
                }
                if !Self::CLOSE_APART && self.fields.last().is_some_and(|f| self.nesting <= f.nesting) { self.close_field(start); return self.pending.pop(); }
                self.nesting = self.nesting.saturating_sub(1);
                if Self::BRACES { self.pop_brace(); }
                self.pos = start + 1;
                return Some((TokenType::BraceClose, line_at_start, start, self.pos));
            }

            // the spec marker atop a field starts the raw text of its spec
            if Self::SPEC != 0 && b == Self::SPEC && self.fields.last().is_some_and(|f| f.nesting == self.nesting) {
                self.open_spec(start, line_at_start);
                return self.pending.pop();
            }

            // bytes that may start a comment pair symbol block brace or conversion go apart
            let flags = if Self::SLOW { Self::SPECIAL[b as usize] } else { 0 };
            let (kind, len) = match flags {
                0 => self.symbol(b, self.at(1)),
                PAIR => self.long_symbol(b),
                COMMENT if Self::BLOCK_MARKS.is_none() => {
                    if self.line_comment_at(start) { return Some(self.line_comment(start, line_at_start)); }
                    self.symbol(b, self.at(1))
                }
                BRACE if Self::BRACES => {
                    let kind = self.open_brace(tokens, lead, start);
                    return Some((kind, line_at_start, start, self.pos));
                }
                CONVERT if Self::CONVERT_SHARED => {
                    let (kind, len) = self.symbol(b, self.at(1));
                    (if !self.fields.is_empty() && self.converts(start) { TokenType::Convert } else { kind }, len)
                }
                _ => {
                    if flags & COMMENT != 0 {
                        if self.line_comment_at(start) { return Some(self.line_comment(start, line_at_start)); }
                        if Self::BLOCK_MARKS.is_some() && self.block_comment(start, line_at_start) { return self.pending.pop(); }
                    }
                    if Self::BRACES && flags & BRACE != 0 {
                        let kind = self.open_brace(tokens, lead, start);
                        return Some((kind, line_at_start, start, self.pos));
                    }
                    if Self::CLOSE_APART && flags & CLOSE != 0 && self.fields.last().is_some_and(|f| self.nesting <= f.nesting) { self.close_field(start); return self.pending.pop(); }
                    let (kind, len) = if flags & PAIR != 0 { self.long_symbol(b) } else { self.symbol(b, self.at(1)) };
                    (if Self::CONVERT_SHARED && flags & CONVERT != 0 && self.converts(start) { TokenType::Convert } else { kind }, len)
                }
            };
            self.pos = start + len;
            // a byte no operator matches is skipped since a stray Eof would end the stream
            if kind == TokenType::Eof {
                self.report(start, self.pos, "unexpected character");
                continue;
            }

            match kind {
                | TokenType::ParenOpen
                | TokenType::BracketOpen
                | TokenType::BraceOpen => self.nesting += 1,
                | TokenType::ParenClose
                | TokenType::BracketClose => self.nesting = self.nesting.saturating_sub(1),
                _ => {},
            }

            return Some((kind, line_at_start, start, self.pos));
        }
    }

    // pushes token `t` with the block bounds its words and line ends imply
    pub(super) fn word_blocks(&mut self, tokens: &mut Vec<Token>, t: Tok) {
        let (kind, line, start, end) = t;
        let push = |tokens: &mut Vec<Token>, kind: TokenType, start: usize, end: usize| tokens.push(Token { kind, line, start, end, comp: false, assigns: 0 });
        let w = &mut self.words;
        match kind {
            TokenType::ParenOpen | TokenType::BracketOpen | TokenType::BraceOpen => { (w.depth, w.fresh) = (w.depth + 1, false); return push(tokens, kind, start, end); }
            TokenType::ParenClose | TokenType::BracketClose | TokenType::BraceClose => { w.depth = w.depth.saturating_sub(1); return push(tokens, kind, start, end); }
            _ if w.depth > 0 => return push(tokens, kind, start, end),
            TokenType::Comment | TokenType::LineBreak => return push(tokens, kind, start, end),
            _ => {}
        }
        let (header, fresh, conditions) = (w.header, w.fresh, w.conditions);
        match kind {
            TokenType::Newline => {
                (w.fresh, w.conditions) = (true, 0);
                match header {
                    // a header still open at its line end opens its block there
                    Some(lead) if Self::IMPLICIT => self.open_clause(tokens, lead, line, start, end),
                    Some(_) => push(tokens, TokenType::LineBreak, start, end),
                    None => push(tokens, TokenType::Newline, start, end),
                }
            }
            TokenType::Semi => { (w.fresh, w.conditions, w.header) = (true, 0, None); push(tokens, kind, start, end); }
            TokenType::BlockOpen => self.open_clause(tokens, header.unwrap_or(TokenType::If), line, start, end),
            TokenType::BlockClose => {
                w.clauses.pop();
                (w.fresh, w.conditions, w.header) = (true, 0, None);
                push(tokens, kind, start, end);
            }
            // the else of a conditional expression in the statement
            TokenType::Else if !fresh && conditions > 0 => { w.conditions -= 1; push(tokens, kind, start, end); }
            TokenType::Else | TokenType::ElseIf | TokenType::Catch | TokenType::Finally => {
                // a chain word closes the clause it continues
                if w.clauses.last().is_some_and(|&c| continues(kind, c)) {
                    w.clauses.pop();
                    push(tokens, TokenType::BlockClose, start, start);
                }
                push(tokens, kind, start, end);
                if matches!(kind, TokenType::Else | TokenType::Finally) {
                    self.open_clause(tokens, kind, line, end, end);
                } else {
                    (w.header, w.fresh, w.conditions) = (Some(kind), false, 0);
                }
            }
            TokenType::Try if fresh => { push(tokens, kind, start, end); self.open_clause(tokens, kind, line, end, end); }
            TokenType::If if !fresh => { w.conditions += 1; push(tokens, kind, start, end); }
            k if fresh && opens_body(k) => { (w.header, w.fresh, w.conditions) = (Some(k), false, 0); push(tokens, kind, start, end); }
            _ => { w.fresh = false; push(tokens, kind, start, end); }
        }
    }

    // a block the header word `lead` opens at `start` with its depth bounded
    fn open_clause(&mut self, tokens: &mut Vec<Token>, lead: TokenType, line: usize, start: usize, end: usize) {
        let deep = self.words.clauses.len();
        (self.words.fresh, self.words.conditions, self.words.header) = (true, 0, None);
        self.words.clauses.push(lead);
        if deep == MAX_INDENT_DEPTH { self.report(start, end, "block nesting depth exceeds maximum (100)"); }
        let kind = if deep >= MAX_INDENT_DEPTH { TokenType::Eof } else { TokenType::BlockOpen };
        tokens.push(Token { kind, line, start, end, comp: false, assigns: 0 });
    }
}

// whether a chain word closes the block a header led by `clause` opened
fn continues(chain: TokenType, clause: TokenType) -> bool {
    use TokenType as T;
    match chain {
        T::ElseIf => matches!(clause, T::If | T::ElseIf),
        T::Else => matches!(clause, T::If | T::ElseIf | T::While | T::For | T::Catch | T::Async),
        T::Catch => matches!(clause, T::Try | T::Catch),
        T::Finally => matches!(clause, T::Try | T::Catch | T::Else),
        _ => false,
    }
}

// whether a token ends the statement before it in a braces table
fn ends_stmt(kind: TokenType) -> bool {
    matches!(kind, TokenType::Newline | TokenType::Semi | TokenType::BlockOpen | TokenType::BlockClose)
}
