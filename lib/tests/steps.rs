use edge::embed::{Limits, VM};
use edge::lang::{Acc, Args, Cont, Cx, Flow, Fold, Item, Native, Natives, Ret, Runtime, STEPS, Sent, Sig, Step, Then};
use edge::value::{SchedulerStatus, Val, VmErr};
use mini::Mini;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

const STEPPED: &[Native] = &[
    Native::new("apply", Sig::exact(2), apply).flags(STEPS),
    Native::new("apply_inc", Sig::exact(2), apply_inc).flags(STEPS),
    Native::new("twice", Sig::exact(2), twice).flags(STEPS),
    Native::new("total", Sig::exact(1), total).flags(STEPS),
    Native::new("first", Sig::exact(1), first).flags(STEPS),
    Native::new("keys", Sig::exact(2), keys).flags(STEPS),
    Native::new("resume", Sig::exact(2), resume).flags(STEPS),
    Native::new("shut", Sig::exact(1), shut).flags(STEPS),
    Native::new("wait", Sig::exact(0), wait),
    Native::new("sums", Sig::range(1, 4), sums).flags(STEPS),
    Native::new("lists", Sig::range(1, 3), lists).flags(STEPS),
    Native::new("joins", Sig::range(1, 4), joins).flags(STEPS),
    Native::new("chained", Sig::range(2, 4), chained).flags(STEPS),
    Native::new("every", Sig::exact(1), every).flags(STEPS),
    Native::new("words", Sig::exact(1), words).flags(STEPS),
    Native::new("most", Sig::exact(1), most).flags(STEPS),
];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, STEPPED],
    methods: [
        Str => [STR],
        Bytes => [BUFFER],
        ByteArray => [ARRAY],
        List => [LIST, LIST_SORT],
        Tuple => [TUPLE],
        Dict => [DICT],
        Set => [SET, SET_MANY],
        Int => [INT],
        Float => [FLOAT],
        Range => [RANGE],
        Coro => [GEN],
    ],
    types: mini::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Over>();

// mini's language over these natives
struct Over;
mini::over!(Over, &NATIVES);

fn apply(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::call(a.pos[0], &a.pos[1..2]).into()) }

fn apply_inc(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::call_then(a.pos[0], &a.pos[1..2], Then::Fn(|cx, v| Ok(cx.add(v, Val::int(1))?.into()))).into()) }

// calls its function on its own value as the second call
struct Again(Val);

impl Cont for Again {
    fn resume(self: Box<Self>, _: &mut Cx<'_, '_>, got: Val) -> Ret { Ok(Step::call(self.0, &[got]).into()) }
    fn trace(&self, mark: &mut dyn FnMut(Val)) { mark(self.0); }
}

fn twice(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::call_then(a.pos[0], &a.pos[1..2], Then::Cont(Box::new(Again(a.pos[0])))).into()) }

fn total(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::new(sum_fold) }.into()) }

fn sum_fold(acc: &mut Acc, _: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) if v.is_int() => { acc.n += v.as_int(); Ok(Flow::More) }
        Item::Next(_) => Err(VmErr::Type("total takes ints")),
        Item::End(_) => Ok(Flow::Stop(Val::int(acc.n))),
        Item::Called(_) => Ok(Flow::More),
    }
}

fn first(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::new(first_fold) }.into()) }

fn first_fold(_: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => Ok(Flow::Stop(v)),
        Item::End(ret) => Err(cx.stop_iteration(ret)),
        Item::Called(_) => Ok(Flow::More),
    }
}

fn keys(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mut acc = Acc::new();
    acc.v[0] = a.pos[1];
    Ok(Step::Drain { it: a.pos[0], fold: Fold::with(keys_fold, acc) }.into())
}

fn keys_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => Ok(Flow::Call(acc.v[0], v)),
        Item::Called(k) => { acc.items.push(k); Ok(Flow::More) }
        Item::End(_) => Ok(Flow::Stop(cx.new_list(core::mem::take(&mut acc.items))?)),
    }
}

// the iterables a chained fold walks after the first kept last first in `v`
fn pending(a: &Args<'_>) -> Acc {
    let mut acc = Acc::new();
    for (k, &it) in a.pos[1..].iter().rev().enumerate() { acc.v[k] = it; }
    acc.n = (a.pos.len() - 1) as i64;
    acc
}

// the next iterable a chained fold walks or None once all went
fn next_of(acc: &mut Acc) -> Option<Val> {
    if acc.n == 0 { return None; }
    acc.n -= 1;
    Some(core::mem::replace(&mut acc.v[acc.n as usize], Val::undef()))
}

fn sums(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mut acc = pending(&a);
    acc.f = (0.0, 0.0);
    Ok(Step::Drain { it: a.pos[0], fold: Fold::with(sums_fold, acc) }.into())
}

fn sums_fold(acc: &mut Acc, _: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) if v.is_int() => { acc.f.0 += v.as_int() as f64; Ok(Flow::More) }
        Item::Next(_) => Err(VmErr::Type("sums takes ints")),
        Item::End(_) => Ok(match next_of(acc) { Some(it) => Flow::Drain(it), None => Flow::Stop(Val::int(acc.f.0 as i64)) }),
        Item::Called(_) => Ok(Flow::More),
    }
}

fn lists(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mut acc = pending(&a);
    acc.v[3] = cx.new_list(Vec::new())?;
    Ok(Step::Drain { it: a.pos[0], fold: Fold::collect(lists_end, acc) }.into())
}

fn lists_end(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return Ok(Flow::More) };
    let part = cx.new_list(core::mem::take(&mut acc.items))?;
    if let Some(edge::value::HeapObj::List(rc)) = cx.heap().try_get(acc.v[3]) { rc.borrow_mut().push(part); }
    Ok(match next_of(acc) { Some(it) => Flow::Drain(it), None => Flow::Stop(acc.v[3]) })
}

fn joins(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::collect(joins_end, pending(&a)) }.into()) }

// the items stay gathered across the chain and form one list at its end
fn joins_end(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return Ok(Flow::More) };
    Ok(match next_of(acc) { Some(it) => Flow::Drain(it), None => Flow::Stop(cx.new_list(core::mem::take(&mut acc.items))?) })
}

// calls its first argument on each item of the iterables after it
fn chained(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mut acc = Acc::new();
    for (k, &it) in a.pos[2..].iter().rev().enumerate() { acc.v[k] = it; }
    (acc.n, acc.v[3]) = ((a.pos.len() - 2) as i64, a.pos[0]);
    Ok(Step::Drain { it: a.pos[1], fold: Fold::with(chained_fold, acc) }.into())
}

fn chained_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => Ok(Flow::Call(acc.v[3], v)),
        Item::Called(k) => { acc.items.push(k); Ok(Flow::More) }
        Item::End(_) => Ok(match next_of(acc) { Some(it) => Flow::Drain(it), None => Flow::Stop(cx.new_list(core::mem::take(&mut acc.items))?) }),
    }
}

fn every(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::truth(false) }.into()) }

fn words(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::words(words_fold, Acc::new()) }.into()) }

// ints the engine added come in `n` and other numbers go to the float total
fn words_fold(acc: &mut Acc, _: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) if v.is_float() => { acc.f.0 += v.as_float(); Ok(Flow::More) }
        Item::Next(v) if v.is_int() => { acc.f.0 += v.as_int() as f64; Ok(Flow::More) }
        Item::Next(_) => Err(VmErr::Type("words takes numbers")),
        Item::End(_) => Ok(Flow::Stop(if acc.f.0 == 0.0 { Val::int(acc.n) } else { Val::float(acc.f.0 + acc.n as f64) })),
        Item::Called(_) => Ok(Flow::More),
    }
}

fn most(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Drain { it: a.pos[0], fold: Fold::extreme(false, most_fold, Acc::new()) }.into()) }

// the pairs the engine left to compare go through the slow path of `<`
fn most_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => { if acc.v[0].is_undef() || cx.lt(acc.v[0], v)? { acc.v[0] = v; } Ok(Flow::More) }
        Item::End(_) if acc.v[0].is_undef() => Err(VmErr::Value("most of nothing")),
        Item::End(_) => Ok(Flow::Stop(acc.v[0])),
        Item::Called(_) => Ok(Flow::More),
    }
}

fn resume(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Resume { g: a.pos[0], sent: Sent::Value(a.pos[1]) }.into()) }

fn shut(_: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Step::Resume { g: a.pos[0], sent: Sent::Close }.into()) }

fn wait(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
    match cx.take_event() { Some(v) => Ok(v.into()), None => Ok(Step::Wait.into()) }
}

/* what `src` printed or its error with one event pushed at each wait */
fn run(src: &str, events: &[&str]) -> Result<String, String> {
    let (chunk, errors) = edge::code::compile_as::<Mini>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let mut events = events.iter();
    loop {
        match vm.run() {
            Ok(_) => return Ok(vm.output_text()),
            Err(VmErr::HostYield(SchedulerStatus::PendingEvent)) => match events.next() {
                Some(e) => { vm.push_event(e).map_err(|e| e.to_string())?; }
                None => return Err("waited past its events".into()),
            },
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn prints(src: &str, want: &str) {
    match run(src, &[]) {
        Ok(out) => assert_eq!(out, want, "{src}"),
        Err(e) => panic!("{src}: {e}"),
    }
}

#[test]
fn a_call_runs_as_a_frame_and_gives_its_value() {
    prints("print(apply(lambda x: x * 2, 21))", "42\n");
    prints("fn f(x):\n    return x + 1\nprint(apply_inc(f, 1))", "3\n");
    prints("print(twice(lambda x: x * 3, 2))", "18\n");
    prints("class K:\n    fn __init__(self, v):\n        self.v = v\nprint(apply(K, 7).v)", "7\n");
    prints("print(apply(len, [1, 2]), apply(str, 5))", "2 5\n");
}

#[test]
fn a_call_in_a_hot_loop_stays_a_frame() {
    prints("t = 0\nfor i in range(3000):\n    t += apply(lambda x: x + 1, i)\nprint(t)", "4501500\n");
    prints("fn h(i):\n    return apply_inc(lambda x: x * 2, i)\nt = 0\nfor i in range(3000):\n    t += h(i)\nprint(t)", "9000000\n");
}

#[test]
fn an_error_in_a_call_rises_from_the_native() {
    let src = "fn bad(x):\n    raise ValueError('no ' + str(x))\ntry:\n    apply(bad, 1)\nexcept ValueError as e:\n    print('caught', e)";
    prints(src, "caught no 1\n");
}

#[test]
fn a_drain_folds_containers_generators_and_lazy_iterators() {
    prints("print(total([1, 2, 3]), total(range(5)), total({1: 0, 2: 0}))", "6 10 3\n");
    prints("print(total(x * x for x in range(4)))", "14\n");
    prints("print(total(map(lambda x: x + 1, [1, 2, 3])))", "9\n");
    prints("print(total(filter(lambda x: x % 2, range(10))))", "25\n");
    prints("print(total(iter([4, 5])))", "9\n");
    let out = run("print(total(['a']))", &[]);
    assert!(out.as_ref().is_err_and(|e| e.contains("total takes ints")), "{out:?}");
}

#[test]
fn a_drain_stops_early_and_keeps_its_generator() {
    prints("fn g():\n    yield 1\n    yield 2\n    return 9\nit = g()\nprint(first(it), first(it))\ntry:\n    first(it)\nexcept StopIteration as e:\n    print('stop', e.value)", "1 2\nstop 9\n");
    prints("it = iter([7, 8])\nprint(first(it), list(it))", "7 [8]\n");
}

#[test]
fn a_drain_calls_between_items() {
    prints("print(keys([3, 1, 2], lambda v: -v))", "[-3, -1, -2]\n");
    prints("print(keys((i for i in range(4)), lambda v: v * 10))", "[0, 10, 20, 30]\n");
    prints("print(keys(map(lambda x: x + 1, [1, 2]), lambda v: v * 2))", "[4, 6]\n");
    prints("print(keys([1, 2], str), keys(['a'], len))", "['1', '2'] [1]\n");
    prints("class K:\n    fn __init__(self, v):\n        self.v = v\nprint([k.v for k in keys([1, 2], K)])", "[1, 2]\n");
    prints("n = keys(list(range(2000)), lambda v: v + 1)\nprint(len(n), n[0], n[-1])", "2000 1 2000\n");
    let src = "fn bad(v):\n    if v == 2:\n        raise KeyError(v)\n    return v\ntry:\n    keys([1, 2, 3], bad)\nexcept KeyError as e:\n    print('caught', e)";
    prints(src, "caught 2\n");
}

#[test]
fn a_drain_walks_on_to_the_iterables_its_end_names() {
    prints("print(sums([1, 2], range(3), (4,)))", "10\n");
    prints("print(sums((i for i in range(3)), (i * 10 for i in range(3))))", "33\n");
    prints("fn g(n):\n    yield n\nprint(sums(g(1), [2, 3], g(4), range(2)))", "11\n");
    prints("print(sums(map(lambda x: x + 1, [1, 2]), (i for i in range(2))))", "6\n");
    prints("print(sums(iter([]), (i for i in []), [7]))", "7\n");
    prints("print(lists((i for i in range(2)), [5], (x for x in 'ab')))", "[[0, 1], [5], ['a', 'b']]\n");
    prints("fn g(n):\n    yield n\n    yield n + 1\nt = 0\nfor i in range(300):\n    t += sums(g(i), [i], g(0))\nprint(t)", "135150\n");
    let src = "fn bad():\n    yield 1\n    raise ValueError('late')\ntry:\n    sums([1], bad())\nexcept ValueError as e:\n    print('caught', e)";
    prints(src, "caught late\n");
}

#[test]
fn a_collect_keeps_what_each_iterable_gave_across_its_chain() {
    let g = "fn g(n):\n    for i in range(n):\n        yield i\n";
    prints(&(g.to_string() + "print(joins([7], g(2), (8,), g(1)))"), "[7, 0, 1, 8, 0]\n");
    prints(&(g.to_string() + "print(joins(g(3), [5], g(0), 'ab'))"), "[0, 1, 2, 5, 'a', 'b']\n");
    prints(&(g.to_string() + "t = 0\nfor i in range(300):\n    t += len(joins([i], g(2), g(3)))\nprint(t)"), "1800\n");
}

#[test]
fn a_drain_past_a_call_opens_the_next_iterables_frames_over_its_caller() {
    prints("print(chained(lambda v: v * 10, [1, 2], [3, 4]))", "[10, 20, 30, 40]\n");
    prints("print(chained(lambda v: v * 10, [1], (i for i in [2, 3]), range(4, 5)))", "[10, 20, 30, 40]\n");
    prints("fn k(v):\n    return v + 1\nout = chained(k, list(range(50)), [100, 200])\nprint(len(out), out[0], out[49], out[50], out[51])", "52 1 50 101 201\n");
    prints("t = 0\nfor i in range(200):\n    t += sum(chained(lambda v: v * 2, [i, i], (i,)))\nprint(t)", "119400\n");
    let src = "fn bad(v):\n    if v == 3:\n        raise KeyError(v)\n    return v\ntry:\n    chained(bad, [1, 2], [3])\nexcept KeyError as e:\n    print('caught', e)";
    prints(src, "caught 3\n");
}

#[test]
fn the_inline_folds_take_items_as_the_engine_folds_do() {
    prints("print(every(range(1, 9)), every(range(0, 9)), every([]), every(i for i in [1, 0]), every(x for x in 'ab'))", "True False True False True\n");
    prints("print(words(range(5)), words(i for i in range(5)), words([1, 2.5, 3]), words(x / 2 for x in range(3)))", "10 10 6.5 1.5\n");
    prints("print(words([2 ** 40, 2 ** 40, 1]), lists(range(3)), lists(i * i for i in range(3)))", "2199023255553 [[0, 1, 2]] [[0, 1, 4]]\n");
    prints("print(most([3, 9, 2]), most(range(5)), most(x * 1.5 for x in range(4)), most(['b', 'c', 'a']), most([1, 2.5, 2]), most(i % 7 for i in range(20)))", "9 4 4.5 c 2.5 6\n");
    prints("print(most((1, 'x')) if false else most([(1, 2), (1, 3)]), every([0.0, 1]), words(range(2 ** 40, 2 ** 40 + 3)))", "(1, 3) False 3298534883331\n");
    let out = run("most([])", &[]);
    assert!(out.as_ref().is_err_and(|e| e.contains("most of nothing")), "{out:?}");
}

#[test]
fn a_resume_sends_throws_and_closes() {
    prints("fn g():\n    x = yield 1\n    yield x * 2\nit = g()\nprint(next(it))\nprint(resume(it, 5))\nprint(shut(it))", "1\n10\nNone\n");
    prints("fn g():\n    yield 1\nit = g()\nnext(it)\ntry:\n    resume(it, 0)\nexcept StopIteration:\n    print('done')", "done\n");
}

#[test]
fn a_wait_parks_until_the_host_pushes_an_event() {
    assert_eq!(run("x = wait()\nprint('got', x)", &["hi"]), Ok("got hi\n".into()));
    assert_eq!(run("fn f():\n    return wait() + '!'\nprint(f(), f())", &["a", "b"]), Ok("a! b!\n".into()));
}

#[test]
fn list_extend_gathers_a_generator_before_it_extends() {
    prints("xs = [0]\nxs.extend(i for i in range(3))\nprint(xs)", "[0, 0, 1, 2]\n");
    prints("xs = []\nxs.extend(map(lambda v: v * 2, [1, 2]))\nprint(xs)", "[2, 4]\n");
    prints("fn g():\n    yield 1\n    raise ValueError('half')\nxs = []\ntry:\n    xs.extend(g())\nexcept ValueError:\n    print(xs)", "[]\n");
}

#[test]
fn a_call_that_misses_a_signature_reads_in_the_runtimes_words() {
    let out = run("apply(1)", &[]);
    assert!(out.as_ref().is_err_and(|e| e.contains("apply expected 2 arguments, got 1")), "{out:?}");
    let out = run("apply(f=1, x=2)", &[]);
    assert!(out.as_ref().is_err_and(|e| e.contains("apply() takes no keyword arguments")), "{out:?}");
    prints("print('a', 'b', sep='-', end='.\\n')", "a-b.\n");
}

#[test]
fn a_native_value_survives_a_snapshot() {
    let src = "xs = [1]\npush = xs.append\nlen_of = apply\nwait()\npush(2)\nprint(xs, len_of(len, xs), push)";
    let (chunk, _) = edge::code::compile_as::<Mini>(src);
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
    let blob = edge::embed::save(&vm, src, "").unwrap_or_else(|e| panic!("{e}"));
    let mut back = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    edge::embed::restore(&mut back, &blob).unwrap_or_else(|e| panic!("{e}"));
    back.push_event("go").unwrap_or_else(|e| panic!("{e}"));
    back.run().unwrap_or_else(|e| panic!("{e}"));
    assert!(back.output_text().starts_with("[1, 2] 2 <built-in method append of list object at"), "{}", back.output_text());
    // another runtime refuses the blob
    let mut bare = VM::with_limits(&chunk, Limits::sandbox());
    assert!(edge::embed::restore(&mut bare, &blob).is_err_and(|e| e.contains("another runtime")));
}
