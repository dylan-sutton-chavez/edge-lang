use edge::embed::{Limits, VM};
use edge::lang::{Args, Cx, Native, Natives, Ret, Runtime, Sig};
use edge::value::VmErr;
use rig::Rig;
use rig::natives::{ARRAY, BUFFER, BYTES, CONVERT, DICT, FLOAT, FOLDS, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};

const TESTED: &[Native] = &[Native::new("fmt", Sig::exact(2), fmt)];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, TYPES, rig::natives::MATCH, rig::natives::DATE, TESTED],
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
    ],
    types: rig::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Over>();

// the rig's language over these natives
struct Over;
rig::over!(Over, &NATIVES);

/* a value's text under a spec or the error the spec raises */
fn fmt(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let spec = cx.text(a.pos[1]).map(String::from).ok_or(VmErr::Type("fmt takes a str spec"))?;
    let out = lib::fmt::spec::format::<Rig>(cx, a.pos[0], &spec)?;
    Ok(cx.new_str(out)?.into())
}

/* what `src` printed or its error */
fn run(src: &str) -> Result<String, String> {
    let (chunk, errors) = edge::code::compile_as::<rig::Rig>(src);
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
    prints("print(fmt(3.14159, \".2f\"), fmt(42, \"05d\"), fmt(42, \">6\"), fmt(255, \"#x\"), fmt(-7, \"=+8\"))", "3.14 00042     42 0xff -      7\n");
    prints("print(fmt(1234567, \",\"), fmt(0.25, \".1%\"), fmt(\"ab\", \"^6\") + \"|\", fmt(-0.0, \"z.1f\"), fmt(2 ** 100, \"_x\"))", "1,234,567 25.0%   ab  | 0.0 10_0000_0000_0000_0000_0000_0000\n");
    prints("print(fmt(1.0e+16, \">8\"), fmt(1.5, \"\"), fmt([1, \"a\"], \"\"), fmt(65, \"c\"), fmt(1.0e-05, \"g\"), fmt(12345.678, \",.2f\"))", "    1e16 1.5 [1, \"a\"] A 1e-05 12,345.68\n");
}

#[test]
fn a_bad_spec_raises_the_words_the_corpus_expects() {
    let words = |v: &str, spec: &str| match run(&format!("fmt({v}, {spec:?})")) { Err(e) => e, Ok(out) => panic!("{v} {spec} printed {out}") };
    assert!(words("1.5", "q").starts_with("ValueError: the format code q does not apply to float"));
    assert!(words("1", ".2fx").starts_with("ValueError: invalid format spec .2fx for int"));
    assert!(words("[1]", ">3").starts_with("TypeError: list takes no format spec"));
    assert!(words("ValueError(\"x\")", ">3").starts_with("TypeError: ValueError takes no format spec"));
    assert!(words("\"s\"", "+").starts_with("ValueError: a sign does not apply to text"));
    assert!(words("1", ",_").starts_with("ValueError: a format spec takes one separator"));
    assert!(words("1", "300000000d").starts_with("ValueError: the format width passes the limit"));
    assert!(words("1.5", ".75342E").starts_with("ValueError: the format precision passes the limit"));
    assert!(words("1114112", "c").starts_with("OverflowError: the c type takes a code point from 0 to 0x10FFFF"));
    assert!(words("2**40000", "d").starts_with("ValueError: the number has more digits than the limit of 10000 for its text"));
}
