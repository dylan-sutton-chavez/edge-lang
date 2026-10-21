use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use wasmtime::{Config, Engine, Linker, Module, Store};
use wasmtime_wasi::{p2::pipe::MemoryOutputPipe, I32Exit, WasiCtxBuilder};

use super::{amount, annotate, ir, list, ROOT};

const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/llvm/cases");
const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/llvm/.snapshot");
// Edge may run this many times the instructions of the same program in Rust, where its types are proven.
const GATE: f64 = 1.3;
// The four counts of a case, in the order the snapshot keeps them.
const MODES: [&str; 4] = ["edge wasm", "rust wasm", "edge native", "rust native"];

// What a run printed to stdout, its exit code and the instructions it ran.
type Run = (String, i32, u64);

// The instructions each case ran in every mode, the same on every machine and every run.
#[derive(serde::Deserialize)]
struct Snapshot {
    rustc: String,
    case_threshold: f64,
    cases: BTreeMap<String, [f64; 4]>,
}

/* Each pair of bench/llvm/cases, a Python program and the same program in Rust, built to wasm and to native, counted,
   held to the gate against Rust and to the snapshot against itself. */
pub fn main(update: bool) {
    let edge = std::env::var_os("EDGE_BIN").map(PathBuf::from).unwrap_or_else(|| Path::new(ROOT).join("cli/target/release/edge"));
    let out = Path::new(ROOT).join("target/bench-llvm");
    std::fs::create_dir_all(&out).expect("creating target/bench-llvm");
    std::fs::write(out.join("empty.rs"), "#![no_main]\n").unwrap();
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).expect("wasmtime engine");
    let names: std::collections::BTreeSet<String> = std::fs::read_dir(CASES).expect("reading bench/llvm/cases").flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".py").map(String::from)).collect();

    let version = sh(Command::new("rustc").arg("-vV"));
    let (rustc, host) = (version.lines().next().unwrap_or("").to_string(), version.lines().find_map(|l| l.strip_prefix("host: ")).expect("rustc -vV names its host").to_string());
    // The VM module edge links for the host, where the tag tests the IR is read against are.
    let vm = std::fs::read_to_string(Path::new(ROOT).join(format!("target/bitcode/{}/vm.ll", host.replace("-gnu", "-musl")))).expect("reading the host's VM module, run make bitcode");
    let (mut failures, mut cases) = (Vec::new(), BTreeMap::new());
    for name in &names {
        let (py, rs) = (Path::new(CASES).join(format!("{name}.py")), Path::new(CASES).join(format!("{name}.rs")));
        let at = |ext: &str| out.join(format!("{name}{ext}"));
        let edge = |args: &[&Path]| sh(Command::new(&edge).arg("build").args(args));
        let rustc = |args: &[&Path]| sh(Command::new("rustc").args(["-Copt-level=3", "-Cpanic=abort"]).args(args));
        edge(&[Path::new("--wasm"), &py, Path::new("-o"), &at(".edge.wasm")]);
        rustc(&[Path::new("-Clto=fat"), Path::new("--target=wasm32-wasip1"), &rs, Path::new("-o"), &at(".rust.wasm")]);
        // The module is written before the counter goes in, so one build gives both.
        let built = edge(&[Path::new("--count"), Path::new("--emit-llvm"), &at(".edge.ll"), &py, Path::new("-o"), &at(".edge")]);
        rustc(&[Path::new("-Clto=fat"), Path::new("--emit=llvm-bc"), &rs, Path::new("-o"), &at(".rust.bc")]);
        // The object goes to rustc's own target, not the static musl edge builds by default.
        edge(&[Path::new("--count"), Path::new("--target"), Path::new(&host), &at(".rust.bc"), Path::new("-o"), &at(".rust.o")]);
        // Linked apart and without LTO, so std comes from its rlibs around the counted object.
        let link = format!("-Clink-arg={}", at(".rust.o").display());
        rustc(&[Path::new(&link), &out.join("empty.rs"), Path::new("-o"), &at(".rust")]);
        let runs = [wasm(&engine, &at(".edge.wasm"), true), wasm(&engine, &at(".rust.wasm"), false), native(&at(".edge")), native(&at(".rust"))];

        if let Some(k) = (1..4).find(|&k| runs[k].0 != runs[0].0 || runs[k].1 != runs[0].1) {
            failures.push(format!("{name} prints {:?} and ends {} in {}, and prints {:?} and ends {} in {}.", runs[0].0, runs[0].1, MODES[0], runs[k].0, runs[k].1, MODES[k]));
        }
        let counts = runs.map(|r| r.2 as f64);
        let (wasm_ratio, native_ratio) = (counts[0] / counts[1], counts[2] / counts[3]);
        let types = built.lines().find(|l| l.starts_with("types:")).unwrap_or("types: not reported");
        annotate("notice", &format!("{name}: wasm {:.2}x of Rust ({} against {}), native {:.2}x ({} against {}), {types}, left in the optimized IR {}.",
            wasm_ratio, amount(counts[0], 0), amount(counts[1], 0), native_ratio, amount(counts[2], 0), amount(counts[3], 0), left(&std::fs::read_to_string(at(".edge.ll")).unwrap(), &vm)));
        if wasm_ratio > GATE || native_ratio > GATE {
            failures.push(format!("{name} runs {wasm_ratio:.2}x of Rust on wasm and {native_ratio:.2}x native, past the {GATE}x gate."));
        }
        cases.insert(name.clone(), counts);
    }

    let last: Option<Snapshot> = std::fs::read_to_string(SNAPSHOT).ok().map(|t| serde_json::from_str(&t).expect("bench/llvm/.snapshot does not parse"));
    if let Some(last) = &last {
        failures.extend(check(last, &rustc, &cases));
    }
    for failure in &failures {
        annotate("error", failure);
    }
    // A first snapshot is only taken from a run that passes, so a failing one never becomes the baseline.
    if update || (last.is_none() && failures.is_empty()) {
        let threshold = last.map_or(0.01, |l| l.case_threshold);
        let lines: Vec<String> = cases.iter().map(|(k, c)| format!("    \"{k}\": [{:.0}, {:.0}, {:.0}, {:.0}]", c[0], c[1], c[2], c[3])).collect();
        let json = format!("{{\n  \"rustc\": {rustc:?},\n  \"case_threshold\": {threshold},\n  \"cases\": {{\n{}\n  }}\n}}\n", lines.join(",\n"));
        std::fs::write(SNAPSHOT, json).expect("writing bench/llvm/.snapshot");
        return println!("  snapshot written");
    }
    if !failures.is_empty() {
        std::process::exit(1);
    }
}

/* The snapshot's rules: the same rustc, the same cases, and no count moved past the case threshold. */
fn check(last: &Snapshot, rustc: &str, cases: &BTreeMap<String, [f64; 4]>) -> Vec<String> {
    if last.rustc != rustc {
        return vec![format!("The snapshot was taken with {}, this is {rustc}, regenerate it with {}.", last.rustc, last.rustc)];
    }
    if last.cases.keys().ne(cases.keys()) {
        return vec![String::from("bench/llvm/cases and the snapshot name different cases, run --update.")];
    }
    // A count that grew is slower than the snapshot and one that shrank leaves it stale, so both are findings, each with its sign.
    let moved: Vec<String> = cases.iter().flat_map(|(k, now)| (0..4).filter_map(move |m| {
        let (l, n) = (last.cases[k][m], now[m]);
        ((n / l - 1.0).abs() > last.case_threshold).then(|| format!("{k} {} {} to {} instructions ({:+.1}%)", MODES[m], amount(l, 0), amount(n, 0), (n / l - 1.0) * 100.0))
    })).collect();
    if moved.is_empty() { return moved }
    vec![format!("{} counts moved past ±{:.0}%. Fix the ones that grew; accept the ones that shrank with --update.{}", moved.len(), last.case_threshold * 100.0, list(&moved))]
}

/* What the program's module still holds that a proven type takes out, over every function of it: LLVM inlines the
   blocks into one another and into main, so no name of a block is sure to survive. */
fn left(ir: &str, vm: &str) -> String {
    let functions = ir::bodies(ir, "");
    let sum = |f: &dyn Fn(&str) -> usize| functions.iter().map(|b| f(b)).sum::<usize>();
    let (tags, overflow, allocations) = (sum(&|b| ir::checks(vm, b, "is_")), sum(&|b| ir::checks(vm, b, "fits_int")), sum(&|b| ir::calls(b, "edge_alloc")));
    // The program's module calls into vm.o, so the interpreter is reached through the VM's own call graph too.
    let both = format!("{ir}\n{vm}");
    let may = ir::reach(&both, "edge_vm_loop");
    let vm = sum(&|b| ir::reaching(&may, b, "edge_vm_loop"));
    format!("{tags} tag checks, {overflow} overflow checks, {allocations} allocations, {vm} calls that may reach the interpreter")
}

/* Runs one .wasm under WASI, and returns what it printed to stdout, its exit code and the instructions it ran. Edge's
   .wasm may import only fd_write and proc_exit, Rust's std asks for its environment as well. */
fn wasm(engine: &Engine, path: &Path, edge: bool) -> Run {
    let module = Module::from_file(engine, path).unwrap_or_else(|e| panic!("compiling {}: {e}", path.display()));
    let imports: Vec<String> = module.imports().map(|i| format!("{}.{}", i.module(), i.name())).collect();
    assert!(!edge || imports.iter().all(|i| ["fd_write", "proc_exit"].map(|f| format!("wasi_snapshot_preview1.{f}")).contains(i)), "{} imports {imports:?}, past WASI's fd_write and proc_exit", path.display());
    let (stdout, stderr) = (MemoryOutputPipe::new(1 << 20), MemoryOutputPipe::new(1 << 20));
    let mut store = Store::new(engine, WasiCtxBuilder::new().stdout(stdout.clone()).stderr(stderr).build_p1());
    store.set_fuel(u64::MAX).expect("fuel is enabled");
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |wasi| wasi).expect("linking WASI");
    let inst = linker.instantiate(&mut store, &module).unwrap_or_else(|e| panic!("instantiating {}: {e}", path.display()));
    let code = match inst.get_typed_func::<(), ()>(&mut store, "_start").unwrap().call(&mut store, ()) {
        Ok(()) => 0,
        Err(e) => e.downcast_ref::<I32Exit>().unwrap_or_else(|| panic!("{} traps: {e}", path.display())).0,
    };
    (String::from_utf8_lossy(&stdout.contents()).into(), code, u64::MAX - store.get_fuel().unwrap())
}

/* Runs a counted binary, and returns what it printed to stdout, its exit code and the total its counter writes last on stderr. */
fn native(path: &Path) -> Run {
    let out = Command::new(path).output().unwrap_or_else(|e| panic!("running {}: {e}", path.display()));
    let err = String::from_utf8_lossy(&out.stderr);
    let count = err.lines().last().and_then(|l| l.strip_prefix("instructions: ")).and_then(|n| n.parse().ok());
    (String::from_utf8_lossy(&out.stdout).into(), out.status.code().unwrap_or(-1), count.unwrap_or_else(|| panic!("{} ends without its count: {err}", path.display())))
}

/* Runs a build step, and returns what it printed, where edge build reports the types it proved. */
fn sh(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("running {cmd:?}: {e}"));
    let printed = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{cmd:?} failed: {printed}");
    printed
}
