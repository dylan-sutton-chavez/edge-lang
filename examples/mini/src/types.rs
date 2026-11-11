use lib::lang::Tree;

use crate::Mini;

// the builtin types besides the errors
const PLAIN: [&str; 12] = ["int", "float", "text", "bytes", "bytearray", "bool", "list", "tuple", "record", "set", "range", "type"];

// each error class beside its parent where the signals have no parent
pub(crate) const CLASSES: [(&str, &str); 18] = [
    ("Error", ""), ("Stop", ""), ("Close", ""), ("Exit", ""),
    ("TypeError", "Error"), ("ValueError", "Error"), ("NameError", "Error"), ("FieldError", "Error"), ("KeyError", "Error"),
    ("IndexError", "Error"), ("DivisionError", "Error"), ("OverflowError", "Error"), ("RecursionError", "Error"),
    ("MemoryError", "Error"), ("CheckError", "Error"), ("SyntaxError", "Error"), ("IOError", "Error"), ("PermissionError", "IOError"),
];

// the classes the type tests walk as mini's names give them
pub static TREE: Tree = Tree::of::<Mini>();

// the names a load gives as type objects
pub const NAMES: &[&str] = &names::<{ PLAIN.len() + CLASSES.len() }>();

/* the plain types then every class of the tree */
const fn names<const N: usize>() -> [&'static str; N] {
    let mut out = [""; N];
    let mut i = 0;
    while i < PLAIN.len() { out[i] = PLAIN[i]; i += 1; }
    let mut j = 0;
    while j < CLASSES.len() { out[i + j] = CLASSES[j].0; j += 1; }
    out
}
