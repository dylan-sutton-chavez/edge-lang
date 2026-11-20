use super::TokenType;
use super::TokenType as T;

// where a table starts and ends a block
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Blocks {
    // a header colon then deeper indentation
    Indent,
    // the BlockOpen and BlockClose symbols of the table after a header and its body
    Braces,
    // block words where an implicit table also opens a block at a header's line end
    Words { implicit: bool },
}

// what ends a statement outside brackets
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ends {
    // a line end and never a semicolon
    Newline,
    // a semicolon or the end of its block while line ends count for nothing
    Semicolon,
    // a line end or a semicolon
    Either,
}

// the comments of a table
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Comments {
    // the markers of a comment that runs to the end of its line
    pub(crate) line: &'static [&'static str],
    // the markers that open and close a comment of any length
    pub(crate) block: Option<(&'static str, &'static str)>,
    // whether a block comment inside a block comment needs a close of its own
    pub(crate) nested: bool,
}

impl Comments {
    pub const PLAIN: Comments = Comments { line: &[], block: None, nested: false };

    setters! {
        line: &'static [&'static str],
        block: Option<(&'static str, &'static str)>,
        nested: bool,
    }
}

// a quote byte and the literals it opens
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Quote {
    pub(crate) byte: u8,
    // whether a literal of one quote may hold a line end
    pub(crate) multiline: bool,
    // whether three quotes open a long literal that spans lines
    pub(crate) triple: bool,
}

impl Quote {
    pub const PLAIN: Quote = Quote { byte: b'"', multiline: false, triple: false };

    setters! {
        byte: u8,
        multiline: bool,
        triple: bool,
    }
}

// a prefix keeps the backslashes of its literal as written
pub const RAW: u8 = 1;
// a prefix makes its literal a bytes literal
pub const BYTES: u8 = 2;
// a prefix makes its literal interpolate
pub const FORMAT: u8 = 4;

// how a backslash names a code point
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unicode {
    None,
    // `\u` with four hex digits and `\U` with eight
    Fixed,
    // `\u` with one to six hex digits in braces
    Braced,
}

// what an escape the table lacks becomes
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unknown {
    // the backslash stays beside the byte
    Keep,
    // the literal fails to lex
    Refuse,
}

// the escapes a literal decodes
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Escapes {
    // the byte after a backslash and the character it stands for
    pub(crate) simple: &'static [(u8, char)],
    // whether `\x` takes two hex digits
    pub(crate) hex: bool,
    pub(crate) unicode: Unicode,
    // whether one to three octal digits name a character
    pub(crate) octal: bool,
    // whether a backslash before a line end joins the two lines
    pub(crate) line_join: bool,
    pub(crate) unknown: Unknown,
    // escapes that fail as the source lexes with the message each one reports
    pub(crate) refused: &'static [(u8, &'static str)],
}

impl Escapes {
    pub const PLAIN: Escapes = Escapes { simple: &[], hex: false, unicode: Unicode::None, octal: false, line_join: false, unknown: Unknown::Keep, refused: &[] };

    setters! {
        simple: &'static [(u8, char)],
        hex: bool,
        unicode: Unicode,
        octal: bool,
        line_join: bool,
        unknown: Unknown,
        refused: &'static [(u8, &'static str)],
    }
}

// the strings of a table
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Strings {
    pub(crate) quotes: &'static [Quote],
    // each prefix in lowercase with the RAW BYTES and FORMAT bits it sets
    pub(crate) prefixes: &'static [(&'static str, u8)],
    // whether a prefix reads in either case
    pub(crate) any_case: bool,
    // whether a raw prefix takes hashes around its quotes as in `r#"a"b"#`
    pub(crate) hashes: bool,
    // whether literals side by side join into one
    pub(crate) adjacent: bool,
    // whether CR and CRLF read as LF inside a literal
    pub(crate) newlines: bool,
    pub(crate) escapes: Escapes,
}

impl Strings {
    pub const PLAIN: Strings = Strings { quotes: &[], prefixes: &[], any_case: false, hashes: false, adjacent: false, newlines: false, escapes: Escapes::PLAIN };

    setters! {
        quotes: &'static [Quote],
        prefixes: &'static [(&'static str, u8)],
        any_case: bool,
        hashes: bool,
        adjacent: bool,
        newlines: bool,
        escapes: Escapes,
    }
}

// what leading zeros make of a decimal integer
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Zeros {
    // an error unless every digit is zero
    Refused,
    // the digits read in base eight
    Octal,
    Allowed,
}

// what letters right after a number make of it
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Suffix {
    // they start the next token
    Apart,
    // the number fails to lex
    Refused,
    // the lexer drops them
    Ignored,
}

// the numbers of a table
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Numbers {
    // the letter after a leading zero in either case and the radix it names
    pub(crate) radixes: &'static [(u8, u32)],
    // the byte that may sit between digits
    pub(crate) separator: Option<u8>,
    pub(crate) zeros: Zeros,
    // whether a float may start at its dot as in `.5`
    pub(crate) leading_dot: bool,
    // whether a float may end at its dot as in `5.`
    pub(crate) trailing_dot: bool,
    // whether `e` and `E` give a float an exponent
    pub(crate) exponent: bool,
    pub(crate) suffix: Suffix,
}

impl Numbers {
    pub const PLAIN: Numbers = Numbers { radixes: &[], separator: None, zeros: Zeros::Allowed, leading_dot: false, trailing_dot: false, exponent: false, suffix: Suffix::Apart };

    setters! {
        radixes: &'static [(u8, u32)],
        separator: Option<u8>,
        zeros: Zeros,
        leading_dot: bool,
        trailing_dot: bool,
        exponent: bool,
        suffix: Suffix,
    }
}

// how a field renders its value before any spec
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Mode {
    Str,
    Repr,
    Ascii,
}

impl Mode {
    // the conversion bits of a FormatValue operand
    pub const fn flags(self) -> u16 {
        match self { Mode::Repr => 1 << 1, Mode::Str => 2 << 1, Mode::Ascii => 3 << 1 }
    }
}

// how the text of an interpolated literal writes a field delimiter as plain text
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Literal {
    // the delimiter twice as in `{{`
    Doubled,
    // a backslash before it as in `\{`
    Backslash,
}

// literals holding fields an expression fills in when they run
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Interp {
    // what diagnostics call such a literal
    pub(crate) name: &'static str,
    // the quotes whose literals interpolate with no prefix
    pub(crate) quotes: &'static [u8],
    // the bytes that open a field
    pub(crate) open: &'static str,
    pub(crate) close: u8,
    pub(crate) literal: Literal,
    // the byte before a bare name that makes the name a field as `$name` does
    pub(crate) short: Option<u8>,
    // the byte after a field's expression that starts its format spec
    pub(crate) spec: Option<u8>,
    // the byte before a letter naming a render mode and each letter with its mode
    pub(crate) convert: Option<(u8, &'static [(u8, Mode)])>,
    // the byte after an expression that keeps the field's text beside its value
    pub(crate) echo: Option<u8>,
}

impl Interp {
    pub const PLAIN: Interp = Interp { name: "", quotes: &[], open: "{", close: b'}', literal: Literal::Doubled, short: None, spec: None, convert: None, echo: None };

    setters! {
        name: &'static str,
        quotes: &'static [u8],
        open: &'static str,
        close: u8,
        literal: Literal,
        short: Option<u8>,
        spec: Option<u8>,
        convert: Option<(u8, &'static [(u8, Mode)])>,
        echo: Option<u8>,
    }
}

// how a statement goes on past a line end outside brackets
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Continuation {
    // the byte that joins a line ending in it to the next
    pub(crate) join: Option<u8>,
    // whether a line ending in a binary operator goes on
    pub(crate) operator: bool,
}

impl Continuation {
    pub const PLAIN: Continuation = Continuation { join: None, operator: false };

    setters! {
        join: Option<u8>,
        operator: bool,
    }
}

// how a conditional expression reads
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cond {
    // the value then the test as in `a if c else b`
    Suffix,
    // the test then a question mark then the two values
    Question,
    None,
}

// how an anonymous function reads
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lambda {
    // its word then the parameters up to a colon then the body
    Word,
    // the parameters between bars then the body as in `|x| x + 1`
    Pipes,
    None,
}

// what a name after a dot reads
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dot {
    Attr,
    // the item at that name as a text key unless a call follows
    Key,
}

// whether a word binds names before an assignment
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Binding {
    None,
    // the word may lead any assignment of names
    Optional,
    // the first store of a name in a scope needs the word
    Required,
}

// the grammar shapes of a table and the kit pieces it switches on
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Shapes {
    pub(crate) cond: Cond,
    pub(crate) lambda: Lambda,
    // the token between a keyword argument's name and its value
    pub(crate) keyword_arg: TokenType,
    // whether the star markers spread values and gather the rest
    pub(crate) splat: bool,
    // whether a subscript takes slices with colons between their bounds
    pub(crate) slices: bool,
    pub(crate) dot: Dot,
    // whether a header holds its expression in parentheses as in `if (c)`
    pub(crate) header_parens: bool,
    pub(crate) comprehensions: bool,
    // whether the At marker before a definition decorates it
    pub(crate) decorators: bool,
    // whether a name may take a value inside an expression
    pub(crate) named: bool,
    pub(crate) binding: Binding,
    // whether a match takes arms as in `p => body` in place of cases
    pub(crate) arms: bool,
    // whether `a..b` and `a..=b` make ranges
    pub(crate) ranges: bool,
    // the modes and forms that read the table's expressions in place of the fixed core
    pub(crate) grammar: Option<&'static super::grammar::Grammar>,
    // whether a free name may read a field of the value a builder names
    pub(crate) subject: bool,
}

impl Shapes {
    setters! {
        cond: Cond,
        lambda: Lambda,
        keyword_arg: TokenType,
        splat: bool,
        slices: bool,
        dot: Dot,
        header_parens: bool,
        comprehensions: bool,
        decorators: bool,
        named: bool,
        binding: Binding,
        arms: bool,
        ranges: bool,
        grammar: Option<&'static super::grammar::Grammar>,
    }
}

impl Shapes {
    // the shapes of the fixed core with every kit piece off
    pub const PLAIN: Shapes = Shapes {
        cond: Cond::Suffix, lambda: Lambda::Word, keyword_arg: T::Equal, splat: true, slices: true, dot: Dot::Attr,
        header_parens: false, comprehensions: true, decorators: true, named: true, binding: Binding::None, arms: false, ranges: false, grammar: None, subject: false,
    };
}

// the token table a runtime brings to the shared lexer and parser
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Syntax {
    pub name: &'static str,
    // the source file extension an import implies without its dot
    pub extension: &'static str,
    // every reserved word with its token where a space joins a phrase like `else if`
    pub words: &'static [(&'static str, TokenType)],
    // symbols of one to three bytes with their tokens where a text's first entry wins
    pub symbols: &'static [(&'static str, TokenType)],
    // the symbols by first byte alone then with `=` then doubled then doubled with `=`
    pub operators: &'static [[TokenType; 4]; 128],
    // the index of `words` a name looks up once
    pub keywords: &'static Keywords,
    // each operator with its left and right power where a zero left marks a prefix
    pub precedence: &'static [(TokenType, u8, u8)],
    // whether `a < b < c` tests `a < b` and `b < c`
    pub chained: bool,
    pub blocks: Blocks,
    pub ends: Ends,
    pub comments: Comments,
    pub strings: Strings,
    pub interp: Option<Interp>,
    pub numbers: Numbers,
    pub continuation: Continuation,
    pub shapes: Shapes,
    // the words reserved only where the grammar wants them
    pub soft: &'static [TokenType],
    // whether a byte order mark at the start is skipped
    pub bom: bool,
}

impl Syntax {
    pub const PLAIN: Syntax = Syntax {
        name: "", extension: "", words: &[], symbols: &[], operators: &operators(&[]), keywords: &index(&[]), precedence: &[], chained: false,
        blocks: Blocks::Braces, ends: Ends::Semicolon, comments: Comments::PLAIN, strings: Strings::PLAIN, interp: None, numbers: Numbers::PLAIN,
        continuation: Continuation::PLAIN, shapes: Shapes::PLAIN, soft: &[], bom: false,
    };

    setters! {
        name: &'static str,
        extension: &'static str,
        words: &'static [(&'static str, TokenType)],
        symbols: &'static [(&'static str, TokenType)],
        operators: &'static [[TokenType; 4]; 128],
        keywords: &'static Keywords,
        precedence: &'static [(TokenType, u8, u8)],
        chained: bool,
        blocks: Blocks,
        ends: Ends,
        comments: Comments,
        strings: Strings,
        interp: Option<Interp>,
        numbers: Numbers,
        continuation: Continuation,
        shapes: Shapes,
        soft: &'static [TokenType],
        bom: bool,
    }
}

impl Syntax {
    internal! {
        pub const fn braces(&self) -> bool { matches!(self.blocks, Blocks::Braces) }
    }

    pub(crate) const fn indent(&self) -> bool { matches!(self.blocks, Blocks::Indent) }

    // whether BlockOpen and BlockClose tokens bound each block
    internal! {
        pub const fn delimited(&self) -> bool { !self.indent() }
    }

    // the token a compound header ends at right before its block
    pub(crate) const fn opener(&self) -> TokenType { if self.delimited() { T::BlockOpen } else { T::Colon } }

    // how the table writes `kind` or None when it has no such token
    pub(crate) fn spelling(&self, kind: TokenType) -> Option<&'static str> {
        self.words.iter().chain(self.symbols).find(|(_, k)| *k == kind).map(|&(s, _)| s)
    }

    // the token a word reserves in this table
    pub fn word(&self, text: &str) -> Option<TokenType> { self.keywords.find(text.as_bytes()) }

    // what `describe` gives every token at compile time or bare with `quoted` off
    pub(crate) const fn shown(&self, quoted: bool) -> Shown {
        let mut s = Shown { bytes: [0; 4096], spans: [(0, 0); 128] };
        let interp = match self.interp { Some(i) => i.name, None => "interpolated string" };
        let mut n = 0usize;
        let mut k = 0;
        while k < 128 {
            let start = n;
            let mut i = 0;
            let mut text: &str = "";
            let mut quote = false;
            while i < self.words.len() + self.symbols.len() {
                let (t, kind) = self.entry(i);
                if kind as usize == k { (text, quote) = (t, true); break; }
                i += 1;
            }
            let (head, tail) = if quote { (text, "") } else {
                match k {
                    _ if k == T::FormatStart as usize => (interp, " start"),
                    _ if k == T::FormatText as usize => (interp, " content"),
                    _ if k == T::FormatEnd as usize => (interp, " end"),
                    _ if k <= T::Question as usize => (match kind_at(k) { Some(kind) => role(kind), None => UNSPELLED }, ""),
                    _ => ("", ""),
                }
            };
            let mark = if quoted { "'" } else { "" };
            let parts: [&str; 4] = if quote { [mark, head, mark, tail] } else { ["", head, tail, ""] };
            let mut p = 0;
            while p < 4 {
                let b = parts[p].as_bytes();
                let mut j = 0;
                while j < b.len() {
                    assert!(n < 4096, "a table's spellings fit four kilobytes");
                    s.bytes[n] = b[j];
                    n += 1;
                    j += 1;
                }
                p += 1;
            }
            assert!(n - start <= TEXT_MAX, "a token's text in a diagnostic is at most 40 bytes, shorten the reserved word or the interpolated string name");
            s.spans[k] = (start as u16, n as u16);
            k += 1;
        }
        s
    }

    // whether `kind` is reserved only where the grammar wants it
    internal! {
        pub const fn is_soft(&self, kind: TokenType) -> bool {
            let mut i = 0;
            while i < self.soft.len() {
                if self.soft[i] as u8 == kind as u8 { return true; }
                i += 1;
            }
            false
        }
    }

    // whether some reserved word is two words like `else if`
    internal! {
        pub const fn phrases(&self) -> bool { self.phrase_list().1 > 0 }
    }

    // which tokens open a phrase with Name standing for an unlisted first word
    internal! {
        pub const fn heads(&self) -> [bool; 128] {
            let ((list, n), mut t, mut i) = (self.phrase_list(), [false; 128], 0);
            while i < n { t[self.head(list[i].first) as usize & 127] = true; i += 1; }
            t
        }
    }

    // the token of the word spelled `w`
    const fn head(&self, w: &[u8]) -> TokenType {
        let mut i = 0;
        while i < self.words.len() {
            if same(self.words[i].0.as_bytes(), w) { return self.words[i].1; }
            i += 1;
        }
        T::Name
    }

    // whether the conversion byte of a field also spells a symbol outside fields
    internal! {
        pub const fn convert_shared(&self) -> bool {
            let Some(Interp { convert: Some((b, _)), .. }) = self.interp else { return false };
            let mut i = 0;
            while i < self.symbols.len() {
                let (s, kind) = self.symbols[i];
                if same(s.as_bytes(), &[b]) && kind as u8 != T::Convert as u8 { return true; }
                i += 1;
            }
            false
        }
    }

    // whether some symbol has no column of the operator table
    internal! { tests
        pub const fn pairs(&self) -> bool { self.pair_list().1 > 0 }
    }

    // the first bytes of the symbols only the pair scan finds
    internal! {
        pub const fn pair_starts(&self) -> [bool; 128] {
            let ((list, n), mut t, mut i) = (self.pair_list(), [false; 128], 0);
            while i < n { t[list[i].0[0] as usize & 127] = true; i += 1; }
            t
        }
    }

    // each phrase as its first word and the text after its space in table order
    pub(super) const fn phrase_list(&self) -> ([Phrase; MAX_PHRASES], usize) {
        let mut t = [Phrase { first: &[], rest: &[], kind: T::Name }; MAX_PHRASES];
        let (mut n, mut i) = (0, 0);
        while i < self.words.len() {
            let (text, kind) = self.words[i];
            let w = text.as_bytes();
            let at = space(w);
            if at < w.len() {
                assert!(n < MAX_PHRASES, "a table holds at most eight phrases");
                let (first, rest) = w.split_at(at);
                t[n] = Phrase { first, rest: rest.split_at(1).1, kind };
                n += 1;
            }
            i += 1;
        }
        (t, n)
    }

    // the symbols only the pair scan finds longest first and in table order among equals
    pub(super) const fn pair_list(&self) -> ([(&'static [u8], TokenType); MAX_PAIRS], usize) {
        let mut t: [(&'static [u8], TokenType); MAX_PAIRS] = [(&[], T::Name); MAX_PAIRS];
        let (mut n, mut i) = (0, 0);
        while i < self.symbols.len() {
            let (text, kind) = self.symbols[i];
            let s = text.as_bytes();
            if paired(s, self.symbols) {
                assert!(n < MAX_PAIRS, "a table holds at most sixteen symbols outside the operator table");
                let mut j = n;
                while j > 0 && t[j - 1].0.len() < s.len() { t[j] = t[j - 1]; j -= 1; }
                t[j] = (s, kind);
                n += 1;
            }
            i += 1;
        }
        (t, n)
    }

    // the one byte symbol spelling `kind` or zero
    pub(crate) const fn byte_of(&self, kind: TokenType) -> u8 {
        let mut i = 0;
        while i < self.symbols.len() {
            let (s, k) = self.symbols[i];
            if k as u8 == kind as u8 && s.len() == 1 { return s.as_bytes()[0]; }
            i += 1;
        }
        0
    }

    // whether some word or symbol spells `kind`
    pub(crate) const fn spells(&self, kind: TokenType) -> bool {
        let mut i = 0;
        while i < self.words.len() + self.symbols.len() {
            if self.entry(i).1 as u8 == kind as u8 { return true; }
            i += 1;
        }
        false
    }

    // refuses at compile time a table the lexer and parser cannot read one way
    internal! {
        pub const fn check(&self) {
            let implicit = matches!(self.blocks, Blocks::Words { implicit: true });
            assert!(!(implicit && matches!(self.ends, Ends::Semicolon)), "a table that opens blocks at line ends needs line ends to end statements");
            assert!(!(self.indent() && matches!(self.ends, Ends::Semicolon)), "an indentation table needs line ends to end statements");
            assert!(!(matches!(self.ends, Ends::Newline) && self.spells(T::Semi)), "a table whose statements end at line ends spells no semicolon");
            assert!(self.indent() || (self.spells(T::BlockOpen) && self.spells(T::BlockClose)), "a table that delimits blocks spells their bounds");
            assert!(!self.braces() || (self.byte_of(T::BlockOpen) == b'{' && self.byte_of(T::BlockClose) == b'}'), "a braces table bounds blocks with braces");
            assert!(!(implicit && (self.is_soft(T::Match) || self.is_soft(T::Case) || self.is_soft(T::Fn))), "a table that opens blocks at line ends reserves its header words");
            let n = self.words.len() + self.symbols.len();
            let mut i = 0;
            while i < n {
                let (a, ka) = self.entry(i);
                let mut j = i + 1;
                while j < n {
                    let (b, kb) = self.entry(j);
                    if same(a.as_bytes(), b.as_bytes()) && ka as u8 != kb as u8 {
                        assert!(shares(ka, kb) || shares(kb, ka), "two tokens of a table share one text");
                    }
                    j += 1;
                }
                i += 1;
            }
            let mut q = 0;
            while q < self.strings.quotes.len() {
                let b = self.strings.quotes[q].byte;
                assert!(!self.symbol_starts(b), "a quote of a table is also a symbol");
                assert!(!matches!(self.numbers.separator, Some(s) if s == b), "a digit separator of a table is also a quote");
                q += 1;
            }
            let mut c = 0;
            while c < self.comments.line.len() {
                let m = self.comments.line[c].as_bytes();
                assert!(!m.is_empty() && !starts_code(m[0]), "a comment marker starts with a byte no name number or quote starts");
                c += 1;
            }
            if let Some((open, close)) = self.comments.block {
                assert!(!open.is_empty() && !close.is_empty() && !starts_code(open.as_bytes()[0]), "a block comment has markers no name number or quote starts");
            }
            if let Some(i) = self.interp {
                assert!(!i.open.is_empty() && i.open.len() <= 2, "a field opens with one or two bytes");
                assert!(!(matches!(i.literal, Literal::Doubled) && i.open.len() > 1), "a doubled delimiter is one byte");
            }
            let mut p = 0;
            while p < self.strings.prefixes.len() {
                assert!(self.interp.is_some() || self.strings.prefixes[p].1 & FORMAT == 0, "an interpolating prefix needs the table's interpolation");
                p += 1;
            }
            let s = self.shapes;
            assert!(!matches!(s.cond, Cond::Question) || (self.spells(T::Question) && self.spells(T::Colon)), "a table of question conditionals spells their marks");
            assert!(!matches!(s.lambda, Lambda::Pipes) || self.spells(T::BitOr), "a table of pipe closures spells the bar");
            assert!(matches!(s.binding, Binding::None) || self.spells(T::Let), "a table that binds with a word spells it");
            assert!(!s.arms || self.spells(T::Arm), "a table that matches with arms spells their mark");
            assert!(!(s.arms && matches!(self.blocks, Blocks::Words { .. })), "a table of block words takes no arms");
            assert!(!s.ranges || self.spells(T::Range), "a table that makes ranges spells their mark");
        }
    }

    // entry `i` of the words then the symbols
    const fn entry(&self, i: usize) -> (&'static str, TokenType) {
        if i < self.words.len() { self.words[i] } else { self.symbols[i - self.words.len()] }
    }

    // whether some symbol starts with `b`
    const fn symbol_starts(&self, b: u8) -> bool {
        let mut i = 0;
        while i < self.symbols.len() {
            let s = self.symbols[i].0.as_bytes();
            if !s.is_empty() && s[0] == b { return true; }
            i += 1;
        }
        false
    }
}

// the most bytes one token's text or a table's interpolated string name shows in a diagnostic
pub(crate) const TEXT_MAX: usize = 40;

// `t` once it fits the bound every message buffer is sized by
pub(crate) const fn capped(t: &'static [u8]) -> &'static [u8] {
    assert!(t.len() <= TEXT_MAX, "an interpolated string name is at most 40 bytes, the size diagnostics leave it");
    t
}

// parts joined at compile time into a buffer of `N` bytes and its length
pub(crate) const fn join<const N: usize>(parts: &[&[u8]]) -> ([u8; N], usize) {
    let mut out = [0u8; N];
    let (mut n, mut i) = (0, 0);
    while i < parts.len() {
        let p = parts[i];
        let mut j = 0;
        while j < p.len() {
            assert!(n < N, "a joined text fits its buffer");
            out[n] = p[j];
            n += 1;
            j += 1;
        }
        i += 1;
    }
    (out, n)
}

// the text a joined buffer holds
pub(crate) const fn joined<const N: usize>(b: &'static ([u8; N], usize)) -> &'static str {
    match core::str::from_utf8(b.0.split_at(b.1).0) { Ok(s) => s, Err(_) => panic!("a joined text is utf-8") }
}

// every token's quoted spelling or role name packed into one buffer for diagnostics
pub(crate) struct Shown {
    bytes: [u8; 4096],
    spans: [(u16, u16); 128],
}

impl Shown {
    // the text of each token by its role
    pub(crate) const fn texts(&'static self) -> [&'static str; 128] {
        let mut t = [""; 128];
        let mut k = 0;
        while k < 128 {
            let (a, b) = self.spans[k];
            t[k] = match core::str::from_utf8(self.bytes.split_at(b as usize).0.split_at(a as usize).1) { Ok(s) => s, Err(_) => "" };
            k += 1;
        }
        t
    }
}

// whether `a` shares a text with `b` as block and display braces or a conversion
const fn shares(a: TokenType, b: TokenType) -> bool {
    let (a, b) = (a as u8, b as u8);
    (a == T::BraceOpen as u8 && b == T::BlockOpen as u8) || (a == T::BraceClose as u8 && b == T::BlockClose as u8) || a == T::Convert as u8
}

// the token of discriminant `k` among those a diagnostic names by role
const fn kind_at(k: usize) -> Option<TokenType> {
    let mut i = 0;
    while i < KINDS.len() {
        if KINDS[i] as usize == k { return Some(KINDS[i]); }
        i += 1;
    }
    None
}

// the tokens a diagnostic names by role when a table writes no text for them
const KINDS: &[TokenType] = &[T::Name, T::Int, T::Float, T::String, T::Bytes, T::Comment, T::Newline, T::LineBreak, T::Indent, T::Dedent, T::Eof, T::BlockOpen, T::BlockClose];

// what a diagnostic calls a token the table writes no text for
pub(crate) const fn role(kind: TokenType) -> &'static str {
    match kind {
        T::Name => "identifier",
        T::Int => "integer",
        T::Float => "floating point number",
        T::String => "string",
        T::Bytes => "bytes literal",
        T::Comment => "comment",
        T::Newline | T::LineBreak => "newline",
        T::Indent => "indentation",
        T::Dedent => "dedentation",
        T::Eof => "EOF",
        T::BlockOpen => "block start",
        T::BlockClose => "block end",
        _ => UNSPELLED,
    }
}

// what a diagnostic calls a token the table neither spells nor names by role
const UNSPELLED: &str = "a token this table does not spell";

// whether `b` may start a name or number or quote or close a field
const fn starts_code(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'"' | b'\'' | b'`' | b'}' | 0x80..=0xff)
}

const fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] { return false; }
        i += 1;
    }
    true
}

// the most phrases and symbols outside the operator table one table holds
pub(super) const MAX_PHRASES: usize = 8;
pub(super) const MAX_PAIRS: usize = 16;

// a reserved phrase like `else if` split at its first space
#[derive(Clone, Copy)]
pub(super) struct Phrase {
    pub first: &'static [u8],
    pub rest: &'static [u8],
    pub kind: TokenType,
}

// the offset of the first space in `w` or its length when it has none
const fn space(w: &[u8]) -> usize {
    let mut j = 0;
    while j < w.len() && w[j] != b' ' { j += 1; }
    j
}

// a symbol the operator table has no column for which the pair scan matches whole
pub(super) const fn paired(s: &[u8], symbols: &[(&str, TokenType)]) -> bool {
    match s.len() {
        2 => s[1] != b'=' && s[1] != s[0],
        3 => !doubled_eq(s, symbols),
        _ => false,
    }
}

// whether `s` is a doubled byte and `=` whose doubled form the table lists too
const fn doubled_eq(s: &[u8], symbols: &[(&str, TokenType)]) -> bool {
    // `==` fills the `=` column of its byte so `===` takes the pair scan
    if !(s.len() == 3 && s[1] == s[0] && s[2] == b'=' && s[0] != b'=') { return false; }
    let mut i = 0;
    while i < symbols.len() {
        if same(symbols[i].0.as_bytes(), &[s[0], s[1]]) { return true; }
        i += 1;
    }
    false
}

// the byte table of a symbol list leaving `}` to the scanner
pub const fn operators(symbols: &[(&str, TokenType)]) -> [[TokenType; 4]; 128] {
    const N: TokenType = T::Eof;
    let mut t = [[N; 4]; 128];
    let mut i = 0;
    while i < symbols.len() {
        let (s, kind) = symbols[i];
        let s = s.as_bytes();
        let col = match s.len() {
            1 if s[0] != b'}' => 0,
            2 if s[1] == b'=' => 1,
            2 if s[1] == s[0] => 2,
            3 if doubled_eq(s, symbols) => 3,
            _ => 4,
        };
        if col < 4 && s[0] < 128 && t[s[0] as usize][col] as u8 == N as u8 { t[s[0] as usize][col] = kind; }
        i += 1;
    }
    t
}

// a reserved word of two to eight bytes in the packed form a lookup compares
#[derive(Clone, Copy)]
struct Packed {
    bytes: u64,
    kind: TokenType,
    // the next word of the same slot where zero ends the chain
    next: u8,
}

// the reserved words of a table indexed by length and first byte at compile time
pub struct Keywords {
    // the token of each one byte word at that byte
    single: [Option<TokenType>; 256],
    // the first packed word of each length and first byte where zero is none
    slots: [[u8; 64]; 9],
    packed: [Packed; 64],
    // the whole table once a word passes eight bytes and empty otherwise
    long: &'static [(&'static str, TokenType)],
}

// the index any table builds so a name never scans its words
pub const fn index(words: &'static [(&'static str, TokenType)]) -> Keywords {
    let mut k = Keywords { single: [None; 256], slots: [[0; 64]; 9], packed: [Packed { bytes: 0, kind: T::Name, next: 0 }; 64], long: &[] };
    let mut used = 1;
    // from the end so the first listing of a word heads its chain
    let mut i = words.len();
    while i > 0 {
        i -= 1;
        let (text, kind) = words[i];
        let s = text.as_bytes();
        // a phrase never matches a name since no name holds a space
        if s.is_empty() || space(s) < s.len() { continue; }
        if s.len() == 1 {
            k.single[s[0] as usize] = Some(kind);
        } else if s.len() > 8 {
            k.long = words;
        } else {
            assert!(used < 64, "an index holds at most 63 reserved words of two to eight bytes");
            let slot = &mut k.slots[s.len()][s[0] as usize & 63];
            k.packed[used] = Packed { bytes: pack(s), kind, next: *slot };
            *slot = used as u8;
            used += 1;
        }
    }
    k
}

// the first and last two bytes of short words or four of longer ones
const fn pack(s: &[u8]) -> u64 {
    let n = s.len();
    let w = if n < 4 { 2 } else { 4 };
    let (mut head, mut tail, mut i) = (0u64, 0u64, 0);
    while i < w {
        head |= (s[i] as u64) << (8 * i);
        tail |= (s[n - w + i] as u64) << (8 * i);
        i += 1;
    }
    head | tail << (8 * w)
}

impl Keywords {
    // the token a name reserves from one slot and at most a few packed compares
    internal! {
        #[inline(always)]
        pub fn find(&self, s: &[u8]) -> Option<TokenType> {
            let n = s.len();
            // the head and tail overlap so together they hold every byte
            match n {
                0 => None,
                1 => self.single[s[0] as usize],
                2 | 3 => {
                    let at = self.slots[n][s[0] as usize & 63];
                    if at == 0 { return None; }
                    self.chain(at, half(&s[..2]) as u64 | (half(&s[n - 2..]) as u64) << 16)
                }
                4..=8 => {
                    let at = self.slots[n][s[0] as usize & 63];
                    if at == 0 { return None; }
                    self.chain(at, word(&s[..4]) as u64 | (word(&s[n - 4..]) as u64) << 32)
                }
                _ => self.long.iter().find(|(w, _)| w.as_bytes() == s).map(|&(_, k)| k),
            }
        }
    }

    // the chained word matching the packed `bytes` from a slot that holds one
    #[inline(always)]
    fn chain(&self, mut at: u8, bytes: u64) -> Option<TokenType> {
        loop {
            let w = &self.packed[at as usize & 63];
            if w.bytes == bytes { return Some(w.kind); }
            at = w.next;
            if at == 0 { return None; }
        }
    }
}

// two bytes read as one little endian load
#[inline(always)]
fn half(s: &[u8]) -> u16 {
    let mut b = [0; 2];
    b.copy_from_slice(&s[..2]);
    u16::from_le_bytes(b)
}

// four bytes read as one little endian load
#[inline(always)]
fn word(s: &[u8]) -> u32 {
    let mut b = [0; 4];
    b.copy_from_slice(&s[..4]);
    u32::from_le_bytes(b)
}

// a table as a type so it compiles in as constants
pub trait Spelling {
    const SYNTAX: Syntax;
    // the natives whose names the parser fuses and whose runtime's words it writes
    const NATIVES: &'static crate::engine::lang::Natives = &crate::engine::lang::Natives::EMPTY;
    // the runtime's own opcodes whose fused natives and folds the parser applies
    #[cfg(feature = "unstable-internals")]
    const OPS: &'static crate::engine::lang::OpSet = &crate::engine::lang::OpSet::NONE;
    // whether the parse keeps spans and an outline which only a read through `Recorded` asks
    const RECORD: bool = false;
}

/* the own opcodes table `S` parses with which only the internals build may set */
#[cfg(feature = "unstable-internals")]
pub(crate) const fn own_ops<S: Spelling>() -> &'static crate::engine::lang::OpSet { S::OPS }

#[cfg(not(feature = "unstable-internals"))]
#[expect(clippy::extra_unused_type_parameters, reason = "the internals build reads the table off `S`")]
pub(crate) const fn own_ops<S: Spelling>() -> &'static crate::engine::lang::OpSet { &crate::engine::lang::OpSet::NONE }
