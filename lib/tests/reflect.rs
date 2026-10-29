use edge::embed::{Limits, VM};
use edge::lang::{Names, Natives, Runtime};
use edge::lexer::{Spelling, Syntax};
use mini::natives::{ATTR, CONVERT, IO};

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// mini's names with the constructor spelled as the engine's plain names spell it
static NAMES: Names = Names { constructor: "init", ..mini::NAMES };

static NATIVES: Natives = edge::natives! {
    language: Init,
    fns: [IO, CONVERT, ATTR],
    methods: [],
    types: mini::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Init>();

// mini's language over these natives naming its constructor init
struct Init;
mini::over!(Init, &NATIVES, &NAMES);

/* mini's spelling over these natives so the calls fuse to this table */
struct Plain;

impl Spelling for Plain {
    const SYNTAX: Syntax = mini::MINI;
    const NATIVES: &'static Natives = &NATIVES;
}

fn run(src: &str) -> String {
    let (chunk, errors) = edge::code::compile_as::<Plain>(src);
    assert!(errors.is_empty(), "{src}: {:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let ran = vm.run();
    let mut out = vm.output_text();
    if let Err(e) = ran { out.push_str(&e.to_string()); }
    out
}

// the constructor's name comes from the runtime so a direct and an indirect test agree
#[test]
fn the_constructor_reads_alike_by_name_and_through_a_value() {
    let src = "class K:\n    pass\nk = K()\nf = hasattr\nprint(hasattr(k, 'init'), f(k, 'init'), callable(getattr(k, 'init')), hasattr(k, '__init__'))\n";
    assert_eq!(run(src), "True True True False\n");
}
