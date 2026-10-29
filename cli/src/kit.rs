use anyhow::Result;
use clap::{Arg, ArgMatches};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use edge::embed::Limits;
pub use edge::lang::Descriptor;

use crate::host::{Access, HostModule, Loaded, Names, Open};
use crate::manifest::Manifest;

/* a run error in the words of the runtime over its source and byte */
pub type Words = fn(&str, &str, Option<usize>) -> String;

/* a runtime as its wasm module and the names its files and errors take */
pub struct Runtime {
    wasm: Cow<'static, [u8]>,
    // native code wasmtime precompiled from the module or none to compile it on load
    native: Option<&'static [u8]>,
    descriptor: &'static Descriptor,
    // how the runtime words a run error where none leaves the engine's own report
    words: Option<Words>,
}

impl Runtime {
    /// # Safety
    pub unsafe fn precompiled(wasm: &'static [u8], native: &'static [u8], descriptor: &'static Descriptor) -> Runtime {
        // the caller vouches `native` is wasmtime's precompile of `wasm` under `host::config::base` for this target
        Runtime { wasm: Cow::Borrowed(wasm), native: Some(native), descriptor, words: None }
    }

    /* a module wasmtime compiles on its first load and maps from the cache after */
    pub fn wasm(bytes: impl Into<Cow<'static, [u8]>>, descriptor: &'static Descriptor) -> Runtime {
        Runtime { wasm: bytes.into(), native: None, descriptor, words: None }
    }

    /* the wording every host prints a run error of this runtime through */
    pub fn words(mut self, words: Words) -> Runtime {
        self.words = Some(words);
        self
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        self.descriptor
    }

    /* the module itself which a browser host and a dist load */
    pub fn module(&self) -> &[u8] {
        &self.wasm
    }

    pub(crate) fn native(&self) -> Option<&'static [u8]> {
        self.native
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
    // where packages and the registry live and what a binary too old says
    origin: Option<&'static str>,
    registry: Option<&'static str>,
    upgrade: Option<&'static str>,
    runtime: Runtime,
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
        let title = format!("The {name} developer CLI");
        let access = Arc::new(Open);
        Kit { name, version, title, site: None, origin: None, registry: None, upgrade: None, runtime, commands: Vec::new(), fields: Vec::new(), modules: Vec::new(), access, limits: Limits::sandbox() }
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

    /* the origin package urls start with which untrusted runs alone may fetch from */
    pub fn origin(mut self, origin: &'static str) -> Kit {
        self.origin = Some(origin);
        self
    }

    /* the registry the package commands resolve names and publish through */
    pub fn registry(mut self, registry: &'static str) -> Kit {
        self.registry = Some(registry);
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
        &self.runtime
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        self.runtime.descriptor
    }

    /* the wording the composition gave its runtime for the errors its runs raise */
    pub fn words(&self) -> Option<Words> {
        self.runtime.words
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

    /* the runtime loaded into wasmtime with the host modules every thread serves */
    pub fn load(&self) -> Result<Arc<Loaded>> {
        self.publish();
        Loaded::new(&self.runtime, self.modules.clone(), self.access.clone(), self.holders(), self.limits)
    }

    /* fixes the names the host reads for this process from the first kit that runs */
    fn publish(&self) {
        crate::host::name(Names { name: self.name.to_string(), origin: self.origin.map(String::from), registry: self.registry.map(String::from) });
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
            .help("Use a specific manifest instead of ./edge.json");
        // added after the commands so each lists it below its own options
        clap::Command::new(self.name)
            .disable_help_subcommand(true)
            .color(clap::ColorChoice::Never)
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommands(specs)
            .arg(manifest)
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
        out.push_str(&format!("{:<19}--manifest <file>   default edge.json\n\n", "Global"));
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

/* the manifest a command reads with edge.json beside the run by default */
pub fn manifest_path(m: &ArgMatches) -> PathBuf {
    manifest_flag(m).unwrap_or_else(|| PathBuf::from("edge.json"))
}

/* the root manifest programs in `dir` resolve through as the walk finds it nearest above */
pub fn root_manifest(m: &ArgMatches, dir: &str) -> PathBuf {
    if let Some(flag) = manifest_flag(m) {
        return flag;
    }
    let found = edge::deps::walk_up_dirs(dir).map(|up| PathBuf::from(format!("{up}edge.json"))).find(|path| path.is_file());
    found.unwrap_or_else(|| PathBuf::from("edge.json"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edge.wasm"));
    const NATIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/edge.cwasm"));

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

        fn open(&self, _inst: &mut crate::host::Instance, _spec: &str, _pkg: &str, _held: crate::host::Held) -> Result<(), String> {
            Ok(())
        }
    }

    /* a kit over the reference runtime build.rs embeds with no component wired */
    pub fn bare() -> Kit {
        // SAFETY build.rs precompiled the cwasm from this module under the host's engine config
        let runtime = unsafe { Runtime::precompiled(WASM, NATIVE, &mini::DESCRIPTOR) };
        Kit::new("edge", "0.0.0", runtime)
    }

    /* a kit over the reference runtime build.rs embeds with a quiet module and no commands */
    pub fn kit() -> Kit {
        let kit = bare();
        // the registry fields come with the publish component when its feature is on
        #[cfg(feature = "publish")]
        let kit = kit.fields(crate::cmd::publish::FIELDS);
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
        let kit = kit().title("The Edge mini developer CLI").site("edgelang.dev").command(Probe);
        let help = kit.help();
        assert!(help.starts_with("The Edge mini developer CLI\n\nUsage  edge <command> [options]\n\nCommands\n"), "{help}");
        assert!(help.contains("\n  probe [path]       Probe *_test.mini files\n\nProbe flags        --deep\nGlobal             --manifest <file>   default edge.json\n\n"), "{help}");
        assert!(help.ends_with("edge <command> -h for details \u{00b7} -v for version \u{00b7} edgelang.dev"), "{help}");
    }

    /* the global manifest flag reaches the matches of every command */
    #[test]
    fn the_manifest_flag_reaches_each_command() {
        let kit = kit().command(Probe);
        let matches = kit.root(vec![Probe.spec(&kit)]).try_get_matches_from(["edge", "--manifest", "custom.json", "probe"]).unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert_eq!(manifest_path(sub), PathBuf::from("custom.json"));
        let matches = kit.root(vec![Probe.spec(&kit)]).try_get_matches_from(["edge", "probe"]).unwrap();
        assert_eq!(manifest_path(matches.subcommand().unwrap().1), PathBuf::from("edge.json"));
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
        assert!(kit.is_source(Path::new("lib/util.mini")));
        assert!(!kit.is_source(Path::new("lib/util.wasm")));
        assert_eq!(kit.reserved(), ["quiet"]);
    }
}
