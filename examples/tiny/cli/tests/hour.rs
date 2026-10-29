use cli::host::{Calls, Fault, Held, HostModule, Reply, Serve, WireValue};
use flate2::bufread::GzDecoder;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tiny_cli::kit;

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "TINY_AS_CLI";

// the file the test process answers the hour's url from
const HOUR_FILE: &str = "TINY_HOUR";

// the job and the grant the docs run in examples/data
const FLOW: &str = include_str!("../../../data/github-hour.flow.yaml");
const GRANT: &str = include_str!("../../../data/edge.json");

// the reports the flow prints in key order and the one it writes
const PRINTED: [&str; 4] = ["top_stars", "busy_branches", "issues_opened", "time_to_close"];
const CSV: &str = "activity.csv";

fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit(Arc::new(Replay)).main()
    }
    // the hour fetched once into the engine's target as tiny's own cases keep it
    let cache = data::cache(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/tmp"));
    let gz = data::hour(&cache);
    let hour = cache.join(data::HOUR.rsplit('/').next().unwrap_or_default());
    let want = Reports::of(&data::inflate(&gz));
    let failed: Vec<String> = [
        the_docs_flow_prints_its_reports_over_the_hour(&hour, &want),
        the_hour_reads_from_its_file_through_files(&hour, &gz, &want),
        a_grant_naming_no_host_stops_the_flow_at_from(&hour),
        test_runs_each_flow_and_fails_the_one_that_raises(&hour),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !failed.is_empty() {
        eprintln!("{} of 4 runs differ\n{}", failed.len(), failed.join("\n"));
        std::process::exit(1);
    }
    println!("4 runs of the tiny CLI passed");
}

/* the docs flow runs unchanged with its url answered from the hour */
fn the_docs_flow_prints_its_reports_over_the_hour(hour: &Path, want: &Reports) -> Option<String> {
    let dir = project("docs", &[("github-hour.flow.yaml", FLOW.as_bytes()), ("edge.json", GRANT.as_bytes())]);
    let (out, err, code) = run(&dir, &["run", "github-hour.flow.yaml"], hour);
    if (err.as_str(), code) != ("", 0) {
        return Some(format!("the docs flow failed with {err:?} and exit {code}"));
    }
    want.differ(&out, &dir).map(|why| format!("the docs flow {why}"))
}

/* the same flow reads the hour's file through files once a grant names it */
fn the_hour_reads_from_its_file_through_files(hour: &Path, gz: &[u8], want: &Reports) -> Option<String> {
    let name = hour.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let (_, rest) = FLOW.split_once('\n').expect("the flow starts with its source line");
    let flow = format!("from: {name}\n{rest}");
    let grant = format!("{{ \"permissions\": {{ \"main\": [\"files:{name}\"] }} }}\n");
    let dir = project("files", &[("main.flow.yaml", flow.as_bytes()), ("edge.json", grant.as_bytes()), (name, gz)]);
    let (out, err, code) = run(&dir, &["run", "main.flow.yaml"], Path::new("no-such-hour"));
    if (err.as_str(), code) != ("", 0) {
        return Some(format!("the flow over the file failed with {err:?} and exit {code}"));
    }
    want.differ(&out, &dir).map(|why| format!("the flow over the file {why}"))
}

/* a grant of net that names no host refuses the request before a byte leaves */
fn a_grant_naming_no_host_stops_the_flow_at_from(hour: &Path) -> Option<String> {
    let dir = project("denied", &[("github-hour.flow.yaml", FLOW.as_bytes()), ("edge.json", b"{ \"permissions\": { \"main\": [\"net\"] } }\n")]);
    let (out, err, code) = run(&dir, &["run", "github-hour.flow.yaml"], hour);
    let words = "from at line 1: Denied, 'main' has no net:data.gharchive.org, edge.json grants it nothing\n";
    match (out.as_str(), err.as_str(), code) {
        ("", seen, 1) if seen == words => None,
        (out, err, code) => Some(format!("the flow without a host printed {out:?} and said {err:?} with exit {code}, not {words:?}")),
    }
}

/* test runs each flow and the one summing a text fails in tiny's words */
fn test_runs_each_flow_and_fails_the_one_that_raises(hour: &Path) -> Option<String> {
    let events = b"{\"type\": \"PushEvent\", \"size\": 3}\n{\"type\": \"PushEvent\", \"size\": \"four\"}\n";
    let dir = project("suite", &[
        ("edge.json", b"{ \"permissions\": { \"main\": [\"files:events.jsonl\"] } }\n"),
        ("events.jsonl", events),
        ("kinds_test.flow.yaml", b"from: events.jsonl\nkinds:\n  count: type\n"),
        ("sizes_test.flow.yaml", b"from: events.jsonl\nsizes:\n  sum: size\n"),
    ]);
    let (out, err, code) = run(&dir, &["test"], hour);
    let verdicts = ["  (successful) kinds_test.flow.yaml\n", "  (unsuccessful) sizes_test.flow.yaml\n", "  1/2 files passed"];
    let words = "report sizes, sum at line 3: WrongType";
    match verdicts.iter().all(|verdict| out.contains(verdict)) && err.contains(words) && code == 1 {
        true => None,
        false => Some(format!("test printed {out:?} and said {err:?} with exit {code}, not the verdicts {verdicts:?} and {words:?}")),
    }
}

/* a project of its own for one case holding the files it names */
fn project(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("tiny").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the target directory takes a project");
    for (file, body) in files {
        std::fs::write(dir.join(file), body).expect("writing a file of the project");
    }
    dir
}

/* one run of the CLI in `dir` with its stdout and stderr and exit code */
fn run(dir: &Path, args: &[&str], hour: &Path) -> (String, String, i32) {
    let out = Command::new(std::env::current_exe().expect("the test binary runs from a path"))
        .args(args)
        .current_dir(dir)
        .env(AS_CLI, "1")
        .env(HOUR_FILE, hour)
        .env("XDG_CACHE_HOME", Path::new(env!("CARGO_TARGET_TMPDIR")).join("tiny").join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("the test binary runs as the CLI");
    (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
}

/* the rows each report holds over the hour by the flow's own keys */
struct Reports {
    printed: Vec<(&'static str, Vec<Value>)>,
    csv: Vec<String>,
}

/* counts by key in the order keys first appear as a report keeps them */
#[derive(Default)]
struct Counts {
    keys: Vec<Vec<String>>,
    at: HashMap<Vec<String>, usize>,
    n: Vec<u64>,
    sums: Vec<f64>,
}

impl Counts {
    fn add(&mut self, key: Vec<String>, measure: f64) {
        let at = *self.at.entry(key.clone()).or_insert_with(|| {
            self.keys.push(key);
            self.n.push(0);
            self.sums.push(0.0);
            self.keys.len() - 1
        });
        self.n[at] += 1;
        self.sums[at] += measure;
    }
}

impl Reports {
    /* the reports of the flow over `lines` read as the flow names each */
    fn of(lines: &[u8]) -> Reports {
        let (mut stars, mut branches, mut minutes, mut opened, mut closed) = (Counts::default(), Counts::default(), Counts::default(), Counts::default(), Counts::default());
        for line in lines.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
            let event: Value = serde_json::from_slice(line).expect("an event of the hour reads");
            // the top where leaves the events of bots out of every report
            if text(&event, &["actor", "login"]).ends_with("[bot]") {
                continue;
            }
            let (kind, repo) = (text(&event, &["type"]), text(&event, &["repo", "name"]));
            minutes.add(vec![text(&event, &["created_at"])[11..16].to_string(), kind.clone()], 0.0);
            match (kind.as_str(), text(&event, &["payload", "action"]).as_str()) {
                ("WatchEvent", _) => stars.add(vec![repo], 0.0),
                ("PushEvent", _) => {
                    let branch = text(&event, &["payload", "ref"]);
                    let branch = branch.strip_prefix("refs/heads/").unwrap_or(&branch).to_string();
                    branches.add(vec![repo, branch], 0.0);
                }
                ("IssuesEvent", "opened") => opened.add(vec![repo], 0.0),
                ("IssuesEvent", "closed") => {
                    let hours = (seconds(&text(&event, &["payload", "issue", "closed_at"])) - seconds(&text(&event, &["payload", "issue", "created_at"]))) as f64 / 3600.0;
                    closed.add(vec![repo], hours);
                }
                _ => {}
            }
        }
        let counted = |c: &Counts, at: usize| json!({ "repo": { "name": c.keys[at][0] }, "count": c.n[at] });
        // top keeps the ten greatest counts with ties in the order their keys first appeared
        let mut ranked: Vec<usize> = (0..stars.keys.len()).collect();
        ranked.sort_by_key(|&at| std::cmp::Reverse(stars.n[at]));
        let top = ranked.into_iter().take(10).map(|at| counted(&stars, at)).collect();
        let busy = (0..branches.keys.len()).filter(|&at| branches.n[at] > 1).map(|at| json!({ "repo": { "name": branches.keys[at][0] }, "branch": branches.keys[at][1], "count": branches.n[at] })).collect();
        let issues = (0..opened.keys.len()).map(|at| counted(&opened, at)).collect();
        let averages = (0..closed.keys.len()).map(|at| json!({ "repo": { "name": closed.keys[at][0] }, "average": closed.sums[at] / closed.n[at] as f64 })).collect();
        let mut csv = vec![String::from("minute,type,count")];
        csv.extend((0..minutes.keys.len()).map(|at| format!("{},{},{}", minutes.keys[at][0], minutes.keys[at][1], minutes.n[at])));
        Reports { printed: vec![("top_stars", top), ("busy_branches", busy), ("issues_opened", issues), ("time_to_close", averages)], csv }
    }

    /* why a run in `dir` printed or wrote other rows or none */
    fn differ(&self, out: &str, dir: &Path) -> Option<String> {
        let mut seen: Vec<(String, Vec<Value>)> = Vec::new();
        for line in out.lines() {
            match PRINTED.contains(&line) {
                true => seen.push((line.to_string(), Vec::new())),
                false => match (seen.last_mut(), serde_json::from_str::<Value>(line)) {
                    (Some((_, rows)), Ok(row)) => rows.push(row),
                    _ => return Some(format!("printed {line:?} which is no row of a report")),
                },
            }
        }
        let names: Vec<&str> = seen.iter().map(|(name, _)| name.as_str()).collect();
        if names != PRINTED {
            return Some(format!("printed the reports {names:?} and not {PRINTED:?}"));
        }
        for ((name, rows), (_, want)) in seen.iter().zip(&self.printed) {
            if rows.len() != want.len() {
                return Some(format!("printed {} rows of {name} and the hour holds {}", rows.len(), want.len()));
            }
            if let Some((row, wanted)) = rows.iter().zip(want).find(|(row, wanted)| !same(row, wanted)) {
                return Some(format!("printed the row {row} of {name} where the hour gives {wanted}"));
            }
        }
        let csv = std::fs::read_to_string(dir.join(CSV)).unwrap_or_default();
        let lines: Vec<&str> = csv.lines().collect();
        match lines == self.csv {
            true => None,
            false => Some(format!("wrote {} lines to {CSV} starting {:?} and the hour gives {} starting {:?}", lines.len(), lines.first(), self.csv.len(), self.csv.first())),
        }
    }
}

/* whether two rows agree where an average may differ in its last bits */
fn same(row: &Value, want: &Value) -> bool {
    match (row.get("average").and_then(Value::as_f64), want.get("average").and_then(Value::as_f64)) {
        (Some(a), Some(b)) => row["repo"] == want["repo"] && (a - b).abs() <= 1e-9 * b.abs().max(1.0),
        _ => row == want,
    }
}

/* the text at `path` in an event or an empty one when missing */
fn text(event: &Value, path: &[&str]) -> String {
    path.iter().fold(event, |at, key| &at[*key]).as_str().unwrap_or_default().to_string()
}

/* the seconds since the epoch of a time as GH Archive writes it */
fn seconds(time: &str) -> i64 {
    let part = |range: std::ops::Range<usize>| time.get(range).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let (year, month, day) = (part(0..4), part(5..7), part(8..10));
    // days since 1970 by the civil calendar whose year starts in march
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + part(11..13) * 3600 + part(14..16) * 60 + part(17..19)
}

/* net answered in this process with the hour read from the file the test names */
struct Replay;

impl HostModule for Replay {
    fn name(&self) -> &str {
        "net"
    }

    // a lowercase host as net's own scopes read
    fn scope(&self, scope: &str) -> bool {
        !scope.is_empty() && scope.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-./".contains(&b))
    }

    // the replay checks each request against its grant itself
    fn reaches(&self, _call: &str, _args: &[WireValue]) -> Result<Option<String>, Fault> {
        Ok(None)
    }

    fn open(&self, inst: &mut dyn Serve, spec: &str, _pkg: &str, held: Held) -> Result<(), String> {
        inst.serve(spec, "net", &["request", "response", "read", "close"], Box::new(Bodies { held, open: HashMap::new(), next: 1 }))
    }
}

/* each request of one package as the reader of its body */
struct Bodies {
    held: Held,
    open: HashMap<i64, Box<dyn Read + Send>>,
    next: i64,
}

// how much of a body one read hands over as net hands over a chunk
const CHUNK: usize = 64 << 10;

impl Calls for Bodies {
    fn call(&mut self, index: usize, args: Vec<WireValue>, reply: Reply) {
        let id = match args.first() {
            Some(WireValue::Int(id)) if index > 0 => *id as i64,
            _ => 0,
        };
        match index {
            0 => reply.answer(self.request(args.get(1), args.get(4))),
            1 => reply.later(match self.open.contains_key(&id) {
                true => Ok(WireValue::List(vec![WireValue::Int(200), WireValue::List(Vec::new())])),
                false => Err(Fault::invalid(format!("no open request {id}"))),
            }),
            2 => reply.later(self.read(id)),
            _ => {
                self.open.remove(&id);
                reply.ok(WireValue::None)
            }
        }
    }
}

impl Bodies {
    /* the id of a request the grant reaches whose body reads from the hour's file */
    fn request(&mut self, url: Option<&WireValue>, inflate: Option<&WireValue>) -> Result<WireValue, Fault> {
        let Some(WireValue::Bytes(url)) = url else { return Err(Fault::invalid("a url must be a str")) };
        let url = String::from_utf8_lossy(url).into_owned();
        let host = url.strip_prefix("https://").and_then(|rest| rest.split('/').next()).ok_or_else(|| Fault::invalid(format!("'{url}' is not an https url")))?;
        if self.held.scopes().is_some_and(|scopes| !scopes.iter().any(|scope| scope == host)) {
            self.held.need(host)?;
        }
        if url != data::HOUR {
            return Err(Fault::failed(format!("net.request to {url} failed, the test answers only {}", data::HOUR)));
        }
        let path = std::env::var_os(HOUR_FILE).unwrap_or_default();
        let file = std::fs::File::open(&path).map_err(|e| Fault::failed(format!("net.request to {url} failed, the hour's file reads {e}")))?;
        let body: Box<dyn Read + Send> = match inflate {
            Some(WireValue::Bytes(how)) if how == b"gzip" => Box::new(GzDecoder::new(BufReader::new(file))),
            _ => Box::new(file),
        };
        let id = self.next;
        self.next += 1;
        self.open.insert(id, body);
        Ok(WireValue::Int(id.into()))
    }

    /* the next chunk of a body or None at its end */
    fn read(&mut self, id: i64) -> Result<WireValue, Fault> {
        let body = self.open.get_mut(&id).ok_or_else(|| Fault::invalid(format!("no open request {id}")))?;
        let mut chunk = vec![0u8; CHUNK];
        let n = body.read(&mut chunk).map_err(|e| Fault::failed(format!("reading the hour failed, {e}")))?;
        chunk.truncate(n);
        Ok(if n == 0 { WireValue::None } else { WireValue::Raw(chunk) })
    }
}
