#![no_std]

extern crate alloc;

use alloc::{boxed::Box, format, string::{String, ToString}, sync::Arc, vec::Vec};

use compiler::lexer::{lex, TokenType};
use compiler::modules::{NativeBinding, Resolved, Resolver};
use compiler::parser::{Diagnostic, Parser, SSAChunk};
use compiler::value::{HeapObj, HeapPool, Val, VmErr};
use compiler::vm::VM;

pub use compiler::vm::Limits;

// a source rewrite applied before lexing, its output must be valid edge python
pub type Transform = dyn Fn(&str) -> String + Send + Sync + 'static;

// scalars round-trip losslessly, composites surface as object carrying their rendered form
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Object(String),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            Value::Bool(b) => Some(*b as i64 as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Value::Str(s) = self { Some(s) } else { None }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Value::Bool(b) = self { Some(*b) } else { None }
    }

    // render covers composites the heap alone cannot stringify
    fn read(v: Val, heap: &HeapPool, render: impl FnOnce(Val) -> String) -> Value {
        if v.is_none() { Value::None }
        else if v.is_bool() { Value::Bool(v.as_bool()) }
        else if v.is_int() { Value::Int(v.as_int()) }
        else if v.is_float() { Value::Float(v.as_float()) }
        else if v.is_heap() {
            match heap.get(v) {
                HeapObj::Str(s) => Value::Str(s.clone()),
                HeapObj::LongInt(i) => match i64::try_from(i.get()) {
                    Ok(n) => Value::Int(n),
                    Err(_) => Value::Object(i.get().to_string()),
                },
                _ => Value::Object(render(v)),
            }
        } else { Value::None }
    }

    // a composite has no owned form here, so it is refused rather than passed blank
    fn read_arg(v: Val, heap: &HeapPool) -> Result<Value, VmErr> {
        if v.is_heap() && !matches!(heap.get(v), HeapObj::Str(_) | HeapObj::LongInt(_)) {
            return Err(VmErr::TypeMsg(String::from("host function argument must be a scalar or a str")));
        }
        Ok(Self::read(v, heap, |_| String::new()))
    }

    // allocated on the engine heap so a native can return it
    fn write(&self, heap: &mut HeapPool) -> Result<Val, VmErr> {
        Ok(match self {
            Value::None => Val::none(),
            Value::Bool(b) => Val::bool(*b),
            Value::Int(i) => match Val::int_checked(*i) {
                Some(v) => v,
                None => heap.alloc(HeapObj::LongInt((*i as i128).into()))?,
            },
            Value::Float(f) => Val::float(*f),
            Value::Str(s) | Value::Object(s) => heap.alloc(HeapObj::Str(s.clone()))?,
        })
    }
}

impl core::fmt::Display for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::None => f.write_str("None"),
            Value::Bool(b) => f.write_str(if *b { "True" } else { "False" }),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) | Value::Object(s) => f.write_str(s),
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self { Value::Bool(v) }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self { Value::Int(v) }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self { Value::Float(v) }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self { Value::Str(s.to_string()) }
}
impl From<String> for Value {
    fn from(s: String) -> Self { Value::Str(s) }
}

// compile carries a rustc style diagnostic, run carries a traceback
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Compile(String),
    Run(String),
}

impl Error {
    pub fn message(&self) -> &str {
        match self { Error::Compile(m) | Error::Run(m) => m }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Compile(m) => write!(f, "compile error: {m}"),
            Error::Run(m) => write!(f, "run error: {m}"),
        }
    }
}

impl core::error::Error for Error {}

// the value the module evaluated to plus its captured output
#[derive(Clone, Debug)]
pub struct Output {
    value: Value,
    text: String,
}

impl Output {
    pub fn value(&self) -> &Value { &self.value }
    pub fn text(&self) -> &str { &self.text }
    pub fn into_value(self) -> Value { self.value }
}

// cheap to clone and reusable across compiles
#[derive(Clone)]
pub struct Engine {
    limits: Limits,
    natives: Arc<Vec<NativeBinding>>,
    transforms: Arc<Vec<Box<Transform>>>,
}

impl core::fmt::Debug for Engine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Engine")
            .field("natives", &self.natives.len())
            .field("transforms", &self.transforms.len())
            .finish_non_exhaustive()
    }
}

impl Engine {
    pub fn builder() -> Builder {
        Builder { limits: Limits::sandbox(), natives: Vec::new(), transforms: Vec::new() }
    }

    // err renders a diagnostic with a source caret
    pub fn compile(&self, source: &str) -> Result<Program, Error> {
        // custom syntax rewrites run first, each fed the previous output
        let mut src = source.to_string();
        for t in self.transforms.iter() {
            src = t(&src);
        }
        let (tokens, lex_errs) = lex(&src);
        let resolver = HostResolver { natives: self.natives.clone() };
        let mut parser = Parser::with_resolver(&src, tokens.into_iter(), Box::new(resolver));
        for e in lex_errs {
            parser.errors.push(Diagnostic { start: e.start, end: e.end, msg: e.msg.into() });
        }
        let (mut chunk, errs) = parser.parse();
        if !errs.is_empty() {
            let mut buf = String::new();
            for (i, e) in errs.iter().enumerate() {
                if i > 0 { buf.push('\n'); }
                buf.push_str(&e.render(&src, None));
            }
            return Err(Error::Compile(buf));
        }
        compiler::optimizer::constant_fold(&mut chunk);
        Ok(Program { chunk, source: src, limits: self.limits })
    }
}

pub struct Builder {
    limits: Limits,
    natives: Vec<NativeBinding>,
    transforms: Vec<Box<Transform>>,
}

impl Builder {
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    // rewrites source before lexing, the hook for custom syntax and dsls
    pub fn transform<F>(mut self, f: F) -> Self
    where F: Fn(&str) -> String + Send + Sync + 'static
    {
        self.transforms.push(Box::new(f));
        self
    }

    // renames whole words only, so a longer identifier never matches
    pub fn keyword(self, from: impl Into<String>, to: impl Into<String>) -> Self {
        let (from, to) = (from.into(), to.into());
        self.transform(move |src| rename_word(src, &from, &to))
    }

    // exposed under from host import name, impure so results never memoize
    pub fn define<F>(mut self, name: impl Into<String>, f: F) -> Self
    where F: Fn(&[Value]) -> Result<Value, String> + Send + Sync + 'static
    {
        let name = name.into();
        let reported = name.clone();
        let f = Arc::new(f);
        self.natives.push(NativeBinding {
            name,
            func: Arc::new(move |heap, argv, kwargs| {
                if kwargs.is_some() {
                    return Err(VmErr::TypeMsg(format!("'{reported}' takes no keyword arguments")));
                }
                let mut args = Vec::with_capacity(argv.len());
                for &v in argv {
                    args.push(Value::read_arg(v, heap)?);
                }
                f(&args).map_err(VmErr::Raised).and_then(|out| out.write(heap))
            }),
            pure: false,
        });
        self
    }

    pub fn build(self) -> Engine {
        Engine {
            limits: self.limits,
            natives: Arc::new(self.natives),
            transforms: Arc::new(self.transforms),
        }
    }
}

// a compiled unit, produced only by engine compile
pub struct Program {
    chunk: SSAChunk,
    source: String,
    limits: Limits,
}

impl Program {
    // binds the natives the chunk resolved at compile time
    fn boot(&self) -> Result<VM<'_>, Error> {
        let mut vm = VM::with_limits(&self.chunk, self.limits);
        vm.bind_chunk_externs().map_err(|e| Error::Run(e.render()))?;
        Ok(vm)
    }

    // renders a vm error as a traceback tied to this program source
    fn trace(&self, vm: &VM<'_>, e: VmErr) -> Error {
        Error::Run(e.render_traceback(
            &self.source, vm.error_pos(), None,
            vm.call_stack_frames(), vm.function_names_ref(),
        ))
    }

    // keeps only output past from, skipping what was already reported
    fn outcome(vm: &VM<'_>, v: Val, from: usize) -> Output {
        let value = Value::read(v, vm.heap(), |v| vm.display(v));
        let full = vm.output_text();
        let text = full.get(from..).unwrap_or("").into();
        Output { value, text }
    }

    // runs to completion under the engine limits, err renders a traceback
    pub fn run(&self) -> Result<Output, Error> {
        let mut vm = self.boot()?;
        match vm.run() {
            Ok(v) => Ok(Self::outcome(&vm, v, 0)),
            Err(e) => Err(self.trace(&vm, e)),
        }
    }

    // runs the module body once so defs are bound, then keeps the vm for calls
    pub fn start(&self) -> Result<Instance<'_>, Error> {
        let mut vm = self.boot()?;
        if let Err(e) = vm.run() {
            return Err(self.trace(&vm, e));
        }
        let emitted = vm.output_text().len();
        Ok(Instance { program: self, vm, emitted })
    }

    // each call gets a fresh instance, so nothing leaks between invocations
    pub fn call(&self, name: &str, args: &[Value]) -> Result<Output, Error> {
        self.start()?.call(name, args)
    }

    pub fn source(&self) -> &str { &self.source }
}

impl core::fmt::Debug for Program {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Program").field("source", &self.source).finish_non_exhaustive()
    }
}

// the module body has already run. calls share one vm, drop it to reset
pub struct Instance<'a> {
    program: &'a Program,
    vm: VM<'a>,
    emitted: usize,
}

impl Instance<'_> {
    // the output carries the return value and only what this call printed
    pub fn call(&mut self, name: &str, args: &[Value]) -> Result<Output, Error> {
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            match a.write(self.vm.heap_mut()) {
                Ok(v) => argv.push(v),
                Err(e) => return Err(self.program.trace(&self.vm, e)),
            }
        }
        let out = match self.vm.call_export(name, &argv) {
            Ok(v) => Program::outcome(&self.vm, v, self.emitted),
            Err(e) => return Err(self.program.trace(&self.vm, e)),
        };
        self.emitted += out.text.len();
        Ok(out)
    }
}

impl core::fmt::Debug for Instance<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Instance").finish_non_exhaustive()
    }
}

// in memory resolver exposing the engine natives under the host module
struct HostResolver {
    natives: Arc<Vec<NativeBinding>>,
}

impl Resolver for HostResolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        if spec != "host" {
            return Err(format!("module '{spec}' not found (only 'host' is available)"));
        }
        // plain functions only, the engine export name conventions stay out of this api
        Ok(Resolved::Native {
            bindings: (*self.natives).clone(),
            classes: Vec::new(),
            consts: Vec::new(),
            canonical: "host".to_string(),
        })
    }
}

// skips strings, f-string text and comments, so only real word tokens change
fn rename_word(src: &str, from: &str, to: &str) -> String {
    if from.is_empty() { return src.to_string(); }
    let (tokens, _) = lex(src);
    let mut out = String::with_capacity(src.len());
    let mut last = 0;
    for t in tokens {
        let literal = matches!(t.kind,
            TokenType::String | TokenType::Bytes | TokenType::Comment
            | TokenType::FstringStart | TokenType::FstringMiddle | TokenType::FstringEnd);
        if literal || t.start < last || src.get(t.start..t.end) != Some(from) { continue; }
        out.push_str(&src[last..t.start]);
        out.push_str(to);
        last = t.end;
    }
    out.push_str(&src[last..]);
    out
}
