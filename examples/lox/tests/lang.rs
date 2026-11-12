use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use edge::deps::ImportSpec;
use edge::embed::{Limits, VM};

// every run meters what it allocates so its memory limit holds
edge::allocator!();

/* what `src` prints or the first line of the error it ends in */
fn run(src: &str) -> Result<String, String> {
    let (chunk, errors) = edge::code::compile(&lox::FRONT, src);
    if let Some(e) = errors.first() { return Err(e.msg.clone()); }
    let mut vm = VM::with_runtime(&lox::RUNTIME, &chunk, Limits::sandbox());
    match vm.run() {
        Ok(_) => Ok(vm.output_text()),
        Err(e) => Err(vm.report_error(&e, src, None).lines().next().unwrap_or("").to_string()),
    }
}

/* a fresh directory under the target holding `files` */
fn project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the target takes a project");
    for (path, text) in files { std::fs::write(dir.join(path), text).expect("a file writes"); }
    dir
}

/* the lox binary run on `file` in `dir` with `input` on its stdin */
fn lox(dir: &Path, file: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lox")).arg(file).current_dir(dir)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("lox starts");
    child.stdin.take().expect("lox reads stdin").write_all(input.as_bytes()).expect("the input writes");
    child.wait_with_output().expect("lox ends")
}

fn text(b: &[u8]) -> String { String::from_utf8_lossy(b).into_owned() }

// a closure made on each pass keeps its own variable also in laid hot code
#[test]
fn each_pass_of_a_hot_loop_keeps_its_own_variable() {
    let src = "fun passes() {
  var first; var second; var third;
  for (var i = 0; i < 3; i = i + 1) {
    var k = i;
    fun get() { return k; }
    if (k == 0) first = get; else if (k == 1) second = get; else third = get;
  }
  return first() * 100 + second() * 10 + third();
}
var t = 0;
for (var n = 0; n < 3000; n = n + 1) t = t + passes();
print t;
";
    assert_eq!(run(src).as_deref(), Ok("36000\n"));
}

// a local function reads its own name through the cell it was declared in
#[test]
fn a_local_function_recurses_through_its_own_cell() {
    let src = "fun outer(n) {
  fun fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); }
  return fib(n);
}
var t = 0;
for (var i = 0; i < 300; i = i + 1) t = t + outer(10);
{ fun even(n) { if (n == 0) return true; return !even(n - 1); } print even(10); }
print t;
";
    assert_eq!(run(src).as_deref(), Ok("true\n16500\n"));
}

// only numbers order and every other pair refuses as the book's comparisons do
#[test]
fn only_numbers_order() {
    for src in ["print \"a\" < \"b\";", "print \"a\" <= \"a\";", "print true > false;", "print nil >= nil;"] {
        assert_eq!(run(src), Err("Operands must be numbers.".into()), "{src}");
    }
    assert_eq!(run("print 1 <= 1; print 2 >= 3;").as_deref(), Ok("true\nfalse\n"));
}

// a class with no init takes no arguments and says how many came
#[test]
fn a_class_with_no_init_counts_what_it_was_given() {
    assert_eq!(run("class A {} A(1);"), Err("Expected 0 arguments but got 1.".into()));
    assert_eq!(run("class A {} print A();").as_deref(), Ok("A instance\n"));
}

// the natives past the book reach the host and show values as print does
#[test]
fn the_natives_past_the_book_show_and_count() {
    assert_eq!(run("print text(1.5) + \"!\"; print len(\"abc\"); print text(nil) + text(true);").as_deref(), Ok("1.5!\n3\nniltrue\n"));
    let out = lox(&project("natives", &[("main.lox", "var a = input();\nprint a + \"/\" + input();\nexit(3);\nprint \"never\";\n")]), "main.lox", "one\ntwo\n");
    assert_eq!((text(&out.stdout), out.status.code()), ("one/two\n".into(), Some(3)));
}

// a module shows its globals and an import inside a function binds at start
#[test]
fn imports_bind_modules_and_their_globals() {
    let dir = project("imports", &[
        ("util.lox", "var n = 3;\nfun show() { return n; }\n"),
        ("main.lox", "var u = import(\"./util.lox\");\nprint u.n;\nprint u.show();\nfun later() { var v = import(\"./util.lox\"); return v.n + 1; }\nprint later();\n"),
    ]);
    let out = lox(&dir, "main.lox", "");
    assert_eq!((text(&out.stdout), text(&out.stderr), out.status.code()), ("3\n3\n4\n".into(), String::new(), Some(0)));
}

// the native target exits with the book's statuses and words its compile errors
#[test]
fn the_native_target_keeps_the_books_statuses() {
    assert_eq!(lox::DESCRIPTOR.statuses, (65, 70));
    let dir = project("statuses", &[("bad.lox", "print 1\nvar = 2;\n"), ("raise.lox", "print \"before\";\nprint -\"x\";\n")]);
    let bad = lox(&dir, "bad.lox", "");
    assert_eq!((text(&bad.stderr), bad.status.code()), ("[line 2] Error at 'var': Expect ';' after value.\n[line 2] Error at '=': Expect variable name.\n".into(), Some(65)));
    let raised = lox(&dir, "raise.lox", "");
    assert_eq!((text(&raised.stdout), text(&raised.stderr), raised.status.code()), ("before\n".into(), "Operand must be a number.\n[line 2] in script\n".into(), Some(70)));
}

// the front lists only the literal imports a call to import names
#[test]
fn the_front_scans_literal_imports() {
    let specs = lox::parse::imports("var a = import(\"net\");\nvar b = import(\"./util.lox\");\nvar c = import(\"lib/x.lox\");\n// import(\"skipped\")\nvar d = import(spec);\nprint \"import(x)\";\n");
    assert_eq!(specs, vec![ImportSpec::Bare("net".into()), ImportSpec::Relative("./util.lox".into()), ImportSpec::Root("lib/x.lox".into())]);
}
