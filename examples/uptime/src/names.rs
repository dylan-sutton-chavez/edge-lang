use edge::lang::{self, Cx, Names};
use edge::value::VmErr;
use lib::fail::{english, raise, At, Class, Fail};
use lib::lang::Tree;

use crate::Uptime;

// the classes a check raises shortest first where the three signals stand apart from Error
const CLASSES: [(&str, &str); 11] = [
    ("Exit", ""), ("Stop", ""), ("Close", ""), ("Error", ""), ("Denied", "Error"), ("BadData", "Error"), ("Missing", "Error"),
    ("WrongType", "Error"), ("CheckFailed", "Error"), ("OutOfBudget", "Error"), ("Unreachable", "Error"),
];

// the classes as names a check can catch
pub const TYPES: [&str; CLASSES.len()] = names(&CLASSES);

const fn names<const N: usize>(classes: &[(&'static str, &'static str); N]) -> [&'static str; N] {
    let mut out = [""; N];
    let mut i = 0;
    while i < N { out[i] = classes[i].0; i += 1; }
    out
}

static RAISED: lang::Tree = lang::Tree::of(&CLASSES);

// the same classes for lib's type tests
static TREE: Tree = Tree::of::<Uptime>();

// a check's words for the engine's kinds errors and iterators with the rest left plain
pub static NAMES: Names = Names {
    kinds: [
        "nil", "bool", "int", "float", "text", "bytes", "bytes", "list", "tuple", "map", "set", "range",
        "fn", "generator", "iterator", "module", "type", "type", "value", "cell", "value", "value",
    ],
    errors: [
        "Error", "Error", "WrongType", "BadData", "Missing", "Missing", "Missing", "Missing", "Missing", "BadData",
        "BadData", "OutOfBudget", "OutOfBudget", "Error", "Stop", "Close", "Exit", "CheckFailed", "BadData", "Denied",
    ],
    iters: ["iterator"; lang::ITERS],
    tree: &RAISED,
    ..Names::PLAIN
};

impl lib::fmt::render::Style for Uptime {}

// every failure raises the class its error kind names in `NAMES`
impl lib::Lang for Uptime {
    const TREE: &'static Tree = &TREE;

    // a body that is not JSON reads as such and lib words the rest
    fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr {
        match f {
            Fail::BadJson { at, why } => raise::<Uptime>(Class::Value, &alloc::format!("the answer is not JSON, {why} at byte {at}")),
            f => english::<Uptime>(cx, at, f),
        }
    }
}
