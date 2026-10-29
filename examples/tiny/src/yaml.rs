use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

/* a YAML node with the bytes of the source it was read from */
#[derive(Debug, Clone)]
pub struct Node {
    pub span: Range<usize>,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum Value {
    // a scalar's text with plain lines folded and quotes decoded
    Scalar { text: String, style: Style },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

/* how a scalar was written */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style { Plain, Single, Double }

/* why the source is no YAML tiny reads */
#[derive(Debug, Clone)]
pub struct Error {
    pub span: Range<usize>,
    pub msg: String,
}

impl Node {
    /* the text of a scalar and none for a collection */
    pub fn text(&self) -> Option<&str> {
        match &self.value { Value::Scalar { text, .. } => Some(text), _ => None }
    }

    /* whether the node is a plain scalar which a flow reads as an expression */
    pub fn plain(&self) -> bool { matches!(self.value, Value::Scalar { style: Style::Plain, .. }) }

    /* whether the node holds nothing as an empty value does */
    pub fn empty(&self) -> bool { matches!(&self.value, Value::Scalar { text, style: Style::Plain } if text.is_empty()) }
}

// collections nested past this many levels are refused before the reader's stack runs out
const DEPTH: usize = 64;

/* every document of `src` with each mapping refusing a key it already holds */
pub fn documents(src: &str) -> Result<Vec<Node>, Error> {
    let mut r = Reader { src, b: src.as_bytes(), at: 0, depth: 0 };
    if src.starts_with('\u{feff}') { r.at = 3; }
    let mut docs = Vec::new();
    loop {
        r.blank()?;
        if r.at >= r.b.len() { break; }
        let marked = r.marker(b"---");
        if marked {
            r.at += 3;
            r.rest_of_line()?;
            r.blank()?;
        }
        if r.at >= r.b.len() || r.marker(b"---") || r.marker(b"...") {
            if !marked { return Err(r.error(r.at..r.at, "a document marker needs a document before it")); }
            docs.push(Node { span: r.at..r.at, value: Value::Scalar { text: String::new(), style: Style::Plain } });
        } else {
            let col = r.col(r.at);
            let node = r.block(col, -1)?;
            docs.push(node);
            r.blank()?;
        }
        if r.marker(b"...") {
            r.at += 3;
            r.rest_of_line()?;
        } else if r.at < r.b.len() && !r.marker(b"---") {
            return Err(r.error(r.at..r.line_end(r.at), "this line is outside the document above it, check its indentation"));
        }
    }
    Ok(docs)
}

struct Reader<'s> {
    src: &'s str,
    b: &'s [u8],
    at: usize,
    // how many collections hold the node being read
    depth: usize,
}

impl Reader<'_> {
    fn error(&self, span: Range<usize>, msg: &str) -> Error { Error { span, msg: msg.into() } }

    fn peek(&self) -> Option<u8> { self.b.get(self.at).copied() }

    fn byte(&self, at: usize) -> Option<u8> { self.b.get(at).copied() }

    fn line_start(&self, at: usize) -> usize { self.b[..at.min(self.b.len())].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1) }

    fn line_end(&self, at: usize) -> usize {
        let end = self.b[at.min(self.b.len())..].iter().position(|&c| c == b'\n').map_or(self.b.len(), |i| at + i);
        if end > at && self.b[end - 1] == b'\r' { end - 1 } else { end }
    }

    fn col(&self, at: usize) -> isize { (at - self.line_start(at)) as isize }

    /* whether a document marker of three bytes starts the line at the current byte */
    fn marker(&self, m: &[u8]) -> bool {
        self.col(self.at) == 0 && self.b[self.at..].starts_with(m) && matches!(self.byte(self.at + 3), None | Some(b' ' | b'\t' | b'\n' | b'\r'))
    }

    /* whether the byte at `at` is a space a line end or the end */
    fn gap(&self, at: usize) -> bool { matches!(self.byte(at), None | Some(b' ' | b'\t' | b'\n' | b'\r')) }

    /* whether anything but spaces and a comment follows on the line */
    fn ahead_on_line(&self) -> bool {
        let mut at = self.at;
        while matches!(self.byte(at), Some(b' ' | b'\t')) { at += 1; }
        !matches!(self.byte(at), None | Some(b'\n' | b'\r' | b'#'))
    }

    /* skips spaces then a comment and needs the line to end there */
    fn rest_of_line(&mut self) -> Result<(), Error> {
        while matches!(self.peek(), Some(b' ' | b'\t')) { self.at += 1; }
        if self.peek() == Some(b'#') { self.at = self.line_end(self.at); }
        match self.peek() {
            None => Ok(()),
            Some(b'\r') if self.byte(self.at + 1) == Some(b'\n') => { self.at += 2; Ok(()) }
            Some(b'\n') => { self.at += 1; Ok(()) }
            _ => Err(self.error(self.at..self.line_end(self.at), "this line goes on past where its value ends")),
        }
    }

    /* moves to the next content of the source refusing tabs in indentation */
    fn blank(&mut self) -> Result<(), Error> {
        loop {
            let mut p = self.at;
            while matches!(self.byte(p), Some(b' ' | b'\t')) { p += 1; }
            match self.byte(p) {
                None => { self.at = p; return Ok(()); }
                Some(b'#' | b'\n' | b'\r') => {
                    p = self.line_end(p);
                    if self.byte(p) == Some(b'\r') { p += 1; }
                    if self.byte(p) == Some(b'\n') { p += 1; }
                    self.at = p;
                }
                _ => {
                    if let Some(t) = self.b[self.at..p].iter().position(|&c| c == b'\t') {
                        return Err(self.error(self.at + t..self.at + t + 1, "indentation takes spaces, not tabs"));
                    }
                    self.at = p;
                    return Ok(());
                }
            }
        }
    }

    /* one more level of nesting refused past the depth a flow may reach */
    fn nest(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > DEPTH { return Err(self.error(self.at..self.at + 1, "this value nests past 64 levels")); }
        Ok(())
    }

    /* a block node starting here in column `col` inside a parent at `parent` */
    fn block(&mut self, col: isize, parent: isize) -> Result<Node, Error> {
        self.nest()?;
        let node = if self.peek() == Some(b'-') && self.gap(self.at + 1) { self.sequence(col) }
            else if self.key_ahead()? { self.mapping(col) }
            else { self.inline(parent) };
        self.depth -= 1;
        node
    }

    /* whether a key and a colon start at the current byte */
    fn key_ahead(&self) -> Result<bool, Error> {
        let mut at = self.at;
        match self.peek() {
            Some(q @ (b'"' | b'\'')) => {
                at += 1;
                loop {
                    match self.byte(at) {
                        None | Some(b'\n' | b'\r') => return Ok(false),
                        Some(b'\\') if q == b'"' => at += 2,
                        Some(b'\'') if q == b'\'' && self.byte(at + 1) == Some(b'\'') => at += 2,
                        Some(c) if c == q => { at += 1; break; }
                        _ => at += 1,
                    }
                }
                while matches!(self.byte(at), Some(b' ' | b'\t')) { at += 1; }
                Ok(self.byte(at) == Some(b':') && self.gap(at + 1))
            }
            Some(b'[' | b'{') => Ok(false),
            _ => {
                while let Some(c) = self.byte(at) {
                    match c {
                        b'\n' | b'\r' => return Ok(false),
                        b'#' if at > self.at && matches!(self.byte(at - 1), Some(b' ' | b'\t')) => return Ok(false),
                        b':' if self.gap(at + 1) => return Ok(true),
                        _ => at += 1,
                    }
                }
                Ok(false)
            }
        }
    }

    /* the entries of a block mapping whose keys sit in column `col` */
    fn mapping(&mut self, col: isize) -> Result<Node, Error> {
        let start = self.at;
        let mut entries: Vec<(Node, Node)> = Vec::new();
        // the keys so far so a repeat is found without a scan of every entry
        let mut keys = BTreeSet::new();
        loop {
            let key = self.key()?;
            if !keys.insert(String::from(key.text().unwrap_or(""))) {
                let msg = alloc::format!("the key \"{}\" is repeated, a mapping holds each key once", key.text().unwrap_or(""));
                return Err(Error { span: key.span.clone(), msg });
            }
            self.at += 1;
            let value = self.value(col, key.span.end + 1)?;
            entries.push((key, value));
            self.blank()?;
            if self.at >= self.b.len() || self.marker(b"---") || self.marker(b"...") { break; }
            let c = self.col(self.at);
            if c < col { break; }
            if c > col { return Err(self.error(self.at..self.line_end(self.at), "this line is indented deeper than the keys around it")); }
            if !self.key_ahead()? {
                let msg = if self.peek() == Some(b'-') { "a list item sits beside the keys of a mapping, indent it under a key" } else { "a line of a mapping holds no key and colon" };
                return Err(self.error(self.at..self.line_end(self.at), msg));
            }
        }
        let end = entries.last().map_or(start, |(_, v)| v.span.end.max(v.span.start));
        Ok(Node { span: start..end, value: Value::Map(entries) })
    }

    /* the scalar key at the current byte leaving the colon after it */
    fn key(&mut self) -> Result<Node, Error> {
        let key = match self.peek() {
            Some(b'"' | b'\'') => self.quoted()?,
            _ => {
                let start = self.at;
                while !(self.peek() == Some(b':') && self.gap(self.at + 1)) { self.at += 1; }
                let mut end = self.at;
                while end > start && matches!(self.b[end - 1], b' ' | b'\t') { end -= 1; }
                self.check_plain(start..end)?;
                Node { span: start..end, value: Value::Scalar { text: self.src[start..end].into(), style: Style::Plain } }
            }
        };
        while matches!(self.peek(), Some(b' ' | b'\t')) { self.at += 1; }
        Ok(key)
    }

    /* the value after a key or dash in column `col` placed at `empty` when absent */
    fn value(&mut self, col: isize, empty: usize) -> Result<Node, Error> {
        while matches!(self.peek(), Some(b' ' | b'\t')) { self.at += 1; }
        if matches!(self.peek(), None | Some(b'\n' | b'\r' | b'#')) {
            self.rest_of_line()?;
            let line = self.at;
            self.blank()?;
            let c = self.col(self.at);
            let nested = self.at < self.b.len() && !self.marker(b"---") && !self.marker(b"...");
            if nested && c > col { return self.block(c, col); }
            if nested && c == col && self.peek() == Some(b'-') && self.gap(self.at + 1) { return self.sequence(c); }
            self.at = line;
            return Ok(Node { span: empty..empty, value: Value::Scalar { text: String::new(), style: Style::Plain } });
        }
        if self.peek() == Some(b'-') && self.gap(self.at + 1) {
            return Err(self.error(self.at..self.line_end(self.at), "a list starts on the line after its key"));
        }
        if self.key_ahead()? {
            return Err(self.error(self.at..self.line_end(self.at), "a plain value holds \": \", quote it or drop the space"));
        }
        let node = self.inline(col)?;
        let quoted = matches!(node.value, Value::Scalar { style: Style::Single | Style::Double, .. });
        if quoted && self.ahead_on_line() {
            return Err(self.error(node.span.end..self.line_end(self.at), "a quoted value is text that ends at its quote, an expression starts with no quote"));
        }
        self.rest_of_line()?;
        Ok(node)
    }

    /* the items of a block sequence whose dashes sit in column `col` */
    fn sequence(&mut self, col: isize) -> Result<Node, Error> {
        let start = self.at;
        let mut items = Vec::new();
        loop {
            self.at += 1;
            while matches!(self.peek(), Some(b' ')) { self.at += 1; }
            let item = if matches!(self.peek(), None | Some(b'\n' | b'\r' | b'#')) {
                self.value(col, self.at)?
            } else {
                let c = self.col(self.at);
                let flow = matches!(self.peek(), Some(b'[' | b'{' | b'"' | b'\''));
                let node = self.block(c, col)?;
                if flow || !matches!(node.value, Value::Map(_) | Value::Seq(_)) { self.rest_of_line()?; }
                node
            };
            items.push(item);
            self.blank()?;
            if self.at >= self.b.len() || self.marker(b"---") || self.marker(b"...") { break; }
            let c = self.col(self.at);
            if c < col { break; }
            if c > col || !(self.peek() == Some(b'-') && self.gap(self.at + 1)) {
                if c == col { break; }
                return Err(self.error(self.at..self.line_end(self.at), "this line is indented deeper than the list items around it"));
            }
        }
        let end = items.last().map_or(start, |n: &Node| n.span.end);
        Ok(Node { span: start..end, value: Value::Seq(items) })
    }

    /* a flow collection or a scalar starting here in a block at `parent` */
    fn inline(&mut self, parent: isize) -> Result<Node, Error> {
        match self.peek() {
            Some(b'[' | b'{') => self.flow(),
            Some(b'"' | b'\'') => self.quoted(),
            Some(b'|' | b'>') => Err(self.error(self.at..self.at + 1, "tiny reads no block scalars, write the text quoted")),
            Some(b'&') => Err(self.error(self.at..self.at + 1, "tiny reads no anchors, write the value out")),
            Some(b'*') => Err(self.error(self.at..self.at + 1, "tiny reads no aliases, write the value out")),
            Some(b'!') => Err(self.error(self.at..self.at + 1, "tiny reads no tags, write the value plain")),
            Some(b'%' | b'@' | b'`') => Err(self.error(self.at..self.at + 1, "a plain value cannot start with this character, quote it")),
            _ => self.plain(parent),
        }
    }

    /* refuses the pairs a plain scalar may not hold */
    fn check_plain(&self, span: Range<usize>) -> Result<(), Error> {
        let text = &self.b[span.clone()];
        for (i, w) in text.windows(2).enumerate() {
            if w == b": " { return Err(self.error(span.start + i..span.start + i + 2, "a plain value holds \": \", quote it or drop the space")); }
        }
        Ok(())
    }

    /* a plain scalar in a block whose lines deeper than `parent` fold into one */
    fn plain(&mut self, parent: isize) -> Result<Node, Error> {
        let start = self.at;
        let mut text = String::new();
        let mut end;
        let mut breaks = 0usize;
        loop {
            let line_start = self.at;
            while let Some(c) = self.peek() {
                if c == b'\n' || c == b'\r' { break; }
                if c == b'#' && self.at > line_start && matches!(self.byte(self.at - 1), Some(b' ' | b'\t')) { break; }
                if c == b':' && self.gap(self.at + 1) { return Err(self.error(self.at..self.at + 1, "a plain value holds \": \", quote it or drop the space")); }
                self.at += 1;
            }
            let mut stop = self.at;
            while stop > line_start && matches!(self.b[stop - 1], b' ' | b'\t') { stop -= 1; }
            end = stop;
            if !text.is_empty() { if breaks > 1 { for _ in 1..breaks { text.push('\n'); } } else { text.push(' '); } }
            text.push_str(&self.src[line_start..stop]);
            // a line deeper than the parent that is no comment goes on folding
            let mut probe = self.at;
            breaks = 0;
            loop {
                if self.byte(probe) == Some(b'#') { break; }
                if self.byte(probe) == Some(b'\r') { probe += 1; }
                if self.byte(probe) != Some(b'\n') { break; }
                probe += 1;
                breaks += 1;
                let mut c = probe;
                while self.byte(c) == Some(b' ') { c += 1; }
                if matches!(self.byte(c), Some(b'\n' | b'\r')) { probe = c; continue; }
                let deep = (c - probe) as isize > parent && !matches!(self.byte(c), None | Some(b'#'));
                let marked = c == probe && (self.b[c..].starts_with(b"---") || self.b[c..].starts_with(b"..."));
                if !deep || marked { breaks = 0; break; }
                probe = c;
                break;
            }
            if breaks == 0 { self.at = end.max(self.at); break; }
            self.at = probe;
        }
        // the cursor stays after the last byte so the caller checks the line's rest
        self.at = self.at.max(end);
        let span = start..end;
        Ok(Node { span, value: Value::Scalar { text, style: Style::Plain } })
    }

    /* a quoted scalar with its escapes decoded and its line breaks folded */
    fn quoted(&mut self) -> Result<Node, Error> {
        let start = self.at;
        let q = self.b[self.at];
        self.at += 1;
        let mut text = String::new();
        loop {
            let Some(c) = self.peek() else { return Err(self.error(start..self.at, "a quoted value never closes")) };
            match c {
                b'\'' if q == b'\'' && self.byte(self.at + 1) == Some(b'\'') => { text.push('\''); self.at += 2; }
                c if c == q => { self.at += 1; break; }
                b'\\' if q == b'"' => self.escape(&mut text)?,
                b'\r' | b'\n' => {
                    while matches!(text.as_bytes().last(), Some(b' ' | b'\t')) { text.pop(); }
                    let mut breaks = 0;
                    while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                        if self.peek() == Some(b'\n') { breaks += 1; }
                        self.at += 1;
                    }
                    if breaks > 1 { for _ in 1..breaks { text.push('\n'); } } else { text.push(' '); }
                }
                _ => {
                    let ch = self.src[self.at..].chars().next().unwrap_or(' ');
                    text.push(ch);
                    self.at += ch.len_utf8();
                }
            }
        }
        let style = if q == b'"' { Style::Double } else { Style::Single };
        Ok(Node { span: start..self.at, value: Value::Scalar { text, style } })
    }

    /* one escape of a double quoted scalar onto `text` */
    fn escape(&mut self, text: &mut String) -> Result<(), Error> {
        let at = self.at;
        self.at += 1;
        let Some(c) = self.peek() else { return Err(self.error(at..self.at, "a quoted value never closes")) };
        self.at += 1;
        let ch = match c {
            b'0' => '\0', b'a' => '\u{7}', b'b' => '\u{8}', b't' | b'\t' => '\t', b'n' => '\n', b'v' => '\u{b}', b'f' => '\u{c}',
            b'r' => '\r', b'e' => '\u{1b}', b' ' => ' ', b'"' => '"', b'/' => '/', b'\\' => '\\', b'N' => '\u{85}', b'_' => '\u{a0}',
            b'L' => '\u{2028}', b'P' => '\u{2029}',
            b'x' | b'u' | b'U' => {
                let n = match c { b'x' => 2, b'u' => 4, _ => 8 };
                let digits = self.src.get(self.at..self.at + n).filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()));
                let code = digits.and_then(|d| u32::from_str_radix(d, 16).ok()).and_then(char::from_u32);
                let Some(ch) = code else { return Err(self.error(at..(self.at + n).min(self.b.len()), "an escape names no character")) };
                self.at += n;
                ch
            }
            b'\r' | b'\n' => {
                if c == b'\r' && self.peek() == Some(b'\n') { self.at += 1; }
                while matches!(self.peek(), Some(b' ' | b'\t')) { self.at += 1; }
                return Ok(());
            }
            _ => return Err(self.error(at..self.at, "an escape YAML does not know")),
        };
        text.push(ch);
        Ok(())
    }

    /* skips spaces line ends and comments inside a flow collection */
    fn flow_gap(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.at += 1,
                Some(b'#') if self.at == 0 || matches!(self.byte(self.at - 1), Some(b' ' | b'\t' | b'\n' | b'\r')) => self.at = self.line_end(self.at),
                _ => return,
            }
        }
    }

    /* a flow sequence or mapping with what it nests */
    fn flow(&mut self) -> Result<Node, Error> {
        self.nest()?;
        let node = self.flow_in();
        self.depth -= 1;
        node
    }

    /* the collection a flow bracket opens */
    fn flow_in(&mut self) -> Result<Node, Error> {
        let start = self.at;
        let open = self.b[self.at];
        let close = if open == b'[' { b']' } else { b'}' };
        self.at += 1;
        let mut items = Vec::new();
        let mut pairs: Vec<(Node, Node)> = Vec::new();
        let mut keys = BTreeSet::new();
        loop {
            self.flow_gap();
            match self.peek() {
                None => return Err(self.error(start..start + 1, "this bracket never closes")),
                Some(c) if c == close => { self.at += 1; break; }
                Some(b',') => return Err(self.error(self.at..self.at + 1, "a comma with no value before it")),
                _ => {}
            }
            let node = self.flow_node()?;
            self.flow_gap();
            if open == b'{' {
                let value = if self.peek() == Some(b':') {
                    self.at += 1;
                    self.flow_gap();
                    if matches!(self.peek(), Some(b',' | b'}')) { Node { span: self.at..self.at, value: Value::Scalar { text: String::new(), style: Style::Plain } } } else { self.flow_node()? }
                } else {
                    Node { span: node.span.end..node.span.end, value: Value::Scalar { text: String::new(), style: Style::Plain } }
                };
                if node.text().is_none() { return Err(self.error(node.span.clone(), "a key is a plain or quoted value")); }
                if !keys.insert(String::from(node.text().unwrap_or(""))) {
                    let msg = alloc::format!("the key \"{}\" is repeated, a mapping holds each key once", node.text().unwrap_or(""));
                    return Err(Error { span: node.span.clone(), msg });
                }
                pairs.push((node, value));
            } else {
                if self.peek() == Some(b':') { return Err(self.error(self.at..self.at + 1, "a pair inside a list goes in braces")); }
                items.push(node);
            }
            self.flow_gap();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(c) if c == close => {}
                None => return Err(self.error(start..start + 1, "this bracket never closes")),
                _ => return Err(self.error(self.at..self.at + 1, "expected a comma or the closing bracket")),
            }
        }
        let value = if open == b'[' { Value::Seq(items) } else { Value::Map(pairs) };
        Ok(Node { span: start..self.at, value })
    }

    /* a node inside a flow collection */
    fn flow_node(&mut self) -> Result<Node, Error> {
        match self.peek() {
            Some(b'[' | b'{') => self.flow(),
            Some(b'"' | b'\'') => self.quoted(),
            Some(b'&' | b'*' | b'!' | b'|' | b'>' | b'%' | b'@' | b'`') => self.inline(-1),
            _ => {
                let start = self.at;
                let mut text = String::new();
                let mut end = start;
                while let Some(c) = self.peek() {
                    let stop = match c {
                        b',' | b'[' | b']' | b'{' | b'}' => true,
                        b':' => matches!(self.byte(self.at + 1), None | Some(b' ' | b'\t' | b'\n' | b'\r' | b',' | b'[' | b']' | b'{' | b'}')),
                        b'#' => self.at > start && matches!(self.byte(self.at - 1), Some(b' ' | b'\t')),
                        _ => false,
                    };
                    if stop { break; }
                    if c == b'\n' || c == b'\r' {
                        let mut p = self.at;
                        while matches!(self.byte(p), Some(b' ' | b'\t' | b'\n' | b'\r')) { p += 1; }
                        if matches!(self.byte(p), None | Some(b',' | b']' | b'}' | b':' | b'#')) { break; }
                        text.push(' ');
                        self.at = p;
                        continue;
                    }
                    let ch = self.src[self.at..].chars().next().unwrap_or(' ');
                    if ch != ' ' && ch != '\t' {
                        // the spaces between words count once a word follows them
                        let gap = &self.src[end..self.at];
                        if !text.is_empty() && !gap.is_empty() && !text.ends_with(' ') { text.push_str(gap); }
                        text.push(ch);
                        end = self.at + ch.len_utf8();
                    }
                    self.at += ch.len_utf8();
                }
                if end == start { return Err(self.error(start..start + 1, "expected a value")); }
                self.at = end.max(self.at);
                Ok(Node { span: start..end, value: Value::Scalar { text, style: Style::Plain } })
            }
        }
    }
}
