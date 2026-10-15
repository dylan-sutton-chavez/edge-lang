use super::VM;

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

/* Parent of a built-in exception type, walked by `matches_exc_class`. Only the standard tree is encoded, user classes stay flat. */
fn exc_parent(name: &str) -> Option<&'static str> {
    Some(match name {
        "ValueError" | "TypeError" | "RuntimeError" | "LookupError" | "AttributeError" | "ArithmeticError"
        | "OSError" | "NameError" | "StopIteration" | "StopAsyncIteration" | "AssertionError" | "MemoryError"
        | "TimeoutError" | "ImportError" | "SyntaxError" => "Exception",
        "KeyError" | "IndexError" => "LookupError",
        "ZeroDivisionError" | "OverflowError" => "ArithmeticError",
        "PermissionError" => "OSError",
        "NotImplementedError" | "RecursionError" => "RuntimeError",
        "UnicodeError" => "ValueError",
        "UnicodeEncodeError" | "UnicodeDecodeError" => "UnicodeError",
        "ModuleNotFoundError" => "ImportError",
        "UnboundLocalError" => "NameError",
        // `SystemExit`, `CancelledError` and `GeneratorExit` sit under `BaseException`, outside `except Exception`.
        "SystemExit" | "CancelledError" | "GeneratorExit" | "Exception" => "BaseException",
        _ => return None,
    })
}

pub(in crate::vm) fn matches_exc_class(actual: &str, expected: &str) -> bool {
    let mut cur = actual;
    loop {
        if cur == expected { return true; }
        match exc_parent(cur) {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

impl<'a> VM<'a> {
    #[inline]
    pub(in crate::vm) fn mark_impure(&mut self) {
        if let Some(top) = self.observed_impure.last_mut() {
            *top = true;
        }
    }
}
