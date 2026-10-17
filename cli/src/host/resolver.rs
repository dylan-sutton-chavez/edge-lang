use super::{cache_root, cdn, get, plugins, site, system, Instance, ORIGIN};
use crate::web::SYSTEM_MODULES;
use compiler::modules::{parse_integrity, rules, system_spec};
use compiler::util::sha256::{hex_encode, sha256};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

// How a fetch error reads when the server says the file does not exist.
const ABSENT: &str = "not found on the server";
// Bounds a runaway download, the largest module is well under a megabyte.
const MAX_FETCH_BYTES: u64 = 64 << 20;
// How long a failing run may wait on the registry for a better hint.
const HINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/* Where a run's files come from and which bare names it may resolve. */
#[derive(Clone, Default)]
pub struct Project {
    // The script a run starts from, a directory ending in '/', or empty for the root.
    pub entry: String,
    pub manifest: Option<String>,
    // An in-memory tree replaces the disk, untrusted runs always carry one.
    pub bundle: Option<Rc<HashMap<String, Vec<u8>>>>,
    pub untrusted: bool,
    // What an untrusted bundle with its own edge.json may grant, None for any other run.
    pub ceiling: Option<Vec<String>>,
}

impl Project {
    pub fn disk(entry: &str, manifest: Option<&str>) -> Project {
        Project { entry: entry.to_string(), manifest: manifest.map(String::from), bundle: None, untrusted: false, ceiling: None }
    }

    pub fn bundle(files: HashMap<String, Vec<u8>>, entry: &str, untrusted: bool) -> Project {
        Project { entry: entry.to_string(), manifest: None, bundle: Some(Rc::new(files)), untrusted, ceiling: None }
    }
}

/* What the engine's walk asks of this host, read out of the step it left in the out buffer. */
enum Step {
    Fetch(String),
    Plugin { spec: String, name: String },
    System(Value),
    Undeclared(Vec<String>),
    Done(Vec<String>),
}

enum Answer {
    Bytes(Vec<u8>),
    Missing,
    Failed(String),
}

/* Registers every module `root_src` reaches, the engine deciding and this host reading what it asks for. */
pub fn prefetch(inst: &mut Instance, root_src: &str) -> Result<(), String> {
    let project = inst.project.clone();
    let mut out = inst.walk_start(root_src, &SYSTEM_MODULES.join("\n"))?;
    loop {
        out = match step(&out)? {
            Step::Fetch(spec) => match fetch(&project, &spec) {
                Answer::Bytes(bytes) => inst.walk_fetched(&bytes, 0)?,
                Answer::Missing => inst.walk_fetched(&[], 1)?,
                Answer::Failed(e) => inst.walk_fetched(e.as_bytes(), 2)?,
            },
            Step::Plugin { spec, name } => {
                let bytes = inst.walk_plugin_bytes()?;
                match project.untrusted {
                    true => inst.walk_plugin(2, "is not available to untrusted eval runs")?,
                    false => match plugins::register_bytes(inst, &name, &spec, &bytes) {
                        Ok(()) => inst.walk_plugin(0, "")?,
                        Err(e) => inst.walk_plugin(1, &e)?,
                    },
                }
            }
            Step::System(packages) => {
                let failures = serve_system(inst, &project, &packages);
                inst.walk_served(&failures.join("\0"))?
            }
            // An untrusted run makes no request on its own behalf, so its hint stays the generic one.
            Step::Undeclared(names) => {
                let known = if project.untrusted { Vec::new() } else { registered(&names) };
                inst.walk_known(&known.join("\0"))?
            }
            Step::Done(failures) if failures.is_empty() => return Ok(()),
            Step::Done(failures) => return Err(failures.iter().map(|f| format!("error: {f}")).collect::<Vec<_>>().join("\n")),
        };
    }
}

fn step(out: &[u8]) -> Result<Step, String> {
    let v: Value = serde_json::from_slice(out).map_err(|e| format!("the compiler left a walk step that is not JSON: {e}"))?;
    let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    if let Some(spec) = text("fetch") {
        return Ok(Step::Fetch(spec));
    }
    if let Some(spec) = text("plugin") {
        return Ok(Step::Plugin { spec, name: text("name").unwrap_or_default() });
    }
    if let Some(packages) = v.get("system") {
        return Ok(Step::System(packages.clone()));
    }
    if let Some(names) = v.get("undeclared").and_then(Value::as_array) {
        return Ok(Step::Undeclared(names.iter().filter_map(Value::as_str).map(str::to_string).collect()));
    }
    let failures = v.get("done").and_then(Value::as_array).ok_or_else(|| format!("the compiler left an unknown walk step: {v}"))?;
    Ok(Step::Done(failures.iter().filter_map(Value::as_str).map(str::to_string).collect()))
}

/* The names the registry has, asked at once with lock=1 so nothing counts, none when it is out of reach. */
fn registered(names: &[String]) -> Vec<String> {
    std::thread::scope(|scope| {
        let asks: Vec<_> = names
            .iter()
            .filter(|name| rules::named(name))
            .map(|name| {
                scope.spawn(move || {
                    let url = site(&format!("/api/resolve/package/{name}?lock=1"));
                    ureq::get(&url).config().timeout_global(Some(HINT_TIMEOUT)).build().call().is_ok().then(|| name.clone())
                })
            })
            .collect();
        asks.into_iter().filter_map(|ask| ask.join().ok().flatten()).collect()
    })
}

/* Serves the system modules to every package the walk met, refused where its importers pass none. */
fn serve_system(inst: &mut Instance, project: &Project, packages: &Value) -> Vec<String> {
    let root = packages["root"].as_str().unwrap_or_default();
    // An untrusted run grants only through its own bundle manifest, and an empty grant needs no checking.
    let declared = packages.get("permissions").filter(|p| (!project.untrusted || project.ceiling.is_some()) && p.as_object().is_some_and(|holders| !holders.is_empty())).cloned();
    if let Some(problem) = declared.as_ref().and_then(system::check) {
        return vec![format!("edge.json at '{root}edge.json': {problem}")];
    }
    let granted = declared.is_some();
    let permissions = declared.unwrap_or_else(|| json!({}));
    let entries: Vec<&Value> = permissions.as_object().into_iter().flat_map(|holders| holders.values()).filter_map(Value::as_array).flatten().collect();
    // The refusal names only what the bundle grants, never the eval grant of the pool.
    if let Some(ceiling) = &project.ceiling
        && !entries.is_empty()
    {
        let over: Vec<(&String, String)> = permissions
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(holder, listed)| {
                let missing = system::unmet(&system::held(&json!([[{ "eval": ceiling }, "eval"]])), &json!({ "main": listed }));
                (!missing.is_empty()).then(|| (holder, missing.join(", ")))
            })
            .collect();
        if !over.is_empty() {
            // Laid out as edge lock lays out what a root misses, one holder to a line.
            let width = over.iter().map(|(holder, _)| holder.len()).max().unwrap_or(0);
            let lines: Vec<String> = over.iter().map(|(holder, asks)| format!("  {holder:<width$}   {asks}")).collect();
            return vec![format!("the pool does not grant eval what this bundle grants\n{}", lines.join("\n"))];
        }
    }
    let mut failures = Vec::new();
    // fs reads the project where this run keeps it, under its root edge.json.
    if entries.iter().any(|e| e.as_str().is_some_and(|e| e.starts_with("fs:"))) {
        match &project.bundle {
            Some(files) => system::files(inst.run(), system::Files::Bundle(std::sync::Arc::new((**files).clone()), root.to_string())),
            None => {
                let beside = project.manifest.as_deref().and_then(|m| Path::new(m).parent()).filter(|dir| !dir.as_os_str().is_empty());
                let dir = beside.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(if root.is_empty() { "." } else { root }));
                if let Ok(dir) = dir.canonicalize() {
                    system::files(inst.run(), system::Files::Disk(dir));
                }
            }
        }
    }
    // SpiderMonkey starts only when a system module is reachable, by an undeclared name or a plugin.
    if !packages["needed"].as_bool().unwrap_or(false) {
        return failures;
    }
    for pair in packages["dirs"].as_array().into_iter().flatten() {
        let (Some(dir), Some(pkg)) = (pair[0].as_str(), pair[1].as_str()) else { continue };
        // A run that grants nothing of its own holds nothing, whatever its manifests pass on.
        let chain = if granted { pair[2].clone() } else { json!([]) };
        for module in SYSTEM_MODULES {
            let spec = system_spec(module, dir);
            let served = match system::scopes(&chain, module) {
                Some(held) => inst.register_system(&spec, pkg, module, &held),
                None => inst.register_error(&spec, &format!("'{pkg}' imports {module}, which edge.json does not grant it")),
            };
            if let Err(e) = served {
                failures.push(e);
            }
        }
    }
    failures
}

/* Bytes for what the walk asks, a manifest or its lock where the project keeps them, anything else as a module. */
fn fetch(project: &Project, spec: &str) -> Answer {
    let (target, pin) = match parse_integrity(spec) {
        Ok(parsed) => parsed,
        Err(e) => return Answer::Failed(e),
    };
    match target.ends_with("edge.json") || target.ends_with(crate::lock::FILE) {
        true => beside(project, target),
        false => module(project, target, pin),
    }
}

/* A manifest or a lock, a `--manifest` override then the only manifest and its lock the one beside it. */
fn beside(project: &Project, target: &str) -> Answer {
    if let Some(path) = &project.manifest {
        let file = match target {
            "edge.json" => PathBuf::from(path),
            "edge.lock" => Path::new(path).with_file_name(crate::lock::FILE),
            _ => return Answer::Missing,
        };
        return match std::fs::read(&file) {
            Ok(bytes) => Answer::Bytes(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Answer::Missing,
            Err(e) => Answer::Failed(format!("reading {}: {e}", file.display())),
        };
    }
    // A remote manifest that answered 404 once stays absent, so later runs skip the request.
    if target.contains("://") && project.bundle.is_none() {
        return fetch_manifest(target).map_or(Answer::Missing, Answer::Bytes);
    }
    match module(project, target, None) {
        Answer::Failed(_) => Answer::Missing,
        found => found,
    }
}

/* A module's bytes, from the bundle, a pinned download or the disk. */
fn module(project: &Project, target: &str, pin: Option<[u8; 32]>) -> Answer {
    if let Some(files) = &project.bundle {
        // Bundle paths are plain, a joined spec may still carry the importer's leading dot.
        if let Some(bytes) = files.get(target.strip_prefix("./").unwrap_or(target)) {
            return Answer::Bytes(bytes.clone());
        }
        if !target.contains("://") {
            return Answer::Missing;
        }
    }
    if target.contains("://") {
        // An untrusted run reads remote modules from the official origin only.
        if project.untrusted && !target.starts_with(&format!("{ORIGIN}/")) {
            return Answer::Failed(format!("module '{target}' is not available to untrusted eval runs"));
        }
        return fetch_cached(target, pin).map_or_else(Answer::Failed, Answer::Bytes);
    }
    match std::fs::read(target) {
        Ok(bytes) => Answer::Bytes(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Answer::Missing,
        Err(e) => Answer::Failed(format!("cannot read module '{target}': {e}")),
    }
}

/* A remote manifest, None when it is absent, a 404 leaves a `.missing` marker in the cache. */
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

/* Downloads once into the user cache, a `.lock` sidecar pins the digest like the JS host lockfile. */
pub fn fetch_cached(url: &str, expected: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
    let dir = cache_dir()?;
    let ext = url.rsplit('.').next().unwrap_or("bin");
    // Keyed by the address actually fetched, so a staging origin never fills a production entry.
    let source = cdn(url);
    let file = dir.join(format!("{}.{ext}", hex_encode(&sha256(source.as_bytes()))));
    let lock = file.with_extension(format!("{ext}.lock"));
    if file.exists() {
        let bytes = std::fs::read(&file).map_err(|e| format!("cannot read cached '{url}': {e}"))?;
        // A stale blob under a new explicit pin is a miss, so refetch instead of failing.
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
    let mut bytes = Vec::new();
    resp.body_mut().as_reader().take(MAX_FETCH_BYTES).read_to_end(&mut bytes).map_err(|e| format!("reading '{source}': {e}"))?;
    let got = check_pin(url, &bytes, None, expected)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating cache dir: {e}"))?;
    // Temp plus rename keeps a truncated download out of the shared cache.
    let tmp = file.with_extension(format!("{ext}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing cache for '{url}': {e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("writing cache for '{url}': {e}"))?;
    std::fs::write(&lock, &got).map_err(|e| format!("writing cache for '{url}': {e}"))?;
    Ok(bytes)
}

/* Hashes `bytes` against an explicit pin, else the sidecar record, unpinned bytes set the pin. */
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
