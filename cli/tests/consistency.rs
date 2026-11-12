use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_edge");
// Each program runs well under a second, so one still going after this has hung.
const DEADLINE: Duration = Duration::from_secs(30);
// Programs a run checks, raised through EDGE_CONSISTENCY_PROGRAMS for a longer sweep.
const PROGRAMS: u64 = 500;
// Functions in each program, and the argument pairs each one is called with.
const FUNCTIONS: u64 = 3;
const PAIRS: u64 = 2;
// Calls of each function on each pair, cold, again and warm, in main, the module and a method.
const CALLS: u64 = 8;
// locals besides the string each kept small by `clamp` after every store
const VARS: u64 = 4;

// What a function returns, at its end or from inside a loop, every local in it.
const RESULT: &str = "text(v0) + \" \" + text(v1) + \" \" + text(v2) + \" \" + text(v3) + \" \" + s";

// the helpers main and the module both define as Lox has no such builtins
const HELPERS: &str = "fun clamp(x) {\n    if (x > 10007 or x < -10007) return x / 7;\n    return x;\n}\nfun max(x, y) {\n    if (x > y) return x;\n    return y;\n}\nfun min(x, y) {\n    if (x < y) return x;\n    return y;\n}\nfun abs(x) {\n    if (x < 0) return -x;\n    return x;\n}\n";

/* Seeded so every failure replays from the number it reports. */
struct Gen {
    state: u64,
    depth: u32,
    loops: u32,
    // Loops the statement being written sits in, where an early return may leave them.
    open: u32,
}

impl Gen {
    fn new(seed: u64) -> Self {
        Gen { state: seed.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15), depth: 0, loops: 0, open: 0 }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn var(&mut self) -> String {
        format!("v{}", self.below(VARS))
    }

    /* a number expression over the parameters and locals bounded by `d` */
    fn num(&mut self, d: u32) -> String {
        if d == 0 || self.chance(30) {
            return match self.below(5) {
                0 => self.below(10).to_string(),
                1 => if self.chance(50) { "a".into() } else { "b".into() },
                2 => "len(s)".into(),
                _ => self.var(),
            };
        }
        let (x, y) = (self.num(d - 1), self.num(d - 1));
        match self.below(7) {
            0 => format!("({x} + {y})"),
            1 => format!("({x} - {y})"),
            2 => format!("({x} * {y} / 1009)"),
            3 => format!("({x} / {})", 1 + self.below(6)),
            4 => format!("max({x}, {y})"),
            5 => format!("min({x}, {y})"),
            _ => format!("abs({x})"),
        }
    }

    fn cond(&mut self) -> String {
        let (x, y) = (self.num(2), self.num(2));
        let c = match self.below(3) {
            0 => format!("{x} < {y}"),
            1 => format!("{x} == {y}"),
            _ => format!("{x} >= {y}"),
        };
        if self.chance(25) { format!("{c} and {} > {}", self.var(), self.below(500)) } else { c }
    }

    /* `n` statements at `pad`, every loop bounded so a run always ends. */
    fn block(&mut self, pad: &str, n: u64, out: &mut String) {
        for _ in 0..n {
            self.stmt(pad, out);
        }
    }

    fn stmt(&mut self, pad: &str, out: &mut String) {
        let inner = format!("{pad}    ");
        // Inside a loop, sometimes leave the function from there, the way a search returns its find.
        if self.open > 0 && self.chance(12) {
            out.push_str(&format!("{pad}if ({}) {{\n{inner}return {RESULT};\n{pad}}}\n", self.cond()));
            return;
        }
        let kinds = if self.depth < 2 { 9 } else { 5 };
        match self.below(kinds) {
            0..=2 => out.push_str(&format!("{pad}{} = clamp({});\n", self.var(), self.num(3))),
            3 => out.push_str(&format!("{pad}if (len(s) < 12) s = s + text({});\n", self.num(2))),
            4 => {
                let (x, y, z) = (self.var(), self.var(), self.var());
                out.push_str(&format!("{pad}if ({z} != 0) {x} = clamp({y} / {z}); else {x} = {};\n", self.below(10)));
            }
            5 => {
                self.depth += 1;
                out.push_str(&format!("{pad}if ({}) {{\n", self.cond()));
                let n = 1 + self.below(3);
                self.block(&inner, n, out);
                if self.chance(60) {
                    out.push_str(&format!("{pad}}} else {{\n"));
                    let n = 1 + self.below(3);
                    self.block(&inner, n, out);
                }
                out.push_str(&format!("{pad}}}\n"));
                self.depth -= 1;
            }
            6 | 7 => {
                let i = self.next_loop();
                self.depth += 1;
                self.open += 1;
                out.push_str(&format!("{pad}for (var i{i} = 0; i{i} < {}; i{i} = i{i} + 1) {{\n", self.below(7)));
                out.push_str(&format!("{inner}{v} = clamp({v} + i{i});\n", v = self.var()));
                let n = 1 + self.below(3);
                self.block(&inner, n, out);
                out.push_str(&format!("{pad}}}\n"));
                self.open -= 1;
                self.depth -= 1;
            }
            _ => {
                let i = self.next_loop();
                self.depth += 1;
                self.open += 1;
                out.push_str(&format!("{pad}var w{i} = 0;\n{pad}while (w{i} < {}) {{\n{inner}w{i} = w{i} + 1;\n", self.below(6)));
                let n = 1 + self.below(3);
                self.block(&inner, n, out);
                out.push_str(&format!("{pad}}}\n"));
                self.open -= 1;
                self.depth -= 1;
            }
        }
    }

    fn next_loop(&mut self) -> u32 {
        self.loops += 1;
        self.loops
    }

    /* a function body over two numbers returning every local so each is checked */
    fn function(&mut self, pad: &str) -> String {
        let inner = format!("{pad}    ");
        let mut body = format!("{inner}var v0 = a;\n{inner}var v1 = b;\n{inner}var v2 = 0;\n{inner}var v3 = 1;\n{inner}var s = \"\";\n");
        let n = 3 + self.below(4);
        self.block(&inner, n, &mut body);
        body.push_str(&format!("{inner}return {RESULT};\n"));
        body
    }
}

/* a program calling each function cold then warm from main a module and a field */
fn program(seed: u64) -> (String, String) {
    let mut g = Gen::new(seed);
    let (mut module, mut main) = (String::from(HELPERS), format!("var m = import(\"m\");\n{HELPERS}"));
    module.push_str("class Holder {}\nvar K = Holder();\n");
    for i in 0..FUNCTIONS {
        let body = g.function("");
        module.push_str(&format!("fun f{i}(a, b) {{\n{body}}}\n"));
        main.push_str(&format!("fun f{i}(a, b) {{\n{body}}}\n"));
        // the field holds the very same function
        module.push_str(&format!("K.f{i} = f{i};\n"));
    }
    main.push_str("fun warm(f, a, b) {\n    for (var j = 0; j < 24; j = j + 1) f(a + j, b - j);\n    return f(a, b);\n}\nvar k = m.K;\n");
    for _ in 0..PAIRS {
        let (a, b) = (g.below(200) as i64 - 100, g.below(200) as i64 - 100);
        for i in 0..FUNCTIONS {
            for call in [format!("f{i}({a}, {b})"), format!("f{i}({a}, {b})"), format!("warm(f{i}, {a}, {b})"), format!("m.f{i}({a}, {b})"), format!("m.f{i}({a}, {b})"), format!("warm(m.f{i}, {a}, {b})"), format!("k.f{i}({a}, {b})"), format!("warm(k.f{i}, {a}, {b})")] {
                main.push_str(&format!("print \"f{i} {a} {b} \" + {call};\n"));
            }
        }
    }
    (main, module)
}

/* Runs one program through the CLI, returning its stdout or why it failed. */
fn run(dir: &PathBuf) -> Result<String, String> {
    // Output goes to files, so a full pipe never stalls a program the deadline then blames.
    let (out, err) = (dir.join("stdout.txt"), dir.join("stderr.txt"));
    let mut child = Command::new(BIN).current_dir(dir).args(["run", "main.lox"]).env("XDG_CACHE_HOME", dir)
        .stdin(Stdio::null()).stdout(File::create(&out).unwrap()).stderr(File::create(&err).unwrap()).spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("still running after {} seconds", DEADLINE.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if !status.success() {
        return Err(std::fs::read_to_string(&err).unwrap_or_default());
    }
    Ok(std::fs::read_to_string(&out).unwrap_or_default())
}

/* The same function with the same arguments answers the same, however often it ran and wherever it was defined. */
#[test]
fn a_function_answers_the_same_cold_warm_and_from_any_module() {
    let programs = std::env::var("EDGE_CONSISTENCY_PROGRAMS").ok().and_then(|n| n.parse().ok()).unwrap_or(PROGRAMS);
    let root = std::env::temp_dir().join(format!("edge-consistency-{}", std::process::id()));
    let mut failures = Vec::new();
    for seed in 0..programs {
        let dir = root.join(seed.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let (main, module) = program(seed);
        std::fs::write(dir.join("main.lox"), main).unwrap();
        std::fs::write(dir.join("m.lox"), module).unwrap();
        std::fs::write(dir.join("edge.json"), "{ \"imports\": { \"m\": \"./m.lox\" } }\n").unwrap();
        let printed = match run(&dir) {
            Ok(printed) => printed,
            Err(e) => {
                failures.push(format!("seed {seed} failed to run, kept at {}\n{e}", dir.display()));
                continue;
            }
        };
        // A program that printed less than it calls proves nothing, so it fails too.
        if printed.lines().count() as u64 != FUNCTIONS * PAIRS * CALLS {
            failures.push(format!("seed {seed} printed {} lines, kept at {}\n{printed}", printed.lines().count(), dir.display()));
            continue;
        }
        // Every call of one function on one pair must print the same answer.
        let mut answers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in printed.lines() {
            let mut parts = line.splitn(4, ' ');
            let key = parts.by_ref().take(3).collect::<Vec<_>>().join(" ");
            answers.entry(key).or_default().push(parts.next().unwrap_or_default().to_string());
        }
        let split: Vec<String> = answers.iter().filter(|(_, got)| got.iter().any(|g| g != &got[0])).map(|(key, got)| format!("  {key}: {got:?}")).collect();
        if split.is_empty() {
            let _ = std::fs::remove_dir_all(&dir);
        } else {
            failures.push(format!("seed {seed} answered differently, kept at {}\n{}", dir.display(), split.join("\n")));
        }
    }
    assert!(failures.is_empty(), "{} of {programs} program(s) failed:\n{}", failures.len(), failures.join("\n"));
    let _ = std::fs::remove_dir_all(&root);
}
