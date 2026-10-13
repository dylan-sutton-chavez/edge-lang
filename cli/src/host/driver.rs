use super::{now_ns, Host, Project, Runtime, Sink, Status, Vm};
use anyhow::{anyhow, bail, Result};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::rc::Rc;

/* Run flags, every path is host-side, none reach the sandboxed script. */
#[derive(Default)]
pub struct RunOpts {
    pub manifest: Option<String>,
    pub preempt: usize,
    pub events: Option<String>,
    pub save_state: Option<String>,
    pub restore_state: Option<String>,
}

pub struct Outcome {
    pub err: Option<String>,
    pub exit_code: Option<i32>,
}

// Streams one print payload to stdout, flushed so it interleaves with stderr.
pub fn stdout_sink() -> Sink {
    Box::new(|s: &str| {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    })
}

fn host() -> Result<Rc<Host>> {
    Host::new(Runtime::new()?)
}

/* Forward-slash spec of a path, the shape the resolver walks. */
pub fn path_spec(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/* One-shot run of a file, inline code, stdin or a packed artifact, the process exit code. */
pub fn run(file: Option<&Path>, code: Option<&str>, opts: &RunOpts) -> Result<i32> {
    if let Some(state) = &opts.restore_state {
        return restore_and_run(state, opts);
    }
    if let Some(path) = file
        && let Some(payload) = crate::cmd::build::file_payload(path)
    {
        return run_bundle(&payload, opts);
    }
    let mut stdin = std::io::stdin();
    let (src, name) = match (code, file) {
        (Some(c), _) => (c.to_string(), String::from("<eval>")),
        (None, Some(p)) => (std::fs::read_to_string(p).map_err(|e| anyhow!("reading {}: {e}", p.display()))?, path_spec(p)),
        (None, None) => {
            // A bare `edge run` from a terminal would block on stdin forever, force a pipe or path.
            if stdin.is_terminal() {
                bail!("no script given; pass a file path or pipe Python to stdin");
            }
            let mut s = String::new();
            stdin.read_to_string(&mut s).map_err(|e| anyhow!("reading stdin: {e}"))?;
            (s, String::from("<stdin>"))
        }
    };
    // Unless the script itself came from stdin, piped stdin feeds `input()`.
    let mut input = None;
    if (file.is_some() || code.is_some()) && !stdin.is_terminal() {
        let mut buf = String::new();
        if stdin.read_to_string(&mut buf).is_ok() && !buf.is_empty() {
            input = Some(buf);
        }
    }
    let project = Project::disk(&name, opts.manifest.as_deref());
    let mut vm = host()?.vm(stdout_sink(), project, None, None)?;
    vm.set_preempt_interval(opts.preempt)?;
    let status = vm.start(&src, input.as_deref())?;
    Ok(drive(&mut vm, status, opts))
}

/* Runs a bundle's entry from memory, nothing is written to disk. */
pub fn run_bundle(payload: &[u8], opts: &RunOpts) -> Result<i32> {
    let bundle = crate::pack::Bundle::decode(payload).map_err(|e| anyhow!("corrupt bundle: {e}"))?;
    let entry = bundle.entry.clone();
    let files = crate::pack::into_files(bundle);
    let src = files.get(&entry).map(|b| String::from_utf8_lossy(b).into_owned()).ok_or_else(|| anyhow!("bundle entry '{entry}' is missing"))?;
    let project = Project::bundle(files, &entry, false);
    let mut vm = host()?.vm(stdout_sink(), project, None, None)?;
    vm.set_preempt_interval(opts.preempt)?;
    let status = vm.start(&src, None)?;
    Ok(drive(&mut vm, status, opts))
}

/* Boots from the blob's embedded source, overlays its saved state, keeps driving. */
pub fn restore_and_run(file: &str, opts: &RunOpts) -> Result<i32> {
    let blob = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: cannot read state '{file}': {e}");
            return Ok(2);
        }
    };
    let project = Project::disk("", opts.manifest.as_deref());
    let mut vm = host()?.vm(stdout_sink(), project, None, None)?;
    vm.set_preempt_interval(opts.preempt)?;
    let status = match vm.restore_state(&blob) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(1);
        }
    };
    Ok(drive(&mut vm, status, opts))
}

/* Serves one run to completion, timers sleep, host calls wait, unservable parks snapshot or fail. */
pub fn drive(vm: &mut Vm, mut status: Status, opts: &RunOpts) -> i32 {
    let mut events: Option<std::io::BufReader<std::fs::File>> = None;
    loop {
        status = match status {
            Status::Done => return 0,
            Status::Exit(code) => return code as i32,
            Status::Error(text) => {
                crate::ui::traceback(&text);
                return 1;
            }
            Status::Preempted => step(vm),
            Status::PendingTimer(deadline) => {
                sleep_until(deadline);
                step(vm)
            }
            Status::PendingEvent => {
                if vm.drain_buffered() > 0 {
                    step(vm)
                } else if let Some(path) = &opts.events {
                    match next_event(&mut events, path) {
                        Some(line) => {
                            vm.push_event(&line);
                            step(vm)
                        }
                        // A drained events file can never serve the wait, park terminally.
                        None => return park(vm, "an event", opts),
                    }
                } else {
                    return park(vm, "an event", opts)
                }
            }
            Status::PendingHostCall => {
                vm.dispatch();
                if vm.inflight() == 0 {
                    return park(vm, "a host call", opts);
                }
                if let Err(e) = vm.wait(None) {
                    eprintln!("error: {e}");
                    return 1;
                }
                step(vm)
            }
        };
    }
}

fn step(vm: &mut Vm) -> Status {
    match vm.resume() {
        Ok(status) => status,
        Err(e) => Status::Error(format!("error: {e}")),
    }
}

pub(super) fn sleep_until(deadline: u64) {
    let now = now_ns();
    if deadline > now {
        std::thread::sleep(std::time::Duration::from_nanos(deadline - now));
    }
}

/* The park report, a servable wait names its flag. */
pub fn suspend_message(what: &str) -> String {
    match what {
        "an event" => "script suspended awaiting an event (wire --events <file>)".to_string(),
        s => format!("script suspended awaiting {s}, nothing can resume it here"),
    }
}

/* Unservable park, snapshot when asked, otherwise report the missing wait and fail. */
fn park(vm: &mut Vm, what: &str, opts: &RunOpts) -> i32 {
    if let Some(file) = &opts.save_state {
        let blob = match vm.save_state() {
            Ok(Some(blob)) => blob,
            Ok(None) => {
                eprintln!("error: the run is not parked, nothing to save");
                return 1;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };
        if let Err(e) = std::fs::write(file, blob) {
            eprintln!("error: cannot write state to '{file}': {e}");
            return 1;
        }
        eprintln!("suspended awaiting {what}, state saved to '{file}'");
        return 0;
    }
    eprintln!("error: {}", suspend_message(what));
    1
}

/* Lazy line reader over `--events`, a FIFO blocks until a writer shows up, a file replays. */
fn next_event(reader: &mut Option<std::io::BufReader<std::fs::File>>, path: &str) -> Option<String> {
    if reader.is_none() {
        match std::fs::File::open(path) {
            Ok(f) => *reader = Some(std::io::BufReader::new(f)),
            Err(e) => {
                eprintln!("error: cannot open events '{path}': {e}");
                return None;
            }
        }
    }
    let mut line = String::new();
    match reader.as_mut()?.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches('\n').to_string()),
    }
}

/* A persistent interpreter for edge test, each input runs exactly once. */
pub struct Session {
    vm: Vm,
}

impl Session {
    pub fn open(manifest: Option<&Path>) -> Result<Session> {
        let project = Project::disk("", manifest.map(path_spec).as_deref());
        let vm = host()?.vm(stdout_sink(), project, None, None)?;
        Ok(Session { vm })
    }

    /* Runs one input as the script `entry`, None means the project root. */
    pub fn eval(&mut self, src: &str, entry: Option<&str>, input: Option<&str>) -> Result<Outcome> {
        self.vm.set_entry(entry.unwrap_or(""));
        let mut status = self.vm.repl_eval(src, input)?;
        loop {
            status = match status {
                Status::Done => return Ok(Outcome { err: None, exit_code: None }),
                Status::Exit(code) => return Ok(Outcome { err: None, exit_code: Some(code as i32) }),
                Status::Error(text) => return Ok(Outcome { err: Some(text), exit_code: None }),
                Status::Preempted => step(&mut self.vm),
                Status::PendingTimer(deadline) => {
                    sleep_until(deadline);
                    step(&mut self.vm)
                }
                Status::PendingHostCall => {
                    self.vm.dispatch();
                    if self.vm.inflight() == 0 {
                        return Ok(Outcome { err: Some(suspend_message("a host call")), exit_code: None });
                    }
                    self.vm.wait(None)?;
                    step(&mut self.vm)
                }
                Status::PendingEvent if self.vm.drain_buffered() > 0 => step(&mut self.vm),
                Status::PendingEvent => return Ok(Outcome { err: Some(suspend_message("an event")), exit_code: None }),
            };
        }
    }

    /* Wipes modules and state, the next input starts in a fresh namespace. */
    pub fn reset(&mut self) -> Result<()> {
        self.vm.reset()
    }
}
