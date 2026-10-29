use edge::embed::{Limits, VM};
use mini::RUNTIME;

const GEN: &str = "fn g(n):\n    for i in range(n):\n        print('g', n, i)\n        yield i\n";

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
    assert_eq!(with_gen("s = {9}\ns.update(g(2), [5], g(1))\nprint(sorted(s))"), "g 2 0\ng 2 1\ng 1 0\n[0, 1, 5, 9]\n");
    assert_eq!(with_gen("print(sorted(set().union(g(1), g(2))), {0, 1, 7}.intersection(g(2), [1, 7]))"), "g 1 0\ng 2 0\ng 2 1\ng 2 0\ng 2 1\n[0, 1] {1}\n");
    assert_eq!(with_gen("print({1, 2}.issubset(g(3)), {0, 5}.isdisjoint(g(1)), {0, 9}.symmetric_difference(g(2)))"), "g 3 0\ng 3 1\ng 3 2\ng 1 0\ng 2 0\ng 2 1\nTrue False {9, 1}\n");
    assert_eq!(with_gen("s = {0, 1, 2}\ns.difference_update(g(1), g(1))\ns.intersection_update(g(3), (2,))\nprint(s)"), "g 1 0\ng 1 0\ng 3 0\ng 3 1\ng 3 2\n{2}\n");
}

#[test]
fn an_operand_in_a_hot_loop_drains_each_time() {
    assert_eq!(run("t = 0\nfor i in range(3000):\n    t += len({i}.union(x for x in range(3)))\nprint(t)"), "11997\n");
    assert_eq!(run("t = 0\nfor i in range(3000):\n    t += {i}.issubset(x for x in range(5))\nprint(t)"), "5\n");
}

#[test]
fn a_failing_operand_leaves_the_set_as_it_was() {
    let src = "fn bad():\n    yield 1\n    raise ValueError('half')\ns = {7}\ntry:\n    s.update([2], bad())\nexcept ValueError as e:\n    print('caught', e)\nprint(s)";
    assert_eq!(run(src), "caught half\n{7}\n");
    assert_eq!(run("s = {7}\ntry:\n    s.update([3], [[1]])\nexcept TypeError as e:\n    print(e)\nprint(s)"), "cannot use 'list' as a set element (unhashable type: 'list')\n{7}\n");
}

#[test]
fn a_set_operand_answers_in_place_as_a_copy_would() {
    assert_eq!(run("a = {1, 2}\nprint(a.issubset({1, 2, 3}), a.issubset({1}), a.issuperset({1}), a.issuperset(a))"), "True False True True\n");
    assert_eq!(run("a = {1, 2}\nprint(a.isdisjoint({3}), a.isdisjoint({2, 3, 4, 5}), set().isdisjoint(set()), a.isdisjoint([3, 4]))"), "True False True True\n");
}

#[test]
fn errors_keep_the_engine_words() {
    assert_eq!(run("{1}.issubset([[1]])"), "TypeError: unhashable type: 'list'");
    assert_eq!(run("{1}.isdisjoint([[1]])"), "TypeError: cannot use 'list' as a set element (unhashable type: 'list')");
    assert_eq!(run("set().add()"), "TypeError: set.add() takes exactly one argument (0 given)");
    assert_eq!(run("set().update(x=1)"), "TypeError: set.update() takes no keyword arguments");
    assert_eq!(run("set().pop()"), "KeyError: 'pop from an empty set'");
    assert_eq!(run("s = {1}\ns.remove(s)"), "KeyError: {1}");
    assert_eq!(run("{1}.remove([2])"), "TypeError: cannot use 'list' as a set element (unhashable type: 'list')");
    assert_eq!(run("s = {1, 2}\ns.discard({3})\ns.discard(2)\nprint(s, s.copy() is s, s.copy() == s)"), "{1} False True\n");
}
