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
// Locals besides the list and the string, all ints kept under 10007.
const VARS: u64 = 4;
// What a function returns, at its end or from inside a loop, every local in it.
const RESULT: &str = "(v0, v1, v2, v3, len(xs), xs[-1], s)";

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

    /* An int expression over the parameters and locals, bounded by `d`. */
    fn int(&mut self, d: u32) -> String {
        if d == 0 || self.chance(30) {
            return match self.below(5) {
                0 => self.below(10).to_string(),
                1 => if self.chance(50) { "a".into() } else { "b".into() },
                2 => "len(xs)".into(),
                _ => self.var(),
            };
        }
        let (x, y) = (self.int(d - 1), self.int(d - 1));
        match self.below(8) {
            0 => format!("({x} + {y})"),
            1 => format!("({x} - {y})"),
            2 => format!("({x} * {y} % 1009)"),
            3 => format!("({x} ~/ {})", 1 + self.below(6)),
            4 => format!("({x} % {})", 2 + self.below(9)),
            5 => format!("max({x}, {y})"),
            6 => format!("min({x}, {y})"),
            _ => format!("abs({x})"),
        }
    }

    fn cond(&mut self) -> String {
        let (x, y) = (self.int(2), self.int(2));
        let c = match self.below(4) {
            0 => format!("{x} < {y}"),
            1 => format!("{x} == {y}"),
            2 => format!("{x} % 2 == 0"),
            _ => format!("{x} >= {y}"),
        };
        if self.chance(25) { format!("{c} && {} > {}", self.var(), self.below(500)) } else { c }
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
            out.push_str(&format!("{pad}if {}:\n{inner}return {RESULT}\n", self.cond()));
            return;
        }
        let kinds = if self.depth < 2 { 11 } else { 6 };
        match self.below(kinds) {
            0..=2 => out.push_str(&format!("{pad}{} = {} % 10007\n", self.var(), self.int(3))),
            3 => out.push_str(&format!("{pad}if len(xs) < 40:\n{inner}xs.append({})\n", self.int(2))),
            4 => out.push_str(&format!("{pad}s = (s + str({} % 10))[-12:]\n", self.int(2))),
            5 => {
                let v = self.var();
                out.push_str(&format!("{pad}{v} = ({v} + xs[-1] + len(s)) % 10007\n"));
            }
            6 => {
                self.depth += 1;
                out.push_str(&format!("{pad}if {}:\n", self.cond()));
                let n = 1 + self.below(3);
                self.block(&inner, n, out);
                if self.chance(60) {
                    out.push_str(&format!("{pad}else:\n"));
                    let n = 1 + self.below(3);
                    self.block(&inner, n, out);
                }
                self.depth -= 1;
            }
            7 => {
                let i = self.next_loop();
                self.depth += 1;
                self.open += 1;
                out.push_str(&format!("{pad}for i{i} in range({}):\n", self.below(7)));
                out.push_str(&format!("{inner}{v} = ({v} + i{i}) % 10007\n", v = self.var()));
                if self.chance(20) {
                    out.push_str(&format!("{inner}if {}:\n{inner}    break\n", self.cond()));
                }
                let n = 1 + self.below(3);
                self.block(&inner, n, out);
                self.open -= 1;
                self.depth -= 1;
            }
            8 => {
                let i = self.next_loop();
                self.depth += 1;
                self.open += 1;
                out.push_str(&format!("{pad}for y{i} in list(xs):\n"));
                out.push_str(&format!("{inner}{v} = ({v} + y{i}) % 10007\n", v = self.var()));
                if self.chance(20) {
                    out.push_str(&format!("{inner}if {}:\n{inner}    continue\n", self.cond()));
                }
                let n = 1 + self.below(2);
                self.block(&inner, n, out);
                self.open -= 1;
                self.depth -= 1;
            }
            9 => {
                let i = self.next_loop();
                self.depth += 1;
                self.open += 1;
                out.push_str(&format!("{pad}w{i} = 0\n{pad}while w{i} < {}:\n{inner}w{i} += 1\n", self.below(6)));
                let n = 1 + self.below(3);
                self.block(&inner, n, out);
                self.open -= 1;
                self.depth -= 1;
            }
            _ => {
                let (x, y, z) = (self.var(), self.var(), self.var());
                out.push_str(&format!("{pad}try:\n{inner}{x} = {y} ~/ {z}\n{pad}except ZeroDivisionError:\n{inner}{x} = {}\n", self.below(10)));
            }
        }
    }

    fn next_loop(&mut self) -> u32 {
        self.loops += 1;
        self.loops
    }

    /* A function body over two ints, every local returned so each one is checked. */
    fn function(&mut self, pad: &str) -> String {
        let inner = format!("{pad}    ");
        let mut body = format!("{inner}v0 = a % 10007\n{inner}v1 = b % 10007\n{inner}v2 = 0\n{inner}v3 = 1\n{inner}xs = [a, b]\n{inner}s = ''\n");
        let n = 3 + self.below(4);
        self.block(&inner, n, &mut body);
        body.push_str(&format!("{inner}return {RESULT}\n"));
        body
    }
}

/* A program that calls each function cold, again and warm, defined in main, in a module and as a method. */
fn program(seed: u64) -> (String, String) {
    let mut g = Gen::new(seed);
    let mut module = String::new();
    let mut main = String::from("import m\n");
    let mut methods = String::from("class K:\n");
    for i in 0..FUNCTIONS {
        let body = g.function("");
        module.push_str(&format!("fn f{i}(a, b):\n{body}\n"));
        main.push_str(&format!("fn f{i}(a, b):\n{body}\n"));
        // The method runs the very same body one level deeper.
        let method: String = body.lines().map(|line| format!("    {line}\n")).collect();
        methods.push_str(&format!("    fn f{i}(self, a, b):\n{method}\n"));
    }
    module.push_str(&methods);
    main.push_str("fn warm(fn, a, b):\n    for j in range(24):\n        fn(a + j, b - j)\n    return fn(a, b)\n");
    let pairs: Vec<String> = (0..PAIRS).map(|_| format!("({}, {})", g.below(200) as i64 - 100, g.below(200) as i64 - 100)).collect();
    main.push_str(&format!("k = m.K()\nfor a, b in [{}]:\n", pairs.join(", ")));
    for i in 0..FUNCTIONS {
        for call in [format!("f{i}(a, b)"), format!("f{i}(a, b)"), format!("warm(f{i}, a, b)"), format!("m.f{i}(a, b)"), format!("m.f{i}(a, b)"), format!("warm(m.f{i}, a, b)"), format!("k.f{i}(a, b)"), format!("warm(k.f{i}, a, b)")] {
            main.push_str(&format!("    print('f{i}', a, b, {call})\n"));
        }
    }
    (main, module)
}

/* Runs one program through the CLI, returning its stdout or why it failed. */
fn run(dir: &PathBuf) -> Result<String, String> {
    // Output goes to files, so a full pipe never stalls a program the deadline then blames.
    let (out, err) = (dir.join("stdout.txt"), dir.join("stderr.txt"));
    let mut child = Command::new(BIN).current_dir(dir).args(["run", "main.mini"]).env("XDG_CACHE_HOME", dir)
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
        std::fs::write(dir.join("main.mini"), main).unwrap();
        std::fs::write(dir.join("m.mini"), module).unwrap();
        std::fs::write(dir.join("edge.json"), "{ \"imports\": { \"m\": \"./m.mini\" } }\n").unwrap();
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
