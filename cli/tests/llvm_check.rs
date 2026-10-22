use std::path::Path;
use std::process::{Command, Stdio};
use rayon::prelude::*;

#[path = "../../bench/src/ir.rs"] mod ir;

const BIN: &str = env!("CARGO_BIN_EXE_edge");
// A native binary may hold this many times its VM's memory limit in address space, so a runaway fails alone instead of starving the machine.
const SPACE: usize = 8;
const ALARM: &str = "edge build relies on these optimizations to run at native speed, a failure here means the native output regressed";

// What a run printed, what it reported and how it ended.
type Run = (String, String, i32);
fn run(cmd: &mut Command, input: &str) -> Run {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawning a run");
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).ok();
    let out = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into(), out.status.code().unwrap_or(-1))
}

/* One program on the VM, native and native under GC torture, with the same input and events, `flags` going to the
   plain build. A build that fails gives its own run, so it shows as a difference. */
fn three(dir: &Path, src: &str, input: &str, events: &[&str], flags: &[&str]) -> [Run; 3] {
    std::fs::write(dir.join("main.py"), src).unwrap();
    std::fs::write(dir.join("events"), events.join("\n")).unwrap();
    let with_events: &[&str] = if events.is_empty() { &[] } else { &["--events", "events"] };
    let vm = run(Command::new(BIN).current_dir(dir).args(["run", "main.py"]).args(with_events), input);
    let native = |name: &str, flags: &[&str]| {
        let built = run(Command::new(BIN).current_dir(dir).args(["build", "main.py", "-o", name]).args(flags), "");
        if built.2 != 0 { return built }
        // A program's SyntaxWarnings belong to compiling it: the build prints them beside its own notes, the binary never does.
        let space = format!("ulimit -v {} && exec \"$0\" \"$@\"", compiler::embed::Limits::sandbox().memory * SPACE >> 10);
        let (out, err, code) = run(Command::new("sh").current_dir(dir).args(["-c", &space]).arg(dir.join(name)).args(with_events), input);
        (out, built.1.lines().filter(|l| !l.starts_with("edge build:")).map(|l| format!("{l}\n")).collect::<String>() + &err, code)
    };
    [vm, native("plain", flags), native("torture", &["--gc-torture"])]
}

/* The ~40-line program that uses everything at once: classes with super(), raise and finally, a closure, a generator,
   recursion, a dict, a heap-free tuple and an int loop. */
#[test]
fn flow() {
    let dir = tempfile::tempdir().unwrap();
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/llvm_check.py")).unwrap();
    // A program this size types well under a second; past two the tree grows paths it should keep as one.
    let rules = compiler::types::rules().unwrap_or_else(|e| panic!("edge.ept is missing or stale for these sources: {e}"));
    let start = std::time::Instant::now();
    compiler::types::typed(&compiler::code::compile(&src).0, &rules).unwrap_or_else(|e| panic!("{e}"));
    assert!(start.elapsed().as_secs_f64() < 2.0, "{ALARM}: its types took {:?}", start.elapsed());
    let [vm, native, torture] = three(dir.path(), &src, "", &[], &["--emit-llvm", "main.ll"]);
    assert_eq!(vm.2, 0, "the VM fails the program: {}", vm.1);
    assert_eq!(native, vm, "{ALARM}, native differs from the VM");
    assert_eq!(torture, vm, "{ALARM}, native under GC torture differs from the VM");
    assert_ne!(std::fs::read(dir.path().join("plain")).unwrap(), std::fs::read(dir.path().join("torture")).unwrap(), "--gc-torture built no torture in");
    let ir = std::fs::read_to_string(dir.path().join("main.ll")).unwrap();
    // The VM module edge linked, the host's as edge build targets it, where the tag tests are read.
    let host = if cfg!(target_os = "macos") { "apple-darwin" } else { "unknown-linux-musl" };
    let vm = std::fs::read_to_string(format!("{}/../target/bitcode/{}-{host}/vm.ll", env!("CARGO_MANIFEST_DIR"), std::env::consts::ARCH)).unwrap();
    // What the optimized module must show, where fib and mix hold only proven ints.
    let [fib, mix] = ["py_fib", "py_mix"].map(|f| *ir::bodies(&ir, f).first().unwrap_or_else(|| panic!("{ALARM}: no {f} in the module")));
    // The program's module calls into vm.o, so the interpreter is reached through the VM's own call graph too.
    let both = format!("{ir}\n{vm}");
    let may = ir::reach(&both, "edge_vm_loop");
    let checks = [
        ("fib or mix call what may reach the interpreter", ir::reaching(&may, fib, "edge_vm_loop") + ir::reaching(&may, mix, "edge_vm_loop") == 0),
        ("fib or mix still test a tag", ir::checks(&vm, fib, "is_") + ir::checks(&vm, mix, "is_") == 0),
        ("mix puts its tuple or its range on the heap", ir::calls(mix, "edge_alloc") == 0),
        ("mix stops at a safepoint, though only ints are live", ir::calls(mix, "edge_safepoint") == 0),
        ("mix still keeps a live or rest buffer in memory", !mix.contains(" = alloca ")),
        ("fib calls itself through the VM, not directly", ir::calls(fib, "py_fib") > 0 && ir::calls(fib, "op_Call") == 0),
    ];
    checks.iter().for_each(|(what, holds)| assert!(holds, "{ALARM}: {what}"));
}

fn texts<'a>(case: &'a serde_json::Value, field: &str) -> Vec<&'a str> { case[field].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect() }

/* Every program of tests/cases/vm.json and snapshot.json ends the same on the VM, native and native under torture. */
#[test]
fn corpus() {
    let read = |name: &str| serde_json::from_str::<Vec<serde_json::Value>>(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/cases").join(name)).unwrap()).unwrap();
    // Plus the programs of tests/native, shapes of the glue a fix needed, a directory a project whose main.py runs; LLVM_CHECK_CASES runs a file of cases instead of the corpus.
    let text = |f: &Path| serde_json::Value::from(std::fs::read_to_string(f).unwrap());
    let own = || std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/native")).unwrap().flatten().map(|f| match std::fs::read_dir(f.path()) {
        Ok(dir) => { let files: serde_json::Map<_, _> = dir.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), text(&e.path()))).collect(); serde_json::json!({"src": files["main.py"], "files": files}) }
        Err(_) => serde_json::json!({"src": text(&f.path())}),
    }).collect();
    let cases = std::env::var("LLVM_CHECK_CASES").map_or_else(|_| [read("vm.json"), read("snapshot.json"), own()].concat(), |f| [read(&f), own()].concat());
    // A documented v1 limit: a native program has no ops budget, so a case ending on the VM's runs forever.
    let (left, cases): (Vec<_>, Vec<_>) = cases.iter().partition(|c| c["error"] == "budget exceeded");
    eprintln!("{} cases left out, on the VM's budget: {:?}", left.len(), left.iter().map(|c| &c["src"]).collect::<Vec<_>>());
    let failures: Vec<String> = cases.par_iter().filter_map(|case| {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in case["files"].as_object().into_iter().flatten() { std::fs::write(dir.path().join(path), text.as_str().unwrap()).unwrap(); }
        let events = [texts(case, "events"), texts(case, "interactive_events")].concat();
        let [vm, native, torture] = three(dir.path(), case["src"].as_str().unwrap(), &texts(case, "input").join("\n"), &events, &[]);
        // A documented v1 limit: under `edge run` a container outgrowing linear memory ends the run with the host's MemoryError, no source line under it.
        let same = |r: &Run| r.0 == vm.0 && r.2 == vm.2 && if vm.1 == "error: MemoryError: memory limit\n" { r.1.starts_with(&vm.1) } else { r.1 == vm.1 };
        // Each one printed as found too, so a run cut short still names what it found.
        (!same(&native) || !same(&torture)).then(|| format!("{:?}\n   vm {vm:?}\n   native {native:?}\n   torture {torture:?}", case["src"])).inspect(|f| eprintln!("DIFF {f}"))
    }).collect();
    assert!(failures.is_empty(), "{ALARM}, {} of {} programs differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
