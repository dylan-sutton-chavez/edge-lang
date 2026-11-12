use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use cli::{files, perms, Kit, Runtime};

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "EDGE_AS_CHANNELS";

// the edge.wasm build.rs copied beside its precompile
const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edge.wasm"));

// the records the program reads through files and the channels it writes them to
const RECORDS: &str = "{\"type\": \"WatchEvent\"}\n{\"type\": \"PushEvent\"}\n{\"type\": \"WatchEvent\"}\n";

// reads the records then sends their size to a channel and a header to a file
const PROGRAM: &str = "var files = import(\"files\");\nvar f = files.open(\"events.jsonl\");\nvar n = 0;\nvar chunk = files.read(f);\nwhile (chunk != nil) {\n    n = n + len(chunk);\n    chunk = files.read(f);\n}\nsend(\"sizes\", text(n));\nsend(\"sizes\", \"done\");\nsend(\"file:activity.csv\", \"minute,type,count\");\nsend(\"totals\", \"one\");\n";

/* run over lox with the files a grant names */
fn kit() -> Kit {
    Kit::new("channels", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &lox::DESCRIPTOR))
        .title("Channels on the Edge engine")
        .command(cli::cmd::run::Run)
        .field(perms::FIELD)
        .access(Arc::new(perms::Perms))
        .module(Arc::new(files::Files))
}

/* a project of its own holding the program and the grants and the records */
fn project(name: &str, program: &str, grants: &[&str]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("channels").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the target directory takes a project");
    let granted: Vec<String> = grants.iter().map(|grant| format!("\"{grant}\"")).collect();
    std::fs::write(dir.join("edge.json"), format!("{{ \"permissions\": {{ \"main\": [{}] }} }}\n", granted.join(", "))).expect("writing edge.json");
    std::fs::write(dir.join("main.lox"), program).expect("writing the program");
    std::fs::write(dir.join("events.jsonl"), RECORDS).expect("writing the records");
    dir
}

/* one run of the CLI in `dir` with what it printed and failed with */
fn run(dir: &Path) -> (String, String, i32) {
    let exe = std::env::current_exe().expect("the test binary runs from a path");
    let out = Command::new(exe)
        .args(["run", "main.lox"])
        .current_dir(dir)
        .env(AS_CLI, "1")
        // one cache for every run so edge.wasm compiles once
        .env("XDG_CACHE_HOME", Path::new(env!("CARGO_TARGET_TMPDIR")).join("channels-cache"))
        .stdin(Stdio::null())
        .output()
        .expect("the test binary runs as the CLI");
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
}

/* a named channel prints its lines under its name and a file channel writes its file */
fn each_channel_writes_where_its_name_says() -> Option<String> {
    let dir = project("named", PROGRAM, &["files:events.jsonl"]);
    let (out, err, code) = run(&dir);
    let want = format!("sizes\n{}\ndone\ntotals\none\n", RECORDS.len());
    if (out.as_str(), err.as_str(), code) != (want.as_str(), "", 0) {
        return Some(format!("the run printed {out:?} and said {err:?} with exit {code}, not {want:?}"));
    }
    match std::fs::read_to_string(dir.join("activity.csv")).unwrap_or_default().as_str() {
        "minute,type,count\n" => None,
        csv => Some(format!("activity.csv holds {csv:?}")),
    }
}

/* a run error reads in the runtime's words naming its line */
fn an_error_reads_in_the_runtimes_words() -> Option<String> {
    let dir = project("words", "var n = 1;\nprint \"type\" + n;\n", &["files:events.jsonl"]);
    let (_, err, code) = run(&dir);
    let want = "Operands must be two numbers or two strings.\n[line 2] in script\n";
    match (err.as_str(), code) {
        (seen, 1) if seen == want => None,
        (seen, code) => Some(format!("the failed run said {seen:?} with exit {code} and not {want:?}")),
    }
}

/* a path no grant names stops the run at its open in the runtime's words */
fn a_denied_path_reads_in_the_runtimes_words() -> Option<String> {
    let dir = project("denied", PROGRAM, &["files:other.jsonl"]);
    let (out, err, code) = run(&dir);
    let want = "'main' has no files:events.jsonl, edge.json grants it files:other.jsonl\n[line 2] in script\n";
    match (out.as_str(), err.as_str(), code) {
        ("", seen, 1) if seen == want => None,
        (out, seen, code) => Some(format!("the denied run printed {out:?} and said {seen:?} with exit {code}, not {want:?}")),
    }
}

/* a channel naming a path above the run is refused and writes nothing */
fn a_channel_above_the_run_is_refused() -> Option<String> {
    let dir = project("stray", "send(\"file:../stray.csv\", \"row\");\n", &[]);
    let (_, err, code) = run(&dir);
    let want = "error: the channel 'file:../stray.csv' names no file below the directory of the run\n";
    if (err.as_str(), code) != (want, 1) {
        return Some(format!("the stray channel said {err:?} with exit {code} and not {want:?}"));
    }
    match dir.parent().map(|above| above.join("stray.csv")).is_some_and(|path| path.exists()) {
        true => Some(String::from("the stray channel wrote a file above the run")),
        false => None,
    }
}

/* a channel running through a link out of the run is refused and writes nothing */
#[cfg(unix)]
fn a_channel_through_a_link_out_of_the_run_is_refused() -> Option<String> {
    let out = "leads through a link out of the directory of the run";
    let nothing = "is a link to nothing, a channel follows no link it cannot resolve";
    // links to a file above the run and to the directory above and to nothing
    for (name, link, to, report, why) in [("linked", "link.csv", "../linked.csv", "link.csv", out), ("linked-dir", "up", "..", "up/escaped.csv", out), ("dangling", "gone.csv", "../gone.csv", "gone.csv", nothing)] {
        let dir = project(name, &format!("send(\"file:{report}\", \"row\");\n"), &[]);
        let above = dir.parent().expect("a project sits in a directory");
        std::fs::write(above.join("linked.csv"), "untouched\n").expect("writing the file above the run");
        for stray in ["escaped.csv", "gone.csv"] {
            let _ = std::fs::remove_file(above.join(stray));
        }
        std::os::unix::fs::symlink(to, dir.join(link)).expect("linking out of the run");
        let (_, err, code) = run(&dir);
        let want = format!("error: the channel 'file:{report}' {why}\n");
        if (err.as_str(), code) != (want.as_str(), 1) {
            return Some(format!("the linked channel said {err:?} with exit {code} and not {want:?}"));
        }
        let kept = std::fs::read_to_string(above.join("linked.csv")).unwrap_or_default();
        if kept != "untouched\n" || ["escaped.csv", "gone.csv"].iter().any(|stray| above.join(stray).exists()) {
            return Some(format!("the channel {report} wrote above the run through its link"));
        }
    }
    None
}

fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit().main()
    }
    let mut found = vec![
        each_channel_writes_where_its_name_says(),
        an_error_reads_in_the_runtimes_words(),
        a_denied_path_reads_in_the_runtimes_words(),
        a_channel_above_the_run_is_refused(),
    ];
    #[cfg(unix)]
    found.push(a_channel_through_a_link_out_of_the_run_is_refused());
    let ran = found.len();
    let failed: Vec<String> = found.into_iter().flatten().collect();
    if !failed.is_empty() {
        eprintln!("{} of {ran} runs differ\n{}", failed.len(), failed.join("\n"));
        std::process::exit(1);
    }
    println!("{ran} runs of the channels CLI passed");
}
