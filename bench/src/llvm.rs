use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Store};

use super::{amount, annotate, ir, list, ROOT};

const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/llvm/cases");
const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/llvm/.snapshot");
// Edge may run this many times the instructions of the same program in Rust, where its types are proven.
const GATE: f64 = 1.3;
// The four counts of a case, in the order the snapshot keeps them.
const MODES: [&str; 4] = ["edge wasm", "rust wasm", "edge native", "rust native"];

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
    let mut names: Vec<String> = std::fs::read_dir(CASES).expect("reading bench/llvm/cases").flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".py").map(String::from)).collect();
    names.sort();

    let (mut failures, mut cases) = (Vec::new(), BTreeMap::new());
    for name in &names {
        let (py, rs) = (Path::new(CASES).join(format!("{name}.py")), Path::new(CASES).join(format!("{name}.rs")));
        let at = |ext: &str| out.join(format!("{name}{ext}"));
        let edge = |args: &[&Path]| sh(Command::new(&edge).arg("build").args(args));
        let rustc = |args: &[&Path]| sh(Command::new("rustc").args(["-Copt-level=3", "-Cpanic=abort"]).args(args));
        edge(&[Path::new("--wasm"), &py, Path::new("-o"), &at(".edge.wasm")]);
        rustc(&[Path::new("-Clto=fat"), Path::new("--target=wasm32-wasip1"), &rs, Path::new("-o"), &at(".rust.wasm")]);
        let built = edge(&[Path::new("--emit-llvm"), &at(".edge.ll"), &py, Path::new("-o"), &at(".edge.plain")]);
        edge(&[Path::new("--count"), &py, Path::new("-o"), &at(".edge")]);
        rustc(&[Path::new("-Clto=fat"), Path::new("--emit=llvm-bc"), &rs, Path::new("-o"), &at(".rust.bc")]);
        edge(&[Path::new("--count"), &at(".rust.bc"), Path::new("-o"), &at(".rust.o")]);
        // Linked apart and without LTO, so std comes from its rlibs around the counted object.
        let link = format!("-Clink-arg={}", at(".rust.o").display());
        rustc(&[Path::new(&link), &out.join("empty.rs"), Path::new("-o"), &at(".rust")]);
        let runs = [wasm(&engine, &at(".edge.wasm")), wasm(&engine, &at(".rust.wasm")), native(&at(".edge")), native(&at(".rust"))];

        if let Some(k) = (1..4).find(|&k| runs[k].0 != runs[0].0) {
            failures.push(format!("{name} prints {:?} in {} and {:?} in {}.", runs[0].0, MODES[0], runs[k].0, MODES[k]));
        }
        let counts = runs.map(|r| r.1 as f64);
        let (wasm_ratio, native_ratio) = (counts[0] / counts[1], counts[2] / counts[3]);
        let types = built.lines().find(|l| l.starts_with("types:")).unwrap_or("types: not reported");
        annotate("notice", &format!("{name}: wasm {:.2}x of Rust ({} against {}), native {:.2}x ({} against {}), {types}, left in the optimized IR {}.",
            wasm_ratio, amount(counts[0], 0), amount(counts[1], 0), native_ratio, amount(counts[2], 0), amount(counts[3], 0), left(&std::fs::read_to_string(at(".edge.ll")).unwrap())));
        if wasm_ratio > GATE || native_ratio > GATE {
            failures.push(format!("{name} runs {wasm_ratio:.2}x of Rust on wasm and {native_ratio:.2}x native, past the {GATE}x gate."));
        }
        cases.insert(name.clone(), counts);
    }

    let rustc = sh(Command::new("rustc").arg("--version")).trim().to_string();
    let last: Option<Snapshot> = std::fs::read_to_string(SNAPSHOT).ok().map(|t| serde_json::from_str(&t).expect("bench/llvm/.snapshot does not parse"));
    if let Some(last) = &last {
        failures.extend(check(last, &rustc, &cases));
    }
    for failure in &failures {
        annotate("error", failure);
    }
    if update || last.is_none() {
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
    let moved = |grew: bool| cases.iter().flat_map(|(k, now)| (0..4).filter_map(move |m| {
        let (l, n) = (last.cases[k][m], now[m]);
        (if grew { n > l * (1.0 + last.case_threshold) } else { n < l * (1.0 - last.case_threshold) })
            .then(|| format!("{k} {} {} to {} instructions ({:+.1}%)", MODES[m], amount(l, 0), amount(n, 0), (n / l - 1.0) * 100.0))
    })).collect::<Vec<_>>();
    let (grew, shrank) = (moved(true), moved(false));
    let mut failures = Vec::new();
    if !grew.is_empty() {
        failures.push(format!("{} counts grew past +{:.0}%. Fix them, they are slower than the snapshot.{}", grew.len(), last.case_threshold * 100.0, list(&grew)));
    }
    if !shrank.is_empty() {
        failures.push(format!("{} counts shrank past -{:.0}%. Accept them with --update.{}", shrank.len(), last.case_threshold * 100.0, list(&shrank)));
    }
    failures
}

/* What the native functions of the program still hold that a proven type takes out. */
fn left(ir: &str) -> String {
    let functions = ir::bodies(ir, "py_");
    let sum = |f: &dyn Fn(&str) -> usize| functions.iter().map(|b| f(b)).sum::<usize>();
    let (tags, overflow, allocations) = (sum(&|b| ir::checks(ir, b, "is_")), sum(&|b| ir::checks(ir, b, "fits_int")), sum(&|b| ir::calls(b, "edge_alloc")));
    format!("{tags} tag checks, {overflow} overflow checks, {allocations} allocations")
}

/* Runs one .wasm under WASI's fd_write and proc_exit, and returns what it printed and the instructions it ran. */
fn wasm(engine: &Engine, path: &Path) -> (String, u64) {
    let module = Module::from_file(engine, path).unwrap_or_else(|e| panic!("compiling {}: {e}", path.display()));
    let mut store = Store::new(engine, Vec::<u8>::new());
    store.set_fuel(u64::MAX).expect("fuel is enabled");
    let mut linker = Linker::new(engine);
    linker.func_wrap("wasi_snapshot_preview1", "fd_write", |mut c: Caller<'_, Vec<u8>>, _fd: i32, iovs: i32, n: i32, written: i32| {
        let Some(Extern::Memory(memory)) = c.get_export("memory") else { return 8 };
        let word = |c: &Caller<'_, Vec<u8>>, at: i32| u32::from_le_bytes(memory.data(c)[at as usize..at as usize + 4].try_into().unwrap()) as usize;
        let mut total = 0;
        for k in 0..n {
            let (ptr, len) = (word(&c, iovs + 8 * k), word(&c, iovs + 8 * k + 4));
            let bytes = memory.data(&c)[ptr..ptr + len].to_vec();
            c.data_mut().extend(bytes);
            total += len;
        }
        memory.data_mut(&mut c)[written as usize..written as usize + 4].copy_from_slice(&(total as u32).to_le_bytes());
        0
    }).unwrap();
    linker.func_wrap("wasi_snapshot_preview1", "proc_exit", |code: i32| -> wasmtime::Result<()> { Err(wasmtime::Error::msg(format!("exit {code}"))) }).unwrap();
    // Rust's std asks for its environment and arguments, which these programs never read; Edge's .wasm asks for nothing more.
    if path.to_string_lossy().ends_with(".rust.wasm") {
        linker.define_unknown_imports_as_default_values(&mut store, &module).unwrap();
    }
    let inst = linker.instantiate(&mut store, &module).unwrap_or_else(|e| panic!("instantiating {}: {e}", path.display()));
    let ended = inst.get_typed_func::<(), ()>(&mut store, "_start").unwrap().call(&mut store, ());
    if let Err(e) = ended.as_ref().map_err(|e| e.to_string()) && e != "exit 0" {
        panic!("{} ends with {e}", path.display());
    }
    let ran = u64::MAX - store.get_fuel().unwrap();
    (String::from_utf8_lossy(store.data()).into(), ran)
}

/* Runs a counted binary, and returns what it printed and the total its counter writes last on stderr. */
fn native(path: &Path) -> (String, u64) {
    let out = Command::new(path).output().unwrap_or_else(|e| panic!("running {}: {e}", path.display()));
    let err = String::from_utf8_lossy(&out.stderr);
    let count = err.lines().last().and_then(|l| l.strip_prefix("instructions: ")).and_then(|n| n.parse().ok());
    (String::from_utf8_lossy(&out.stdout).into(), count.unwrap_or_else(|| panic!("{} ends without its count: {err}", path.display())))
}

/* Runs a build step, and returns what it printed, where edge build reports the types it proved. */
fn sh(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("running {cmd:?}: {e}"));
    let printed = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{cmd:?} failed: {printed}");
    printed
}
