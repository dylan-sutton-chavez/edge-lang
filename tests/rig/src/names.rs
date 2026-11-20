use interpreter::lang::{ErrKind, Fields, KindId, Names, Tree};

// the error classes the rig raises each beside its parent with the signals apart
static TREE: Tree = Tree::of(&crate::types::CLASSES);

// what rig calls a value of each engine kind which its wide ints take too
pub(crate) const KIND_NAMES: &[(KindId, &str)] = &[
    (KindId::None, "nil"), (KindId::Bool, "bool"), (KindId::Int, "int"), (KindId::Float, "float"), (KindId::Str, "text"),
    (KindId::Bytes, "bytes"), (KindId::ByteArray, "bytearray"), (KindId::List, "list"), (KindId::Tuple, "tuple"),
    (KindId::Dict, "record"), (KindId::Set, "set"), (KindId::Range, "range"), (KindId::Func, "fn"), (KindId::Coro, "generator"),
    (KindId::Iter, "iterator"), (KindId::Module, "module"), (KindId::Type, "type"), (KindId::Class, "type"),
    (KindId::Instance, "value"), (KindId::Cell, "cell"), (KindId::Other, "value"), (KindId::Foreign, "value"),
];

// the class of each engine error kind which the descriptor takes too
pub(crate) const ERRORS: &[(ErrKind, &str)] = &[
    (ErrKind::Base, "Error"), (ErrKind::Error, "Error"), (ErrKind::Type, "TypeError"), (ErrKind::Value, "ValueError"),
    (ErrKind::Name, "NameError"), (ErrKind::Unbound, "NameError"), (ErrKind::Attribute, "FieldError"), (ErrKind::Key, "KeyError"),
    (ErrKind::Index, "IndexError"), (ErrKind::ZeroDiv, "DivisionError"), (ErrKind::Overflow, "OverflowError"),
    (ErrKind::Recursion, "RecursionError"), (ErrKind::Memory, "MemoryError"), (ErrKind::Runtime, "Error"), (ErrKind::Stop, "Stop"),
    (ErrKind::Close, "Close"), (ErrKind::Exit, "Exit"), (ErrKind::Assert, "CheckError"), (ErrKind::Syntax, "SyntaxError"),
    (ErrKind::Permission, "PermissionError"),
];

// every builtin iterator reads as one type
const ITERS: [&str; 19] = ["iterator"; 19];

// the rig's words for the engine's kinds and errors with the rest the engine's own
pub static NAMES: Names = Names::PLAIN
    .kinds(KIND_NAMES)
    .errors(ERRORS)
    .iters(&ITERS)
    .coroutine("generator")
    .descriptor("fn")
    .native("fn")
    .method("fn")
    .exception("error")
    .fields(Fields::PLAIN.receiver("receiver").function("function").args("args").value("value").start("start").stop("stop").step("step"))
    .anonymous("<fn>")
    .tree(&TREE);
