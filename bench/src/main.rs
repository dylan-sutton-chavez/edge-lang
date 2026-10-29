use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use wasmtime::{Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

#[path = "../../tests/cases.rs"]
mod cases;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.snapshot");
// only budget-exhausting cases reach this many ops and they end the same way sooner
const OPS: i64 = 10_000_000;
// the MB the memory limit counts in
const MB: f64 = (1 << 20) as f64;
// a report names at most this many cases
const TOP: usize = 5;
// the threads a layout check runs its cases on beside other jobs
const LAYOUT_THREADS: usize = 3;

// the wasm instructions and memory peak in MB of each case alike on every machine
#[derive(serde::Deserialize)]
struct Snapshot {
    rustc: String,
    threshold: f64,
    case_threshold: f64,
    // the memory a case may move before its threshold applies so one small object passes
    memory_floor: f64,
    cases: BTreeMap<String, (f64, f64)>,
}

// how a report words one measure so instructions and memory meet the same rules
struct Measure {
    pick: fn((f64, f64)) -> f64,
    floor: fn(&Snapshot) -> f64,
    unit: &'static str,
    // decimals of one case and of a total
    places: usize,
    total_places: usize,
    grew: &'static str,
    shrank: &'static str,
    most_grown: &'static str,
    most_shrunk: &'static str,
    own: &'static str,
    total: (&'static str, &'static str),
}

const INSTRUCTIONS: Measure = Measure { pick: |c| c.0, floor: |_| 0.0, unit: "instructions", places: 0, total_places: 0, grew: "runs more instructions", shrank: "runs fewer instructions", most_grown: "Most grown", most_shrunk: "Biggest drops", own: "", total: ("runs", "summed over its cases") };
const MEMORY: Measure = Measure { pick: |c| c.1, floor: |s| s.memory_floor, unit: "MB", places: 6, total_places: 3, grew: "holds more memory", shrank: "holds less memory", most_grown: "Most grown", most_shrunk: "Biggest drops", own: " in memory", total: ("peaks at", "summed over its cases") };

fn main() {
    let update = std::env::args().any(|a| a == "--update");
    // a snapshot that takes a slower case needs it said outright
    let accept_slower = std::env::args().any(|a| a == "--accept-slower");
    let wasm = std::env::var_os("EDGE_WASM").map(PathBuf::from).unwrap_or_else(|| Path::new(ROOT).join("target/wasm32-unknown-unknown/cli/edge.wasm"));
    let built = std::fs::metadata(&wasm).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    // the engine the lib and the reference runtime all build into edge.wasm
    if ["src", "lib/src", "examples/mini/src"].iter().any(|dir| built < newest(&Path::new(ROOT).join(dir))) {
        println!("{} is older than its sources, build it with make wasm-cli", wasm.display());
        std::process::exit(1);
    }
    let bytes = std::fs::read(&wasm).expect("reading edge.wasm");
    let mut config = Config::new();
    config.consume_fuel(true);
    // the stack the CLI gives the engine so the deepest nesting runs alike everywhere
    config.max_wasm_stack(1 << 20);
    let engine = Engine::new(&config).expect("wasmtime engine");
    let module = Module::new(&engine, &bytes).expect("compiling edge.wasm");

    let cases = cases::vm_cases(Path::new(ROOT));
    println!("tests/cases/vm  {} cases", cases.len());
    // a build with its static data shifted must count every case alike
    if let Some(other) = std::env::args().skip_while(|a| a != "--layout").nth(1) {
        return layout(&engine, &module, Path::new(&other), &cases);
    }
    let ran: Vec<(String, (u64, u64))> = cases.iter().map(|case| (cases::key(case), run(&engine, &module, case))).collect();
    let measured = ran.iter().map(|(k, (ops, peak))| (k.clone(), (*ops as f64, (*peak as f64 / MB * 1e6).round() / 1e6))).collect();
    let rustc = Command::new("rustc").arg("--version").current_dir(ROOT).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let now = Snapshot { rustc, threshold: 0.005, case_threshold: 0.05, memory_floor: 0.004, cases: measured };
    let sources: BTreeMap<String, &str> = cases.iter().map(|c| (cases::key(c), c["src"].as_str().unwrap_or(""))).collect();

    // a missing snapshot is taken unchecked and a broken one fails unless `--update` replaces it
    let last = match std::fs::read_to_string(SNAPSHOT) {
        Err(_) => None,
        Ok(text) => match serde_json::from_str::<Snapshot>(&text) {
            Ok(snapshot) => Some(snapshot),
            Err(_) if update => None,
            Err(e) => {
                annotate("error", &format!("bench/.snapshot does not parse, {e}, take it again with --update."));
                std::process::exit(1);
            }
        },
    };
    let (failures, slower) = last.as_ref().map(|l| check(l, &now, &sources)).unwrap_or_default();
    for failure in &failures {
        annotate("error", failure);
    }
    if update && slower > 0 && !accept_slower {
        annotate("error", &format!("{slower} cases grew past their threshold, so --update keeps the snapshot. Fix them, or accept them with make bench-accept-slower if they are meant to."));
        std::process::exit(1);
    }
    if update || last.is_none() {
        let (threshold, case_threshold, memory_floor) = last.map_or((now.threshold, now.case_threshold, now.memory_floor), |l| (l.threshold, l.case_threshold, l.memory_floor));
        write(&Snapshot { threshold, case_threshold, memory_floor, ..now });
        return println!("  snapshot written");
    }
    if !failures.is_empty() {
        std::process::exit(1);
    }
}

/* a paragraph per broken rule and how many cases grew past their threshold */
fn check(last: &Snapshot, now: &Snapshot, sources: &BTreeMap<String, &str>) -> (Vec<String>, usize) {
    // another rustc builds another edge.wasm whose instructions say nothing about this code
    if last.rustc != now.rustc {
        return (vec![format!("The snapshot was taken with {}, this is {}, regenerate it with {}.", last.rustc, now.rustc, last.rustc)], 0);
    }
    let mut failures = Vec::new();
    let new = now.cases.keys().filter(|k| !last.cases.contains_key(*k)).count();
    let gone = last.cases.keys().filter(|k| !now.cases.contains_key(*k)).count();
    if new + gone > 0 {
        failures.push(format!("{new} cases are missing from the snapshot and {gone} entries have no case, run --update."));
    }
    let (instructions, grew) = compare(&INSTRUCTIONS, last, now, sources);
    let (memory, grew_memory) = compare(&MEMORY, last, now, sources);
    failures.extend(instructions);
    failures.extend(memory);
    (failures, grew + grew_memory)
}

/* holds a geometric mean to the band and each case to its own threshold */
fn compare(m: &Measure, last: &Snapshot, now: &Snapshot, sources: &BTreeMap<String, &str>) -> (Vec<String>, usize) {
    let mut failures = Vec::new();
    let (unit, floor, p) = (m.unit, (m.floor)(last), m.total_places);
    let all: Vec<(&String, f64, f64)> = now.cases.iter().filter_map(|(k, &n)| last.cases.get(k).map(|&l| (k, (m.pick)(l), (m.pick)(n)))).collect();
    // every case weighs the same in a geometric mean so no long case decides it
    let paired: Vec<(&String, f64, f64)> = all.iter().copied().filter(|&(_, l, n)| l > 0.0 && n > 0.0).collect();
    if paired.is_empty() {
        return (failures, 0);
    }
    // a move under the floor counts as none here as it does per case
    let change = (paired.iter().map(|&(_, l, n)| if (n - l).abs() > floor { (n / l).ln() } else { 0.0 }).sum::<f64>() / paired.len().max(1) as f64).exp() - 1.0;
    let (total, before) = paired.iter().fold((0.0, 0.0), |(t, w), &(_, l, n)| (t + n, w + l));
    let moved = |grew: bool| {
        let mut picked: Vec<&(&String, f64, f64)> = paired.iter().filter(|(_, l, n)| if grew { n > l } else { n < l }).collect();
        picked.sort_by(|a, b| (b.2 / b.1).ln().abs().total_cmp(&(a.2 / a.1).ln().abs()));
        picked.iter().map(|&&(k, l, n)| moved_line(m, sources, k, l, n)).collect::<Vec<_>>()
    };
    if change > last.threshold {
        failures.push(format!("tests/cases/vm {}, {} {unit} against {} {unit}, {:+.2}% across {} cases. Fix it, or accept it with --update if it is intended. {}{}", m.grew, amount(total, p), amount(before, p), change * 100.0, paired.len(), m.most_grown, list(&moved(true))));
    } else if change < -last.threshold {
        failures.push(format!("tests/cases/vm {}, {} {unit} against {} {unit}, {:+.2}%. The snapshot no longer matches the code, run --update and commit it with the change. {}{}", m.shrank, amount(total, p), amount(before, p), change * 100.0, m.most_shrunk, list(&moved(false))));
    }
    // a case at 0 has no ratio so leaving or reaching 0 sorts it first
    let by = |&(_, l, n): &(&String, f64, f64)| if l > 0.0 && n > 0.0 { (n / l).ln().abs() } else { f64::INFINITY };
    let mut apart: Vec<(&String, f64, f64)> = all.iter().copied().filter(|&(_, l, n)| (n - l).abs() > floor && (l == 0.0 || n == 0.0 || (n / l - 1.0).abs() > last.case_threshold)).collect();
    apart.sort_by(|a, b| by(b).total_cmp(&by(a)));
    // grown cases are listed first and apart so slower ones never hide among faster
    let (grew, shrank): (Vec<_>, Vec<_>) = apart.iter().partition(|&&(_, l, n)| n > l);
    let past = |sign: &str| if floor > 0.0 { format!("{sign}{:.0}% and {floor} {unit}", last.case_threshold * 100.0) } else { format!("{sign}{:.0}%", last.case_threshold * 100.0) };
    let lines = |cases: &[&(&String, f64, f64)]| cases.iter().map(|&&(k, l, n)| moved_line(m, sources, k, l, n)).collect::<Vec<_>>();
    if !grew.is_empty() {
        failures.push(format!("{} cases grew past {} on their own{}. Fix them, they are slower than the snapshot.{}", grew.len(), past("+"), m.own, list(&lines(&grew))));
    }
    if !shrank.is_empty() {
        failures.push(format!("{} cases shrank past {} on their own{}. Accept them with --update.{}", shrank.len(), past("-"), m.own, list(&lines(&shrank))));
    }

    // printed even on a failure so new cases never hide how existing ones moved
    annotate("notice", &format!("tests/cases/vm {} {} {unit} {}, {:+.2}% against the snapshot across {} cases, the band is ±{:.1}%.", m.total.0, amount(total, p), m.total.1, change * 100.0, paired.len(), last.threshold * 100.0));
    (failures, grew.len())
}

fn moved_line(m: &Measure, sources: &BTreeMap<String, &str>, key: &str, last: f64, now: f64) -> String {
    let (unit, places) = (m.unit, m.places);
    let change = if last > 0.0 { format!("{:+.1}%", (now / last - 1.0) * 100.0) } else { String::from("from 0") };
    format!("{} {} {unit} to {} {unit} ({change})", source(sources, key), amount(last, places), amount(now, places))
}

/* `places` decimals or a whole count grouped by three digits to read at a glance */
fn amount(value: f64, places: usize) -> String {
    if places > 0 { return format!("{value:.places$}"); }
    let digits = format!("{value:.0}");
    let mut grouped = String::new();
    for (k, c) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k) % 3 == 0 { grouped.push(','); }
        grouped.push(c);
    }
    grouped
}

fn source(sources: &BTreeMap<String, &str>, key: &str) -> String {
    format!("{:?}", sources.get(key).unwrap_or(&"").chars().take(50).collect::<String>())
}

/* the first cases of a finding one per line and how many more there are */
fn list(lines: &[String]) -> String {
    let more = lines.len().saturating_sub(TOP);
    let shown = lines.iter().take(TOP).map(|l| format!("\n  {l}")).collect::<String>();
    if more > 0 { format!("{shown}\n  and {more} more.") } else { shown }
}

/* prints a finding and under GitHub Actions also raises it as a run annotation */
fn annotate(level: &str, message: &str) {
    println!("  {message}");
    if std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true") {
        println!("::{level} title=Bench::{}", message.trim_end().replace('%', "%25").replace('\n', "%0A"));
    }
}

/* each case on edge.wasm and on the shifted build `other` which must count it alike */
fn layout(engine: &Engine, module: &Module, other: &Path, cases: &[serde_json::Value]) {
    let built = std::fs::metadata(other).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    if ["src", "lib/src", "examples/mini/src"].iter().any(|dir| built < newest(&Path::new(ROOT).join(dir))) {
        println!("{} is older than its sources, build it with make bench-layout", other.display());
        std::process::exit(1);
    }
    let shifted = Module::new(engine, std::fs::read(other).expect("reading the shifted build")).expect("compiling the shifted build");
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut apart: Vec<(String, u64, u64)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..LAYOUT_THREADS).map(|_| s.spawn(|| {
            let mut found = Vec::new();
            while let Some(case) = cases.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                let (a, b) = (run(engine, module, case).0, run(engine, &shifted, case).0);
                if a != b { found.push((cases::key(case), a, b)); }
            }
            found
        })).collect();
        workers.into_iter().flat_map(|w| w.join().expect("a layout worker")).collect()
    });
    apart.sort();
    // a shift past a page boundary moves the heap too which dlmalloc grows from there
    let pages = |m: &Module| m.exports().find_map(|e| e.ty().memory().map(|t| t.minimum()));
    if pages(module) != pages(&shifted) { println!("  the shifted build starts its heap on another page so heap addresses moved too"); }
    if apart.is_empty() {
        return println!("  every case runs as many instructions with its static data shifted");
    }
    let sources: BTreeMap<String, &str> = cases.iter().map(|c| (cases::key(c), c["src"].as_str().unwrap_or(""))).collect();
    let lines: Vec<String> = apart.iter().map(|(k, a, b)| format!("{k} {} {} to {} instructions ({:+})", source(&sources, k), amount(*a as f64, 0), amount(*b as f64, 0), *b as i64 - *a as i64)).collect();
    annotate("error", &format!("{} cases run another count once static data shifts, so a lookup follows an address.{}", apart.len(), list(&lines)));
    // the annotation names a few and the log every one
    for line in lines.iter().skip(TOP) { println!("  {line}"); }
    std::process::exit(1);
}

/* the instructions one case runs on a fresh instance and its memory peak in bytes */
fn run(engine: &Engine, module: &Module, case: &serde_json::Value) -> (u64, u64) {
    let texts = |field: &str| case[field].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
    let (input, events) = (texts("input"), [texts("events"), texts("interactive_events")].concat());
    let mut store = Store::new(engine, ());
    store.set_fuel(u64::MAX).expect("fuel is enabled");
    let mut linker = Linker::new(engine);
    linker.define_unknown_imports_as_default_values(&mut store, module).expect("stubbing the host imports");
    let inst = linker.instantiate(&mut store, module).expect("instantiating edge.wasm");
    let memory = inst.get_memory(&mut store, "memory").expect("edge.wasm exports memory");
    func::<(i64, i64), ()>(&inst, &mut store, "set_limits").call(&mut store, (0, OPS)).ok();
    if !input.is_empty() {
        let (ptr, len) = stage(&inst, &mut store, memory, &input.join("\n"));
        func::<(u32, u32), ()>(&inst, &mut store, "set_input").call(&mut store, (ptr, len)).ok();
    }
    let (ptr, len) = stage(&inst, &mut store, memory, case["src"].as_str().unwrap_or(""));
    let (resume, push) = (func::<(), u32>(&inst, &mut store, "run_resume"), func::<(u32, u32), i32>(&inst, &mut store, "run_push_event"));
    let mut events = events.iter();

    let before = store.get_fuel().unwrap_or(0);
    let mut status = func::<(u32, u32), u32>(&inst, &mut store, "run_start").call(&mut store, (ptr, len));
    // preempts resume and event waits take the next event and anything else ends the run
    while let Ok(s) = status {
        status = match s >> 29 {
            7 => resume.call(&mut store, ()),
            3 => match events.next() {
                Some(event) => {
                    let (ptr, len) = stage(&inst, &mut store, memory, event);
                    push.call(&mut store, (ptr, len)).ok();
                    resume.call(&mut store, ())
                }
                None => break,
            },
            _ => break,
        };
    }
    let ran = before - store.get_fuel().unwrap_or(0);
    (ran, func::<(), u64>(&inst, &mut store, "memory_peak").call(&mut store, ()).unwrap_or(0))
}

fn func<P: wasmtime::WasmParams, R: wasmtime::WasmResults>(inst: &Instance, store: &mut Store<()>, name: &str) -> TypedFunc<P, R> {
    inst.get_typed_func(&mut *store, name).unwrap_or_else(|e| panic!("edge.wasm export {name}: {e}"))
}

/* copies `text` into instance memory where the exports read their arguments */
fn stage(inst: &Instance, store: &mut Store<()>, memory: Memory, text: &str) -> (u32, u32) {
    let ptr = func::<u32, u32>(inst, store, "wasm_alloc").call(&mut *store, text.len().max(1) as u32).expect("wasm_alloc");
    memory.write(&mut *store, ptr as usize, text.as_bytes()).expect("writing into edge.wasm memory");
    (ptr, text.len() as u32)
}

/* one case per line in whole instructions and decimal MB so each reads without converting */
fn write(snapshot: &Snapshot) {
    let (t, m) = (INSTRUCTIONS.places, MEMORY.places);
    let cases: Vec<String> = snapshot.cases.iter().map(|(k, (s, mb))| format!("    \"{k}\": [{s:.t$}, {mb:.m$}]")).collect();
    let json = format!(
        "{{\n  \"rustc\": {:?},\n  \"threshold\": {},\n  \"case_threshold\": {},\n  \"memory_floor\": {},\n  \"cases\": {{\n{}\n  }}\n}}\n",
        snapshot.rustc, snapshot.threshold, snapshot.case_threshold, snapshot.memory_floor, cases.join(",\n")
    );
    std::fs::write(SNAPSHOT, json).expect("writing bench/.snapshot");
}

/* the latest change under `dir` which edge.wasm must not predate */
fn newest(dir: &Path) -> SystemTime {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| {
        let path = entry.path();
        if path.is_dir() { newest(&path) } else { entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH) }
    }).max().unwrap_or(SystemTime::UNIX_EPOCH)
}
