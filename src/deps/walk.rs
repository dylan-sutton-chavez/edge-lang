use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::lang::Front;
use crate::s;
use crate::util::hash::{FxHashMap, FxHashSet};
use super::bundle::Bundle;
use super::json::{list, quote};
use super::lock::{self, Lock};
use super::perms::{self, Section};
use super::{dir_of, join_relative, joined, outside, parent_dir, parse_integrity, parse_manifest_with, rules, served, split, walk_up_dirs, DepHost, ImportSpec, Manifest, ModuleEntry};

// importers nest this deep before a chain of grants is cut far past real trees
const MAX_DEPTH: usize = 64;

// copies of shared packages past this many end the walk naming the import that asked
const MAX_COPIES: usize = 4096;

/* each section from the root to a package with the key the next arrives under */
pub type Chain = Vec<(Section, String)>;

/* the manifest field each chain link carries and the holders that field reserves */
#[derive(Clone, Default)]
pub struct Carry {
    // empty when the host names no field so every link carries nothing
    pub field: String,
    pub holders: Vec<String>,
}

/* what the walk needs next from its host or the runtime it registers into */
pub enum Step {
    /* the bytes behind a spec with its #sha256- pin kept and answered with `fetched` */
    Fetch(String),
    /* a plugin for the host to instantiate and register answered with `loaded` */
    Plugin { spec: String, name: String, bytes: Vec<u8> },
    Code { spec: String, src: String },
    /* a spec whose import fails with why */
    Refuse { spec: String, msg: String },
    /* a manifest as the compiler resolves names through it with its versions locked */
    Manifest { spec: String, manifest: Manifest },
    /* every package met for the host to serve its system modules answered with `served` */
    System(Packages),
    /* undeclared bare names for the host to say which a registry has answered with `known` */
    Undeclared(Vec<String>),
    /* every failure and none when everything registered */
    Done(Vec<String>),
}

/* who the modules belong to and what the root grants for serving system modules */
pub struct Packages {
    // each manifest dir read in order with its package name and grants
    pub dirs: Vec<(String, String, Chain)>,
    // the root manifest's dir where a malformed grant is reported
    pub root: String,
    // the carried field and what the root manifest holds under it
    pub field: String,
    pub section: Section,
    // whether an undeclared name or a plugin can reach a system module
    pub needed: bool,
}

/* who first imports each package by key and which reach one by relative imports alone */
struct Owners {
    keys: FxHashMap<String, (String, String)>,
    paths: FxHashMap<String, Vec<String>>,
}

/* how the host answered a fetch */
pub enum Fetched {
    Bytes(Vec<u8>),
    /* nothing there with the host's own explanation when it has a better one */
    Missing(Option<String>),
    Failed(String),
}

/* how the host answered a plugin */
pub enum Loaded {
    Ok,
    Failed(String),
    /* this host loads no plugin here so the import fails with why */
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

/* the resolution walk hosts drive lazily so only what a program imports is fetched */
pub struct Walk {
    entry_dir: String,
    system: Vec<String>,
    carry: Carry,
    // the import scan and file suffix of the spelling every source is written in
    scan: fn(&str) -> Vec<ImportSpec>,
    extension: &'static str,
    visited: FxHashSet<String>,
    queue: VecDeque<String>,
    failures: Vec<String>,
    // bare names waiting on their importer's manifest then the ones no manifest declares
    pending_bare: Vec<(String, String, Option<String>)>,
    // root relative imports waiting on their importer's manifest chain
    pending_root: Vec<(String, String, Option<String>)>,
    manifest_dirs: FxHashSet<String>,
    // the same dirs in one spelling which is the identity of a package
    homes: FxHashSet<String>,
    missing: FxHashSet<String>,
    // each spec to the name its first importer wrote and that importer's name if any
    origins: FxHashMap<String, (String, Option<String>)>,
    // files of every imported package keyed under the spec the package came in as
    mounted: FxHashMap<String, Vec<u8>>,
    // each manifest dir to its joined imports and extended dir spelled like the compiler
    imports: FxHashMap<String, Vec<(String, String)>>,
    extends: FxHashMap<String, String>,
    grants: FxHashMap<String, Section>,
    // each manifest as the compiler reads it so a copy can point one import elsewhere
    manifests: FxHashMap<String, Manifest>,
    // every bare import resolved as its manifest dir and name and the spec it reached
    edges: Vec<(String, String, String)>,
    // every relative import as the dir of its importer and the spec it reached
    paths: Vec<(String, String)>,
    // each copy to the package it copies both without the closing slash
    copies: Vec<(String, String)>,
    plugins: bool,
    out: VecDeque<Step>,
    waiting: Option<Waiting>,
    done: bool,
}

impl Walk {
    /* starts from the entry source where `system` names served modules no import may shadow */
    pub fn new(front: &Front, root_src: &str, entry_dir: &str, system: Vec<String>, carry: Carry) -> Walk {
        let mut walk = Walk {
            entry_dir: entry_dir.to_string(),
            system,
            carry,
            scan: front.imports,
            extension: front.extension,
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
            paths: Vec::new(),
            copies: Vec::new(),
            plugins: false,
            out: VecDeque::new(),
            waiting: None,
            done: false,
        };
        for imp in (front.imports)(root_src) {
            walk.enqueue_import(imp, entry_dir, None);
        }
        walk.enqueue_manifest_chain(entry_dir);
        walk
    }

    /* the next step called again after answering whatever the last one asked */
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
                self.void_grants();
                self.split_paths();
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
            // a plugin sits in a package too so its manifest chain names its owner
            Loaded::Ok => {
                self.plugins = true;
                let dir = dir_of(&spec);
                self.enqueue_manifest_chain(&dir);
            }
            Loaded::Failed(e) => self.failures.push(e),
            Loaded::Refused(reason) => self.refuse(&spec, &reason),
        }
    }

    /* the system modules are served and a still undeclared name is asked about before failing */
    pub fn served(&mut self, failures: Vec<String>) {
        if !matches!(self.waiting.take(), Some(Waiting::System)) {
            return;
        }
        self.failures.extend(failures);
        let mut seen = FxHashSet::default();
        // a served module resolves before any refusal so no registry is asked about it
        let undeclared = |name: &String| !self.system.contains(name) && seen.insert(name.clone());
        let names: Vec<String> = core::mem::take(&mut self.pending_bare).into_iter().map(|(name, _, _)| name).filter(undeclared).collect();
        if names.is_empty() {
            self.done = true;
            return;
        }
        self.waiting = Some(Waiting::Undeclared(names.clone()));
        self.out.push_back(Step::Undeclared(names));
    }

    /* each undeclared name fails at its import saying where it can come from */
    pub fn known(&mut self, registered: Vec<String>) {
        let Some(Waiting::Undeclared(names)) = self.waiting.take() else { return };
        for name in names {
            let help = match registered.contains(&name) {
                true => s!("a registry holds '", str &name, "', declare it in edge.json"),
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
                self.out.push_back(Step::Refuse { msg: s!("module '", str &name, "' is JavaScript, ship a .", str self.extension, " or a .wasm"), spec });
            }
            "so" | "dylib" => self.refuse(&spec, "is not supported, ship a .wasm"),
            _ => {
                let waiting = Waiting::Module(spec.clone());
                self.request(spec, waiting)
            }
        }
    }

    /* bytes a mounted package holds answer at once and anything else asks the host */
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
            // the root always has a manifest so its code is package `main` even without one
            Fetched::Missing(_) if spec == "edge.json" => b"{}".to_vec(),
            Fetched::Missing(_) => {
                self.missing.insert(spec);
                self.retry_pending();
                return self.retry_root();
            }
            Fetched::Failed(e) => return self.failures.push(e),
        };
        let field = self.carry.field.as_str();
        let read = parse_manifest_with(&bytes, field).and_then(|(manifest, held)| {
            let section = held.map(|held| perms::section(&held, field)).transpose()?;
            Ok((manifest, section))
        });
        let (manifest, section) = match read {
            Ok(read) => read,
            Err(e) => return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e)),
        };
        // a package names its lowest engine so one for a later engine is refused here
        let system: Vec<&str> = self.system.iter().map(String::as_str).collect();
        let reserved: Vec<(&str, &str)> = self.carry.holders.iter().map(|holder| (holder.as_str(), field)).collect();
        let refused = rules::floor_error(&manifest).or_else(|| manifest.imports.iter().find_map(|(name, target)| rules::import_error(name, target, &system, &reserved)));
        if let Some(e) = refused {
            return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e));
        }
        let dir = dir_of(&spec);
        self.manifest_dirs.insert(dir.clone());
        self.homes.insert(norm(&dir).to_string());
        if let Some(section) = section {
            self.grants.insert(norm(&dir).to_string(), section);
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

    /* each declared version becomes the url and digest its lock holds before anything reads it */
    fn merge(&mut self, spec: String, manifest: Manifest, held: Option<&Lock>) {
        let mut resolved = Vec::with_capacity(manifest.imports.len());
        for (name, target) in &manifest.imports {
            match lock::locked_spec(name, target, held) {
                Ok(target) => resolved.push((name.clone(), target)),
                Err(e) => return self.failures.push(s!("edge.json at '", str &spec, "': ", str &e)),
            }
        }
        let dir = dir_of(&spec);
        // a manifest served from a URL names nothing past its host
        let mut joins = Vec::with_capacity(resolved.len());
        for (name, target) in &resolved {
            match joined(&dir, target) {
                Ok(at) => joins.push((name.clone(), at)),
                Err(home) => return self.failures.push(s!("edge.json at '", str &spec, "': import '", str name, "' at ", str &outside(target, &home))),
            }
        }
        let next = match &manifest.extends {
            Some(ext) => match joined(&dir, ext) {
                Ok(next) if next.ends_with('/') => Some(next),
                Ok(next) => Some(s!(str &next, "/")),
                Err(home) => return self.failures.push(s!("edge.json at '", str &spec, "': extends ", str &outside(ext, &home))),
            },
            None => None,
        };
        self.imports.insert(dir.clone(), joins);
        let manifest = Manifest { imports: resolved, ..manifest };
        self.manifests.insert(spec.clone(), manifest.clone());
        self.out.push_back(Step::Manifest { spec, manifest });
        if let Some(next) = next {
            self.queue.push_back(s!(str &next, "edge.json"));
            self.extends.insert(dir, next);
        }
        self.retry_pending();
        self.retry_root();
    }

    /* a published package verified whole with its entry as the module and its files mounted */
    fn package(&mut self, spec: String, answer: Fetched) {
        let Some(bytes) = self.pinned(&spec, answer) else { return };
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

    // a .wasm spec is a plugin and so is wasm magic under another suffix
    fn module(&mut self, spec: String, answer: Fetched) {
        let Some(bytes) = self.pinned(&spec, answer) else { return };
        if extension(&spec) == "wasm" || (!self.written(&spec) && bytes.starts_with(b"\0asm")) {
            let name = self.label(&spec);
            self.waiting = Some(Waiting::Plugin(spec.clone()));
            return self.out.push_back(Step::Plugin { spec, name, bytes });
        }
        self.code(spec, bytes);
    }

    /* whether `spec` names a source of the front end's extension however many dots it holds */
    fn written(&self, spec: &str) -> bool {
        let path = spec.split(['?', '#']).next().unwrap_or(spec);
        path.strip_suffix(self.extension).is_some_and(|stem| stem.ends_with('.'))
    }

    /* the fetched bytes held to the spec's pin or none once a failure says why */
    fn pinned(&mut self, spec: &str, answer: Fetched) -> Option<Vec<u8>> {
        let bytes = match answer {
            Fetched::Bytes(bytes) => bytes,
            // what cannot be read fails at its import as a refused module does
            Fetched::Missing(hint) => {
                let at = target(spec);
                let why = match hint {
                    Some(hint) => s!("could not be read, ", str &hint),
                    None if self.label(spec) == at => s!("could not be read"),
                    None => s!("could not be read at '", str at, "'"),
                };
                self.refuse(spec, &why);
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

    // a code module registers then its own imports queue so transitive deps stay lazy
    fn code(&mut self, spec: String, bytes: Vec<u8>) {
        let src = String::from_utf8_lossy(&bytes).into_owned();
        let dir = dir_of(&spec);
        let via = self.origins.get(&spec).map(|(name, _)| name.clone());
        for imp in (self.scan)(&src) {
            self.enqueue_import(imp, &dir, via.as_deref());
        }
        self.enqueue_manifest_chain(&dir);
        self.out.push_back(Step::Code { spec, src });
    }

    /* why a module cannot load raised at its import under the name its importer wrote */
    fn refuse(&mut self, spec: &str, reason: &str) {
        let (name, via) = self.origins.get(spec).cloned().unwrap_or_else(|| (target(spec).to_string(), None));
        let via = via.map(|v| s!(" (via ", str &v, ")")).unwrap_or_default();
        self.out.push_back(Step::Refuse { spec: spec.to_string(), msg: s!("module '", str &name, "' ", str reason, str &via) });
    }

    fn label(&self, spec: &str) -> String {
        self.origins.get(spec).map_or_else(|| target(spec).to_string(), |(name, _)| name.clone())
    }

    /* the dir of the root manifest in one spelling and empty without one */
    fn root(&self) -> String {
        match self.root_for(&self.entry_dir) {
            Some(Some(root)) => norm(&root).to_string(),
            _ => String::new(),
        }
    }

    /* fails a grant under an import key whose target has no manifest of its own */
    fn void_grants(&mut self) {
        let mut void = Vec::new();
        for (from, key, spec) in &self.edges {
            let granted = self.grants.get(norm(from)).is_some_and(|section| section.iter().any(|(holder, entries)| holder == key && !entries.is_empty()));
            if granted && self.landing(from, spec).is_none() {
                void.push(s!("edge.json at '", str from, "edge.json': permissions for '", str key, "' reach nothing, its import '", str target(spec), "' has no edge.json of its own so it runs as part of this package"));
            }
        }
        self.failures.extend(void);
    }

    /* fails a package that relative imports from importers holding different grants reach */
    fn split_paths(&mut self) {
        let (root, owners) = (self.root(), self.owners());
        let held = |home: &str| {
            let mut held = perms::held(&self.reach(home, &root, &owners, 0).1);
            held.sort_unstable();
            held
        };
        let mut split = Vec::new();
        for (at, importers) in &owners.paths {
            let first = held(&importers[0]);
            if let Some(other) = importers[1..].iter().find(|other| held(other) != first) {
                split.push(s!("'", str at, "' is reached by relative imports from '", str &importers[0], "' and '", str other, "' which hold different grants, import it under a key of each edge.json instead"));
            }
        }
        split.sort();
        self.failures.extend(split);
    }

    fn packages(&self) -> Packages {
        let root = self.root();
        let owners = self.owners();
        let mut dirs: Vec<String> = self.manifest_dirs.iter().cloned().collect();
        dirs.sort();
        let dirs = dirs.into_iter().map(|dir| {
            let (name, chain) = self.reach(norm(&dir), &root, &owners, 0);
            (dir, name, chain)
        }).collect();
        let section = self.grants.get(&root).cloned().unwrap_or_default();
        Packages { dirs, field: self.carry.field.clone(), section, needed: !self.pending_bare.is_empty() || self.plugins, root }
    }

    /* the name a package dir answers to and the grants reaching it importer by importer */
    fn reach(&self, dir: &str, root: &str, owners: &Owners, depth: usize) -> (String, Chain) {
        let section = |at: &str| self.grants.get(at).cloned().unwrap_or_default();
        if dir == root {
            return (s!("main"), vec![(section(root), s!("main"))]);
        }
        if depth < MAX_DEPTH
            && let Some((from, key)) = owners.keys.get(dir)
        {
            // the root grants its imports outright and other importers pass on only what they hold
            let mut chain = if from == root { Vec::new() } else { self.reach(from, root, owners, depth + 1).1 };
            chain.push((section(from), key.clone()));
            return (key.clone(), chain);
        }
        // reached by relative imports alone it belongs to the package that imported it
        if depth < MAX_DEPTH
            && let Some(importer) = owners.paths.get(dir).and_then(|importers| importers.first())
        {
            return self.reach(importer, root, owners, depth + 1);
        }
        // reached by no import at all it holds what the package around it holds
        match self.enclosing(dir) {
            Some(outer) if depth < MAX_DEPTH => self.reach(&outer, root, owners, depth + 1),
            _ => (dir.to_string(), Vec::new()),
        }
    }

    /* each package dir to its first key import or the packages reaching it by path */
    fn owners(&self) -> Owners {
        let mut keys = FxHashMap::default();
        for (from, key, spec) in &self.edges {
            if let Some(home) = self.landing(from, spec) {
                keys.entry(home).or_insert_with(|| (norm(from).to_string(), key.clone()));
            }
        }
        let mut paths: FxHashMap<String, Vec<String>> = FxHashMap::default();
        for (dir, spec) in &self.paths {
            let Some(Some(home)) = self.root_for(dir) else { continue };
            let home = norm(&home).to_string();
            // a keyed package keeps its grants and code around the importer stays its own
            match self.landing(&home, spec) {
                Some(at) if !keys.contains_key(&at) => {
                    let importers = paths.entry(at).or_default();
                    if !importers.contains(&home) { importers.push(home); }
                }
                _ => {}
            }
        }
        Owners { keys, paths }
    }

    /* copies a shared package per importer granting it otherwise and is true when work queued */
    fn settle(&mut self) -> bool {
        self.retry_pending();
        self.retry_root();
        let owners = self.owners();
        let root = self.root();
        let shared: Vec<(usize, String)> = self.edges.iter().enumerate().filter_map(|(i, (from, key, spec))| {
            let home = self.landing(from, spec)?;
            let (owner, named) = &owners.keys[&home];
            (owner != norm(from) || named != key).then_some((i, home))
        }).collect();
        for (i, home) in shared {
            if self.cycles(&self.edges[i].0, &home, &owners) || self.alike(i, &home, &root, &owners) {
                continue;
            }
            if self.copies.len() >= MAX_COPIES {
                let (from, key, _) = &self.edges[i];
                self.failures.push(s!("the imports make more than ", int MAX_COPIES, " copies of packages held to different grants, the last through '", str key, "' in '", str from, "edge.json'"));
                return false;
            }
            self.copy(i, &home);
        }
        !self.queue.is_empty() || !self.out.is_empty()
    }

    /* whether edge `i` holds what the first import of `home` holds so both share it */
    fn alike(&self, i: usize, home: &str, root: &str, owners: &Owners) -> bool {
        let (from, key, _) = &self.edges[i];
        let from = norm(from);
        let mut chain = if from == root { Vec::new() } else { self.reach(from, root, owners, 1).1 };
        chain.push((self.grants.get(from).cloned().unwrap_or_default(), key.clone()));
        let (mut there, mut here) = (perms::held(&self.reach(norm(home), root, owners, 0).1), perms::held(&chain));
        there.sort_unstable();
        here.sort_unstable();
        there == here
    }

    /* points one import at a copy beside the original so relative imports find the files */
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

    /* where an import from `from` lands or none inside that package or one around it */
    fn landing(&self, from: &str, spec: &str) -> Option<String> {
        let home = norm(&self.root_for(&dir_of(spec))??).to_string();
        let around = walk_up_dirs(norm(from)).any(|d| d == home);
        (!around).then_some(home)
    }

    /* whether `home` holds the importer or one above it since copying a cycle never ends */
    fn cycles(&self, from: &str, home: &str, owners: &Owners) -> bool {
        let home = self.real(home);
        let mut at = norm(from).to_string();
        for _ in 0..MAX_DEPTH {
            if self.real(&at) == home {
                return true;
            }
            at = match owners.keys.get(&at) {
                Some((parent, _)) => parent.clone(),
                None => match owners.paths.get(&at).and_then(|importers| importers.first()).cloned().or_else(|| self.enclosing(&at)) {
                    Some(outer) => outer,
                    None => return false,
                },
            };
        }
        true
    }

    /* the nearest manifest dir above `dir` whose package reached it by path */
    fn enclosing(&self, dir: &str) -> Option<String> {
        walk_up_dirs(dir).skip(1).find(|d| self.homes.contains(d.as_str()))
    }

    /* where the bytes of `spec` live with a copy read from the package it copies */
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

    // queues a module spec whose first importer names it in refusals
    fn enqueue(&mut self, spec: String, name: String, via: Option<String>) {
        self.origins.entry(spec.clone()).or_insert((name, via));
        self.queue.push_back(spec);
    }

    fn enqueue_import(&mut self, imp: ImportSpec, dir: &str, via: Option<&str>) {
        let via = via.map(String::from);
        match imp {
            ImportSpec::Relative(path) => match joined(dir, &path) {
                Ok(spec) => {
                    self.paths.push((dir.to_string(), spec.clone()));
                    self.enqueue(spec, path, via);
                }
                // code served from a URL never reaches past its host so nothing local loads
                Err(home) => self.failures.push(s!("module ", str &outside(&path, &home))),
            },
            ImportSpec::Root(path) => self.enqueue_root(path, dir.to_string(), via),
            ImportSpec::Bare(name) => self.enqueue_bare(name, dir.to_string(), via),
        }
    }

    // a bare name waits on its importer's manifest and then its import is kept
    fn enqueue_bare(&mut self, name: String, dir: String, via: Option<String>) {
        let Some(Some((from, spec))) = self.declared(&name, &dir) else {
            return self.pending_bare.push((name, dir, via));
        };
        if !self.edges.iter().any(|(at, key, _)| *at == from && *key == name) {
            self.edges.push((from, name.clone(), spec.clone()));
        }
        self.enqueue(spec, name, via);
    }

    /* where a bare name from `dir` leads through the nearest manifest and what it extends */
    fn declared(&self, name: &str, dir: &str) -> Option<Option<(String, String)>> {
        let Some(from) = self.root_for(dir)? else { return Some(None) };
        let mut at = from.clone();
        // bounded like the compiler so an extends loop ends
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

    // probes every ancestor manifest as the compiler walks up to resolve
    fn enqueue_manifest_chain(&mut self, dir: &str) {
        for d in walk_up_dirs(dir) {
            let m = s!(str &d, "edge.json");
            if !self.missing.contains(&m) {
                self.queue.push_back(m);
            }
        }
    }

    /* the nearest manifest dir at or above `dir` or `None` while its probes pend */
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
            Some(Some(root)) => match joined(&root, &spec) {
                Ok(at) => self.enqueue(at, spec, via),
                Err(home) => self.failures.push(s!("module ", str &outside(&spec, &home))),
            },
            // no manifest anywhere so the compiler reports it
            Some(None) => {}
        }
    }

    fn retry_root(&mut self) {
        for (spec, dir, via) in core::mem::take(&mut self.pending_root) {
            self.enqueue_root(spec, dir, via);
        }
    }
}

/* points `key` at `target` added when the manifest reached it only through what it extends */
fn set(imports: &mut Vec<(String, String)>, key: &str, target: String) {
    match imports.iter_mut().find(|(name, _)| name == key) {
        Some((_, at)) => *at = target,
        None => imports.push((key.to_string(), target)),
    }
}

/* `spec` as a manifest in `dir` writes it since targets join to that dir */
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

/* a dir in one spelling since `./lib/` and `lib/` are the same place */
fn norm(dir: &str) -> &str {
    dir.trim_start_matches("./")
}

fn target(spec: &str) -> &str {
    spec.split_once('#').map_or(spec, |(t, _)| t)
}

/* the last segment's extension without query or fragment which picks how a module loads */
fn extension(spec: &str) -> &str {
    let path = spec.split(['?', '#']).next().unwrap_or(spec);
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/* starts the walk over `src` from `dir` with the newline joined modules this host serves */
pub fn start<H: DepHost>(front: &Front, host: &mut H, src: &str, dir: &str, system: &[u8], carry: Carry) -> String {
    let system = split(system, '\n');
    host.deps(|d| d.walk = Some(Walk::new(front, src, dir, system, carry)));
    drive(host)
}

/* the field a host names on its first line and its holders on the rest */
pub fn carry(spec: &[u8]) -> Carry {
    let mut lines = core::str::from_utf8(spec).unwrap_or("").split('\n');
    let field = lines.next().unwrap_or_default().to_string();
    // holders belong to a field so a host naming none reserves none
    let holders = match field.is_empty() {
        true => Vec::new(),
        false => lines.filter(|holder| !holder.is_empty()).map(String::from).collect(),
    };
    Carry { field, holders }
}

/* answers the last fetch where `kind` 0 is bytes 1 missing and 2 failed */
pub fn fetched<H: DepHost>(host: &mut H, bytes: &[u8], kind: u32) -> String {
    let text = || String::from_utf8_lossy(bytes).into_owned();
    let answer = match kind {
        0 => Fetched::Bytes(bytes.to_vec()),
        1 => Fetched::Missing((!bytes.is_empty()).then(text)),
        _ => Fetched::Failed(text()),
    };
    host.deps(|d| d.walk.as_mut().map(|walk| walk.fetched(answer)));
    drive(host)
}

/* the bytes of the plugin the last step named taken once */
pub fn plugin_bytes<H: DepHost>(host: &mut H) -> Vec<u8> {
    host.deps(|d| core::mem::take(&mut d.plugin))
}

/* answers the last plugin as `kind` 0 registered 1 failed or 2 refused with why */
pub fn loaded<H: DepHost>(host: &mut H, kind: u32, text: String) -> String {
    let answer = match kind {
        0 => Loaded::Ok,
        1 => Loaded::Failed(text),
        _ => Loaded::Refused(text),
    };
    host.deps(|d| d.walk.as_mut().map(|walk| walk.loaded(answer)));
    drive(host)
}

/* the system modules are served with failures joined by NUL as one may span lines */
pub fn served<H: DepHost>(host: &mut H, failures: &[u8]) -> String {
    let failures = split(failures, '\0');
    host.deps(|d| d.walk.as_mut().map(|walk| walk.served(failures)));
    drive(host)
}

/* answers the undeclared names with the ones a registry has joined by NUL */
pub fn known<H: DepHost>(host: &mut H, names: &[u8]) -> String {
    let names = split(names, '\0');
    host.deps(|d| d.walk.as_mut().map(|walk| walk.known(names)));
    drive(host)
}

/* registers what `step` names in the host's modules or gives back one the host answers */
pub fn settle<H: DepHost>(host: &mut H, step: Step) -> Option<Step> {
    match step {
        Step::Code { spec, src } => {
            host.deps(|d| d.register(spec, ModuleEntry::Code(src)));
            host.changed();
        }
        Step::Refuse { spec, msg } => {
            host.deps(|d| d.refuse(spec, msg));
            host.changed();
        }
        Step::Manifest { spec, manifest } => host.deps(|d| {
            d.manifests.retain(|(s, _)| *s != spec);
            d.manifests.push((spec, manifest));
            // a manifest found anew can move the package a file belongs to
            d.packages.clear();
        }),
        step => return Some(step),
    }
    None
}

/* applies what the walk registers itself up to the first step only the host answers */
fn drive<H: DepHost>(host: &mut H) -> String {
    loop {
        let Some(step) = host.deps(|d| d.walk.as_mut().map(Walk::step)) else {
            return s!("{\"done\":[\"no resolution walk is running\"]}");
        };
        let Some(step) = settle(host, step) else { continue };
        let mut out = String::new();
        match step {
            Step::Code { .. } | Step::Refuse { .. } | Step::Manifest { .. } => {}
            Step::Fetch(spec) => {
                out.push_str("{\"fetch\":");
                quote(&mut out, &spec);
                out.push('}');
                return out;
            }
            Step::Plugin { spec, name, bytes } => {
                host.deps(|d| d.plugin = bytes);
                out.push_str("{\"plugin\":");
                quote(&mut out, &spec);
                out.push_str(",\"name\":");
                quote(&mut out, &name);
                out.push('}');
                return out;
            }
            Step::System(packages) => return served::system(&packages),
            Step::Undeclared(names) => {
                out.push_str("{\"undeclared\":");
                list(&mut out, &names);
                out.push('}');
                return out;
            }
            Step::Done(failures) => {
                host.deps(|d| d.walk = None);
                out.push_str("{\"done\":");
                list(&mut out, &failures);
                out.push('}');
                return out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use self::mini::Mini;
    use crate::deps::bundle::Entry;
    use crate::deps::resolver::tests::Host;
    use alloc::vec;

    // mini's front end over its own table
    const FRONT: Front = Front::spelled::<Mini>();

    /* everything a walk handed its host and runtime driven over an in-memory tree */
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

    /* what a host wiring the grant logic names for the walk to carry */
    fn carried() -> Carry {
        Carry { field: s!(str perms::FIELD), holders: perms::HOLDERS.iter().map(|holder| s!(str holder)).collect() }
    }

    fn walk(files: &[(&str, Vec<u8>)], src: &str) -> Seen {
        walk_carrying(files, src, carried())
    }

    fn walk_carrying(files: &[(&str, Vec<u8>)], src: &str, carry: Carry) -> Seen {
        let mut walk = Walk::new(&FRONT, src, "", vec![s!("net"), s!("time")], carry);
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
                // a registry that has json and nothing else
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

    /* a package dir's name and the key of each grant link that reaches it */
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
        let dep = b"fn shout(w) {\n    return w.loud() }\n".to_vec();
        let digest = lock::digest_of(&dep);
        let lock_text = s!("{ \"greet\": { \"version\": \"0.1.0\", \"url\": \"https://cdn.test/greet.mini\", \"digest\": \"", str &digest, "\" } }");
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "greet": "0.1.0" } }"#),
            ("edge.lock", lock_text.into_bytes()),
            ("https://cdn.test/greet.mini", dep),
        ], "from greet import shout\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        let pinned = s!("https://cdn.test/greet.mini#", str &digest);
        assert_eq!(seen.code, vec![pinned.clone()]);
        let root = &seen.manifests.iter().find(|(spec, _)| spec == "edge.json").unwrap().1;
        assert_eq!(root.imports, vec![(s!("greet"), pinned)]);
    }

    #[test]
    fn bytes_that_break_their_pin_fail_the_walk() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "lib": "./lib.mini#sha256-a4bf0317b809477e6e475cd791573953d3ae05c4e2b03bd5f35199609557dd90" } }"#),
            file("lib.mini", "x = 1\n"),
        ], "import lib\n");
        assert!(seen.failures.iter().any(|f| f.starts_with("integrity check failed for './lib.mini'")), "{:?}", seen.failures);
    }

    #[test]
    fn a_package_answers_its_own_files_and_carries_its_plugin() {
        let bundle = Bundle {
            entry: s!("main.mini"),
            files: vec![
                Entry { path: s!("main.mini"), bytes: b"from _fast import go\n".to_vec() },
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

    // the name a manifest gives itself never counts so `other` cannot pose as `analytics`
    #[test]
    fn a_package_is_granted_through_the_keys_that_import_it() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "analytics": "./analytics/main.mini", "other": "./other/main.mini" }, "permissions": { "analytics": ["secret:KEY"] } }"#),
            file("analytics/edge.json", r#"{ "name": "analytics", "imports": { "trace": "./trace/main.mini" }, "permissions": { "trace": ["secret:KEY"] } }"#),
            file("analytics/main.mini", "import trace\n"),
            file("analytics/trace/edge.json", r#"{ "name": "trace" }"#),
            file("analytics/trace/main.mini", ""),
            file("other/edge.json", r#"{ "name": "analytics" }"#),
            file("other/main.mini", "from .inner.main import x\n"),
            file("other/inner/edge.json", r#"{ "name": "analytics" }"#),
            file("other/inner/main.mini", "x = 1\n"),
        ], "import analytics\nimport other\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, ""), named("main", &["main"]));
        assert_eq!(grants(&seen, "./analytics/"), named("analytics", &["analytics"]));
        assert_eq!(grants(&seen, "./analytics/trace/"), named("trace", &["analytics", "trace"]));
        assert_eq!(grants(&seen, "./other/"), named("other", &["other"]));
        // reached by path so it holds what the package around it holds
        assert_eq!(grants(&seen, "./other/inner/"), named("other", &["other"]));
        let packages = seen.packages.unwrap();
        let (_, _, chain) = packages.dirs.iter().find(|(at, _, _)| at == "./analytics/trace/").unwrap();
        assert_eq!(chain[1].0, vec![(s!("trace"), vec![s!("secret:KEY")])]);
    }

    // two importers of one package each get a copy holding only what that importer passes
    #[test]
    fn a_package_two_importers_share_loads_once_for_each() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.mini", "shop": "./shop/main.mini" }, "permissions": { "pay": ["secret:PAY"], "shop": ["secret:SHOP"] } }"#),
            file("pay/edge.json", r#"{ "imports": { "vault": "../vault/main.mini" }, "permissions": { "vault": ["secret:PAY"] } }"#),
            file("pay/main.mini", "import vault\n"),
            file("shop/edge.json", r#"{ "imports": { "vault": "../vault/main.mini" }, "permissions": { "vault": ["secret:SHOP"] } }"#),
            file("shop/main.mini", "import vault\n"),
            file("vault/edge.json", "{}"),
            file("vault/main.mini", "from .util import x\n"),
            file("vault/util.mini", "x = 1\n"),
        ], "import pay\nimport shop\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, "./vault/"), named("vault", &["pay", "vault"]));
        assert_eq!(grants(&seen, "./.edge-copy-1-vault/"), named("vault", &["shop", "vault"]));
        for spec in ["./vault/main.mini", "./vault/util.mini", "./.edge-copy-1-vault/main.mini", "./.edge-copy-1-vault/util.mini"] {
            assert!(seen.code.contains(&s!(str spec)), "{spec} never loaded: {:?}", seen.code);
        }
        assert!(!seen.fetched.iter().any(|f| f.contains(".edge-copy")), "a copy reads the original: {:?}", seen.fetched);
        let shop = &seen.manifests.iter().rev().find(|(spec, _)| spec == "./shop/edge.json").unwrap().1;
        assert_eq!(shop.imports, vec![(s!("vault"), s!("../.edge-copy-1-vault/main.mini"))]);
    }

    #[test]
    fn a_shared_bundle_opens_its_copy_beside_itself() {
        let bundle = Bundle {
            entry: s!("main.mini"),
            files: vec![
                Entry { path: s!("main.mini"), bytes: b"from .util import x\n".to_vec() },
                Entry { path: s!("edge.json"), bytes: br#"{ "name": "http" }"#.to_vec() },
                Entry { path: s!("util.mini"), bytes: b"x = 1\n".to_vec() },
            ],
        }.encode();
        let importer = |secret: &str| s!("{ \"imports\": { \"http\": \"https://cdn.test/pkg/http/0.1.0/app.edge\" }, \"permissions\": { \"http\": [\"secret:", str secret, "\"] } }").into_bytes();
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.mini", "shop": "./shop/main.mini" }, "permissions": { "pay": ["secret:PAY"], "shop": ["secret:SHOP"] } }"#),
            ("pay/edge.json", importer("PAY")),
            file("pay/main.mini", "import http\n"),
            ("shop/edge.json", importer("SHOP")),
            file("shop/main.mini", "import http\n"),
            ("https://cdn.test/pkg/http/0.1.0/app.edge", bundle),
        ], "import pay\nimport shop\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, "https://cdn.test/pkg/http/0.1.0/app.edge/"), named("http", &["pay", "http"]));
        let copy = "https://cdn.test/pkg/http/0.1.0/.edge-copy-1-app.edge/";
        assert_eq!(grants(&seen, copy), named("http", &["shop", "http"]));
        assert!(seen.code.contains(&s!(str copy, "util.mini")), "{:?}", seen.code);
        assert!(!seen.fetched.iter().any(|f| f.contains(".edge-copy")), "a copy reads the original: {:?}", seen.fetched);
    }

    // a package importing its importer reaches that same package since a copy never ends
    #[test]
    fn an_import_cycle_between_packages_ends() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "a": "./a/main.mini" } }"#),
            file("a/edge.json", r#"{ "imports": { "b": "../b/main.mini" } }"#),
            file("a/main.mini", "import b\n"),
            file("b/edge.json", r#"{ "imports": { "a": "../a/main.mini" } }"#),
            file("b/main.mini", "import a\n"),
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
        let shadow = walk(&[file("edge.json", r#"{ "imports": { "time": "./fake.mini" } }"#), file("fake.mini", "")], "import time\n");
        assert!(shadow.failures.iter().any(|f| f.contains("import 'time' takes the name of a system module")), "{:?}", shadow.failures);
    }

    #[test]
    fn what_cannot_load_is_refused_where_it_is_imported() {
        let seen = walk(&[file("edge.json", r#"{ "imports": { "ui": "./ui.js" } }"#)], "import ui\nimport json\nimport my_helpers\n");
        assert!(seen.refused.contains(&(s!("./ui.js"), s!("module 'ui' is JavaScript, ship a .mini or a .wasm"))), "{:?}", seen.refused);
        let help = |name: &str| seen.refused.iter().find(|(spec, _)| spec == name).map(|(_, msg)| msg.split_once("\nhelp: ").unwrap().1.to_string());
        assert_eq!(help("json").as_deref(), Some("a registry holds 'json', declare it in edge.json"));
        assert_eq!(help("my_helpers").as_deref(), Some("declare it in edge.json, or use a relative import"));
        assert!(seen.packages.unwrap().needed);
    }

    #[test]
    fn the_root_is_main_even_without_a_manifest_and_a_missing_module_fails() {
        let seen = walk(&[], "from .helper import x\n");
        assert!(seen.refused.contains(&(s!("./helper.mini"), s!("module './helper.mini' could not be read"))), "{:?}", seen.refused);
        assert_eq!(grants(&seen, ""), named("main", &["main"]));
        let packages = seen.packages.unwrap();
        assert_eq!(packages.dirs.len(), 1);
        assert!(!packages.needed);
    }

    #[test]
    fn the_host_drives_the_walk_through_its_json_steps() {
        let mut host = Host::default();
        assert_eq!(start(&FRONT, &mut host, "import x\n", "", b"net\ntime", carried()), r#"{"fetch":"edge.json"}"#);
        assert_eq!(fetched(&mut host, br#"{ "imports": { "x": "./x.mini" } }"#, 0), r#"{"fetch":"./x.mini"}"#);
        assert_eq!(fetched(&mut host, b"y = 1\n", 0), r#"{"fetch":"./edge.json"}"#);
        assert_eq!(fetched(&mut host, b"", 1), r#"{"system":{"root":"","needed":false,"dirs":[["","main",[[{},"main"]]]],"permissions":{}}}"#);
        assert_eq!(served(&mut host, b""), r#"{"done":[]}"#);
        assert_eq!(known(&mut host, b""), r#"{"done":["no resolution walk is running"]}"#);
        let state = host.state();
        assert!(matches!(state.deps.registry.as_slice(), [(spec, ModuleEntry::Code(src))] if spec == "./x.mini" && src == "y = 1\n"));
        assert_eq!((state.changed, state.deps.manifests.len()), (1, 1));
    }

    #[test]
    fn a_plugin_and_an_undeclared_name_answer_through_the_host() {
        let mut host = Host::default();
        assert_eq!(start(&FRONT, &mut host, "import fast\nimport json\n", "", b"", carried()), r#"{"fetch":"edge.json"}"#);
        assert_eq!(fetched(&mut host, br#"{ "imports": { "fast": "./fast.wasm" } }"#, 0), r#"{"fetch":"./fast.wasm"}"#);
        assert_eq!(fetched(&mut host, b"\0asm\x01\0\0\0", 0), r#"{"plugin":"./fast.wasm","name":"fast"}"#);
        assert_eq!(plugin_bytes(&mut host), b"\0asm\x01\0\0\0");
        assert!(plugin_bytes(&mut host).is_empty());
        assert_eq!(loaded(&mut host, 0, String::new()), r#"{"fetch":"./edge.json"}"#);
        assert_eq!(fetched(&mut host, b"", 1), r#"{"system":{"root":"","needed":true,"dirs":[["","main",[[{},"main"]]]],"permissions":{}}}"#);
        assert_eq!(served(&mut host, b""), r#"{"undeclared":["json"]}"#);
        assert_eq!(known(&mut host, b"json\0yaml"), r#"{"done":[]}"#);
        let state = host.state();
        assert_eq!(state.deps.refusals, vec![(s!("json"), s!("module 'json' is not provided by this host and no edge.json declares it\nhelp: a registry holds 'json', declare it in edge.json"))]);
        assert_eq!(state.changed, 1);
    }

    #[test]
    fn a_refused_plugin_and_a_failed_fetch_end_the_walk_with_why() {
        let mut host = Host::default();
        assert_eq!(start(&FRONT, &mut host, "import fast\nfrom .gone import x\n", "", b"", carried()), r#"{"fetch":"./gone.mini"}"#);
        assert_eq!(fetched(&mut host, b"timed out", 2), r#"{"fetch":"edge.json"}"#);
        assert_eq!(fetched(&mut host, br#"{ "imports": { "fast": "./fast.wasm" } }"#, 0), r#"{"fetch":"./fast.wasm"}"#);
        assert_eq!(fetched(&mut host, b"\0asm", 0), r#"{"plugin":"./fast.wasm","name":"fast"}"#);
        assert_eq!(loaded(&mut host, 2, s!("is not loaded here")), r#"{"system":{"root":"","needed":false,"dirs":[["","main",[[{},"main"]]]],"permissions":{}}}"#);
        assert_eq!(served(&mut host, b"net failed\0"), r#"{"done":["timed out","net failed"]}"#);
        assert_eq!(host.state().deps.refusals, vec![(s!("./fast.wasm"), s!("module 'fast' is not loaded here"))]);
    }

    // a module the host serves resolves before any refusal so no registry hears of it
    #[test]
    fn a_served_module_is_never_asked_about_as_undeclared() {
        let mut host = Host::default();
        assert_eq!(start(&FRONT, &mut host, "import net\nimport json\n", "", b"net", carried()), r#"{"fetch":"edge.json"}"#);
        assert_eq!(fetched(&mut host, b"", 1), r#"{"system":{"root":"","needed":true,"dirs":[["","main",[[{},"main"]]]],"permissions":{}}}"#);
        assert_eq!(served(&mut host, b""), r#"{"undeclared":["json"]}"#);
    }

    // a host naming no field reads no section and reserves no holder
    #[test]
    fn a_walk_carries_only_the_field_its_host_names() {
        let files = [file("edge.json", r#"{ "imports": { "main": "./lib.mini" }, "permissions": "junk" }"#), file("lib.mini", "x = 1\n")];
        let bare = walk_carrying(&files, "import main\n", Carry::default());
        assert!(bare.failures.is_empty(), "{:?}", bare.failures);
        let packages = bare.packages.unwrap();
        assert!(packages.field.is_empty() && packages.section.is_empty());
        let json = served::system(&packages);
        assert!(json.starts_with(r#"{"system":{"root":"","needed":false,"dirs":[["","main",[[{},"main"]]]"#) && json.ends_with("]]}}") && !json.contains("permissions"), "{json}");
        let held = walk(&files, "import main\n");
        assert!(held.failures.iter().any(|f| f == "edge.json at 'edge.json': permissions must map each package to a list of entries"), "{:?}", held.failures);
        let named = walk_carrying(&[file("edge.json", r#"{ "imports": { "main": "./lib.mini" } }"#), file("lib.mini", "")], "import main\n", carried());
        assert!(named.failures.iter().any(|f| f.ends_with("import 'main' is reserved for permissions")), "{:?}", named.failures);
        assert_eq!(carry(b"permissions\nall\nmain").holders, [s!("all"), s!("main")]);
        assert!(carry(b"").field.is_empty() && carry(b"\nmain").holders.is_empty());
    }

    // importers granting a package alike share one copy so a diamond loads each package once
    #[test]
    fn a_package_its_importers_grant_alike_loads_once() {
        let mut owned = vec![(s!("edge.json"), s!(r#"{ "imports": { "a": "./p0/main.mini", "b": "./p0/main.mini" } }"#))];
        for n in 0..16 {
            let next = n + 1;
            let (manifest, main) = match next < 16 {
                true => (s!("{ \"imports\": { \"a\": \"../p", int next, "/main.mini\", \"b\": \"../p", int next, "/main.mini\" } }"), s!("import a\nimport b\n")),
                false => (s!("{}"), s!("v = 1\n")),
            };
            owned.push((s!("p", int n, "/edge.json"), manifest));
            owned.push((s!("p", int n, "/main.mini"), main));
        }
        let files: Vec<(&str, Vec<u8>)> = owned.iter().map(|(path, text)| (path.as_str(), text.as_bytes().to_vec())).collect();
        let seen = walk(&files, "import a\nimport b\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(seen.code.len(), 16, "{:?}", seen.code);
        assert!(!seen.packages.unwrap().dirs.iter().any(|(dir, _, _)| dir.contains(".edge-copy")));
    }

    // a key whose target has no manifest of its own grants nothing
    #[test]
    fn a_grant_under_a_key_without_a_manifest_is_refused() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "util": "./util/main.mini", "kept": "./kept/main.mini" }, "permissions": { "util": ["net"], "kept": ["net"] } }"#),
            file("util/main.mini", "x = 1\n"),
            file("kept/edge.json", "{}"),
            file("kept/main.mini", "y = 1\n"),
        ], "import util\nimport kept\n");
        assert_eq!(seen.failures, [s!("edge.json at 'edge.json': permissions for 'util' reach nothing, its import './util/main.mini' has no edge.json of its own so it runs as part of this package")]);
    }

    // a package served from a URL reaches URLs alone so it reads no local file
    #[test]
    fn a_url_package_never_reaches_a_local_file() {
        let bundle = |main: &str, manifest: &str| Bundle {
            entry: s!("main.mini"),
            files: vec![
                Entry { path: s!("main.mini"), bytes: main.as_bytes().to_vec() },
                Entry { path: s!("edge.json"), bytes: manifest.as_bytes().to_vec() },
            ],
        }.encode();
        let root = file("edge.json", r#"{ "imports": { "pay": "https://cdn.test/pkg/pay/0.1.0/app.edge" } }"#);
        let secret = file("config.mini", "SECRET = 'hunter2'\n");
        for (main, manifest, why) in [
            ("from ......config import SECRET\nleak = SECRET\n", "{}", "module '../../../../../config.mini' is outside https://cdn.test/ which serves its importer"),
            ("import cfg\n", r#"{ "imports": { "cfg": "/abs/project/config.mini" } }"#, "import 'cfg' at '/abs/project/config.mini' is outside https://cdn.test/"),
            ("import cfg\n", r#"{ "imports": { "cfg": "../../../../../config.mini" } }"#, "import 'cfg' at '../../../../../config.mini' is outside https://cdn.test/"),
            ("import cfg\n", r#"{ "imports": { "cfg": "file:///abs/project/config.mini" } }"#, "is outside https://cdn.test/"),
            ("import cfg\n", r#"{ "extends": "/abs/project" }"#, "extends '/abs/project' is outside https://cdn.test/"),
        ] {
            let seen = walk(&[root.clone(), secret.clone(), ("https://cdn.test/pkg/pay/0.1.0/app.edge", bundle(main, manifest))], "import pay\n");
            assert!(seen.failures.iter().any(|f| f.contains(why)), "{main:?} {manifest}: {:?}", seen.failures);
            // the root's own manifest and lock are the only local files the walk reads
            assert!(seen.fetched.iter().all(|f| f.contains("://") || f == "edge.json" || f == "edge.lock"), "{main:?} {manifest} asked for a local file: {:?}", seen.fetched);
            assert!(!seen.code.iter().any(|c| c.ends_with("config.mini")), "{:?}", seen.code);
        }
    }

    // code a relative import reaches belongs to its importer even under its own manifest
    #[test]
    fn a_sibling_reached_by_path_holds_what_its_importer_holds() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.mini" }, "permissions": { "main": ["net:bank.example.com"], "pay": ["net"] } }"#),
            file("pay/edge.json", "{}"),
            file("pay/main.mini", "from ..shared.main import f\n"),
            file("shared/edge.json", "{}"),
            file("shared/main.mini", "fn f():\n    pass\n"),
        ], "import pay\n");
        assert!(seen.failures.is_empty(), "{:?}", seen.failures);
        assert_eq!(grants(&seen, "./shared/"), named("pay", &["pay"]));
        let packages = seen.packages.unwrap();
        let (_, _, chain) = packages.dirs.iter().find(|(at, _, _)| at == "./shared/").unwrap();
        assert_eq!(perms::held(chain), perms::held(&packages.dirs.iter().find(|(at, _, _)| at == "./pay/").unwrap().2));
    }

    // packages holding different grants that reach one package by path stop the walk
    #[test]
    fn a_package_reached_by_path_from_importers_granted_otherwise_is_refused() {
        let seen = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.mini", "shop": "./shop/main.mini" }, "permissions": { "pay": ["net:a.example.com"], "shop": ["net:b.example.com"] } }"#),
            file("pay/edge.json", "{}"),
            file("pay/main.mini", "from ..shared.main import f\n"),
            file("shop/edge.json", "{}"),
            file("shop/main.mini", "from ..shared.main import f\n"),
            file("shared/edge.json", "{}"),
            file("shared/main.mini", "fn f():\n    pass\n"),
        ], "import pay\nimport shop\n");
        assert!(seen.failures.iter().any(|f| f.starts_with("'shared/' is reached by relative imports from 'pay/' and 'shop/'")), "{:?}", seen.failures);
        // importers holding alike share the package as a key's importers do
        let alike = walk(&[
            file("edge.json", r#"{ "imports": { "pay": "./pay/main.mini", "shop": "./shop/main.mini" }, "permissions": { "pay": ["net"], "shop": ["net"] } }"#),
            file("pay/edge.json", "{}"),
            file("pay/main.mini", "from ..shared.main import f\n"),
            file("shop/edge.json", "{}"),
            file("shop/main.mini", "from ..shared.main import f\n"),
            file("shared/edge.json", "{}"),
            file("shared/main.mini", "fn f():\n    pass\n"),
        ], "import pay\nimport shop\n");
        assert!(alike.failures.is_empty(), "{:?}", alike.failures);
    }

    // the walk scans imports alone so mini's table needs no natives here
    static NATIVES: crate::lang::Natives = crate::lang::Natives::EMPTY;

    // mini's own table read from its file so the walk scans what its programs import
    mod mini {
        use crate as edge;
        include!("../../examples/mini/src/syntax.rs");
    }
}
