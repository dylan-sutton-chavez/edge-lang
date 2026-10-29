use alloc::{boxed::Box, string::{String, ToString}, sync::Arc, vec::Vec};

use crate::s;
use crate::value::{HeapPool, Val, VmErr};
use crate::syntax::lexer::{lex_as, Spelling, Token, TokenType};

pub mod bundle;
pub mod json;
pub mod lock;
pub mod manifest;
pub mod package;
// the one grant logic every host links or reaches through the runtime exports
pub mod perms;
pub mod resolver;
pub mod rules;
pub mod served;
pub mod walk;
pub use manifest::{Manifest, parse_manifest, parse_manifest_with, parent_dir, walk_up_dirs, dir_of, entry_dir, join_relative, joined, origin, outside, system_spec};
pub use resolver::HostResolver;

/* a plain fn native whose last argument holds the keywords when a call passes any */
pub type ExternFnPlain = fn(&mut HeapPool, &[Val], Option<Val>) -> Result<Val, VmErr>;

/* the shared callable form so a native can keep state between calls */
pub type ExternFnPtr = crate::value::ExternCallable;

/* the extern a call site binds */
pub type NativeBinding = crate::value::ExternFn;

impl NativeBinding {
    /* wraps a plain fn native in the shared callable form */
    pub fn from_fn(name: impl Into<String>, func: ExternFnPlain) -> Self {
        Self { name: name.into(), func: Arc::new(func) }
    }
}

/* a native class whose methods become externs of the class built at boot */
#[derive(Clone)]
pub struct NativeClass {
    pub name: String,
    pub methods: Vec<NativeBinding>,
}

/* what an import resolves to with the canonical spec that dedupes diamond imports */
#[derive(Clone)]
pub enum Resolved {
    Code { src: alloc::string::String, canonical: alloc::string::String },
    Native { bindings: Vec<NativeBinding>, classes: Vec<NativeClass>, consts: Vec<NativeBinding>, canonical: alloc::string::String },
}

/* splits bindings by export name into `__class_` methods and `__const_` values and free functions */
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

/* the class and method an export named `__class_<class>_<method>` binds */
fn parse_class_export(export: &str) -> Option<(&str, &str)> {
    let rest = export.strip_prefix("__class_")?;
    let sep = rest.find('_')?;
    let (class_name, method_part) = rest.split_at(sep);
    if class_name.is_empty() { return None; }
    let method = &method_part[1..];
    if method.is_empty() { return None; }
    Some((class_name, method))
}

/* what the parser asks the host once per import statement */
pub trait Resolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String>;

    /* a resolver rescoped to the imported file's dir for its own imports */
    fn child(&self, _spec: &str) -> Box<dyn Resolver> {
        Box::new(NoopResolver)
    }

    /* the raw bytes behind `spec` which must hash to `expected_hash` when given */
    fn fetch_bytes(&mut self, _spec: &str, _expected_hash: Option<[u8; 32]>) -> Result<alloc::vec::Vec<u8>, String> {
        Err(s!("module '", str _spec, "' integrity verification not supported by this resolver"))
    }
}

/* splits `spec` into its target and the sha256 its fragment pins */
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

/* the resolver that refuses every import when none is configured */
pub struct NoopResolver;

impl Resolver for NoopResolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        Err(s!("module '", str spec, "' not found (no resolver configured)"))
    }
}

impl Default for Box<dyn Resolver> {
    fn default() -> Self { Box::new(NoopResolver) }
}

/* what a host registered under a spec */
#[derive(Clone)]
pub enum ModuleEntry {
    Code(String),
    // each function name with the id the host answers it under
    Native(Vec<(String, u32)>),
}

/* the modules a host registered and what resolving them has read so far */
#[derive(Default)]
pub struct Deps {
    registry: Vec<(String, ModuleEntry)>,
    // specs the host refuses with the message their import fails with
    refusals: Vec<(String, String)>,
    manifests: Vec<(String, Manifest)>,
    // the package dir each source dir belongs to found once per dir
    packages: Vec<(String, Option<String>)>,
    // the walk the host is driving and the plugin bytes its last step named
    walk: Option<walk::Walk>,
    plugin: Vec<u8>,
}

impl Deps {
    pub const fn new() -> Self {
        Deps { registry: Vec::new(), refusals: Vec::new(), manifests: Vec::new(), packages: Vec::new(), walk: None, plugin: Vec::new() }
    }

    /* registers `spec` in place of what an earlier registration of it left */
    pub fn register(&mut self, spec: String, entry: ModuleEntry) {
        self.registry.retain(|(s, _)| *s != spec);
        self.registry.push((spec, entry));
    }

    /* makes an import of `spec` fail with `msg` */
    pub fn refuse(&mut self, spec: String, msg: String) {
        self.refusals.retain(|(s, _)| *s != spec);
        self.refusals.push((spec, msg));
    }

    /* forgets every module and refusal and what was read and stops the walk */
    pub fn reset(&mut self) {
        self.registry.clear();
        self.manifests.clear();
        self.packages.clear();
        self.refusals.clear();
        self.walk = None;
    }
}

/* what dependency management asks of its host as a handle bindings can keep */
pub trait DepHost: Clone + Send + Sync + 'static {
    /* runs `f` on the modules this host keeps */
    fn deps<R>(&mut self, f: impl FnOnce(&mut Deps) -> R) -> R;

    /* the bytes held for `spec` hashing to `pin` if given or why there are none */
    fn fetch(&mut self, spec: &str, pin: Option<[u8; 32]>) -> Result<Vec<u8>, String>;

    /* a binding that calls function `id` of the native module at `module` */
    fn native(&mut self, name: String, id: u32, module: Arc<str>) -> NativeBinding;

    /* the dir the code calling a native right now resolves from */
    fn caller_dir(&mut self) -> String;

    /* the modules changed so code compiled against them is stale */
    fn changed(&mut self) {}
}

/* the entries of a host list joined by `at` without the empty ones */
pub(crate) fn split(bytes: &[u8], at: char) -> Vec<String> {
    core::str::from_utf8(bytes).unwrap_or("").split(at).filter(|l| !l.is_empty()).map(String::from).collect()
}

/* a scanned import whose paths carry the spelling's suffix and slashes for dots */
#[derive(Debug, Clone, PartialEq)]
pub enum ImportSpec {
    // a plain name resolved through the manifest chain
    Bare(String),
    // leading dots anchored at the importer dir
    Relative(String),
    // a dotted name anchored at the nearest edge.json dir
    Root(String),
}

/* why the tokens after an import word name no module */
pub(crate) enum NoSpec {
    // a quoted path where the import wants a module name
    Quoted,
    // leading dots with no name after them
    Dots,
    // no name where the module starts
    Name,
    // a dot with no name after it
    Segment,
}

/* the spec at token `j` with where its first name sits and where it ends */
pub(crate) fn module_spec<S: Spelling>(src: &str, tokens: &[Token], j: usize) -> Result<(ImportSpec, usize, usize), (NoSpec, usize)> {
    let named = |k: usize| tokens.get(k).is_some_and(|t| matches!(t.kind, TokenType::Name | TokenType::Underscore));
    let dots = |k: usize| tokens.get(k).map_or(0, |x| crate::syntax::lexer::dots(x.kind, &src[x.start..x.end]));
    // a run of dot tokens counts every dot each one spells
    let (mut ups, mut k) = (0, j);
    while let n @ 1.. = dots(k) { ups += n; k += 1; }
    if ups == 0 && tokens.get(j).is_some_and(|t| t.kind == TokenType::String) { return Err((NoSpec::Quoted, j)); }
    if !named(k) { return Err((if ups > 0 { NoSpec::Dots } else { NoSpec::Name }, if ups > 0 { j } else { k })); }
    let first = k;
    let mut path = src[tokens[k].start..tokens[k].end].to_string();
    k += 1;
    while tokens.get(k).is_some_and(|t| t.kind == TokenType::Dot) {
        if !named(k + 1) { return Err((NoSpec::Segment, k + 1)); }
        path.push('/');
        path.push_str(&src[tokens[k + 1].start..tokens[k + 1].end]);
        k += 2;
    }
    let ext = S::SYNTAX.extension;
    let spec = match ups {
        0 if k == first + 1 => ImportSpec::Bare(path),
        0 => ImportSpec::Root(s!(str &path, ".", str ext)),
        1 => ImportSpec::Relative(s!("./", str &path, ".", str ext)),
        n => ImportSpec::Relative(s!(str &"../".repeat(n - 1), str &path, ".", str ext)),
    };
    Ok((spec, first, k))
}

/* every import spec read by the lexer so a comment or string never holds one */
pub fn scan_imports<S: Spelling>(src: &str) -> Vec<ImportSpec> {
    let (tokens, _errs) = lex_as::<S>(src);
    let mut out = Vec::new();
    // an import reads only where a statement starts as the parser reads it
    crate::syntax::lexer::statements::<S>(&tokens, |i| match tokens[i].kind {
        TokenType::From => if let Ok((spec, _, _)) = module_spec::<S>(src, &tokens, i + 1) { out.push(spec); },
        TokenType::Import => {
            let mut j = i + 1;
            // comma separated specs each with an optional `as` alias
            while let Ok((spec, _, next)) = module_spec::<S>(src, &tokens, j) {
                out.push(spec);
                j = next;
                if tokens.get(j).is_some_and(|t| t.kind == TokenType::As) {
                    if !tokens.get(j + 1).is_some_and(|t| matches!(t.kind, TokenType::Name | TokenType::Underscore)) { break; }
                    j += 2;
                }
                if !tokens.get(j).is_some_and(|t| t.kind == TokenType::Comma) { break; }
                j += 1;
            }
        }
        _ => {}
    });
    out
}
