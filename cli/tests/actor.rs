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
    let payload = bundle("main.mini", &[
        ("main.mini", "import util\nprint(util.hi())\n"),
        ("util.mini", "fn hi():\n    return \"bundled and run\"\n"),
        ("edge.json", "{ \"imports\": { \"util\": \"./util.mini\" } }\n"),
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
    let payload = bundle("main.mini", &[
        ("main.mini", "import net\nprint(net.request('GET', 'http://127.0.0.1:1/') > 0)\n"),
        ("edge.json", "{ \"permissions\": { \"main\": [\"net:127.0.0.1\"] } }\n"),
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
    assert!(reply.contains("the pool does not grant eval what this bundle grants") && reply.contains("main   net:127.0.0.1"), "reply was {reply:?}");
}

/* A bundle holds what it grants within the eval grant of the pool, and a snippet holds nothing. */
#[test]
fn an_eval_bundle_grants_within_what_the_pool_grants_eval() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-ceiling-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7813\n  control: tcp://127.0.0.1:9813\ngroups:\n  runners:\n    eval: true\n").unwrap();
    std::fs::write(scratch.join("edge.json"), r#"{ "permissions": { "main": ["net:127.0.0.1"], "all": ["net:127.0.0.1"], "eval": ["net:127.0.0.1/eval"] } }"#).unwrap();
    let granting = |source: &str, grant: &str| {
        let payload = bundle("main.mini", &[("main.mini", source), ("edge.json", &format!(r#"{{ "permissions": {{ "main": ["{grant}"] }} }}"#))]);
        post_eval("127.0.0.1:9813", "/eval/runners", &format!("EDGEPKG:{}", base64_encode(&payload)))
    };

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let held = granting("import net\nprint(net.request('GET', 'http://127.0.0.1:1/eval/x') > 0)\n", "net:127.0.0.1/eval");
    let outside = granting("import net\nprint(net.request('GET', 'http://127.0.0.1:1/other'))\n", "net:127.0.0.1/eval");
    let beyond = granting("import net\nprint(net.request('GET', 'http://127.0.0.1:1/'))\n", "net:127.0.0.1");
    let snippet = post_eval("127.0.0.1:9813", "/eval/runners", "import net\nprint(net.request('GET', 'http://127.0.0.1:1/eval/x') > 0)\n");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(held.contains(r#"{"ok":true,"stdout":"True\n"}"#), "held was {held:?}");
    assert!(outside.contains("'main' has no net:127.0.0.1/other, edge.json grants it net:127.0.0.1/eval"), "outside was {outside:?}");
    assert!(beyond.contains("main   net:127.0.0.1") && !beyond.contains("/eval"), "beyond was {beyond:?}");
    assert!(snippet.contains("'main' imports net, which edge.json does not grant it"), "snippet was {snippet:?}");
}

/* An eval that spins and an actor that never stops computing share one thread, and neither holds up the rest. */
#[test]
fn a_busy_actor_never_holds_up_the_rest() {
    use std::io::BufRead;
    use std::time::Instant;
    let scratch = std::env::temp_dir().join(format!("edge-actor-share-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7814\n  control: tcp://127.0.0.1:9814\ngroups:\n  runners:\n    eval: true\n    replicas: 3\n  echo:\n    code: print('echo', receive())\n  spin:\n    code: |\n      receive()\n      while true:\n          pass\n").unwrap();

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
    for line in ["runners while true: pass", "spin go", "echo hi"] {
        let _ = writeln!(sock, "{line}");
    }
    let _ = sock.flush();
    let echoed = lines.recv_timeout(Duration::from_secs(15)).map(|(at, line)| (at.duration_since(sent).as_secs_f64(), line));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(echoed.as_ref().is_ok_and(|(t, line)| line == "echo hi" && *t < 1.5), "an echo waited behind the spinning eval or the spinning actor, {echoed:?}");
}

/* An eval group sets its own timeout, which ends a spin the op budget would not. */
#[test]
fn an_eval_group_timeout_ends_a_spin() {
    use std::time::Instant;
    let scratch = std::env::temp_dir().join(format!("edge-actor-timeout-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7815\n  control: tcp://127.0.0.1:9815\ngroups:\n  runners:\n    eval: true\n    limits:\n      timeout: 1\n      ops: 18446744073709551615\n").unwrap();

    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    // The first post waits out the boot, so only the spin is timed.
    let booted = post_eval("127.0.0.1:9815", "/eval/runners", "print('up')");
    let start = Instant::now();
    // the op budget is the largest a group takes so only the clock can end the spin
    let spun = post_eval("127.0.0.1:9815", "/eval/runners", "while true:\n    pass");
    let spun_for = start.elapsed().as_secs_f64();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
    // Under the ten-second default the spin would run ten seconds.
    assert!(booted.contains(r#""stdout":"up\n""#), "the first eval answered {booted:?}");
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

/* a setting past what it takes stops the boot and names what it takes */
#[test]
fn a_setting_past_what_it_takes_stops_the_boot() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-settings-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    let cases = [
        ("groups:\n  g:\n    eval: true\n    code: print(1)\n", "group 'g' sets more than one of code, run and eval, it takes one"),
        ("groups:\n  g:\n    code: print(1)\n    run: main.mini\n", "group 'g' sets more than one of code, run and eval, it takes one"),
        ("groups:\n  g:\n    code: print(1)\n    replicas: 0\n", "group 'g' sets replicas to 0, it takes 1 or more"),
        ("runtime:\n  max_actors: 0\ngroups:\n  g:\n    code: print(1)\n", "runtime.max_actors is 0, it takes 1 or more"),
        ("groups:\n  g:\n    code: print(1)\n    out: stdrr\n", "group 'g' sets out to 'stdrr', it takes stdout, null or file://<path>"),
        ("groups:\n  g:\n    code: print(1)\n    out: file:///edge-no-such-dir/out.log\n", "opening the out file of group 'g'"),
        ("runtime:\n  schedulers: many\ngroups:\n  g:\n    code: print(1)\n", "runtime.schedulers takes auto or a count of 1 or more"),
        ("runtime:\n  schedulers: 0\ngroups:\n  g:\n    code: print(1)\n", "runtime.schedulers takes auto or a count of 1 or more"),
    ];
    let mut booted = Vec::new();
    for (yml, why) in cases {
        std::fs::write(&manifest, yml).unwrap();
        let out = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() || !err.contains(why) || !out.stdout.is_empty() {
            booted.push(format!("{yml:?} gave {:?} {err:?}", out.status));
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(booted.is_empty(), "settings past what they take booted\n{}", booted.join("\n"));
}

/* a control endpoint off loopback stops the boot before anything binds and names the tunnel */
#[test]
fn a_control_endpoint_off_loopback_stops_the_boot() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-control-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    let group = "groups:\n  echo:\n    code: print(receive())\n    seed: [\"x\"]\n";
    let mut booted = Vec::new();
    // a pool without listen never binds and no interface here holds 192.0.2.1
    for (runtime, addr) in [("  control: tcp://0.0.0.0:9819\n", "0.0.0.0:9819"), ("  control: tcp://[::]:9819\n", "[::]:9819"), ("  listen: tcp://127.0.0.1:7819\n  control: tcp://192.0.2.1:9819\n", "192.0.2.1:9819")] {
        std::fs::write(&manifest, format!("runtime:\n{runtime}{group}")).unwrap();
        let (refused, err) = boot_briefly(&manifest);
        if !refused || !err.contains(&format!("runtime.control is {addr}, but the control endpoint is local-only")) || !err.contains("ssh -L 9819:127.0.0.1:9819 <host>") {
            booted.push(format!("{addr} gave {err:?}"));
        }
    }
    let ingress = TcpStream::connect("127.0.0.1:7819");
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(booted.is_empty(), "a control endpoint off loopback booted\n{}", booted.join("\n"));
    assert!(ingress.is_err(), "the ingress came up beside a refused control endpoint");
}

/* whether the pool failed within five seconds with its stderr killing one that stays up */
fn boot_briefly(manifest: &std::path::Path) -> (bool, String) {
    use std::io::Read;
    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let mut status = None;
    for _ in 0..100 {
        status = child.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut err = String::new();
    let _ = child.stderr.take().unwrap().read_to_string(&mut err);
    (status.is_some_and(|s| !s.success()), err)
}

/* An idle client, a slow one, an eval caller waiting on its run, a line past the cap and a flood hold up no other client. */
#[test]
fn the_server_serves_every_client_at_once_and_holds_a_flood_back() {
    use std::io::{BufRead, Read};
    use std::time::Instant;
    let scratch = std::env::temp_dir().join(format!("edge-actor-clients-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7816\n  control: tcp://127.0.0.1:9816\ngroups:\n  runners:\n    eval: true\n  echo:\n    code: |\n      while true:\n          print('echo', receive())\n  stuck:\n    code: |\n      receive()\n      while true:\n          pass\n").unwrap();
    // An eval that computes for a while, which takes the actor thread and no control thread.
    let sleeper = "for _ in range(3000000):\n    pass\nprint('slept')".to_string();

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
    let pool = |hold: &str| format!("runtime:\n  listen: tcp://127.0.0.1:7817\ngroups:\n  echo:\n    code: |\n      while true:\n          print('echo', receive())\n  hold:\n    code: |\n{hold}");
    // The first run holds its first message forever and queues the second, the next one prints both.
    std::fs::write(&manifest, pool("      receive()\n      while true:\n          pass\n")).unwrap();
    let first = run_killed(&manifest, "127.0.0.1:7817", &["echo a", "echo b", "hold x", "hold y"], "echo b");
    std::fs::write(&manifest, pool("      while true:\n          print('hold', receive())\n")).unwrap();
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
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7818\ngroups:\n  slow:\n    code: |\n      while true:\n          msg = receive()\n          for _ in range(2000000):\n              pass\n          print('slow', msg)\n").unwrap();

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

/* an eval bundle reads granted files below the pool's manifest and never where it started */
#[test]
fn an_eval_bundle_reads_files_below_its_pool_and_nowhere_else() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-files-{}", std::process::id()));
    let (pool, away) = (scratch.join("pool"), scratch.join("away"));
    for dir in [&pool, &away] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(pool.join("edge.json"), r#"{ "permissions": { "eval": ["files:notes.txt"] } }"#).unwrap();
    std::fs::write(pool.join("notes.txt"), "pool notes\n").unwrap();
    std::fs::write(away.join("notes.txt"), "where the pool started\n").unwrap();
    // each seed a bundle granting itself one entry and reading one path
    let seed = |grant: &str, path: &str| {
        let main = format!("import files\nprint(files.read(files.open('{path}', nil)))\n");
        let grants = format!(r#"{{ "permissions": {{ "main": ["{grant}"] }} }}"#);
        format!("\"EDGEPKG:{}\"", base64_encode(&bundle("main.mini", &[("main.mini", &main), ("edge.json", &grants)])))
    };
    let seeds = [seed("files:notes.txt", "notes.txt"), seed("files:notes.txt", "../away/notes.txt"), seed("files:secret.txt", "secret.txt"), seed("files:../away/notes.txt", "notes.txt")];
    std::fs::write(pool.join("actor.yml"), format!("groups:\n  guest:\n    eval: true\n    seed: [{}]\n", seeds.join(", "))).unwrap();
    let out = edge().args(["actor", pool.join("actor.yml").to_str().unwrap()]).current_dir(&away).stdin(Stdio::null()).output().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout, "b'pool notes\\n'\n", "stderr {stderr:?}");
    let refusals = [
        "error: an eval run of group 'guest' failed\nerror: ValueError: '../away/notes.txt' is not a plain path",
        "error: an eval run of group 'guest' failed\nerror: the pool does not grant eval what this bundle grants\n  main   files:secret.txt",
        "error: an eval run of group 'guest' failed\nerror: edge.json at 'edge.json': permissions for 'main' give files the scope '../away/notes.txt', which it does not have",
    ];
    for refusal in refusals {
        assert!(stderr.contains(refusal), "stderr {stderr:?} lacks {refusal:?}");
    }
    assert_eq!(out.status.code(), Some(1), "three refused bundles fail the pool");
}

/* an eval run that raises or sends fails the pool under its group's name */
#[test]
fn an_eval_run_that_raises_or_sends_fails_under_its_group() {
    let scratch = std::env::temp_dir().join(format!("edge-actor-raises-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, r#"groups:
  g:
    eval: true
    seed: ["print('b')\nsend('g', 'm')\nprint('after')", "print('a')\nx = 1 / 0", "print('fine')"]
"#).unwrap();
    let out = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).output().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout, "b\na\nfine\n", "stderr {stderr:?}");
    let sent = "error: an eval run of group 'g' failed, it sent to 'g' and an eval run cannot send\nerror: RuntimeError: send() needs an actor scheduler";
    let raised = "error: an eval run of group 'g' failed\nerror: ZeroDivisionError: division by zero\n  --> ";
    assert!(stderr.contains(sent) && stderr.contains(raised), "stderr {stderr:?}");
    assert_eq!(stderr.matches("error: an eval run of group").count(), 2, "stderr {stderr:?}");
    assert_eq!(out.status.code(), Some(1), "two failed runs fail the pool");
}

/* the control port answers only requests naming it and none a web page sends */
#[test]
fn the_control_port_refuses_a_rebound_host_and_a_web_page() {
    use std::io::Read;
    let scratch = std::env::temp_dir().join(format!("edge-actor-hosts-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  listen: tcp://127.0.0.1:7821\n  control: tcp://127.0.0.1:9821\ngroups:\n  echo:\n    code: |\n      while true:\n          print('echo', receive())\n").unwrap();
    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    // the whole answer to one request written as given
    let ask = |request: &str| {
        let Some(mut sock) = connect("127.0.0.1:9821") else { return String::new() };
        let _ = sock.write_all(request.as_bytes());
        let mut answer = String::new();
        let _ = sock.read_to_string(&mut answer);
        answer
    };
    let named = ask("GET /stats HTTP/1.0\r\nHost: 127.0.0.1:9821\r\n\r\n");
    let rebound = ask("GET /stats HTTP/1.0\r\nHost: attacker.example:9821\r\n\r\n");
    let nameless = ask("GET /stats HTTP/1.0\r\n\r\n");
    let page = ask("POST /pub/echo HTTP/1.0\r\nHost: 127.0.0.1:9821\r\nOrigin: https://attacker.example\r\nContent-Length: 4\r\n\r\npage");
    // a browser posting to the ingress opens with a request line so nothing lands
    if let Some(mut sock) = connect("127.0.0.1:7821") {
        let _ = sock.write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1:7821\r\nContent-Type: text/plain\r\nContent-Length: 14\r\n\r\necho injected\n");
    }
    if let Some(mut sock) = connect("127.0.0.1:7821") {
        let _ = sock.write_all(b"echo hi\n");
    }
    std::thread::sleep(Duration::from_millis(800));
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(named.starts_with("HTTP/1.1 200 OK"), "named was {named:?}");
    assert!(rebound.starts_with("HTTP/1.1 421") && nameless.starts_with("HTTP/1.1 200 OK"), "rebound was {rebound:?} and nameless {nameless:?}");
    assert!(page.starts_with("HTTP/1.1 403"), "page was {page:?}");
    assert!(stdout.contains("echo hi") && !stdout.contains("injected") && !stdout.contains("page"), "stdout was {stdout:?}");
}

/* an eval import awaiting a silent server holds up no actor and ends on time */
#[test]
fn an_eval_import_waiting_on_the_network_holds_up_no_actor() {
    use std::io::BufRead;
    use std::time::Instant;
    // a server that takes every connection and never answers one
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = silent.local_addr().unwrap();
    std::thread::spawn(move || {
        let held: Vec<TcpStream> = silent.incoming().map_while(Result::ok).collect();
        drop(held);
    });
    let scratch = std::env::temp_dir().join(format!("edge-actor-silent-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&scratch);
    std::fs::write(scratch.join("edge.json"), r#"{ "imports": { "slow": "https://cdn.edgelang.dev/slow/mod.mini" } }"#).unwrap();
    let manifest = scratch.join("actor.yml");
    std::fs::write(&manifest, "runtime:\n  schedulers: 1\ngroups:\n  ask:\n    eval: true\n    limits:\n      timeout: 2\n    seed: [\"import slow\\nprint('never')\"]\n  echo:\n    code: print('echo', receive())\n    seed: [\"hi\"]\n").unwrap();
    let started = Instant::now();
    let mut child = edge().args(["actor", manifest.to_str().unwrap()]).env("EDGE_CDN_BASE", format!("http://{at}")).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send((started.elapsed().as_secs_f64(), line));
        }
    });
    let echoed = lines.recv_timeout(Duration::from_secs(15));
    let ended = (0..300).find_map(|_| {
        std::thread::sleep(Duration::from_millis(50));
        child.try_wait().ok().flatten().map(|status| (started.elapsed().as_secs_f64(), status))
    });
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // the echo comes while the fetch waits and the pool ends at the eval deadline
    let ok = matches!((&echoed, ended), (Ok((echo, line)), Some((end, status))) if line == "echo hi" && *echo + 1.0 < end && status.code() == Some(1));
    assert!(ok, "the echo waited behind the fetch or the pool outlived the deadline, {echoed:?} {ended:?}");
    assert!(stderr.contains("error: an eval run of group 'ask' failed") && stderr.contains("time limit"), "stderr {stderr:?}");
}
