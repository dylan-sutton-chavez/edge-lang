mod access;
#[cfg(feature = "web")]
pub mod browser;
pub mod driver;
mod engine;
mod module;
mod native;
mod resolver;
pub mod scan;
mod send;
mod shared;
#[cfg(feature = "wasm")]
mod wasm;

pub use access::{Access, Closed, Held, Open, Package, Packages, Scopes, Untrusted};
pub use edge::abi::{ErrorKind, WireValue};
pub use engine::{snapshot_source, Completion, Engine, Status};
pub use module::{Calls, Reply};
pub use native::Native;
pub use resolver::{fetch_cached, Fetched, Project, Remote, Walk};
pub(crate) use resolver::Side;
pub use send::{ChannelSink, Senders, FILE};
pub use shared::{Hosted, Hosting, Opened, Waits, Wiring};
#[cfg(feature = "wasm")]
pub use wasm::{config, linear_cap, time_limit, Host, Instance, Loaded, Vm, TICK_NS};

use edge::value::Limits;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::kit::Descriptor;

// a build pulls dozens of files so one reset among them should not end it
const ATTEMPTS: usize = 3;

// how long a connection and a whole fetch may take before either fails
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// the most one download may hold since the largest module is well under a megabyte
pub const MAX_FETCH_BYTES: u64 = 64 << 20;

/* where a composition's packages come from and what its caches and variables are named */
#[derive(Clone, Debug, Default)]
pub struct Names {
    // the binary's name which its cache dirs and variables follow
    pub name: String,
    // the origin package urls start with which untrusted runs alone may fetch from
    pub origin: Option<String>,
    // the registry the package commands ask
    pub registry: Option<String>,
}

static NAMES: OnceLock<Names> = OnceLock::new();

/* the names of the composition this process runs as its kit set them first */
pub fn names() -> &'static Names {
    NAMES.get_or_init(|| {
        let name = std::env::current_exe().ok().and_then(|exe| exe.file_stem().map(|stem| stem.to_string_lossy().into_owned()));
        Names { name: name.unwrap_or_default(), origin: None, registry: None }
    })
}

/* fixes the names every later call of this process reads where the first kit wins */
pub fn name(names: Names) {
    let _ = NAMES.set(names);
}

/* an engine lock message with the command this binary writes the lock with */
pub fn lock_help(msg: &str) -> String {
    match msg.ends_with(&format!(" in {}", edge::deps::lock::FILE)) {
        true => format!("{msg}\nhelp: run {} lock", names().name),
        false => msg.to_string(),
    }
}

/* a variable of the composition such as `EDGE_CDN_BASE` named after its binary */
fn variable(suffix: &str) -> Option<String> {
    let name = names().name.to_ascii_uppercase().replace('-', "_");
    std::env::var(format!("{name}_{suffix}")).ok()
}

/* the registry endpoint for `path` which tests and staging move with `<NAME>_SITE_BASE` */
pub fn site(path: &str) -> Result<String, String> {
    let base = variable("SITE_BASE").or_else(|| names().registry.clone()).ok_or_else(|| format!("{} names no registry to ask", names().name))?;
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/* tests and staging serve the composition's origin from `<NAME>_CDN_BASE` which production never sets */
pub fn cdn(url: &str) -> String {
    match &names().origin {
        Some(origin) => rebase(origin, url, variable("CDN_BASE").as_deref(), variable("SITE_BASE").is_some()),
        None => url.to_string(),
    }
}

/* whether an untrusted run may fetch `url` which only the composition's origin serves */
pub fn admitted(url: &str) -> bool {
    names().origin.as_ref().is_some_and(|origin| url.strip_prefix(origin.as_str()).is_some_and(|rest| rest.starts_with('/')))
}

// real registry packages live only on the real CDN and move only with the registry
fn rebase(origin: &str, url: &str, base: Option<&str>, registry_moved: bool) -> String {
    match (url.strip_prefix(origin), base) {
        (Some(path), Some(base)) if registry_moved || !path.starts_with("/pkg/") => format!("{}{path}", base.trim_end_matches('/')),
        _ => url.to_string(),
    }
}

/* retries hiccups with a growing pause and returns any answer about the resource at once */
pub fn get(source: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let call = || ureq::get(source).config().timeout_connect(Some(CONNECT_TIMEOUT)).timeout_global(Some(FETCH_TIMEOUT)).build().call();
    let mut pause = std::time::Duration::from_millis(200);

    for _ in 1..ATTEMPTS {
        match call() {
            Err(e) if again(&e) => std::thread::sleep(pause),
            done => return done,
        }
        pause *= 3;
    }

    call()
}

/* the whole body of a download refused past the one cap every fetch keeps */
pub fn body(resp: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, ureq::Error> {
    resp.body_mut().with_config().limit(MAX_FETCH_BYTES).read_to_vec()
}

/* a reset or an overloaded server may answer next time while a 404 already answered */
fn again(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(code) => *code == 429 || *code >= 500,
        // a server silent for the whole budget gets no second one
        ureq::Error::Timeout(ureq::Timeout::Global) => false,
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed | ureq::Error::Protocol(_) => true,
        _ => false,
    }
}

/* the per-user cache of the composition holding the modules runs fetched */
pub fn cache_root() -> Result<PathBuf, String> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(x).join(&names().name));
    }
    let home = std::env::var("HOME").map_err(|_| "cannot locate a cache dir (no HOME)".to_string())?;
    Ok(PathBuf::from(home).join(".cache").join(&names().name))
}

/* downloads stay out of the wiped cache as a browser never refetches like a module */
pub fn data_root() -> Result<PathBuf, String> {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(x).join(&names().name));
    }
    let home = std::env::var("HOME").map_err(|_| "cannot locate a data dir (no HOME)".to_string())?;
    Ok(PathBuf::from(home).join(".local").join("share").join(&names().name))
}

// wall-clock nanoseconds that every actor deadline is minted against
pub fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

pub type Sink = Box<dyn FnMut(&str) + Send>;

/* whether a bind address is the loopback interface a request must then name */
pub fn loopback_bind(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/* whether a request's Host names the loopback address on `port` where no browser omits one */
pub fn hosted_here(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return true };
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| host.eq_ignore_ascii_case(&format!("{name}:{port}")) || (port == 80 && host.eq_ignore_ascii_case(name)))
}

/* whether a walk enters `dir` first by its real path so a link loop ends */
pub fn first_visit(seen: &mut std::collections::HashSet<PathBuf>, dir: &std::path::Path) -> bool {
    std::fs::canonicalize(dir).is_ok_and(|real| seen.insert(real))
}

/* whether `path` is names joined by slashes below a directory and nothing above it */
pub use edge::deps::rules::plain as plain_path;

// each registered module spec with the native table slice it occupies
pub type Registered = HashMap<String, (usize, Vec<String>)>;

/* where a host module opened for a package serves its calls */
pub trait Serve {
    /* where the files of the run come from which a module reads its own dirs by */
    fn project(&self) -> &Project;

    /* serves the calls `module` opened under `spec` with `names` in the order their index answers */
    fn serve(&mut self, spec: &str, module: &str, names: &[&str], calls: Box<dyn Calls>) -> Result<(), String>;
}

/* a module the host serves under a name no import may shadow */
pub trait HostModule: Send + Sync {
    fn name(&self) -> &str;

    /* whether a grant may give the module `scope` after its name and a colon */
    fn scope(&self, scope: &str) -> bool;

    /* the scope `call` reaches with `args` which the kit checks first or none */
    fn reaches(&self, call: &str, args: &[WireValue]) -> Result<Option<String>, Fault>;

    /* whether `scope` from a grant covers the scope a call reaches */
    fn covers(&self, scope: &str, reached: &str) -> bool {
        scope == reached
    }

    /* what a refusal names as asked for a reached scope no held one covers */
    fn asked(&self, _held: &[String], reached: &str) -> String {
        reached.to_string()
    }

    /* registers the calls of the module in `inst` under `spec` for `pkg` holding `held` */
    fn open(&self, inst: &mut dyn Serve, spec: &str, pkg: &str, held: Held) -> Result<(), String>;

    /* whether what a package holds lets it write `path` through a file channel */
    fn writes(&self, _held: &Held, _path: &str) -> bool {
        false
    }
}

/* a failed host call as an abi error kind and the text the program reads */
#[derive(Clone, Debug)]
pub struct Fault {
    pub kind: ErrorKind,
    // the class a custom kind raises which the runtime's descriptor names
    pub class: Option<Class>,
    pub text: String,
}

/* the roles of the classes a refused or failed host call raises */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class { Denied, Failed }

impl Fault {
    pub fn new(kind: ErrorKind, text: impl Into<String>) -> Fault {
        Fault { kind, class: None, text: text.into() }
    }

    /* the kind every failure without one of its own takes */
    pub fn runtime(text: impl Into<String>) -> Fault {
        Fault::new(ErrorKind::Runtime, text)
    }

    /* a call whose arguments the module refuses before it reaches anything */
    pub fn invalid(text: impl Into<String>) -> Fault {
        Fault::new(ErrorKind::Value, text)
    }

    /* a call no grant of the package covers */
    pub fn denied(text: impl Into<String>) -> Fault {
        Fault { class: Some(Class::Denied), ..Fault::new(ErrorKind::Custom, text) }
    }

    /* a call that reached out and met a failure */
    pub fn failed(text: impl Into<String>) -> Fault {
        Fault { class: Some(Class::Failed), ..Fault::new(ErrorKind::Custom, text) }
    }

    /* the text the program reads with the class the runtime names in front */
    pub fn message(&self, d: &Descriptor) -> String {
        match self.class {
            Some(Class::Denied) => format!("{}: {}", d.denied_error, self.text),
            Some(Class::Failed) => format!("{}: {}", d.failed_error, self.text),
            None => self.text.clone(),
        }
    }
}

/* what `--memory` in MB and `--ops` asked for with `None` for the composition's value */
#[derive(clap::Args, Clone, Copy, Default)]
pub struct RunLimits {
    /// Memory a run may hold, in MB
    #[arg(long, value_name = "MB")]
    pub memory: Option<usize>,
    /// Operations a run may spend
    #[arg(long)]
    pub ops: Option<u64>,
}

static RUN_LIMITS: OnceLock<RunLimits> = OnceLock::new();

impl RunLimits {
    /* sets them once from the command line for every run this process starts */
    pub fn set(self) {
        let _ = RUN_LIMITS.set(self);
    }

    pub fn get() -> RunLimits {
        RUN_LIMITS.get().copied().unwrap_or_default()
    }

    /* the limits a run holds where each flag given replaces the one `base` sets */
    pub fn over(self, base: Limits) -> Limits {
        Limits { memory: self.memory.map_or(base.memory, |mb| mb.saturating_mul(1 << 20)), ops: self.ops.unwrap_or(base.ops) }
    }

    /* the limit flags of `command` with help naming the defaults `limits` gives */
    pub fn shown(command: clap::Command, limits: Limits) -> clap::Command {
        command
            .mut_arg("memory", |arg| arg.help(format!("Memory a run may hold, in MB [default: {}]", limits.memory >> 20)))
            .mut_arg("ops", |arg| arg.help(format!("Operations a run may spend [default: {}]", limits.ops)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // a package follows its registry while everything built comes from the staged CDN
    #[test]
    fn a_staged_run_reads_its_cdn_but_the_real_registrys_packages() {
        let (origin, staged) = ("https://cdn.example", "http://127.0.0.1:8788");
        for path in ["/edge.wasm", "/js/src/index.js", "/cli/install.sh"] {
            assert_eq!(rebase(origin, &format!("{origin}{path}"), Some(staged), false), format!("{staged}{path}"));
        }
        let pkg = format!("{origin}/pkg/test/0.1.0/app.edge");
        assert_eq!(rebase(origin, &pkg, Some(staged), false), pkg);
        assert_eq!(rebase(origin, &pkg, Some(staged), true), format!("{staged}/pkg/test/0.1.0/app.edge"));
    }

    // a refused or failed call raises the class its runtime names for that role
    #[test]
    fn a_host_fault_reads_its_class_from_the_runtime() {
        let named = Descriptor { denied_error: "Refused", failed_error: "Broken", ..mini::DESCRIPTOR };
        assert_eq!(Fault::denied("no grant").message(&named), "Refused: no grant");
        assert_eq!(Fault::failed("gone").message(&named), "Broken: gone");
        assert_eq!(Fault::invalid("bad").message(&named), "bad");
        assert_eq!(Fault::denied("no grant").message(&mini::DESCRIPTOR), "PermissionError: no grant");
    }
}
