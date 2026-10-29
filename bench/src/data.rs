use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use abi::WireValue;
use wasmtime::{Caller, Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
// how much of the hour one read hands over as the CLI's net does
const CHUNK: usize = 64 << 10;
// the run status kinds the runtime packs above bit 29
const DONE: u32 = 0;
const ERROR: u32 = 4;
const WAITING: u32 = 5;
const EXIT: u32 = 6;
const PREEMPTED: u32 = 7;
// the calls of the net module in the order the walk registers them
const CALLS: [&str; 6] = ["request", "response", "read", "close", "connect", "send"];
// a counting chunk holds this many of the hour's records as literals
const LITERALS: usize = 1000;
// the reports both jobs write as mini names them in its printout
const REPORTS: [&str; 5] = ["top_stars", "busy_branches", "activity.csv", "issues_opened", "time_to_close"];

/* net and the output channels a CLI host gives a run over the pinned hour */
struct Host {
    body: std::sync::Arc<Vec<u8>>,
    at: usize,
    // answers known at the call that reach the run once it parks as net does
    parked: Vec<(u32, WireValue)>,
    printed: String,
    sent: Vec<(String, String)>,
    started: Instant,
    first: Option<Duration>,
    closed: bool,
}

/* the exports of the runtime the host drives */
#[derive(Clone)]
struct Exports {
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    free: TypedFunc<(i32, i32), ()>,
    out_ptr: TypedFunc<(), i32>,
    out_len: TypedFunc<(), i32>,
    encode: TypedFunc<(i32, i32, i32), i32>,
    decode: TypedFunc<(i32, i32, i32, i32), i32>,
}

impl Exports {
    fn bind(inst: &Instance, store: &mut Store<Host>) -> Exports {
        Exports {
            memory: inst.get_memory(&mut *store, "memory").expect("the runtime exports memory"),
            alloc: func(inst, store, "wasm_alloc"),
            free: func(inst, store, "wasm_free"),
            out_ptr: func(inst, store, "out_ptr"),
            out_len: func(inst, store, "out_len"),
            encode: func(inst, store, "host_edge_encode"),
            decode: func(inst, store, "host_edge_decode"),
        }
    }
}

fn func<P: wasmtime::WasmParams, R: wasmtime::WasmResults>(inst: &Instance, store: &mut Store<Host>, name: &str) -> TypedFunc<P, R> {
    inst.get_typed_func(&mut *store, name).unwrap_or_else(|e| panic!("runtime export {name}: {e}"))
}

/* the exports a host function reaches through its caller */
fn exports(caller: &mut Caller<'_, Host>) -> Exports {
    let get = |caller: &mut Caller<'_, Host>, name: &str| caller.get_export(name).unwrap_or_else(|| panic!("runtime export {name}"));
    let memory = get(caller, "memory").into_memory().expect("memory");
    macro_rules! f {
        ($name:literal) => {
            get(caller, $name).into_func().expect($name).typed(&*caller).expect($name)
        };
    }
    Exports { memory, alloc: f!("wasm_alloc"), free: f!("wasm_free"), out_ptr: f!("out_ptr"), out_len: f!("out_len"), encode: f!("host_edge_encode"), decode: f!("host_edge_decode") }
}

fn read(caller: &mut Caller<'_, Host>, memory: Memory, ptr: i32, len: i32) -> Vec<u8> {
    let mut buf = vec![0u8; len.max(0) as usize];
    memory.read(&*caller, ptr as u32 as usize, &mut buf).expect("reading runtime memory");
    buf
}

/* a handle the runtime gave as the value it crosses as */
fn decode(caller: &mut Caller<'_, Host>, ex: &Exports, handle: u32) -> WireValue {
    let tag_at = ex.alloc.call(&mut *caller, 4).expect("wasm_alloc");
    let mut cap = 256;
    let mut dst = ex.alloc.call(&mut *caller, cap).expect("wasm_alloc");
    let mut n = ex.decode.call(&mut *caller, (handle as i32, tag_at, dst, cap)).expect("host_edge_decode");
    if n < 0 {
        ex.free.call(&mut *caller, (dst, cap)).expect("wasm_free");
        cap = -n;
        dst = ex.alloc.call(&mut *caller, cap).expect("wasm_alloc");
        n = ex.decode.call(&mut *caller, (handle as i32, tag_at, dst, cap)).expect("host_edge_decode");
    }
    let tag = u32::from_le_bytes(read(caller, ex.memory, tag_at, 4).try_into().expect("a tag"));
    let body = read(caller, ex.memory, dst, n.max(0));
    ex.free.call(&mut *caller, (tag_at, 4)).expect("wasm_free");
    ex.free.call(&mut *caller, (dst, cap)).expect("wasm_free");
    WireValue::decode_body(tag, &body).unwrap_or(WireValue::None)
}

/* a value made in the runtime with the handle that names it */
fn encode(cx: &mut impl wasmtime::AsContextMut<Data = Host>, ex: &Exports, value: &WireValue) -> i32 {
    let mut body = Vec::new();
    value.encode_body(&mut body);
    let ptr = ex.alloc.call(&mut *cx, body.len().max(1) as i32).expect("wasm_alloc");
    ex.memory.write(&mut *cx, ptr as usize, &body).expect("writing runtime memory");
    let handle = ex.encode.call(&mut *cx, (value.tag() as i32, ptr, body.len() as i32)).expect("host_edge_encode");
    ex.free.call(&mut *cx, (ptr, body.len().max(1) as i32)).expect("wasm_free");
    handle
}

/* the env imports every runtime artifact declares */
fn link(linker: &mut Linker<Host>) {
    linker.func_wrap("env", "host_print", |mut caller: Caller<'_, Host>, ptr: i32, len: i32| {
        let ex = exports(&mut caller);
        let text = read(&mut caller, ex.memory, ptr, len);
        let host = caller.data_mut();
        host.first.get_or_insert_with(|| host.started.elapsed());
        host.printed.push_str(&String::from_utf8_lossy(&text));
    }).expect("linking host_print");
    linker.func_wrap("env", "host_fetch_bytes", |mut caller: Caller<'_, Host>, _: i32, _: i32, _: i32, out_len: i32| -> i32 {
        let ex = exports(&mut caller);
        ex.memory.write(&mut caller, out_len as usize, &0u32.to_le_bytes()).expect("writing runtime memory");
        0
    }).expect("linking host_fetch_bytes");
    // every channel a run sends to is kept the way an outbox keeps it
    linker.func_wrap("env", "host_send", |mut caller: Caller<'_, Host>, gp: i32, gl: i32, bp: i32, bl: i32| -> i32 {
        let ex = exports(&mut caller);
        let (group, body) = (read(&mut caller, ex.memory, gp, gl), read(&mut caller, ex.memory, bp, bl));
        let host = caller.data_mut();
        host.first.get_or_insert_with(|| host.started.elapsed());
        host.sent.push((String::from_utf8_lossy(&group).into_owned(), String::from_utf8_lossy(&body).into_owned()));
        0
    }).expect("linking host_send");
    linker.func_wrap("env", "host_call_native", |mut caller: Caller<'_, Host>, id: i32, call_id: i32, argv: i32, argc: i32, out: i32| -> i32 {
        let ex = exports(&mut caller);
        let raw = read(&mut caller, ex.memory, argv, argc.max(1) * 4);
        let handles: Vec<u32> = raw.chunks(4).map(|c| u32::from_le_bytes(c.try_into().expect("a handle"))).collect();
        // the trailing slot holds the kwargs a host module call never takes
        let positional = &handles[..handles.len().saturating_sub(1)];
        let args: Vec<WireValue> = positional.iter().map(|&h| decode(&mut caller, &ex, h)).collect();
        let host = caller.data_mut();
        let answer = match CALLS.get(id as usize).copied() {
            Some("request") => {
                let asked = (&args.get(1), &args.get(4));
                assert!(matches!(asked, (Some(WireValue::Bytes(url)), Some(WireValue::Bytes(z))) if url == data::HOUR.as_bytes() && z == b"gzip"), "the bench serves {} inflated only", data::HOUR);
                Some(WireValue::Int(1))
            }
            Some("response") => {
                host.parked.push((call_id as u32, WireValue::List(vec![WireValue::Int(200), WireValue::List(Vec::new())])));
                None
            }
            Some("read") => {
                let end = (host.at + CHUNK).min(host.body.len());
                let chunk = if host.at == end { WireValue::None } else { WireValue::Raw(host.body[host.at..end].to_vec()) };
                host.at = end;
                host.parked.push((call_id as u32, chunk));
                None
            }
            Some("close") => {
                host.closed = true;
                Some(WireValue::None)
            }
            other => panic!("the bench serves no net call {other:?}"),
        };
        match answer {
            Some(value) => {
                let handle = encode(&mut caller, &ex, &value);
                ex.memory.write(&mut caller, out as usize, &(handle as u32).to_le_bytes()).expect("writing runtime memory");
                0
            }
            None => 2,
        }
    }).expect("linking host_call_native");
}

/* what one run of a job over the hour measured and wrote */
struct Ran {
    instructions: u64,
    wall: Duration,
    first: Option<Duration>,
    peak: u64,
    printed: String,
    sent: Vec<(String, String)>,
}

/* stages `bytes` in the runtime for a call taking a pointer and a length */
fn staged<R>(store: &mut Store<Host>, ex: &Exports, bytes: &[u8], call: impl FnOnce(&mut Store<Host>, i32, i32) -> R) -> R {
    let ptr = ex.alloc.call(&mut *store, bytes.len().max(1) as i32).expect("wasm_alloc");
    ex.memory.write(&mut *store, ptr as usize, bytes).expect("writing runtime memory");
    let out = call(store, ptr, bytes.len() as i32);
    ex.free.call(&mut *store, (ptr, bytes.len().max(1) as i32)).expect("wasm_free");
    out
}

/* what the last call left in the runtime's out buffer */
fn out(store: &mut Store<Host>, ex: &Exports) -> Vec<u8> {
    let (ptr, len) = (ex.out_ptr.call(&mut *store, ()).expect("out_ptr"), ex.out_len.call(&mut *store, ()).expect("out_len"));
    let mut buf = vec![0u8; len.max(0) as usize];
    ex.memory.read(&*store, ptr as usize, &mut buf).expect("reading runtime memory");
    buf
}

/* resolves the job's imports as the CLI's walk does with net the one host module */
fn walk(store: &mut Store<Host>, inst: &Instance, ex: &Exports, src: &str) {
    if let Ok(carry) = inst.get_typed_func::<(i32, i32), ()>(&mut *store, "walk_carry") {
        staged(store, ex, b"", |s, p, n| carry.call(s, (p, n))).expect("walk_carry");
    }
    let start: TypedFunc<(i32, i32, i32, i32), i32> = func(inst, store, "walk_start");
    let fetched: TypedFunc<(i32, i32, i32), i32> = func(inst, store, "walk_fetched");
    let served: TypedFunc<(i32, i32), i32> = func(inst, store, "walk_served");
    let known: TypedFunc<(i32, i32), i32> = func(inst, store, "walk_known");
    let register: TypedFunc<(i32, i32, i32, i32, i32), ()> = func(inst, store, "register_native_module");
    let src_at = ex.alloc.call(&mut *store, src.len().max(1) as i32).expect("wasm_alloc");
    ex.memory.write(&mut *store, src_at as usize, src.as_bytes()).expect("writing runtime memory");
    staged(store, ex, b"net", |s, p, n| start.call(s, (src_at, src.len() as i32, p, n))).expect("walk_start");
    ex.free.call(&mut *store, (src_at, src.len().max(1) as i32)).expect("wasm_free");
    loop {
        let step: serde_json::Value = serde_json::from_slice(&out(store, ex)).expect("a walk step is JSON");
        if step.get("fetch").is_some() {
            // no manifest stands beside the job so every package holds net unbounded
            staged(store, ex, b"", |s, p, n| fetched.call(s, (p, n, 1))).expect("walk_fetched");
        } else if let Some(system) = step.get("system") {
            for dir in system["dirs"].as_array().into_iter().flatten() {
                let spec = format!("system:net@{}", dir[0].as_str().unwrap_or(""));
                let names = CALLS.join("\n");
                let spec_at = ex.alloc.call(&mut *store, spec.len() as i32).expect("wasm_alloc");
                ex.memory.write(&mut *store, spec_at as usize, spec.as_bytes()).expect("writing runtime memory");
                staged(store, ex, names.as_bytes(), |s, p, n| register.call(s, (spec_at, spec.len() as i32, p, n, 0))).expect("register_native_module");
                ex.free.call(&mut *store, (spec_at, spec.len() as i32)).expect("wasm_free");
            }
            staged(store, ex, b"", |s, p, n| served.call(s, (p, n))).expect("walk_served");
        } else if step.get("undeclared").is_some() {
            staged(store, ex, b"", |s, p, n| known.call(s, (p, n))).expect("walk_known");
        } else {
            let failures = step["done"].as_array().unwrap_or_else(|| panic!("an unknown walk step {step}"));
            assert!(failures.is_empty(), "the job's imports failed {failures:?}");
            return;
        }
    }
}

/* one run of `src` over the hour metered by fuel when `fuel` holds */
fn job(engine: &Engine, module: &Module, entry: &str, src: &str, body: &std::sync::Arc<Vec<u8>>, fuel: bool) -> Ran {
    let host = Host { body: body.clone(), at: 0, parked: Vec::new(), printed: String::new(), sent: Vec::new(), started: Instant::now(), first: None, closed: false };
    let mut store = Store::new(engine, host);
    if fuel {
        store.set_fuel(u64::MAX).expect("fuel is enabled");
    }
    let mut linker = Linker::new(engine);
    link(&mut linker);
    let inst = linker.instantiate(&mut store, module).expect("instantiating the runtime");
    let ex = Exports::bind(&inst, &mut store);
    func::<(), ()>(&inst, &mut store, "reset_modules").call(&mut store, ()).expect("reset_modules");
    staged(&mut store, &ex, entry.as_bytes(), |s, p, n| func::<(i32, i32), ()>(&inst, s, "set_entry").call(s, (p, n))).expect("set_entry");
    walk(&mut store, &inst, &ex, src);
    // a gigabyte and ops past any run so the measures never meet a limit
    func::<(i64, i64), ()>(&inst, &mut store, "set_limits").call(&mut store, (1 << 30, 1 << 50)).expect("set_limits");
    let (resume, deliver) = (func::<(), i32>(&inst, &mut store, "run_resume"), func::<(i32, i32), i32>(&inst, &mut store, "set_host_result_by_id"));
    let before = store.get_fuel().unwrap_or(0);
    let started = Instant::now();
    store.data_mut().started = started;
    let mut status = staged(&mut store, &ex, src.as_bytes(), |s, p, n| func::<(i32, i32), i32>(&inst, s, "run_start").call(s, (p, n))).expect("run_start") as u32;
    loop {
        status = match status >> 29 {
            DONE | EXIT => break,
            PREEMPTED => resume.call(&mut store, ()).expect("run_resume") as u32,
            WAITING => {
                for (id, value) in std::mem::take(&mut store.data_mut().parked) {
                    let handle = encode(&mut store, &ex, &value);
                    assert_eq!(deliver.call(&mut store, (id as i32, handle)).expect("set_host_result_by_id"), 0, "nothing waits on call {id}");
                }
                resume.call(&mut store, ()).expect("run_resume") as u32
            }
            ERROR => panic!("{entry} failed: {}", String::from_utf8_lossy(&out(&mut store, &ex))),
            kind => panic!("{entry} parked on {kind}, which the bench never serves"),
        };
    }
    let wall = started.elapsed();
    let instructions = before - store.get_fuel().unwrap_or(0);
    let peak = func::<(), u64>(&inst, &mut store, "memory_peak").call(&mut store, ()).expect("memory_peak");
    let host = store.into_data();
    assert_eq!(host.at, host.body.len(), "{entry} stopped before the end of the hour");
    Ran { instructions, wall, first: host.first, peak, printed: host.printed, sent: host.sent }
}

/* a runtime artifact compiled under a config with and without fuel */
fn engines(path: &Path) -> [(Engine, Module); 2] {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}, build it with make bench-data", path.display()));
    [true, false].map(|fuel| {
        let mut config = Config::new();
        config.consume_fuel(fuel);
        // the stack the CLI gives the engine so the deepest nesting runs alike everywhere
        config.max_wasm_stack(1 << 20);
        let engine = Engine::new(&config).expect("wasmtime engine");
        let module = Module::new(&engine, &bytes).expect("compiling the runtime");
        (engine, module)
    })
}

/* the two reports tables of one hour so the runs can be held to agree */
fn reports(printed: &str, sent: &[(String, String)]) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Vec<String>>) {
    let mut mini: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut at = String::new();
    for line in printed.lines() {
        if REPORTS.contains(&line) {
            at = line.to_string();
            mini.entry(at.clone()).or_default();
        } else {
            mini.entry(at.clone()).or_default().push(line.to_string());
        }
    }
    let mut tiny: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (channel, line) in sent {
        let report = channel.strip_prefix("file:").unwrap_or(channel).to_string();
        let shown = if report.ends_with(".csv") { line.clone() } else {
            let row: serde_json::Value = serde_json::from_str(line).expect("tiny writes a line of JSON");
            let repo = row["repo"]["name"].as_str().unwrap_or("");
            match report.as_str() {
                "busy_branches" => format!("{:>6}  {repo} {}", row["count"].as_i64().unwrap_or(0), row["branch"].as_str().unwrap_or("")),
                "time_to_close" => format!("{:>8.2}  {repo}", row["average"].as_f64().unwrap_or(f64::NAN)),
                _ => format!("{:>6}  {repo}", row["count"].as_i64().unwrap_or(0)),
            }
        };
        tiny.entry(report).or_default().push(shown);
    }
    (mini, tiny)
}

/* the hour's records as literals of the fields the counting loop reads in chunks */
fn chunks(hour: &[u8]) -> Vec<(usize, String)> {
    let quote = |v: &serde_json::Value| format!("'{}'", v.as_str().unwrap_or("").replace('\\', "\\\\").replace('\'', "\\'"));
    let lines: Vec<&[u8]> = hour.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
    lines.chunks(LITERALS).map(|part| {
        let records: Vec<String> = part.iter().map(|line| {
            let e: serde_json::Value = serde_json::from_slice(line).expect("an event of the hour reads");
            let repo = if e["repo"]["name"].is_string() { format!("{{'name': {}}}", quote(&e["repo"]["name"])) } else { String::from("{}") };
            format!("    {{'type': {}, 'actor': {{'login': {}}}, 'repo': {repo}}},", quote(&e["type"]), quote(&e["actor"]["login"]))
        }).collect();
        (part.len(), format!("records = [\n{}\n]\n", records.join("\n")))
    }).collect()
}

// the top_stars counting loop in a function which today spells with fn
const COUNTING: &str = "def top_stars(records):
    stars = {}
    for e in records:
        if e['actor']['login'].endswith('[bot]'):
            continue
        if e['type'] == 'WatchEvent':
            repo = e['repo']['name']
            stars[repo] = stars.get(repo, 0) + 1
    return sorted(stars.items(), key=lambda kv: -kv[1])[:10]
top = top_stars(records)
";

/* the instructions one corpus-style case runs on a fresh instance as the bench counts them */
fn case(engine: &Engine, module: &Module, src: &str) -> u64 {
    let mut store = Store::new(engine, ());
    store.set_fuel(u64::MAX).expect("fuel is enabled");
    let mut linker = Linker::new(engine);
    linker.define_unknown_imports_as_default_values(&mut store, module).expect("stubbing the host imports");
    let inst = linker.instantiate(&mut store, module).expect("instantiating the engine");
    let memory = inst.get_memory(&mut store, "memory").expect("the engine exports memory");
    let call = |store: &mut Store<()>, name: &str| inst.get_typed_func::<(u32, u32), u32>(&mut *store, name).unwrap_or_else(|e| panic!("engine export {name}: {e}"));
    inst.get_typed_func::<(i64, i64), ()>(&mut store, "set_limits").expect("set_limits").call(&mut store, (1 << 30, 1 << 40)).expect("set_limits");
    let ptr = inst.get_typed_func::<u32, u32>(&mut store, "wasm_alloc").expect("wasm_alloc").call(&mut store, src.len() as u32).expect("wasm_alloc");
    memory.write(&mut store, ptr as usize, src.as_bytes()).expect("writing engine memory");
    let before = store.get_fuel().unwrap_or(0);
    let status = call(&mut store, "run_start").call(&mut store, (ptr, src.len() as u32)).expect("run_start");
    assert_eq!(status >> 29, DONE, "a counting case failed on an engine");
    before - store.get_fuel().unwrap_or(0)
}

/* `n` grouped by three digits to read at a glance */
fn grouped(n: f64) -> String {
    let digits = format!("{n:.0}");
    let mut out = String::new();
    for (k, c) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}

fn main() {
    let root = Path::new(ROOT);
    let artifact = |var: &str, path: &str| std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| root.join(path));
    let (mini_wasm, tiny_wasm, main_wasm) = (
        artifact("EDGE_WASM", "target/wasm32-unknown-unknown/cli/edge.wasm"),
        artifact("TINY_WASM", "target/wasm32-unknown-unknown/cli/tiny.wasm"),
        artifact("MAIN_WASM", "target/main-a0fe0d84/target/wasm32-unknown-unknown/cli/compiler.wasm"),
    );
    let dir = data::cache(&root.join("target/tmp"));
    let hour = std::sync::Arc::new(data::inflate(&data::hour(&dir)));
    let records = hour.iter().filter(|&&b| b == b'\n').count();
    println!("{} records, {:.1} MB inflated, read through net in chunks of {} KB", grouped(records as f64), hour.len() as f64 / 1e6, CHUNK >> 10);

    let jobs = [("tiny", &tiny_wasm, "github-hour.flow.yaml"), ("mini", &mini_wasm, "github-hour.mini")];
    let mut outputs = Vec::new();
    println!("{:<6}{:>16}{:>24}{:>14}{:>22}", "", "records/s", "instructions/record", "peak MB", "first output after");
    for (name, wasm, file) in jobs {
        let src = std::fs::read_to_string(root.join("examples/data").join(file)).expect("reading the job");
        let [(fueled, metered), (plain, timed)] = engines(wasm);
        let counted = job(&fueled, &metered, file, &src, &hour, true);
        let ran = job(&plain, &timed, file, &src, &hour, false);
        let first = ran.first.map_or_else(|| String::from("none"), |d| format!("{:.2} s", d.as_secs_f64()));
        println!("{name:<6}{:>16}{:>24}{:>14.1}{:>22}", grouped(records as f64 / ran.wall.as_secs_f64()), grouped(counted.instructions as f64 / records as f64), counted.peak as f64 / (1 << 20) as f64, first);
        outputs.push(ran);
    }
    let (mini, _) = reports(&outputs[1].printed, &[]);
    let (_, tiny) = reports("", &outputs[0].sent);
    let rows: usize = tiny.values().map(Vec::len).sum();
    if mini == tiny {
        println!("both jobs wrote the same {} rows across {} reports", grouped(rows as f64), tiny.len());
    } else {
        let first = tiny.keys().find(|k| mini.get(*k) != tiny.get(*k)).cloned().unwrap_or_default();
        println!("the jobs disagree, first on {first}");
        std::process::exit(1);
    }

    // the counting loop alone is the case minus its literals on each engine
    let [(today_engine, today), _] = engines(&mini_wasm);
    let [(main_engine, main), _] = engines(&main_wasm);
    let (mut on_main, mut on_today) = (0u64, 0u64);
    for (_, literals) in chunks(&hour) {
        let counting = format!("{literals}{COUNTING}");
        on_main += case(&main_engine, &main, &counting) - case(&main_engine, &main, &literals);
        let counting = format!("{literals}{}", COUNTING.replacen("def ", "fn ", 1));
        on_today += case(&today_engine, &today, &counting) - case(&today_engine, &today, &literals);
    }
    let (per_main, per_today) = (on_main as f64 / records as f64, on_today as f64 / records as f64);
    println!("top_stars counting loop over the hour as literals in chunks of {}", grouped(LITERALS as f64));
    println!("  main a0fe0d84 {:>8} instructions/record", grouped(per_main));
    println!("  today         {:>8} instructions/record  {:+.2}%", grouped(per_today), (per_today / per_main - 1.0) * 100.0);
}
