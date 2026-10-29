use super::{Host, Project, Sink, Status, Vm, FILE};
use anyhow::{anyhow, bail, Result};
use crate::kit::Kit;
use std::collections::BTreeMap;
use std::io::{BufRead, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// how long a run waits for the first byte a pipe gives
const FIRST_BYTE: Duration = Duration::from_millis(100);

/* run flags whose paths stay on the host side and never reach the sandboxed script */
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

// flushed per print so stdout interleaves with stderr
pub fn stdout_sink() -> Sink {
    Box::new(|s: &str| {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    })
}

fn host(kit: &Kit) -> Result<Rc<Host>> {
    Host::new(kit.load()?)
}

/* the named outputs of a run which print or write the file a name holds */
pub struct Channels {
    out: Sink,
    // a writer per file channel opened at its first line and flushed at the end
    files: BTreeMap<String, BufWriter<std::fs::File>>,
    // the channel the last printed line came from so a new one names itself first
    last: String,
}

/* the channels of a run its prints and sends write through in program order */
pub type Shared = Arc<Mutex<Channels>>;

fn lock(channels: &Shared) -> MutexGuard<'_, Channels> {
    channels.lock().unwrap_or_else(|e| e.into_inner())
}

impl Channels {
    pub fn new(out: Sink) -> Channels {
        Channels { out, files: BTreeMap::new(), last: String::new() }
    }

    /* channels over stdout shared by the prints of a run and its sends */
    pub fn stdout() -> Shared {
        Arc::new(Mutex::new(Channels::new(stdout_sink())))
    }

    /* the print sink of a run which writes between its channel lines */
    pub fn printer(channels: &Shared) -> Sink {
        let channels = channels.clone();
        Box::new(move |text: &str| lock(&channels).plain(text))
    }

    /* hands every line `vm` sends to these channels the moment the program sends it */
    pub fn serve(vm: &Vm, channels: &Shared) {
        let channels = channels.clone();
        vm.instance().borrow_mut().send_to(Box::new(move |channel: &str, body: &str| lock(&channels).send(channel, body)));
    }

    /* one line a program sent written where its channel says */
    fn send(&mut self, channel: &str, body: &str) -> Result<(), String> {
        match channel.strip_prefix(FILE) {
            Some(path) => self.write_file(path, body).map_err(|e| e.to_string()),
            None => {
                self.print(channel, body);
                Ok(())
            }
        }
    }

    // a print between channel lines makes the next channel line name its channel again
    fn plain(&mut self, text: &str) {
        (self.out)(text);
        self.last.clear();
    }

    // a named output prints its lines under its name as a run prints a report
    fn print(&mut self, channel: &str, body: &str) {
        if self.last != channel {
            (self.out)(&format!("{channel}\n"));
            self.last = channel.to_string();
        }
        (self.out)(&format!("{body}\n"));
    }

    /* a file channel writes the file its name holds made fresh at its first line */
    fn write_file(&mut self, path: &str, body: &str) -> Result<()> {
        if !super::plain_path(path) {
            bail!("the channel '{FILE}{path}' names no file below the directory of the run");
        }
        let file = match self.files.get_mut(path) {
            Some(open) => open,
            None => {
                let made = std::fs::File::create(inside(path)?).map_err(|e| anyhow!("writing the channel '{FILE}{path}': {e}"))?;
                self.files.entry(path.to_string()).or_insert(BufWriter::new(made))
            }
        };
        writeln!(file, "{body}").map_err(|e| anyhow!("writing the channel '{FILE}{path}': {e}"))
    }

    /* flushes what the file channels hold so a run leaves nothing in a buffer */
    pub fn finish(&mut self) -> Result<()> {
        for (path, file) in &mut self.files {
            file.flush().map_err(|e| anyhow!("writing the channel '{FILE}{path}': {e}"))?;
        }
        Ok(())
    }
}

/* where a channel's file really lies which must be below the run's directory */
fn inside(path: &str) -> Result<PathBuf> {
    let failed = |e: std::io::Error| anyhow!("writing the channel '{FILE}{path}': {e}");
    let root = std::env::current_dir().and_then(std::fs::canonicalize).map_err(failed)?;
    let name = Path::new(path);
    // a file still to come resolves through its directory and a dangling link never
    let real = match std::fs::canonicalize(name) {
        Ok(real) => real,
        Err(_) if std::fs::symlink_metadata(name).is_ok() => bail!("the channel '{FILE}{path}' is a link to nothing, a channel follows no link it cannot resolve"),
        Err(_) => {
            let dir = name.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
            std::fs::canonicalize(dir).map_err(failed)?.join(name.file_name().unwrap_or_default())
        }
    };
    if !real.starts_with(&root) {
        bail!("the channel '{FILE}{path}' leads through a link out of the directory of the run");
    }
    Ok(real)
}

/* stdin read on a thread so a pipe left open never holds a run back */
pub struct Piped {
    head: Receiver<Vec<u8>>,
    // the instant by which a first byte must arrive for the run to take input
    by: Instant,
}

impl Piped {
    /* starts reading when stdin holds input rather than the script itself */
    pub fn start(stdin: &std::io::Stdin, wanted: bool) -> Option<Piped> {
        if !wanted || stdin.is_terminal() {
            return None;
        }
        let (tx, head) = channel();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut first = [0u8; 1];
            let read = stdin.read(&mut first).unwrap_or(0);
            // a first byte says a writer is there and the rest may take longer
            if tx.send(first[..read].to_vec()).is_err() || read == 0 {
                return;
            }
            let mut rest = Vec::new();
            let _ = stdin.read_to_end(&mut rest);
            let _ = tx.send(rest);
        });
        Some(Piped { head, by: Instant::now() + FIRST_BYTE })
    }

    /* what a writer piped in or none from a pipe that gave nothing */
    pub fn text(self) -> Option<String> {
        let wait = self.by.saturating_duration_since(Instant::now());
        let mut bytes = self.head.recv_timeout(wait).ok()?;
        if bytes.is_empty() {
            return None;
        }
        bytes.extend(self.head.recv().ok()?);
        String::from_utf8(bytes).ok().filter(|text| !text.is_empty())
    }
}

/* a path with forward slashes as the resolver walks it */
pub fn path_spec(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/* a one-shot run from any source that returns the process exit code */
pub fn run(kit: &Kit, file: Option<&Path>, code: Option<&str>, opts: &RunOpts) -> Result<i32> {
    if let Some(state) = &opts.restore_state {
        return restore_and_run(kit, state, opts);
    }
    if let Some(path) = file
        && let Some(payload) = crate::pack::file_payload(path)
    {
        return run_bundle(kit, &payload, opts);
    }
    let mut stdin = std::io::stdin();
    let (src, name) = match (code, file) {
        (Some(c), _) => (c.to_string(), String::from("<eval>")),
        (None, Some(p)) => (std::fs::read_to_string(p).map_err(|e| anyhow!("reading {}: {e}", p.display()))?, path_spec(p)),
        (None, None) => {
            // a bare `edge run` at a terminal would wait on stdin forever
            if stdin.is_terminal() {
                bail!("no script given; pass a file path or pipe {} to stdin", kit.descriptor().language);
            }
            let mut s = String::new();
            stdin.read_to_string(&mut s).map_err(|e| anyhow!("reading stdin: {e}"))?;
            (s, String::from("<stdin>"))
        }
    };
    // piped stdin feeds `input()` unless the script came from it
    let piped = Piped::start(&stdin, file.is_some() || code.is_some());
    let project = Project::disk(&name, opts.manifest.as_deref());
    let channels = Channels::stdout();
    let mut vm = host(kit)?.vm(Channels::printer(&channels), project, None, None)?;
    Channels::serve(&vm, &channels);
    vm.set_preempt_interval(opts.preempt)?;
    let input = piped.and_then(Piped::text);
    let status = vm.start(&src, input.as_deref())?;
    Ok(drive(kit, &mut vm, status, opts, &src, &channels))
}

/* runs the entry of a bundle from memory without writing to disk */
pub fn run_bundle(kit: &Kit, payload: &[u8], opts: &RunOpts) -> Result<i32> {
    let bundle = crate::pack::Bundle::decode(payload).map_err(|e| anyhow!("corrupt bundle: {e}"))?;
    let entry = bundle.entry.clone();
    let files = crate::pack::into_files(bundle);
    let src = files.get(&entry).map(|b| String::from_utf8_lossy(b).into_owned()).ok_or_else(|| anyhow!("bundle entry '{entry}' is missing"))?;
    let project = Project::bundle(files, &entry, false);
    let channels = Channels::stdout();
    let mut vm = host(kit)?.vm(Channels::printer(&channels), project, None, None)?;
    Channels::serve(&vm, &channels);
    vm.set_preempt_interval(opts.preempt)?;
    let status = vm.start(&src, None)?;
    Ok(drive(kit, &mut vm, status, opts, &src, &channels))
}

/* boots the source the blob embeds and overlays its saved state before driving on */
pub fn restore_and_run(kit: &Kit, file: &str, opts: &RunOpts) -> Result<i32> {
    let blob = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: cannot read state '{file}': {e}");
            return Ok(2);
        }
    };
    // the script the blob's run started from names its traceback and roots its relative imports
    let project = Project::disk(edge::embed::entry_of(&blob).unwrap_or_default(), opts.manifest.as_deref());
    let channels = Channels::stdout();
    let mut vm = host(kit)?.vm(Channels::printer(&channels), project, None, None)?;
    Channels::serve(&vm, &channels);
    vm.set_preempt_interval(opts.preempt)?;
    let status = match vm.restore_state(&blob) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(1);
        }
    };
    // the blob carries the source a restored run goes on reporting against
    let src = super::vm::snapshot_source(&blob).unwrap_or_default();
    Ok(drive(kit, &mut vm, status, opts, &src, &channels))
}

/* drives to the end with the channels written and an unservable park saved or failed */
pub fn drive(kit: &Kit, vm: &mut Vm, status: Status, opts: &RunOpts, src: &str, channels: &Shared) -> i32 {
    let code = driven(kit, vm, status, opts, src);
    match lock(channels).finish() {
        Ok(()) => code,
        Err(e) => {
            crate::ui::error(&e);
            1
        }
    }
}

/* drives to the end where host calls wait and unservable parks snapshot or fail */
fn driven(kit: &Kit, vm: &mut Vm, mut status: Status, opts: &RunOpts, src: &str) -> i32 {
    let mut events: Option<std::io::BufReader<std::fs::File>> = None;
    loop {
        // a channel that failed a line ends the run once the sending step returns
        if let Some(e) = vm.channel_error() {
            crate::ui::error(&anyhow!(e));
            return 1;
        }
        status = match status {
            Status::Done => return 0,
            Status::Exit(code) => return code as i32,
            Status::Error(text) => {
                crate::ui::traceback(&kit.worded(src, &text));
                return 1;
            }
            Status::Preempted => step(vm),
            Status::PendingEvent => {
                if vm.drain_buffered() > 0 {
                    step(vm)
                } else if let Some(path) = &opts.events {
                    match next_event(&mut events, path) {
                        Some(line) => {
                            vm.push_event(&line);
                            step(vm)
                        }
                        // a drained events file can never serve the wait so the park is final
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

/* the park report where a servable wait names its flag */
pub fn suspend_message(what: &str) -> String {
    match what {
        "an event" => "script suspended awaiting an event (wire --events <file>)".to_string(),
        s => format!("script suspended awaiting {s}, nothing can resume it here"),
    }
}

/* an unservable park saves a snapshot when asked and otherwise reports the wait and fails */
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

/* a lazy `--events` reader where a FIFO waits for a writer and a file replays */
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

/* a persistent interpreter for `edge test` that runs each input exactly once */
pub struct Session {
    vm: Vm,
    channels: Shared,
    words: Option<crate::kit::Words>,
}

impl Session {
    pub fn open(kit: &Kit, manifest: Option<&Path>) -> Result<Session> {
        let project = Project::disk("", manifest.map(path_spec).as_deref());
        let channels = Channels::stdout();
        let vm = host(kit)?.vm(Channels::printer(&channels), project, None, None)?;
        Channels::serve(&vm, &channels);
        Ok(Session { vm, channels, words: kit.words() })
    }

    /* runs one input as the script `entry` where `None` means the project root */
    pub fn eval(&mut self, src: &str, entry: Option<&str>, input: Option<&str>) -> Result<Outcome> {
        self.vm.set_entry(entry.unwrap_or(""));
        let mut status = self.vm.repl_eval(src, input)?;
        loop {
            // a channel that failed a line fails the input once the sending step returns
            if let Some(e) = self.vm.channel_error() {
                lock(&self.channels).finish()?;
                bail!(e);
            }
            status = match status {
                Status::Done => return self.ended(Outcome { err: None, exit_code: None }),
                Status::Exit(code) => return self.ended(Outcome { err: None, exit_code: Some(code as i32) }),
                Status::Error(text) => {
                    let err = crate::kit::worded(self.words, src, &text);
                    return self.ended(Outcome { err: Some(err), exit_code: None });
                }
                Status::Preempted => step(&mut self.vm),
                Status::PendingHostCall => {
                    self.vm.dispatch();
                    if self.vm.inflight() == 0 {
                        return self.ended(Outcome { err: Some(suspend_message("a host call")), exit_code: None });
                    }
                    self.vm.wait(None)?;
                    step(&mut self.vm)
                }
                Status::PendingEvent if self.vm.drain_buffered() > 0 => step(&mut self.vm),
                Status::PendingEvent => return self.ended(Outcome { err: Some(suspend_message("an event")), exit_code: None }),
            };
        }
    }

    /* the outcome of an input once the files its channels wrote hold every line */
    fn ended(&mut self, outcome: Outcome) -> Result<Outcome> {
        lock(&self.channels).finish()?;
        Ok(outcome)
    }

    /* wipes modules and state so the next input starts in a fresh namespace */
    pub fn reset(&mut self) -> Result<()> {
        self.vm.reset()
    }
}
