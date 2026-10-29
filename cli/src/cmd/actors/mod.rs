use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use crate::host::RunLimits;
use crate::kit::{self, Command, Kit};

mod actor;
mod pool;
mod scheduler;
// the live server a pool becomes with its ingress control endpoint and durable log
#[cfg(feature = "actors-server")]
pub mod server;

pub use pool::{live, run, Eval, GroupSpec, Message, Out, Pool, Reply};
pub use scheduler::{Intake, Pass, Stats};

#[derive(Args)]
struct ActorArgs {
    /// Path to the actor.yml manifest.
    file: PathBuf,
}

/* `actor` running the pool an actor.yml describes */
pub struct Actor;

impl Command for Actor {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        ActorArgs::augment_args(clap::Command::new("actor").about("Run a pool of actors from an actor.yml manifest"))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("actor <file>".to_string(), "Run an actor pool from actor.yml".to_string())
    }

    fn compiles(&self) -> bool {
        true
    }

    // the groups resolve through the manifest nearest above their actor.yml
    fn manifest(&self, m: &ArgMatches) -> PathBuf {
        match m.try_get_one::<PathBuf>("file").ok().flatten() {
            Some(file) => kit::root_manifest(m, &edge::deps::dir_of(&crate::host::driver::path_spec(file))),
            None => kit::manifest_path(m),
        }
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let path = ActorArgs::from_arg_matches(m)?.file;
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let manifest: Manifest = serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let settings = manifest.runtime;
        let max_actors = settings.max_actors.unwrap_or(usize::MAX);
        if max_actors == 0 {
            bail!("runtime.max_actors is 0, it takes 1 or more");
        }
        let threads = threads(settings.schedulers.as_ref())?;
        // a control endpoint off loopback stops the boot before anything opens or binds
        let control = settings.control.as_deref().map(local).transpose()?;
        let mut dir = path.parent().and_then(|p| p.to_str()).unwrap_or(".").replace('\\', "/");
        // the manifest walk-up probes `{dir}edge.json` so a named directory needs its slash
        if !dir.is_empty() && !dir.ends_with('/') {
            dir.push('/');
        }
        let groups = group_specs(kit, manifest.groups, &dir, kit::manifest_flag(m).map(|p| p.to_string_lossy().replace('\\', "/")))?;
        let pool = Pool { groups, max_actors };
        match settings.listen {
            None => Ok(run(kit, pool, threads)),
            Some(listen) => serve(kit, pool, bare(&listen), control, Path::new(&dir).join(settings.durable.as_deref().unwrap_or("actor.wal"))),
        }
    }
}

// the actor.yml shape whose unknown keys stay free for readers such as tests
#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    runtime: Settings,
    #[serde(default)]
    groups: BTreeMap<String, Spec>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Settings {
    max_actors: Option<usize>,
    // auto for one scheduler per core or a fixed thread count
    schedulers: Option<serde_yaml_ng::Value>,
    // the ingress address whose presence turns the pool into a live server
    listen: Option<String>,
    // the durable log that survives restarts kept beside the manifest by default
    durable: Option<String>,
    // the loopback address of the HTTP control endpoint for stats publishing and eval replies
    control: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Spec {
    // a script path relative to the manifest where `code` gives an inline body instead
    run: Option<String>,
    code: Option<String>,
    replicas: Option<usize>,
    // untrusted mode where each message compiles as its own program with no send access
    eval: bool,
    // times a crashing message is retried on another actor before it is dropped
    retry: usize,
    limits: LimitSpec,
    out: Option<String>,
    // seed messages as the entry point that kicks off an actor run
    seed: Vec<String>,
}

// default and maximum seconds of an eval run which holds an instance all along
const EVAL_TIMEOUT: u64 = 10;
const EVAL_TIMEOUT_MAX: u64 = 300;

// eval runs a group takes at once so one slow client never holds the rest
const EVAL_REPLICAS: usize = 4;

/* what a group may hold in MB and spend and how often it yields */
#[derive(Deserialize, Default)]
#[serde(default)]
struct LimitSpec {
    memory: Option<usize>,
    ops: Option<u64>,
    preempt: Option<usize>,
    // seconds an eval run may last including its waits
    timeout: Option<u64>,
}

/* the groups of actor.yml with `manifest` overriding the manifest walk-up of every group */
fn group_specs(kit: &Kit, specs: BTreeMap<String, Spec>, dir: &str, manifest: Option<String>) -> Result<Vec<GroupSpec>> {
    // eval snippets that bring no edge.json resolve through the one of the pool
    let pool = manifest.clone().unwrap_or_else(|| format!("{dir}edge.json"));
    // read once so a malformed eval grant stops the boot instead of a run
    let untrusted = specs.values().any(|s| s.eval).then(|| kit.grants().untrusted(kit, &pool)).transpose()?;
    let mut groups = Vec::with_capacity(specs.len());
    for (name, spec) in specs {
        if [spec.code.is_some(), spec.run.is_some(), spec.eval].into_iter().filter(|&set| set).count() > 1 {
            bail!("group '{name}' sets more than one of code, run and eval, it takes one");
        }
        // a directory target runs its entry file with the project as base dir
        let (source, base) = match (spec.code, &spec.run, spec.eval) {
            (Some(code), _, _) => (code, dir.to_string()),
            (None, Some(run), _) => load_run(kit, dir, run).with_context(|| format!("loading '{run}' for group '{name}'"))?,
            (None, None, true) => (String::new(), dir.to_string()),
            (None, None, false) => bail!("group '{name}' needs run, code or eval"),
        };
        if spec.limits.timeout.is_some() && !spec.eval {
            bail!("group '{name}' sets limits.timeout, which only an eval group takes");
        }
        let timeout = spec.limits.timeout.unwrap_or(EVAL_TIMEOUT);
        if !(1..=EVAL_TIMEOUT_MAX).contains(&timeout) {
            bail!("group '{name}' sets limits.timeout to {timeout}, it takes 1 to {EVAL_TIMEOUT_MAX} seconds");
        }
        let replicas = spec.replicas.unwrap_or(if spec.eval { EVAL_REPLICAS } else { 1 });
        if replicas == 0 {
            bail!("group '{name}' sets replicas to 0, it takes 1 or more");
        }
        let out = out(&name, spec.out.as_deref())?;
        let limits = RunLimits { memory: spec.limits.memory, ops: spec.limits.ops }.over(kit.run_limits());
        let eval = untrusted.as_ref().filter(|_| spec.eval).map(|untrusted| Eval { timeout, untrusted: untrusted.clone(), manifest: pool.clone(), words: kit.words() });
        let preempt = spec.limits.preempt.unwrap_or(2000);
        groups.push(GroupSpec { name, source, dir: base, manifest: manifest.clone(), replicas, limits, preempt, out, retry: spec.retry, seed: spec.seed, eval });
    }
    if groups.is_empty() {
        bail!("actor has no groups");
    }
    Ok(groups)
}

/* a run target as source and base dir with a directory as its own base */
fn load_run(kit: &Kit, dir: &str, run: &str) -> Result<(String, String)> {
    let path = Path::new(dir).join(run);
    if path.is_dir() {
        let entry = path.join(kit.descriptor().entry());
        let source = std::fs::read_to_string(&entry).with_context(|| format!("reading {}", entry.display()))?;
        // the resolver walks up from `{base}edge.json` so a directory base needs a trailing slash
        let mut base = path.to_string_lossy().replace('\\', "/");
        if !base.ends_with('/') {
            base.push('/');
        }
        return Ok((source, base));
    }
    Ok((std::fs::read_to_string(&path)?, dir.to_string()))
}

/* auto or absent for a scheduler per core or else a count past zero */
fn threads(value: Option<&serde_yaml_ng::Value>) -> Result<usize> {
    let cores = || std::thread::available_parallelism().map_or(1, |n| n.get());
    match value {
        None => Ok(cores()),
        Some(v) if v.as_str() == Some("auto") => Ok(cores()),
        Some(v) => v.as_u64().filter(|&n| n > 0).map(|n| n as usize).ok_or_else(|| anyhow!("runtime.schedulers takes auto or a count of 1 or more")),
    }
}

/* the sink an out uri names where a file must open for append */
fn out(group: &str, uri: Option<&str>) -> Result<Out> {
    match uri {
        None | Some("stdout") => Ok(Out::Stdout),
        Some("null") => Ok(Out::Null),
        Some(uri) => {
            let Some(path) = uri.strip_prefix("file://") else { bail!("group '{group}' sets out to '{uri}', it takes stdout, null or file://<path>") };
            std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("opening the out file of group '{group}'"))?;
            Ok(Out::File(path.to_string()))
        }
    }
}

/* an address as actor.yml writes it without its scheme */
fn bare(addr: &str) -> String {
    addr.strip_prefix("tcp://").unwrap_or(addr).to_string()
}

/* the loopback addresses a control endpoint binds or why `addr` names another */
fn local(addr: &str) -> Result<Vec<SocketAddr>> {
    let addr = bare(addr);
    let loopback = |ip: std::net::IpAddr| ip == Ipv4Addr::LOCALHOST || ip == Ipv6Addr::LOCALHOST;
    // localhost names the loopback interface whatever a resolver would answer for it
    let found = match addr.rsplit_once(':') {
        Some((host, port)) if host.eq_ignore_ascii_case("localhost") => port.parse::<u16>().ok().map(|port| vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port)), SocketAddr::from((Ipv6Addr::LOCALHOST, port))]),
        _ => addr.parse::<SocketAddr>().ok().filter(|at| loopback(at.ip())).map(|at| vec![at]),
    };
    found.ok_or_else(|| {
        let port = addr.rsplit(':').next().filter(|p| p.parse::<u16>().is_ok()).unwrap_or("<port>");
        anyhow!("runtime.control is {addr}, but the control endpoint is local-only and binds only 127.0.0.1, ::1 or localhost, so reach it from another machine through an SSH tunnel such as ssh -L {port}:127.0.0.1:{port} <host>")
    })
}

/* the live server a listen address asks for */
#[cfg(feature = "actors-server")]
fn serve(kit: &Kit, pool: Pool, listen: String, control: Option<Vec<SocketAddr>>, durable: PathBuf) -> Result<i32> {
    Ok(server::serve(kit, pool, server::Serve { listen, control, durable }))
}

/* a composition without the server refuses a pool that asks for one */
#[cfg(not(feature = "actors-server"))]
fn serve(_kit: &Kit, _pool: Pool, _listen: String, _control: Option<Vec<SocketAddr>>, _durable: PathBuf) -> Result<i32> {
    bail!("runtime.listen needs the actors-server component, which this composition does not wire")
}

#[cfg(test)]
mod tests {
    use super::*;

    /* the control endpoint takes the loopback addresses alone in every spelling */
    #[test]
    fn a_control_address_off_loopback_is_refused() {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 9090));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 9090));
        assert_eq!(local("tcp://127.0.0.1:9090").unwrap(), [v4]);
        assert_eq!(local("[::1]:9090").unwrap(), [v6]);
        assert_eq!(local("tcp://localhost:9090").unwrap(), [v4, v6]);
        assert_eq!(local("LocalHost:9090").unwrap(), [v4, v6]);
        let refused = ["0.0.0.0:9090", "tcp://[::]:9090", "10.0.0.5:9090", "192.168.1.20:9090", "127.0.0.2:9090", "[::ffff:127.0.0.1]:9090", "example.com:9090", "localhost.example.com:9090", "localhost:x", "localhost", "127.0.0.1", ":9090"];
        for addr in refused {
            let why = local(addr).unwrap_err().to_string();
            assert!(why.contains("the control endpoint is local-only") && why.contains("SSH tunnel"), "{addr} gave {why}");
        }
        let why = local("tcp://0.0.0.0:9090").unwrap_err().to_string();
        assert!(why.starts_with("runtime.control is 0.0.0.0:9090, but") && why.ends_with("ssh -L 9090:127.0.0.1:9090 <host>"), "{why}");
        assert!(local("127.0.0.1").unwrap_err().to_string().contains("ssh -L <port>:127.0.0.1:<port> <host>"));
    }
}
