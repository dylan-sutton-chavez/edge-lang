use edge::lang::{self, Fields, Names, Tree};

// the exception classes mini raises each beside its parent
static TREE: Tree = Tree::of(&crate::types::CLASSES);

// what mini calls a value of each engine kind which its wide ints take too
pub(crate) const KIND_NAMES: [&str; lang::KINDS] = [
    "NoneType", "bool", "int", "float", "str", "bytes", "bytearray", "list", "tuple", "dict", "set", "range",
    "function", "generator", "iterator", "module", "type", "type", "object", "cell", "object", "object",
];

// the class of each engine error kind which the descriptor names for the CLI too
pub(crate) const ERRORS: [&str; lang::ERRORS] = [
    "BaseException", "Exception", "TypeError", "ValueError", "NameError", "UnboundLocalError", "AttributeError", "KeyError",
    "IndexError", "ZeroDivisionError", "OverflowError", "RecursionError", "MemoryError", "RuntimeError", "StopIteration",
    "GeneratorExit", "SystemExit", "AssertionError", "SyntaxError", "PermissionError",
];

// mini's words for the engine's kinds errors and conventions
pub static NAMES: Names = Names {
    kinds: KIND_NAMES,
    errors: ERRORS,
    iters: [
        "list_iterator", "range_iterator", "tuple_iterator", "str_ascii_iterator", "str_iterator", "dict_keyiterator", "set_iterator",
        "bytes_iterator", "callable_iterator", "map", "filter", "zip", "enumerate", "list_reverseiterator", "reversed",
        "dict_reversekeyiterator", "bytearray_iterator", "dict_valueiterator", "dict_itemiterator",
    ],
    coroutine: "coroutine",
    descriptor: "method_descriptor",
    native: "builtin_function_or_method",
    method: "method",
    exception: "exception",
    ellipsis_type: "ellipsis",
    ellipsis: "Ellipsis",
    constructor: "__init__",
    name_field: "__name__",
    fields: Fields {
        receiver: "__self__",
        function: "__func__",
        match_args: "__match_args__",
        args: "args",
        value: "value",
        start: "start",
        stop: "stop",
        step: "step",
    },
    module_name: "__name__",
    entry: "__main__",
    doc: "__doc__",
    anonymous: "<lambda>",
    locals: "<locals>",
    module_body: "<module>",
    comprehensions: ["<genexpr>", "<listcomp>", "<dictcomp>", "<setcomp>"],
    compiled: "<string>",
    unnamed: "<anonymous>",
    overflow: "Python int too large to convert to C ssize_t",
    nil: "None",
    index_method: "__index__",
    send: "send",
    throw: "throw",
    close: "close",
    tree: &TREE,
};
