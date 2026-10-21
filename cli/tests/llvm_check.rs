use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../bench/src/ir.rs"]
mod ir;

const BIN: &str = env!("CARGO_BIN_EXE_edge");
const ALARM: &str = "edge build relies on these optimizations to run at native speed, a failure here means the native output regressed";

// What a run printed, what it reported and how it ended.
type Run = (String, String, i32);
fn run(cmd: &mut Command, input: &str) -> Run {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawning a run");
    child.stdin.take().unwrap().write_all(input.as_bytes()).ok();
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
    let ir = std::fs::read_to_string(dir.path().join("main.ll")).unwrap();
    for (what, holds) in checks(&ir) {
        assert!(holds, "{ALARM}: {what}");
    }
}

/* What the optimized module must show for the program above, where fib and mix hold only proven ints. */
fn checks(ir: &str) -> Vec<(&'static str, bool)> {
    let [fib, mix] = ["py_fib(", "py_mix("].map(|f| *ir::bodies(ir, f).first().unwrap_or_else(|| panic!("{ALARM}: no {f} in the module")));
    vec![
        ("fib or mix call what may reach the interpreter", ir::reaching(ir, fib, "edge_vm_loop") + ir::reaching(ir, mix, "edge_vm_loop") == 0),
        ("fib or mix still test a tag", ir::checks(ir, fib, "is_") + ir::checks(ir, mix, "is_") == 0),
        ("mix puts its tuple or its range on the heap", ir::calls(mix, "edge_alloc") == 0),
        ("mix stops at a safepoint, though only ints are live", ir::calls(mix, "edge_safepoint") == 0),
        ("mix still keeps a live or rest buffer in memory", !mix.contains(" = alloca ")),
        ("fib calls itself through the VM, not directly", ir::calls(fib, "py_fib") > 0 && ir::calls(fib, "op_Call") == 0),
    ]
}

fn texts<'a>(case: &'a serde_json::Value, field: &str) -> Vec<&'a str> {
    case[field].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect()
}

/* Every program of tests/cases/vm.json and snapshot.json ends the same on the VM, native and native under torture. */
#[test]
fn corpus() {
    let read = |name: &str| serde_json::from_str::<Vec<serde_json::Value>>(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/cases").join(name)).unwrap()).unwrap();
    let cases = [read("vm.json"), read("snapshot.json")].concat();
    let (next, failures) = (AtomicUsize::new(0), Mutex::new(Vec::new()));
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            s.spawn(|| while let Some(case) = cases.get(next.fetch_add(1, Ordering::Relaxed)) {
                let dir = tempfile::tempdir().unwrap();
                let events = [texts(case, "events"), texts(case, "interactive_events")].concat();
                let [vm, native, torture] = three(dir.path(), case["src"].as_str().unwrap(), &texts(case, "input").join("\n"), &events, &[]);
                if native != vm || torture != vm {
                    failures.lock().unwrap().push(format!("{:?}\n   vm {vm:?}\n   native {native:?}\n   torture {torture:?}", case["src"]));
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{ALARM}, {} of {} programs differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
