use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use crate::deps::walk::{self, Carry, Fetched, Loaded, Step, Walk};
use crate::deps::{DepHost, Deps, HostResolver, NativeBinding, Resolver};
use crate::lang::{ErrKind, Front, Runtime};
use crate::syntax::{chunk::SSAChunk, diag::Diagnostic};
use crate::value::{Limits, SchedulerStatus, VmErr};
use crate::vm::VM;

/* a source file compiled through a runtime's front end and ready to boot */
pub struct Script {
    pub path: String,
    pub src: String,
    pub chunk: SSAChunk,
}

impl Script {
    /* the file at `path` compiled with its imports read beside it or why not */
    pub fn read(front: &Front, path: &str) -> Result<Script, String> {
        let src = std::fs::read_to_string(path).map_err(|e| format!("error: cannot read '{path}': {e}"))?;
        Script::beside(front, Path::new(path), path, src)
    }

    /* `src` of the file at `at` named `shown` with imports read from its directory */
    fn beside(front: &Front, at: &Path, shown: &str, src: String) -> Result<Script, String> {
        let mut files = Files::of(at);
        files.gather(front, &src)?;
        Script::compile(front, shown, src, Box::new(HostResolver::new(String::new(), files)))
    }

    /* `src` compiled as the file at `path` with its imports through `resolver` */
    pub fn compile(front: &Front, path: &str, src: String, resolver: Box<dyn Resolver>) -> Result<Script, String> {
        let chunk = front.program(&src, resolver).map_err(|errs| Diagnostic::render_all(&errs, &src, Some(path)))?;
        let mut chunk = Rc::unwrap_or_clone(chunk);
        crate::code::constant_fold(&mut chunk, front.policy);
        Ok(Script { path: path.into(), src, chunk })
    }

    /* a VM over runtime `rt` that prints to stdout and reads its input from stdin */
    pub fn boot(&self, rt: &'static Runtime, limits: Limits) -> VM<'_> {
        let mut vm = VM::with_runtime(rt, &self.chunk, limits);
        vm.print_hook = Some(print);
        vm.strict_input = false;
        vm
    }

    /* runs `vm` to its end and gives the status the process exits with */
    pub fn run(&self, vm: &mut VM<'_>) -> u8 {
        let ran = loop {
            match vm.run() {
                Err(VmErr::HostYield(SchedulerStatus::Preempted)) => continue,
                ran => break ran,
            }
        };
        // what the program printed lands before any report of how it ended
        let _ = std::io::stdout().flush();
        let report = match ran {
            Ok(_) => return 0,
            Err(VmErr::HostYield(wait)) => {
                let what = if matches!(wait, SchedulerStatus::PendingEvent) { "an event" } else { "a host call" };
                format!("error: script suspended awaiting {what}, nothing can resume it here")
            }
            Err(e) => match vm.system_exit_code() {
                // an uncaught exit with an int code ends cleanly with its low byte
                Some(code) => return code as u8,
                None => vm.report_error(&e, &self.src, Some(&self.path)),
            },
        };
        fail(&report)
    }
}

/* hands the exact text a program prints to stdout */
pub fn print(text: &str) {
    // a closed stdout stops no run so a failed write is dropped
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

/* the main of a binary that runs the file its first argument names */
pub fn main(rt: &'static Runtime, front: &Front, limits: Limits) -> ExitCode {
    // a path that is no UTF-8 still opens and reads lossily in what is reported
    let mut args = std::env::args_os();
    let bin = args.next().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(path) = args.next() else {
        return ExitCode::from(fail(&format!("usage: {bin} <file.{}>", front.extension)));
    };
    let shown = path.to_string_lossy();
    let read = std::fs::read_to_string(&path).map_err(|e| format!("error: cannot read '{shown}': {e}"));
    match read.and_then(|src| Script::beside(front, Path::new(&path), &shown, src)) {
        Ok(script) => {
            let mut vm = script.boot(rt, limits);
            ExitCode::from(script.run(&mut vm))
        }
        Err(report) => ExitCode::from(fail(&report)),
    }
}

/* the modules a script imports read from the directory it sits in */
#[derive(Clone)]
struct Files {
    dir: Arc<PathBuf>,
    deps: Arc<Mutex<Deps>>,
}

impl Files {
    fn of(script: &Path) -> Files {
        let dir = script.parent().map(Path::to_path_buf).unwrap_or_default();
        Files { dir: Arc::new(dir), deps: Arc::new(Mutex::new(Deps::new())) }
    }

    /* walks every import `src` reaches and registers each module or gives the failures */
    fn gather(&mut self, front: &Front, src: &str) -> Result<(), String> {
        let mut walk = Walk::new(front, src, "", Vec::new(), Carry::default());
        loop {
            let Some(step) = walk::settle(self, walk.step()) else { continue };
            match step {
                Step::Fetch(spec) => walk.fetched(self.read(&spec)),
                Step::Plugin { .. } => walk.loaded(Loaded::Refused(String::from("is a plugin and a native script loads none"))),
                // a native script serves no system module and asks no registry
                Step::System(_) => walk.served(Vec::new()),
                Step::Undeclared(_) => walk.known(Vec::new()),
                Step::Done(failures) if failures.is_empty() => return Ok(()),
                Step::Done(failures) => return Err(failures.iter().map(|f| format!("error: {f}")).collect::<Vec<_>>().join("\n")),
                Step::Code { .. } | Step::Refuse { .. } | Step::Manifest { .. } => {}
            }
        }
    }

    /* the file `spec` names below the script's directory read whole */
    fn read(&self, spec: &str) -> Fetched {
        let target = spec.split_once('#').map_or(spec, |(t, _)| t);
        if target.contains("://") { return Fetched::Failed(format!("module '{target}' is a url and a native script reads local files only")); }
        let file = self.dir.join(target.trim_start_matches("./"));
        match std::fs::read(&file) {
            Ok(bytes) => Fetched::Bytes(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Fetched::Missing(None),
            Err(e) => Fetched::Failed(format!("cannot read '{}': {e}", file.display())),
        }
    }
}

impl DepHost for Files {
    fn deps<R>(&mut self, f: impl FnOnce(&mut Deps) -> R) -> R { f(&mut self.deps.lock().unwrap_or_else(PoisonError::into_inner)) }

    fn fetch(&mut self, spec: &str, _pin: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
        match self.read(spec) {
            Fetched::Bytes(bytes) => Ok(bytes),
            Fetched::Missing(_) => Err(format!("no file '{spec}'")),
            Fetched::Failed(e) => Err(e),
        }
    }

    // no module of a native script is native so a binding only refuses
    fn native(&mut self, name: String, _id: u32, module: Arc<str>) -> NativeBinding {
        let refusal = format!("module '{module}' is native and a native script serves none");
        NativeBinding { name, func: Arc::new(move |_, _, _| Err(VmErr::Of(ErrKind::Runtime, refusal.clone()))) }
    }

    fn caller_dir(&mut self) -> String { String::new() }
}

/* writes `report` to stderr as one block and gives the failing status */
fn fail(report: &str) -> u8 {
    eprintln!("{}", report.trim_end());
    1
}
