mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    #[serde(default)] given: BTreeMap<String, String>,
    // Binary fixtures, each destination copied from a path relative to the repository root.
    #[serde(default)] copy: BTreeMap<String, String>,
    #[serde(default)] env: BTreeMap<String, String>,
    // Commands run first in the same dir, each must succeed, the way lock precedes a run.
    #[serde(default)] before: Vec<Vec<String>>,
    run: Vec<String>,
    #[serde(default)] stdin: String,
    #[serde(default)] stdout: Vec<String>,
    #[serde(default)] stderr: Vec<String>,
    #[serde(default)] fails: Option<Vec<String>>,
    #[serde(default)] creates: Vec<String>,
    #[serde(default)] contains: BTreeMap<String, String>,
    // A second command in the same dir, the way a packed artifact runs itself.
    #[serde(default)] then: Option<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    bin: String,
    #[serde(default)] run: Vec<String>,
    #[serde(default)] env: BTreeMap<String, String>,
    #[serde(default)] stdout: Vec<String>,
    #[serde(default)] stderr: Vec<String>,
    #[serde(default)] fails: Option<Vec<String>>,
}

/// JSON-driven CLI suite, each case in `cli.json` is one tempdir plus one spawn of the `edge` binary.
#[test]
fn cli_suite() {
    suite(include_str!("cli.json"), &[&[]]);
}

/// Third party wasm plugins built from `pdk/example`, run the same way a project declares them.
#[test]
fn plugin_suite() {
    suite(include_str!("plugins.json"), &[&[]]);
}

/// Programs both hosts must run alike, each case natively and again in the browser, which needs a Chrome.
#[test]
fn hosts_suite() {
    suite(include_str!("hosts.json"), &[&[], &["--web"]]);
}

fn suite(json: &str, hosts: &[&[&str]]) {
    let cases: Vec<Case> = serde_json::from_str(json).expect("case file parse");
    let bin = env!("CARGO_BIN_EXE_edge");
    // One cache per suite, so no case touches the real one and the runtime downloads once.
    let cache = tempfile::tempdir().expect("suite cache dir");
    let mut failed = vec![];
    for c in &cases {
        for host in hosts {
            // A host flag goes right after the subcommand, where `run` and `test` read it.
            let mut args = c.run.clone();
            args.splice(1..1, host.iter().map(|flag| flag.to_string()));
            if let Err(e) = check(bin, c, &args, cache.path()) {
                failed.push(format!("[edge {}] {e}", args.join(" ")));
            }
        }
    }
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

fn check(bin: &str, c: &Case, args: &[String], cache: &std::path::Path) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    for (p, v) in &c.given {
        let path = dir.path().join(p);
        if let Some(d) = path.parent() { let _ = std::fs::create_dir_all(d); }
        std::fs::write(path, v).map_err(|e| e.to_string())?;
    }
    for (dest, src) in &c.copy {
        let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(src);
        let path = dir.path().join(dest);
        if let Some(d) = path.parent() { let _ = std::fs::create_dir_all(d); }
        std::fs::copy(&from, path).map_err(|e| format!("fixture {}: {e}, build it first", from.display()))?;
    }
    for step in &c.before {
        let want = Expect { stdout: &[], stderr: &[], fails: None };
        run(bin, step, &c.env, dir.path(), cache, "", want).map_err(|e| format!("[before {}] {e}", step.join(" ")))?;
    }
    let want = Expect { stdout: &c.stdout, stderr: &c.stderr, fails: c.fails.as_deref() };
    run(bin, args, &c.env, dir.path(), cache, &c.stdin, want)?;
    for f in &c.creates { if !dir.path().join(f).exists() { return Err(format!("file missing: {f}")); } }
    for (f, n) in &c.contains {
        let t = std::fs::read_to_string(dir.path().join(f)).map_err(|e| e.to_string())?;
        if !t.contains(n) { return Err(format!("{f} missing {n:?}; got: {t}")); }
    }
    if let Some(s) = &c.then {
        let want = Expect { stdout: &s.stdout, stderr: &s.stderr, fails: s.fails.as_deref() };
        // `edge` names the binary under test, any other bin is a path in the case's dir.
        run(if s.bin == "edge" { bin } else { &s.bin }, &s.run, &s.env, dir.path(), cache, "", want).map_err(|e| format!("[then {}] {e}", s.bin))?;
    }
    Ok(())
}

// What one command must print and whether it must fail.
struct Expect<'a> {
    stdout: &'a [String],
    stderr: &'a [String],
    fails: Option<&'a [String]>,
}

fn run(bin: &str, args: &[String], env: &BTreeMap<String, String>, dir: &std::path::Path, cache: &std::path::Path, stdin: &str, want: Expect) -> Result<(), String> {
    let mut cmd = Command::new(bin);
    cmd.env("EDGE_CDN_BASE", common::cdn_base()?).env("XDG_CACHE_HOME", cache);
    let mut child = cmd.args(args).current_dir(dir).envs(env)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| e.to_string())?;
    if !stdin.is_empty() {
        child.stdin.as_mut().unwrap().write_all(stdin.as_bytes()).map_err(|e| e.to_string())?;
    }
    drop(child.stdin.take()); // close stdin so the process sees EOF
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let so = String::from_utf8_lossy(&out.stdout);
    let se = String::from_utf8_lossy(&out.stderr);
    let exit = out.status.code().unwrap_or(-1);
    let want_fail = want.fails.is_some();
    if (exit != 0) != want_fail {
        return Err(format!("exit {exit}; want {}; stderr: {se}", if want_fail { "non-zero" } else { "0" }));
    }
    for n in want.stderr.iter().chain(want.fails.into_iter().flatten()) {
        if !se.contains(n.as_str()) { return Err(format!("stderr missing {n:?}; got: {se}")); }
    }
    for n in want.stdout { if !so.contains(n) { return Err(format!("stdout missing {n:?}; got: {so}")); } }
    Ok(())
}
