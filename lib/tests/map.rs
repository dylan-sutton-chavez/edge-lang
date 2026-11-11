use edge::embed::{Limits, VM};
use mini::RUNTIME;

const GEN: &str = "fn gen() {\n    print(\"gen\")\n    yield (\"k\", 1)\n    yield (\"j\", 2)\n}\nfn p(k) {\n    yield k\n    yield len(k)\n}\n";

/* what `src` printed and then the words of the error it ended in */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits { ops: 10_000_000, memory: usize::MAX });
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

fn with_gen(src: &str) -> String { run(&(GEN.to_string() + src)) }

#[test]
fn keywords_merge_after_the_source() {
    assert_eq!(run("d = {a = 0}\nd.update(x=1)\nd.update([(\"b\", 2)], c=3)\nd.update({c = 4}, a=5)\nd.update()\nprint(d)"), "{a = 5, x = 1, b = 2, c = 4}\n");
    assert_eq!(run("d = record([(1, 2)])\nd.update(d, **{s = 3})\nprint(d)"), "{1 = 2, s = 3}\n");
}

#[test]
fn a_source_or_a_pair_that_steps_runs_as_frames() {
    assert_eq!(with_gen("d = {}\nd.update(gen(), z=9)\nprint(d)"), "gen\n{k = 1, j = 2, z = 9}\n");
    assert_eq!(with_gen("d = {}\nd.update([p(\"a\"), (\"b\", 2)])\nd.update((p(k) for k in [\"xy\", \"z\"]))\nprint(d)"), "{a = 1, b = 2, xy = 2, z = 1}\n");
    assert_eq!(with_gen("print(record.fromkeys((k for (k, _) in gen())), record.fromkeys(\"ab\", 0))"), "gen\n{k = nil, j = nil} {a = 0, b = 0}\n");
    assert_eq!(with_gen("t = 0\nfor i in range(2000) {\n    d = {}\n    d.update(((text(i), i) for i in range(3)))\n    t += len(d)\n}\nprint(t)"), "6000\n");
}

#[test]
fn the_methods_read_and_change_the_dict() {
    assert_eq!(run("print(record([(1, 2)]).get(1), {}.get(5, \"d\"), {}.pop(1, \"p\"), {}.setdefault(3))"), "2 d p nil\n");
    assert_eq!(run("d = record([(1, 2), (3, 4)])\nprint(d.popitem(), d)\nprint(d.setdefault(1, 9), d.pop(1), d)"), "(3, 4) {1 = 2}\n2 2 {}\n");
    assert_eq!(run("d = record([(1, 2)])\nc = d.copy()\nd.clear()\nprint(c, d, c == d)"), "{1 = 2} {} false\n");
    assert_eq!(run("d = record([(1, \"a\"), (2, \"b\")])\nk, v, i = (d.keys(), d.values(), d.items())\nd[1] = \"z\"\nprint(list(k), list(v), list(i))"), "[1, 2] [\"z\", \"b\"] [(1, \"z\"), (2, \"b\")]\n");
}

#[test]
fn errors_keep_the_engine_words() {
    assert_eq!(run("{}.popitem()"), "KeyError: popitem found the record empty");
    assert_eq!(run("{}.pop(\"x\")"), "KeyError: \"x\"");
    assert_eq!(run("{}.get()"), "TypeError: record.get() takes 1 to 2 arguments, not 0");
    assert_eq!(run("{}.update(1, 2)"), "TypeError: record.update() takes 0 to 1 arguments, not 2");
    assert_eq!(run("{}.keys(1)"), "TypeError: record.keys() takes 0 arguments, not 1");
    assert_eq!(run("{}.get(1, x=2)"), "TypeError: record.get() takes no keyword x");
    assert_eq!(run("{}.update([\"abc\"])"), "ValueError: entry 0 holds 3 items where a pair holds 2");
    assert_eq!(run("{}.update([1])"), "TypeError: each entry must be a pair of a key and a value");
    assert_eq!(run("{}.get([1])"), "TypeError: a value of kind list cannot stand as a key");
    assert_eq!(run("record.fromkeys([[1]])"), "TypeError: a value of kind list cannot stand as a key");
}

#[test]
fn a_function_a_record_holds_calls_through_its_field() {
    let src = "fn add(a, b) {\n    return a + b\n}\nfn warm(k) {\n    t = 0\n    for i in range(30) {\n        t += k.f(i, 1)\n    }\n    return t\n}\nk = {f = add, n = 2}\nprint(k.f(1, 2), k.n, warm(k))\nk.g(1)";
    assert_eq!(run(src), "3 2 465\nFieldError: record has no field g");
}
