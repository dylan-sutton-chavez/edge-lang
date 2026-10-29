#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use edge::code::{Diagnostic, Parsed, Unit};
use edge::lang::{Descriptor, Front, Native, Natives, Runtime};
use edge::deps::ImportSpec;

pub mod expr;
pub mod flow;
mod kernels;
pub mod lang;
mod language;
mod lower;
mod natives;
pub mod words;
pub mod yaml;

pub use expr::{TinyExpr, TINY};
pub use lang::Tiny;
pub use words::report;

// the lib kernels the lowering calls under names no expression can spell
const CALLED: &[Native] = &lib::table![Tiny;
    "@iter" => lib::iter::iter,
    "@sorted" => lib::seq::sort::sorted{"key", "reverse"},
    "@top" => lib::seq::top::top{"key"},
];

pub static NATIVES: Natives = edge::natives! {
    language: Tiny,
    fns: [natives::FNS, natives::OWN, kernels::KERNELS, CALLED],
    methods: [
        Str => [natives::STR],
    ],
    types: lang::TYPES,
};

pub static RUNTIME: Runtime = Runtime::of::<Tiny>();

// tiny reads YAML itself and hands each expression to the kit's parser
pub const FRONT: Front = Front { extension: TINY.extension, parse, imports, syntax: None, policy: &<Tiny as edge::lang::Language>::POLICY };

/* the program `src` holds or the problems in the parser's form */
fn program(src: &str) -> Result<flow::Program, Vec<Diagnostic>> {
    let docs = yaml::documents(src).map_err(|e| alloc::vec![Diagnostic { start: e.span.start, end: e.span.end, msg: e.msg }])?;
    flow::check(src, &docs).map_err(|ds| ds.into_iter().map(|d| Diagnostic { start: d.span.start, end: d.span.end, msg: d.msg }).collect())
}

fn parse(src: &str, unit: Unit) -> Parsed {
    let prog = program(src)?;
    edge::code::build::<TinyExpr>(src, unit, |b| lower::lower(b, &prog))
}

/* the modules a flow imports as net for a url and files for a path */
fn imports(src: &str) -> Vec<ImportSpec> {
    match program(src).map(|p| p.from.source) {
        Ok(flow::Source::Net { .. }) => alloc::vec![ImportSpec::Bare("net".into())],
        Ok(flow::Source::Path { .. }) => alloc::vec![ImportSpec::Bare("files".into())],
        _ => Vec::new(),
    }
}

// the names the CLI gives this runtime's files and errors
pub const DESCRIPTOR: Descriptor = Descriptor {
    language: "tiny",
    extension: TINY.extension,
    entries: &["main.flow.yaml"],
    test_suffix: "_test.flow.yaml",
    scaffold: "from: input\ntotal:\n  count:\n",
    example_fence: "tiny",
    manifest_fence: "tiny-manifest",
    type_error: "WrongType",
    value_error: "BadData",
    runtime_error: "Error",
    denied_error: "Denied",
    failed_error: "HostFailed",
};

#[cfg(all(target_arch = "wasm32", feature = "web"))]
edge::web_exports!(RUNTIME);
