use cli::host::{Calls, Fault, Held, HostModule, Reply, Serve, WireValue};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use uptime_cli::{kit, Clock};

// the variable that makes this binary the CLI for one run of a case
const AS_CLI: &str = "UPTIME_AS_CLI";

// a host whose answers a case leaves unrecorded so its requests fail
const DOWN: &str = "UPTIME_DOWN";

// the bodies the five URLs of the checks answered on 23 October 2026
const ANSWERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/answers");

/* one CLI run on a copy of the checks with its output and exit code */
struct Case {
    name: &'static str,
    args: &'static [&'static str],
    // the grant the copy's edge.json loses
    ungrant: Option<&'static str>,
    down: Option<&'static str>,
    // one more check written beside the copied ones
    wrote: Option<(&'static str, &'static str)>,
    // the example plugin copied beside the checks and imported as slugify_mod
    plugin: bool,
    stdout: &'static str,
    stderr: &'static str,
    code: i32,
}

const CASES: &[Case] = &[
    Case {
        name: "the board prints each service and exits with how many are degraded",
        args: &["run", "status.up"],
        ungrant: None,
        down: None,
        wrote: None,
        plugin: false,
        stdout: "github      200   143 ms  All Systems Operational\nnpm         200   143 ms  All Systems Operational\ncloudflare  200   143 ms  Partial System Outage\n",
        stderr: "",
        code: 1,
    },
    Case {
        name: "the tests pass but cloudflare whose page reports an outage",
        args: &["test"],
        ungrant: None,
        down: None,
        wrote: None,
        plugin: false,
        stdout: "  (unsuccessful) cloudflare_test.up\n  (successful) github_test.up\n  (successful) npm_test.up\n\n  2/3 files passed · 0.0s\n",
        stderr: "error: CheckFailed: Cloudflare reports Partial System Outage\n  --> ./cloudflare_test.up:4:1\n  |\n4 | check status.indicator == \"none\", \"Cloudflare reports {status.description}\"\n  | ^\n\n",
        code: 1,
    },
    Case {
        name: "a host edge.json does not grant is refused before its request leaves",
        args: &["run", "status.up"],
        ungrant: Some("net:www.cloudflarestatus.com"),
        down: None,
        wrote: None,
        plugin: false,
        stdout: "github      200   143 ms  All Systems Operational\nnpm         200   143 ms  All Systems Operational\n",
        stderr: "error: Denied: 'main' has no net:www.cloudflarestatus.com, edge.json grants it net:www.githubstatus.com, net:status.npmjs.org, net:registry.npmjs.org\n   --> status.up:12:5\n   |\n12 |     let page = http.get(url)\n   |     ^\n\n",
        code: 1,
    },
    Case {
        name: "a check that reads the clock without its grant never starts",
        args: &["run", "status.up"],
        ungrant: Some("clock"),
        down: None,
        wrote: None,
        plugin: false,
        stdout: "",
        stderr: "error: module './http.up' parse error: 'main' imports clock, which edge.json does not grant it\n  --> status.up:1:5\n  |\n1 | use .http\n  |     ^^^^^\n\n",
        code: 1,
    },
    Case {
        name: "a service that does not answer reads as down and the board goes on",
        args: &["run", "status.up"],
        ungrant: None,
        down: Some("status.npmjs.org"),
        wrote: None,
        plugin: false,
        stdout: "github      200   143 ms  All Systems Operational\nnpm         down  net.request to https://status.npmjs.org/api/v2/status.json failed, no answer was recorded\ncloudflare  200   143 ms  Partial System Outage\n",
        stderr: "",
        code: 2,
    },
    Case {
        name: "a check that adds a text to a number fails in the classes this runtime names",
        args: &["run", "broken.up"],
        ungrant: None,
        down: None,
        wrote: Some(("broken.up", "let total = 1 + \"two\"\nprint(total)\n")),
        plugin: false,
        stdout: "",
        stderr: "error: WrongType: + does not apply to int and text\n  --> broken.up:1:1\n  |\n1 | let total = 1 + \"two\"\n  | ^\n\n",
        code: 1,
    },
    Case {
        name: "a plugin iterates the list a check hands it and reads its keyword in this runtime",
        args: &["run", "slugs.up"],
        ungrant: None,
        down: None,
        wrote: Some(("slugs.up", "use slugify_mod\n\nlet services = [\"GitHub Status\", \"npm Registry\", \"Cloudflare Status\"]\nprint(slugify_mod.slug_all(services))\nprint(slugify_mod.slug_all(services, sep=\"_\"))\nprint(slugify_mod.slug_all(services, sep=nil))\n")),
        plugin: true,
        stdout: "github-status-npm-registry-cloudflare-status\ngithub-status_npm-registry_cloudflare-status\ngithub-status-npm-registry-cloudflare-status\n",
        stderr: "",
        code: 0,
    },
    Case {
        name: "the chunks a later answer hands a check count against its memory limit",
        args: &["run", "--memory", "1", "keep.up"],
        ungrant: None,
        down: None,
        wrote: Some(("keep.up", "use net\n\nlet kept = {}\nlet n = 0\nwhile n < 400\n  let id = net.request(\"GET\", \"https://www.githubstatus.com/api/v2/components.json\")\n  let chunk = net.read(id)\n  while chunk != nil\n    kept[size(kept)] = chunk\n    chunk = net.read(id)\n  end\n  n += 1\nend\nprint(size(kept))\n")),
        plugin: false,
        stdout: "",
        stderr: "error: OutOfBudget: memory limit\n   --> keep.up:10:5\n   |\n10 |     chunk = net.read(id)\n   |     ^\n\n",
        code: 1,
    },
];

fn main() {
    if std::env::var_os(AS_CLI).is_some() {
        kit(Arc::new(Recorded), Arc::new(Clock(ticks))).main()
    }
    let checks = Path::new(env!("CARGO_MANIFEST_DIR")).join("../checks");
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join("uptime");
    // the plugin make plugin builds into the engine's target
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/wasm32-unknown-unknown/release/slugify_mod.wasm");
    let mut failed = Vec::new();
    for (i, case) in CASES.iter().enumerate() {
        let dir = tmp.join(i.to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the target directory takes a copy of the checks");
        for entry in std::fs::read_dir(&checks).expect("examples/uptime/checks reads").flatten() {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).expect("copying a check");
        }
        if let Some((name, body)) = case.wrote {
            std::fs::write(dir.join(name), body).expect("writing the check a case adds");
        }
        if case.plugin {
            std::fs::copy(&plugin, dir.join("slugify_mod.wasm")).unwrap_or_else(|e| panic!("copying {}: {e}, run make plugin first", plugin.display()));
            let manifest = std::fs::read_to_string(dir.join("edge.json")).expect("the checks hold edge.json");
            let imported = manifest.replacen('{', "{\n  \"imports\": { \"slugify_mod\": \"./slugify_mod.wasm\" },", 1);
            std::fs::write(dir.join("edge.json"), imported).expect("writing the copy's edge.json");
        }
        if let Some(grant) = case.ungrant {
            let manifest = std::fs::read_to_string(dir.join("edge.json")).expect("the checks hold edge.json");
            let quoted = format!("\"{grant}\"");
            let kept = manifest.replace(&format!("{quoted}, "), "").replace(&format!(", {quoted}"), "");
            assert_ne!(kept, manifest, "{}: edge.json grants no {grant}", case.name);
            std::fs::write(dir.join("edge.json"), kept).expect("writing the copy's edge.json");
        }
        let mut cli = Command::new(std::env::current_exe().expect("the test binary runs from a path"));
        cli.args(case.args).current_dir(&dir).env(AS_CLI, "1").env("XDG_CACHE_HOME", tmp.join("cache")).stdin(Stdio::null());
        if let Some(host) = case.down {
            cli.env(DOWN, host);
        }
        let out = cli.output().expect("the test binary runs as the CLI");
        let stdout = timeless(&String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.status.code().unwrap_or(-1);
        if (stdout.as_str(), stderr.as_str(), code) != (case.stdout, case.stderr, case.code) {
            failed.push(format!("{}\n  stdout {stdout:?}\n  stderr {stderr:?}\n  exit {code}", case.name));
        }
    }
    if !failed.is_empty() {
        eprintln!("{} of {} runs differ\n{}", failed.len(), CASES.len(), failed.join("\n"));
        std::process::exit(1);
    }
    println!("{} runs of the uptime CLI passed", CASES.len());
}

/* the summary of a test run with its seconds left out since they vary */
fn timeless(text: &str) -> String {
    match text.rfind(" · ") {
        Some(at) if text.ends_with("s\n") => format!("{} · 0.0s\n", &text[..at]),
        _ => text.to_string(),
    }
}

/* each reading 143 ms after the last so every GET takes 143 ms */
fn ticks() -> u64 {
    static NOW: AtomicU64 = AtomicU64::new(1_792_781_688_000);
    NOW.fetch_add(143, Ordering::Relaxed)
}

/* net answered in this process from the bodies tests/answers holds under each host and path */
struct Recorded;

impl HostModule for Recorded {
    fn name(&self) -> &str {
        "net"
    }

    // a lowercase host with an optional path as net's own scopes read
    fn scope(&self, scope: &str) -> bool {
        !scope.is_empty() && scope.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-./".contains(&b))
    }

    // the replay checks each request against its grant itself
    fn reaches(&self, _call: &str, _args: &[WireValue]) -> Result<Option<String>, Fault> {
        Ok(None)
    }

    fn open(&self, inst: &mut dyn Serve, spec: &str, _pkg: &str, held: Held) -> Result<(), String> {
        inst.serve(spec, "net", &["request", "response", "read", "close"], Box::new(Replay { held, open: HashMap::new(), next: 1 }))
    }
}

/* each request of one package as its body in chunks or its failure */
struct Replay {
    held: Held,
    open: HashMap<i64, Result<Vec<Vec<u8>>, Fault>>,
    next: i64,
}

// how much of a body one read hands over so a body takes several
const CHUNK: usize = 1024;

impl Calls for Replay {
    fn call(&mut self, index: usize, args: Vec<WireValue>, reply: Reply) {
        let id = match args.first() {
            Some(WireValue::Int(id)) if index > 0 => *id as i64,
            _ => 0,
        };
        match index {
            0 => reply.answer(self.request(args.get(1))),
            1 => reply.later(match self.open.get(&id) {
                Some(Ok(_)) => Ok(WireValue::List(vec![WireValue::Int(200), WireValue::List(Vec::new())])),
                Some(Err(fault)) => Err(fault.clone()),
                None => Err(Fault::invalid(format!("no open request or socket {id}"))),
            }),
            2 => reply.later(match self.open.get_mut(&id) {
                Some(Ok(chunks)) if !chunks.is_empty() => Ok(WireValue::Raw(chunks.remove(0))),
                Some(Err(fault)) => Err(fault.clone()),
                _ => Ok(WireValue::None),
            }),
            _ => {
                self.open.remove(&id);
                reply.ok(WireValue::None)
            }
        }
    }
}

impl Replay {
    /* the id of a request the grant reaches whose answer reads from disk */
    fn request(&mut self, url: Option<&WireValue>) -> Result<WireValue, Fault> {
        let Some(WireValue::Bytes(url)) = url else { return Err(Fault::invalid("a url must be a str")) };
        let url = String::from_utf8_lossy(url).into_owned();
        let rest = url.strip_prefix("https://").ok_or_else(|| Fault::invalid(format!("'{url}' is not an https url")))?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        reach(&self.held, host, path)?;
        let down = std::env::var(DOWN).is_ok_and(|d| d == host);
        let answer = match std::fs::read(format!("{ANSWERS}/{host}{path}")) {
            Ok(body) if !down => Ok(body.chunks(CHUNK).map(<[u8]>::to_vec).collect()),
            _ => Err(Fault::failed(format!("net.request to {url} failed, no answer was recorded"))),
        };
        let id = self.next;
        self.next += 1;
        self.open.insert(id, answer);
        Ok(WireValue::Int(id.into()))
    }
}

/* refused as net refuses unless a scope names the host or its path prefix */
fn reach(held: &Held, host: &str, path: &str) -> Result<(), Fault> {
    let Some(scopes) = held.scopes() else { return Ok(()) };
    let bounded: Vec<&String> = scopes.iter().filter(|scope| scope.split('/').next() == Some(host)).collect();
    if bounded.iter().any(|scope| *scope == host || path.starts_with(&scope[host.len()..])) {
        return Ok(());
    }
    held.need(&if bounded.is_empty() { host.to_string() } else { format!("{host}{path}") })
}
