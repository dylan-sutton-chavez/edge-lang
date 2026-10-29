use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ops::Range;

use crate::yaml::{Node, Style, Value};

/* a problem with a flow at the bytes of the source it names */
#[derive(Debug, Clone)]
pub struct Diag {
    pub span: Range<usize>,
    pub msg: String,
}

/* a flow as its source then the filter every report reads through and its reports */
pub struct Program {
    pub from: From,
    pub filter: Option<Clause>,
    pub reports: Vec<Report>,
}

/* where a flow reads its records and how they are written */
pub struct From {
    // the entry naming the source which its errors point at
    pub span: Range<usize>,
    pub source: Source,
    pub format: Format,
}

pub enum Source {
    // a url read through the net plugin and whether its body is inflated
    Net { url: String, inflate: bool },
    // the lines the host feeds the run
    Input,
    // a file of the package read through the files module and whether it is inflated
    Path { path: String, inflate: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    JsonLines,
    // an array of records or JSON lines as the first byte of the content tells
    Json,
    Csv,
    Lines,
}

/* an entry holding an expression with the bytes of the entry and of its value */
#[derive(Clone, Debug)]
pub struct Clause {
    pub span: Range<usize>,
    pub expr: Range<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agg { Count, Sum, Average, Min, Max }

impl Agg {
    const ALL: [Agg; 5] = [Agg::Count, Agg::Sum, Agg::Average, Agg::Min, Agg::Max];

    /* the key naming the aggregate which also names the column it fills */
    pub const fn word(self) -> &'static str {
        match self { Agg::Count => "count", Agg::Sum => "sum", Agg::Average => "average", Agg::Min => "min", Agg::Max => "max" }
    }
}

/* a key rows group by read at a path or computed under a name */
#[derive(Clone, Debug)]
pub enum Key {
    Path { span: Range<usize>, parts: Vec<String> },
    Named { span: Range<usize>, name: String, expr: Range<usize> },
}

impl Key {
    /* the column the key fills as the parts of its path */
    pub fn column(&self) -> Vec<&str> {
        match self {
            Key::Path { parts, .. } => parts.iter().map(String::as_str).collect(),
            Key::Named { name, .. } => alloc::vec![name.as_str()],
        }
    }

    pub fn span(&self) -> Range<usize> { match self { Key::Path { span, .. } | Key::Named { span, .. } => span.clone() } }
}

/* a report's aggregate with what it measures and the keys of its rows */
pub struct Fold {
    pub agg: Agg,
    // the aggregate's entry
    pub span: Range<usize>,
    pub measure: Option<Range<usize>>,
    pub keys: Vec<Key>,
    // the entry of by when the keys come from it
    pub by: Option<Range<usize>>,
}

/* where a report writes */
#[derive(Clone, Debug)]
pub enum Sink {
    Out(String),
    Csv(String),
    Jsonl(String),
}

/* a report named by its output with each key it holds */
pub struct Report {
    pub name: String,
    pub span: Range<usize>,
    // the report's own key which its writes point at
    pub key: Range<usize>,
    pub sink: Sink,
    pub filter: Option<Clause>,
    pub fold: Option<Fold>,
    pub keep: Option<Clause>,
    pub top: Option<(Range<usize>, u64)>,
    pub sort: Option<Sort>,
}

/* a sort's entry with each key and whether that key sorts descending */
pub struct Sort {
    pub span: Range<usize>,
    pub keys: Vec<(Range<usize>, bool)>,
}

impl Report {
    /* the bytes of each entry the report holds with the key that names it */
    pub fn entries(&self) -> Vec<(Range<usize>, &'static str)> {
        let mut out = Vec::new();
        if let Some(w) = &self.filter { out.push((w.span.clone(), "where")); }
        if let Some(f) = &self.fold {
            out.push((f.span.clone(), f.agg.word()));
            if let Some(by) = &f.by { out.push((by.clone(), "by")); }
        }
        if let Some(k) = &self.keep { out.push((k.span.clone(), "keep")); }
        if let Some((s, _)) = &self.top { out.push((s.clone(), "top")); }
        if let Some(s) = &self.sort { out.push((s.span.clone(), "sort")); }
        out
    }
}

// the keys a report takes
const KEYS: [&str; 10] = ["where", "count", "sum", "average", "min", "max", "by", "keep", "top", "sort"];

// words an expression reads as itself which no field or key may take
const WORDS: [&str; 7] = ["and", "or", "not", "in", "True", "False", "None"];

struct Checker<'s> {
    src: &'s str,
    diags: Vec<Diag>,
}

/* the flow of the documents read from `src` or every problem found in it */
pub fn check(src: &str, docs: &[Node]) -> Result<Program, Vec<Diag>> {
    let mut c = Checker { src, diags: Vec::new() };
    let prog = match docs {
        [] => { c.diag(0..0, "a flow file holds a mapping of from and its reports"); None }
        [doc] => c.program(doc),
        [_, second, ..] => { c.diag(second.span.clone(), "a flow file holds one mapping, so it takes no second document"); None }
    };
    match prog {
        Some(p) if c.diags.is_empty() => Ok(p),
        _ => Err(c.diags),
    }
}

/* whether `s` is a name an expression may read as the lexer reads one */
pub fn is_name(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()) && cs.all(name_char)
}

/* whether `c` may go on a name where any character past ascii is a letter */
fn name_char(c: char) -> bool { c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii() }

/* the bytes of an entry from its key to the end of its value */
fn entry(k: &Node, v: &Node) -> Range<usize> { k.span.start..v.span.end.max(k.span.end) }

impl Checker<'_> {
    fn diag(&mut self, span: Range<usize>, msg: impl Into<String>) { self.diags.push(Diag { span, msg: msg.into() }); }

    fn program(&mut self, doc: &Node) -> Option<Program> {
        let Value::Map(entries) = &doc.value else {
            self.diag(doc.span.clone(), "a flow is a mapping of from, where and its reports");
            return None;
        };
        let (mut from, mut filter, mut reports, mut named) = (None, None, Vec::new(), false);
        for (k, v) in entries {
            match k.text().unwrap_or("") {
                "from" => { named = true; from = self.from(k, v); }
                "where" => filter = self.clause(k, v),
                _ => if let Some(r) = self.report(k, v) { reports.push(r); },
            }
        }
        if !named { self.diag(doc.span.start..doc.span.start, "a flow names its source with from"); }
        if entries.iter().all(|(k, _)| matches!(k.text(), Some("from" | "where"))) {
            self.diag(doc.span.start..doc.span.start, "a flow writes at least one report, such as a key total with count under it");
        }
        Some(Program { from: from?, filter, reports })
    }

    fn from(&mut self, k: &Node, v: &Node) -> Option<From> {
        let span = entry(k, v);
        let text = match &v.value {
            Value::Scalar { text, .. } if !text.is_empty() => text.as_str(),
            _ => { self.diag(v.span_or(k), "from takes one line naming a url, a path or input"); return None; }
        };
        if text == "input" { return Some(From { span, source: Source::Input, format: Format::Json }); }
        let net = text.starts_with("https://") || text.starts_with("http://");
        if !net && text.contains("://") {
            self.diag(v.span.clone(), "from reads a url through http:// or https://");
            return None;
        }
        // a url's query and fragment follow the path its extension ends
        let path = if net { text.split(['?', '#']).next().unwrap_or(text) } else { text };
        let (inflate, path) = match path.strip_suffix(".gz") { Some(p) => (true, p), None => (false, path) };
        let format = match path.rsplit_once('.').map(|(_, ext)| ext) {
            Some("jsonl") => Format::JsonLines,
            Some("json") => Format::Json,
            Some("csv") => Format::Csv,
            Some("txt") => Format::Lines,
            _ => {
                self.diag(v.span.clone(), "from names a source whose extension gives its format, .jsonl, .json, .csv or .txt, with .gz after it when gzipped");
                return None;
            }
        };
        let source = if net { Source::Net { url: text.to_string(), inflate } } else { Source::Path { path: text.to_string(), inflate } };
        Some(From { span, source, format })
    }

    /* an entry whose value is an expression */
    fn clause(&mut self, k: &Node, v: &Node) -> Option<Clause> {
        let expr = self.expr(k, v, k.text().unwrap_or(""))?;
        Some(Clause { span: entry(k, v), expr })
    }

    /* the bytes of a value read as an expression */
    fn expr(&mut self, k: &Node, v: &Node, what: &str) -> Option<Range<usize>> {
        match &v.value {
            Value::Scalar { text, style: Style::Plain } if text.is_empty() => { self.diag(k.span.clone(), format!("{what} takes an expression")); None }
            Value::Scalar { style: Style::Plain, .. } => Some(v.span.clone()),
            Value::Scalar { .. } => { self.diag(v.span.clone(), format!("{what} takes an expression written with no quotes around it")); None }
            _ => { self.diag(v.span.clone(), format!("{what} takes an expression, not a list or a mapping")); None }
        }
    }

    fn report(&mut self, k: &Node, v: &Node) -> Option<Report> {
        let name = k.text().unwrap_or("").to_string();
        if name.is_empty() || name.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\')) {
            self.diag(k.span.clone(), format!("the report \"{name}\" takes a name with no spaces, since its name is its output"));
            return None;
        }
        let entries: &[(Node, Node)] = match &v.value {
            Value::Map(entries) => entries,
            _ if v.empty() => &[],
            _ => {
                let hint = hint(&name, ["from", "where"].into_iter());
                self.diag(k.span.clone(), format!("report {name} takes a mapping of keys such as where and count{hint}"));
                return None;
            }
        };
        let before = self.diags.len();
        for (ek, _) in entries {
            let key = ek.text().unwrap_or("");
            if !KEYS.contains(&key) {
                let hint = hint(key, KEYS.into_iter());
                self.diag(ek.span.clone(), format!("report {name} has no key \"{key}\"{hint}"));
            }
        }
        let found = |key: &str| entries.iter().find(|(ek, _)| ek.text() == Some(key));
        let aggs: Vec<(&Node, &Node, Agg)> = entries.iter()
            .filter_map(|(ek, ev)| Agg::ALL.into_iter().find(|a| ek.text() == Some(a.word())).map(|a| (ek, ev, a)))
            .collect();
        if let [(_, _, a), (second, _, b), ..] = aggs.as_slice() {
            self.diag(second.span.clone(), format!("report {name} takes one aggregate, not both {} and {}", a.word(), b.word()));
        }
        let filter = found("where").and_then(|(ek, ev)| self.clause(ek, ev));
        let keep = found("keep").and_then(|(ek, ev)| self.clause(ek, ev));
        let top = found("top").and_then(|(ek, ev)| self.top(ek, ev));
        let sort = found("sort").and_then(|(ek, ev)| self.sort(ek, ev));
        let fold = aggs.first().and_then(|&(ak, av, agg)| self.fold(agg, ak, av, found("by")));
        if aggs.is_empty() {
            if let Some((bk, _)) = found("by") { self.diag(bk.span.clone(), "by gives the keys of sum, average, min or max, so the report takes one of them"); }
            if let Some((kk, _)) = found("keep") { self.diag(kk.span.clone(), "keep filters the rows an aggregate makes, so a report of records filters with where"); }
            if let (Some((tk, _)), None) = (found("top"), found("sort")) {
                self.diag(tk.span.clone(), "top keeps the largest rows by the aggregate, so the report takes count, sum, average, min or max, or a sort");
            }
        }
        if self.diags.len() > before { return None; }
        let sink = if name.ends_with(".csv") { Sink::Csv(name.clone()) } else if name.ends_with(".jsonl") { Sink::Jsonl(name.clone()) } else { Sink::Out(name.clone()) };
        Some(Report { name, span: entry(k, v), key: k.span.clone(), sink, filter, fold, keep, top, sort })
    }

    /* the aggregate of entry `k` with the keys it or by gives */
    fn fold(&mut self, agg: Agg, k: &Node, v: &Node, by: Option<&(Node, Node)>) -> Option<Fold> {
        let span = entry(k, v);
        if agg == Agg::Count {
            if let Some((bk, _)) = by { self.diag(bk.span.clone(), "count takes its keys itself, by gives the keys of sum, average, min or max"); return None; }
            let keys = self.keys(k, v, "count")?;
            return self.columns(agg, keys).map(|keys| Fold { agg, span, measure: None, keys, by: None });
        }
        let measure = self.expr(k, v, agg.word())?;
        let (keys, by) = match by {
            Some((bk, bv)) if bv.empty() => { self.diag(bk.span.clone(), "by takes the keys of the rows, as by: name"); return None; }
            Some((bk, bv)) => (self.keys(bk, bv, "by")?, Some(entry(bk, bv))),
            None => (Vec::new(), None),
        };
        let keys = self.columns(agg, keys)?;
        Some(Fold { agg, span, measure: Some(measure), keys, by })
    }

    /* the keys count or by lists each a field or a name given an expression */
    fn keys(&mut self, k: &Node, v: &Node, what: &str) -> Option<Vec<Key>> {
        let before = self.diags.len();
        let mut keys = Vec::new();
        for span in self.items(k, v, what)? {
            let text = &self.src[span.clone()];
            if text.is_empty() { self.diag(span, format!("{what} holds an empty key between its commas")); continue; }
            if let Some((name, expr)) = named(self.src, span.clone()) {
                if WORDS.contains(&name) { self.diag(span, format!("\"{name}\" is no name a key may take")); continue; }
                if expr.is_empty() { self.diag(span, format!("the key {name} takes an expression after =")); continue; }
                keys.push(Key::Named { span, name: name.to_string(), expr });
            } else if let Some(parts) = path(text) {
                keys.push(Key::Path { span, parts });
            } else {
                self.diag(span, format!("\"{text}\" is no field, so the key takes a name, as name = {text}"));
            }
        }
        (self.diags.len() == before).then_some(keys)
    }

    /* the items of a list or of a value cut at its top level commas */
    fn items(&mut self, k: &Node, v: &Node, what: &str) -> Option<Vec<Range<usize>>> {
        match &v.value {
            Value::Scalar { style: Style::Plain, .. } => Some(split(self.src, v.span.clone())),
            Value::Seq(items) => {
                let mut out = Vec::new();
                for item in items {
                    match &item.value {
                        Value::Scalar { text, style: Style::Plain } if !text.is_empty() => out.push(item.span.clone()),
                        _ => { self.diag(item.span_or(k), format!("each item of {what} is a plain value such as name or a.b")); return None; }
                    }
                }
                Some(out)
            }
            Value::Scalar { .. } => { self.diag(v.span.clone(), format!("{what} takes its keys written with no quotes around them")); None }
            Value::Map(_) => { self.diag(v.span.clone(), format!("{what} takes its keys separated by commas, not a mapping")); None }
        }
    }

    /* refuses two keys of a fold that fill one column or one the aggregate fills */
    fn columns(&mut self, agg: Agg, keys: Vec<Key>) -> Option<Vec<Key>> {
        let before = self.diags.len();
        for (i, key) in keys.iter().enumerate() {
            let col = key.column();
            if col[0] == agg.word() {
                self.diag(key.span(), format!("the aggregate fills the column {}, so the key takes another name", agg.word()));
                continue;
            }
            for other in &keys[..i] {
                let o = other.column();
                let n = col.len().min(o.len());
                if col[..n] == o[..n] {
                    let msg = if col == o { format!("the key {} is given twice", col.join(".")) } else { format!("the keys {} and {} overlap", o.join("."), col.join(".")) };
                    self.diag(key.span(), msg);
                    break;
                }
            }
        }
        (self.diags.len() == before).then_some(keys)
    }

    fn top(&mut self, k: &Node, v: &Node) -> Option<(Range<usize>, u64)> {
        match v.text().and_then(|t| t.parse::<u64>().ok()) {
            Some(n) if v.plain() => Some((entry(k, v), n)),
            _ => { self.diag(v.span_or(k), "top takes a whole number"); None }
        }
    }

    /* the keys of a sort where a leading minus sorts that key descending */
    fn sort(&mut self, k: &Node, v: &Node) -> Option<Sort> {
        let items = self.items(k, v, "sort")?;
        if items.is_empty() { self.diag(k.span.clone(), "sort takes the keys it orders by"); return None; }
        let mut keys = Vec::new();
        for span in items {
            let text = &self.src[span.clone()];
            let desc = text.starts_with('-');
            let lead = if desc { 1 + text[1..].len() - text[1..].trim_start().len() } else { 0 };
            if span.start + lead >= span.end { self.diag(span, "sort holds an empty key"); return None; }
            keys.push((span.start + lead..span.end, desc));
        }
        Some(Sort { span: entry(k, v), keys })
    }
}

impl Node {
    /* the value's bytes or its key's when the value is empty */
    fn span_or(&self, key: &Node) -> Range<usize> { if self.span.is_empty() { key.span.clone() } else { self.span.clone() } }
}

/* the words naming the nearest of `known` to `key` or nothing when none is near */
fn hint<'k>(key: &str, known: impl Iterator<Item = &'k str>) -> String {
    edge::util::suggest::nearest(key, known).map_or_else(String::new, |k| format!(", did you mean {k}"))
}

/* the items of `span` cut at commas outside brackets and quotes then trimmed */
fn split(src: &str, span: Range<usize>) -> Vec<Range<usize>> {
    let b = src.as_bytes();
    let (mut out, mut start, mut depth, mut quote) = (Vec::new(), span.start, 0usize, None::<u8>);
    let mut at = span.start;
    while at < span.end {
        let c = b[at];
        match quote {
            Some(_) if c == b'\\' => at += 1,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'"' | b'\'' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => { out.push(trim(src, start..at)); start = at + 1; }
                _ => {}
            },
        }
        at += 1;
    }
    let last = trim(src, start..span.end);
    if !out.is_empty() || !last.is_empty() { out.push(last); }
    out
}

/* `span` without spaces and line ends at either end and empty when blank */
fn trim(src: &str, span: Range<usize>) -> Range<usize> {
    let text = &src[span.clone()];
    let end = span.start + text.trim_end().len();
    (span.start + text.len() - text.trim_start().len()).min(end)..end
}

/* the name and expression bytes of a key written as name = expression */
fn named(src: &str, span: Range<usize>) -> Option<(&str, Range<usize>)> {
    let text = &src[span.clone()];
    let end = text.find(|c: char| !name_char(c)).unwrap_or(text.len());
    let name = &text[..end];
    let rest = text[end..].trim_start();
    if !is_name(name) || !rest.starts_with('=') || rest.starts_with("==") { return None; }
    let at = span.end - rest.len() + 1;
    Some((name, trim(src, at..span.end)))
}

/* the parts of a field written as names joined by dots */
fn path(text: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = text.split('.').map(str::to_string).collect();
    parts.iter().all(|p| is_name(p) && !WORDS.contains(&p.as_str())).then_some(parts)
}
