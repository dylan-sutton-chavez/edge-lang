#[cfg(feature = "web")]
use anyhow::anyhow;
use anyhow::Result;
use clap::{ArgMatches, Args, FromArgMatches};
#[cfg(feature = "web")]
use std::path::Path;
use std::path::PathBuf;

use crate::host::driver::{self, RunOpts};
#[cfg(feature = "web")]
use crate::host::browser;
use crate::host::RunLimits;
use crate::kit::{self, Command, Kit};

#[derive(Args)]
struct RunArgs {
    /// Script, a packed .edge, an app binary, or stdin when omitted.
    file: Option<PathBuf>,
    /// Run this code inline instead of a file or stdin.
    #[arg(short = 'c', conflicts_with = "file")]
    code: Option<String>,
    /// Feed each line of this file (or FIFO) to the run as one event.
    #[arg(long)]
    events: Option<PathBuf>,
    /// Snapshot to this file when the script suspends on an unservable wait.
    #[arg(long)]
    save_state: Option<PathBuf>,
    /// Boot from a snapshot instead of a script and keep running.
    #[arg(long)]
    restore_state: Option<PathBuf>,
    /// Yield every n loop back-edges and resume.
    #[arg(long)]
    preempt: Option<usize>,
    /// Run on the browser host in headless Chrome instead of the native engine.
    #[cfg(feature = "web")]
    #[arg(long)]
    web: bool,
    #[command(flatten)]
    limits: RunLimits,
}

/* `run` over a script or a packed bundle or stdin or inline code */
pub struct Run;

impl Command for Run {
    fn spec(&self, kit: &Kit) -> clap::Command {
        RunLimits::shown(RunArgs::augment_args(clap::Command::new("run").about("Run a script")), kit.run_limits())
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("run <file|.edge>".to_string(), format!("Run a script, a .edge, stdin or -c <code>{}", crate::web::FLAG))
    }

    fn flags(&self) -> Option<(&'static str, &'static str)> {
        Some(("Run flags", "--events <f>  --save-state <f>  --restore-state <f>  --preempt <n>"))
    }

    fn compiles(&self) -> bool {
        true
    }

    // a script resolves through the manifest nearest above it and a bundle through its own
    fn manifest(&self, m: &ArgMatches) -> PathBuf {
        match m.try_get_one::<PathBuf>("file").ok().flatten() {
            Some(file) if crate::pack::file_payload(file).is_none() => kit::root_manifest(m, &edge::deps::dir_of(&driver::path_spec(file))),
            _ => kit::manifest_path(m),
        }
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let args = RunArgs::from_arg_matches(m)?;
        args.limits.set();
        let manifest = kit::manifest_flag(m);
        #[cfg(feature = "web")]
        if args.web {
            // flags only the native engine serves are refused here rather than ignored
            let native_only = [("--events", args.events.is_some()), ("--save-state", args.save_state.is_some()), ("--restore-state", args.restore_state.is_some()), ("--preempt", args.preempt.is_some())];
            return match native_only.iter().find(|(_, given)| *given) {
                Some((flag, _)) => Err(anyhow!("{flag} belongs to the native engine, drop it or drop --web")),
                None => web_run(kit, args.file.as_deref(), args.code.as_deref(), manifest.as_deref()),
            };
        }
        let opts = RunOpts {
            manifest: manifest.as_deref().map(|p| p.to_string_lossy().replace('\\', "/")),
            preempt: args.preempt.unwrap_or(0),
            events: args.events.map(|p| p.to_string_lossy().into_owned()),
            save_state: args.save_state.map(|p| p.to_string_lossy().into_owned()),
            restore_state: args.restore_state.map(|p| p.to_string_lossy().into_owned()),
        };
        driver::run(kit, args.file.as_deref(), args.code.as_deref(), &opts)
    }
}

/* runs a script on the browser host taking its source the way `edge run` does */
#[cfg(feature = "web")]
fn web_run(kit: &Kit, file: Option<&Path>, code: Option<&str>, manifest: Option<&Path>) -> Result<i32> {
    let src = match (file, code) {
        (_, Some(code)) => code.to_string(),
        (Some(path), None) => std::fs::read_to_string(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?,
        (None, None) => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).map_err(|e| anyhow!("reading stdin: {e}"))?;
            buf
        }
    };

    let default = PathBuf::from("edge.json");
    // the entry names of a native run so a traceback reads alike on both hosts
    let entry = match (file, code) {
        (_, Some(_)) => String::from("<eval>"),
        (Some(path), None) => driver::path_spec(path),
        (None, None) => String::from("<stdin>"),
    };
    browser::run(kit, &src, &entry, Some(manifest.unwrap_or(default.as_path())))
}
