mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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
    #[serde(default)] todo: Option<String>,
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

/// third party wasm plugins built from `plugin/example` run the way a project declares them
#[test]
fn plugin_suite() {
    suite(include_str!("plugins.json"), &[&[]]);
}

/// Programs both hosts must run alike, each case natively and again in the browser, which needs a Chrome.
#[test]
fn hosts_suite() {
    suite(include_str!("hosts.json"), &[&[], &["--web"]]);
}

/// the net cases on both hosts against one loopback fixture which needs a Chrome
#[test]
fn net_suite() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let at = listener.local_addr().expect("tcp addr");
    // the fixture the cases name is the one just bound
    let text = include_str!("../../examples/net/cases.json").replace("$FIXTURE", &format!("http://{at}")).replace("$SOCKET", &format!("ws://{at}"));
    let file: NetCases = serde_json::from_str(&text).expect("cases.json parse");
    std::thread::spawn(move || {
        let routes = std::sync::Arc::new(file.routes);
        for stream in listener.incoming().flatten() {
            let routes = routes.clone();
            std::thread::spawn(move || answer(stream, &routes));
        }
    });
    run_suite(&file.cases, &[&[], &["--web"]]);
}

/* the net cases with the canned answers of the fixture they reach */
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetCases {
    routes: BTreeMap<String, Route>,
    cases: Vec<Case>,
}

/* what the fixture answers on one path and the paths below it */
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    #[serde(default)] status: Option<u16>,
    #[serde(default)] headers: Vec<(String, String)>,
    #[serde(default)] body: Option<String>,
    // a text and how many times the body repeats it
    #[serde(default)] repeat: Option<(String, usize)>,
    // each text a gzip stream of its own one after another
    #[serde(default)] gzip: Option<Vec<String>>,
    // the method target and these request headers then the request body
    #[serde(default)] echo: Option<Vec<String>>,
    // a socket echoing what it hears until told to close
    #[serde(default)] socket: Option<String>,
    // milliseconds the fixture waits before it answers
    #[serde(default)] delay: Option<u64>,
    // texts the body sends one at a time with `gap` milliseconds between them
    #[serde(default)] parts: Option<Vec<String>>,
    #[serde(default)] gap: Option<u64>,
}

/* one request to the fixture answered from its route or with a 404 */
fn answer(mut stream: TcpStream, routes: &BTreeMap<String, Route>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let header = |name: &str| headers.iter().find(|(seen, _)| seen == name).map(|(_, value)| value.as_str());
    let mut body = vec![0u8; header("content-length").and_then(|n| n.parse().ok()).unwrap_or(0)];
    let _ = reader.read_exact(&mut body);
    // a page on another port reads it too so every answer allows any origin
    let open = "access-control-allow-origin: *\r\naccess-control-expose-headers: *\r\ncache-control: no-store\r\nconnection: close\r\n";
    if method == "OPTIONS" {
        let _ = stream.write_all(format!("HTTP/1.1 204 No Content\r\n{open}access-control-allow-methods: *\r\naccess-control-allow-headers: *\r\ncontent-length: 0\r\n\r\n").as_bytes());
        return;
    }
    let path = target.split('?').next().unwrap_or("/");
    let found = routes.iter().filter(|(at, _)| path == at.as_str() || path.strip_prefix(at.as_str()).is_some_and(|rest| rest.starts_with('/'))).max_by_key(|(at, _)| at.len());
    let Some((_, route)) = found else {
        let _ = stream.write_all(format!("HTTP/1.1 404 Not Found\r\n{open}content-length: 0\r\n\r\n").as_bytes());
        return;
    };
    if let Some(ms) = route.delay {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    if route.socket.is_some() && header("upgrade").is_some_and(|up| up.eq_ignore_ascii_case("websocket")) {
        return echo_socket(stream, header("sec-websocket-key").unwrap_or_default());
    }
    let sent: Vec<u8> = match route {
        Route { body: Some(text), .. } => text.clone().into_bytes(),
        Route { repeat: Some((text, times)), .. } => text.repeat(*times).into_bytes(),
        Route { gzip: Some(members), .. } => members.iter().flat_map(|member| {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = encoder.write_all(member.as_bytes());
            encoder.finish().unwrap_or_default()
        }).collect(),
        Route { echo: Some(names), .. } => {
            let mut echoed = format!("{method} {target}\n");
            for name in names {
                if let Some(value) = header(name) {
                    echoed.push_str(&format!("{name}: {value}\n"));
                }
            }
            let mut echoed = echoed.into_bytes();
            echoed.extend_from_slice(&body);
            echoed
        }
        Route { parts: Some(parts), .. } => parts.concat().into_bytes(),
        _ => Vec::new(),
    };
    let status = route.status.unwrap_or(200);
    let named: String = route.headers.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
    let mut out = format!("HTTP/1.1 {status} Fixture\r\n{open}{named}content-length: {}\r\n\r\n", sent.len()).into_bytes();
    // the head leaves with the first part and each later part waits the gap
    let parts: Vec<&[u8]> = route.parts.as_ref().map_or_else(|| vec![sent.as_slice()], |parts| parts.iter().map(|part| part.as_bytes()).collect());
    for (at, part) in parts.into_iter().enumerate() {
        if at > 0 {
            std::thread::sleep(std::time::Duration::from_millis(route.gap.unwrap_or(0)));
        }
        out.extend_from_slice(part);
        if stream.write_all(&out).is_err() {
            return;
        }
        out.clear();
    }
}

/* a socket that echoes each text and bytes it hears and closes when told to */
fn echo_socket(mut stream: TcpStream, key: &str) {
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let _ = stream.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n").as_bytes());
    let mut socket = tungstenite::WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);
    while let Ok(message) = socket.read() {
        let sent = match message {
            tungstenite::Message::Text(text) if text.as_str() == "close" => socket.close(None),
            tungstenite::Message::Text(text) => socket.send(tungstenite::Message::text(format!("echo {}", text.as_str()))),
            tungstenite::Message::Binary(bytes) => socket.send(tungstenite::Message::binary(bytes)),
            _ => Ok(()),
        };
        if sent.is_err() {
            break;
        }
    }
}

fn suite(json: &str, hosts: &[&[&str]]) {
    let cases: Vec<Case> = serde_json::from_str(json).expect("case file parse");
    run_suite(&cases, hosts);
}

fn run_suite(cases: &[Case], hosts: &[&[&str]]) {
    let bin = env!("CARGO_BIN_EXE_edge");
    // One cache per suite, so no case touches the real one and the runtime downloads once.
    let cache = tempfile::tempdir().expect("suite cache dir");
    let mut failed = vec![];
    // TODO remove the package publication path and every case marked todo
    for c in cases.iter().filter(|c| c.todo.is_none()) {
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
        run(&s.bin, &s.run, &s.env, dir.path(), cache, "", want).map_err(|e| format!("[then {}] {e}", s.bin))?;
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
