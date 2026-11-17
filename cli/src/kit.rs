use anyhow::Result;
use clap::{Arg, ArgMatches};
#[cfg(feature = "wasm")]
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub use interpreter::embed::Limits;
pub use interpreter::lang::Descriptor;

use crate::host::{Access, Engine, HostModule, Names, Native, Open, Project, Sink, Wiring};
#[cfg(feature = "wasm")]
use crate::host::{linear_cap, Host, Loaded};
use crate::manifest::Manifest;

// the manifest key a project declares its runtime under and the variable that stands in for it
pub const RUNTIME: &str = "runtime";
pub const RUNTIME_ENV: &str = "INTERPRETER_RUNTIME";

// the library a native run's child loads which only the parent of that run sets
#[cfg(feature = "native")]
const HOSTED_ENV: &str = "INTERPRETER_NATIVE_HOSTED";

// the one line a run on a native runtime prints first
pub const NATIVE_WARNING: &str = "warning: native runtime, it runs outside the sandbox and the grants of interpreter.json do not hold it";

/* how a child that ended with no exit code ended */
#[cfg(feature = "native")]
fn crashed(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(status) {
        return match signal {
            6 => String::from("it aborted"),
            11 => String::from("it read memory it does not own"),
            n => format!("signal {n} ended it"),
        };
    }
    format!("it ended with {status}")
}

/* a run error in the words of the runtime over its source and byte */
pub type Words = fn(&str, &str, Option<usize>) -> String;

/* a runtime as the engine that runs it and the names its files and errors take */
pub struct Runtime {
    engine: Engines,
    descriptor: &'static Descriptor,
    // how the runtime words a run error where none leaves the engine's own report
    words: Option<Words>,
}

/* what runs the programs of a runtime */
enum Engines {
    // a wasm module with the native code wasmtime precompiled from it or none to compile it on load
    #[cfg(feature = "wasm")]
    Wasm { module: Cow<'static, [u8]>, native: Option<&'static [u8]> },
    // a runtime built into this binary whose metering needs `interpreter::allocator!()`
    Native(&'static interpreter::lang::Runtime),
    // a native library a child process of this binary loads where it never reaches a page
    #[cfg(feature = "native")]
    Library(Arc<crate::host::library::Library>),
}

impl Runtime {
    /// # Safety
    #[cfg(feature = "wasm")]
    pub unsafe fn precompiled(wasm: &'static [u8], native: &'static [u8], descriptor: &'static Descriptor) -> Runtime {
        // the caller vouches `native` is wasmtime's precompile of `wasm` under `host::config::base` for this target
        Runtime { engine: Engines::Wasm { module: Cow::Borrowed(wasm), native: Some(native) }, descriptor, words: None }
    }

    /* a module wasmtime compiles on its first load and maps from the cache after */
    #[cfg(feature = "wasm")]
    pub fn wasm(bytes: impl Into<Cow<'static, [u8]>>, descriptor: &'static Descriptor) -> Runtime {
        Runtime { engine: Engines::Wasm { module: bytes.into(), native: None }, descriptor, words: None }
    }

    /* the runtime `spec` names as a path below `base` or as a URL pinned with `#sha256-` */
    #[cfg(feature = "wasm")]
    pub fn declared(spec: &str, base: &Path) -> Result<Runtime> {
        let bytes = if spec.contains("://") {
            let (url, pin) = interpreter::deps::parse_integrity(spec).map_err(|e| anyhow::anyhow!("runtime '{spec}': {e}"))?;
            let Some(pin) = pin else { anyhow::bail!("the runtime '{spec}' comes from a URL so it pins its bytes with #sha256-<64 hex>") };
            crate::host::fetch_cached(url, Some(pin)).map_err(|e| anyhow::anyhow!("runtime '{spec}': {e}"))?
        } else {
            let path = base.join(spec);
            std::fs::read(&path).map_err(|e| anyhow::anyhow!("reading the runtime {}: {e}", path.display()))?
        };
        Runtime::loaded(bytes).map_err(|e| anyhow::anyhow!("runtime '{spec}': {e:#}"))
    }

    /* a runtime module whose names come from its own descriptor */
    #[cfg(feature = "wasm")]
    pub fn loaded(bytes: Vec<u8>) -> Result<Runtime> {
        if !bytes.starts_with(b"\0asm") {
            anyhow::bail!("the runtime is no .wasm module");
        }
        let descriptor = crate::host::describe(&bytes)?;
        Ok(Runtime::wasm(bytes, descriptor))
    }

    /* a runtime built into this binary as the native test boots lox */
    pub fn native(runtime: &'static interpreter::lang::Runtime) -> Runtime {
        Runtime { engine: Engines::Native(runtime), descriptor: runtime.descriptor, words: None }
    }

    /* the wording every host prints a run error of this runtime through */
    pub fn words(mut self, words: Words) -> Runtime {
        self.words = Some(words);
        self
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        self.descriptor
    }

    /* the module a browser host and a dist load which a runtime run natively has none of */
    pub fn module(&self) -> Option<&[u8]> {
        match &self.engine {
            #[cfg(feature = "wasm")]
            Engines::Wasm { module, .. } => Some(module),
            Engines::Native(_) => None,
            #[cfg(feature = "native")]
            Engines::Library(_) => None,
        }
    }

    /* the native library this runtime runs on, which a bundle carries beside the project */
    pub fn library_path(&self) -> Option<&Path> {
        match &self.engine {
            #[cfg(feature = "native")]
            Engines::Library(lib) => Some(&lib.path),
            _ => None,
        }
    }

    /* a native runtime library opened in this process, which only the child of a native run does */
    #[cfg(feature = "native")]
    pub fn library(path: &Path) -> Result<Runtime> {
        let lib = crate::host::library::Library::open(path)?;
        Ok(Runtime { descriptor: lib.descriptor, engine: Engines::Library(lib), words: None })
    }
}

/* a subcommand a composition wires into its kit */
pub trait Command {
    /* the clap spec whose words come from the kit and the runtime it hosts */
    fn spec(&self, kit: &Kit) -> clap::Command;

    /* the usage and the summary the top-level help lists this command under */
    fn summary(&self, kit: &Kit) -> (String, String);

    /* a label and the flags the top-level help gives them below the commands */
    fn flags(&self) -> Option<(&'static str, &'static str)> {
        None
    }

    /* the bundle its input carries whose runtime its programs run on */
    fn carried(&self, _m: &ArgMatches) -> Option<Vec<u8>> {
        None
    }

    /* whether it runs programs so the project declares its runtime before it */
    fn runs(&self) -> bool {
        self.compiles()
    }

    /* whether it compiles programs so the manifest checks run before it */
    fn compiles(&self) -> bool {
        false
    }

    /* the manifest its programs resolve through which those checks read */
    fn manifest(&self, m: &ArgMatches) -> PathBuf {
        manifest_path(m)
    }

    /* takes the whole process before any argument is read as a carried app does */
    fn boot(&self, _kit: &Kit) -> Option<Result<i32>> {
        None
    }

    /* runs with the matches of its own spec and returns the exit code */
    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32>;
}

/* a manifest key a component reads with the check its value must pass */
#[derive(Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    // why the value under the named key cannot stand in words of its own
    pub check: fn(&str, &serde_json::Value) -> Result<(), String>,
    // names no package or import may take since the field gives them meaning
    pub reserves: &'static [&'static str],
}

/* the core every command runs through over the runtime its composition hands it */
pub struct Kit {
    name: &'static str,
    version: &'static str,
    title: String,
    site: Option<&'static str>,
    // what a binary too old says
    upgrade: Option<&'static str>,
    // set at build for a fixed composition and from the project before a command that runs programs
    runtime: OnceLock<Runtime>,
    // what the project wrote to declare its runtime, the directory that spec reads from and whether it is native
    declaration: OnceLock<(String, PathBuf, bool)>,
    commands: Vec<Box<dyn Command>>,
    fields: Vec<Field>,
    modules: Vec<Arc<dyn HostModule>>,
    // without a grant component trusted runs reach every module wired and untrusted none
    access: Arc<dyn Access>,
    // what a run may spend and hold where its command line names no limit
    limits: Limits,
}

impl Kit {
    /* a kit named as its binary is with the version it reports */
    pub fn new(name: &'static str, version: &'static str, runtime: Runtime) -> Kit {
        let kit = Kit::per_project(name, version);
        let _ = kit.runtime.set(runtime);
        kit
    }

    /* a kit whose runtime each project declares in its manifest, with --runtime or in INTERPRETER_RUNTIME */
    pub fn per_project(name: &'static str, version: &'static str) -> Kit {
        let title = format!("The {name} developer CLI");
        let access = Arc::new(Open);
        Kit { name, version, title, site: None, upgrade: None, runtime: OnceLock::new(), declaration: OnceLock::new(), commands: Vec::new(), fields: Vec::new(), modules: Vec::new(), access, limits: Limits::sandbox() }
    }

    /* the first line of the top-level help */
    pub fn title(mut self, title: impl Into<String>) -> Kit {
        self.title = title.into();
        self
    }

    /* where the top-level help sends a reader last */
    pub fn site(mut self, site: &'static str) -> Kit {
        self.site = Some(site);
        self
    }

    /* how a user gets a newer binary when a project needs a later engine */
    pub fn upgrade(mut self, help: &'static str) -> Kit {
        self.upgrade = Some(help);
        self
    }

    /* the help a binary too old for a project prints after naming the engine */
    pub fn upgrade_help(&self) -> Option<&'static str> {
        self.upgrade
    }

    pub fn command(mut self, command: impl Command + 'static) -> Kit {
        self.commands.push(Box::new(command));
        self
    }

    /* a manifest key a save writes first in the order fields were declared */
    pub fn field(mut self, field: Field) -> Kit {
        self.fields.push(field);
        self
    }

    pub fn fields(mut self, fields: &[Field]) -> Kit {
        self.fields.extend_from_slice(fields);
        self
    }

    /* a module the host serves whose name no import or package may take */
    pub fn module(mut self, module: Arc<dyn HostModule>) -> Kit {
        self.modules.push(module);
        self
    }

    /* what each package holds of each module in place of every trusted run holding all */
    pub fn access(mut self, access: Arc<dyn Access>) -> Kit {
        self.access = access;
        self
    }

    /* what every run may spend and hold in place of the sandbox's limits */
    pub fn limits(mut self, limits: Limits) -> Kit {
        self.limits = limits;
        self
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /* whether the composition wires a command of this name */
    pub fn wires(&self, command: &str) -> bool {
        self.commands.iter().any(|c| c.spec(self).get_name() == command)
    }

    pub fn runtime(&self) -> &Runtime {
        self.runtime.get().expect("a command that runs programs declares its runtime before it runs")
    }

    /* the descriptor of the runtime once a fixed composition or a project gave one */
    pub fn described(&self) -> Option<&'static Descriptor> {
        self.runtime.get().map(|runtime| runtime.descriptor)
    }

    /* what the project wrote to name its runtime and the directory it reads from */
    pub fn declaration(&self) -> Option<&(String, PathBuf, bool)> {
        self.declaration.get()
    }

    /* runs every program on the runtime a bundle carries when no runtime was given before */
    #[cfg(feature = "wasm")]
    pub fn carry(&self, payload: &[u8]) -> Result<()> {
        if self.runtime.get().is_some() {
            return Ok(());
        }
        // a native runtime the bundle carries runs in a child over a copy of the library it unpacks
        if let Some(bytes) = crate::pack::carried_native(payload) {
            #[cfg(feature = "native")]
            if std::env::var_os(HOSTED_ENV).is_some() {
                return self.declare(Path::new(""), None, None);
            }
            return self.host_natively(&crate::pack::unpacked_native(&bytes)?);
        }
        let Some(bytes) = crate::pack::carried_runtime(payload) else { return Ok(()) };
        let _ = self.runtime.set(Runtime::loaded(bytes).map_err(|e| anyhow::anyhow!("the runtime the bundle carries: {e:#}"))?);
        Ok(())
    }

    #[cfg(not(feature = "wasm"))]
    pub fn carry(&self, _payload: &[u8]) -> Result<()> {
        Ok(())
    }

    /* loads the runtime the --runtime flag, the manifest or INTERPRETER_RUNTIME names in that order */
    #[cfg(feature = "wasm")]
    pub fn declare(&self, manifest: &Path, flag: Option<&str>, native: Option<&str>) -> Result<()> {
        if self.runtime.get().is_some() {
            return Ok(());
        }
        // the child of a native run loads the library its parent resolved and runs the command itself
        #[cfg(feature = "native")]
        if let Some(hosted) = std::env::var_os(HOSTED_ENV) {
            let path = PathBuf::from(hosted);
            let _ = self.runtime.set(Runtime::library(&path)?);
            let _ = self.declaration.set((path.to_string_lossy().into_owned(), PathBuf::from("."), true));
            return Ok(());
        }
        let here = PathBuf::from(".");
        let value = std::fs::read_to_string(manifest).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok()).map(|v| v[RUNTIME].clone());
        let dir = manifest.parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(|| here.clone(), Path::to_path_buf);
        let declared_native = match (native, flag, &value) {
            (Some(path), _, _) => Some((path.to_string(), here.clone())),
            (None, None, Some(serde_json::Value::Object(o))) => match o.get("native") {
                Some(serde_json::Value::String(path)) => Some((path.clone(), dir.clone())),
                _ => anyhow::bail!("\"{RUNTIME}\" in interpreter.json is a path to a .wasm or {{ \"native\": \"path\" }}"),
            },
            _ => None,
        };
        if let Some((spec, base)) = declared_native {
            if spec.contains("://") {
                anyhow::bail!("a native runtime loads only from a local path and '{spec}' is a URL, declare a .wasm to load a runtime from a URL");
            }
            return self.host_natively(&base.join(spec));
        }
        let written = value.and_then(|v| v.as_str().map(String::from));
        let (spec, base) = match (flag, written, std::env::var(RUNTIME_ENV).ok()) {
            (Some(flag), _, _) => (flag.to_string(), here),
            (None, Some(written), _) => (written, dir),
            (None, None, Some(env)) => (env, here),
            (None, None, None) => anyhow::bail!("no runtime is declared, name its .wasm under \"{RUNTIME}\" in interpreter.json, pass --runtime <file> or set {RUNTIME_ENV}"),
        };
        let runtime = Runtime::declared(&spec, &base)?;
        let _ = self.runtime.set(runtime);
        let _ = self.declaration.set((spec, base, false));
        Ok(())
    }

    /* reruns this command in a child that loads the native library at `path` and ends as it does */
    #[cfg(feature = "native")]
    fn host_natively(&self, path: &Path) -> Result<()> {
        let path = std::fs::canonicalize(path).map_err(|e| anyhow::anyhow!("reading the native runtime {}: {e}", path.display()))?;
        eprintln!("{NATIVE_WARNING}");
        let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("locating {}: {e}", self.name))?;
        let status = std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(HOSTED_ENV, &path).status().map_err(|e| anyhow::anyhow!("starting the native runtime: {e}"))?;
        std::process::exit(match status.code() {
            Some(code) => code,
            None => {
                crate::ui::error(&anyhow::anyhow!("the native runtime crashed, {}", crashed(&status)));
                1
            }
        })
    }

    #[cfg(all(feature = "wasm", not(feature = "native")))]
    fn host_natively(&self, path: &Path) -> Result<()> {
        anyhow::bail!("{} loads no native runtime such as {} without its native feature", self.name, path.display())
    }

    /* a kit built with no wasm engine runs only the runtime its composition fixed */
    #[cfg(not(feature = "wasm"))]
    pub fn declare(&self, _manifest: &Path, _flag: Option<&str>, _native: Option<&str>) -> Result<()> {
        match self.runtime.get() {
            Some(_) => Ok(()),
            None => anyhow::bail!("{} loads no runtime of a project without its wasm feature", self.name),
        }
    }

    /* the wasm module a page runs the programs of this kit on */
    pub fn page_module(&self) -> Result<&[u8]> {
        self.runtime().module().ok_or_else(|| anyhow::anyhow!("{} runs its runtime natively and a page runs only a wasm module", self.name))
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        self.runtime().descriptor
    }

    /* the wording the composition gave its runtime for the errors its runs raise */
    pub fn words(&self) -> Option<Words> {
        self.runtime.get().and_then(|runtime| runtime.words)
    }

    /* a run error of `src` in the runtime's words or as the engine reported */
    pub fn worded(&self, src: &str, rendered: &str) -> String {
        worded(self.words(), src, rendered)
    }

    pub fn declared(&self) -> &[Field] {
        &self.fields
    }

    /* the host modules the composition wired whose scopes a grant may name */
    pub fn wired(&self) -> &[Arc<dyn HostModule>] {
        &self.modules
    }

    /* the names of the host modules which the walk and the manifest rules reserve */
    pub fn reserved(&self) -> Vec<&str> {
        self.modules.iter().map(|module| module.name()).collect()
    }

    /* each name a declared field reserves beside the field reserving it */
    pub fn holders(&self) -> Vec<(&'static str, &'static str)> {
        self.fields.iter().flat_map(|field| field.reserves.iter().map(|holder| (*holder, field.name))).collect()
    }

    pub fn grants(&self) -> &dyn Access {
        &*self.access
    }

    /* the limits a run holds where its command line names none */
    pub fn run_limits(&self) -> Limits {
        self.limits
    }

    /* what the composition wired that every run of its runtime serves */
    pub fn wiring(&self) -> Arc<Wiring> {
        self.publish();
        Arc::new(Wiring::new(self.descriptor(), self.modules.clone(), self.access.clone(), self.holders(), self.limits))
    }

    /* the runtime loaded into wasmtime with the host modules every thread serves */
    #[cfg(feature = "wasm")]
    pub fn load(&self) -> Result<Arc<Loaded>> {
        match &self.runtime().engine {
            Engines::Wasm { module, native } => Loaded::new(module, *native, self.wiring()),
            Engines::Native(_) => anyhow::bail!("{} runs its runtime natively and loads no wasm module", self.name),
            #[cfg(feature = "native")]
            Engines::Library(_) => anyhow::bail!("{} runs a native runtime and loads no wasm module", self.name),
        }
    }

    /* the runtime an actor pool shares across its scheduler threads */
    #[cfg(feature = "wasm")]
    pub fn pool_runtime(&self) -> Result<crate::host::pooled::Runtime> {
        match &self.runtime().engine {
            #[cfg(feature = "native")]
            Engines::Library(lib) => Ok(crate::host::pooled::Runtime::Native(lib.clone(), self.wiring())),
            _ => Ok(crate::host::pooled::Runtime::Wasm(self.load()?)),
        }
    }

    /* one interpreter of the runtime printing through `print` over the files of `project` */
    pub fn engine(&self, print: Sink, project: Project) -> Result<Box<dyn Engine>> {
        match &self.runtime().engine {
            #[cfg(feature = "wasm")]
            Engines::Wasm { .. } => Ok(Box::new(Host::new(self.load()?)?.vm(print, project, None, None)?)),
            Engines::Native(runtime) => Ok(Box::new(Native::new(runtime, self.wiring(), print, project)?)),
            #[cfg(feature = "native")]
            Engines::Library(lib) => Ok(Box::new(crate::host::library::Vm::new(lib.clone(), self.wiring(), print, project)?)),
        }
    }

    /* an interpreter that only walks imports and runs nothing */
    pub(crate) fn side(&self) -> Result<Box<dyn crate::host::Side>> {
        match &self.runtime().engine {
            #[cfg(feature = "wasm")]
            Engines::Wasm { .. } => Ok(Box::new(Host::new(self.load()?)?.fresh(Box::new(|_: &str| {}), Project::default(), None, linear_cap(Limits::sandbox().memory))?)),
            Engines::Native(runtime) => Ok(Box::new(Native::new(runtime, self.wiring(), Box::new(|_: &str| {}), Project::default())?)),
            #[cfg(feature = "native")]
            Engines::Library(lib) => Ok(Box::new(crate::host::library::Instance::fresh(lib.clone(), self.wiring(), Box::new(|_: &str| {}), Project::default())?)),
        }
    }

    /* fixes the names the host reads for this process from the first kit that runs */
    fn publish(&self) {
        crate::host::name(Names { name: self.name.to_string() });
    }

    /* the manifest at `path` held to the engine rules and to every declared field */
    pub fn manifest(&self, path: &Path) -> Result<Manifest> {
        Manifest::load(path, self)
    }

    /* whether `path` names a source file of the runtime */
    pub fn is_source(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == self.descriptor().extension)
    }

    /* parses the process arguments and exits with the code of the command they name */
    pub fn main(self) -> ! {
        self.publish();
        for command in &self.commands {
            if let Some(outcome) = command.boot(&self) {
                std::process::exit(finish(outcome));
            }
        }
        let first = std::env::args().nth(1);
        match first.as_deref() {
            // only the top-level help is intercepted so a command's own help still reaches clap
            None | Some("-h" | "--help") => {
                println!("{}", self.help());
                std::process::exit(0);
            }
            Some("-v" | "--version") => {
                println!("{} {}", self.name, self.version);
                std::process::exit(0);
            }
            _ => {}
        }
        let specs: Vec<clap::Command> = self.commands.iter().map(|command| command.spec(&self)).collect();
        let names: Vec<String> = specs.iter().map(|spec| spec.get_name().to_string()).collect();
        let matches = self.root(specs).get_matches();
        let Some((name, sub)) = matches.subcommand() else { std::process::exit(2) };
        let Some(at) = names.iter().position(|known| known == name) else { std::process::exit(2) };
        let command = &self.commands[at];
        let flag = sub.get_one::<String>("runtime").map(String::as_str);
        let native = sub.get_one::<String>("runtime-native").map(String::as_str);
        // a bundle runs on the runtime it carries unless the command line names another
        if let (None, None, Some(payload)) = (flag, native, command.carried(sub))
            && let Err(e) = self.carry(&payload)
        {
            crate::ui::error(&e);
            std::process::exit(1);
        }
        if command.runs()
            && let Err(e) = self.declare(&command.manifest(sub), flag, native)
        {
            crate::ui::error(&e);
            std::process::exit(1);
        }
        if command.compiles()
            && let Err(e) = self.held(&command.manifest(sub))
        {
            crate::ui::error(&e);
            std::process::exit(1);
        }
        std::process::exit(finish(command.run(&self, sub)))
    }

    /* an old binary names the engine a project needs and the grants follow */
    fn held(&self, manifest: &Path) -> Result<()> {
        Manifest::check_engine(manifest, self)?;
        self.access.project(self, manifest, None)
    }

    fn root(&self, specs: Vec<clap::Command>) -> clap::Command {
        let manifest = Arg::new("manifest")
            .long("manifest")
            .global(true)
            .value_name("MANIFEST")
            .value_parser(clap::value_parser!(PathBuf))
            .help("Use a specific manifest instead of ./interpreter.json");
        let runtime = Arg::new("runtime")
            .long("runtime")
            .global(true)
            .value_name("RUNTIME")
            .help("Run the programs on this runtime instead of the one interpreter.json declares");
        let native = Arg::new("runtime-native")
            .long("runtime-native")
            .global(true)
            .value_name("LIBRARY")
            .conflicts_with("runtime")
            .help("Run the programs on this native library, which runs outside the sandbox");
        // added after the commands so each lists it below its own options
        clap::Command::new(self.name)
            .disable_help_subcommand(true)
            .color(clap::ColorChoice::Never)
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommands(specs)
            .arg(manifest)
            .arg(runtime)
            .arg(native)
    }

    /* written by hand so the three top-level help forms print alike */
    fn help(&self) -> String {
        let mut out = format!("{}\n\nUsage  {} <command> [options]\n\nCommands\n", self.title, self.name);
        for command in &self.commands {
            let (usage, summary) = command.summary(self);
            out.push_str(&format!("  {usage:<19}{summary}\n"));
        }
        out.push('\n');
        for (label, flags) in self.commands.iter().filter_map(|command| command.flags()) {
            out.push_str(&format!("{label:<19}{flags}\n"));
        }
        let globals = [("--manifest <file>", "default interpreter.json"), ("--runtime <file>", "default the runtime interpreter.json declares"), ("--runtime-native <library>", "a native runtime, outside the sandbox")];
        for (i, (flag, about)) in globals.iter().enumerate() {
            out.push_str(&format!("{:<19}{flag:<29}{about}\n", if i == 0 { "Global" } else { "" }));
        }
        out.push('\n');
        out.push_str(&format!("{} <command> -h for details \u{00b7} -v for version", self.name));
        if let Some(site) = self.site {
            out.push_str(&format!(" \u{00b7} {site}"));
        }
        out
    }
}

/* `rendered` in the runtime's words where it names a class and `words` has some */
pub fn worded(words: Option<Words>, src: &str, rendered: &str) -> String {
    let Some(words) = words else { return rendered.to_string() };
    let Some((head, at)) = raised(src, rendered) else { return rendered.to_string() };
    words(src, &head, at)
}

/* the class and message a report leads with and the byte its caret points at */
fn raised(src: &str, rendered: &str) -> Option<(String, Option<usize>)> {
    let mut lines = rendered.lines();
    let head = lines.next()?.strip_prefix("error: ")?;
    // a compile report names no class so only a raised error is reworded
    let (class, _) = head.split_once(": ")?;
    if class.is_empty() || !class.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let at = lines.find_map(|line| line.trim_start().strip_prefix("--> ")).and_then(|place| {
        let mut parts = place.rsplitn(3, ':');
        let col: usize = parts.next()?.parse().ok()?;
        let line: usize = parts.next()?.parse().ok()?;
        Some(byte_at(src, line, col))
    });
    Some((head.to_string(), at))
}

/* the byte of `src` the 1-indexed line and column of a report point at */
fn byte_at(src: &str, line: usize, col: usize) -> usize {
    let start: usize = src.split_inclusive('\n').take(line.saturating_sub(1)).map(str::len).sum();
    let rest = &src[start.min(src.len())..];
    // a column counts characters so an ascii line reads byte for byte
    start + rest.char_indices().nth(col.saturating_sub(1)).map_or(rest.len(), |(at, _)| at)
}

/* the code a command ended with where an error prints and fails */
fn finish(outcome: Result<i32>) -> i32 {
    outcome.unwrap_or_else(|e| {
        crate::ui::error(&e);
        1
    })
}

/* the manifest `--manifest` names as given */
pub fn manifest_flag(m: &ArgMatches) -> Option<PathBuf> {
    m.try_get_one::<PathBuf>("manifest").ok().flatten().cloned()
}

/* the manifest a command reads with interpreter.json beside the run by default */
pub fn manifest_path(m: &ArgMatches) -> PathBuf {
    manifest_flag(m).unwrap_or_else(|| PathBuf::from("interpreter.json"))
}

/* the root manifest programs in `dir` resolve through as the walk finds it nearest above */
pub fn root_manifest(m: &ArgMatches, dir: &str) -> PathBuf {
    if let Some(flag) = manifest_flag(m) {
        return flag;
    }
    let found = interpreter::deps::walk_up_dirs(dir).map(|up| PathBuf::from(format!("{up}interpreter.json"))).find(|path| path.is_file());
    found.unwrap_or_else(|| PathBuf::from("interpreter.json"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;


    /* a host module that serves nothing and takes any scope */
    struct Quiet;

    impl crate::host::HostModule for Quiet {
        fn name(&self) -> &str {
            "quiet"
        }

        fn scope(&self, _scope: &str) -> bool {
            true
        }

        fn reaches(&self, _call: &str, _args: &[crate::host::WireValue]) -> Result<Option<String>, crate::host::Fault> {
            Ok(None)
        }

        fn open(&self, _inst: &mut dyn crate::host::Serve, _spec: &str, _pkg: &str, _held: crate::host::Held) -> Result<(), String> {
            Ok(())
        }
    }

    // the lox runtime make wasm-cli builds which the unit tests run their programs on
    const WASM: &[u8] = include_bytes!("../../target/wasm32-unknown-unknown/cli/interpreter.wasm");

    /* a kit over the lox runtime with no component wired */
    pub fn bare() -> Kit {
        Kit::new("interpreter", "0.0.0", Runtime::wasm(WASM, &interpreter_lox::DESCRIPTOR))
    }

    /* a kit over the lox runtime with a quiet module and no commands */
    pub fn kit() -> Kit {
        let kit = bare().fields(crate::manifest::FIELDS);
        #[cfg(feature = "perms")]
        let kit = kit.field(crate::perms::FIELD).access(Arc::new(crate::perms::Perms));
        kit.field(crate::docs::FIELD).module(Arc::new(Quiet))
    }

    struct Probe;

    impl Command for Probe {
        fn spec(&self, kit: &Kit) -> clap::Command {
            clap::Command::new("probe").about(format!("Probe *{} files", kit.descriptor().test_suffix))
        }

        fn summary(&self, kit: &Kit) -> (String, String) {
            ("probe [path]".to_string(), format!("Probe *{} files", kit.descriptor().test_suffix))
        }

        fn flags(&self) -> Option<(&'static str, &'static str)> {
            Some(("Probe flags", "--deep"))
        }

        fn run(&self, _kit: &Kit, _m: &ArgMatches) -> Result<i32> {
            Ok(0)
        }
    }

    /* the top-level help lists each command the composition wired with words from its runtime */
    #[test]
    fn the_help_lists_what_the_composition_wired() {
        let kit = kit().title("The Lox developer CLI").site("example.com").command(Probe);
        let help = kit.help();
        assert!(help.starts_with("The Lox developer CLI\n\nUsage  interpreter <command> [options]\n\nCommands\n"), "{help}");
        assert!(help.contains("\n  probe [path]       Probe *_test.lox files\n\nProbe flags        --deep\nGlobal             --manifest <file>            default interpreter.json\n                   --runtime <file>             default the runtime interpreter.json declares\n                   --runtime-native <library>   a native runtime, outside the sandbox\n\n"), "{help}");
        assert!(help.ends_with("interpreter <command> -h for details \u{00b7} -v for version \u{00b7} example.com"), "{help}");
    }

    /* the global manifest flag reaches the matches of every command */
    #[test]
    fn the_manifest_flag_reaches_each_command() {
        let kit = kit().command(Probe);
        let matches = kit.root(vec![Probe.spec(&kit)]).try_get_matches_from(["interpreter", "--manifest", "custom.json", "probe"]).unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert_eq!(manifest_path(sub), PathBuf::from("custom.json"));
        let matches = kit.root(vec![Probe.spec(&kit)]).try_get_matches_from(["interpreter", "probe"]).unwrap();
        assert_eq!(manifest_path(matches.subcommand().unwrap().1), PathBuf::from("interpreter.json"));
    }

    // the words of a runtime read the class and the place of a report
    #[test]
    fn a_raised_error_reaches_the_runtimes_words_at_the_byte_its_caret_names() {
        let src = "total:\n  count: n = type + 1\n";
        let rendered = "error: WrongType: cannot add text and number\n  --> main.flow.yaml:2:14\n  |\n2 |   count: n = type + 1\n  |              ^\n";
        let words = |src: &str, head: &str, at: Option<usize>| format!("{head} at {:?} of {}", at, src.len());
        assert_eq!(worded(Some(words), src, rendered), "WrongType: cannot add text and number at Some(20) of 29");
        assert_eq!(&src[20..24], "type");
        // a report of the engine stands as rendered for a runtime that words nothing
        assert_eq!(worded(None, src, rendered), rendered);
    }

    /* a compile report names no class so the words of a runtime never reword it */
    #[test]
    fn a_diagnostic_keeps_the_report_the_engine_rendered() {
        let src = "total:\n  wher: 1\n";
        let words = |_: &str, _: &str, _: Option<usize>| String::from("reworded");
        for rendered in ["error: report total has no key \"wher\", did you mean where\n  --> f:2:3\n", "error: module 'x' parse error: nothing\n", "no report at all\n"] {
            assert_eq!(worded(Some(words), src, rendered), rendered, "{rendered}");
        }
    }

    #[test]
    fn a_source_is_a_file_with_the_runtime_extension() {
        let kit = kit();
        assert!(kit.is_source(Path::new("lib/util.lox")));
        assert!(!kit.is_source(Path::new("lib/util.wasm")));
        assert_eq!(kit.reserved(), ["quiet"]);
    }
}
