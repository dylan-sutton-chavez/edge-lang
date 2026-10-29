use edge::embed::{Limits, VM};
use edge::lang::Fast1;
use edge::value::{HeapObj, HeapPool, Val};
use mini::RUNTIME;

/* what `src` printed and its error after it when one ended the run */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    // no memory limit so no metering allocator is needed
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits { ops: 50_000_000, memory: usize::MAX });
    let end = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = end { out.push_str(&e.to_string()); }
    out
}

fn prints(src: &str, want: &str) { assert_eq!(run(src), want, "{src}"); }

fn raises(src: &str, want: &str) {
    let out = run(src);
    assert!(out.contains(want), "{src}: {out}");
}

#[test]
fn len_counts_chars_items_and_range_steps() {
    prints("print(len('héllo'), len(b'ab'), len([1]), len({1: 2}), len({3}), len(range(10, 0, -3)), len(range(10**20, 10**20 + 7)))", "5 2 1 1 1 4 7\n");
    raises("len(range(-2**62, 2**62))", "OverflowError");
    raises("len(5)", "object of type 'int' has no len()");
    raises("len()", "len() takes exactly one argument (0 given)");
}

#[test]
fn range_keeps_word_ints_and_widens_past_them() {
    prints("print(list(range(3)), list(range(5, 1, -2)), list(range(true)), list(range(10**20, 10**20 + 2)))", "[0, 1, 2] [5, 3] [0] [100000000000000000000, 100000000000000000001]\n");
    raises("range(1.5)", "'float' object cannot be interpreted as an integer");
    raises("range(10**40, 'a')", "'str' object cannot be interpreted as an integer");
    raises("range(10**20, 5, 0)", "range() arg 3 must not be zero");
    raises("range(1, 2, 0)", "range() arg 3 must not be zero");
}

#[test]
fn iter_makes_each_kind_its_iterator() {
    prints("print([type(iter(x)).__name__ for x in [[1], (1,), 'a', 'ñ', {1: 2}, {1}, b'a', range(2)]])",
        "['list_iterator', 'tuple_iterator', 'str_ascii_iterator', 'str_iterator', 'dict_keyiterator', 'set_iterator', 'bytes_iterator', 'range_iterator']\n");
    prints("n = [0]\nfn f():\n    n[0] += 1\n    return n[0]\nprint(list(iter(f, 4)), type(iter(f, 9)).__name__)", "[1, 2, 3] callable_iterator\n");
    raises("iter(5, 1)", "iter(v, w): v must be callable");
    raises("iter(5)", "'int' object is not iterable");
}

#[test]
fn next_steps_iterators_and_generators_to_their_end() {
    prints("it = iter([1, 2])\nprint(next(it), next(it), next(it, 'd'))", "1 2 d\n");
    prints("fn g():\n    yield 1\n    return 'r'\nit = g()\nprint(next(it))\ntry:\n    next(it)\nexcept StopIteration as e:\n    print('stop', e.value)\nprint(next(it, 'd'))", "1\nstop r\nd\n");
    prints("fn g(n=4):\n    for i in range(n):\n        yield i\ngs = [g()]\nprint(list(map(any, gs)), next(gs[0]), next(gs[0], 'e'), next(gs[0], 'e'))", "[True] 2 3 e\n");
    prints("class C:\n    fn __init__(self):\n        self.n = 0\n    fn step(self):\n        self.n += 1\n        return self.n\nit = iter(C().step, 3)\nprint(next(it), next(it), next(it, 'end'))", "1 2 end\n");
    prints("m = map(lambda v: v * 2, [1, 2])\nprint(next(m), next(m), next(m, 'e'))", "2 4 e\n");
    raises("next([1])", "'list' object is not an iterator");
    raises("next(iter([]))", "StopIteration");
}

#[test]
fn enumerate_zip_map_and_filter_build_lazy_iterators() {
    prints("print(list(enumerate('ab', 1)), list(enumerate(iterable='ab')), list(enumerate('ab', true)))", "[(1, 'a'), (2, 'b')] [(0, 'a'), (1, 'b')] [(1, 'a'), (2, 'b')]\n");
    raises("enumerate('ab', 1.5)", "enumerate() start must be an integer");
    raises("enumerate('a', 1, start=2)", "got multiple values for argument 'start'");
    prints("print(list(zip('ab', 'xyz')), list(zip()), list(zip('ab', 'xy', strict=true)))", "[('a', 'x'), ('b', 'y')] [] [('a', 'x'), ('b', 'y')]\n");
    raises("list(zip('ab', 'xyz', strict=true))", "zip() argument 2 is longer than argument 1");
    prints("print(list(map(lambda a, b: a + b, [1, 2], [10, 20, 30])), list(filter(nil, [0, 1, '', 'a'])))", "[11, 22] [1, 'a']\n");
}

#[test]
fn reversed_walks_back_live_lists_ranges_and_copies() {
    prints("xs = [1, 2, 3]\nr = reversed(xs)\nxs.append(4)\nprint(list(r), list(reversed('ab')), list(reversed((1, 2))), list(reversed({1: 0, 2: 0})))", "[3, 2, 1] ['b', 'a'] [2, 1] [2, 1]\n");
    prints("print(list(reversed(range(5))), list(reversed(range(10, 0, -3))), list(reversed(range(10**20, 10**20 + 2))))", "[4, 3, 2, 1, 0] [1, 4, 7, 10] [100000000000000000001, 100000000000000000000]\n");
    prints("print(next(reversed(range(-2**63, 2**63 - 1))), list(reversed(range(-2**63, -2**63 + 2))))", "9223372036854775806 [-9223372036854775807, -9223372036854775808]\n");
    raises("reversed({1, 2})", "'set' object is not reversible");
}

#[test]
fn all_and_any_stop_at_the_item_that_decides() {
    prints("print(all([]), any([]), all([1, 0]), any([0, 'a']), all(range(1, 9)), all(range(-4, 4, 2)), any(range(0, 1)), any(range(0, 2)))", "True False False True True False False True\n");
    prints("log = []\nfn g():\n    for i in range(5):\n        log.append(i)\n        yield i\nit = g()\nprint(any(it), list(log), next(it))", "True [0, 1] 2\n");
    raises("all(5)", "'int' object is not iterable");
}

#[test]
fn range_methods_find_by_arithmetic() {
    prints("r = range(0, 20, 3)\nprint(r.index(9), r.index(9.0), r.count(9), r.count(10), r.count(9.0), range(10**20, 10**20 + 9, 3).index(10**20 + 6))", "3 3 1 0 1 2\n");
    raises("range(5).index(7)", "range.index(x): x not in range");
    raises("range(5).index('a')", "sequence.index(x): x not in sequence");
    raises("range(10**20, 10**20 + 3).index('a')", "sequence.index(x): x not in sequence");
}

#[test]
fn a_generator_takes_sends_throws_and_closes() {
    prints("fn acc():\n    t = 0\n    while true:\n        v = yield t\n        t += v\na = acc()\nnext(a)\nprint(a.send(1), a.send(2))", "1 3\n");
    prints("fn g():\n    try:\n        yield 1\n    except KeyError:\n        yield 'k'\nit = g()\nnext(it)\nprint(it.throw(KeyError), it.close())", "k None\n");
    raises("fn g():\n    yield 1\ng().send(1)", "can't send non-None value to a just-started generator");
    raises("fn g():\n    yield 1\ng().send()", "generator.send() takes exactly one argument (0 given)");
}

/* the fast path mini's iteration table gives native `name` */
fn fast(name: &str) -> Fast1 { mini::natives::ITER.iter().find(|n| n.name == name).and_then(|n| n.fast1).unwrap_or_else(|| panic!("{name}")) }

/* a fold's fast path over a word range and the items it read */
fn truth(heap: &mut HeapPool, name: &str, s: i64, e: i64, st: i64) -> Option<(bool, usize)> {
    let r = heap.alloc(HeapObj::Range(s, e, st)).ok()?;
    heap.take_work();
    let v = fast(name)(heap, r)?;
    Some((v.as_bool(), heap.take_work()))
}

#[test]
fn the_fast_paths_answer_as_the_full_calls_do() {
    let mut heap = HeapPool::new(usize::MAX);
    let put = |heap: &mut HeapPool, obj: HeapObj| heap.alloc(obj).unwrap_or(Val::none());
    let (len_of, range_to, all_of, any_of, reversed_of) = (fast("len"), fast("range"), fast("all"), fast("any"), fast("reversed"));
    let s = put(&mut heap, HeapObj::Str("ñandú".into()));
    let r = put(&mut heap, HeapObj::Range(0, 10, 3));
    let t = put(&mut heap, HeapObj::Tuple(vec![Val::int(1), Val::int(0)]));
    assert_eq!(len_of(&mut heap, s).map(|v| v.as_int()), Some(5));
    assert_eq!(len_of(&mut heap, r).map(|v| v.as_int()), Some(4));
    assert!(len_of(&mut heap, Val::int(3)).is_none());
    assert!(range_to(&mut heap, Val::int(4)).is_some_and(|v| matches!(heap.try_get(v), Some(HeapObj::Range(0, 4, 1)))));
    assert!(range_to(&mut heap, Val::float(4.0)).is_none());
    assert_eq!(all_of(&mut heap, t).map(|v| v.as_bool()), Some(false));
    assert_eq!(any_of(&mut heap, r).map(|v| v.as_bool()), Some(true));
    // a range settles by arithmetic and pays for the items up to the deciding one
    assert_eq!(truth(&mut heap, "all", -6, 6, 2), Some((false, 4)));
    assert_eq!(truth(&mut heap, "all", 1, 5, 1), Some((true, 4)));
    assert_eq!(truth(&mut heap, "any", 0, 1, 1), Some((false, 1)));
    let it = reversed_of(&mut heap, t);
    assert!(it.is_some_and(|v| matches!(heap.try_get(v), Some(HeapObj::Iter(_, "reversed")))));
    let far = put(&mut heap, HeapObj::Range(i64::MAX, i64::MIN, i64::MIN));
    assert!(reversed_of(&mut heap, far).is_none());
}
