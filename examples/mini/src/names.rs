use edge::lang::{self, Fields, Names, Tree};

// the error classes mini raises each beside its parent where the signals stand apart from Error
static TREE: Tree = Tree::of(&crate::types::CLASSES);

// what mini calls a value of each engine kind which its wide ints take too
pub(crate) const KIND_NAMES: [&str; lang::KINDS] = [
    "nil", "bool", "int", "float", "text", "bytes", "bytearray", "list", "tuple", "record", "set", "range",
    "fn", "generator", "iterator", "module", "type", "type", "value", "cell", "value", "value",
];

// the class of each engine error kind which the descriptor names for the CLI too
pub(crate) const ERRORS: [&str; lang::ERRORS] = [
    "Error", "Error", "TypeError", "ValueError", "NameError", "NameError", "FieldError", "KeyError",
    "IndexError", "DivisionError", "OverflowError", "RecursionError", "MemoryError", "Error", "Stop",
    "Close", "Exit", "CheckError", "SyntaxError", "PermissionError",
];

// mini's words for the engine's kinds and errors with the rest the engine's own
pub static NAMES: Names = Names {
    kinds: KIND_NAMES,
    errors: ERRORS,
    iters: ["iterator"; lang::ITERS],
    coroutine: "generator", descriptor: "fn", native: "fn", method: "fn", exception: "error",
    fields: Fields { receiver: "receiver", function: "function", match_args: "match_fields", args: "args", value: "value", start: "start", stop: "stop", step: "step" },
    anonymous: "<fn>",
    tree: &TREE,
    ..Names::PLAIN
};
