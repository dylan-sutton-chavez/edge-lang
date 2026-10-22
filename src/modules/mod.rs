use alloc::{boxed::Box, string::{String, ToString}, sync::Arc, vec::Vec};

use crate::s;
use crate::value::{HeapPool, Val, VmErr};
use crate::lexer::{lex, Token, TokenType};

pub mod bundle;
pub mod json;
pub mod lock;
pub mod manifest;
pub mod rules;
pub mod table;
pub mod walk;
pub use manifest::{Manifest, parse_manifest, parent_dir, walk_up_dirs, dir_of, entry_dir, join_relative, system_spec};

/* Plain fn-pointer alias for hand-written Rust natives, the `Arc<dyn Fn ...>` form lives in `ExternFnPtr` below. Third arg is the kwargs slot, `None` for plain positional calls, `Some(dict_val)` when the caller used `name=value`, natives that don't accept kwargs ignore it. */
pub type ExternFnPlain = fn(&mut HeapPool, &[Val], Option<Val>) -> Result<Val, VmErr>;

/* Arc-wrapped callable for EdgePython natives, supports stateful loaders. */
pub type ExternFnPtr = crate::value::ExternCallable;

/* A native binding is the extern a call site binds. */
pub type NativeBinding = crate::value::ExternFn;

impl NativeBinding {
    /* Convenience constructor wrapping a plain fn pointer into an Arc for hand-written Rust natives. */
    pub fn from_fn(name: impl Into<String>, func: ExternFnPlain) -> Self {
        Self { name: name.into(), func: Arc::new(func) }
    }
}

/* Native class definition, methods become Extern values inside a HeapObj::Class built at init time. */
#[derive(Clone)]
pub struct NativeClass {
    pub name: String,
    pub methods: Vec<NativeBinding>,
}

/* Parse-time resolver result, Code splices .py defs, Native registers extern bindings and classes, the VM sees Call/CallExtern. canonical dedupes diamond imports by resolved path. */
#[derive(Clone)]
pub enum Resolved {
    Code { src: alloc::string::String, canonical: alloc::string::String },
    Native { bindings: Vec<NativeBinding>, classes: Vec<NativeClass>, consts: Vec<NativeBinding>, canonical: alloc::string::String },
}

/* Splits native bindings by export-name convention, `__class_` methods, `__const_` values, rest free functions. */
pub fn partition_bindings(all: Vec<NativeBinding>) -> (Vec<NativeBinding>, Vec<NativeClass>, Vec<NativeBinding>) {
    let mut bindings = Vec::new();
    let mut class_map: Vec<(String, Vec<NativeBinding>)> = Vec::new();
    let mut consts = Vec::new();
    for b in all {
        if let Some(name) = b.name.strip_prefix("__fn_") {
            let name = name.to_string();
            bindings.push(NativeBinding { name, ..b });
        } else if let Some(name) = b.name.strip_prefix("__const_") {
            let name = name.to_string();
            consts.push(NativeBinding { name, ..b });
        } else if let Some((class_name, method)) = parse_class_export(&b.name) {
            let (class_name, method) = (class_name.to_string(), method.to_string());
            let m = NativeBinding { name: method, ..b };
            if let Some(e) = class_map.iter_mut().find(|(n, _)| *n == class_name) {
                e.1.push(m);
            } else {
                class_map.push((class_name, alloc::vec![m]));
            }
        } else {
            bindings.push(b);
        }
    }
    let classes = class_map.into_iter().map(|(name, methods)| NativeClass { name, methods }).collect();
    (bindings, classes, consts)
}

/* Returns (class_name, method_name) when export matches `__class_<Name>_<method>`, else None. */
fn parse_class_export(export: &str) -> Option<(&str, &str)> {
    let rest = export.strip_prefix("__class_")?;
    let sep = rest.find('_')?;
    let (class_name, method_part) = rest.split_at(sep);
    if class_name.is_empty() { return None; }
    let method = &method_part[1..];
    if method.is_empty() { return None; }
    Some((class_name, method))
}

/* Host-injected trait, resolve called once per import statement. &mut self allows internal caching. */
pub trait Resolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String>;

    /* Returns a child resolver rescoped to the imported file's directory for transitive imports, default is NoopResolver. */
    fn child(&self, _spec: &str) -> Box<dyn Resolver> {
        Box::new(NoopResolver)
    }

    /* Returns raw bytes for integrity verification and manifest walk-up, Err signals absent file. expected_hash must match. */
    fn fetch_bytes(&mut self, _spec: &str, _expected_hash: Option<[u8; 32]>) -> Result<alloc::vec::Vec<u8>, String> {
        Err(s!("module '", str _spec, "' integrity verification not supported by this resolver"))
    }
}

/* Splits spec into (url, Option<[u8;32]>), errors on malformed #sha256- fragment, decodes hex once. */
pub fn parse_integrity(spec: &str) -> Result<(&str, Option<[u8; 32]>), String> {
    let Some((url, frag)) = spec.split_once('#') else {
        return Ok((spec, None));
    };
    let Some(hex) = frag.strip_prefix("sha256-") else {
        return Err(s!("unrecognized integrity fragment in '", str spec, "'; expected '#sha256-<64 hex chars>'"));};
    if hex.len() != 64 {
        return Err(s!("sha256 fragment must be 64 hex chars in '", str spec, "'; got ", int hex.len() as i64));
    }
    let hash = crate::util::sha256::hex_decode_32(hex).ok_or_else(|| s!("invalid hex in sha256 fragment of '", str spec, "'"))?;
    Ok((url, Some(hash)))
}

/* Makes the resolver for code compiled at run time, given the dir of the file that compiled it. */
pub type ResolverHook = &'static dyn Fn(&str) -> Box<dyn Resolver>;

/* Default resolver rejecting all specs with a diagnostic, used when no resolver is configured. */
pub struct NoopResolver;

impl Resolver for NoopResolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        Err(s!("module '", str spec, "' not found (no resolver configured)"))
    }
}

impl Default for Box<dyn Resolver> {
    fn default() -> Self { Box::new(NoopResolver) }
}


/* A scanned import, paths carry the .py suffix with dots already mapped to slashes. */
#[derive(Debug, Clone, PartialEq)]
pub enum ImportSpec {
    // A plain name, resolved against the manifest chain.
    Bare(String),
    // Leading dots, anchored at the importer dir.
    Relative(String),
    // A dotted name, anchored at the nearest edge.json dir.
    Root(String),
}

/* Consumes `name(.name)*` at token `j`, returns ("a/b/c.py", segment count, index past). */
fn dotted_path(src: &str, tokens: &[Token], j: usize) -> (String, usize, usize) {
    let mut path = src[tokens[j].start..tokens[j].end].to_string();
    let mut segs = 1;
    let mut k = j + 1;
    while tokens.get(k).map(|x| x.kind) == Some(TokenType::Dot)
        && let Some(seg) = tokens.get(k + 1).filter(|s| s.kind == TokenType::Name)
    {
        path.push('/');
        path.push_str(&src[seg.start..seg.end]);
        segs += 1;
        k += 2;
    }
    path.push_str(".py");
    (path, segs, k)
}

/* Reads the module spec at token `j`, leading dots are importer-relative, dotted names root-relative, a plain name is Bare. Quoted strings are not specs. Returns (spec, index past it). */
fn read_spec(src: &str, tokens: &[Token], j: usize) -> Option<(ImportSpec, usize)> {
    let t = tokens.get(j)?;
    match t.kind {
        TokenType::Dot => {
            let mut ups = 1;
            while tokens.get(j + ups).map(|x| x.kind) == Some(TokenType::Dot) { ups += 1; }
            if tokens.get(j + ups).map(|x| x.kind) != Some(TokenType::Name) { return None; }
            let (path, _, next) = dotted_path(src, tokens, j + ups);
            let prefix = if ups == 1 { s!("./") } else { "../".repeat(ups - 1) };
            Some((ImportSpec::Relative(s!(str &prefix, str &path)), next))
        }
        TokenType::Name => {
            let (path, segs, next) = dotted_path(src, tokens, j);
            if segs == 1 {
                Some((ImportSpec::Bare(src[t.start..t.end].to_string()), next))
            } else {
                Some((ImportSpec::Root(path), next))
            }
        }
        _ => None,
    }
}

/* Every import spec, classified Bare vs path, via the lexer so a `from`/`import` inside a comment or string is never a false hit. */
pub fn scan_imports(src: &str) -> Vec<ImportSpec> { scan(src).0 }

/* The import specs of `src`, and whether it names `eval`, `exec` or `compile`, whose text may import what the program never did. */
pub fn scan(src: &str) -> (Vec<ImportSpec>, bool) {
    let (tokens, _errs) = lex(src);
    let runs_text = tokens.iter().any(|t| t.kind == TokenType::Name && matches!(&src[t.start..t.end], "eval" | "exec" | "compile"));
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenType::From => {
                if let Some((spec, next)) = read_spec(src, &tokens, i + 1) {
                    out.push(spec);
                    // Step past the `import` of this from-statement so it isn't read as a fresh statement.
                    i = if tokens.get(next).map(|x| x.kind) == Some(TokenType::Import) { next + 1 } else { next };
                } else {
                    i += 1;
                }
            }
            TokenType::Import => {
                // `import a, b as c`, comma-separated specs, each with an optional `as` alias.
                let mut j = i + 1;
                while let Some((spec, next)) = read_spec(src, &tokens, j) {
                    out.push(spec);
                    j = next;
                    if tokens.get(j).map(|x| x.kind) == Some(TokenType::As) {
                        j += if tokens.get(j + 1).map(|x| x.kind) == Some(TokenType::Name) { 2 } else { 1 };
                    }
                    if tokens.get(j).map(|x| x.kind) != Some(TokenType::Comma) { break; }
                    j += 1;
                }
                i = j.max(i + 1);
            }
            _ => i += 1,
        }
    }
    (out, runs_text)
}
