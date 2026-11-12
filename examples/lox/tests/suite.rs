use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/* what one test file asks of a run as the book's test runner reads it */
#[derive(Default)]
struct Expect {
    out: Vec<String>,
    errors: BTreeSet<String>,
    runtime: Option<(String, usize)>,
    code: i32,
}

/* the digits leading `s` and the rest */
fn digits(s: &str) -> Option<(usize, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    Some((s[..end].parse().ok()?, &s[end..]))
}

/* an error line marker of clox or of both as `[c line 3] Error` */
fn error_line(c: &str) -> Option<(usize, &str)> {
    let rest = c.strip_prefix('[')?;
    let rest = rest.strip_prefix("c ").unwrap_or(rest);
    if rest.starts_with("java ") { return None; }
    let (n, rest) = digits(rest.strip_prefix("line ")?)?;
    let rest = rest.strip_prefix("] ")?;
    rest.starts_with("Error").then_some((n, rest))
}

fn expectations(src: &str) -> Option<Expect> {
    let mut e = Expect::default();
    for (i, line) in src.lines().enumerate() {
        let n = i + 1;
        if line.contains("// nontest") { return None; }
        let Some(at) = line.find("// ") else { continue };
        let c = &line[at + 3..];
        if let Some(rest) = line.find("// expect:").map(|k| &line[k + 10..]) {
            e.out.push(rest.strip_prefix(' ').unwrap_or(rest).into());
        } else if let Some(k) = line.find("// Error") {
            e.errors.insert(format!("[{n}] {}", &line[k + 3..]));
            e.code = 65;
        } else if let Some((m, rest)) = error_line(c) {
            e.errors.insert(format!("[{m}] {rest}"));
            e.code = 65;
        } else if c.contains("[java line") {
            continue;
        } else if let Some(k) = line.find("// expect runtime error: ") {
            e.runtime = Some((line[k + 25..].into(), n));
            e.code = 70;
        }
    }
    Some(e)
}

/* the line a `[line N]` marker in `s` names */
fn marked(s: &str) -> Option<(usize, &str)> {
    let at = s.find("line ")?;
    let (n, rest) = digits(&s[at + 5..])?;
    Some((n, rest.strip_prefix("] ")?))
}

/* every failure of one test with its output beside what the file expects */
fn check(path: &Path) -> Option<Vec<String>> {
    let src = std::fs::read_to_string(path).ok()?;
    let e = expectations(&src)?;
    let run = Command::new(env!("CARGO_BIN_EXE_lox")).arg(path).output().expect("lox runs");
    let (out, err) = (String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr));
    let err: Vec<&str> = err.lines().collect();
    let mut fails = Vec::new();
    match &e.runtime {
        Some((msg, line)) => {
            if err.first() != Some(&msg.as_str()) { fails.push(format!("expected runtime error '{msg}' and got {:?}", err.first())); }
            match err.iter().skip(1).find_map(|l| l.find("[line ").and_then(|k| digits(&l[k + 6..]))) {
                Some((n, _)) if n == *line => {}
                got => fails.push(format!("expected runtime error on line {line} and got {got:?}")),
            }
        }
        None => {
            let mut found = BTreeSet::new();
            for l in err.iter().filter(|l| !l.is_empty()) {
                match l.starts_with('[').then(|| marked(l)).flatten().map(|(n, rest)| format!("[{n}] {rest}")) {
                    Some(m) if e.errors.contains(&m) => { found.insert(m); }
                    _ => fails.push(format!("unexpected error output '{l}'")),
                }
            }
            for m in e.errors.difference(&found) { fails.push(format!("missing expected error '{m}'")); }
        }
    }
    let code = run.status.code().unwrap_or(-1);
    if code != e.code { fails.push(format!("expected exit code {} and got {code}", e.code)); }
    let got: Vec<&str> = out.lines().collect();
    for i in 0..got.len().max(e.out.len()) {
        match (e.out.get(i), got.get(i)) {
            (Some(w), Some(g)) if w == g => {}
            (w, g) => fails.push(format!("output line {}: expected {w:?} and got {g:?}", i + 1)),
        }
    }
    Some(fails)
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the suite reads") {
        let p = entry.expect("an entry").path();
        if p.is_dir() { files(&p, out); } else if p.extension().is_some_and(|x| x == "lox") { out.push(p); }
    }
}

// the book's suite as the external bar every change must keep whole
#[test]
fn the_book_suite_passes() {
    let mut all = Vec::new();
    files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/suite"), &mut all);
    all.sort();
    let (mut passed, mut report) = (0, String::new());
    let total = all.iter().filter(|p| {
        let Some(fails) = check(p) else { return false };
        if fails.is_empty() { passed += 1; } else { report += &format!("FAIL {}\n  {}\n", p.display(), fails.join("\n  ")); }
        true
    }).count();
    println!("lox suite: {passed} of {total} pass");
    assert!(passed == total, "{report}lox suite: {passed} of {total} pass");
}
