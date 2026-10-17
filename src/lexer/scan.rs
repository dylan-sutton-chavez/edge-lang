use super::{TokenType, LexError};
use super::tables::*;

use alloc::vec::Vec;
use core::cmp::Ordering;

/* What a named escape such as `\N{BULLET}` reports, the Unicode name table being left out. */
const NAMED_ESCAPE: &str = "named escapes are not supported, write the character or use \\u03b1";

const MAX_INDENT_DEPTH: usize = 100;
const MAX_FSTRING_DEPTH: usize = 200;

pub(super) struct Scanner<'a> {
    pub src: &'a [u8],
    pub pos: usize,
    /* LIFO queue for states that emit multiple tokens per source position (newline/dedent, f-string). */
    pub pending: Vec<(TokenType,usize,usize,usize)>,
    pub indent_stack: Vec<usize>,
    pub nesting: u32,
    pub line: usize,
    /* Each open f-string, its quote, whether it is triple, whether it decodes escapes, and the bracket depth it opened at. */
    pub fstring_stack: Vec<(u8, bool, bool, u32)>,
    /* Lex-time diagnostics surfaced alongside tokens so the parser folds them into a single coherent error report. */
    pub errors: Vec<LexError>,
}

impl<'a> Scanner<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        Self {
            src, pos: 0,
            pending: Vec::new(),
            indent_stack: Vec::new(),
            nesting: 0, line: 0,
            fstring_stack: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[inline]
    #[cold]
    fn report(&mut self, start: usize, end: usize, msg: &'static str) {
        // A backslash inside an f-string field belongs to its format spec, which the parser reads off the source.
        if self.src.get(start) == Some(&b'\\') && end == start + 1 && !self.fstring_stack.is_empty() { return; }
        self.errors.push(LexError { start, end, msg });
    }

    /* All inner scan loops use BYTE_CLASS indexed loads, zero branches per byte. */
    #[inline]
    fn scan_while(&mut self, pred: impl Fn(u8) -> bool) {
        while self.pos < self.src.len() && pred(self.src[self.pos]) {
            self.pos += 1;
        }
    }

    pub fn skip_whitespace(&mut self) {
        loop {
            self.scan_while(|b| BYTE_CLASS[b as usize] & SPACE != 0);
            // Backslash-newline line continuation, consume the escape and the newline.
            if self.pos + 1 < self.src.len() && self.src[self.pos] == b'\\'
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
        self.scan_while(|b| BYTE_CLASS[b as usize] & DIGIT != 0 || b == b'_');
    }

    /* Underscores must sit between digits, no leading, trailing, or consecutive. Empty body only valid after a decimal dot. */
    fn check_digits(&mut self, start: usize, end: usize, allow_empty: bool) {
        if start == end {
            if !allow_empty {
                self.report(start, end, "missing digits in numeric literal");
            }
            return;
        }
        let s = &self.src[start..end];
        if s[0] == b'_' || s[s.len() - 1] == b'_' {
            self.report(start, end, "invalid '_' in numeric literal");
        } else if s.windows(2).any(|w| w == *b"__") {
            self.report(start, end, "consecutive '_' in numeric literal");
        }
    }
    // One underscore may follow the base prefix, as in `0x_FF`.
    fn check_prefixed(&mut self, b: usize) {
        let skip = usize::from(self.src.get(b) == Some(&b'_') && b + 1 < self.pos);
        self.check_digits(b + skip, self.pos, false);
    }
    fn scan_hex_digits(&mut self) {
        self.scan_while(|b| b.is_ascii_hexdigit() || b == b'_');
    }
    fn scan_oct_digits(&mut self) {
        self.scan_while(|b| matches!(b, b'0'..=b'7' | b'_'));
    }
    fn scan_bin_digits(&mut self) {
        self.scan_while(|b| matches!(b, b'0' | b'1' | b'_'));
    }

    #[inline(always)]
    fn at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    // Numbers, decimal, hex, octal, binary, float.

    #[inline]
    fn scan_exponent(&mut self) -> bool {
        if self.pos < self.src.len() && matches!(self.src[self.pos], b'e' | b'E') {
            self.pos += 1;
            if self.pos < self.src.len() && matches!(self.src[self.pos], b'+' | b'-') {
                self.pos += 1;
            }
            let body_start = self.pos;
            self.scan_digits();
            /* Format specs reuse `e/E` (e.g. `.2e`), leave empty-exponent detection to the float parser instead of erroring at lex time. */
            self.check_digits(body_start, self.pos, true);
            true
        } else {
            false
        }
    }

    fn scan_number(&mut self, start: usize) -> TokenType {
        if self.src[start] == b'0' && self.pos < self.src.len() {
            match self.src[self.pos] {
                b'x' | b'X' => { self.pos += 1; self.scan_hex_digits(); self.check_prefixed(start + 2); return TokenType::Int; }
                b'o' | b'O' => { self.pos += 1; self.scan_oct_digits(); self.check_prefixed(start + 2); return TokenType::Int; }
                b'b' | b'B' => { self.pos += 1; self.scan_bin_digits(); self.check_prefixed(start + 2); return TokenType::Int; }
                _ => {}
            }
        }
        self.scan_digits();
        self.check_digits(start, self.pos, false);
        let mut is_float = false;
        if self.pos < self.src.len() && self.src[self.pos] == b'.' && self.at(1) != Some(b'.') {
            is_float = true;
            self.pos += 1;
            let frac_start = self.pos;
            self.scan_digits();
            // Trailing dot like `5.` is valid, only validate non-empty fractions.
            self.check_digits(frac_start, self.pos, true);
        }
        is_float |= self.scan_exponent();
        // An imaginary literal reads as a float its parser sees the `j` of.
        is_float |= self.imaginary();
        if is_float { TokenType::Float } else { TokenType::Int }
    }

    fn scan_dot_number(&mut self) -> TokenType {
        let body_start = self.pos;
        self.scan_digits();
        self.check_digits(body_start, self.pos, false);
        self.scan_exponent();
        self.imaginary();
        TokenType::Float
    }

    /* Eats the `j` suffix of an imaginary literal, true when there was one. */
    fn imaginary(&mut self) -> bool {
        let j = matches!(self.at(0), Some(b'j' | b'J'));
        if j { self.pos += 1; }
        j
    }

    // Strings, single, double, triple-quoted (escape-aware).

    /* `decodes` is false for a raw or bytes literal, where `\N{...}` stays plain text. */
    fn scan_string(&mut self, quote: u8, start: usize, decodes: bool) {
        if self.at(0) == Some(quote) && self.at(1) == Some(quote) {
            self.pos += 2;
            self.scan_triple_string(quote, start, decodes);
        } else {
            self.scan_single_string(quote, start, decodes);
        }
    }

    fn check_escape(&mut self, decodes: bool) {
        if decodes && self.at(1) == Some(b'N') && self.at(2) == Some(b'{') {
            self.report(self.pos, self.pos + 3, NAMED_ESCAPE);
        }
    }

    fn scan_single_string(&mut self, quote: u8, start: usize, decodes: bool) {
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b == quote { self.pos += 1; return; }
            if b == b'\\' && self.at(1).is_some() { self.check_escape(decodes); self.pos += 1; }
            if b == b'\n' {
                /* Anchor unterminated-string error at the opener so `^` lands on the quote, not end-of-line. */
                self.report(start, start + 1, "unterminated string literal");
                return;
            }
            self.pos += 1;
        }
        // Hit EOF without closing quote.
        self.report(start, start + 1, "unterminated string literal");
    }

    fn scan_triple_string(&mut self, quote: u8, start: usize, decodes: bool) {
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b == quote && self.at(1) == Some(quote) && self.at(2) == Some(quote) {
                self.pos += 3; return;
            }
            if b == b'\\' && self.at(1).is_some() { self.check_escape(decodes); self.pos += 1; }
            if b == b'\n' { self.line += 1; }
            self.pos += 1;
        }
        // EOF inside triple-quoted string.
        self.report(start, start + 3, "unterminated triple-quoted string literal");
    }

    // F-strings emit Start/Middle/End and suspend at `{`.

    fn start_fstring(&mut self, start: usize, prefix_end: usize) {
        let quote = self.src[prefix_end];
        let triple = self.src.get(prefix_end + 1) == Some(&quote) && self.src.get(prefix_end + 2) == Some(&quote);
        let quote_len = if triple { 3 } else { 1 };
        self.pos = prefix_end + quote_len;
        let body_start = self.pos;
        let decodes = !self.src[start..prefix_end].iter().any(|c| c.eq_ignore_ascii_case(&b'r'));
        self.scan_fstring_body(quote, triple, decodes, body_start);
        self.pending.push((TokenType::FstringStart, self.line, start, body_start));
    }

    fn scan_fstring_body(&mut self, quote: u8, triple: bool, decodes: bool, body_start: usize) {
        let ql: usize = if triple { 3 } else { 1 };
        let mut pos = self.pos;
        while pos < self.src.len() {
            let closes = if triple {
                pos + 2 < self.src.len()
                    && self.src[pos] == quote
                    && self.src[pos + 1] == quote
                    && self.src[pos + 2] == quote
            } else {
                self.src[pos] == quote
            };
            if closes {
                self.pending.push((TokenType::FstringEnd, self.line, pos, pos + ql));
                // Pushed after End so LIFO pop yields Middle first.
                if pos > self.pos {
                    self.pending.push((TokenType::FstringMiddle, self.line, body_start, pos));
                }
                self.pos = pos + ql;
                return;
            }
            match self.src[pos] {
                // A named escape fails, and its braces stay text rather than open a field.
                b'\\' if decodes && self.src.get(pos + 1) == Some(&b'N') && self.src.get(pos + 2) == Some(&b'{') => {
                    self.report(pos, pos + 3, NAMED_ESCAPE);
                    pos = self.src[pos..].iter().position(|&c| c == b'}').map_or(self.src.len(), |n| pos + n + 1);
                }
                // A raw backslash stays text and shields only a quote or a backslash, so a brace after it still counts.
                b'\\' if !decodes && !matches!(self.src.get(pos + 1), Some(&c) if c == quote || c == b'\\') => pos += 1,
                b'\\' => pos = (pos + 2).min(self.src.len()),
                b'{' if self.src.get(pos + 1) == Some(&b'{') => {
                    pos += 2;
                }
                b'}' if self.src.get(pos + 1) == Some(&b'}') => {
                    pos += 2;
                }
                b'}' => {
                    self.report(pos, pos + 1, "f-string: single '}' is not allowed");
                    pos += 1;
                }
                b'{' => {
                    if self.fstring_stack.len() >= MAX_FSTRING_DEPTH {
                        self.report(pos, pos + 1, "f-string nesting depth exceeds maximum (200)");
                        self.pos = pos + 1;
                        return;
                    }
                    self.pending.push((TokenType::Lbrace, self.line, pos, pos + 1));
                    if pos > self.pos {
                        self.pending.push((TokenType::FstringMiddle, self.line, body_start, pos));
                    }
                    self.fstring_stack.push((quote, triple, decodes, self.nesting));
                    self.pos = pos + 1;
                    return;
                }
                b'\n' => { self.line += 1; pos += 1; }
                _ => pos += 1,
            }
        }
        /* Synthesise End on EOF so the parser sees a balanced structure, then report the unterminated literal. */
        self.report(self.pos, pos, "unterminated f-string literal");
        self.pending.push((TokenType::FstringEnd, self.line, pos, pos));
        if pos > self.pos {
            self.pending.push((TokenType::FstringMiddle, self.line, body_start, pos));
        }
        self.pos = pos;
    }

    // Newlines emit Newline/Indent/Dedent, or NL inside brackets.

    fn handle_newline(&mut self, start: usize) {
        let current_line = self.line;
        self.line += 1;

        if self.nesting > 0 {
            self.pending.push((TokenType::Nl, current_line, start, self.pos));
            return;
        }

        let mut level = 0usize;
        let mut has_space = false;
        let mut has_tab = false;
        let mut p = self.pos;
        while p < self.src.len() && (self.src[p] == b' ' || self.src[p] == b'\t') {
            has_space |= self.src[p] == b' ';
            has_tab |= self.src[p] == b'\t';
            level += 1; p += 1;
        }

        if has_space && has_tab {
            self.report(self.pos, p, "inconsistent indentation: mixing tabs and spaces");
            self.pending.push((TokenType::Endmarker, current_line, start, self.pos));
            self.pending.push((TokenType::Newline, current_line, start, self.pos));
            return;
        }

        if matches!(self.src.get(p), Some(b'\n' | b'\r' | b'#')) {
            self.pending.push((TokenType::Nl, current_line, start, self.pos));
            return;
        }

        let line_pos = self.pos + level;
        let current = *self.indent_stack.last().unwrap_or(&0);

        match level.cmp(&current) {
            Ordering::Greater => {
                if self.indent_stack.len() >= MAX_INDENT_DEPTH {
                    self.report(self.pos, line_pos, "indentation depth exceeds maximum (100)");
                    self.pending.push((TokenType::Endmarker, current_line, start, self.pos));
                    self.pending.push((TokenType::Newline, current_line, start, self.pos));
                    return;
                }
                self.indent_stack.push(level);
                self.pending.push((TokenType::Indent, self.line, line_pos, line_pos));
                self.pending.push((TokenType::Newline,current_line, start, self.pos));
            }
            Ordering::Less => {
                while self.indent_stack.last().is_some_and(|&t| t > level) {
                    self.indent_stack.pop();
                    self.pending.push((TokenType::Dedent, self.line, line_pos, line_pos));
                }
                // After popping, the new top must equal `level` exactly.
                if *self.indent_stack.last().unwrap_or(&0) != level {
                    self.report(self.pos, line_pos, "unindent does not match any outer indentation level");
                }
                self.pending.push((TokenType::Newline, current_line, start, self.pos));
            }
            Ordering::Equal => {
                self.pending.push((TokenType::Newline, current_line, start, self.pos));
            }
        }
    }

    /* Routes `}` to f-string body resume or plain Rbrace based on nesting depth. */
    fn close_brace(&mut self, start: usize) {
        let end = start + 1;
        if let Some(&(_, _, _, saved_nesting)) = self.fstring_stack.last() {
            if self.nesting > saved_nesting {
                self.nesting -= 1;
            } else {
                let (quote, triple, decodes, _) = self.fstring_stack.pop().unwrap();
                self.pos = end;
                self.scan_fstring_body(quote, triple, decodes, end);
                self.pending.push((TokenType::Rbrace, self.line, start, end));
                // `pos` already advanced by scan_fstring_body.
                return;
            }
        } else {
            self.nesting = self.nesting.saturating_sub(1);
        }
        self.pending.push((TokenType::Rbrace, self.line, start, end));
        self.pos = end;
    }

    /* Main dispatch, drains pending queue, then routes each byte via BYTE_CLASS / OPERATORS lookups. */
    pub fn next_token(&mut self) -> Option<(TokenType, usize, usize, usize)> {
        if let Some(tok) = self.pending.pop() {
            return Some(tok);
        }

        self.skip_whitespace();
        if self.pos >= self.src.len() { return None; }

        let start = self.pos;
        let line_at_start = self.line;
        let b = self.src[self.pos];
        let cl = BYTE_CLASS[b as usize];

        // Newline
        if b == b'\n' {
            self.pos += 1;
            self.handle_newline(start);
            return self.pending.pop();
        }

        if b == b'#' { // inside an f-string interpolation, '#' is part of the format spec, skip silently
            if !self.fstring_stack.is_empty() {
                self.pos += 1;
                return self.next_token();
            }
            while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                self.pos += 1;
            }
            return Some((TokenType::Comment, line_at_start, start, self.pos));
        }

        // Identifier / keyword / string-prefix / f-string-prefix
        if cl & ID_START != 0 {
            self.pos += if b >= 0x80 { utf8_char_len(b) } else { 1 };
            self.scan_id_rest();
            let slice = &self.src[start..self.pos];

            // A prefix opens a literal only right before a quote, the parser decodes bytes-specific escapes later.
            if let Some(&q @ (b'"' | b'\'')) = self.src.get(self.pos)
                && let Some(kind) = string_prefix(slice)
            {
                if kind == TokenType::FstringStart {
                    let pe = self.pos;
                    self.start_fstring(start, pe);
                    return self.pending.pop();
                }
                let q_start = self.pos;
                self.pos += 1;
                let decodes = kind == TokenType::String && !slice.iter().any(|c| c.eq_ignore_ascii_case(&b'r'));
                self.scan_string(q, q_start, decodes);
                return Some((kind, line_at_start, start, self.pos));
            }

            let kind = keyword(slice).unwrap_or(TokenType::Name);
            return Some((kind, line_at_start, start, self.pos));
        }

        // Number (digit start)
        if cl & DIGIT != 0 {
            self.pos += 1;
            let kind = self.scan_number(start);
            return Some((kind, line_at_start, start, self.pos));
        }

        // Ellipsis
        if b == b'.' && self.at(1) == Some(b'.') && self.at(2) == Some(b'.') {
            self.pos += 3;
            return Some((TokenType::Ellipsis, line_at_start, start, self.pos));
        }

        // Dot-number (.123)
        if b == b'.' && self.at(1).is_some_and(|c| BYTE_CLASS[c as usize] & DIGIT != 0) {
            self.pos += 1;
            let kind = self.scan_dot_number();
            return Some((kind, line_at_start, start, self.pos));
        }

        // Bare string
        if b == b'"' || b == b'\'' {
            self.pos += 1;
            self.scan_string(b, start, true);
            return Some((TokenType::String, line_at_start, start, self.pos));
        }

        // Close brace (f-string aware)
        if b == b'}' {
            self.close_brace(start);
            return self.pending.pop();
        }

        // Operators by first byte, a doubled byte and a trailing `=` picking the longer forms.
        let [alone, with_eq, doubled, doubled_eq] = OPERATORS[b as usize & 127];
        let next = self.at(1);
        // At the top of an f-string field a colon opens the spec, so `f"{x:=^9}"` fills with `=`.
        let spec = b == b':' && self.fstring_stack.last().is_some_and(|f| f.3 == self.nesting);
        let (kind, len) = if next == Some(b) && doubled != TokenType::Endmarker {
            if self.at(2) == Some(b'=') { (doubled_eq, 3) } else { (doubled, 2) }
        } else if next == Some(b'=') && with_eq != TokenType::Endmarker && !spec {
            (with_eq, 2)
        } else if b == b'-' && next == Some(b'>') {
            (TokenType::Rarrow, 2)
        } else {
            (alone, 1)
        };
        self.pos += len;
        /* Endmarker means no operator, report and skip instead of emitting a stray Endmarker that truncates the stream. */
        if kind == TokenType::Endmarker {
            self.report(start, self.pos, "unexpected character");
            return self.next_token();
        }

        match kind {
            | TokenType::Lpar
            | TokenType::Lsqb
            | TokenType::Lbrace => self.nesting += 1,
            | TokenType::Rpar
            | TokenType::Rsqb => self.nesting = self.nesting.saturating_sub(1),
            _ => {},
        }

        Some((kind, line_at_start, start, self.pos))
    }
}
