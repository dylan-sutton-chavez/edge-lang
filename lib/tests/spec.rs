use edge::embed::{Limits, VM};
use edge::lang::{Args, Cx, Native, Natives, Ret, Runtime, Sig};
use edge::value::VmErr;
use mini::Mini;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

const TESTED: &[Native] = &[Native::new("fmt", Sig::exact(2), fmt)];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, TESTED],
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

/* a value's text under a spec or the error the spec raises */
fn fmt(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let spec = cx.text(a.pos[1]).map(String::from).ok_or(VmErr::Type("fmt takes a str spec"))?;
    let out = lib::fmt::spec::format::<Mini>(cx, a.pos[0], &spec)?;
    Ok(cx.new_str(out)?.into())
}

/* what `src` printed or its error */
fn run(src: &str) -> Result<String, String> {
    let (chunk, errors) = edge::code::compile_as::<mini::Mini>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    // nothing meters these tests so the instance budget bounds the room a spec checks
    edge::value::meter::set_budget(256 << 20);
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits { memory: usize::MAX, ..Limits::sandbox() });
    vm.run().map_err(|e| e.to_string())?;
    Ok(vm.output_text())
}

fn prints(src: &str, want: &str) {
    match run(src) {
        Ok(out) => assert_eq!(out, want, "{src}"),
        Err(e) => panic!("{src}: {e}"),
    }
}

#[test]
fn a_spec_reads_as_the_corpus_expects() {
    prints("print(fmt(3.14159, '.2f'), fmt(42, '05d'), fmt(42, '>6'), fmt(255, '#x'), fmt(-7, '=+8'))", "3.14 00042     42 0xff -      7\n");
    prints("print(fmt(1234567, ','), fmt(0.25, '.1%'), fmt('ab', '^6') + '|', fmt(-0.0, 'z.1f'), fmt(2**100, '_x'))", "1,234,567 25.0%   ab  | 0.0 10_0000_0000_0000_0000_0000_0000\n");
    prints("print(fmt(1e16, '>8'), fmt(1.5, ''), fmt([1, 'a'], ''), fmt(65, 'c'), fmt(1e-5, 'g'), fmt(12345.678, ',.2f'))", "   1e+16 1.5 [1, 'a'] A 1e-05 12,345.68\n");
}

#[test]
fn a_bad_spec_raises_the_words_the_corpus_expects() {
    let words = |v: &str, spec: &str| match run(&format!("fmt({v}, {spec:?})")) { Err(e) => e, Ok(out) => panic!("{v} {spec} printed {out}") };
    assert!(words("1.5", "q").starts_with("ValueError: Unknown format code 'q' for object of type 'float'"));
    assert!(words("1", ".2fx").starts_with("ValueError: Invalid format specifier '.2fx' for object of type 'int'"));
    assert!(words("[1]", ">3").starts_with("TypeError: unsupported format string passed to list.__format__"));
    assert!(words("ValueError('x')", ">3").starts_with("TypeError: unsupported format string passed to ValueError.__format__"));
    assert!(words("'s'", "+").starts_with("ValueError: Sign not allowed in string format specifier"));
    assert!(words("1", ",_").starts_with("ValueError: Cannot specify both ',' and '_'."));
    assert!(words("1", "300000000d").starts_with("ValueError: format width exceeds limit"));
    assert!(words("1.5", ".75342E").starts_with("ValueError: format precision exceeds limit"));
    assert!(words("1114112", "c").starts_with("OverflowError: %c arg not in range(0x110000)"));
    assert!(words("2**20000", "d").starts_with("ValueError: Exceeds the limit (4300 digits) for integer string conversion"));
}
