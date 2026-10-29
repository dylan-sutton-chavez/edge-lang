use super::{admitted, cache_root, cdn, get, plugins, Access, Instance, Packages, Senders};
use edge::deps::{parse_integrity, system_spec};
use edge::util::sha256::{hex_encode, sha256};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

// how a fetch error reads when the server says the file does not exist
const ABSENT: &str = "not found on the server";

/* where the files of a run come from and which bare names it may resolve */
#[derive(Clone, Default)]
pub struct Project {
    // the starting script or a directory ending in `/` or empty for the root
    pub entry: String,
    pub manifest: Option<String>,
    // an in-memory tree replacing the disk which untrusted runs always carry
    pub bundle: Option<Rc<HashMap<String, Vec<u8>>>>,
    pub untrusted: bool,
    // what the run holds as its pool decided or none for its kind of run
    pub access: Option<Arc<dyn Access>>,
    // the directory of the pool manifest an untrusted run reads files below
    pub pool: Option<String>,
}

impl Project {
    pub fn disk(entry: &str, manifest: Option<&str>) -> Project {
        Project { entry: entry.to_string(), manifest: manifest.map(String::from), bundle: None, untrusted: false, access: None, pool: None }
    }

    pub fn bundle(files: HashMap<String, Vec<u8>>, entry: &str, untrusted: bool) -> Project {
        Project { entry: entry.to_string(), manifest: None, bundle: Some(Rc::new(files)), untrusted, access: None, pool: None }
    }
}

/* what the engine walk asks of this host read from its out buffer */
pub(super) enum Step {
    Fetch(String),
    Plugin { spec: String },
    System(Value),
    Undeclared,
    Done(Vec<String>),
}

enum Answer {
    Bytes(Vec<u8>),
    Missing,
    Failed(String),
}

/* where the answer to a fetch step comes from */
enum Source {
    // the disk or the bundle answered at once
    Here(Answer),
    Remote(Remote),
}

/* how a remote file reads once fetched */
#[derive(Clone, Copy)]
enum Kind {
    Module,
    // a manifest that fails to arrive reads as absent
    Manifest,
    // the manifest of a remote package whose absence a marker keeps for later runs
    Marked,
}

/* a remote file a walk waits on which any thread may fetch */
pub struct Remote {
    url: String,
    pin: Option<[u8; 32]>,
    kind: Kind,
}

impl Remote {
    /* what fetching the file gives which takes as long as the network does */
    pub fn fetch(&self) -> Fetched {
        Fetched(match self.kind {
            Kind::Marked => fetch_manifest(&self.url).map_or(Answer::Missing, Answer::Bytes),
            kind => match fetch_cached(&self.url, self.pin) {
                Ok(bytes) => Answer::Bytes(bytes),
                Err(_) if matches!(kind, Kind::Manifest) => Answer::Missing,
                Err(e) => Answer::Failed(e),
            },
        })
    }
}

/* what a remote fetch gave the walk waiting on it */
pub struct Fetched(Answer);

impl Fetched {
    /* the answer of a fetch whose thread ended without giving one */
    pub fn lost() -> Fetched {
        Fetched(Answer::Failed(String::from("the fetch ended without an answer")))
    }
}

/* an engine walk between the steps this host answers */
pub struct Walk {
    out: Vec<u8>,
    access: Arc<dyn Access>,
    // the entry and source a finished walk leaves the instance prepared for
    pub(super) key: (String, String),
}

impl Walk {
    /* the walk of `src` begun from the entry of the instance's project */
    pub(super) fn start(inst: &mut Instance, src: &str) -> Result<Walk, String> {
        let loaded = inst.host.loaded.clone();
        let access = loaded.access(&inst.project);
        inst.walk_carry(&loaded.carried(&*access))?;
        let out = inst.walk_start(src, &loaded.reserved())?;
        Ok(Walk { out, access, key: (inst.project.entry.clone(), src.to_string()) })
    }

    /* answers each step it can until the walk ends or waits on a remote file */
    pub(super) fn advance(&mut self, inst: &mut Instance) -> Result<Option<Remote>, String> {
        let project = inst.project.clone();
        loop {
            self.out = match step(&self.out)? {
                Step::Fetch(spec) => match locate(&project, &spec) {
                    Source::Here(answer) => fed(inst, answer)?,
                    Source::Remote(remote) => return Ok(Some(remote)),
                },
                Step::Plugin { spec } => {
                    let bytes = inst.walk_plugin_bytes()?;
                    match project.untrusted {
                        true => inst.walk_plugin(2, "is not available to untrusted eval runs")?,
                        // a plugin that does not load fails at its import as a refused one does
                        false => match plugins::register_bytes(inst, &spec, &bytes) {
                            Ok(()) => inst.walk_plugin(0, "")?,
                            Err(why) => inst.walk_plugin(2, &format!("is not a valid plugin, {why}"))?,
                        },
                    }
                }
                Step::System(packages) => {
                    let failures = serve_modules(inst, &self.access, &packages);
                    inst.walk_served(&failures.join("\0"))?
                }
                // a run asks no registry about a name nothing declares so its hint stays generic
                Step::Undeclared => inst.walk_known("")?,
                Step::Done(failures) if failures.is_empty() => return Ok(None),
                Step::Done(failures) => return Err(failures.iter().map(|f| format!("error: {}", super::lock_help(f))).collect::<Vec<_>>().join("\n")),
            };
        }
    }

    /* hands the walk what the remote fetch it waited on gave */
    pub(super) fn answer(&mut self, inst: &mut Instance, fetched: Fetched) -> Result<(), String> {
        self.out = fed(inst, fetched.0)?;
        Ok(())
    }
}

/* the walk's next step once it holds `answer` */
fn fed(inst: &mut Instance, answer: Answer) -> Result<Vec<u8>, String> {
    match answer {
        Answer::Bytes(bytes) => inst.walk_fetched(&bytes, 0),
        Answer::Missing => inst.walk_fetched(&[], 1),
        Answer::Failed(e) => inst.walk_fetched(e.as_bytes(), 2),
    }
}

/* registers every module `root_src` reaches as the engine decides and this host fetches */
pub fn prefetch(inst: &mut Instance, root_src: &str) -> Result<(), String> {
    let mut walk = Walk::start(inst, root_src)?;
    while let Some(remote) = walk.advance(inst)? {
        walk.answer(inst, remote.fetch())?;
    }
    Ok(())
}

pub(super) fn step(out: &[u8]) -> Result<Step, String> {
    let v: Value = serde_json::from_slice(out).map_err(|e| format!("the runtime left a walk step that is not JSON: {e}"))?;
    let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    if let Some(spec) = text("fetch") {
        return Ok(Step::Fetch(spec));
    }
    if let Some(spec) = text("plugin") {
        return Ok(Step::Plugin { spec });
    }
    if let Some(packages) = v.get("system") {
        return Ok(Step::System(packages.clone()));
    }
    if v.get("undeclared").is_some() {
        return Ok(Step::Undeclared);
    }
    let failures = v.get("done").and_then(Value::as_array).ok_or_else(|| format!("the runtime left an unknown walk step: {v}"))?;
    Ok(Step::Done(failures.iter().filter_map(Value::as_str).map(str::to_string).collect()))
}

/* serves each package the walk met every host module `access` gives it */
fn serve_modules(inst: &mut Instance, access: &Arc<dyn Access>, step: &Value) -> Vec<String> {
    let host = inst.host.clone();
    let modules = host.loaded.modules();
    let packages = Packages::read(step, access.field());
    if let Some(problem) = access.check(&packages, modules) {
        return vec![problem];
    }
    let mut failures = Vec::new();
    // only an undeclared name or a plugin can reach a host module
    if !packages.needed {
        inst.store.data_mut().senders = Some(Senders::new(packages, access.clone(), modules.to_vec()));
        return failures;
    }
    for package in &packages.dirs {
        for module in modules {
            let spec = system_spec(module.name(), &package.dir);
            let served = match access.held(&packages, package, module.name()) {
                Ok(held) => {
                    inst.opening = Some((module.clone(), held.clone()));
                    let opened = module.open(inst, &spec, &package.name, held);
                    inst.opening = None;
                    opened
                }
                Err(refusal) => inst.register_error(&spec, &refusal),
            };
            if let Err(e) = served {
                failures.push(e);
            }
        }
    }
    inst.store.data_mut().senders = Some(Senders::new(packages, access.clone(), modules.to_vec()));
    failures
}

/* reads a manifest or lock beside the project and anything else as a module */
fn locate(project: &Project, spec: &str) -> Source {
    let (target, pin) = match parse_integrity(spec) {
        Ok(parsed) => parsed,
        Err(e) => return Source::Here(Answer::Failed(e)),
    };
    match target.ends_with("edge.json") || target.ends_with(crate::lock::FILE) {
        true => beside(project, target),
        false => module(project, target, pin),
    }
}

/* a manifest or lock where a `--manifest` override takes the root pair's place alone */
fn beside(project: &Project, target: &str) -> Source {
    // every other package keeps the manifest and lock it holds itself
    let replaced = project.manifest.as_ref().and_then(|path| match target {
        "edge.json" => Some(PathBuf::from(path)),
        "edge.lock" => Some(Path::new(path).with_file_name(crate::lock::FILE)),
        _ => None,
    });
    if let Some(file) = replaced {
        return Source::Here(match std::fs::read(&file) {
            Ok(bytes) => Answer::Bytes(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Answer::Missing,
            Err(e) => Answer::Failed(format!("reading {}: {e}", file.display())),
        });
    }
    // a remote manifest that answered 404 once stays absent so later runs skip the request
    if target.contains("://") && project.bundle.is_none() {
        return Source::Remote(Remote { url: target.to_string(), pin: None, kind: Kind::Marked });
    }
    match module(project, target, None) {
        Source::Here(Answer::Failed(_)) => Source::Here(Answer::Missing),
        Source::Remote(remote) => Source::Remote(Remote { kind: Kind::Manifest, ..remote }),
        found => found,
    }
}

/* a module's bytes from the bundle or the disk or else the download it awaits */
fn module(project: &Project, target: &str, pin: Option<[u8; 32]>) -> Source {
    if let Some(files) = &project.bundle {
        // bundle paths are plain though a joined spec may keep a leading dot
        if let Some(bytes) = files.get(target.strip_prefix("./").unwrap_or(target)) {
            return Source::Here(Answer::Bytes(bytes.clone()));
        }
        if !target.contains("://") {
            return Source::Here(Answer::Missing);
        }
    }
    if target.contains("://") {
        // an untrusted run reads remote modules from the origin its composition names only
        if project.untrusted && !admitted(target) {
            return Source::Here(Answer::Failed(format!("module '{target}' is not available to untrusted eval runs")));
        }
        return Source::Remote(Remote { url: target.to_string(), pin, kind: Kind::Module });
    }
    Source::Here(match std::fs::read(target) {
        Ok(bytes) => Answer::Bytes(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Answer::Missing,
        Err(e) => Answer::Failed(format!("cannot read module '{target}': {e}")),
    })
}

/* a remote manifest or `None` when absent with a 404 leaving a `.missing` marker */
fn fetch_manifest(url: &str) -> Option<Vec<u8>> {
    let dir = cache_dir().ok()?;
    let marker = dir.join(format!("{}.missing", hex_encode(&sha256(cdn(url).as_bytes()))));
    if marker.exists() {
        return None;
    }
    match fetch_cached(url, None) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            if e.ends_with(ABSENT) {
                let _ = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&marker, b""));
            }
            None
        }
    }
}

/* downloads once into the user cache with a `.lock` sidecar pinning the digest */
pub fn fetch_cached(url: &str, expected: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
    let dir = cache_dir()?;
    let ext = url.rsplit('.').next().unwrap_or("bin");
    // keyed by the address actually fetched so a staging origin never fills a production entry
    let source = cdn(url);
    let file = dir.join(format!("{}.{ext}", hex_encode(&sha256(source.as_bytes()))));
    let lock = file.with_extension(format!("{ext}.lock"));
    if file.exists() {
        let bytes = std::fs::read(&file).map_err(|e| format!("cannot read cached '{url}': {e}"))?;
        // a stale blob under a new explicit pin is a miss refetched instead of failing
        match check_pin(url, &bytes, std::fs::read_to_string(&lock).ok(), expected) {
            Ok(_) => return Ok(bytes),
            Err(e) if expected.is_none() => return Err(e),
            Err(_) => {}
        }
    }
    let mut resp = get(&source).map_err(|e| match e {
        ureq::Error::StatusCode(404 | 410) => format!("fetching '{source}': {ABSENT}"),
        e => format!("fetching '{source}': {e}"),
    })?;
    let bytes = super::body(&mut resp).map_err(|e| format!("reading '{source}': {e}"))?;
    let got = check_pin(url, &bytes, None, expected)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating cache dir: {e}"))?;
    // a temp file and a rename keep a truncated download out of the shared cache
    static FETCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let fetch = FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = file.with_extension(format!("{ext}.{}.{fetch}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing cache for '{url}': {e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("writing cache for '{url}': {e}"))?;
    std::fs::write(&lock, &got).map_err(|e| format!("writing cache for '{url}': {e}"))?;
    Ok(bytes)
}

/* hashes `bytes` against an explicit pin or else the sidecar and unpinned bytes set it */
fn check_pin(spec: &str, bytes: &[u8], locked: Option<String>, expected: Option<[u8; 32]>) -> Result<String, String> {
    let got = hex_encode(&sha256(bytes));
    if let Some(want) = expected.map(|h| hex_encode(&h)) {
        if want != got {
            return Err(format!("integrity check failed for '{spec}'\n expected sha256-{want}\n got sha256-{got}"));
        }
    } else if let Some(want) = locked
        && want != got
    {
        return Err(format!("integrity drift for '{spec}'\n  locked: sha256-{want}\n  remote: sha256-{got}"));
    }
    Ok(got)
}

fn cache_dir() -> Result<PathBuf, String> {
    Ok(cache_root()?.join("modules"))
}
