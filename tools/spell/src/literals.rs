use std::path::{Path, PathBuf};

use edge::lexer::TokenType as T;

use crate::{line_of, respell, Spell};

// a string literal of a rust file decoded with the line it opens on
pub struct Literal {
    pub line: usize,
    pub text: String,
    // the bytes between its quotes and whether it is raw
    pub span: std::ops::Range<usize>,
    pub raw: bool,
}

fn ident(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'_' }

// the escapes of a plain rust string from just past its opening quote
fn plain(b: &[u8], mut i: usize) -> (String, usize) {
    let mut out = Vec::new();
    while i < b.len() && b[i] != b'"' {
        if b[i] != b'\\' { out.push(b[i]); i += 1; continue; }
        let e = b.get(i + 1).copied().unwrap_or(b'\\');
        i += 2;
        match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'0' => out.push(0),
            b'x' => {
                let hex = std::str::from_utf8(b.get(i..i + 2).unwrap_or(b"00")).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 2;
            }
            b'u' => {
                let end = (i..b.len()).find(|&k| b[k] == b'}').unwrap_or(i);
                let hex = std::str::from_utf8(b.get(i + 1..end).unwrap_or(b"")).unwrap_or("");
                let c = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{fffd}');
                out.extend(c.to_string().bytes());
                i = end + 1;
            }
            b'\n' => while i < b.len() && b[i].is_ascii_whitespace() { i += 1; },
            other => out.push(other),
        }
    }
    (String::from_utf8_lossy(&out).into_owned(), i + 1)
}

// every string literal of a rust source outside comments and char literals
pub fn rust_strings(src: &str) -> Vec<Literal> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let fresh = i == 0 || !ident(b[i - 1]);
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => while i < b.len() && b[i] != b'\n' { i += 1; },
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
                i += 2;
            }
            b'\'' => {
                // a char literal is skipped whole and a lifetime only by its quote
                let len = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                i = if b.get(i + 1) == Some(&b'\\') {
                    (i + 3..b.len()).find(|&k| b[k] == b'\'').map_or(b.len(), |k| k + 1)
                } else if b.get(i + 1 + len) == Some(&b'\'') { i + 2 + len } else { i + 1 };
            }
            b'"' => {
                let (text, end) = plain(b, i + 1);
                out.push(Literal { line: line_of(src, i), text, span: i + 1..end - 1, raw: false });
                i = end;
            }
            b'b' | b'r' if fresh => {
                let mut j = i + (b[i] == b'b') as usize;
                let raw = b.get(j) == Some(&b'r');
                j += raw as usize;
                let hashes = b[j..].iter().take_while(|&&c| c == b'#').count();
                if b.get(j + hashes) != Some(&b'"') || (!raw && hashes > 0) || (!raw && b[i] == b'r') {
                    while i < b.len() && ident(b[i]) { i += 1; }
                    continue;
                }
                let start = j + hashes + 1;
                if raw {
                    let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                    let end = (start..b.len()).find(|&k| b[k..].starts_with(&close)).unwrap_or(b.len());
                    out.push(Literal { line: line_of(src, i), text: String::from_utf8_lossy(&b[start..end]).into_owned(), span: start..end, raw: true });
                    i = end + close.len();
                } else {
                    let (text, end) = plain(b, start);
                    out.push(Literal { line: line_of(src, i), text, span: start..end - 1, raw: false });
                    i = end;
                }
            }
            c if ident(c) => while i < b.len() && ident(b[i]) { i += 1; },
            _ => i += 1,
        }
    }
    out
}

// whether a literal compiles as a program rather than an output or a message
fn program_like(spell: Spell, text: &str) -> bool {
    let (toks, errors) = spell.lex(text);
    if errors > 0 || spell.compile(text).1.iter().any(|m| !m.contains("no resolver configured")) { return false; }
    let toks: Vec<T> = toks.iter().map(|t| t.kind).filter(|k| !matches!(k, T::Comment | T::LineBreak | T::Eof)).collect();
    let starts = |i: &usize| *i == 0 || matches!(toks[i - 1], T::Newline | T::Indent | T::Dedent | T::Semi | T::BlockOpen | T::BlockClose);
    (0..toks.len()).filter(starts).any(|i| match toks[i] {
        T::Fn | T::Class | T::If | T::For | T::While | T::Try | T::Import | T::From | T::Return | T::With | T::Async | T::Throw
        | T::Assert | T::Delete | T::Global | T::Outer | T::Empty | T::Yield | T::Match => true,
        T::Name => matches!(toks.get(i + 1), Some(T::ParenOpen | T::Equal | T::Dot | T::BracketOpen | T::Comma | T::PlusEqual | T::MinusEqual)),
        _ => false,
    })
}

// one literal or file a person respells by hand at the flip
pub struct Hand {
    pub file: PathBuf,
    pub line: usize,
    pub what: String,
    pub status: String,
}

fn status(from: Spell, to: Spell, text: &str) -> Option<String> {
    match respell(from, to, text) {
        Ok(out) if out == text => None,
        Ok(_) => Some("translates".into()),
        Err(e) => Some(format!("untranslatable line {} {}", line_of(text, e.at), e.why)),
    }
}

// the programs the walk does not rewrite with how each would translate
pub fn by_hand(root: &Path, from: Spell, to: Spell) -> Vec<Hand> {
    let mut out = Vec::new();
    let mut files = Vec::new();
    for dir in ["src", "tests", "lib", "cli/src", "cli/tests", "examples", "bench/src", "skill", "fuzz/src"] {
        crate::corpus::walk(&root.join(dir), &mut files);
    }
    files.retain(|p| p.extension().is_some_and(|e| e == "rs"));
    files.sort();
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        for lit in rust_strings(&src) {
            let file = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            let preview = |t: &str| serde_json::to_string(&t.chars().take(70).collect::<String>()).unwrap_or_default();
            // an actor file written as a literal holds its programs in its groups
            if lit.text.contains("groups:") && let Ok(sites) = crate::corpus::sites(crate::corpus::Kind::ActorYml, &lit.text) {
                for site in sites {
                    if let Some(status) = status(from, to, &site.text) {
                        out.push(Hand { file: file.clone(), line: lit.line + site.line - 1, what: preview(&site.text), status });
                    }
                }
                continue;
            }
            if !program_like(from, &lit.text) { continue; }
            if let Some(status) = status(from, to, &lit.text) { out.push(Hand { file, line: lit.line, what: preview(&lit.text), status }); }
        }
    }
    out
}

// a text as the body of a plain rust string literal
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// one literal the rust respell left alone with why and the start of its text
pub struct Skipped {
    pub line: usize,
    pub why: String,
    pub text: String,
}

// whether the text between two literals only separates the items of one list
fn joins_items(between: &str) -> bool {
    between.lines().all(|l| { let t = l.trim().trim_start_matches(',').trim(); t.is_empty() || t.starts_with("//") })
}

// a rust source with each literal run holding a `from` program printed in `to`
pub fn respell_rust(src: &str, from: Spell, to: Spell) -> (String, usize, Vec<Skipped>) {
    let lits = rust_strings(src);
    // literals that each end a line on lines of their own read as one text
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, l) in lits.iter().enumerate() {
        let joined = groups.last().and_then(|g| g.last()).is_some_and(|&p| {
            let gap = &src[lits[p].span.end + 1..l.span.start.saturating_sub(1)];
            !lits[p].raw && !l.raw && lits[p].text.ends_with('\n') && gap.contains('\n') && joins_items(gap)
        });
        if joined { if let Some(g) = groups.last_mut() { g.push(i); } } else { groups.push(vec![i]); }
    }
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut skipped = Vec::new();
    for g in &groups {
        let first = &lits[g[0]];
        let text: String = g.iter().map(|&i| lits[i].text.as_str()).collect();
        // an actor file written as a literal respells the programs of its groups alone
        if text.contains("groups:") && let Ok(sites) = crate::corpus::sites(crate::corpus::Kind::ActorYml, &text) {
            let programs: Vec<String> = sites.iter().map(|site| respell(from, to, &site.text).unwrap_or_else(|_| site.text.clone())).collect();
            let new = crate::corpus::rewrite(&text, &sites, &programs);
            if new != text && g.len() == 1 && !first.raw && escaped(&first.text) == src[first.span.clone()] { edits.push((first.span.clone(), escaped(&new))); }
            else if new != text { skipped.push(Skipped { line: first.line, why: "an actor file over several literals".into(), text: text.chars().take(90).collect() }); }
            continue;
        }
        let (toks, errors) = from.lex(&text);
        let blocks = toks.iter().any(|t| matches!(t.kind, T::BlockOpen | T::BlockClose));
        if errors > 0 || !blocks || !program_like(from, &text) && from.compile(&text).1.is_empty() { continue; }
        let skip = |why: String| Skipped { line: first.line, why, text: text.chars().take(90).collect() };
        let before = &src[first.span.start.saturating_sub(24)..first.span.start];
        if ["format!(", "print!(", "println!(", "write!(", "writeln!(", "panic!(", "format_args!("].iter().any(|m| before.contains(m)) { skipped.push(skip("a format string".into())); continue; }
        let new = match respell(from, to, &text) {
            Ok(new) => new,
            Err(e) => { skipped.push(skip(format!("line {} {}", line_of(&text, e.at), e.why))); continue; }
        };
        if let Err(why) = crate::check::same_program(&from.compile(&text), &to.compile(&new)) { skipped.push(skip(format!("compiles to another program, {why}"))); continue; }
        // the new text splits back into the literals by the lines each held
        let mut pieces = Vec::new();
        let mut rest = new.as_str();
        for (k, &i) in g.iter().enumerate() {
            let lines = lits[i].text.matches('\n').count();
            let cut = if k + 1 == g.len() { rest.len() } else { rest.match_indices('\n').nth(lines.saturating_sub(1)).map_or(rest.len(), |(at, _)| at + 1) };
            pieces.push(&rest[..cut]);
            rest = &rest[cut..];
        }
        let fits = g.len() == 1 || pieces.iter().zip(g).all(|(p, &i)| p.matches('\n').count() == lits[i].text.matches('\n').count());
        if !fits { skipped.push(skip("the respelled text splits otherwise".into())); continue; }
        for (p, &i) in pieces.iter().zip(g) {
            let l = &lits[i];
            if *p == l.text { continue; }
            if !l.raw && escaped(&l.text) != src[l.span.clone()] { skipped.push(Skipped { line: l.line, why: "escapes the respell would write otherwise".into(), text: l.text.chars().take(90).collect() }); continue; }
            edits.push((l.span.clone(), if l.raw { p.to_string() } else { escaped(p) }));
        }
    }
    let mut out = String::with_capacity(src.len());
    let mut at = 0;
    for (span, text) in &edits {
        out.push_str(&src[at..span.start]);
        out.push_str(text);
        at = span.end;
    }
    out.push_str(&src[at..]);
    (out, edits.len(), skipped)
}
