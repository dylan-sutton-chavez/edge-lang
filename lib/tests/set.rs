use edge::embed::{Limits, VM};
use mini::RUNTIME;

const GEN: &str = "fn g(n) {\n    for i in range(n) {\n        print(\"g\", n, i)\n        yield i\n    }\n}\n";

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
fn operands_that_step_run_as_frames_in_order() {
    assert_eq!(with_gen("s = set([9])\ns.update(g(2), [5], g(1))\nprint(sorted(s))"), "g 2 0\ng 2 1\ng 1 0\n[0, 1, 5, 9]\n");
    assert_eq!(with_gen("print(sorted(set().union(g(1), g(2))), set([0, 1, 7]).intersection(g(2), [1, 7]))"), "g 1 0\ng 2 0\ng 2 1\ng 2 0\ng 2 1\n[0, 1] {1}\n");
    assert_eq!(with_gen("print(set([1, 2]).issubset(g(3)), set([0, 5]).isdisjoint(g(1)), set([0, 9]).symmetric_difference(g(2)))"), "g 3 0\ng 3 1\ng 3 2\ng 1 0\ng 2 0\ng 2 1\ntrue false {9, 1}\n");
    assert_eq!(with_gen("s = set([0, 1, 2])\ns.difference_update(g(1), g(1))\ns.intersection_update(g(3), (2,))\nprint(s)"), "g 1 0\ng 1 0\ng 3 0\ng 3 1\ng 3 2\n{2}\n");
}

#[test]
fn an_operand_in_a_hot_loop_drains_each_time() {
    assert_eq!(run("t = 0\nfor i in range(3000) {\n    t += len(set([i]).union((x for x in range(3))))\n}\nprint(t)"), "11997\n");
    assert_eq!(run("t = 0\nfor i in range(3000) {\n    t += set([i]).issubset((x for x in range(5)))\n}\nprint(t)"), "5\n");
}

#[test]
fn a_failing_operand_leaves_the_set_as_it_was() {
    let src = "fn bad() {\n    yield 1\n    throw ValueError(\"half\")\n}\ns = set([7])\ntry {\n    s.update([2], bad())\n} catch ValueError as e {\n    print(\"caught\", e)\n}\nprint(s)";
    assert_eq!(run(src), "caught half\n{7}\n");
    assert_eq!(run("s = set([7])\ntry {\n    s.update([3], [[1]])\n} catch TypeError as e {\n    print(e)\n}\nprint(s)"), "a value of kind list cannot stand as a key\n{7}\n");
}

#[test]
fn a_set_operand_answers_in_place_as_a_copy_would() {
    assert_eq!(run("a = set([1, 2])\nprint(a.issubset(set([1, 2, 3])), a.issubset(set([1])), a.issuperset(set([1])), a.issuperset(a))"), "true false true true\n");
    assert_eq!(run("a = set([1, 2])\nprint(a.isdisjoint(set([3])), a.isdisjoint(set([2, 3, 4, 5])), set().isdisjoint(set()), a.isdisjoint([3, 4]))"), "true false true true\n");
}

#[test]
fn errors_keep_the_engine_words() {
    assert_eq!(run("set([1]).issubset([[1]])"), "TypeError: a value of kind list cannot stand as a key");
    assert_eq!(run("set([1]).isdisjoint([[1]])"), "TypeError: a value of kind list cannot stand as a key");
    assert_eq!(run("set().add()"), "TypeError: set.add() takes 1 argument, not 0");
    assert_eq!(run("set().update(x=1)"), "TypeError: set.update() takes no keyword x");
    assert_eq!(run("set().pop()"), "KeyError: pop found the set empty");
    assert_eq!(run("s = set([1])\ns.remove(s)"), "KeyError: {1}");
    assert_eq!(run("set([1]).remove([2])"), "TypeError: a value of kind list cannot stand as a key");
    assert_eq!(run("s = set([1, 2])\ns.discard(set([3]))\ns.discard(2)\nc = s.copy()\nc.add(9)\nprint(s, c == s, s.copy() == s)"), "{1} false true\n");
}
