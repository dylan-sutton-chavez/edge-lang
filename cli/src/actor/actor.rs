use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use compiler::modules::dir_of;
use compiler::vm::Limits;

use crate::host::{driver, now_ns, Host, Project, Sink, Status, Vm, TICK_NS};
use crate::pack::{base64_decode, Bundle, BUNDLE_TAG};

use super::config::{Message, Out, Reply};

// The reply when host-side waits would outlast the deadline, worded like the epoch trap.
const EVAL_TIME_LIMIT: &str = "error: RuntimeError: run exceeded its time limit";

// How an actor runs, a fixed program looping over receive(), or an untrusted per-message evaluator.
enum Mode {
    // A persistent interpreter driven by push_event into receive().
    Fixed { vm: Box<Vm>, started: bool },
    // Boots a fresh isolated interpreter per message, keeping the run in progress while it waits.
    Eval { limits: Limits, preempt: usize, timeout: u64, run: Option<Box<EvalRun>> },
}

/* An untrusted run between steps, with its interpreter, waiting caller, log record, print and deadline. */
struct EvalRun {
    vm: Vm,
    reply: Option<Reply>,
    id: Option<u64>,
    printed: Arc<Mutex<String>>,
    deadline: u64,
}

/* What a group hands an actor for a step, the pieces a fresh eval interpreter needs. */
#[derive(Clone)]
pub struct Context {
    pub host: Rc<Host>,
    pub source: Rc<String>,
    pub out: Out,
    // The group's edge.json, what an eval snippet without its own resolves through.
    pub manifest: String,
    pub ceiling: Vec<String>,
}

// One live actor plus the mailbox its work drains from.
pub struct Actor {
    mode: Mode,
    // A deque so draining the oldest message is O(1) instead of shifting the whole buffer.
    pub mailbox: VecDeque<Message>,
    pub done: bool,
    // False until the first step, a fresh fixed actor runs once to reach its first receive().
    pub ran: bool,
    // True when parked in receive() with an empty mailbox, the load balancer's free signal.
    pub idle: bool,
    // Set when a wait ended, the actor runs again even with an empty mailbox.
    pub runnable: bool,
    // In the ready queue, so a burst of deliveries queues it once.
    pub scheduled: bool,
    // Wall-clock deadline of a sleep, the scheduler wakes the actor past it.
    pub wake_at: Option<u64>,
    // Parked on a host call, the scheduler polls it for answers while it waits.
    pub blocked: bool,
    // The message fed into the interpreter this step, so the scheduler can retry it on a crash.
    in_flight: Option<Message>,
    // Set once the program took a message from its mailbox.
    consumed: bool,
    // Logged messages it finished since the scheduler last asked.
    finished: Vec<u64>,
}

// What a run step left the actor waiting on.
pub enum Step {
    // Ran to completion, the actor can be retired.
    Done,
    // Parked in receive(), feed it a message to wake it.
    Waiting,
    // Parked on a timer until the wall-clock deadline.
    Sleeping(u64),
    // Parked on a host call a worker thread is answering, woken at the deadline it carries.
    Blocked(Option<u64>),
    // Handed the thread on mid-run, it runs again from the back of the queue.
    Yield,
    // Raised, carries the traceback and the message that was being processed.
    Failed(String, Option<Message>),
}

impl Actor {
    // A fixed actor wraps a booted interpreter, the program starts on its first step.
    pub fn fixed(vm: Vm) -> Self {
        Self::new(Mode::Fixed { vm: Box::new(vm), started: false })
    }

    // An eval actor holds only the settings to boot a fresh interpreter per snippet.
    pub fn eval(limits: Limits, preempt: usize, timeout: u64) -> Self {
        Self::new(Mode::Eval { limits, preempt, timeout, run: None })
    }

    fn new(mode: Mode) -> Self {
        Actor { mode, mailbox: VecDeque::new(), done: false, ran: false, idle: false, runnable: false, scheduled: false, wake_at: None, blocked: false, in_flight: None, consumed: false, finished: Vec::new() }
    }

    // Delivers a message to the mailbox, waking the actor from its idle wait.
    pub fn deliver(&mut self, msg: Message) {
        self.mailbox.push_back(msg);
        self.idle = false;
    }

    /* Injects the completions a blocked actor waited on, true when it can run again. */
    pub fn poll(&mut self) -> bool {
        match &mut self.mode {
            Mode::Fixed { vm, .. } => vm.poll().unwrap_or(0) > 0,
            Mode::Eval { run, .. } => run.as_mut().is_some_and(|r| r.vm.poll().unwrap_or(0) > 0),
        }
    }

    /* True once the instance hosting this actor trapped. */
    pub fn trapped(&self) -> bool {
        match &self.mode {
            Mode::Fixed { vm, .. } => vm.instance().borrow().poisoned(),
            Mode::Eval { .. } => false,
        }
    }

    /* The message it was processing and the ones still queued, what a retired actor hands back. */
    pub fn into_messages(mut self) -> (Option<Message>, Vec<Message>) {
        (self.in_flight.take(), std::mem::take(&mut self.mailbox).into_iter().collect())
    }

    /* Queued messages go back out only when the program reads its mailbox, else another run would loop, so they drop. */
    pub fn leftover(self) -> (Vec<Message>, Vec<Message>) {
        let queued = self.mailbox.into_iter().collect();
        if self.consumed { (queued, Vec::new()) } else { (Vec::new(), queued) }
    }

    pub fn take_finished(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.finished)
    }

    /* Everything the last step sent, an eval actor never sends. */
    pub fn take_sends(&mut self) -> Vec<(String, String)> {
        match &mut self.mode {
            Mode::Fixed { vm, .. } => vm.take_sends(),
            Mode::Eval { .. } => Vec::new(),
        }
    }

    pub fn step(&mut self, ctx: &Context) -> Step {
        self.ran = true;
        self.runnable = false;
        match &mut self.mode {
            Mode::Fixed { .. } => self.step_fixed(ctx),
            Mode::Eval { .. } => self.step_eval(ctx),
        }
    }

    // Drives the persistent interpreter, feeding one mailbox message per receive().
    fn step_fixed(&mut self, ctx: &Context) -> Step {
        let Mode::Fixed { vm, started } = &mut self.mode else { unreachable!() };
        loop {
            let result = if *started {
                vm.resume()
            } else {
                *started = true;
                vm.start(&ctx.source, None)
            };
            let status = match result {
                Ok(status) => status,
                Err(e) => {
                    self.done = true;
                    return Step::Failed(format!("error: {e}"), self.in_flight.take());
                }
            };
            match status {
                Status::Done | Status::Exit(_) => {
                    self.finished.extend(self.in_flight.take().and_then(|m| m.id));
                    self.done = true;
                    return Step::Done;
                }
                Status::Preempted => return Step::Yield,
                // Back in receive(), so the message it was given is done.
                Status::PendingEvent => {
                    self.finished.extend(self.in_flight.take().and_then(|m| m.id));
                    // An event that arrived mid-step goes before the mailbox.
                    if vm.drain_buffered() > 0 {
                        continue;
                    }
                    let Some(msg) = self.mailbox.pop_front() else {
                        self.idle = true;
                        return Step::Waiting;
                    };
                    if vm.push_event(&msg.body) {
                        self.consumed = true;
                        self.in_flight = Some(msg);
                        continue;
                    }
                    self.mailbox.push_front(msg);
                    self.idle = true;
                    return Step::Waiting;
                }
                Status::PendingTimer(deadline) => return Step::Sleeping(deadline),
                Status::PendingHostCall => {
                    vm.dispatch();
                    if vm.inflight() > 0 {
                        return Step::Blocked(None);
                    }
                    self.done = true;
                    return Step::Failed(format!("error: {}", driver::suspend_message("a host call")), self.in_flight.take());
                }
                Status::Error(tb) => {
                    self.done = true;
                    return Step::Failed(tb, self.in_flight.take());
                }
            }
        }
    }

    /* Runs one message at a time in a fresh capped interpreter, parked while it waits. */
    fn step_eval(&mut self, ctx: &Context) -> Step {
        let Mode::Eval { limits, preempt, timeout, run } = &mut self.mode else { unreachable!() };
        let (limits, preempt, timeout) = (*limits, *preempt, *timeout);
        let (mut current, status) = match run.take() {
            // A run that waited past its deadline ends there, whatever it waited on.
            Some(current) if now_ns() >= current.deadline => return self.finish(current, Err(EVAL_TIME_LIMIT.to_string())),
            Some(mut current) => {
                let status = current.vm.resume();
                (current, status)
            }
            None => {
                let Some(msg) = self.mailbox.pop_front() else {
                    self.idle = true;
                    return Step::Waiting;
                };
                match start_eval(ctx, &msg.body, limits, preempt, timeout, msg.reply.is_some()) {
                    Ok((mut current, status)) => {
                        current.reply = msg.reply;
                        current.id = msg.id;
                        (current, Ok(status))
                    }
                    Err(e) => {
                        if let Some(reply) = msg.reply {
                            reply.send(Err(e));
                        }
                        self.finished.extend(msg.id);
                        return self.next_run();
                    }
                }
            }
        };
        let outcome = match status {
            Err(e) => Err(format!("error: {e}")),
            Ok(Status::Done | Status::Exit(_)) => Ok(current.printed.lock().map(|b| b.clone()).unwrap_or_default()),
            Ok(Status::Error(tb)) => Err(tb),
            Ok(Status::Preempted) => return self.park(current, Step::Yield),
            Ok(Status::PendingTimer(wake)) if wake <= current.deadline => return self.park(current, Step::Sleeping(wake)),
            Ok(Status::PendingTimer(_)) => Err(EVAL_TIME_LIMIT.to_string()),
            Ok(Status::PendingHostCall) => {
                current.vm.dispatch();
                if current.vm.inflight() > 0 {
                    let until = current.deadline;
                    return self.park(current, Step::Blocked(Some(until)));
                }
                Err(format!("error: {}", driver::suspend_message("a host call")))
            }
            Ok(Status::PendingEvent) => Err(format!("error: {}", driver::suspend_message("receive()"))),
        };
        self.finish(current, outcome)
    }

    /* Keeps the run for the step that resumes it. */
    fn park(&mut self, current: Box<EvalRun>, step: Step) -> Step {
        if let Mode::Eval { run, .. } = &mut self.mode {
            *run = Some(current);
        }
        step
    }

    /* Answers the caller waiting on the run and drops its interpreter. */
    fn finish(&mut self, current: Box<EvalRun>, outcome: Result<String, String>) -> Step {
        if let Some(reply) = &current.reply {
            reply.send(outcome);
        }
        self.finished.extend(current.id);
        drop(current);
        self.next_run()
    }

    /* The next queued message starts on a later step, so one burst never holds the thread. */
    fn next_run(&mut self) -> Step {
        if self.mailbox.is_empty() {
            self.idle = true;
            return Step::Waiting;
        }
        Step::Yield
    }
}

/* Boots and starts a fresh capped interpreter for one untrusted program. */
fn start_eval(ctx: &Context, body: &str, limits: Limits, preempt: usize, timeout: u64, capture: bool) -> Result<(Box<EvalRun>, Status), String> {
    let (source, files, entry_dir) = match unbundle(body) {
        Some((source, files, entry_dir)) => (source, files, entry_dir),
        None => (body.to_string(), HashMap::new(), String::new()),
    };
    let printed = Arc::new(Mutex::new(String::new()));
    let sink: Sink = if capture {
        let printed = printed.clone();
        Box::new(move |s: &str| {
            if let Ok(mut b) = printed.lock() {
                b.push_str(s);
            }
        })
    } else {
        sink_for(&ctx.out)
    };
    let own_manifest = files.contains_key(&format!("{entry_dir}edge.json"));
    let mut project = Project::bundle(files, &entry_dir, true);
    match own_manifest {
        true => project.ceiling = Some(ctx.ceiling.clone()),
        false => project.manifest = Some(ctx.manifest.clone()),
    }
    let memory = crate::host::linear_cap(limits.memory);
    // The epoch ticker advances once per TICK_NS, so the timeout becomes that many of its ticks.
    let ticks = timeout * 1_000_000_000 / TICK_NS;
    let mut vm = ctx.host.vm(sink, project, Some(ticks), Some(memory)).map_err(|e| format!("error: {e}"))?;
    vm.set_preempt_interval(preempt).map_err(|e| format!("error: {e}"))?;
    vm.set_limits(&limits).map_err(|e| format!("error: {e}"))?;
    // Host-side waits count toward the same budget the epoch ticker enforces inside the sandbox.
    let deadline = now_ns() + ticks * TICK_NS;
    let status = vm.start(&source, None).map_err(|e| format!("error: {e}"))?;
    Ok((Box::new(EvalRun { vm, reply: None, id: None, printed, deadline }), status))
}

/* Decodes a bundled project into an in-memory tree, its entry source and entry dir. */
fn unbundle(body: &str) -> Option<(String, HashMap<String, Vec<u8>>, String)> {
    let b64 = body.strip_prefix(BUNDLE_TAG)?;
    let bytes = base64_decode(b64.trim())?;
    let bundle = Bundle::decode(&bytes).ok()?;
    let entry = bundle.entry.clone();
    let files = crate::pack::into_files(bundle);
    let source = String::from_utf8_lossy(files.get(&entry)?).into_owned();
    Some((source, files, dir_of(&entry)))
}

/* The print sink a group's setting names, a file opens in append mode. */
pub fn sink_for(out: &Out) -> Sink {
    match out {
        Out::Stdout => driver::stdout_sink(),
        Out::Null => Box::new(|_: &str| {}),
        Out::File(path) => match std::fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                let file = Mutex::new(file);
                Box::new(move |s: &str| {
                    use std::io::Write;
                    if let Ok(mut f) = file.lock() {
                        let _ = f.write_all(s.as_bytes());
                    }
                })
            }
            Err(_) => driver::stdout_sink(),
        },
    }
}
