use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::check::{moved_line, same_renamed};
use crate::{line_of, respell, Spell, INDENT, MINI};

// where a program sits in its file so a rewrite keeps every other byte
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    // a yaml literal block from its chomping indicator through its last line
    Block { indent: usize },
    // a double quoted string of json or yaml
    Quoted,
    // a yaml scalar written bare to the end of its line
    Plain,
    // text copied as it stands like a fenced block or a whole file
    Raw,
}

#[derive(Clone, Debug)]
pub struct Site {
    pub span: Range<usize>,
    pub style: Style,
    // what the scalar holds before the program like the group of a published message
    pub prefix: String,
    pub text: String,
    pub line: usize,
    pub label: String,
}

// what one program became and how it came back
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Same,
    Translated,
    // the translator refused at this line of the program
    Untranslatable { line: usize, why: String },
    // the translation compiles to another program so the source stays
    Differs(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Back {
    Exact,
    // the first line that came back otherwise with both texts
    Inexact { line: usize, was: String, now: String },
    Fails(String),
}

// one program translated and checked against its source
#[derive(Clone, Debug)]
pub struct Checked {
    pub status: Status,
    // the text the output tree holds which is the source when the translation failed
    pub out: String,
    // both spellings reject the program in other words so only the tokens were compared
    pub invalid: Option<(Vec<String>, Vec<String>)>,
    pub back: Option<Back>,
    // the first statement whose line moved as a line of each spelling
    pub moved: Option<(usize, usize)>,
}

// translates one program and checks it compiles alike and comes back
pub fn check(from: Spell, to: Spell, src: &str) -> Checked {
    let kept = |status| Checked { status, out: src.to_string(), invalid: None, back: None, moved: None };
    let out = match respell(from, to, src) {
        Ok(out) => out,
        Err(e) => return kept(Status::Untranslatable { line: line_of(src, e.at), why: e.why }),
    };
    let (before, after) = (from.compile(src), to.compile(&out));
    // errors worded alike such as an import no resolver answers still leave chunks to compare
    let invalid = !before.1.is_empty() && !after.1.is_empty() && before.1 != after.1;
    let (renames, forward) = from.renames(to);
    if !invalid && let Err(why) = same_renamed(&before, &after, &|n| renames.chunk_name(n, forward)) { return kept(Status::Differs(why)); }
    let moved = if invalid { None } else { moved_line(&before.0, src, &after.0, &out) };
    let invalid = invalid.then_some((before.1, after.1));
    let back = match respell(to, from, &out) {
        Ok(back) if back == src => Back::Exact,
        Ok(back) => {
            let (a, b): (Vec<&str>, Vec<&str>) = (src.split('\n').collect(), back.split('\n').collect());
            let at = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)).unwrap_or(0);
            Back::Inexact { line: at + 1, was: a.get(at).unwrap_or(&"").to_string(), now: b.get(at).unwrap_or(&"").to_string() }
        }
        Err(e) => Back::Fails(format!("line {} {}", line_of(&out, e.at), e.why)),
    };
    let status = if out == src { Status::Same } else { Status::Translated };
    Checked { status, out, invalid, back: Some(back), moved }
}

// the kinds of file the walk reads programs from
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    VmCases,
    CaseJson,
    SuiteJson,
    ActorYml,
    Markdown,
    Plain,
    // a list of objects whose `script` is a program as the js suite writes them
    ScriptJson,
}

// every file holding programs under `root` in a fixed order
pub fn sources(root: &Path) -> Vec<(PathBuf, Kind)> {
    let mut out = Vec::new();
    let mut add = |dir: &str, kind: Kind, keep: &dyn Fn(&Path) -> bool| {
        let mut found = Vec::new();
        walk(&root.join(dir), &mut found);
        found.retain(|p| keep(p));
        found.sort();
        out.extend(found.into_iter().filter_map(|p| p.strip_prefix(root).ok().map(|r| (r.to_path_buf(), kind))));
    };
    let ext = |e: &'static str| move |p: &Path| p.extension().is_some_and(|x| x == e);
    add("tests/cases/vm", Kind::VmCases, &ext("yml"));
    add("tests/cases", Kind::CaseJson, &|p| p.file_name().is_some_and(|n| n == "snapshot.json" || n == "modules.json"));
    add("cli/tests", Kind::SuiteJson, &|p| p.parent().is_some_and(|d| d.ends_with("cli/tests")) && ext("json")(p));
    add("cli/tests/actor", Kind::ActorYml, &|p| p.parent().is_some_and(|d| d.ends_with("actor")) && ext("yml")(p));
    add("cli/tests/actor/proj", Kind::Plain, &|p| source_file(&p.to_string_lossy()));
    add("docs", Kind::Markdown, &ext("mdx"));
    add("skill", Kind::Markdown, &|p| p.file_name().is_some_and(|n| n == "SKILL.md"));
    add("examples/net", Kind::SuiteJson, &|p| p.file_name().is_some_and(|n| n == "cases.json"));
    add("js/tests", Kind::ScriptJson, &|p| p.file_name().is_some_and(|n| n == "js.json"));
    add("js/tests", Kind::Plain, &|p| source_file(&p.to_string_lossy()));
    add("plugin/example", Kind::Plain, &|p| source_file(&p.to_string_lossy()));
    out
}

// how the walk reads the file at `rel` if it reads it at all
pub fn kind_of(root: &Path, rel: &Path) -> Option<Kind> {
    sources(root).into_iter().find(|(r, _)| r == rel).map(|(_, k)| k)
}

// a file name either spelling gives a program
pub fn source_file(name: &str) -> bool {
    [INDENT.extension, MINI.extension].iter().any(|e| name.strip_suffix(e).is_some_and(|n| n.ends_with('.')))
}

pub fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() { walk(&path, out) } else { out.push(path) }
    }
}

// the programs of a file in the order they appear
pub fn sites(kind: Kind, text: &str) -> Result<Vec<Site>, String> {
    match kind {
        Kind::VmCases => vm_sites(text),
        Kind::CaseJson => json_sites(text, case_program),
        Kind::SuiteJson => json_sites(text, suite_program),
        Kind::ActorYml => yml_sites(text, 0),
        Kind::Markdown => fence_sites(text),
        Kind::Plain => Ok(vec![Site { span: 0..text.len(), style: Style::Raw, prefix: String::new(), text: text.to_string(), line: 1, label: "file".into() }]),
        Kind::ScriptJson => json_sites(text, |path, _| match path { [Seg::At(i), Seg::Key(k)] if k == "script" => Some(format!("case {i}")), _ => None }),
    }
}

// the byte ranges of each line with its line break
fn lines(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for piece in text.split_inclusive('\n') {
        out.push(at..at + piece.len());
        at += piece.len();
    }
    out
}

fn bare<'a>(text: &'a str, r: &Range<usize>) -> &'a str { text[r.clone()].trim_end_matches('\n') }

// a literal block whose indicator sits at `at` on line `head`
fn block(text: &str, rows: &[Range<usize>], head: usize, at: usize) -> Result<(Site, usize), String> {
    let keep = bare(text, &rows[head])[at - rows[head].start..].trim_end() == "|";
    let lead = |l: &str| l.len() - l.trim_start_matches(' ').len();
    let (mut indent, mut last, mut j) = (None, head, head + 1);
    while j < rows.len() {
        let l = bare(text, &rows[j]);
        if l.trim_matches(' ').is_empty() { j += 1; continue; }
        match indent {
            None if lead(l) == 0 => break,
            None => indent = Some(lead(l)),
            Some(n) if lead(l) < n => break,
            Some(_) => {}
        }
        last = j;
        j += 1;
    }
    let Some(n) = indent else { return Err(format!("line {} holds an empty block", head + 1)) };
    let body: Vec<&str> = (head + 1..=last).map(|k| { let l = bare(text, &rows[k]); l.get(n..).unwrap_or("") }).collect();
    let mut program = body.join("\n");
    if keep { program.push('\n'); }
    Ok((Site { span: at..rows[last].end, style: Style::Block { indent: n }, prefix: String::new(), text: program, line: head + 2, label: String::new() }, last + 1))
}

// the end of the double quoted scalar opening at `at`
fn quoted_end(text: &str, at: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

fn vm_sites(text: &str) -> Result<Vec<Site>, String> {
    let rows = lines(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let l = bare(text, &rows[i]);
        let Some(rest) = l.strip_prefix("- case:") else { i += 1; continue };
        let at = rows[i].start + l.len() - rest.trim_start().len();
        let label = format!("case {}", out.len());
        let rest = rest.trim();
        if rest == "|-" || rest == "|" {
            let (mut site, next) = block(text, &rows, i, at)?;
            site.label = label;
            out.push(site);
            i = next;
        } else if rest.starts_with('"') {
            let end = quoted_end(text, at).ok_or_else(|| format!("line {} opens a string it never closes", i + 1))?;
            let value: String = serde_yaml_ng::from_str(&text[at..end]).map_err(|e| format!("line {} {e}", i + 1))?;
            out.push(Site { span: at..end, style: Style::Quoted, prefix: String::new(), text: value, line: i + 1, label });
            i += 1;
        } else {
            return Err(format!("line {} writes a case in a style the walk does not rewrite", i + 1));
        }
    }
    // the scanner reads each case as the yaml parser does or the file is refused
    let parsed: Vec<Value> = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    if parsed.len() != out.len() { return Err(format!("{} cases parse but {} were found", parsed.len(), out.len())); }
    for (k, (site, case)) in out.iter().zip(&parsed).enumerate() {
        if case["case"].as_str() != Some(site.text.as_str()) { return Err(format!("case {k} at line {} reads otherwise", site.line)); }
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Seg {
    Key(String),
    At(usize),
}

// every string value of a json text with its path and byte range
pub type Strings = Vec<(Vec<Seg>, Range<usize>)>;

// the decoded string at each path
type Values = HashMap<Vec<Seg>, String>;

pub fn json_strings(text: &str) -> Result<Strings, String> {
    let mut out = Vec::new();
    let end = json_value(text.as_bytes(), skip_ws(text.as_bytes(), 0), &mut Vec::new(), &mut out)?;
    if skip_ws(text.as_bytes(), end) != text.len() { return Err(format!("text after the value at byte {end}")); }
    Ok(out)
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
    i
}

fn json_string(b: &[u8], at: usize) -> Result<usize, String> {
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(format!("a string at byte {at} never closes"))
}

fn json_value(b: &[u8], at: usize, path: &mut Vec<Seg>, out: &mut Strings) -> Result<usize, String> {
    let close = |i: usize, c: u8| if b.get(i) == Some(&c) { Ok(i + 1) } else { Err(format!("expected {} at byte {i}", c as char)) };
    match b.get(at) {
        Some(b'"') => {
            let end = json_string(b, at)?;
            out.push((path.clone(), at..end));
            Ok(end)
        }
        Some(b'{') => {
            let mut i = skip_ws(b, at + 1);
            if b.get(i) == Some(&b'}') { return Ok(i + 1); }
            loop {
                let end = json_string(b, i)?;
                let key: String = serde_json::from_slice(&b[i..end]).map_err(|e| e.to_string())?;
                i = close(skip_ws(b, end), b':')?;
                path.push(Seg::Key(key));
                i = skip_ws(b, json_value(b, skip_ws(b, i), path, out)?);
                path.pop();
                if b.get(i) == Some(&b',') { i = skip_ws(b, i + 1); continue; }
                return close(i, b'}');
            }
        }
        Some(b'[') => {
            let mut i = skip_ws(b, at + 1);
            if b.get(i) == Some(&b']') { return Ok(i + 1); }
            let mut k = 0;
            loop {
                path.push(Seg::At(k));
                i = skip_ws(b, json_value(b, i, path, out)?);
                path.pop();
                k += 1;
                if b.get(i) == Some(&b',') { i = skip_ws(b, i + 1); continue; }
                return close(i, b']');
            }
        }
        Some(_) => {
            let mut i = at;
            while i < b.len() && !matches!(b[i], b',' | b']' | b'}') && !b[i].is_ascii_whitespace() { i += 1; }
            Ok(i)
        }
        None => Err("the text ends inside a value".into()),
    }
}

// the program fields of the engine's json cases
fn case_program(path: &[Seg], _: &Values) -> Option<String> {
    match path {
        [Seg::At(i), Seg::Key(k)] if k == "src" => Some(format!("case {i}")),
        [Seg::At(i), Seg::Key(m), Seg::Key(name), Seg::Key(c)] if m == "modules" && c == "code" => Some(format!("case {i} module {name}")),
        _ => None,
    }
}

// the program fields of the cli suites as given files and `-c` arguments
fn suite_program(path: &[Seg], all: &Values) -> Option<String> {
    let at = |p: &[Seg]| match p.first() { Some(Seg::At(i)) => *i, _ => 0 };
    match path {
        [.., Seg::Key(g), Seg::Key(name)] if g == "given" && source_file(name) => Some(format!("case {} given {name}", at(path))),
        [.., Seg::Key(r), Seg::At(k)] if r == "run" && *k > 0 => {
            let mut before = path.to_vec();
            before.pop();
            before.push(Seg::At(k - 1));
            (all.get(&before).map(String::as_str) == Some("-c")).then(|| format!("case {} run -c", at(path)))
        }
        _ => None,
    }
}

fn json_sites(text: &str, pick: fn(&[Seg], &Values) -> Option<String>) -> Result<Vec<Site>, String> {
    let strings = json_strings(text)?;
    let mut values = HashMap::new();
    for (path, span) in &strings {
        let value: String = serde_json::from_str(&text[span.clone()]).map_err(|e| e.to_string())?;
        values.insert(path.clone(), value);
    }
    Ok(strings.iter().filter_map(|(path, span)| {
        let label = pick(path, &values)?;
        Some(Site { span: span.clone(), style: Style::Quoted, prefix: String::new(), text: values[path].clone(), line: line_of(text, span.start), label })
    }).collect())
}

// the programs an actor file runs as group code and eval messages
fn yml_programs(doc: &serde_yaml_ng::Value) -> Vec<(String, String)> {
    let get = |v: &serde_yaml_ng::Value, k: &str| v.get(k).cloned().unwrap_or(serde_yaml_ng::Value::Null);
    let groups = get(doc, "groups");
    let mut eval = Vec::new();
    if let Some(map) = groups.as_mapping() {
        for (name, g) in map {
            if get(g, "eval").as_bool() == Some(true) && let Some(n) = name.as_str() { eval.push(n.to_string()); }
        }
    }
    let mut out = Vec::new();
    if let Some(map) = groups.as_mapping() {
        for (name, g) in map {
            let n = name.as_str().unwrap_or("");
            if eval.iter().any(|e| e == n) && let Some(seeds) = get(g, "seed").as_sequence() {
                out.extend(seeds.iter().filter_map(|s| s.as_str()).map(|s| (String::new(), s.to_string())));
            }
        }
    }
    for p in get(doc, "publish").as_sequence().into_iter().flatten().filter_map(|p| p.as_str()) {
        if let Some((g, body)) = p.split_once(' ') && eval.iter().any(|e| e == g) { out.push((format!("{g} "), body.to_string())); }
    }
    for post in get(doc, "post").as_sequence().into_iter().flatten() {
        let path = get(post, "path");
        if path.as_str().and_then(|p| p.strip_prefix("/eval/")).is_some_and(|g| eval.iter().any(|e| e == g))
            && let Some(body) = get(post, "body").as_str() { out.push((String::new(), body.to_string())); }
    }
    out
}

// the code blocks and quoted eval programs of a yaml text starting at byte `base`
fn yml_sites(text: &str, base: usize) -> Result<Vec<Site>, String> {
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    let mut wanted = yml_programs(&doc);
    let rows = lines(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let l = bare(text, &rows[i]);
        let t = l.trim_start();
        if let Some(rest) = t.strip_prefix("code:") && matches!(rest.trim(), "|" | "|-") {
            let at = rows[i].start + l.len() - rest.trim_start().len();
            let (mut site, next) = block(text, &rows, i, at)?;
            site.label = format!("code at line {}", i + 1);
            out.push(site);
            i = next;
            continue;
        }
        let mut j = rows[i].start;
        while j < rows[i].end {
            match text.as_bytes()[j] {
                b'#' if j == rows[i].start || text.as_bytes()[j - 1] == b' ' => break,
                b'"' => {
                    let Some(end) = quoted_end(text, j) else { break };
                    let value: String = serde_yaml_ng::from_str(&text[j..end]).unwrap_or_default();
                    if let Some(k) = wanted.iter().position(|(p, body)| value.strip_prefix(p.as_str()) == Some(body.as_str())) {
                        let (prefix, body) = wanted.remove(k);
                        out.push(Site { span: j..end, style: Style::Quoted, prefix, text: body, line: i + 1, label: format!("eval at line {}", i + 1) });
                    }
                    j = end;
                }
                _ => j += 1,
            }
        }
        i += 1;
    }
    if let Some((_, body)) = wanted.first() { return Err(format!("{} eval programs are not double quoted, the first {body:?}", wanted.len())); }
    for s in &mut out {
        s.span = s.span.start + base..s.span.end + base;
    }
    Ok(out)
}

// the program blocks of a markdown page and the code of its actor cells
fn fence_sites(text: &str) -> Result<Vec<Site>, String> {
    let rows = lines(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let l = bare(text, &rows[i]);
        let Some(info) = l.trim_start().strip_prefix("```") else { i += 1; continue };
        let close = (i + 1..rows.len()).find(|&k| bare(text, &rows[k]).trim() == "```");
        let Some(close) = close else { return Err(format!("line {} opens a fence it never closes", i + 1)) };
        let body = rows[i].end..rows[close].start;
        let mut words = info.split_whitespace();
        match (words.next().unwrap_or(""), words.next().unwrap_or("")) {
            ("python" | "edge" | "mini", _) => out.push(Site {
                span: body.clone(), style: Style::Raw, prefix: String::new(), text: text[body].to_string(), line: i + 2, label: format!("block at line {}", i + 1),
            }),
            ("yml", "actor" | "untrusted") => {
                let mut cell = yml_sites(&text[body.clone()], body.start).map_err(|e| format!("cell at line {} {e}", i + 1))?;
                for s in &mut cell { s.line += i + 1; }
                out.extend(cell);
            }
            _ => {}
        }
        i = close + 1;
    }
    Ok(out)
}

// a program written back in the style its site had
fn encode(site: &Site, program: &str, original: &str) -> String {
    match site.style {
        Style::Raw | Style::Plain => program.to_string(),
        Style::Quoted => serde_json::to_string(&format!("{}{program}", site.prefix)).unwrap_or_default(),
        Style::Block { indent } => {
            let body = program.strip_suffix('\n');
            let fits = !program.contains(['\r', '\u{2028}', '\u{2029}'])
                && !program.ends_with("\n\n")
                && program.split('\n').find(|l| !l.is_empty()).is_some_and(|l| !l.starts_with([' ', '\t']));
            // a program no literal block can hold falls back to a quoted string
            if !fits { return format!("{}\n", serde_json::to_string(program).unwrap_or_default()); }
            let mut out = String::from(if body.is_some() { "|" } else { "|-" });
            out.push('\n');
            for l in body.unwrap_or(program).split('\n') {
                if !l.is_empty() { out.push_str(&" ".repeat(indent)); out.push_str(l); }
                out.push('\n');
            }
            if !original.ends_with('\n') { out.pop(); }
            out
        }
    }
}

// the file with each changed program written over its site
pub fn rewrite(text: &str, sites: &[Site], programs: &[String]) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut at = 0;
    for (site, program) in sites.iter().zip(programs) {
        if *program == site.text { continue; }
        out.push_str(&text[at..site.span.start]);
        out.push_str(&encode(site, program, &text[site.span.clone()]));
        at = site.span.end;
    }
    out.push_str(&text[at..]);
    out
}

// a rewritten file reads back its new programs and every other field unchanged
pub fn verify(kind: Kind, old: &str, new: &str, programs: &[String]) -> Result<(), String> {
    let got: Vec<String> = sites(kind, new)?.into_iter().map(|s| s.text).collect();
    if got != programs { return Err("the rewritten file reads other programs back".into()); }
    match kind {
        Kind::VmCases => {
            let (a, b): (Vec<Value>, Vec<Value>) = (serde_yaml_ng::from_str(old).map_err(|e| e.to_string())?, serde_yaml_ng::from_str(new).map_err(|e| e.to_string())?);
            let strip = |mut v: Value| { if let Some(m) = v.as_object_mut() { m.remove("case"); } v };
            if a.len() != b.len() || a.into_iter().zip(b).any(|(x, y)| strip(x) != strip(y)) { return Err("a field beside a program changed".into()); }
        }
        Kind::CaseJson | Kind::SuiteJson | Kind::ScriptJson => {
            let (a, b) = (json_strings(old)?, json_strings(new)?);
            if a.iter().map(|x| &x.0).ne(b.iter().map(|x| &x.0)) { return Err("the json around the programs changed".into()); }
        }
        Kind::ActorYml => { serde_yaml_ng::from_str::<serde_yaml_ng::Value>(new).map_err(|e| e.to_string())?; }
        Kind::Markdown | Kind::Plain => {}
    }
    Ok(())
}

// the keys whose values hold what a case prints or the error it ends with
const TEXT_KEYS: &[&str] = &["expected", "error", "output", "stdout", "stderr", "fails"];

// the expected outputs and errors of a file which a respell renames words of
pub fn text_sites(kind: Kind, text: &str) -> Result<Vec<Site>, String> {
    match kind {
        Kind::VmCases => vm_text_sites(text),
        Kind::CaseJson | Kind::SuiteJson => json_sites(text, |path, _| path.iter().any(|s| matches!(s, Seg::Key(k) if TEXT_KEYS.contains(&k.as_str()))).then(|| "text".to_string())),
        _ => Ok(Vec::new()),
    }
}

fn vm_text_sites(text: &str) -> Result<Vec<Site>, String> {
    let rows = lines(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let l = bare(text, &rows[i]);
        let Some((key, rest)) = l.strip_prefix("  ").and_then(|k| k.split_once(':')) else { i += 1; continue };
        if !TEXT_KEYS.contains(&key) || key.starts_with(' ') { i += 1; continue; }
        let at = rows[i].start + l.len() - rest.trim_start().len();
        let value = rest.trim();
        if value == "|-" || value == "|" {
            let (site, next) = block(text, &rows, i, at)?;
            out.push(Site { label: key.to_string(), ..site });
            i = next;
            continue;
        }
        if value.starts_with('"') {
            let end = quoted_end(text, at).ok_or_else(|| format!("line {} opens a string it never closes", i + 1))?;
            let decoded: String = serde_yaml_ng::from_str(&text[at..end]).map_err(|e| format!("line {} {e}", i + 1))?;
            out.push(Site { span: at..end, style: Style::Quoted, prefix: String::new(), text: decoded, line: i + 1, label: key.to_string() });
        } else if !value.is_empty() {
            out.push(Site { span: at..at + value.len(), style: Style::Plain, prefix: String::new(), text: value.to_string(), line: i + 1, label: key.to_string() });
        }
        i += 1;
    }
    Ok(out)
}

// a file with its texts' words renamed refusing a renamed text that reads otherwise
pub fn rename_texts(kind: Kind, text: &str, renames: &crate::Renames, forward: bool) -> Result<String, String> {
    if renames.texts.is_empty() { return Ok(text.to_string()); }
    let sites = text_sites(kind, text)?;
    let new: Vec<String> = sites.iter().map(|s| renames.text(&s.text, forward)).collect();
    let out = rewrite(text, &sites, &new);
    let got: Vec<String> = text_sites(kind, &out)?.into_iter().map(|s| s.text).collect();
    if got != new { return Err("the renamed texts read back otherwise".into()); }
    Ok(out)
}

// a walked file with what each program became and the text the output tree takes
pub struct FileReport {
    pub rel: PathBuf,
    // where the output tree holds the file once its name takes the other extension
    pub to: PathBuf,
    pub kind: Kind,
    pub outcomes: Vec<(Site, Checked)>,
    // why the file went to the output unchanged
    pub error: Option<String>,
    pub text: String,
}

// `text` with each file name ending in `.from` ending in `.to` instead
pub fn renamed(text: &str, from: &str, to: &str) -> String {
    let dot = format!(".{from}");
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut rest = text;
    while let Some(at) = rest.find(&dot) {
        let end = at + dot.len();
        // a longer word such as `.pyc` is no file name of the spelling
        let longer = rest[end..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
        out.push_str(&rest[..at]);
        out.push('.');
        out.push_str(if longer { from } else { to });
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

// translates the programs of `files` under `root` past the first `first` cases of each file
pub fn translate_tree(root: &Path, files: Vec<(PathBuf, Kind)>, from: Spell, to: Spell, first: usize) -> Result<Vec<FileReport>, String> {
    let (old, new) = (from.syntax().extension, to.syntax().extension);
    let mut out = Vec::new();
    for (rel, kind) in files {
        let text = std::fs::read_to_string(root.join(&rel)).map_err(|e| format!("{} {e}", rel.display()))?;
        // a project's files and the names suites give them take the spelling's extension
        let dest = match kind {
            Kind::Plain => PathBuf::from(renamed(&rel.to_string_lossy(), old, new)),
            _ => rel.clone(),
        };
        let found = match sites(kind, &text) {
            Ok(found) => found,
            Err(e) => {
                out.push(FileReport { rel, to: dest, kind, outcomes: Vec::new(), error: Some(e), text });
                continue;
            }
        };
        // the cases another package appended translate while those before them stay as they are
        let kept = |i: usize| kind == Kind::VmCases && i < first;
        let checked: Vec<Checked> = found.iter().enumerate().map(|(i, s)| if kept(i) { Checked { status: Status::Same, out: s.text.clone(), invalid: None, back: None, moved: None } } else { check(from, to, &s.text) }).collect();
        let programs: Vec<String> = checked.iter().map(|c| c.out.clone()).collect();
        let new_text = rewrite(&text, &found, &programs);
        let (renames, forward) = from.renames(to);
        let checked_text = verify(kind, &text, &new_text, &programs).and_then(|()| rename_texts(kind, &new_text, &renames, forward));
        let (error, text) = match checked_text {
            Ok(done) if matches!(kind, Kind::CaseJson | Kind::SuiteJson | Kind::ActorYml) => (None, renamed(&done, old, new)),
            Ok(done) => (None, done),
            Err(e) => (Some(e), text),
        };
        out.push(FileReport { rel, to: dest, kind, outcomes: found.into_iter().zip(checked).collect(), error, text });
    }
    Ok(out)
}

// the file line of line `at` of a program held at `site`
pub fn file_line(site: &Site, at: usize) -> usize {
    match site.style {
        Style::Quoted | Style::Plain => site.line,
        Style::Block { .. } | Style::Raw => site.line + at.saturating_sub(1),
    }
}
