use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_interpreter");
// the lox runtime as a .wasm and as the native library make native builds beside a crash build of it
const WASM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/wasm32-unknown-unknown/cli/interpreter.wasm");
// the compiled code every case shares since it is keyed by the bytes it came from
const CODE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/cli-code-cache");
const WARNING: &str = "warning: native runtime, it runs outside the sandbox and the grants of interpreter.json do not hold it";

static SEQ: AtomicU32 = AtomicU32::new(0);

fn native(name: &str) -> String {
    format!("{}/../target/native/{name}.{}", env!("CARGO_MANIFEST_DIR"), std::env::consts::DLL_EXTENSION)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("interpreter-library-{}-{name}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN).current_dir(dir).args(args).env("XDG_CACHE_HOME", dir).env("INTERPRETER_CODE_CACHE", CODE).env_remove("INTERPRETER_RUNTIME").output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// a run on a native runtime says once and first that it runs outside the sandbox
#[test]
fn a_native_run_warns_once_that_it_runs_outside_the_sandbox() {
    let dir = scratch("warn");
    std::fs::write(dir.join("main.lox"), "print \"hi\";\n").unwrap();
    let out = run(&dir, &["run", "main.lox", "--runtime-native", &native("lox")]);
    let stderr = text(&out.stderr);
    assert_eq!(stderr.lines().collect::<Vec<_>>(), [WARNING], "{stderr}");
    assert_eq!(text(&out.stdout), "hi\n");
    assert_eq!(out.status.code(), Some(0));
    // the same run on the wasm runtime runs in the sandbox and warns of nothing
    let out = run(&dir, &["run", "main.lox", "--runtime", WASM]);
    assert_eq!(text(&out.stderr), "");
}

// a bundle built on a native runtime carries it and warns wherever it runs
#[test]
fn a_bundle_that_carries_a_native_runtime_warns_where_it_runs() {
    let dir = scratch("bundle");
    std::fs::write(dir.join("main.lox"), "print \"packed\";\n").unwrap();
    std::fs::write(dir.join("interpreter.json"), format!("{{ \"runtime\": {{ \"native\": {:?} }} }}\n", native("lox"))).unwrap();
    let built = run(&dir, &["build"]);
    assert_eq!(built.status.code(), Some(0), "{}", text(&built.stderr));
    let away = scratch("away");
    std::fs::copy(dir.join("app.interpreter"), away.join("app.interpreter")).unwrap();
    let out = run(&away, &["run", "app.interpreter"]);
    assert_eq!(text(&out.stdout), "packed\n");
    assert!(text(&out.stderr).lines().next() == Some(WARNING), "{}", text(&out.stderr));
}

// a native runtime that panics ends the run with the panic as its error and the CLI stays alive
#[test]
fn a_native_runtime_that_panics_fails_the_run_with_its_message() {
    let dir = scratch("panic");
    std::fs::write(dir.join("main.lox"), "print \"before\";\ncrash_panic();\nprint \"after\";\n").unwrap();
    let out = run(&dir, &["run", "main.lox", "--runtime-native", &native("lox-crash")]);
    assert_eq!(text(&out.stdout), "before\n");
    assert!(text(&out.stderr).contains("error: internal panic: a crash test runtime panics on purpose"), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));
}

// a native runtime that aborts its process reads as an error of the run and never of the CLI
#[test]
fn a_native_runtime_that_aborts_fails_the_run_and_the_cli_reports_it() {
    let dir = scratch("abort");
    std::fs::write(dir.join("main.lox"), "print \"before\";\ncrash_abort();\n").unwrap();
    let out = run(&dir, &["run", "main.lox", "--runtime-native", &native("lox-crash")]);
    assert_eq!(text(&out.stdout), "before\n");
    assert!(text(&out.stderr).contains("error: the native runtime crashed, it aborted"), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));
}

// a native runtime loads only from a local path
#[test]
fn a_native_runtime_named_by_url_is_refused() {
    let dir = scratch("url");
    std::fs::write(dir.join("main.lox"), "print 1;\n").unwrap();
    std::fs::write(dir.join("interpreter.json"), "{ \"runtime\": { \"native\": \"https://example.com/lox.so\" } }\n").unwrap();
    let out = run(&dir, &["run", "main.lox"]);
    assert!(text(&out.stderr).contains("a native runtime loads only from a local path"), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));
}

/* every `.lox` file below `dir` */
fn programs(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            programs(&path, found);
        } else if path.extension().is_some_and(|e| e == "lox") {
            found.push(path);
        }
    }
}

// the book's suite gives the same output error and status on the wasm runtime and the native one
#[test]
fn the_book_suite_runs_alike_on_wasm_and_native() {
    let mut found = Vec::new();
    programs(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/lox/tests/suite"), &mut found);
    found.sort();
    let dir = scratch("suite");
    let mut differ = Vec::new();
    for program in &found {
        let file = program.to_str().unwrap();
        let wasm = run(&dir, &["run", file, "--runtime", WASM]);
        let lib = run(&dir, &["run", file, "--runtime-native", &native("lox")]);
        let lib_stderr = text(&lib.stderr).strip_prefix(&format!("{WARNING}\n")).map(String::from).unwrap_or_else(|| text(&lib.stderr));
        if wasm.stdout != lib.stdout || text(&wasm.stderr) != lib_stderr || wasm.status.code() != lib.status.code() {
            differ.push(program.display().to_string());
        }
    }
    assert!(found.len() > 200, "the suite holds {} programs", found.len());
    assert!(differ.is_empty(), "{} of {} programs differ\n{}", differ.len(), found.len(), differ.join("\n"));
}

// actors spread over schedulers in parallel each in a context of their own answer as on wasm
#[test]
fn parallel_native_actors_answer_as_wasm_actors_do() {
    let dir = scratch("actors");
    let mut groups = String::new();
    for g in 0..8 {
        groups.push_str(&format!("  work{g}:\n    replicas: 2\n    code: |\n      fun fib(n) {{ if (n < 2) return n; return fib(n - 1) + fib(n - 2); }}\n      var n = receive();\n      send(\"sink\", \"{g}:\" + text(fib(18)) + \":\" + n);\n    seed:\n      - a{g}\n      - b{g}\n"));
    }
    let pool = format!("runtime:\n  schedulers: 4\n\ngroups:\n{groups}  sink:\n    replicas: 1\n    code: |\n      while (true) {{\n        print receive();\n      }}\n");
    std::fs::write(dir.join("pool.yml"), pool).unwrap();
    let sorted = |out: &Output| {
        let mut lines: Vec<String> = text(&out.stdout).lines().map(String::from).collect();
        lines.sort();
        lines
    };
    let wasm = run(&dir, &["actor", "pool.yml", "--runtime", WASM]);
    let lib = run(&dir, &["actor", "pool.yml", "--runtime-native", &native("lox")]);
    assert_eq!(sorted(&wasm).len(), 16, "{}", text(&wasm.stderr));
    assert_eq!(sorted(&wasm), sorted(&lib), "{}", text(&lib.stderr));
    assert_eq!(wasm.status.code(), lib.status.code());
}

// every command on a native runtime warns once and what needs the sandbox is refused by name
#[test]
fn a_native_runtime_warns_on_every_command_and_refuses_what_needs_the_sandbox() {
    let dir = scratch("commands");
    std::fs::write(dir.join("main.lox"), "print \"app\";\n").unwrap();
    std::fs::write(dir.join("ok_test.lox"), "print 1;\n").unwrap();
    std::fs::write(dir.join("interpreter.json"), format!("{{ \"runtime\": {{ \"native\": {:?} }} }}\n", native("lox"))).unwrap();
    let warned = |out: &Output| text(&out.stderr).lines().filter(|l| *l == WARNING).count() == 1;
    for args in [&["test"][..], &["build"], &["build", "--app", "--out", "app"]] {
        let out = run(&dir, args);
        assert!(out.status.success() && warned(&out), "{args:?}\n{}", text(&out.stderr));
    }
    // the binary build --app wrote carries the library and warns wherever it runs
    let app = Command::new(dir.join("app")).current_dir(&dir).env("XDG_CACHE_HOME", &dir).output().unwrap();
    assert!(text(&app.stdout) == "app\n" && warned(&app), "{}", text(&app.stderr));

    // a plugin is a .wasm the sandbox runs so a native runtime names it and loads nothing
    let plugin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/wasm32-unknown-unknown/release/slugify_mod.wasm");
    std::fs::copy(plugin, dir.join("slugify_mod.wasm")).unwrap();
    std::fs::write(dir.join("plugged.lox"), "var slugify = import(\"slugify_mod\");\nprint slugify.version;\n").unwrap();
    std::fs::write(dir.join("interpreter.json"), format!("{{ \"runtime\": {{ \"native\": {:?} }}, \"imports\": {{ \"slugify_mod\": \"./slugify_mod.wasm\" }} }}\n", native("lox"))).unwrap();
    let out = run(&dir, &["run", "plugged.lox"]);
    assert!(text(&out.stderr).contains("is a .wasm plugin, which a native runtime does not load, run the project on a wasm runtime"), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));

    // an eval group runs code nobody vetted so it runs only where the sandbox holds it
    std::fs::write(dir.join("pool.yml"), "groups:\n  runners:\n    replicas: 1\n    eval: true\n").unwrap();
    let out = run(&dir, &["actor", "pool.yml"]);
    assert!(text(&out.stderr).contains("an eval group runs untrusted code so it runs only on a wasm runtime"), "{}", text(&out.stderr));
    assert_ne!(out.status.code(), Some(0));
}
