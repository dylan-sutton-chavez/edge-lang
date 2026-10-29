use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use edge::code::{Diagnostic, SSAChunk};
use edge::embed::{Limits, VM};
use edge::deps::{partition_bindings, NativeBinding, Resolved, Resolver};
use edge::value::{HeapObj, HeapPool, SchedulerStatus, Val, VmErr};
use serde::Deserialize;

// every run meters what it allocates as tiny.wasm does
edge::allocator!();

// the origin the net conformance routes answer on which no socket ever opens
const FIXTURE: &str = "http://127.0.0.1:1";

/* the slice as its lines and as the gzip stream the hour's url serves */
struct Slice {
    lines: Vec<String>,
    gzip: Vec<u8>,
}

/* the data job's slice of the pinned hour with its gzip stream beside it */
fn slice() -> &'static Slice {
    static SLICE: OnceLock<Slice> = OnceLock::new();
    SLICE.get_or_init(|| {
        let at = data::cache(Path::new(env!("CARGO_TARGET_TMPDIR")));
        let body = data::slice(&at);
        let gz = at.join("slice.jsonl.gz");
        if !gz.exists() {
            let out = Command::new("gzip").args(["-c", "-n"]).arg(at.join("slice.jsonl")).output().expect("gzip compresses the slice");
            std::fs::write(&gz, out.stdout).expect("writing the compressed slice");
        }
        let lines: Vec<String> = String::from_utf8(body).expect("the slice is utf-8").lines().map(String::from).collect();
        assert_eq!(lines.len(), data::SLICE_RECORDS);
        Slice { lines, gzip: std::fs::read(&gz).expect("reading the compressed slice") }
    })
}

/* the slice as an RFC 4180 table of three of its fields */
fn slice_csv() -> Vec<String> {
    let mut out = vec!["type,repo,created_at".to_string()];
    for line in &slice().lines {
        let event: serde_yaml_ng::Value = serde_yaml_ng::from_str(line).expect("an event of the slice reads");
        let text = |v: &serde_yaml_ng::Value| v.as_str().unwrap_or("").to_string();
        let quote = |s: String| if s.contains([',', '"']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s };
        out.push(format!("{},{},{}", quote(text(&event["type"])), quote(text(&event["repo"]["name"])), quote(text(&event["created_at"]))));
    }
    out
}

/* the slice as the fixture serves it under each name a case reads it by */
fn served(url: &str) -> Option<Vec<u8>> {
    let lines = match url.strip_prefix(FIXTURE)? {
        "/hour/events.jsonl" | "/hour/events.txt" | "/hour/lines.json" => slice().lines.clone(),
        "/hour/events.csv" => slice_csv(),
        "/hour/names.jsonl" => repo_names(),
        "/hour/fields.jsonl" => fields(),
        "/hour/events.json" => vec![format!("[{}]", slice().lines.join(","))],
        "/hour/pretty.json" => array_lines(),
        "/hour/pairs.jsonl" => pairs(),
        "/hour/broken.json" => vec![r#"[{"a":1},{"a":2]"#.to_string()],
        _ => return None,
    };
    Some(lines.iter().flat_map(|l| l.bytes().chain(std::iter::once(b'\n'))).collect())
}

/* the slice as one JSON array with a record on each line between its brackets */
fn array_lines() -> Vec<String> {
    let n = slice().lines.len();
    let records = slice().lines.iter().enumerate().map(|(i, l)| format!("  {l}{}", if i + 1 < n { "," } else { "" }));
    std::iter::once("[".to_string()).chain(records).chain(std::iter::once("]".to_string())).collect()
}

/* each event's type and time as a line holding a JSON array */
fn pairs() -> Vec<String> {
    slice().lines.iter().map(|line| {
        let event: serde_yaml_ng::Value = serde_yaml_ng::from_str(line).expect("an event of the slice reads");
        format!("[\"{}\",\"{}\"]", event["type"].as_str().unwrap_or(""), event["created_at"].as_str().unwrap_or(""))
    }).collect()
}

/* each event's repository name as a line of JSON text */
fn repo_names() -> Vec<String> {
    slice().lines.iter().map(|line| {
        let event: serde_yaml_ng::Value = serde_yaml_ng::from_str(line).expect("an event of the slice reads");
        let name = event["repo"]["name"].as_str().expect("every event names its repository");
        assert!(!name.contains(['"', '\\']), "a repository name needs no escapes");
        format!("\"{name}\"")
    }).collect()
}

/* each event's type and time beside the bytes of its line and its minute */
fn fields() -> Vec<String> {
    slice().lines.iter().map(|line| {
        let event: serde_yaml_ng::Value = serde_yaml_ng::from_str(line).expect("an event of the slice reads");
        let kind = event["type"].as_str().expect("every event has a type");
        let at = event["created_at"].as_str().expect("every event has a time");
        let minute: u32 = at[14..16].parse().expect("a time names its minute");
        format!("{{\"type\":\"{kind}\",\"time\":\"{at}\",\"len\":{},\"min\":{minute}}}", line.len())
    }).collect()
}

/* what one route of the fixture answers */
#[derive(Deserialize, Clone, Default)]
struct Route {
    #[serde(default)] status: Option<u16>,
    #[serde(default)] headers: Vec<(String, String)>,
    #[serde(default)] body: Option<String>,
    #[serde(default)] repeat: Option<(String, usize)>,
    #[serde(default)] gzip: Option<Vec<String>>,
    #[serde(default)] echo: Option<Vec<String>>,
    #[serde(default)] socket: Option<String>,
}

#[derive(Deserialize)]
struct NetCases { routes: BTreeMap<String, Route> }

/* the net suite's routes on the fixture and the hour's url serving the slice */
fn routes() -> &'static BTreeMap<String, Route> {
    static ROUTES: OnceLock<BTreeMap<String, Route>> = OnceLock::new();
    ROUTES.get_or_init(|| {
        // a json document reads as yaml so the suite needs no second reader
        let file: NetCases = serde_yaml_ng::from_str(include_str!("../../net/cases.json")).expect("examples/net/cases.json reads");
        let mut all: BTreeMap<String, Route> = file.routes.into_iter().map(|(p, r)| (format!("{FIXTURE}{p}"), r)).collect();
        all.insert(data::HOUR.to_string(), Route { headers: vec![("content-type".into(), "application/gzip".into())], ..Route::default() });
        all
    })
}

/* the gzip stream of `text` as a compressor writes it */
fn gzipped(text: &str) -> Vec<u8> {
    let mut child = Command::new("gzip").args(["-c", "-n"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().expect("gzip compresses");
    let mut stdin = child.stdin.take().expect("gzip reads its input");
    std::io::Write::write_all(&mut stdin, text.as_bytes()).expect("feeding gzip");
    drop(stdin);
    child.wait_with_output().expect("gzip compresses").stdout
}

/* one request the transport serves as the net plugin's contract words it */
struct Request {
    url: String,
    body: Result<Vec<u8>, String>,
    at: usize,
    status: u16,
    closed: bool,
}

// how much of a body one read hands over as the native host reads it
const CHUNK: usize = 64 << 10;

/* a call the program made that waits for the transport to answer it */
enum Pending { Head(i64), Read(i64) }

/* the net plugin's contract served in the test process with no socket */
#[derive(Default)]
struct Transport {
    // the scopes the manifest grants net and none when every host is held
    grants: Option<Vec<String>>,
    requests: BTreeMap<i64, Request>,
    pending: Vec<Pending>,
    next: i64,
    // the files a flow opened through the files module with how far each was read
    files: BTreeMap<i64, (Vec<u8>, usize)>,
}

impl Transport {
    fn request(&mut self, url: &str, inflate: bool) -> Result<i64, String> {
        let (host, path) = place(url).ok_or_else(|| format!("BadData: '{url}' is not a plain url, net takes scheme://host/path with no user, backslash or space"))?;
        if let Some(grants) = &self.grants {
            let scopes: Vec<String> = grants.iter().filter_map(|g| g.strip_prefix("net:")).map(String::from).collect();
            let under = |p: &str| path == p || path.starts_with(&format!("{p}/"));
            let bounded: Vec<&String> = scopes.iter().filter(|s| s.split('/').next() == Some(host.as_str())).collect();
            let held = bounded.iter().any(|s| **s == host || s.strip_prefix(&host).is_some_and(under));
            if !held {
                // the path shows only once the host is held so a refused host reads alone
                let asked = if bounded.is_empty() { host.clone() } else { format!("{host}{path}") };
                let (before, after) = edge::deps::perms::refusal("main", "net", &scopes);
                return Err(format!("Denied: {before}{asked}{after}"));
            }
        }
        let base = url.split(['?', '#']).next().unwrap_or(url);
        let route = routes().iter().filter(|(r, _)| base == r.as_str() || base.starts_with(&format!("{r}/"))).max_by_key(|(r, _)| r.len()).map(|(_, r)| r.clone());
        let file = served(base);
        let route = route.unwrap_or(Route { status: Some(if file.is_some() { 200 } else { 404 }), ..Route::default() });
        let status = route.status.unwrap_or(200);
        let body = if let Some(lines) = file {
            if inflate { Err("HostFailed: reading the response failed, invalid gzip header".into()) } else { Ok(lines) }
        } else if base == data::HOUR {
            if inflate { Ok(slice().lines.iter().flat_map(|l| l.bytes().chain(std::iter::once(b'\n'))).collect()) } else { Ok(slice().gzip.clone()) }
        } else if let Some(members) = &route.gzip {
            match (inflate, members.len()) {
                (true, 1) => Ok(members[0].clone().into_bytes()),
                (true, _) => Err("HostFailed: reading the response failed, junk found after the end of the gzip stream".into()),
                (false, _) => Ok(members.iter().flat_map(|m| gzipped(m)).collect()),
            }
        } else if inflate {
            Err("HostFailed: reading the response failed, invalid gzip header".into())
        } else if let Some((text, n)) = &route.repeat {
            Ok(text.repeat(*n).into_bytes())
        } else {
            let _ = (&route.echo, &route.socket, &route.headers);
            Ok(route.body.clone().unwrap_or_default().into_bytes())
        };
        self.next += 1;
        self.requests.insert(self.next, Request { url: url.to_string(), body, at: 0, status, closed: false });
        Ok(self.next)
    }

    /* a waiting call's answer as a value on `heap` or its error */
    fn answer(&mut self, call: Pending, heap: &mut HeapPool) -> Result<Val, VmErr> {
        let (Pending::Head(id) | Pending::Read(id)) = call;
        let Some(r) = self.requests.get_mut(&id) else { return Err(VmErr::Raised(format!("BadData: no open request or socket {id}"))) };
        if r.closed { return Err(VmErr::Raised(format!("HostFailed: net.request to {} failed, the request was closed", r.url))); }
        match call {
            Pending::Head(_) => {
                if matches!(r.status, 301 | 302 | 303 | 307 | 308) {
                    return Err(VmErr::Raised(format!("HostFailed: net.request to {} was redirected, request the new address with its own net.request", r.url)));
                }
                let status = Val::int(i64::from(r.status));
                let headers = heap.alloc(HeapObj::List(Rc::new(RefCell::new(Vec::new()))))?;
                heap.alloc(HeapObj::List(Rc::new(RefCell::new(vec![status, headers]))))
            }
            Pending::Read(_) => {
                let body = match &r.body { Ok(b) => b, Err(e) => return Err(VmErr::Raised(e.clone())) };
                if r.at >= body.len() { return Ok(Val::none()); }
                let end = (r.at + CHUNK).min(body.len());
                let chunk = body[r.at..end].to_vec();
                r.at = end;
                heap.alloc(HeapObj::Bytes(chunk))
            }
        }
    }
}

/* the slice as the files below a flow that the cases read by path */
fn local(path: &str) -> Option<Vec<u8>> {
    let lines = |lines: Vec<String>| lines.iter().flat_map(|l| l.bytes().chain(std::iter::once(b'\n'))).collect();
    match path {
        "events.jsonl" | "hour/events.jsonl" | "hour/events.txt" => Some(lines(slice().lines.clone())),
        "hour/events.csv" => Some(lines(slice_csv())),
        "hour/events.jsonl.gz" => Some(slice().gzip.clone()),
        // a record of each kind whose fields the error cases read
        "odd.jsonl" => Some(lines(vec![r#"{"a":"x5","b":3,"c":2.5,"e":{"f":1},"g":null}"#.into()])),
        // fields named past ascii as real headers name them
        "accents.jsonl" => Some(lines(vec![r#"{"città":"x","a":{"é":1}}"#.into(), r#"{"città":"y","a":{"é":1}}"#.into(), r#"{"città":"x","a":{"é":2}}"#.into()])),
        // quoted fields holding line breaks with one quote that never closes
        "multi.csv" => Some(lines(vec!["a,b".into(), "1,\"x".into(), "y\"".into(), "2,z".into(), "3,\"p".into(), "".into(), "q \"\"r\"\"\"".into()])),
        "open.csv" => Some(lines(vec!["a,b".into(), "1,x".into(), "2,\"y".into(), "3,z".into()])),
        // one line of eight megabytes that no line end closes
        "long.txt" => Some(vec![b'x'; 8 << 20]),
        _ => None,
    }
}

/* whether `path` names a file below the flow with only plain names between slashes */
fn plain_path(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', ':', '\0']))
}

impl Transport {
    /* the id of a file opened as the CLI's files module words its refusals */
    fn open(&mut self, path: &str, inflate: bool) -> Result<i64, String> {
        if !plain_path(path) { return Err(format!("BadData: '{path}' is not a plain path, files takes names joined by slashes below the package")); }
        if let Some(grants) = &self.grants {
            let scopes: Vec<String> = grants.iter().filter_map(|g| g.strip_prefix("files:")).map(String::from).collect();
            if !scopes.iter().any(|s| s == path || (s.ends_with('/') && path.starts_with(s.as_str()))) {
                let (before, after) = edge::deps::perms::refusal("main", "files", &scopes);
                return Err(format!("Denied: {before}{path}{after}"));
            }
        }
        let body = local(path).ok_or_else(|| format!("HostFailed: cannot open {path}, no such file"))?;
        let body = match (inflate, path == "hour/events.jsonl.gz") {
            (true, true) => local("hour/events.jsonl").unwrap_or_default(),
            (true, false) => return Err("HostFailed: reading the file failed, invalid gzip header".into()),
            (false, _) => body,
        };
        self.next += 1;
        self.files.insert(self.next, (body, 0));
        Ok(self.next)
    }

    /* the next chunk of an open file or none at its end */
    fn read_file(&mut self, id: i64, heap: &mut HeapPool) -> Result<Val, VmErr> {
        let Some((body, at)) = self.files.get_mut(&id) else { return Err(VmErr::Raised(format!("BadData: no open file {id}"))) };
        if *at >= body.len() { return Ok(Val::none()); }
        let end = (*at + CHUNK).min(body.len());
        let chunk = body[*at..end].to_vec();
        *at = end;
        heap.alloc(HeapObj::Bytes(chunk))
    }
}

/* the host and path of a plain url or none for one net refuses */
fn place(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") || url.contains([' ', '\\', '@']) { return None; }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].split(':').next()?.to_ascii_lowercase();
    if host.is_empty() { return None; }
    let path = rest[end..].split(['?', '#']).next().unwrap_or("");
    Some((host, if path.is_empty() { "/".into() } else { path.into() }))
}

/* the resolver that gives a flow the net plugin and the files module over `transport` */
struct Net(Arc<Mutex<Transport>>);

/* the text a host call got as its argument `i` */
fn text_arg(heap: &HeapPool, args: &[Val], i: usize) -> Option<String> {
    match args.get(i).map(|&v| heap.try_get(v)) { Some(Some(HeapObj::Str(s))) => Some(s.to_string()), _ => None }
}

impl Net {
    /* the files module the CLI wires answering each read at once as it does */
    fn files(&self) -> Resolved {
        let (a, b, c) = (self.0.clone(), self.0.clone(), self.0.clone());
        let open = NativeBinding { name: "open".into(), func: Arc::new(move |heap: &mut HeapPool, args: &[Val], _| {
            let path = text_arg(heap, args, 0).ok_or(VmErr::Raised("BadData: a path must be a text".into()))?;
            let inflate = text_arg(heap, args, 1).as_deref() == Some("gzip");
            a.lock().unwrap().open(&path, inflate).map(Val::int).map_err(VmErr::Raised)
        }) };
        let read = NativeBinding { name: "read".into(), func: Arc::new(move |heap: &mut HeapPool, args: &[Val], _| {
            b.lock().unwrap().read_file(args.first().map_or(0, |v| v.as_int()), heap)
        }) };
        let close = NativeBinding { name: "close".into(), func: Arc::new(move |_: &mut HeapPool, args: &[Val], _| {
            c.lock().unwrap().files.remove(&args.first().map_or(0, |v| v.as_int()));
            Ok(Val::none())
        }) };
        let (bindings, classes, consts) = partition_bindings(vec![open, read, close]);
        Resolved::Native { bindings, classes, consts, canonical: "files".into() }
    }
}

impl Resolver for Net {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        if spec == "files" { return Ok(self.files()); }
        if spec != "net" { return Err(format!("module '{spec}' is not provided by this host")); }
        let (a, b, c, d) = (self.0.clone(), self.0.clone(), self.0.clone(), self.0.clone());
        let request = NativeBinding { name: "request".into(), func: Arc::new(move |heap: &mut HeapPool, args: &[Val], _| {
            let url = text_arg(heap, args, 1).ok_or(VmErr::Raised("BadData: a url must be a text".into()))?;
            let inflate = text_arg(heap, args, 4).as_deref() == Some("gzip");
            a.lock().unwrap().request(&url, inflate).map(Val::int).map_err(VmErr::Raised)
        }) };
        let response = NativeBinding { name: "response".into(), func: Arc::new(move |_: &mut HeapPool, args: &[Val], _| {
            b.lock().unwrap().pending.push(Pending::Head(args.first().map_or(0, |v| v.as_int())));
            Err(VmErr::HostCallDeferred)
        }) };
        let read = NativeBinding { name: "read".into(), func: Arc::new(move |_: &mut HeapPool, args: &[Val], _| {
            c.lock().unwrap().pending.push(Pending::Read(args.first().map_or(0, |v| v.as_int())));
            Err(VmErr::HostCallDeferred)
        }) };
        let close = NativeBinding { name: "close".into(), func: Arc::new(move |_: &mut HeapPool, args: &[Val], _| {
            if let Some(r) = d.lock().unwrap().requests.get_mut(&args.first().map_or(0, |v| v.as_int())) { r.closed = true; }
            Ok(Val::none())
        }) };
        let (bindings, classes, consts) = partition_bindings(vec![request, response, read, close]);
        Ok(Resolved::Native { bindings, classes, consts, canonical: "net".into() })
    }
}

/* one case of a topic file */
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    flow: String,
    // the host input as the slice or its table or its array or none
    #[serde(default)]
    input: Option<String>,
    // the lines each channel received in order
    #[serde(default)]
    out: Option<BTreeMap<String, Vec<String>>>,
    // the one line the run's error reads as
    #[serde(default)]
    error: Option<String>,
    // the problems the flow's checker reports each as line and words
    #[serde(default)]
    diagnostics: Option<Vec<String>>,
    // what the manifest grants main with none holding every host
    #[serde(default)]
    grants: Option<Vec<String>>,
    // every wait on the host saves a snapshot and restores it into a fresh run
    #[serde(default)]
    hops: bool,
    #[serde(default)]
    limits: Option<CaseLimits>,
    // how many times the input repeats so a case reads a large one
    #[serde(default)]
    repeat: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseLimits { ops: Option<u64>, memory: Option<usize> }

thread_local! { static SENT: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) }; }

fn keep(channel: &str, line: &str) -> bool {
    SENT.with(|s| s.borrow_mut().push((channel.to_string(), line.to_string())));
    true
}

/* what a run of a case gave as its channels or its one error line */
enum Ran { Out(BTreeMap<String, Vec<String>>), Error(String), Diagnostics(Vec<String>) }

fn line_of(src: &str, at: usize) -> usize { src.as_bytes()[..at.min(src.len())].iter().filter(|&&c| c == b'\n').count() + 1 }

fn boot(chunk: &'static SSAChunk, limits: Limits) -> VM<'static> {
    let mut vm = VM::with_runtime(&tiny::RUNTIME, chunk, limits);
    vm.send_hook = Some(keep);
    vm.strict_input = true;
    vm
}

/* what a case's run gave */
fn run(case: &Case) -> Ran {
    let transport = Arc::new(Mutex::new(Transport { grants: case.grants.clone(), ..Transport::default() }));
    let src = case.flow.as_str();
    let chunk = match tiny::FRONT.program(src, Box::new(Net(transport.clone()))) {
        Ok(c) => c,
        Err(errs) => return Ran::Diagnostics(errs.iter().map(|d: &Diagnostic| format!("line {}: {}", line_of(src, d.start), d.msg)).collect()),
    };
    let chunk: &'static SSAChunk = edge::util::leak(Box::new(Rc::unwrap_or_clone(chunk)));
    let limits = match &case.limits {
        Some(l) => Limits { ops: l.ops.unwrap_or(Limits::sandbox().ops), memory: l.memory.unwrap_or(Limits::sandbox().memory) },
        None => Limits { ops: 2_000_000_000, memory: 512 << 20 },
    };
    let mut vm = boot(chunk, limits);
    vm.input_buffer = match case.input.as_deref() {
        Some("slice") => slice().lines.clone(),
        Some("slice-csv") => slice_csv(),
        Some("slice-array") => array_lines(),
        Some(other) if other != "none" => panic!("{}: input is slice, slice-csv, slice-array or none, not {other}", case.name),
        _ => Vec::new(),
    };
    if let Some(n) = case.repeat { vm.input_buffer = std::iter::repeat_n(vm.input_buffer.clone(), n).flatten().collect(); }
    SENT.with(|s| s.borrow_mut().clear());
    let mut id = 0u64;
    let outcome = loop {
        match vm.run() {
            Ok(_) => break Ok(()),
            Err(VmErr::HostYield(SchedulerStatus::PendingHostCall)) => {
                if case.hops {
                    let blob = edge::embed::save(&vm, src, "").unwrap_or_else(|e| panic!("{}: save {e}", case.name));
                    let mut next = boot(chunk, limits);
                    edge::embed::restore(&mut next, &blob).unwrap_or_else(|e| panic!("{}: restore {e}", case.name));
                    vm = next;
                }
                let call = transport.lock().unwrap().pending.pop().unwrap_or_else(|| panic!("{}: a wait with no call", case.name));
                let answer = transport.lock().unwrap().answer(call, vm.heap_mut());
                let delivered = match answer { Ok(v) => vm.inject_host_result_by_id(id, v), Err(e) => vm.inject_host_error_by_id(id, e) };
                assert!(delivered, "{}: nothing waits on call {id}", case.name);
                id += 1;
            }
            Err(e) => break Err(tiny::report(src, &e.render(&tiny::lang::NAMES), vm.error_pos())),
        }
    };
    match outcome {
        Ok(()) => {
            let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (c, l) in SENT.with(|s| s.borrow().clone()) { out.entry(c).or_default().push(l); }
            Ran::Out(out)
        }
        Err(e) => Ran::Error(e),
    }
}

/* every case of `flows/<topic>.yml` run with each mismatch reported */
fn suite(topic: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/flows").join(format!("{topic}.yml"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let cases: Vec<Case> = serde_yaml_ng::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut failed = Vec::new();
    for case in &cases {
        let expected = [case.out.is_some(), case.error.is_some(), case.diagnostics.is_some()].iter().filter(|&&b| b).count();
        assert_eq!(expected, 1, "{}: a case expects one of out, error and diagnostics", case.name);
        let ran = run(case);
        let wrong = match (&ran, case) {
            (Ran::Out(got), Case { out: Some(want), .. }) if got == want => None,
            (Ran::Error(got), Case { error: Some(want), .. }) if got == want => None,
            (Ran::Diagnostics(got), Case { diagnostics: Some(want), .. }) if got == want => None,
            (Ran::Out(got), _) => Some(format!("out {}", serde_yaml_ng::to_string(got).unwrap_or_default())),
            (Ran::Error(got), _) => Some(format!("error {got:?}")),
            (Ran::Diagnostics(got), _) => Some(format!("diagnostics {got:?}")),
        };
        if let Some(w) = wrong { failed.push(format!("{}: {w}", case.name)); }
    }
    assert!(failed.is_empty(), "{} of {} cases in {topic}.yml failed\n{}", failed.len(), cases.len(), failed.join("\n"));
}

#[test]
fn keys() { suite("keys"); }

#[test]
fn reports() { suite("reports"); }

#[test]
fn expressions() { suite("expressions"); }

#[test]
fn errors() { suite("errors"); }

#[test]
fn hosts() { suite("hosts"); }

#[test]
fn snapshots() { suite("snapshots"); }

#[test]
fn limits() { suite("limits"); }

#[test]
fn rules() { suite("rules"); }

/* a flow of many reports builds in time linear in its reports */
#[test]
fn many_reports_build_in_linear_time() {
    let flow = |n: usize| core::iter::once("from: input\n".to_string()).chain((0..n).map(|i| format!("r{i}:\n  count: type\n"))).collect::<String>();
    let (half, whole) = (flow(3000), flow(6000));
    let time = |src: &str| {
        let start = std::time::Instant::now();
        assert!(tiny::FRONT.program(src, Box::new(Net(Arc::default()))).is_ok());
        start.elapsed()
    };
    // the fastest of three rounds keeps a loaded host's stalls out of the ratio
    let (mut n, mut twice) = (std::time::Duration::MAX, std::time::Duration::MAX);
    for _ in 0..3 { n = n.min(time(&half)); twice = twice.min(time(&whole)); }

    // twice the reports take about twice the time where a quadratic build takes four times
    assert!(twice < n * 3, "3000 reports built in {n:?} and 6000 in {twice:?}");
}

// a refused escape before a wide character spans all of it so its report renders
#[test]
fn a_refused_escape_before_a_wide_character_renders() {
    let src = "from: input\nodd:\n  where: type == \"\\é\"\n  count:\n";
    let errs = tiny::FRONT.program(src, Box::new(Net(Arc::default()))).err().expect("the flow fails");
    assert!(errs.iter().all(|e| src.is_char_boundary(e.start) && src.is_char_boundary(e.end)), "{:?}", errs.iter().map(|e| (e.start, e.end)).collect::<Vec<_>>());
    assert!(edge::code::Diagnostic::render_all(&errs, src, Some("flow.yaml")).contains("invalid escape in string literal"));
}
