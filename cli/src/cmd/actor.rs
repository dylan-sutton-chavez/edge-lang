use anyhow::{anyhow, Context, Result};
use crate::actor::{Group, Message, Out, ActorConfig};
use crate::host::RunLimits;
use compiler::vm::Limits;
use serde::Deserialize;
use std::path::Path;

// The actor.yml shape, groups keyed by name with per-group overrides.
#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    runtime: Runtime,
    #[serde(default)]
    groups: std::collections::BTreeMap<String, GroupSpec>,
}

#[derive(Deserialize, Default)]
struct Runtime {
    #[serde(default)]
    max_actors: Option<usize>,
    // "auto" for one scheduler per core, or a fixed thread count.
    #[serde(default)]
    schedulers: Option<serde_yaml_ng::Value>,
    // Host and port for the live ingress, its presence turns the actor into a server.
    #[serde(default)]
    listen: Option<String>,
    // Path to the durable log that survives restarts, defaults beside the manifest.
    #[serde(default)]
    durable: Option<String>,
    // Host and port of the HTTP control port, stats, publishing and eval replies.
    #[serde(default)]
    control: Option<String>,
}

#[derive(Deserialize)]
struct GroupSpec {
    // A script path relative to the manifest, or use `code` for an inline body.
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    replicas: Option<usize>,
    // Untrusted mode, each message is compiled as its own program with no send access.
    #[serde(default)]
    eval: bool,
    // Times a crashing message is retried on another actor before it is dropped.
    #[serde(default)]
    retry: usize,
    #[serde(default)]
    limits: LimitSpec,
    #[serde(default)]
    out: Option<String>,
    // Seed messages, the entry point that kicks a actor run.
    #[serde(default)]
    seed: Vec<String>,
}

// Seconds an eval run lasts unless its group says, and the most a group may give it, an instance held all along.
const EVAL_TIMEOUT: u64 = 10;
const EVAL_TIMEOUT_MAX: u64 = 300;

/* What a group may hold and spend, memory in MB, and how often it yields. */
#[derive(Deserialize, Default)]
struct LimitSpec {
    memory: Option<usize>,
    ops: Option<u64>,
    preempt: Option<usize>,
    // Seconds an eval run may last, its waits included.
    timeout: Option<u64>,
}

// Loads actor.yml, boots the described pool, `manifest_path` overrides every group's manifest walk-up.
pub fn run(path: &Path, manifest_path: Option<&Path>) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest = serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut dir = path.parent().and_then(|p| p.to_str()).unwrap_or(".").replace('\\', "/");
    // The manifest walk-up probes `{dir}edge.json`, so a named directory needs its slash.
    if !dir.is_empty() && !dir.ends_with('/') {
        dir.push('/');
    }
    let manifest_path = manifest_path.map(|p| p.to_string_lossy().replace('\\', "/"));
    // Read once, so a malformed eval grant stops the boot instead of a run.
    let ceiling = eval_ceiling(&manifest_path.clone().unwrap_or_else(|| format!("{dir}edge.json")))?;

    let mut groups = Vec::new();
    for (name, spec) in manifest.groups {
        // A directory target runs its main.py with the project as base dir.
        let (source, group_dir) = match (&spec.code, &spec.run, spec.eval) {
            (Some(code), _, _) => (code.clone(), dir.clone()),
            (None, Some(run), _) => load_run(&dir, run).with_context(|| format!("loading '{run}' for group '{name}'"))?,
            (None, None, true) => (String::new(), dir.clone()),
            (None, None, false) => return Err(anyhow!("group '{name}' needs run, code or eval")),
        };
        if spec.limits.timeout.is_some() && !spec.eval {
            return Err(anyhow!("group '{name}' sets limits.timeout, which only an eval group takes"));
        }
        let timeout = spec.limits.timeout.unwrap_or(EVAL_TIMEOUT);
        if !(1..=EVAL_TIMEOUT_MAX).contains(&timeout) {
            return Err(anyhow!("group '{name}' sets limits.timeout to {timeout}, it takes 1 to {EVAL_TIMEOUT_MAX} seconds"));
        }
        let limits = RunLimits { memory: spec.limits.memory, ops: spec.limits.ops }.engine().unwrap_or_else(Limits::sandbox);
        let inbox = spec.seed.into_iter().map(|body| Message::new(name.clone(), body)).collect();
        groups.push(Group {
            name,
            source,
            dir: group_dir,
            manifest: manifest_path.clone(),
            replicas: spec.replicas.unwrap_or(1),
            eval: spec.eval,
            timeout,
            ceiling: ceiling.clone(),
            retry: spec.retry,
            limits,
            preempt: spec.limits.preempt.unwrap_or(2000),
            out: parse_out(spec.out.as_deref()),
            inbox,
        });
    }
    if groups.is_empty() {
        return Err(anyhow!("actor has no groups"));
    }

    let config = ActorConfig { groups, max_actors: manifest.runtime.max_actors.unwrap_or(usize::MAX) };
    // A listen address turns the actor into a live server, else it processes to quiescence.
    let code = match &manifest.runtime.listen {
        Some(listen) => {
            let addr = listen.strip_prefix("tcp://").unwrap_or(listen);
            // A relative durable path sits beside the manifest, the default is actor.wal there.
            let wal = match &manifest.runtime.durable {
                Some(d) => Path::new(&dir).join(d),
                None => Path::new(&dir).join("actor.wal"),
            };
            let control = manifest.runtime.control.as_deref().map(|c| c.strip_prefix("tcp://").unwrap_or(c));
            crate::actor::serve(config, addr, control, &wal)
        }
        None => crate::actor::run(config, resolve_schedulers(manifest.runtime.schedulers.as_ref())),
    };
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/* What the pool edge.json grants its eval groups, nothing when it names no eval holder. */
fn eval_ceiling(path: &str) -> Result<Vec<String>> {
    let Ok(bytes) = std::fs::read(path) else { return Ok(Vec::new()) };
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| format!("parsing {path}"))?;
    let Some(entries) = manifest.pointer("/permissions/eval") else { return Ok(Vec::new()) };
    if let Some(problem) = crate::host::system::check(&serde_json::json!({ "eval": entries })) {
        return Err(anyhow!("edge.json at '{path}': {problem}"));
    }
    Ok(serde_json::from_value(entries.clone())?)
}

/* Loads a run target as source plus base dir, a directory runs its main.py from inside it. */
fn load_run(dir: &str, run: &str) -> Result<(String, String)> {
    let path = Path::new(dir).join(run);
    if path.is_dir() {
        let entry = path.join("main.py");
        let source = std::fs::read_to_string(&entry).with_context(|| format!("reading {}", entry.display()))?;
        // The resolver walks up from `{base}edge.json`, so a directory base needs a trailing slash.
        let mut base = path.to_string_lossy().replace('\\', "/");
        if !base.ends_with('/') { base.push('/'); }
        return Ok((source, base));
    }
    Ok((std::fs::read_to_string(&path)?, dir.to_string()))
}

// Resolves the scheduler count, auto or absent means one per core.
fn resolve_schedulers(value: Option<&serde_yaml_ng::Value>) -> usize {
    let cores = || std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    match value {
        Some(serde_yaml_ng::Value::Number(n)) => n.as_u64().map(|n| n as usize).unwrap_or(1).max(1),
        _ => cores(),
    }
}

// Maps the out uri to a sink, stdout by default.
fn parse_out(out: Option<&str>) -> Out {
    match out {
        None | Some("stdout") => Out::Stdout,
        Some("null") => Out::Null,
        Some(uri) => match uri.strip_prefix("file://") {
            Some(path) => Out::File(path.to_string()),
            None => Out::Stdout,
        },
    }
}
