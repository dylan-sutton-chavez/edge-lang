#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use edge::deps::{
    NativeBinding, NoopResolver, Resolved, Resolver, partition_bindings,
    Manifest, walk_up_dirs, dir_of, join_relative,
};
use edge::value::{HeapObj, HeapPool, Limits, Val, VmErr};
use edge::code::{Diagnostic, Parser, SSAChunk};
use edge::embed::VM;
use edge::lexer::{lex_as, LexError, Token};

/* the spelling every program of these suites is written in */
pub type Spell = mini::Mini;

/* a VM on `chunk` whose program runs through the reference runtime's natives */
pub fn boot(chunk: &SSAChunk, limits: Limits) -> VM<'_> { VM::with_runtime(&mini::RUNTIME, chunk, limits) }

/* the tokens of `src` and what its lexer reported */
pub fn lex(src: &str) -> (Vec<Token>, Vec<LexError>) { lex_as::<Spell>(src) }

/* the parser of the tokens of `src` whose imports `resolver` answers */
pub fn parser<I: Iterator<Item = Token>>(src: &str, tokens: I, resolver: Box<dyn Resolver>) -> Parser<'_, I, Spell> {
    Parser::spelled(src, tokens, resolver)
}

/* `src` parsed with no imports and only the parser's diagnostics */
pub fn parse(src: &str) -> (SSAChunk, Vec<Diagnostic>) { parser(src, lex(src).0.into_iter(), Box::new(NoopResolver)).parse() }

/* `src` lexed parsed and folded as the engine compiles it */
pub fn compile(src: &str) -> (SSAChunk, Vec<Diagnostic>) { edge::code::compile_as::<Spell>(src) }

/* answers deferred call `id` with text the way `set_host_result_by_id` does for the wasm host */
pub fn answer(vm: &mut VM, id: u64, text: &str) -> bool {
    let val = vm.heap_mut().alloc(HeapObj::Str(text.into())).expect("the heap holds the answer");
    vm.inject_host_result_by_id(id, val)
}

/* raises `message` into deferred call `id` the way `set_host_error_by_id` does for the wasm host */
pub fn fail(vm: &mut VM, id: u64, message: &str) -> bool {
    vm.inject_host_error_by_id(id, VmErr::Raised(message.into()))
}

/* with `EDGE_SLICE=k/n` every n-th case from the k-th so Miri walks a corpus in slices */
pub fn slice<I: IntoIterator>(cases: I) -> impl Iterator<Item = I::Item> {
    let (k, n) = match std::env::var("EDGE_SLICE") {
        Ok(s) => s.split_once('/')
            .and_then(|(k, n)| Some((k.parse::<usize>().ok()?, n.parse::<usize>().ok()?)))
            .filter(|(k, n)| k < n)
            .unwrap_or_else(|| panic!("EDGE_SLICE is k/n with k below n, not {s:?}")),
        Err(_) => (0, 1),
    };
    cases.into_iter().enumerate().filter(move |(i, _)| i % n == k).map(|(_, case)| case)
}

/* under Miri a long case stops early so it checks code paths and not results */
pub fn corpus_limits() -> Limits {
    if cfg!(miri) { Limits { ops: MIRI_OPS, ..Limits::sandbox() } } else { Limits::sandbox() }
}

/* the budget of a corpus case under Miri which the sample's coverage is chosen on */
pub const MIRI_OPS: u64 = 5_000;

/* a run Miri stopped at the corpus budget whose result the native run checks */
pub fn cut_short<T>(run: &Result<T, VmErr>) -> bool {
    cfg!(miri) && matches!(run, Err(VmErr::Budget))
}

/* shared with child resolvers as the bridge shares its flat cache and in-flight set */
#[derive(Default)]
struct TestResolverState {
    modules: HashMap<String, Resolved>,
    /* keyed by directory so a walk up checks each parent of the importer */
    manifests: HashMap<String, Manifest>,
    in_flight: HashSet<String>,
}

pub struct TestResolver {
    state: Rc<RefCell<TestResolverState>>,
    in_flight_marker: Option<String>,
    dir: String, // where bare-name imports walk up from and empty for the entry script
}

impl Drop for TestResolver {
    fn drop(&mut self) {
        if let Some(canon) = self.in_flight_marker.take() {
            self.state.borrow_mut().in_flight.remove(&canon);
        }
    }
}

impl TestResolver {
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(TestResolverState::default())),
            in_flight_marker: None,
            dir: String::new(),
        }
    }

    pub fn with_native(self, spec: &str, bindings: Vec<NativeBinding>) -> Self {
        // split by export name convention as the real wasm host resolver does
        let (bindings, classes, consts) = partition_bindings(bindings);
        self.state.borrow_mut().modules.insert(
            spec.to_string(),
            Resolved::Native { bindings, classes, consts, canonical: spec.to_string() },
        );
        self
    }

    pub fn with_code(self, spec: &str, src: &str) -> Self {
        self.state.borrow_mut().modules.insert(
            spec.to_string(),
            Resolved::Code { src: src.to_string(), canonical: spec.to_string() },
        );
        self
    }

    /* adds an alias to the root manifest and calls accumulate */
    pub fn with_alias(self, name: &str, target: &str) -> Self {
        {
            let mut s = self.state.borrow_mut();
            let m = s.manifests.entry(String::new()).or_default();
            m.imports.push((name.to_string(), target.to_string()));
        }
        self
    }

    /* registers a manifest at `dir` where nearer manifests win for bare names */
    pub fn with_manifest(self, dir: &str, imports: &[(&str, &str)], extends: Option<&str>) -> Self {
        let imp = imports.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let m = Manifest { imports: imp, extends: extends.map(|s| s.to_string()), ..Manifest::default() };
        self.state.borrow_mut().manifests.insert(dir.to_string(), m);
        self
    }
}

impl Resolver for TestResolver {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        if !spec.contains('/') {
            let dir = self.dir.clone();
            return self.resolve_bare(spec, &dir);
        }
        let canonical = if spec.contains("://") || spec.starts_with('/') {
            spec.to_string()
        } else if spec.starts_with("./") || spec.starts_with("../") {
            join_relative(&self.dir, spec)
        } else {
            let root = self.manifest_root(spec)?;
            join_relative(&root, spec)
        };
        self.resolve_canonical(&canonical)
    }

    /* a transitive resolver sharing state under its own `dir` whose drop clears the in-flight mark */
    fn child(&self, spec: &str) -> Box<dyn Resolver> {
        let canon = spec.to_string();
        self.state.borrow_mut().in_flight.insert(canon.clone());
        Box::new(TestResolver {
            state: Rc::clone(&self.state),
            in_flight_marker: Some(canon),
            dir: dir_of(spec),
        })
    }
}

impl TestResolver {
    /* the nearest ancestor dir with a fixture manifest as the host probe finds it */
    fn manifest_root(&self, spec: &str) -> Result<String, String> {
        let s = self.state.borrow();
        for dir in walk_up_dirs(&self.dir) {
            if s.manifests.contains_key(&dir) {
                return Ok(dir);
            }
        }
        Err(format!("no edge.json above '{}' to resolve '{}'", self.dir, spec))
    }

    /* the nearest manifest above `start_dir` declaring `name` following `extends` without cycling */
    fn resolve_bare(&mut self, name: &str, start_dir: &str) -> Result<Resolved, String> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut search_dir = start_dir.to_string();
        let mut hops = 0u32;
        loop {
            if hops > 32 { return Err(format!("edge.json walk-up exceeded 32 hops resolving '{}'", name)); }
            hops += 1;
            let mut hit: Option<(String, Option<String>, Option<String>)> = None;
            for dir in walk_up_dirs(&search_dir) {
                let s = self.state.borrow();
                if let Some(m) = s.manifests.get(&dir) {
                    let target = m.imports.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
                    let ext = m.extends.clone();
                    drop(s);
                    hit = Some((dir, target, ext));
                    break;
                }
            }
            let Some((dir, target, ext)) = hit else {
                return Err(format!(
                    "module '{}' is not provided by this host and no edge.json declares it", name));
            };
            if let Some(target) = target {
                let canonical = join_relative(&dir, &target);
                return self.resolve_canonical(&canonical);
            }
            if let Some(ext) = ext {
                let m_spec = format!("{}edge.json", dir);
                if !visited.insert(m_spec) {
                    return Err("circular extends chain in edge.json".to_string());
                }
                let mut next = join_relative(&dir, &ext);
                if !next.ends_with('/') { next.push('/'); }
                search_dir = next;
                continue;
            }
            return Err(format!("module '{}' is not provided by this host and no edge.json declares it", name));
        }
    }

    fn resolve_canonical(&self, spec: &str) -> Result<Resolved, String> {
        let s = self.state.borrow();
        if s.in_flight.contains(spec) {
            return Err(format!("circular import: '{}'", spec));
        }
        match s.modules.get(spec) {
            // cloned so one module can be imported again under other aliases
            Some(r) => Ok(r.clone()),
            None => Err(format!("module '{}' not found in TestResolver", spec)),
        }
    }
}

/* exercises CallExtern dispatch and argument marshalling */
fn add(_: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    if args.len() != 2 { return Err(VmErr::Type("add: expected 2 args")); }
    let a = if args[0].is_int() { args[0].as_int() } else { return Err(VmErr::Type("add: arg 0 not int")); };
    let b = if args[1].is_int() { args[1].as_int() } else { return Err(VmErr::Type("add: arg 1 not int")); };
    Ok(Val::int(a + b))
}

/* checks nested calls when it wraps `add` */
fn square(_: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    if args.len() != 1 { return Err(VmErr::Type("square: expected 1 arg")); }
    let x = if args[0].is_int() { args[0].as_int() } else { return Err(VmErr::Type("square: arg not int")); };
    Ok(Val::int(x * x))
}

/* a heap string of length `n` allocated from inside an extern */
fn make_str(heap: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    if args.len() != 1 { return Err(VmErr::Type("make_str: expected 1 arg")); }
    let n = if args[0].is_int() { args[0].as_int() } else { return Err(VmErr::Type("make_str: arg not int")); };
    let s: String = "x".repeat(n.max(0) as usize);
    heap.alloc(HeapObj::Str(s))
}

/* each call sees the count the one before it left */
fn counter(_: &mut HeapPool, _args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    use std::sync::atomic::{AtomicI64, Ordering};
    static N: AtomicI64 = AtomicI64::new(0);
    Ok(Val::int(N.fetch_add(1, Ordering::SeqCst)))
}

/* for tests that only check the extern was called */
fn const_42(_: &mut HeapPool, _args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    Ok(Val::int(42))
}

/* always fails to exercise how extern errors propagate through dispatch */
fn boom(_: &mut HeapPool, _args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    Err(VmErr::Runtime("boom from extern"))
}

/* a float round trip through an extern with no int coercion */
fn double_f(_: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    if args.len() != 1 || !args[0].is_float() {
        return Err(VmErr::Type("double_f: expected one float arg"));
    }
    Ok(Val::float(args[0].as_float() * 2.0))
}

/* bool tags survive the extern dispatch */
fn negate(_: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    if args.len() != 1 || !args[0].is_bool() {
        return Err(VmErr::Type("negate: expected one bool arg"));
    }
    Ok(Val::bool(!args[0].as_bool()))
}

/* exercises the PendingHostCall yield path through `call_extern` */
fn host_defer(_: &mut HeapPool, _args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    Err(VmErr::HostCallDeferred)
}

/* a zero-arg export materialised at init and bound as a module attribute */
fn const_pi(_: &mut HeapPool, _args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    Ok(Val::float(core::f64::consts::PI))
}

/* mixed argument types check that each one decodes on its own */
fn pick(_: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    if args.len() != 3 || !args[0].is_bool() || !args[1].is_int() || !args[2].is_int() {
        return Err(VmErr::Type("pick: expected (bool, int, int)"));
    }
    Ok(if args[0].as_bool() { args[2] } else { args[1] })
}

/* `Box(v)` stores `v` on self and `get` reads it back through the self convention */
fn class_box_init(heap: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    let [inst, v] = args else { return Err(VmErr::Type("Box: expected (self, value)")); };
    let Some(attrs) = heap.fields(*inst) else {
        return Err(VmErr::Type("Box: self is not an instance"));
    };
    let attrs = attrs.clone();
    let key = heap.alloc(HeapObj::Str("v".into()))?;
    attrs.borrow_mut().insert(key, *v, heap);
    Ok(Val::none())
}

fn class_box_get(heap: &mut HeapPool, args: &[Val], _kw: Option<Val>) -> Result<Val, VmErr> {
    let [inst] = args else { return Err(VmErr::Type("Box.get: expected (self)")); };
    let Some(attrs) = heap.fields(*inst) else {
        return Err(VmErr::Type("Box.get: self is not an instance"));
    };
    let attrs = attrs.clone();
    let key = heap.alloc(HeapObj::Str("v".into()))?;
    let out = attrs.borrow().get(&key, heap).copied();
    out.ok_or(VmErr::Attribute("Box.get: 'v' not set".into()))
}

/* the runner turns each fixture into a NativeBinding */
type NativeFn = fn(&mut HeapPool, &[Val], Option<Val>) -> Result<Val, VmErr>;

pub fn test_native(name: &str) -> Option<NativeBinding> {
    let func: NativeFn = match name {
        "add" => add,
        "square" => square,
        "make_str" => make_str,
        "counter" => counter,
        "const_42" => const_42,
        "boom" => boom,
        "double_f" => double_f,
        "negate" => negate,
        "pick" => pick,
        "host_defer" => host_defer,
        "__const_pi" => const_pi,
        "__class_Box___init__" => class_box_init,
        "__class_Box_get" => class_box_get,
        // registered under a builtin name to check imported natives shadow builtins
        "abs" => const_42,
        _ => return None,
    };
    Some(NativeBinding::from_fn(name, func))
}
