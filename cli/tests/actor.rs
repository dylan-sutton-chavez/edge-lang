use std::io::Write;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_edge");

/* The edge binary with a private cache, remote modules resolve through the local CDN. */
fn edge() -> Command {
    let base = std::env::var("EDGE_CDN_BASE").unwrap_or_else(|_| panic!("set EDGE_CDN_BASE (make serve)"));
    let cache = std::env::temp_dir().join(format!("edge-actor-cache-{}", std::process::id()));
    let mut cmd = Command::new(BIN);
    cmd.env("EDGE_CDN_BASE", base).env("XDG_CACHE_HOME", cache);
    cmd
}

// A case file, expect is the assertion, publish and listen drive the live-server cases.
#[derive(serde::Deserialize)]
struct Case {
    #[serde(default)]
    expect: Vec<String>,
    // Lines fed to the ingress after boot, only for cases that listen.
    #[serde(default)]
    publish: Vec<String>,
    // Substring the /stats body must contain, only for cases with a control port.
    #[serde(default)]
    expect_status: Option<String>,
    // POSTs against the control port, each reply body must carry expect as a substring.
    #[serde(default)]
    post: Vec<Post>,
    #[serde(default)]
    runtime: Runtime,
}

// One POST to the control port, path and body go out, expect matches the reply.
#[derive(serde::Deserialize)]
struct Post {
    path: String,
    body: String,
    expect: String,
}

#[derive(serde::Deserialize, Default)]
struct Runtime {
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    control: Option<String>,
}

// Runs every cli/tests/actor/*.yml case, asserting the pool's stdout matches its expect block.
#[test]
fn actor_cases_match_their_expected_output() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/actor");
    let mut failures = Vec::new();
    let mut ran = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        ran += 1;
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let case: Case = serde_yaml_ng::from_str(&text).unwrap();

        let (got, status, replies) = match &case.runtime.listen {
            Some(addr) => run_server(&path, addr, &case),
            None => (run_batch(&path), String::new(), Vec::new()),
        };
        // Order across groups is not fixed, compare as a sorted multiset of lines.
        let (mut got, mut expected) = (got, case.expect.clone());
        got.sort();
        expected.sort();
        if got != expected {
            failures.push(format!("[{name}] output mismatch\n  want {expected:?}\n  got  {got:?}"));
        }
        if let Some(want) = &case.expect_status
            && !status.contains(want.as_str())
        {
            failures.push(format!("[{name}] status missing {want:?}, got {status:?}"));
        }
        for (i, (p, reply)) in case.post.iter().zip(&replies).enumerate() {
            if !reply.contains(p.expect.as_str()) {
                failures.push(format!("[{name}] post #{i} reply missing {:?}, got {reply:?}", p.expect));
            }
        }
    }
    assert!(ran > 0, "no actor cases found");
    assert!(failures.is_empty(), "{} actor case(s) failed:\n{}", failures.len(), failures.join("\n"));
}

// A batch pool runs to completion, its stdout lines are the result.
fn run_batch(path: &std::path::Path) -> Vec<String> {
    let out = edge().args(["actor", path.to_str().unwrap()]).stdin(Stdio::null()).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()
}

// A server pool stays alive, publish feeds the ingress, then stdout, /stats and posts are read.
fn run_server(path: &std::path::Path, listen: &str, case: &Case) -> (Vec<String>, String, Vec<String>) {
    let addr = listen.strip_prefix("tcp://").unwrap_or(listen);
    let scratch = std::env::temp_dir().join(format!("edge-actor-{}-{}", std::process::id(), path.file_stem().unwrap().to_string_lossy()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::copy(path, &manifest).unwrap();
    // The groups resolve actor through the manifest beside the yml, so it travels along.
    std::fs::copy(path.with_file_name("edge.json"), scratch.join("edge.json")).unwrap();

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap();
    if let Some(mut sock) = connect(addr) {
        for line in &case.publish {
            let _ = writeln!(sock, "{line}");
        }
        let _ = sock.flush();
    }
    std::thread::sleep(Duration::from_millis(400));
    let control_addr = case.runtime.control.as_deref().map(|c| c.strip_prefix("tcp://").unwrap_or(c));
    let status = match (control_addr, &case.expect_status) {
        (Some(c), Some(want)) => status_until(c, want),
        (Some(c), None) => get_status(c),
        (None, _) => String::new(),
    };
    let replies = match control_addr {
        Some(c) => case.post.iter().map(|p| post_eval(c, &p.path, &p.body)).collect(),
        None => Vec::new(),
    };
    // A /send is accepted once queued, give the actors a beat to print before the kill.
    std::thread::sleep(Duration::from_millis(400));
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    (String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect(), status, replies)
}

// Connects once the port binds, a debug pool can take seconds to boot, and a read that never answers fails.
fn connect(addr: &str) -> Option<TcpStream> {
    for _ in 0..200 {
        if let Ok(sock) = TcpStream::connect(addr) {
            let _ = sock.set_read_timeout(Some(Duration::from_secs(30)));
            return Some(sock);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// POSTs the body to a control path over a bare HTTP request, returning just the reply body.
fn post_eval(addr: &str, path: &str, body: &str) -> String {
    use std::io::Read;
    let Some(mut sock) = connect(addr) else { return String::new() };
    let req = format!("POST {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let _ = sock.write_all(req.as_bytes());
    let mut resp = String::new();
    let _ = sock.read_to_string(&mut resp);
    resp.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default()
}

/* The bundle wire format `edge build` writes, magic, entry, then length-prefixed files. */
fn bundle(entry: &str, files: &[(&str, &str)]) -> Vec<u8> {
    fn put(b: &mut Vec<u8>, bytes: &[u8]) {
        b.extend_from_slice(bytes.len().to_string().as_bytes());
        b.push(b'\n');
        b.extend_from_slice(bytes);
    }
    let mut b = b"EDGEPKG\x01".to_vec();
    put(&mut b, entry.as_bytes());
    b.extend_from_slice(files.len().to_string().as_bytes());
    b.push(b'\n');
    for (path, content) in files {
        put(&mut b, path.as_bytes());
        put(&mut b, content.as_bytes());
    }
    b
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

/* An untrusted client sends a whole project bundle to an eval group, which runs it in isolation. */
#[test]
fn eval_group_runs_a_bundled_project_over_the_wire() {
    let payload = bundle("main.py", &[
        ("main.py", "import util\nprint(util.hi())\n"),
        ("util.py", "def hi():\n    return \"bundled and run\"\n"),
        ("edge.json", "{ \"imports\": { \"util\": \"./util.py\" } }\n"),
    ]);
    let line = format!("runners EDGEPKG:{}", base64_encode(&payload));

    let scratch = std::env::temp_dir().join(format!("edge-actor-bundle-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7811\ngroups:\n  runners:\n    eval: true\n").unwrap();

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut sock = connect("127.0.0.1:7811").expect("ingress never came up");
    let _ = writeln!(sock, "{line}");
    let _ = sock.flush();
    std::thread::sleep(Duration::from_millis(800));
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    let err = String::from_utf8_lossy(&out.stderr);
    let got: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    assert_eq!(got, vec!["bundled and run"], "stdout was {got:?}, stderr {err:?}");
}

/* A pool that grants eval nothing refuses a bundle that grants itself anything. */
#[test]
fn a_pool_that_grants_eval_nothing_refuses_a_bundle_that_grants() {
    let payload = bundle("main.py", &[
        ("main.py", "import time\nprint(time.now() > 0)\n"),
        ("edge.json", "{ \"permissions\": { \"main\": [\"time:wall\"] } }\n"),
    ]);
    let scratch = std::env::temp_dir().join(format!("edge-actor-grant-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7812\n  control: tcp://127.0.0.1:9812\ngroups:\n  runners:\n    eval: true\n").unwrap();

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let reply = post_eval("127.0.0.1:9812", "/eval/runners", &format!("EDGEPKG:{}", base64_encode(&payload)));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(reply.contains("the pool does not grant eval what this bundle grants") && reply.contains("main   time:wall"), "reply was {reply:?}");
}

/* A bundle holds what it grants within the eval grant of the pool, and a snippet holds nothing. */
#[test]
fn an_eval_bundle_grants_within_what_the_pool_grants_eval() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-ceiling-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7813\n  control: tcp://127.0.0.1:9813\ngroups:\n  runners:\n    eval: true\n").unwrap();
    std::fs::write(scratch.join("edge.json"), r#"{ "permissions": { "main": ["time:wall"], "all": ["time:wall"], "eval": ["time:monotonic"] } }"#).unwrap();
    let granting = |source: &str, grant: &str| {
        let payload = bundle("main.py", &[("main.py", source), ("edge.json", &format!(r#"{{ "permissions": {{ "main": ["{grant}"] }} }}"#))]);
        post_eval("127.0.0.1:9813", "/eval/runners", &format!("EDGEPKG:{}", base64_encode(&payload)))
    };

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let held = granting("import time\nprint(time.now('monotonic') > 0)\n", "time:monotonic");
    let outside = granting("import time\nprint(time.now())\n", "time:monotonic");
    let beyond = granting("import time\nprint(time.now())\n", "time:wall");
    let snippet = post_eval("127.0.0.1:9813", "/eval/runners", "import time\nprint(time.now('monotonic') > 0)\n");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(held.contains(r#"{"ok":true,"stdout":"True\n"}"#), "held was {held:?}");
    assert!(outside.contains("'main' has no time:wall, edge.json grants it time:monotonic"), "outside was {outside:?}");
    assert!(beyond.contains("main   time:wall") && !beyond.contains("monotonic"), "beyond was {beyond:?}");
    assert!(snippet.contains("'main' imports time, which edge.json does not grant it"), "snippet was {snippet:?}");
}

/* Evals that sleep or spin and an actor that never stops computing share one thread, and none of them holds up the rest. */
#[test]
fn a_waiting_or_busy_actor_never_holds_up_the_rest() {
    use std::io::BufRead;
    use std::time::Instant;
    let scratch = std::env::temp_dir().join(format!("edge-actor-share-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7814\n  control: tcp://127.0.0.1:9814\ngroups:\n  runners:\n    eval: true\n    replicas: 3\n  echo:\n    code: print('echo', receive())\n  spin:\n    code: |\n      receive()\n      while True:\n          pass\n").unwrap();
    // A time scope puts a run on the wall clock, without one every sleep passes at once.
    std::fs::write(scratch.join("edge.json"), r#"{ "permissions": { "eval": ["time:monotonic"] } }"#).unwrap();
    let sleeper = |seconds: u32, says: &str| {
        let source = format!("import time\nsleep({seconds})\nprint('{says}')\n");
        let payload = bundle("main.py", &[("main.py", &source), ("edge.json", r#"{ "permissions": { "main": ["time:monotonic"] } }"#)]);
        format!("EDGEPKG:{}", base64_encode(&payload))
    };

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send((Instant::now(), line));
        }
    });
    let mut sock = connect("127.0.0.1:7814").expect("ingress never came up");
    let sent = Instant::now();
    // A snippet holds no grant, so it can only spin, and it spins until its op budget runs out.
    let spinning = "runners while True: pass".to_string();
    for line in [spinning, format!("runners {}", sleeper(3, "first eval")), format!("runners {}", sleeper(3, "second eval")), "spin go".into(), "echo hi".into()] {
        let _ = writeln!(sock, "{line}");
    }
    let _ = sock.flush();
    // Each line with the seconds it took to print, until both evals printed or the wait gives up.
    let mut seen: Vec<(f64, String)> = Vec::new();
    while seen.iter().filter(|(_, l)| l.ends_with("eval")).count() < 2 {
        let Ok((at, line)) = lines.recv_timeout(Duration::from_secs(15)) else { break };
        seen.push((at.duration_since(sent).as_secs_f64(), line));
    }
    let late = Instant::now();
    let timed_out = post_eval("127.0.0.1:9814", "/eval/runners", &sleeper(20, "never"));
    let late = late.elapsed().as_secs_f64();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);

    let at = |want: &str| seen.iter().find(|(_, l)| l == want).map(|(t, _)| *t);
    assert!(at("echo hi").is_some_and(|t| t < 1.5), "an echo waited behind the sleeping evals or the spinning actor, {seen:?}");
    // Run one after the other the two sleeps would take six seconds.
    let both = at("first eval").zip(at("second eval")).map(|(a, b)| a.max(b));
    assert!(both.is_some_and(|t| t < 5.0), "the two evals did not sleep side by side, {seen:?}");
    assert!(timed_out.contains("run exceeded its time limit") && late < 2.0, "a sleep past the deadline answered {timed_out:?} after {late}s");
}

/* An eval group sets its own timeout, which ends a spin and a sleep alike. */
#[test]
fn an_eval_group_timeout_ends_a_spin_and_a_sleep_alike() {
    use std::time::Instant;
    let scratch = std::env::temp_dir().join(format!("edge-actor-timeout-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7815\n  control: tcp://127.0.0.1:9815\ngroups:\n  runners:\n    eval: true\n    limits:\n      timeout: 1\n      ops: 1000000000000\n").unwrap();
    std::fs::write(scratch.join("edge.json"), r#"{ "permissions": { "eval": ["time:monotonic"] } }"#).unwrap();
    let payload = bundle("main.py", &[("main.py", "import time\nsleep(2)\nprint('slept')\n"), ("edge.json", r#"{ "permissions": { "main": ["time:monotonic"] } }"#)]);

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    // The first post waits out the boot, so only the spin is timed.
    let slept = post_eval("127.0.0.1:9815", "/eval/runners", &format!("EDGEPKG:{}", base64_encode(&payload)));
    let start = Instant::now();
    // The op budget sits far off, so only the clock can end the spin.
    let spun = post_eval("127.0.0.1:9815", "/eval/runners", "while True:\n    n = sum(range(1000000))");
    let spun_for = start.elapsed().as_secs_f64();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
    // Under the ten-second default the sleep would print and the spin would run ten seconds.
    assert!(slept.contains("run exceeded its time limit"), "the sleep answered {slept:?}");
    assert!(spun.contains("time limit") && spun_for < 2.5, "the spin answered {spun:?} after {spun_for}s");
}

/* A timeout past five minutes, or on a group that does not eval, stops the boot. */
#[test]
fn a_timeout_out_of_bounds_or_off_eval_stops_the_boot() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-timeout-bounds-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let boot = |yml: &str| {
        let manifest = scratch.join("actor.yml");
        std::fs::write(&manifest, yml).unwrap();
        let out = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let long = boot("groups:\n  runners:\n    eval: true\n    limits:\n      timeout: 301\n");
    let fixed = boot("groups:\n  echo:\n    code: print(receive())\n    limits:\n      timeout: 5\n");
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(!long.0 && long.1.contains("limits.timeout to 301, it takes 1 to 300 seconds"), "a long timeout booted, {long:?}");
    assert!(!fixed.0 && fixed.1.contains("limits.timeout, which only an eval group takes"), "a fixed group timeout booted, {fixed:?}");
}

/* An idle client, a slow one, an eval caller waiting on its run, a line past the cap and a flood hold up no other client. */
#[test]
fn the_server_serves_every_client_at_once_and_holds_a_flood_back() {
    use std::io::{BufRead, Read};
    use std::time::Instant;
    let scratch = std::env::temp_dir().join(format!("edge-actor-clients-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7816\n  control: tcp://127.0.0.1:9816\ngroups:\n  runners:\n    eval: true\n  echo:\n    code: |\n      while True:\n          print('echo', receive())\n  stuck:\n    code: |\n      receive()\n      while True:\n          pass\n").unwrap();
    std::fs::write(scratch.join("edge.json"), r#"{ "permissions": { "eval": ["time:monotonic"] } }"#).unwrap();
    let payload = bundle("main.py", &[("main.py", "import time\nsleep(3)\nprint('slept')\n"), ("edge.json", r#"{ "permissions": { "main": ["time:monotonic"] } }"#)]);
    let sleeper = format!("EDGEPKG:{}", base64_encode(&payload));

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send((Instant::now(), line));
        }
    });
    // A client that connects and says nothing, the ingress once read it alone until it hung up.
    let _idle = connect("127.0.0.1:7816").expect("ingress never came up");
    let mut other = connect("127.0.0.1:7816").unwrap();
    let sent = Instant::now();
    let _ = writeln!(other, "echo second");
    let echoed = lines.recv_timeout(Duration::from_secs(5)).map(|(at, line)| (at.duration_since(sent).as_secs_f64(), line));

    let waiter = std::thread::spawn(move || post_eval("127.0.0.1:9816", "/eval/runners", &sleeper));
    // Clients that never finish their request, once enough of them took every control thread.
    let slow: Vec<TcpStream> = (0..8)
        .filter_map(|_| connect("127.0.0.1:9816"))
        .map(|mut sock| {
            let _ = sock.write_all(b"POST /pub/echo HTTP/1.1\r\nContent-Length: 100000\r\n\r\nab");
            sock
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    let start = Instant::now();
    let status = get_status("127.0.0.1:9816");
    let published = post_eval("127.0.0.1:9816", "/pub/echo", "third");
    let answered_in = start.elapsed().as_secs_f64();
    let slept = waiter.join().unwrap();
    drop(slow);

    // A client that asks before sending its body hears 100 Continue, as curl does past 1 KB.
    let mut asking = connect("127.0.0.1:9816").unwrap();
    let _ = asking.write_all(b"POST /pub/echo HTTP/1.1\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n");
    let mut interim = [0; 25];
    let _ = asking.read_exact(&mut interim);
    let _ = asking.write_all(b"asked");
    let mut asked = String::new();
    let _ = asking.read_to_string(&mut asked);

    // Writing and reading run apart, a server that never cuts the line would block them.
    let (cut_tx, cut) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut long = connect("127.0.0.1:7816").unwrap();
        let _ = long.write_all(&vec![b'x'; 17 << 20]);
        let closed = match long.read(&mut [0; 1]) {
            Ok(n) => n == 0,
            Err(e) => !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
        };
        let _ = cut_tx.send(closed);
    });
    let cut = cut.recv_timeout(Duration::from_secs(10)).unwrap_or(false);

    // The stuck actor takes one message and spins, so every later one waits in its mailbox.
    let mut flood = connect("127.0.0.1:7816").unwrap();
    std::thread::spawn(move || {
        let lines: String = (0..70_000).map(|i| format!("stuck {i}\n")).collect();
        let _ = flood.write_all(lines.as_bytes());
    });
    // The intake refuses too while the scheduler lags behind it, so the wait is for the backlog itself to fill.
    let pending_of = |status: &str| status.split("\"pending\":").nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()?.parse::<usize>().ok());
    let (mut refused, mut backlog) = (String::new(), String::new());
    for _ in 0..300 {
        refused = post_eval("127.0.0.1:9816", "/pub/echo", "late");
        backlog = get_status("127.0.0.1:9816");
        if refused.contains("queue is full") && pending_of(&backlog).is_some_and(|n| n >= 1 << 16) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(echoed.as_ref().is_ok_and(|(t, line)| line == "echo second" && *t < 1.5), "a second client waited behind an idle one, {echoed:?}");
    assert!(status.contains("\"actors\"") && published.contains("{\"ok\":true}") && answered_in < 1.0, "control waited behind an eval or a slow client, {status:?} {published:?} after {answered_in}s");
    assert!(slept.contains(r#""stdout":"slept\n""#), "the eval answered {slept:?}");
    assert!(interim.starts_with(b"HTTP/1.1 100 Continue") && asked.contains("{\"ok\":true}"), "a client that asked first got {:?} then {asked:?}", String::from_utf8_lossy(&interim));
    assert!(cut, "a line past 16 MiB kept its connection open");
    assert!(refused.contains("queue is full") && pending_of(&backlog).is_some_and(|n| n >= 1 << 16), "a flood was not held back, {refused:?} {backlog:?}");
}

/* A server killed mid-run replays at its next start only the messages its actors never finished. */
#[test]
fn a_restart_replays_only_the_messages_left_unfinished() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-wal-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    let pool = |hold: &str| format!("runtime:\n  listen: tcp://127.0.0.1:7817\ngroups:\n  echo:\n    code: |\n      while True:\n          print('echo', receive())\n  hold:\n    code: |\n{hold}");
    // The first run holds its first message forever and queues the second, the next one prints both.
    std::fs::write(&manifest, pool("      receive()\n      while True:\n          pass\n")).unwrap();
    let first = run_killed(&manifest, "127.0.0.1:7817", &["echo a", "echo b", "hold x", "hold y"], "echo b");
    std::fs::write(&manifest, pool("      while True:\n          print('hold', receive())\n")).unwrap();
    let mut second = run_killed(&manifest, "127.0.0.1:7817", &[], "hold y");
    let _ = std::fs::remove_dir_all(&scratch);
    second.sort();
    assert_eq!(first, ["echo a", "echo b"], "the first run printed {first:?}");
    assert_eq!(second, ["hold x", "hold y"], "the restart replayed {second:?}");
}

/* SIGTERM stops a server taking messages and lets its actors finish before it exits. */
#[cfg(unix)]
#[test]
fn a_sigterm_lets_the_actors_finish_before_the_server_exits() {
    use std::io::Read;
    let scratch = std::env::temp_dir().join(format!("edge-actor-drain-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7818\ngroups:\n  slow:\n    code: |\n      import time\n      while True:\n          msg = receive()\n          sleep(1)\n          print('slow', msg)\n").unwrap();
    std::fs::write(scratch.join("edge.json"), r#"{ "permissions": { "main": ["time:monotonic"] } }"#).unwrap();

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut sock = connect("127.0.0.1:7818").expect("ingress never came up");
    let _ = writeln!(sock, "slow z");
    std::thread::sleep(Duration::from_millis(300));
    let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
    let mut status = None;
    for _ in 0..200 {
        status = child.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let mut printed = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut printed);
    // Finished before the exit, the message is done and the next start has nothing to replay.
    let replayed = run_killed(&manifest, "127.0.0.1:7818", &[], "slow");
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(status.is_some_and(|s| s.success()) && printed.contains("slow z"), "the server exited {status:?} having printed {printed:?}");
    assert!(replayed.is_empty(), "the restart replayed {replayed:?}");
}

/* Boots the pool, sends each line, and kills it a beat after a line starting with `until` prints or two seconds pass. */
fn run_killed(manifest: &std::path::Path, addr: &str, send: &[&str], until: &str) -> Vec<String> {
    use std::io::BufRead;
    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut sock = connect(addr).expect("ingress never came up");
    for line in send {
        let _ = writeln!(sock, "{line}");
    }
    let mut seen = Vec::new();
    while let Ok(line) = lines.recv_timeout(Duration::from_secs(2)) {
        let done = line.starts_with(until);
        seen.push(line);
        if done {
            break;
        }
    }
    // The done record follows the print, so the kill waits for it.
    std::thread::sleep(Duration::from_millis(300));
    let _ = child.kill();
    let _ = child.wait();
    seen.extend(lines.try_iter());
    seen
}

// Polls /stats until it carries `want`, messages settle after the publish returns.
fn status_until(addr: &str, want: &str) -> String {
    let mut body = String::new();
    for _ in 0..50 {
        body = get_status(addr);
        if body.contains(want) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    body
}

// Fetches /stats over a bare HTTP GET, returning just the response body.
fn get_status(addr: &str) -> String {
    use std::io::Read;
    let Some(mut sock) = connect(addr) else { return String::new() };
    let _ = write!(sock, "GET /stats HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    let mut resp = String::new();
    let _ = sock.read_to_string(&mut resp);
    resp.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default()
}
