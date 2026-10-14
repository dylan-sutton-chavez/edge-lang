use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::s;
use crate::util::hash::{FxHashMap, FxHashSet};
use super::bundle::Bundle;
use super::lock::{self, Lock};
use super::{dir_of, join_relative, parent_dir, parse_integrity, parse_manifest, rules, scan, walk_up_dirs, ImportSpec, Manifest};

// How deep importers may nest before a chain of grants is cut, far past any real tree.
const MAX_DEPTH: usize = 64;

/* Each section from the root to a package, beside the key the next comes in under. */
pub type Chain = Vec<(Vec<(String, Vec<String>)>, String)>;

/* What the walk needs next, from its host or from the runtime it registers into. */
pub enum Step {
    /* The bytes behind a spec, its #sha256- pin still on it, answered with `fetched`. */
    Fetch(String),
    /* A plugin for the host to instantiate and register, answered with `loaded`. */
    Plugin { spec: String, name: String, bytes: Vec<u8> },
    Code { spec: String, src: String },
    /* A spec whose import fails, with why. */
    Refuse { spec: String, msg: String },
    /* A manifest as the compiler resolves names through it, its versions already locked. */
    Manifest { spec: String, manifest: Manifest },
    /* Every package met, for the host to serve its system modules, answered with `served`. */
    System(Packages),
    /* Bare names no manifest declares, for the host to say which a registry has, answered with `known`. */
    Undeclared(Vec<String>),
    /* Every failure, none when everything registered. */
    Done(Vec<String>),
}

/* Who the modules belong to and what the root grants, what serving the system modules takes. */
pub struct Packages {
    // Every manifest dir read, sorted, with the name its package answers to and its grants.
    pub dirs: Vec<(String, String, Chain)>,
    // The root manifest's dir, where a malformed grant is reported.
    pub root: String,
    pub permissions: Vec<(String, Vec<String>)>,
    // Whether anything can reach a system module, a name no manifest declared or a plugin.
    pub needed: bool,
}

/* How the host answered a fetch. */
pub enum Fetched {
    Bytes(Vec<u8>),
    /* Nothing there, with the host's own explanation when it has a better one. */
    Missing(Option<String>),
    Failed(String),
}

/* How the host answered a plugin. */
pub enum Loaded {
    Ok,
    Failed(String),
    /* This host loads no plugin here, so the import fails with why. */
    Refused(String),
}

enum Waiting {
    Module(String),
    Package(String),
    Manifest(String),
    Lock { spec: String, manifest: Manifest },
    Plugin(String),
    System,
    Undeclared(Vec<String>),
}

/* The resolution walk both hosts drive, lazy so only what a program imports is ever fetched. */
pub struct Walk {
    entry_dir: String,
    system: Vec<String>,
    visited: FxHashSet<String>,
    queue: VecDeque<String>,
    failures: Vec<String>,
    // Bare names waiting on the manifest of their importer, then the ones no manifest declares.
    pending_bare: Vec<(String, String, Option<String>)>,
    // Root-relative imports waiting on their importer's manifest chain.
    pending_root: Vec<(String, String, Option<String>)>,
    manifest_dirs: FxHashSet<String>,
    // The same dirs in one spelling, the identity of a package.
    homes: FxHashSet<String>,
    missing: FxHashSet<String>,
    // Spec to the name its first importer wrote and that importer's own name, None for the entry.
    origins: FxHashMap<String, (String, Option<String>)>,
    // Files of every imported package, keyed under the package spec it came in as.
    mounted: FxHashMap<String, Vec<u8>>,
    // Each manifest dir to its joined imports and the dir it extends, spelled like the compiler.
    imports: FxHashMap<String, Vec<(String, String)>>,
    extends: FxHashMap<String, String>,
    grants: FxHashMap<String, Vec<(String, Vec<String>)>>,
    // Each manifest as the compiler reads it, so a copy can point one import elsewhere.
    manifests: FxHashMap<String, Manifest>,
    // Every bare import resolved, with the manifest dir, the name and the spec it reached.
    edges: Vec<(String, String, String)>,
    // Each copy to the package it copies, both without the closing slash.
    copies: Vec<(String, String)>,
    plugins: bool,
    // Whether some code runs text through eval or exec, whose imports reach the system modules its package holds.
    runs_text: bool,
    out: VecDeque<Step>,
    waiting: Option<Waiting>,
    done: bool,
}

impl Walk {
    /* Starts from the entry source, `system` naming the modules this host serves, which no import may shadow. */
    pub fn new(root_src: &str, entry_dir: &str, system: Vec<String>) -> Walk {
        let mut walk = Walk {
            entry_dir: entry_dir.to_string(),
            system,
            visited: FxHashSet::default(),
            queue: VecDeque::new(),
            failures: Vec::new(),
            pending_bare: Vec::new(),
            pending_root: Vec::new(),
            manifest_dirs: FxHashSet::default(),
            homes: FxHashSet::default(),
            missing: FxHashSet::default(),
            origins: FxHashMap::default(),
            mounted: FxHashMap::default(),
            imports: FxHashMap::default(),
            extends: FxHashMap::default(),
            grants: FxHashMap::default(),
            manifests: FxHashMap::default(),
            edges: Vec::new(),
            copies: Vec::new(),
            plugins: false,
            runs_text: false,
            out: VecDeque::new(),
            waiting: None,
            done: false,
        };
        let (imports, runs_text) = scan(root_src);
        walk.runs_text = runs_text;
        for imp in imports {
            walk.enqueue_import(imp, entry_dir, None);
        }
        walk.enqueue_manifest_chain(entry_dir);
        walk
    }

    /* The next step, called again after answering whatever the last one asked. */
    pub fn step(&mut self) -> Step {
        loop {
            if let Some(step) = self.out.pop_front() {
                return step;
            }
            if self.done {
                return Step::Done(core::mem::take(&mut self.failures));
            }
            let Some(spec) = self.queue.pop_front() else {
                if self.settle() {
                    continue;
                }
                self.waiting = Some(Waiting::System);
                return Step::System(self.packages());
            };
            if self.visited.insert(spec.clone()) {
                self.visit(spec);
            }
        }
    }

    pub fn fetched(&mut self, answer: Fetched) {
        match self.waiting.take() {
            Some(Waiting::Manifest(spec)) => self.manifest(spec, answer),
            Some(Waiting::Lock { spec, manifest }) => self.lock(spec, manifest, answer),
            Some(Waiting::Package(spec)) => self.package(spec, answer),
            Some(Waiting::Module(spec)) => self.module(spec, answer),
            other => self.waiting = other,
        }
    }

    pub fn loaded(&mut self, answer: Loaded) {
        let Some(Waiting::Plugin(spec)) = self.waiting.take() else { return };
        match answer {
            // A plugin sits in a package too, so its manifest chain names who it belongs to.
            Loaded::Ok => {
                self.plugins = true;
                let dir = dir_of(&spec);
                self.enqueue_manifest_chain(&dir);
            }
            Loaded::Failed(e) => self.failures.push(e),
            Loaded::Refused(reason) => self.refuse(&spec, &reason),
        }
    }

    /* The host served the system modules, and a bare name still undeclared is asked about before it fails. */
    pub fn served(&mut self, failures: Vec<String>) {
        if !matches!(self.waiting.take(), Some(Waiting::System)) {
            return;
        }
        self.failures.extend(failures);
        let mut seen = FxHashSet::default();
        let names: Vec<String> = core::mem::take(&mut self.pending_bare).into_iter().map(|(name, _, _)| name).filter(|name| seen.insert(name.clone())).collect();
        if names.is_empty() {
            self.done = true;
            return;
        }
        self.waiting = Some(Waiting::Undeclared(names.clone()));
        self.out.push_back(Step::Undeclared(names));
    }

    /* Each undeclared name fails at its import, pointing at `edge add` when the registry has it. */
    pub fn known(&mut self, registered: Vec<String>) {
        let Some(Waiting::Undeclared(names)) = self.waiting.take() else { return };
        for name in names {
            let help = match registered.contains(&name) {
                true => s!("run `edge add ", str &name, "`"),
                false => s!("declare it in edge.json, or use a relative import"),
            };
            let msg = s!("module '", str &name, "' is not provided by this host and no edge.json declares it\nhelp: ", str &help);
            self.out.push_back(Step::Refuse { spec: name, msg });
        }
        self.done = true;
    }

    fn visit(&mut self, spec: String) {
        if spec.ends_with("edge.json") {
            let waiting = Waiting::Manifest(spec.clone());
            return self.request(spec, waiting);
        }
        match extension(&spec) {
            "edge" => {
                let waiting = Waiting::Package(spec.clone());
                self.request(spec, waiting)
            }
            "js" | "mjs" => {
                let name = self.label(&spec);
                self.out.push_back(Step::Refuse { msg: s!("module '", str &name, "' is JavaScript, ship a .py or a .wasm"), spec });
            }
            "so" | "dylib" => self.refuse(&spec, "is not supported, ship a .wasm"),
            _ => {
                let waiting = Waiting::Module(spec.clone());
                self.request(spec, waiting)
            }
        }
    }

    /* Bytes a mounted package holds answer at once, anything else is asked of the host. */
    fn request(&mut self, spec: String, waiting: Waiting) {
        self.waiting = Some(waiting);
        let held = match parse_integrity(&spec) {
            Ok((target, _)) => self.mounted.get(target).cloned().map(Fetched::Bytes),
            Err(e) => Some(Fetched::Failed(e)),
        };
        match held {
            Some(answer) => self.fetched(answer),
            None => self.out.push_back(Step::Fetch(self.real(&spec))),
        }
    }

    fn manifest(&mut self, spec: String, answer: Fetched) {
        let bytes = match answer {
            Fetched::Bytes(bytes) => bytes,
            // The root always has a manifest, so the program's own code is the package `main` even with none on disk.
            Fetched::Missing(_) if spec == "edge.json" => b"{}".to_vec(),
            Fetched::Missing(_) => {
                self.missing.insert(spec);
                self.retry_pending();
                return self.retry_root();
            }
            Fetched::Failed(e) => return self.failures.push(e),
        };
        let manifest = match parse_manifest(&bytes) {
            Ok(m) => m,
            Err(e) => return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e)),
        };
        // A package says the lowest engine it runs on, so one written for a later one is refused where it loads.
        let system: Vec<&str> = self.system.iter().map(String::as_str).collect();
        let refused = rules::floor_error(&manifest).or_else(|| manifest.imports.iter().find_map(|(name, target)| rules::import_error(name, target, &system)));
        if let Some(e) = refused {
            return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e));
        }
        let dir = dir_of(&spec);
        self.manifest_dirs.insert(dir.clone());
        self.homes.insert(norm(&dir).to_string());
        if let Some(grants) = &manifest.permissions {
            self.grants.insert(norm(&dir).to_string(), grants.clone());
        }
        if manifest.imports.iter().any(|(_, target)| lock::needs_lock(target)) {
            let beside = s!(str &dir, str lock::FILE);
            return self.request(beside, Waiting::Lock { spec, manifest });
        }
        self.merge(spec, manifest, None);
    }

    fn lock(&mut self, spec: String, manifest: Manifest, answer: Fetched) {
        let held = match answer {
            Fetched::Bytes(bytes) => match Lock::parse(&bytes) {
                Ok(lock) => Some(lock),
                Err(e) => return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e)),
            },
            Fetched::Missing(_) => None,
            Fetched::Failed(e) => return self.failures.push(e),
        };
        self.merge(spec, manifest, held.as_ref());
    }

    /* Every version it declares becomes the url and digest its lock holds, before a name or the compiler sees it. */
    fn merge(&mut self, spec: String, manifest: Manifest, held: Option<&Lock>) {
        let mut resolved = Vec::with_capacity(manifest.imports.len());
        for (name, target) in &manifest.imports {
            match lock::locked_spec(name, target, held) {
                Ok(target) => resolved.push((name.clone(), target)),
                Err(e) => return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e)),
            }
        }
        let dir = dir_of(&spec);
        self.imports.insert(dir.clone(), resolved.iter().map(|(name, target)| (name.clone(), join_relative(&dir, target))).collect());
        let extends = manifest.extends.clone();
        let manifest = Manifest { imports: resolved, ..manifest };
        self.manifests.insert(spec.clone(), manifest.clone());
        self.out.push_back(Step::Manifest { spec, manifest });
        if let Some(ext) = extends {
            let mut next = join_relative(&dir, &ext);
            if !next.ends_with('/') {
                next.push('/');
            }
            self.queue.push_back(s!(str &next, "edge.json"));
            self.extends.insert(dir, next);
        }
        self.retry_pending();
        self.retry_root();
    }

    /* A published package, verified whole, then its entry runs as the module and its other files answer from inside it. */
    fn package(&mut self, spec: String, answer: Fetched) {
        let Some(bytes) = self.pinned(&spec, answer, "package") else { return };
        let bundle = match Bundle::decode(&bytes) {
            Ok(bundle) => bundle,
            Err(e) => return self.failures.push(s!("package '", str target(&spec), "' is not a packed .edge, ", str &e)),
        };
        let base = dir_of(&spec);
        let entry = s!(str &base, str &bundle.entry);
        for file in bundle.files {
            self.mounted.insert(s!(str &base, str &file.path), file.bytes);
        }
        match self.mounted.get(&entry).cloned() {
            Some(code) => self.code(spec, code),
            None => self.failures.push(s!("package '", str target(&spec), "' names an entry it does not carry")),
        }
    }

    // A .wasm spec is a plugin, past .py the wasm magic marks one too.
    fn module(&mut self, spec: String, answer: Fetched) {
        let Some(bytes) = self.pinned(&spec, answer, "module") else { return };
        let ext = extension(&spec);
        if ext == "wasm" || (ext != "py" && bytes.starts_with(b"\0asm")) {
            let name = self.label(&spec);
            self.waiting = Some(Waiting::Plugin(spec.clone()));
            return self.out.push_back(Step::Plugin { spec, name, bytes });
        }
        self.code(spec, bytes);
    }

    /* The bytes a fetch answered with, held to the spec's pin, None once a failure explains why there are none. */
    fn pinned(&mut self, spec: &str, answer: Fetched, what: &str) -> Option<Vec<u8>> {
        let bytes = match answer {
            Fetched::Bytes(bytes) => bytes,
            Fetched::Missing(hint) => {
                self.failures.push(hint.unwrap_or_else(|| s!("could not read ", str what, " '", str target(spec), "'")));
                return None;
            }
            Fetched::Failed(e) => {
                self.failures.push(e);
                return None;
            }
        };
        match lock::verify_pin(spec, &bytes) {
            Ok(()) => Some(bytes),
            Err(e) => {
                self.failures.push(e);
                None
            }
        }
    }

    // A code module registers, then its own imports queue so transitive deps stay lazy.
    fn code(&mut self, spec: String, bytes: Vec<u8>) {
        let src = String::from_utf8_lossy(&bytes).into_owned();
        let dir = dir_of(&spec);
        let via = self.origins.get(&spec).map(|(name, _)| name.clone());
        let (imports, runs_text) = scan(&src);
        self.runs_text |= runs_text;
        for imp in imports {
            self.enqueue_import(imp, &dir, via.as_deref());
        }
        self.enqueue_manifest_chain(&dir);
        self.out.push_back(Step::Code { spec, src });
    }

    /* Why a module cannot load here, raised at its import and named as its importer wrote it. */
    fn refuse(&mut self, spec: &str, reason: &str) {
        let (name, via) = self.origins.get(spec).cloned().unwrap_or_else(|| (target(spec).to_string(), None));
        let via = via.map(|v| s!(" (via ", str &v, ")")).unwrap_or_default();
        self.out.push_back(Step::Refuse { spec: spec.to_string(), msg: s!("module '", str &name, "' ", str reason, str &via) });
    }

    fn label(&self, spec: &str) -> String {
        self.origins.get(spec).map_or_else(|| target(spec).to_string(), |(name, _)| name.clone())
    }

    fn packages(&self) -> Packages {
        let root = match self.root_for(&self.entry_dir) {
            Some(Some(root)) => norm(&root).to_string(),
            _ => String::new(),
        };
        let owners = self.owners();
        let mut dirs: Vec<String> = self.manifest_dirs.iter().cloned().collect();
        dirs.sort();
        let dirs = dirs.into_iter().map(|dir| {
            let (name, chain) = self.reach(norm(&dir), &root, &owners, 0);
            (dir, name, chain)
        }).collect();
        let permissions = self.grants.get(&root).cloned().unwrap_or_default();
        Packages { dirs, permissions, needed: !self.pending_bare.is_empty() || self.plugins || self.runs_text, root }
    }

    /* The name a package dir answers to and the grants reaching it, importer by importer. */
    fn reach(&self, dir: &str, root: &str, owners: &FxHashMap<String, (String, String)>, depth: usize) -> (String, Chain) {
        let section = |at: &str| self.grants.get(at).cloned().unwrap_or_default();
        if dir == root {
            return (s!("main"), vec![(section(root), s!("main"))]);
        }
        if depth < MAX_DEPTH
            && let Some((from, key)) = owners.get(dir)
        {
            // The root grants its imports outright, any other importer passes on only what it holds.
            let mut chain = if from == root { Vec::new() } else { self.reach(from, root, owners, depth + 1).1 };
            chain.push((section(from), key.clone()));
            return (key.clone(), chain);
        }
        // Reached by path and no key, it holds what the package around it holds.
        match self.enclosing(dir) {
            Some(outer) if depth < MAX_DEPTH => self.reach(&outer, root, owners, depth + 1),
            _ => (dir.to_string(), Vec::new()),
        }
    }

    /* Each package dir to the manifest dir and key that first import it. */
    fn owners(&self) -> FxHashMap<String, (String, String)> {
        let mut owners = FxHashMap::default();
        for (from, key, spec) in &self.edges {
            if let Some(home) = self.landing(from, spec) {
                owners.entry(home).or_insert_with(|| (norm(from).to_string(), key.clone()));
            }
        }
        owners
    }

    /* Copies a shared package for each importer but the first, true while that queued work. */
    fn settle(&mut self) -> bool {
        self.retry_pending();
        self.retry_root();
        let owners = self.owners();
        let shared: Vec<(usize, String)> = self.edges.iter().enumerate().filter_map(|(i, (from, key, spec))| {
            let home = self.landing(from, spec)?;
            let (owner, named) = &owners[&home];
            (owner != norm(from) || named != key).then_some((i, home))
        }).collect();
        for (i, home) in shared {
            if !self.cycles(&self.edges[i].0, &home, &owners) {
                self.copy(i, &home);
            }
        }
        !self.queue.is_empty() || !self.out.is_empty()
    }

    /* Points one import at a copy beside the original, so relative imports reach the same files. */
    fn copy(&mut self, i: usize, home: &str) {
        let (from, key, spec) = self.edges[i].clone();
        let n = self.copies.len() + 1;
        let original = home.trim_end_matches('/');
        let copy = match (parent_dir(home), original.rsplit_once('/')) {
            (Some(_), Some((parent, last))) => s!(str parent, "/.edge-copy-", int n, "-", str last),
            (Some(_), None) => s!(".edge-copy-", int n, "-", str original),
            (None, _) => s!(str original, "/.edge-copy-", int n),
        };
        let Some(rest) = norm(&spec).strip_prefix(original) else { return };
        let rebased = s!(str &copy, str rest);
        self.copies.push((copy, original.to_string()));
        let written = written_from(&from, &rebased);
        let spec = join_relative(&from, &written);
        set(self.imports.entry(from.clone()).or_default(), &key, spec.clone());
        let m_spec = s!(str &from, "edge.json");
        if let Some(manifest) = self.manifests.get_mut(&m_spec) {
            set(&mut manifest.imports, &key, written);
            let manifest = manifest.clone();
            self.out.push_back(Step::Manifest { spec: m_spec, manifest });
        }
        let via = self.origins.get(&self.edges[i].2).and_then(|(_, via)| via.clone());
        self.edges[i].2 = spec.clone();
        self.enqueue(spec, key, via);
    }

    /* Where an import from `from` lands, None inside that package or one around it. */
    fn landing(&self, from: &str, spec: &str) -> Option<String> {
        let home = norm(&self.root_for(&dir_of(spec))??).to_string();
        let around = walk_up_dirs(norm(from)).any(|d| d == home);
        (!around).then_some(home)
    }

    /* Whether `home` already holds the importer or one above it, a cycle copies would never end. */
    fn cycles(&self, from: &str, home: &str, owners: &FxHashMap<String, (String, String)>) -> bool {
        let home = self.real(home);
        let mut at = norm(from).to_string();
        for _ in 0..MAX_DEPTH {
            if self.real(&at) == home {
                return true;
            }
            at = match owners.get(&at) {
                Some((parent, _)) => parent.clone(),
                None => match self.enclosing(&at) {
                    Some(outer) => outer,
                    None => return false,
                },
            };
        }
        true
    }

    /* The nearest manifest dir above `dir`, the package whose code reached it by path. */
    fn enclosing(&self, dir: &str) -> Option<String> {
        walk_up_dirs(dir).skip(1).find(|d| self.homes.contains(d.as_str()))
    }

    /* Where the bytes of `spec` live, a copy read from the package it copies. */
    fn real(&self, spec: &str) -> String {
        let mut at = spec.to_string();
        for (copy, original) in self.copies.iter().rev() {
            let next = norm(&at).strip_prefix(copy.as_str()).filter(|rest| rest.is_empty() || rest.starts_with(['/', '#'])).map(|rest| s!(str original, str rest));
            if let Some(next) = next {
                at = next;
            }
        }
        at
    }

    // Queues a module spec, the first importer to reach it names it in refusals.
    fn enqueue(&mut self, spec: String, name: String, via: Option<String>) {
        self.origins.entry(spec.clone()).or_insert((name, via));
        self.queue.push_back(spec);
    }

    fn enqueue_import(&mut self, imp: ImportSpec, dir: &str, via: Option<&str>) {
        let via = via.map(String::from);
        match imp {
            ImportSpec::Relative(path) => self.enqueue(join_relative(dir, &path), path, via),
            ImportSpec::Root(path) => self.enqueue_root(path, dir.to_string(), via),
            ImportSpec::Bare(name) => self.enqueue_bare(name, dir.to_string(), via),
        }
    }

    // A bare name waits on the manifest of its importer, then its import is kept.
    fn enqueue_bare(&mut self, name: String, dir: String, via: Option<String>) {
        let Some(Some((from, spec))) = self.declared(&name, &dir) else {
            return self.pending_bare.push((name, dir, via));
        };
        if !self.edges.iter().any(|(at, key, _)| *at == from && *key == name) {
            self.edges.push((from, name.clone(), spec.clone()));
        }
        self.enqueue(spec, name, via);
    }

    /* Where a bare name from `dir` leads, through the nearest manifest and what it extends. */
    fn declared(&self, name: &str, dir: &str) -> Option<Option<(String, String)>> {
        let Some(from) = self.root_for(dir)? else { return Some(None) };
        let mut at = from.clone();
        // Bounded like the compiler, so an extends loop ends.
        for _ in 0..32 {
            if let Some((_, spec)) = self.imports.get(&at)?.iter().find(|(key, _)| key == name) {
                return Some(Some((from, spec.clone())));
            }
            match self.extends.get(&at) {
                Some(next) if !self.missing.contains(&s!(str next, "edge.json")) => at = next.clone(),
                _ => return Some(None),
            }
        }
        Some(None)
    }

    fn retry_pending(&mut self) {
        for (name, dir, via) in core::mem::take(&mut self.pending_bare) {
            self.enqueue_bare(name, dir, via);
        }
    }

    // Probes every ancestor manifest, the same walk-up the compiler resolves through.
    fn enqueue_manifest_chain(&mut self, dir: &str) {
        for d in walk_up_dirs(dir) {
            let m = s!(str &d, "edge.json");
            if !self.missing.contains(&m) {
                self.queue.push_back(m);
            }
        }
    }

    /* Nearest manifest dir at or above `dir`, None while probes pend, Some(None) once probed bare. */
    fn root_for(&self, dir: &str) -> Option<Option<String>> {
        for d in walk_up_dirs(dir) {
            if self.manifest_dirs.contains(&d) {
                return Some(Some(d));
            }
            let m = s!(str &d, "edge.json");
            if !self.visited.contains(&m) && !self.missing.contains(&m) {
                return None;
            }
        }
        Some(None)
    }

    fn enqueue_root(&mut self, spec: String, dir: String, via: Option<String>) {
        match self.root_for(&dir) {
            None => self.pending_root.push((spec, dir, via)),
            Some(Some(root)) => self.enqueue(join_relative(&root, &spec), spec, via),
            // No manifest anywhere, the compiler reports it.
            Some(None) => {}
        }
    }

    fn retry_root(&mut self) {
        for (spec, dir, via) in core::mem::take(&mut self.pending_root) {
            self.enqueue_root(spec, dir, via);
        }
    }
}

/* Points `key` at `target`, added when the manifest only reached it through what it extends. */
fn set(imports: &mut Vec<(String, String)>, key: &str, target: String) {
    match imports.iter_mut().find(|(name, _)| name == key) {
        Some((_, at)) => *at = target,
        None => imports.push((key.to_string(), target)),
    }
}

/* `spec` as a manifest in `dir` writes it, since the compiler joins targets to that dir. */
fn written_from(dir: &str, spec: &str) -> String {
    if spec.contains("://") || spec.starts_with('/') {
        return spec.to_string();
    }
    let from: Vec<&str> = norm(dir).split('/').filter(|part| !part.is_empty()).collect();
    let to: Vec<&str> = norm(spec).split('/').collect();
    let common = from.iter().zip(&to[..to.len() - 1]).take_while(|(a, b)| a == b).count();
    let rest = to[common..].join("/");
    match from.len() - common {
        0 => s!("./", str &rest),
        up => s!(str &"../".repeat(up), str &rest),
    }
}

/* A dir as one spelling, since `./lib/` and `lib/` are the same place under the project. */
fn norm(dir: &str) -> &str {
    dir.trim_start_matches("./")
}

fn target(spec: &str) -> &str {
    spec.split_once('#').map_or(spec, |(t, _)| t)
}

/* The extension of the last path segment, query and fragment stripped, it picks how a module loads. */
fn extension(spec: &str) -> &str {
    let path = spec.split(['?', '#']).next().unwrap_or(spec);
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or("", |(_, ext)| ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::bundle::Entry;
    use alloc::vec;

    /* Everything a walk handed its host and its runtime, driven over an in-memory tree. */
    #[derive(Default)]
    struct Seen {
        code: Vec<String>,
        refused: Vec<(String, String)>,
        manifests: Vec<(String, Manifest)>,
        fetched: Vec<String>,
        plugins: Vec<String>,
        packages: Option<Packages>,
        failures: Vec<String>,
    }

    fn walk(files: &[(&str, Vec<u8>)], src: &str) -> Seen {
        let mut walk = Walk::new(src, "", vec![s!("net"), s!("time")]);
        let mut seen = Seen::default();
        loop {
            match walk.step() {
                Step::Fetch(spec) => {
                    seen.fetched.push(spec.clone());
                    let found = files.iter().find(|(path, _)| *path == target(&spec).trim_start_matches("./")).map(|(_, bytes)| bytes.clone());
                    walk.fetched(found.map_or(Fetched::Missing(None), Fetched::Bytes));
                }
                Step::Plugin { spec, .. } => {
                    seen.plugins.push(spec);
                    walk.loaded(Loaded::Ok);
                }
                Step::Code { spec, .. } => seen.code.push(spec),
                Step::Refuse { spec, msg } => seen.refused.push((spec, msg)),
                Step::Manifest { spec, manifest } => seen.manifests.push((spec, manifest)),
                Step::System(packages) => {
                    seen.packages = Some(packages);
                    walk.served(Vec::new());
                }
                // A registry that has json and nothing else.
                Step::Undeclared(names) => walk.known(names.into_iter().filter(|name| name == "json").collect()),
                Step::Done(failures) => {
                    seen.failures = failures;
                    return seen;
                }
            }
        }
    }

    fn file(path: &'static str, text: &str) -> (&'static str, Vec<u8>) {
        (path, text.as_bytes().to_vec())
    }

    /* The name a package dir answers to and the key of each link of grants that reaches it. */
    fn grants(seen: &Seen, dir: &str) -> (String, Vec<String>) {
        let packages = seen.packages.as_ref().unwrap();
        let (_, name, chain) = packages.dirs.iter().find(|(at, _, _)| at == dir).unwrap_or_else(|| panic!("no package at {dir}: {:?}", packages.dirs.iter().map(|(at, _, _)| at).collect::<Vec<_>>()));
        (name.clone(), chain.iter().map(|(_, key)| key.clone()).collect())
    }

    fn named(name: &str, keys: &[&str]) -> (String, Vec<String>) {
        (name.to_string(), keys.iter().map(|key| key.to_string()).collect())
    }

    #[test]
    fn a_declared_version_resolves_through_its_lock_to_pinned_bytes() {
        let dep = b"def shout(w):\n    return w.upper()\n".to_vec();
        let digest = lock::digest_of(&dep);
        let lock_text = s!("{ \"greet\": { \"version\": \"0.1.0\", \"url\": \"https://cdn.test/greet.py\", \"digest\": \"", str &digest, "\" } }");
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "greet": "0.1.0" } }"#),
            ("edge.lock", lock_text.into_bytes()),
            ("https://cdn.test/greet.py", dep),
        ], "from greet import shout\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        let pinned = s!("https://cdn.test/greet.py#", str &digest);
        assert_eq!(seen.code, vec![pinned.clone()]);
        let root = &seen.manifests.iter().find(|(spec, _)| spec == "edge.json").unwrap().1;
        assert_eq!(root.imports, vec![(s!("greet"), pinned)]);
    }

    #[test]
    fn bytes_that_break_their_pin_fail_the_walk() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "lib": "./lib.py#sha256-a4bf0317b809477e6e475cd791573953d3ae05c4e2b03bd5f35199609557dd90" } }"#),
            file("lib.py", "x = 1\n"),
        ], "import lib\n");
        assert!(seen.failures.iter().any(|f| f.starts_with("integrity check failed for './lib.py'")), "{:?}", seen.failures);
    }

    #[test]
    fn a_package_answers_its_own_files_and_carries_its_plugin() {
        let bundle = Bundle {
            entry: s!("main.py"),
            files: vec![
                Entry { path: s!("main.py"), bytes: b"from _fast import go\n".to_vec() },
                Entry { path: s!("edge.json"), bytes: br#"{ "name": "fast", "imports": { "_fast": "./fast.wasm" } }"#.to_vec() },
                Entry { path: s!("fast.wasm"), bytes: b"\0asm\x01\0\0\0".to_vec() },
            ],
        }.encode();
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "fast": "https://cdn.test/pkg/fast/0.1.0/app.edge" } }"#),
            ("https://cdn.test/pkg/fast/0.1.0/app.edge", bundle),
        ], "import fast\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(seen.plugins, vec![s!("https://cdn.test/pkg/fast/0.1.0/app.edge/fast.wasm")]);
        assert!(!seen.fetched.iter().any(|f| f.contains("app.edge/")), "files inside a package never reach the host: {:?}", seen.fetched);
        assert!(seen.packages.as_ref().unwrap().needed);
        assert_eq!(grants(&seen, "https://cdn.test/pkg/fast/0.1.0/app.edge/"), named("fast", &["fast"]));
    }

    // The name a manifest gives itself never counts, so `other` cannot pose as `analytics`.
    #[test]
    fn a_package_is_granted_through_the_keys_that_import_it() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "analytics": "./analytics/main.py", "other": "./other/main.py" }, "permissions": { "analytics": ["secret:KEY"] } }"#),
            file("analytics/edge.json", r#"{ "name": "analytics", "imports": { "trace": "./trace/main.py" }, "permissions": { "trace": ["secret:KEY"] } }"#),
            file("analytics/main.py", "import trace\n"),
            file("analytics/trace/edge.json", r#"{ "name": "trace" }"#),
            file("analytics/trace/main.py", ""),
            file("other/edge.json", r#"{ "name": "analytics" }"#),
            file("other/main.py", "from .inner.main import x\n"),
            file("other/inner/edge.json", r#"{ "name": "analytics" }"#),
            file("other/inner/main.py", "x = 1\n"),
        ], "import analytics\nimport other\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, ""), named("main", &["main"]));
        assert_eq!(grants(&seen, "./analytics/"), named("analytics", &["analytics"]));
        assert_eq!(grants(&seen, "./analytics/trace/"), named("trace", &["analytics", "trace"]));
        assert_eq!(grants(&seen, "./other/"), named("other", &["other"]));
        // Reached by path, so it holds what the package around it holds.
        assert_eq!(grants(&seen, "./other/inner/"), named("other", &["other"]));
        let packages = seen.packages.unwrap();
        let (_, _, chain) = packages.dirs.iter().find(|(at, _, _)| at == "./analytics/trace/").unwrap();
        assert_eq!(chain[1].0, vec![(s!("trace"), vec![s!("secret:KEY")])]);
    }

    // Two importers of one package each get a copy, so each copy holds only what its own importer passes.
    #[test]
    fn a_package_two_importers_share_loads_once_for_each() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.py", "shop": "./shop/main.py" } }"#),
            file("pay/edge.json", r#"{ "imports": { "vault": "../vault/main.py" }, "permissions": { "vault": ["secret:PAY"] } }"#),
            file("pay/main.py", "import vault\n"),
            file("shop/edge.json", r#"{ "imports": { "vault": "../vault/main.py" }, "permissions": { "vault": ["secret:SHOP"] } }"#),
            file("shop/main.py", "import vault\n"),
            file("vault/edge.json", "{}"),
            file("vault/main.py", "from .util import x\n"),
            file("vault/util.py", "x = 1\n"),
        ], "import pay\nimport shop\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, "./vault/"), named("vault", &["pay", "vault"]));
        assert_eq!(grants(&seen, "./.edge-copy-1-vault/"), named("vault", &["shop", "vault"]));
        for spec in ["./vault/main.py", "./vault/util.py", "./.edge-copy-1-vault/main.py", "./.edge-copy-1-vault/util.py"] {
            assert!(seen.code.contains(&s!(str spec)), "{spec} never loaded: {:?}", seen.code);
        }
        assert!(!seen.fetched.iter().any(|f| f.contains(".edge-copy")), "a copy reads the original: {:?}", seen.fetched);
        let shop = &seen.manifests.iter().rev().find(|(spec, _)| spec == "./shop/edge.json").unwrap().1;
        assert_eq!(shop.imports, vec![(s!("vault"), s!("../.edge-copy-1-vault/main.py"))]);
    }

    #[test]
    fn a_shared_bundle_opens_its_copy_beside_itself() {
        let bundle = Bundle {
            entry: s!("main.py"),
            files: vec![
                Entry { path: s!("main.py"), bytes: b"from .util import x\n".to_vec() },
                Entry { path: s!("edge.json"), bytes: br#"{ "name": "http" }"#.to_vec() },
                Entry { path: s!("util.py"), bytes: b"x = 1\n".to_vec() },
            ],
        }.encode();
        let importer = r#"{ "imports": { "http": "https://cdn.test/pkg/http/0.1.0/app.edge" } }"#;
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.py", "shop": "./shop/main.py" } }"#),
            file("pay/edge.json", importer),
            file("pay/main.py", "import http\n"),
            file("shop/edge.json", importer),
            file("shop/main.py", "import http\n"),
            ("https://cdn.test/pkg/http/0.1.0/app.edge", bundle),
        ], "import pay\nimport shop\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, "https://cdn.test/pkg/http/0.1.0/app.edge/"), named("http", &["pay", "http"]));
        let copy = "https://cdn.test/pkg/http/0.1.0/.edge-copy-1-app.edge/";
        assert_eq!(grants(&seen, copy), named("http", &["shop", "http"]));
        assert!(seen.code.contains(&s!(str copy, "util.py")), "{:?}", seen.code);
        assert!(!seen.fetched.iter().any(|f| f.contains(".edge-copy")), "a copy reads the original: {:?}", seen.fetched);
    }

    // A package importing the one that imports it reaches that same package, since a copy would never end.
    #[test]
    fn an_import_cycle_between_packages_ends() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "a": "./a/main.py" } }"#),
            file("a/edge.json", r#"{ "imports": { "b": "../b/main.py" } }"#),
            file("a/main.py", "import b\n"),
            file("b/edge.json", r#"{ "imports": { "a": "../a/main.py" } }"#),
            file("b/main.py", "import a\n"),
        ], "import a\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, "./a/"), named("a", &["a"]));
        assert_eq!(grants(&seen, "./b/"), named("b", &["a", "b"]));
        assert!(!seen.packages.unwrap().dirs.iter().any(|(dir, _, _)| dir.contains(".edge-copy")));
    }

    #[test]
    fn a_manifest_for_a_later_engine_or_shadowing_a_system_module_is_refused() {
        let later = walk(&[file("edge.json", r#"{ "edge": "99.0.0" }"#)], "");
        assert!(later.failures.iter().any(|f| f.starts_with("edge.json at 'edge.json': needs edge 99.0.0")), "{:?}", later.failures);
        let shadow = walk(&[file("edge.json", r#"{ "imports": { "time": "./fake.py" } }"#), file("fake.py", "")], "import time\n");
        assert!(shadow.failures.iter().any(|f| f.contains("import 'time' takes the name of a system module")), "{:?}", shadow.failures);
    }

    #[test]
    fn what_cannot_load_is_refused_where_it_is_imported() {
        let seen = walk(&[file("edge.json", r#"{ "imports": { "ui": "./ui.js" } }"#)], "import ui\nimport json\nimport my_helpers\n");
        assert!(seen.refused.contains(&(s!("./ui.js"), s!("module 'ui' is JavaScript, ship a .py or a .wasm"))), "{:?}", seen.refused);
        let help = |name: &str| seen.refused.iter().find(|(spec, _)| spec == name).map(|(_, msg)| msg.split_once("\nhelp: ").unwrap().1.to_string());
        assert_eq!(help("json").as_deref(), Some("run `edge add json`"));
        assert_eq!(help("my_helpers").as_deref(), Some("declare it in edge.json, or use a relative import"));
        assert!(seen.packages.unwrap().needed);
    }

    #[test]
    fn the_root_is_main_even_without_a_manifest_and_a_missing_module_fails() {
        let seen = walk(&[], "from .helper import x\n");
        assert!(seen.failures.contains(&s!("could not read module './helper.py'")), "{:?}", seen.failures);
        assert_eq!(grants(&seen, ""), named("main", &["main"]));
        let packages = seen.packages.unwrap();
        assert_eq!(packages.dirs.len(), 1);
        assert!(!packages.needed);
    }
}
