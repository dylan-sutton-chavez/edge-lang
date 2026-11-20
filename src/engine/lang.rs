mod bind;
mod cx;
mod error;
mod language;
mod native;
mod policy;
pub(crate) mod sig;

pub use bind::*;
pub use cx::Cx;
pub use error::*;
pub use language::*;
pub use native::*;
pub use policy::*;
pub use sig::Misfit;

use crate::engine::value::{HeapObj, HeapPool, Val};

/* how the tools around the engine name a runtime's files and its errors */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Descriptor {
    // the name help and messages give the language
    pub(crate) language: &'static str,
    // a source file extension without its dot
    pub(crate) extension: &'static str,
    // entry names searched in order where the first is the default
    pub(crate) entries: &'static [&'static str],
    pub(crate) test_suffix: &'static str,
    // the program a new project gets as its entry
    pub(crate) scaffold: &'static str,
    // the docs fence of a runnable example
    pub(crate) example_fence: &'static str,
    // the docs fence of the interpreter.json an example runs under
    pub(crate) manifest_fence: &'static str,
    // the class of the abi type error kind
    pub(crate) type_error: &'static str,
    // the class of the abi value error kind
    pub(crate) value_error: &'static str,
    // the class of the abi runtime kind and of every unknown failure
    pub(crate) runtime_error: &'static str,
    // the class of a host call no grant covers
    pub(crate) denied_error: &'static str,
    // the class of a host call that reached out and met a failure
    pub(crate) failed_error: &'static str,
    // the exit statuses of a native binary after a compile error and a run error
    pub(crate) statuses: (u8, u8),
    // the words a native binary gives compile errors where none keeps the engine's report
    pub(crate) diagnosed: Option<fn(&str, &[crate::engine::syntax::diag::Diagnostic]) -> alloc::string::String>,
}

impl Descriptor {
    setters! {
        language: &'static str,
        entries: &'static [&'static str],
        test_suffix: &'static str,
        scaffold: &'static str,
        example_fence: &'static str,
        manifest_fence: &'static str,
        failed_error: &'static str,
        statuses: (u8, u8),
        diagnosed: Option<fn(&str, &[crate::engine::syntax::diag::Diagnostic]) -> alloc::string::String>,
    }
}

impl Descriptor {
    /* the names of a machine booted with no runtime which a runtime's setters start from */
    pub const PLAIN: Descriptor = Descriptor {
        language: "", extension: "", entries: &[""], test_suffix: "", scaffold: "", example_fence: "", manifest_fence: "",
        type_error: "Type", value_error: "Value", runtime_error: "Runtime", denied_error: "Permission", failed_error: "Runtime",
        statuses: (1, 1), diagnosed: None,
    };

    /* these names with the extension its front reads and the error classes its names give */
    pub(crate) const fn derived(self, extension: &'static str, names: &Names) -> Descriptor {
        Descriptor {
            extension,
            type_error: names.error(ErrKind::Type),
            value_error: names.error(ErrKind::Value),
            runtime_error: names.error(ErrKind::Runtime),
            denied_error: names.error(ErrKind::Permission),
            ..self
        }
    }

    // the entry a new project gets and a directory runs
    pub(crate) const fn entry(&self) -> &'static str { self.entries[0] }
}

/* the engine kinds a native method is registered on */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
#[non_exhaustive]
pub enum KindId { None, Bool, Int, Float, Str, Bytes, ByteArray, List, Tuple, Dict, Set, Range, Func, Coro, Iter, Module, Type, Class, Instance, Cell, Other, Foreign }

pub const KINDS: usize = KindId::Foreign as usize + 1;

impl KindId {
    internal! {
        pub const ALL: [KindId; KINDS] = [
            KindId::None, KindId::Bool, KindId::Int, KindId::Float, KindId::Str, KindId::Bytes, KindId::ByteArray, KindId::List,
            KindId::Tuple, KindId::Dict, KindId::Set, KindId::Range, KindId::Func, KindId::Coro, KindId::Iter,
            KindId::Module, KindId::Type, KindId::Class, KindId::Instance, KindId::Cell, KindId::Other, KindId::Foreign,
        ];
    }

    pub(crate) fn from_u8(b: u8) -> Option<KindId> { KindId::ALL.get(b as usize).copied() }
}

/* the errors the engine raises itself each of which a runtime gives a class */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
#[non_exhaustive]
pub enum ErrKind {
    // the root of every class a program raises
    Base,
    // what a raise reads as when its value names no class of its own
    Error,
    Type, Value, Name,
    // a local read before anything binds it
    Unbound,
    Attribute, Key, Index, ZeroDiv,
    // an int too wide for the size it must fit
    Overflow,
    // calls nested past the recursion limit
    Recursion,
    // the memory limit reached
    Memory,
    Runtime,
    // the end of an iteration which ends the loop stepping it
    Stop,
    // the signal a close throws into a generator
    Close,
    // a program asking its host to end with a code
    Exit,
    Assert,
    // code compiled while the program runs that does not parse
    Syntax,
    // a host call or an import the grants refuse
    Permission,
}

pub const ERRORS: usize = ErrKind::Permission as usize + 1;

// a builtin no program can bind which a failed assert with a message raises
pub(crate) const ASSERT_CLASS: &str = "<assert fail>";

// the builtin iterators a snapshot stores by their place in `Names::iters`
pub const ITERS: usize = 19;

/* the place of each builtin iterator in `Names::iters` */
pub(crate) mod iters {
    pub(crate) const LIST: usize = 0;
    pub(crate) const RANGE: usize = 1;
    pub(crate) const TUPLE: usize = 2;
    pub(crate) const ASCII: usize = 3;
    pub(crate) const TEXT: usize = 4;
    pub(crate) const KEYS: usize = 5;
    pub(crate) const SET: usize = 6;
    pub(crate) const BYTES: usize = 7;
    pub(crate) const CALL: usize = 8;
    pub(crate) const MAP: usize = 9;
    pub(crate) const FILTER: usize = 10;
    pub(crate) const ZIP: usize = 11;
    pub(crate) const COUNT: usize = 12;
    pub(crate) const LIST_BACK: usize = 13;
    pub(crate) const BACK: usize = 14;
    pub(crate) const KEYS_BACK: usize = 15;
    pub(crate) const BYTE_ARRAY: usize = 16;
    pub(crate) const VALUES: usize = 17;
    pub(crate) const ITEMS: usize = 18;
}

/* what a runtime calls the kinds and errors of the engine and its conventions */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Names {
    // the type of a value of each engine kind as its errors name it
    pub(crate) kinds: [&'static str; KINDS],
    // the class of each engine error kind
    pub(crate) errors: [&'static str; ERRORS],
    // the type of each builtin iterator in snapshot order
    pub(crate) iters: [&'static str; ITERS],
    // the types a value shows past its kind
    pub(crate) coroutine: &'static str,
    pub(crate) descriptor: &'static str,
    pub(crate) native: &'static str,
    pub(crate) method: &'static str,
    pub(crate) exception: &'static str,
    pub(crate) ellipsis_type: &'static str,
    // the builtin holding the ellipsis
    pub(crate) ellipsis: &'static str,
    // the method a class runs on a new object
    pub(crate) constructor: &'static str,
    // the field holding a function's or class's own name
    pub(crate) name_field: &'static str,
    // the other reflection fields a value shows
    pub(crate) fields: Fields,
    // the global holding a module's name and its value in the entry module
    pub(crate) module_name: &'static str,
    pub(crate) entry: &'static str,
    // the global holding a module's docstring
    pub(crate) doc: &'static str,
    // a function with no name of its own in qualified names and tracebacks
    pub(crate) anonymous: &'static str,
    // the scope of a function inside a qualified name
    pub(crate) locals: &'static str,
    // the body of a module where a generator or a traceback names it
    pub(crate) module_body: &'static str,
    // a generator expression then list map and set comprehensions
    pub(crate) comprehensions: [&'static str; 4],
    // the path a traceback gives code compiled while the program runs
    pub(crate) compiled: &'static str,
    // a frame whose function the traceback cannot name
    pub(crate) unnamed: &'static str,
    // the words of an int too wide for an index
    pub(crate) overflow: &'static str,
    // the word for nil in the engine's own messages
    pub(crate) nil: &'static str,
    // the method that makes an object usable as an index
    pub(crate) index_method: &'static str,
    // the methods that send a value into a generator throw into it and close it
    pub(crate) send: &'static str,
    pub(crate) throw: &'static str,
    pub(crate) close: &'static str,
    // the classes the runtime raises with their parents
    pub(crate) tree: &'static Tree,
}

impl Names {
    /* the type each listed kind reads as with every kind it leaves out kept */
    pub const fn kinds(mut self, kinds: &[(KindId, &'static str)]) -> Self {
        let mut k = 0;
        while k < kinds.len() { self.kinds[kinds[k].0 as usize] = kinds[k].1; k += 1; }
        self
    }

    /* the class each listed error kind raises as with every kind it leaves out kept */
    pub const fn errors(mut self, errors: &[(ErrKind, &'static str)]) -> Self {
        let mut k = 0;
        while k < errors.len() { self.errors[errors[k].0 as usize] = errors[k].1; k += 1; }
        self
    }

    /* the types of the builtin iterators in snapshot order from the first with the rest kept */
    pub const fn iters(mut self, iters: &[&'static str]) -> Self {
        assert!(iters.len() <= ITERS, "more iterator names than builtin iterators");
        let mut k = 0;
        while k < iters.len() { self.iters[k] = iters[k]; k += 1; }
        self
    }

    setters! {
        coroutine: &'static str,
        descriptor: &'static str,
        native: &'static str,
        method: &'static str,
        exception: &'static str,
        ellipsis_type: &'static str,
        ellipsis: &'static str,
        constructor: &'static str,
        name_field: &'static str,
        fields: Fields,
        module_name: &'static str,
        entry: &'static str,
        doc: &'static str,
        anonymous: &'static str,
        locals: &'static str,
        module_body: &'static str,
        comprehensions: [&'static str; 4],
        compiled: &'static str,
        unnamed: &'static str,
        overflow: &'static str,
        nil: &'static str,
        index_method: &'static str,
        send: &'static str,
        throw: &'static str,
        close: &'static str,
        tree: &'static Tree,
    }
}

/* the fields the engine reads off its kinds under the runtime's names */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Fields {
    // what a bound method was bound to and its function
    pub(crate) receiver: &'static str,
    pub(crate) function: &'static str,
    // the arguments an error was made with
    pub(crate) args: &'static str,
    // the value an ending iteration carries
    pub(crate) value: &'static str,
    // the bounds and the step of a range or a slice
    pub(crate) start: &'static str,
    pub(crate) stop: &'static str,
    pub(crate) step: &'static str,
}

impl Fields {
    setters! {
        receiver: &'static str,
        function: &'static str,
        args: &'static str,
        value: &'static str,
        start: &'static str,
        stop: &'static str,
        step: &'static str,
    }
}

impl Fields {
    /* the engine's own field names which every runtime naming nothing gets */
    pub const PLAIN: Fields = Fields {
        receiver: "receiver", function: "function_of", args: "arguments", value: "carried",
        start: "first", stop: "end", step: "stride",
    };

    /* which bound or step of a range or a slice the field `name` reads */
    pub(crate) fn bound(&self, name: &str) -> Option<usize> { [self.start, self.stop, self.step].iter().position(|&n| native::same_text(n, name)) }
}

impl Names {
    /* the engine's own words which every runtime naming nothing gets */
    pub const PLAIN: Names = Names {
        kinds: [
            "Nil", "Bool", "Int", "Float", "Str", "Bytes", "ByteArray", "List", "Tuple", "Dict", "Set", "Range",
            "Func", "Coro", "Iter", "Module", "Type", "Class", "Instance", "Cell", "Other", "Foreign",
        ],
        errors: [
            "Base", "Error", "Type", "Value", "Name", "Unbound", "Attribute", "Key", "Index", "ZeroDiv", "Overflow",
            "Recursion", "Memory", "Runtime", "Stop", "Close", "Exit", "Assert", "Syntax", "Permission",
        ],
        iters: [
            "ListIter", "RangeIter", "TupleIter", "AsciiIter", "StrIter", "KeyIter", "SetIter", "BytesIter", "CallIter",
            "MapIter", "FilterIter", "ZipIter", "CountIter", "ListBackIter", "BackIter", "KeyBackIter", "ByteArrayIter",
            "ValueIter", "ItemIter",
        ],
        coroutine: "Async", descriptor: "Descriptor", native: "Native", method: "Method", exception: "Fault",
        ellipsis_type: "Dots", ellipsis: "Dots", constructor: "init", name_field: "name",
        fields: Fields::PLAIN,
        module_name: "module_name",
        entry: "main", doc: "doc", anonymous: "<anonymous>", locals: "<scope>", module_body: "<body>",
        comprehensions: ["<each>", "<each>", "<each>", "<each>"], compiled: "<compiled>", unnamed: "<anonymous>",
        overflow: "int too large for an index", nil: "nil", index_method: "as_index", send: "resume", throw: "raise_into", close: "finish", tree: &Tree::EMPTY,
    };

    /* the type a value of kind `k` reads as */
    #[inline]
    pub const fn kind(&self, k: KindId) -> &'static str { self.kinds[k as usize] }

    /* the class of engine error kind `e` */
    #[inline]
    pub const fn error(&self, e: ErrKind) -> &'static str { self.errors[e as usize] }

    /* the kind a builtin type object `name` makes or None past kinds with methods */
    pub(crate) fn kind_named(&self, name: &str) -> Option<KindId> {
        use KindId::*;
        if same_text(name, self.coroutine) { return Some(Coro); }
        [None, Bool, Int, Float, Str, Bytes, ByteArray, List, Tuple, Dict, Set, Range, Coro].into_iter().find(|&k| same_text(self.kind(k), name))
    }

    /* the error kind whose class is `name` */
    pub(crate) fn error_named(&self, name: &str) -> Option<ErrKind> { ErrKind::ALL.into_iter().find(|&e| same_text(self.error(e), name)) }

    /* whether `name` is a class the runtime raises */
    #[inline]
    pub(crate) fn is_exception(&self, name: &str) -> bool { self.tree.contains(name) }
}

impl ErrKind {
    internal! {
        pub const ALL: [ErrKind; ERRORS] = [
            ErrKind::Base, ErrKind::Error, ErrKind::Type, ErrKind::Value, ErrKind::Name, ErrKind::Unbound, ErrKind::Attribute,
            ErrKind::Key, ErrKind::Index, ErrKind::ZeroDiv, ErrKind::Overflow, ErrKind::Recursion, ErrKind::Memory,
            ErrKind::Runtime, ErrKind::Stop, ErrKind::Close, ErrKind::Exit, ErrKind::Assert, ErrKind::Syntax, ErrKind::Permission,
        ];
    }
}

/* the kind of `v` by its tag and heap variant with wide ints as ints */
#[inline]
pub fn kind_of(heap: &HeapPool, v: Val) -> KindId {
    if !v.is_heap() {
        if v.is_int() { return KindId::Int; }
        if v.is_float() { return KindId::Float; }
        if v.is_bool() { return KindId::Bool; }
        if v.is_none() { return KindId::None; }
        if v.is_foreign() { return if heap.is_wide(v) { KindId::Int } else { KindId::Foreign }; }
        return KindId::Other;
    }
    match heap.try_get(v) {
        Some(HeapObj::Str(_)) => KindId::Str,
        Some(HeapObj::List(_)) => KindId::List,
        Some(HeapObj::Dict(_)) => KindId::Dict,
        Some(HeapObj::Tuple(_)) => KindId::Tuple,
        Some(HeapObj::Set(_)) => KindId::Set,
        Some(HeapObj::Bytes(_)) => KindId::Bytes,
        Some(HeapObj::ByteArray(_)) => KindId::ByteArray,
        Some(HeapObj::Range(..) | HeapObj::BigRange(..)) => KindId::Range,
        Some(HeapObj::Func { .. }) => KindId::Func,
        Some(HeapObj::Bound { func, .. }) if func.is_heap() => KindId::Func,
        Some(HeapObj::Coroutine(_)) => KindId::Coro,
        Some(HeapObj::Iter(..)) => KindId::Iter,
        Some(HeapObj::Module(..)) => KindId::Module,
        Some(HeapObj::Object { meta, .. }) => if meta.is_type_meta() { KindId::Type } else if meta.is_class_meta() { KindId::Class } else { KindId::Instance },
        Some(HeapObj::Cell(_)) => KindId::Cell,
        _ => KindId::Other,
    }
}
