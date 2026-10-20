pub mod async_ops;
pub mod attr;
pub mod bytes_helpers;
pub mod compile;
pub mod container;
pub mod conversion;
pub mod identity;
pub mod io;
pub mod numeric;
pub mod sequence;

/* Parent of a built-in exception type, walked by `matches_exc_class`, matched by length first so a lookup compares few names. `ExceptionGroup` also derives from `BaseExceptionGroup`, which `exc_also` gives. */
fn exc_parent(name: &str) -> Option<&'static str> {
    Some(match name.len() {
        7 => match name { "OSError" => "Exception", "Warning" => "Exception", _ => return None },
        8 => match name { "EOFError" => "Exception", "KeyError" => "LookupError", "TabError" => "IndentationError", _ => return None },
        9 => match name { "Exception" => "BaseException", "NameError" => "Exception", "TypeError" => "Exception", _ => return None },
        10 => match name { "IndexError" => "LookupError", "SystemExit" => "BaseException", "ValueError" => "Exception", _ => return None },
        11 => match name { "BufferError" => "Exception", "ImportError" => "Exception", "LookupError" => "Exception", "MemoryError" => "Exception", "SyntaxError" => "Exception", "SystemError" => "Exception", "UserWarning" => "Warning", _ => return None },
        12 => match name { "BytesWarning" => "Warning", "RuntimeError" => "Exception", "TimeoutError" => "OSError", "UnicodeError" => "ValueError", _ => return None },
        13 => match name { "FutureWarning" => "Warning", "GeneratorExit" => "BaseException", "ImportWarning" => "Warning", "OverflowError" => "ArithmeticError", "StopIteration" => "Exception", "SyntaxWarning" => "Warning", _ => return None },
        14 => match name { "AssertionError" => "Exception", "AttributeError" => "Exception", "ExceptionGroup" => "Exception", "RecursionError" => "RuntimeError", "ReferenceError" => "Exception", "RuntimeWarning" => "Warning", "UnicodeWarning" => "Warning", _ => return None },
        15 => match name { "ArithmeticError" => "Exception", "BlockingIOError" => "OSError", "BrokenPipeError" => "ConnectionError", "ConnectionError" => "OSError", "EncodingWarning" => "Warning", "FileExistsError" => "OSError", "PermissionError" => "OSError", "ResourceWarning" => "Warning", _ => return None },
        16 => match name { "IndentationError" => "SyntaxError", "InterruptedError" => "OSError", _ => return None },
        17 => match name { "ChildProcessError" => "OSError", "FileNotFoundError" => "OSError", "IsADirectoryError" => "OSError", "KeyboardInterrupt" => "BaseException", "UnboundLocalError" => "NameError", "ZeroDivisionError" => "ArithmeticError", _ => return None },
        18 => match name { "BaseExceptionGroup" => "BaseException", "DeprecationWarning" => "Warning", "FloatingPointError" => "ArithmeticError", "NotADirectoryError" => "OSError", "ProcessLookupError" => "OSError", "StopAsyncIteration" => "Exception", "UnicodeDecodeError" => "UnicodeError", "UnicodeEncodeError" => "UnicodeError", _ => return None },
        19 => match name { "ModuleNotFoundError" => "ImportError", "NotImplementedError" => "RuntimeError", _ => return None },
        20 => match name { "ConnectionResetError" => "ConnectionError", _ => return None },
        21 => match name { "UnicodeTranslateError" => "UnicodeError", _ => return None },
        22 => match name { "ConnectionAbortedError" => "ConnectionError", "ConnectionRefusedError" => "ConnectionError", _ => return None },
        23 => match name { "PythonFinalizationError" => "RuntimeError", _ => return None },
        25 => match name { "PendingDeprecationWarning" => "Warning", _ => return None },
        _ => return None,
    })
}

/* Whether `name` is a builtin exception type, every one but the root having a parent in the tree. */
#[inline]
pub(in crate::vm) fn is_exc_type(name: &str) -> bool { name == "BaseException" || exc_parent(name).is_some() }

/* Whether builtin exception `name` is a group, the two group types having no builtin subclass. */
#[inline]
pub(crate) fn is_group_type(name: &str) -> bool { matches!(name, "ExceptionGroup" | "BaseExceptionGroup") }

/* The second parent of a builtin exception with two, `ExceptionGroup` under `BaseExceptionGroup`. */
fn exc_also(name: &str) -> Option<&'static str> {
    (name == "ExceptionGroup").then_some("BaseExceptionGroup")
}

/* The parents of builtin exception `name`, two for `ExceptionGroup` and none for `BaseException` or a name outside the tree. */
pub fn exc_parents(name: &str) -> alloc::vec::Vec<&'static str> {
    exc_also(name).into_iter().chain(exc_parent(name)).collect()
}

/* The bases of a builtin type in order, the exception tree plus `bool` under `int`. */
pub(in crate::vm) fn type_parents(name: &str) -> alloc::vec::Vec<&'static str> {
    if name == "bool" { return alloc::vec!["int"]; }
    exc_parents(name)
}

pub(crate) fn matches_exc_class(actual: &str, expected: &str) -> bool {
    let mut cur = actual;
    loop {
        if cur == expected { return true; }
        if exc_also(cur).is_some_and(|p| matches_exc_class(p, expected)) { return true; }
        match exc_parent(cur) {
            Some(p) => cur = p,
            None => return false,
        }
    }
}
