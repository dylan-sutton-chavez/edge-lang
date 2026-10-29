use edge::lang::{self as kit, Cx, Fields, Names};
use edge::value::VmErr;
use edge::value::foreign::Kind;
use lib::Lang;
use lib::fail::{At, Class, Fail};
use lib::fmt::render::Style;
use lib::lang::Tree;

/* the runtime whose choices tiny's kernels follow */
pub struct Tiny;

// the kinds tiny boxes its times and durations in
pub static TIME: Kind = lib::date::time_kind::<Tiny>("time");
// the kind of the one value the ellipsis builtin names
pub static DOTS: Kind = lib::types::dots::<Tiny>();
// the kind of the slice a key of bounds makes
pub static SLICE: Kind = lib::seq::slice::<Tiny>("slice");
pub static DURATION: Kind = lib::date::duration_kind::<Tiny>("duration");

impl lib::date::Dates for Tiny {
    const TIME: &'static Kind = &TIME;
    const DURATION: &'static Kind = &DURATION;
}

// the type names a load gives as type objects of which tiny has none
pub const TYPES: &[&str] = &[];

// the classes tiny raises with the signals that end loops and generators
const RAISED: [(&str, &str); 10] = [
    ("Error", ""), ("Denied", "Error"), ("BadData", "Error"), ("WrongType", "Error"), ("HostFailed", "Error"),
    ("OutOfBudget", "Error"), ("MissingField", "Error"), ("Stop", "Error"), ("Close", "Error"), ("Exit", "Error"),
];

static RAISED_TREE: kit::Tree = kit::Tree::of(&RAISED);

// the same classes for lib's type tests
pub static TREE: Tree = Tree::of::<Tiny>();

// tiny's words for the engine's kinds and errors with unspellable names keeping fields free
pub static NAMES: Names = Names {
    kinds: [
        "None", "bool", "number", "number", "text", "bytes", "bytes", "list", "list", "record", "set", "range",
        "function", "generator", "iterator", "module", "type", "type", "value", "cell", "value", "value",
    ],
    errors: [
        "Error", "Error", "WrongType", "BadData", "MissingField", "MissingField", "MissingField", "MissingField", "MissingField", "BadData",
        "BadData", "OutOfBudget", "OutOfBudget", "Error", "Stop", "Close", "Exit", "Error", "BadData", "Denied",
    ],
    iters: ["iterator"; kit::ITERS],
    coroutine: "generator",
    descriptor: "method",
    native: "function",
    method: "method",
    exception: "error",
    ellipsis_type: "dots",
    ellipsis: "@dots",
    constructor: "@new",
    name_field: "@name",
    fields: Fields {
        receiver: "@receiver", function: "@function", match_args: "@match", args: "@arguments", value: "@value",
        start: "@start", stop: "@stop", step: "@step",
    },
    module_name: "@module",
    entry: "main",
    doc: "@doc",
    anonymous: "fn",
    locals: "<scope>",
    module_body: "<flow>",
    comprehensions: ["<each>"; 4],
    compiled: "<compiled>",
    unnamed: "<anonymous>",
    overflow: "number too large for an index",
    nil: "None",
    index_method: "@index",
    send: "@send",
    throw: "@throw",
    close: "@close",
    tree: &RAISED_TREE,
};

// tiny shows the two truths as its expressions spell them
impl Style for Tiny {
    const TRUE: &'static str = "True";
    const FALSE: &'static str = "False";
}

impl Lang for Tiny {
    const TREE: &'static Tree = &TREE;
    // the lowering steps a source through calls until one gives the sentinel
    const SENTINEL: bool = true;

    // runtime failures are the host's and exits bad data with the rest by kind
    fn class(c: Class) -> &'static str {
        match c {
            Class::Runtime => "HostFailed",
            Class::Exit => "BadData",
            c => lib::lang::class::<Tiny>(c),
        }
    }

    fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr { crate::words::words(cx, at, f) }
}
