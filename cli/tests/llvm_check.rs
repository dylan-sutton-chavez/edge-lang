use std::path::Path;
use std::process::{Command, Stdio};
use rayon::prelude::*;

#[path = "../../bench/src/ir.rs"] mod ir;

const BIN: &str = env!("CARGO_BIN_EXE_edge");
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
        if built.2 != 0 { built } else { run(Command::new(dir.join(name)).current_dir(dir).args(with_events), input) }
    };
    [vm, native("plain", flags), native("torture", &["--gc-torture"])]
}

/* The ~40-line program that uses everything at once: classes with super(), raise and finally, a closure, a generator,
   recursion, a dict, a heap-free tuple and an int loop. */
#[test]
fn flow() {
    let dir = tempfile::tempdir().unwrap();
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/llvm_check.py")).unwrap();
    let [vm, native, torture] = three(dir.path(), &src, "", &[], &["--emit-llvm", "main.ll"]);
    assert_eq!(vm.2, 0, "the VM fails the program: {}", vm.1);
    assert_eq!(native, vm, "{ALARM}, native differs from the VM");
    assert_eq!(torture, vm, "{ALARM}, native under GC torture differs from the VM");
    assert_ne!(std::fs::read(dir.path().join("plain")).unwrap(), std::fs::read(dir.path().join("torture")).unwrap(), "--gc-torture built no torture in");
    if let Err(why) = compiler::types::rules() { panic!("edge.ept is missing or stale for these sources, so edge build runs untyped: {why}") }
    let ir = std::fs::read_to_string(dir.path().join("main.ll")).unwrap();
    // The VM module edge linked, the host's as edge build targets it, where the tag tests are read.
    let host = if cfg!(target_os = "macos") { "apple-darwin" } else { "unknown-linux-musl" };
    let vm = std::fs::read_to_string(format!("{}/../target/bitcode/{}-{host}/vm.ll", env!("CARGO_MANIFEST_DIR"), std::env::consts::ARCH)).unwrap();
    for (what, holds) in checks(&ir, &vm) {
        assert!(holds, "{ALARM}: {what}");
    }
}

/* What the optimized module must show for the program above, where fib and mix hold only proven ints. */
fn checks(ir: &str, vm: &str) -> Vec<(&'static str, bool)> {
    let [fib, mix] = ["py_fib", "py_mix"].map(|f| *ir::bodies(ir, f).first().unwrap_or_else(|| panic!("{ALARM}: no {f} in the module")));
    // The program's module calls into vm.o, so the interpreter is reached through the VM's own call graph too.
    let both = format!("{ir}\n{vm}");
    vec![
        ("fib or mix call what may reach the interpreter", ir::reaching(&both, fib, "edge_vm_loop") + ir::reaching(&both, mix, "edge_vm_loop") == 0),
        ("fib or mix still test a tag", ir::checks(vm, fib, "is_") + ir::checks(vm, mix, "is_") == 0),
        ("mix puts its tuple or its range on the heap", ir::calls(mix, "edge_alloc") == 0),
        ("mix stops at a safepoint, though only ints are live", ir::calls(mix, "edge_safepoint") == 0),
        ("mix still keeps a live or rest buffer in memory", !mix.contains(" = alloca ")),
        ("fib calls itself through the VM, not directly", ir::calls(fib, "py_fib") > 0 && ir::calls(fib, "op_Call") == 0),
    ]
}

fn texts<'a>(case: &'a serde_json::Value, field: &str) -> Vec<&'a str> { case[field].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect() }

/* Every program of tests/cases/vm.json and snapshot.json ends the same on the VM, native and native under torture. */
#[test]
fn corpus() {
    let read = |name: &str| serde_json::from_str::<Vec<serde_json::Value>>(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/cases").join(name)).unwrap()).unwrap();
    // LLVM_CHECK_CASES names a file of cases to run alone, the ones a fix is meant for, before the whole corpus.
    // Beside the corpus, the programs of tests/native, each a shape of the glue a fix once needed.
    let own = || std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/native")).unwrap().flatten().map(|f| serde_json::json!({"src": std::fs::read_to_string(f.path()).unwrap()})).collect();
    let cases = std::env::var("LLVM_CHECK_CASES").map_or_else(|_| [read("vm.json"), read("snapshot.json"), own()].concat(), |f| [read(&f), own()].concat());
    // A native program has no ops budget (a documented deviation), so the cases that end on the VM's budget run forever there.
    let (unbudgeted, cases): (Vec<_>, Vec<_>) = cases.iter().partition(|case| case["error"] == "budget exceeded");
    eprintln!("{} cases end on the VM's budget and are left out: {:?}", unbudgeted.len(), unbudgeted.iter().map(|c| &c["src"]).collect::<Vec<_>>());
    let failures: Vec<String> = cases.par_iter().filter_map(|case| {
        let dir = tempfile::tempdir().unwrap();
        let events = [texts(case, "events"), texts(case, "interactive_events")].concat();
        let [vm, native, torture] = three(dir.path(), case["src"].as_str().unwrap(), &texts(case, "input").join("\n"), &events, &[]);
        // Each one printed as found too, so a run cut short still names what it found.
        (native != vm || torture != vm).then(|| format!("{:?}\n   vm {vm:?}\n   native {native:?}\n   torture {torture:?}", case["src"])).inspect(|f| eprintln!("DIFF {f}"))
    }).collect();
    assert!(failures.is_empty(), "{ALARM}, {} of {} programs differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
