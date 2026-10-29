use edge::embed::{Limits, VM};
use mini::RUNTIME;

/* what `src` printed and then the words of the error it ended in */
fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits { ops: 10_000_000, memory: usize::MAX });
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

#[test]
fn plain_kinds_order_stably_both_ways() {
    assert_eq!(run("print(sorted([3, 1, 2]), sorted([3, 1, 2], reverse=true), sorted([]), sorted([7]))"), "[1, 2, 3] [3, 2, 1] [] [7]\n");
    assert_eq!(run("print(sorted([1, 1.0, true]), sorted([true, 1.0, 1], reverse=true), sorted([0.0, -0.0]), sorted([-0.0, 0.0]))"), "[1, 1.0, True] [True, 1.0, 1] [0.0, -0.0] [-0.0, 0.0]\n");
    assert_eq!(run("print(sorted([2.5, -1, 3, -7.25, 0]), sorted(['b', 'a', 'ab', 'é', 'B']), sorted('cab'))"), "[-7.25, -1, 0, 2.5, 3] ['B', 'a', 'ab', 'b', 'é'] ['a', 'b', 'c']\n");
    assert_eq!(run("print(sorted([(1, 'b'), (0, 'z'), (1, 'a')]), sorted([(2,), (1, 2), (1,)]), sorted([((1, 2), 'x'), ((1, 1), 'y')], reverse=true))"), "[(0, 'z'), (1, 'a'), (1, 'b')] [(1,), (1, 2), (2,)] [((1, 2), 'x'), ((1, 1), 'y')]\n");
    assert_eq!(run("print(sorted({3: 0, 1: 0}), sorted(range(5, 0, -2)), sorted((3, 1)))"), "[1, 3] [1, 3, 5] [1, 3]\n");
}

#[test]
fn the_full_comparison_orders_what_no_plain_kind_does() {
    assert_eq!(run("print(sorted([[2], [1, 5], [1]]), sorted([3, float('nan'), 1]), sorted([2 ** 70, 1, -2 ** 70]))"), "[[1], [1, 5], [2]] [3, nan, 1] [-1180591620717411303424, 1, 1180591620717411303424]\n");
    assert_eq!(run("print(sorted([(1, 'a'), (1, 2)]))"), "TypeError: '<' not supported between instances of 'int' and 'str'");
    assert_eq!(run("xs = [[i % 3, -i] for i in range(200)]\nys = sorted(xs, reverse=true)\nprint(ys[:2], ys[-2:], ys == [list(t) for t in sorted([(a, b) for a, b in xs], reverse=true)])"), "[[2, -2], [2, -5]] [[0, -195], [0, -198]] True\n");
}

#[test]
fn a_long_run_merges_as_a_stable_sort() {
    let src = "xs = [((i * 7919) % 1000, i) for i in range(3000)]\nby = sorted(xs, key=lambda p: p[0] ~/ 10)\nok = all(a[0] ~/ 10 < b[0] ~/ 10 || a[0] ~/ 10 == b[0] ~/ 10 && a[1] < b[1] for a, b in zip(by, by[1:]))\nprint(len(by), ok)";
    assert_eq!(run(src), "3000 True\n");
    let src = "xs = [[(i * 37) % 101, i] for i in range(1500)]\nys = sorted(xs)\nprint(all(a < b for a, b in zip(ys, ys[1:])), ys[0], ys[-1])";
    assert_eq!(run(src), "True [0, 0] [100, 1444]\n");
}

#[test]
fn a_key_runs_once_on_each_item_in_order() {
    assert_eq!(run("print(sorted([3, 1, 2], key=lambda v: -v), sorted(['bb', 'a', 'ccc'], key=len, reverse=true), sorted([3, 1], key=nil))"), "[3, 2, 1] ['ccc', 'bb', 'a'] [1, 3]\n");
    assert_eq!(run("seen = []\nfn k(v):\n    seen.append(v)\n    return v % 2\nprint(sorted([4, 1, 3, 2], key=k), seen)"), "[4, 2, 1, 3] [4, 1, 3, 2]\n");
    assert_eq!(run("seen = []\nfn k(v):\n    seen.append(v)\n    return 0\nprint(sorted([9], key=k), sorted([], key=k), seen)"), "[9] [] [9]\n");
    assert_eq!(run("rows = [('x', 3), ('y', 1), ('z', 3), ('w', 2)]\nprint(sorted(rows, key=lambda r: r[1], reverse=true))"), "[('x', 3), ('z', 3), ('w', 2), ('y', 1)]\n");
}

#[test]
fn a_generator_is_gathered_before_its_keys() {
    let src = "fn g():\n    for i in [3, 1, 2]:\n        print('yield', i)\n        yield i\nfn k(v):\n    print('key', v)\n    return -v\nprint(sorted(g(), key=k))\nprint(sorted(g(), reverse=true), sorted(i * i for i in []))";
    assert_eq!(run(src), "yield 3\nyield 1\nyield 2\nkey 3\nkey 1\nkey 2\n[3, 2, 1]\nyield 3\nyield 1\nyield 2\n[3, 2, 1] []\n");
    assert_eq!(run("print(sorted(map(lambda v: v * 2, [2, 1])), sorted(filter(nil, [0, 2, 1]), key=lambda v: -v))"), "[2, 4] [2, 1]\n");
    assert_eq!(run("print(sorted(map(lambda v: v + 1, [3, 1, 2]), key=lambda v: -v), sorted(filter(lambda v: v > 1, [3, 1, 2]), key=lambda v: -v))"), "[4, 3, 2] [3, 2]\n");
    assert_eq!(run("fn g(n):\n    yield from range(n)\nprint(sorted((g(k) for k in range(3)), key=lambda it: -len(list(it)))[0] is !nil, sorted(g(4), key=lambda v: v % 2))"), "True [0, 2, 1, 3]\n");
}

#[test]
fn list_sort_orders_in_place() {
    assert_eq!(run("xs = [3, 1, 2]\nprint(xs.sort(), xs)\nxs.sort(reverse=true)\nprint(xs)\nxs.sort(key=lambda v: v % 3)\nprint(xs)"), "None [1, 2, 3]\n[3, 2, 1]\n[3, 1, 2]\n");
    assert_eq!(run("ws = ['b', 'C', 'a']\nws.sort(key=str.lower)\nprint(ws)\nws.sort()\nprint(ws)\nts = [(1, 'b'), (1, 'a')]\nts.sort()\nprint(ts)"), "['a', 'b', 'C']\n['C', 'a', 'b']\n[(1, 'a'), (1, 'b')]\n");
    assert_eq!(run("seen = []\nxs = []\nxs.sort(key=lambda v: seen.append(v))\nys = [5]\nys.sort(key=lambda v: seen.append(v))\nprint(xs, ys, seen)"), "[] [5] [5]\n");
    assert_eq!(run("xs = [[2], [1]]\nxs.sort()\nprint(xs)\nl = [1]\nl.append(l)\ntry:\n    l.sort()\nexcept TypeError as e:\n    print(e, len(l))"), "[[1], [2]]\n'<' not supported between instances of 'list' and 'int' 2\n");
}

#[test]
fn a_key_that_changes_the_list_keys_and_sorts_what_it_held() {
    assert_eq!(run("l = [3, 1, 2]\nl.sort(key=lambda v: (l.append(0), v)[1])\nprint(l)"), "[1, 2, 3]\n");
    assert_eq!(run("l = [3, 1, 2]\nl.sort(key=lambda v: (l.clear(), -v)[1])\nprint(l)"), "[3, 2, 1]\n");
    assert_eq!(run("l = [3, 1, 2, 5]\nseen = []\nl.sort(key=lambda v: (seen.append(v), l.pop(), v)[2])\nprint(l, seen)"), "[1, 2, 3, 5] [3, 1, 2, 5]\n");
}

#[test]
fn an_error_leaves_the_list_as_it_was() {
    let src = "fn bad(v):\n    if v == 2:\n        raise KeyError(v)\n    return v\nys = [5, 2, 9]\ntry:\n    ys.sort(key=bad)\nexcept KeyError as e:\n    print('KeyError', e, ys)\ntry:\n    sorted(ys, key=bad)\nexcept KeyError as e:\n    print('again', e)";
    assert_eq!(run(src), "KeyError 2 [5, 2, 9]\nagain 2\n");
    assert_eq!(run("zs = [3, 'a', 1]\ntry:\n    zs.sort()\nexcept TypeError as e:\n    print(e, zs)"), "'<' not supported between instances of 'str' and 'int' [3, 'a', 1]\n");
    assert_eq!(run("fn stop(v):\n    raise StopIteration(v)\ntry:\n    sorted([1, 2], key=stop)\nexcept StopIteration as e:\n    print('stop', e.value)"), "stop 1\n");
}

#[test]
fn errors_keep_their_words() {
    assert_eq!(run("[].sort(1)"), "TypeError: sort() takes no positional arguments");
    assert_eq!(run("[].sort(1, 2, 3)"), "TypeError: sort() takes at most 2 arguments (3 given)");
    assert_eq!(run("[].sort(kye=len)"), "TypeError: sort() got an unexpected keyword argument 'kye'");
    assert_eq!(run("[].sort(reversed=true)"), "TypeError: sort() got an unexpected keyword argument 'reversed'. Did you mean 'reverse'?");
    assert_eq!(run("sorted()"), "TypeError: sorted() takes exactly one argument (0 given)");
    assert_eq!(run("sorted([], keys=len)"), "TypeError: sorted() got an unexpected keyword argument 'keys'");
    assert_eq!(run("sorted(5)"), "TypeError: 'int' object is not iterable");
    assert_eq!(run("sorted([1, 2], key=5)"), "TypeError: 'int' object is not callable");
}

// top beside mini's tables as a runtime that ranks rows would name it
const TOP: &[edge::lang::Native] = &lib::table![mini::Mini; "top" => lib::seq::top::top{"key"}];

static RANKED: edge::lang::Natives = edge::natives! {
    language: Ranked,
    fns: [mini::natives::IO, mini::natives::ITER, mini::natives::NEXT, mini::natives::NUM, mini::natives::FOLDS, mini::natives::CONVERT, mini::natives::BYTES, mini::natives::SORT, mini::natives::ATTR, mini::natives::TYPES, TOP],
    methods: [
        Str => [mini::natives::STR],
        List => [mini::natives::LIST, mini::natives::LIST_SORT],
        Tuple => [mini::natives::TUPLE],
        Dict => [mini::natives::DICT],
        Int => [mini::natives::INT],
        Float => [mini::natives::FLOAT],
    ],
    types: mini::types::NAMES,
};

static RANKED_RUNTIME: edge::lang::Runtime = edge::lang::Runtime::of::<Ranked>();

// mini's language over the ranked natives
struct Ranked;
mini::over!(Ranked, &RANKED);

/* what `src` printed under mini's tables with top beside them */
fn ranked(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}");
    let mut vm = VM::with_runtime(&RANKED_RUNTIME, &chunk, Limits { ops: 50_000_000, memory: usize::MAX });
    let end = vm.run().err().map(|e| e.to_string()).unwrap_or_default();
    vm.output_text() + &end
}

#[test]
fn top_keeps_the_rows_a_stable_descending_sort_puts_first() {
    let src = concat!(
        "fn same(xs, key):\n    return all(top(xs, k, key=key) == sorted(xs, key=key, reverse=true)[:k] for k in [0, 1, 2, 3, 7, 10, len(xs) - 1, len(xs), len(xs) + 5])\n",
        "ints = [(i * 37) % 11 for i in range(300)]\nmixed = [(i * 13) % 7 + (0.5 if i % 3 == 0 else 0) for i in range(200)]\n",
        "words = ['pear', 'fig', 'kiwi', 'fig', 'plum', 'pear', 'date'] * 9\npairs = [((i * 7919) % 23, i % 4) for i in range(400)]\n",
        "lists = [[i % 3, (i * 5) % 4] for i in range(60)]\nwide = [2 ** 70, 5, -2 ** 70, 2 ** 70, 7, 2 ** 69, 5]\n",
        "print(same(ints, nil), same(mixed, nil), same(words, nil), same(pairs, nil), same(lists, nil), same(wide, nil))\n",
        "print(same(pairs, lambda p: p[0]), same(words, len), same(ints, lambda v: -v))\n",
        "stars = {'Strata': 4, 'Fieldwatch': 4, 'ponytail': 4, 'DiPlay': 3, 'AIHOT': 3, 'nature-skills': 2}\n",
        "print(top(stars, 2), top(stars, 4)[-1])\n",
        "fn said(f):\n    try:\n        f()\n    except TypeError as e:\n        return str(e)\n",
        "print(said(lambda: top([3, 'a', 1, 'b'], 1)))\nprint(said(lambda: sorted([3, 'a', 1, 'b'], reverse=true)))\n",
    );
    assert_eq!(ranked(src), "True True True True True True\nTrue True True\n[('Strata', 4), ('Fieldwatch', 4)] ('DiPlay', 3)\n'<' not supported between instances of 'int' and 'str'\n'<' not supported between instances of 'int' and 'str'\n");
}

#[test]
fn the_greatest_keys_cost_about_one_comparison_each() {
    let keys: Vec<u64> = (0..10_000u64).map(|i| (i * 2_654_435_761) % 1_000).collect();
    let mut asked = 0usize;
    let kept = lib::seq::top::greatest::<()>(keys.len(), 10, |x, y| { asked += 1; Ok(keys[x] < keys[y]) }).unwrap_or_default();
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(keys[i]));
    let (mut got, mut want) = (kept.clone(), order[..10].to_vec());
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want);
    // a full sort asks over a hundred thousand times for these keys
    assert!(asked < 2 * keys.len(), "asked {asked} times");
}
