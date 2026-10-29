use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use cli::{files, perms, Kit, Runtime};

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "EDGE_AS_FLOWS";

// the flow runtime make wasm-tiny builds
const WASM: &[u8] = include_bytes!("../../target/wasm32-unknown-unknown/release/tiny.wasm");

// the canonical flow of examples/data whose source line each case points at its own file
const FLOW: &str = include_str!("../../examples/data/github-hour.flow.yaml");

// the reports of that flow in the order its keys name them
const REPORTS: [&str; 5] = ["top_stars", "busy_branches", "activity.csv", "issues_opened", "time_to_close"];

/* run over the flow runtime with its own words and the files a grant names */
fn kit() -> Kit {
    Kit::new("flows", env!("CARGO_PKG_VERSION"), Runtime::wasm(WASM, &tiny::DESCRIPTOR).words(tiny::report))
        .title("Flows on the Edge engine")
        .command(cli::cmd::run::Run)
        .field(perms::FIELD)
        .access(Arc::new(perms::Perms))
        .module(Arc::new(files::Files))
}

/* the flow with its one source line pointing at the file the case wrote */
fn over(file: &str) -> String {
    let rest = FLOW.split_once('\n').expect("the canonical flow starts with its source line").1;
    format!("from: {file}\n{rest}")
}

/* a project of its own holding the flow and the grants and the records */
fn project(name: &str, flow: &str, grants: &[&str], records: &[String]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("channels").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the target directory takes a project");
    let granted: Vec<String> = grants.iter().map(|grant| format!("\"{grant}\"")).collect();
    std::fs::write(dir.join("edge.json"), format!("{{ \"permissions\": {{ \"main\": [{}] }} }}\n", granted.join(", "))).expect("writing edge.json");
    std::fs::write(dir.join("main.flow.yaml"), flow).expect("writing the flow");
    if !records.is_empty() {
        std::fs::write(dir.join("events.jsonl"), format!("{}\n", records.join("\n"))).expect("writing the records");
    }
    dir
}

/* one run of the CLI in `dir` with what it printed and failed with */
fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let exe = std::env::current_exe().expect("the test binary runs from a path");
    let out = Command::new(exe)
        .args(args)
        .current_dir(dir)
        .env(AS_CLI, "1")
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("the test binary runs as the CLI");
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
}

/* the printed channels in the order the host named them with the lines under each */
fn printed(out: &str) -> Vec<(String, Vec<String>)> {
    let mut channels: Vec<(String, Vec<String>)> = Vec::new();
    for line in out.lines() {
        match REPORTS.contains(&line) {
            true => channels.push((line.to_string(), Vec::new())),
            false => match channels.last_mut() {
                Some((_, lines)) => lines.push(line.to_string()),
                None => panic!("the host printed {line:?} before naming a channel"),
            },
        }
    }
    channels
}

/* the records the flow keeps with the bots its top-level where leaves out */
fn kept(slice: &str) -> Vec<serde_json::Value> {
    slice
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("an event of the slice reads"))
        .filter(|event| !event["actor"]["login"].as_str().unwrap_or("").ends_with("[bot]"))
        .collect()
}

fn text(event: &serde_json::Value, path: &[&str]) -> String {
    let mut at = event;
    for key in path {
        at = &at[*key];
    }
    at.as_str().unwrap_or("").to_string()
}

fn failures(found: Vec<Option<String>>) -> Vec<String> {
    found.into_iter().flatten().collect()
}

/* the canonical flow prints its named reports and writes the csv in a file */
fn the_flow_writes_each_report_to_the_channel_its_name_holds(records: &[String]) -> Option<String> {
    let dir = project("canonical", &over("events.jsonl"), &["files:events.jsonl"], records);
    let (out, err, code) = run(&dir, &["run", "main.flow.yaml"]);
    if (err.as_str(), code) != ("", 0) {
        return Some(format!("the canonical flow failed with {err:?} and exit {code}"));
    }
    let channels = printed(&out);
    let named: Vec<&str> = REPORTS.iter().copied().filter(|name| !name.ends_with(".csv")).collect();
    let names: Vec<&str> = channels.iter().map(|(name, _)| name.as_str()).collect();
    if names != named {
        return Some(format!("the run printed the channels {names:?} and not {named:?}"));
    }
    let lines = |name: &str| channels.iter().find(|(seen, _)| seen == name).map(|(_, lines)| lines.clone()).unwrap_or_default();
    // the slice holds the rows each report must write and the test counts them itself
    let events = kept(&records.join("\n"));
    let mut stars: BTreeMap<String, usize> = BTreeMap::new();
    let mut opened: BTreeSet<String> = BTreeSet::new();
    let mut minutes: BTreeSet<(String, String)> = BTreeSet::new();
    for event in &events {
        let kind = text(event, &["type"]);
        let repo = text(event, &["repo", "name"]);
        minutes.insert((text(event, &["created_at"])[11..16].to_string(), kind.clone()));
        if kind == "WatchEvent" {
            *stars.entry(repo.clone()).or_default() += 1;
        }
        if kind == "IssuesEvent" && text(event, &["payload", "action"]) == "opened" {
            opened.insert(repo);
        }
    }
    let rows = |name: &str| lines(name).len();
    // top keeps ten rows and a count has one row per key it counted
    let want = [("top_stars", 10), ("issues_opened", opened.len())];
    if let Some((name, n)) = want.iter().find(|(name, n)| rows(name) != *n) {
        return Some(format!("{name} wrote {} rows and not {n}", rows(name)));
    }
    let greatest = stars.values().copied().max().unwrap_or(0);
    let first = lines("top_stars").first().cloned().unwrap_or_default();
    let top: serde_json::Value = serde_json::from_str(&first).unwrap_or(serde_json::Value::Null);
    if top["count"].as_u64() != Some(greatest as u64) {
        return Some(format!("top_stars led with {first} and the slice's greatest count is {greatest}"));
    }
    if let Some(row) = lines("busy_branches").iter().find(|row| serde_json::from_str::<serde_json::Value>(row).map(|r| r["count"].as_u64().unwrap_or(0) < 2).unwrap_or(true)) {
        return Some(format!("busy_branches kept the row {row} which pushed once"));
    }
    // the csv channel writes a file beside the flow headed by its keys
    let csv = std::fs::read_to_string(dir.join("activity.csv")).unwrap_or_default();
    let lines: Vec<&str> = csv.lines().collect();
    if lines.first() != Some(&"minute,type,count") || lines.len() != minutes.len() + 1 {
        return Some(format!("activity.csv holds {} lines under {:?} and the slice has {} keys", lines.len(), lines.first(), minutes.len()));
    }
    None
}

/* a report that fails reads in the runtime's words naming its key and line */
fn an_error_reads_in_the_runtimes_words(records: &[String]) -> Option<String> {
    let flow = "from: events.jsonl\nodd:\n  count: n = type + 1\n";
    let dir = project("words", flow, &["files:events.jsonl"], records);
    let (_, err, code) = run(&dir, &["run", "main.flow.yaml"]);
    let want = "report odd, count at line 3: WrongType, cannot add text and number\n";
    match (err.as_str(), code) {
        (seen, 1) if seen == want => None,
        (seen, code) => Some(format!("the failed flow said {seen:?} with exit {code} and not {want:?}")),
    }
}

/* a path no grant names stops the flow at its source in the runtime's words */
fn a_denied_path_reads_in_the_runtimes_words(records: &[String]) -> Option<String> {
    let dir = project("denied", &over("events.jsonl"), &["files:other.jsonl"], records);
    let (out, err, code) = run(&dir, &["run", "main.flow.yaml"]);
    let want = "from at line 1: Denied, 'main' has no files:events.jsonl, edge.json grants it files:other.jsonl\n";
    match (out.as_str(), err.as_str(), code) {
        ("", seen, 1) if seen == want => None,
        (out, seen, code) => Some(format!("the denied flow printed {out:?} and said {seen:?} with exit {code}, not {want:?}")),
    }
}

/* a channel naming a path above the run is refused and writes nothing */
fn a_channel_above_the_run_is_refused(records: &[String]) -> Option<String> {
    let flow = "from: events.jsonl\n../stray.csv:\n  count: type\n";
    let dir = project("stray", flow, &["files:events.jsonl"], records);
    let (_, err, code) = run(&dir, &["run", "main.flow.yaml"]);
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
fn a_channel_through_a_link_out_of_the_run_is_refused(records: &[String]) -> Option<String> {
    let out = "leads through a link out of the directory of the run";
    let nothing = "is a link to nothing, a channel follows no link it cannot resolve";
    // links to a file above the run and to the directory above and to nothing
    for (name, link, to, report, why) in [("linked", "link.csv", "../linked.csv", "link.csv", out), ("linked-dir", "up", "..", "up/escaped.csv", out), ("dangling", "gone.csv", "../gone.csv", "gone.csv", nothing)] {
        let flow = format!("from: events.jsonl\n{report}:\n  count: type\n");
        let dir = project(name, &flow, &["files:events.jsonl"], records);
        let above = dir.parent().expect("a project sits in a directory");
        std::fs::write(above.join("linked.csv"), "untouched\n").expect("writing the file above the run");
        for stray in ["escaped.csv", "gone.csv"] {
            let _ = std::fs::remove_file(above.join(stray));
        }
        std::os::unix::fs::symlink(to, dir.join(link)).expect("linking out of the run");
        let (_, err, code) = run(&dir, &["run", "main.flow.yaml"]);
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

/* refuses a flow runtime older than its sources so no run reads a stale one */
fn fresh() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let built = std::fs::metadata(repo.join("target/wasm32-unknown-unknown/release/tiny.wasm")).and_then(|m| m.modified());
    let Ok(built) = built else { panic!("tiny.wasm is missing, run make wasm-tiny") };
    let mut dirs = vec![repo.join("src"), repo.join("lib/src"), repo.join("examples/tiny/src")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let (path, meta) = (entry.path(), entry.metadata());
            match meta {
                Ok(meta) if meta.is_dir() => dirs.push(path),
                Ok(meta) if meta.modified().is_ok_and(|at| at > built) => panic!("tiny.wasm is older than {}, run make wasm-tiny", path.display()),
                _ => {}
            }
        }
    }
}

fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit().main()
    }
    fresh();
    // the slice of the pinned hour fetched once into the suite's own temp dir
    let hour = data::cache(Path::new(env!("CARGO_TARGET_TMPDIR")));
    let slice = data::slice(&hour);
    let records: Vec<String> = String::from_utf8(slice).expect("the slice is utf-8").lines().map(String::from).collect();
    let mut found = vec![
        the_flow_writes_each_report_to_the_channel_its_name_holds(&records),
        an_error_reads_in_the_runtimes_words(&records),
        a_denied_path_reads_in_the_runtimes_words(&records),
        a_channel_above_the_run_is_refused(&records),
    ];
    #[cfg(unix)]
    found.push(a_channel_through_a_link_out_of_the_run_is_refused(&records));
    let ran = found.len();
    let failed = failures(found);
    if !failed.is_empty() {
        eprintln!("{} of {ran} runs differ\n{}", failed.len(), failed.join("\n"));
        std::process::exit(1);
    }
    println!("{ran} runs of the flow CLI passed");
}
