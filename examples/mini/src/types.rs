use lib::lang::Tree;

use crate::Mini;

// the builtin types besides the exceptions
const PLAIN: [&str; 14] = ["int", "float", "str", "bytes", "bytearray", "bool", "list", "tuple", "dict", "set", "range", "type", "NoneType", "object"];

// each builtin exception beside its parent with the root's parent empty
pub(crate) const CLASSES: [(&str, &str); 31] = [
    ("OSError", "Exception"),
    ("EOFError", "Exception"), ("KeyError", "LookupError"),
    ("Exception", "BaseException"), ("NameError", "Exception"), ("TypeError", "Exception"),
    ("IndexError", "LookupError"), ("SystemExit", "BaseException"), ("ValueError", "Exception"),
    ("ImportError", "Exception"), ("LookupError", "Exception"), ("MemoryError", "Exception"), ("SyntaxError", "Exception"),
    ("RuntimeError", "Exception"), ("TimeoutError", "OSError"), ("UnicodeError", "ValueError"),
    ("BaseException", ""), ("GeneratorExit", "BaseException"), ("OverflowError", "ArithmeticError"), ("StopIteration", "Exception"),
    ("AssertionError", "Exception"), ("AttributeError", "Exception"), ("RecursionError", "RuntimeError"),
    ("ArithmeticError", "Exception"), ("PermissionError", "OSError"),
    ("UnboundLocalError", "NameError"), ("ZeroDivisionError", "ArithmeticError"),
    ("UnicodeDecodeError", "UnicodeError"), ("UnicodeEncodeError", "UnicodeError"),
    ("ModuleNotFoundError", "ImportError"), ("NotImplementedError", "RuntimeError"),
];

// the classes the type tests walk as mini's names give them
pub static TREE: Tree = Tree::of::<Mini>();

// the names a load gives as type objects
pub const NAMES: &[&str] = &names::<{ PLAIN.len() + CLASSES.len() }>();

/* the plain types then every exception of the tree */
const fn names<const N: usize>() -> [&'static str; N] {
    let mut out = [""; N];
    let mut i = 0;
    while i < PLAIN.len() { out[i] = PLAIN[i]; i += 1; }
    let mut j = 0;
    while j < CLASSES.len() { out[i + j] = CLASSES[j].0; j += 1; }
    out
}
