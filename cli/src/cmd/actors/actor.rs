use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use edge::deps::dir_of;

use crate::host::{driver, linear_cap, now_ns, time_limit, Fetched, Host, Project, Sink, Status, Vm, Walk, TICK_NS};
use crate::pack::{base64_decode, into_files, Bundle, BUNDLE_TAG};

use super::pool::{Eval, GroupSpec, Message, Out, Reply};

// a fixed program waiting on one message after another or an untrusted run per message
enum Mode {
    // a persistent interpreter whose waits the mailbox answers
    Fixed { vm: Box<Vm>, started: bool },
    // a fresh isolated interpreter per message kept while it waits
    Eval(Option<Box<EvalRun>>),
}

/* an untrusted run kept between steps */
struct EvalRun {
    vm: Vm,
    reply: Option<Reply>,
    id: Option<u64>,
    printed: Arc<Mutex<String>>,
    deadline: u64,
    // the program the run compiled which its errors are worded over
    source: String,
    // the import walk while a thread of its own fetches a remote file
    walking: Option<Walking>,
}

/* an import walk waiting on the file a thread fetches for it */
struct Walking {
    walk: Walk,
    from: Receiver<Fetched>,
    fetched: Option<Fetched>,
}

impl Walking {
    /* whether the fetch answered keeping the answer for the step that goes on */
    fn ready(&mut self) -> bool {
        if self.fetched.is_none() {
            self.fetched = match self.from.try_recv() {
                Ok(fetched) => Some(fetched),
                Err(TryRecvError::Disconnected) => Some(Fetched::lost()),
                Err(TryRecvError::Empty) => None,
            };
        }
        self.fetched.is_some()
    }
}

impl EvalRun {
    /* steps the import walk then starts the program or none while a fetch runs */
    fn walked(&mut self, walk: Result<Walk, String>, fetched: Option<Fetched>) -> Option<anyhow::Result<Status>> {
        let mut walk = match walk {
            Ok(walk) => walk,
            Err(e) => return Some(Ok(Status::Error(e))),
        };
        match self.vm.walk_on(&mut walk, fetched) {
            // the scheduler goes on with other actors while a thread waits on the network
            Ok(Some(remote)) => {
                let (to, from) = channel();
                std::thread::spawn(move || {
                    let _ = to.send(remote.fetch());
                });
                self.walking = Some(Walking { walk, from, fetched: None });
                None
            }
            Ok(None) => Some(self.vm.start(&self.source, None)),
            Err(e) => Some(Ok(Status::Error(e))),
        }
    }
}

// one live actor and the mailbox its work drains from
pub struct Actor {
    mode: Mode,
    // a deque so taking the oldest message never shifts the whole buffer
    pub mailbox: VecDeque<Message>,
    pub done: bool,
    // a fresh fixed actor runs once before any message to reach its first wait
    pub ran: bool,
    // waiting on a message with an empty mailbox so the load balancer picks it first
    pub idle: bool,
    // a wait ended so the actor runs again even with an empty mailbox
    pub runnable: bool,
    // in the ready queue so a burst of deliveries queues it once
    pub scheduled: bool,
    // wall-clock deadline of a blocked eval run past which the scheduler wakes the actor
    pub wake_at: Option<u64>,
    // parked on a host call while the scheduler polls it for answers
    pub blocked: bool,
    // the message fed in this step so the scheduler can retry it on a crash
    in_flight: Option<Message>,
    // set once the program took a message from its mailbox
    consumed: bool,
    // logged messages finished since the scheduler last asked
    finished: Vec<u64>,
    // eval runs that failed with no caller to answer since the scheduler last asked
    failed: Vec<String>,
}

// what a run step left the actor waiting on
pub enum Step {
    // ran to completion so the actor can be retired
    Done,
    // waiting on a message until one arrives
    Waiting,
    // parked on a host call a worker answers with the deadline that wakes it
    Blocked(Option<u64>),
    // handed the thread on mid-run to run again from the back of the queue
    Yield,
    // raised with the traceback and the message it was processing
    Failed(String, Option<Message>),
}

impl Actor {
    // wraps a booted interpreter whose program starts on the first step
    pub fn fixed(vm: Vm) -> Self {
        Self::new(Mode::Fixed { vm: Box::new(vm), started: false })
    }

    // boots a fresh interpreter per message from the settings of its group
    pub fn eval() -> Self {
        Self::new(Mode::Eval(None))
    }

    fn new(mode: Mode) -> Self {
        Actor { mode, mailbox: VecDeque::new(), done: false, ran: false, idle: false, runnable: false, scheduled: false, wake_at: None, blocked: false, in_flight: None, consumed: false, finished: Vec::new(), failed: Vec::new() }
    }

    pub fn deliver(&mut self, msg: Message) {
        self.mailbox.push_back(msg);
        self.idle = false;
    }

    /* injects the completions a blocked actor waited on and tells whether it can run again */
    pub fn poll(&mut self) -> bool {
        match &mut self.mode {
            Mode::Fixed { vm, .. } => vm.poll().unwrap_or(0) > 0,
            Mode::Eval(run) => run.as_mut().is_some_and(|r| match r.walking.as_mut() {
                Some(walking) => walking.ready(),
                None => r.vm.poll().unwrap_or(0) > 0,
            }),
        }
    }

    /* whether the instance hosting this actor trapped */
    pub fn trapped(&self) -> bool {
        match &self.mode {
            Mode::Fixed { vm, .. } => vm.instance().borrow().poisoned(),
            Mode::Eval(_) => false,
        }
    }

    /* what a retired actor hands back as its current message and the queued ones */
    pub fn into_messages(mut self) -> (Option<Message>, Vec<Message>) {
        (self.in_flight.take(), std::mem::take(&mut self.mailbox).into_iter().collect())
    }

    /* the queue goes back only if the program read its mailbox so no rerun loops */
    pub fn leftover(self) -> (Vec<Message>, Vec<Message>) {
        let queued = self.mailbox.into_iter().collect();
        if self.consumed { (queued, Vec::new()) } else { (Vec::new(), queued) }
    }

    pub fn take_finished(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.finished)
    }

    /* the failures of eval runs nobody waited on which fail the pool as crashes do */
    pub fn take_failed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.failed)
    }

    /* what the last step sent which is nothing for an eval actor */
    pub fn take_sends(&mut self) -> Vec<(String, String)> {
        match &mut self.mode {
            Mode::Fixed { vm, .. } => vm.take_sends(),
            Mode::Eval(_) => Vec::new(),
        }
    }

    pub fn step(&mut self, spec: &GroupSpec, host: &Rc<Host>) -> Step {
        self.ran = true;
        self.runnable = false;
        match &spec.eval {
            None => self.step_fixed(&spec.source),
            Some(eval) => self.step_eval(spec, eval, host),
        }
    }

    // drives the persistent interpreter with one mailbox message per wait
    fn step_fixed(&mut self, source: &str) -> Step {
        let Mode::Fixed { vm, started } = &mut self.mode else { unreachable!() };
        loop {
            let result = if *started {
                vm.resume()
            } else {
                *started = true;
                vm.start(source, None)
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
                // back in a wait so the message it was given is done
                Status::PendingEvent => {
                    self.finished.extend(self.in_flight.take().and_then(|m| m.id));
                    // an event that arrived mid-step goes before the mailbox
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

    /* runs one message at a time in a fresh capped interpreter parked while it waits */
    fn step_eval(&mut self, spec: &GroupSpec, eval: &Eval, host: &Rc<Host>) -> Step {
        let Mode::Eval(run) = &mut self.mode else { unreachable!() };
        let (mut current, status) = match run.take() {
            // a run that waited past its deadline ends there whatever it waited on
            Some(current) if now_ns() >= current.deadline => return self.finish(spec, eval, current, Err(time_limit(host.descriptor()))),
            Some(mut current) => match current.walking.take() {
                Some(mut walking) => {
                    if !walking.ready() {
                        current.walking = Some(walking);
                        return self.wait(current);
                    }
                    match current.walked(Ok(walking.walk), walking.fetched) {
                        Some(status) => (current, status),
                        None => return self.wait(current),
                    }
                }
                None => {
                    let status = current.vm.resume();
                    (current, status)
                }
            },
            None => {
                let Some(msg) = self.mailbox.pop_front() else {
                    self.idle = true;
                    return Step::Waiting;
                };
                match start(spec, eval, host, &msg.body, msg.reply.is_some()) {
                    Ok(mut current) => {
                        current.reply = msg.reply;
                        current.id = msg.id;
                        let walk = current.vm.walk(&current.source);
                        match current.walked(walk, None) {
                            Some(status) => (current, status),
                            None => return self.wait(current),
                        }
                    }
                    Err(e) => {
                        self.answer(&spec.name, msg.reply, Err(e), None);
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
            Ok(Status::PendingHostCall) => {
                current.vm.dispatch();
                if current.vm.inflight() > 0 {
                    return self.wait(current);
                }
                Err(format!("error: {}", driver::suspend_message("a host call")))
            }
            // nothing sends an eval run a message so its wait could never end
            Ok(Status::PendingEvent) => Err(format!("error: {}", driver::suspend_message("a message"))),
        };
        self.finish(spec, eval, current, outcome)
    }

    /* parks a run on its deadline until what it waits on answers */
    fn wait(&mut self, current: Box<EvalRun>) -> Step {
        let until = current.deadline;
        self.park(current, Step::Blocked(Some(until)))
    }

    /* keeps the run for the step that resumes it */
    fn park(&mut self, current: Box<EvalRun>, step: Step) -> Step {
        if let Mode::Eval(run) = &mut self.mode {
            *run = Some(current);
        }
        step
    }

    /* answers the run's caller in the runtime's words and drops its interpreter */
    fn finish(&mut self, spec: &GroupSpec, eval: &Eval, mut current: Box<EvalRun>, outcome: Result<String, String>) -> Step {
        let outcome = outcome.map_err(|e| crate::kit::worded(eval.words, &current.source, &e));
        self.answer(&spec.name, current.reply.take(), outcome, current.vm.unsent());
        self.finished.extend(current.id);
        drop(current);
        self.next_run()
    }

    /* hands an outcome to its caller or keeps a failure nobody waits on */
    fn answer(&mut self, group: &str, reply: Option<Reply>, outcome: Result<String, String>, unsent: Option<String>) {
        match (reply, outcome) {
            (Some(reply), outcome) => reply(outcome),
            (None, Err(e)) => self.failed.push(failure(group, &e, unsent)),
            (None, Ok(_)) => {}
        }
    }

    /* the next message starts on a later step so one burst never holds the thread */
    fn next_run(&mut self) -> Step {
        if self.mailbox.is_empty() {
            self.idle = true;
            return Step::Waiting;
        }
        Step::Yield
    }
}

/* boots a fresh capped interpreter for one untrusted program its walk then starts */
fn start(spec: &GroupSpec, eval: &Eval, host: &Rc<Host>, body: &str, capture: bool) -> Result<Box<EvalRun>, String> {
    let (source, files, entry_dir) = unbundle(body).unwrap_or_else(|| (body.to_string(), HashMap::new(), String::new()));
    let printed = Arc::new(Mutex::new(String::new()));
    let sink: Sink = if capture {
        let printed = printed.clone();
        Box::new(move |s: &str| {
            if let Ok(mut b) = printed.lock() {
                b.push_str(s);
            }
        })
    } else {
        sink_for(&spec.out)
    };
    let own_manifest = files.contains_key(&format!("{entry_dir}edge.json"));
    let mut project = Project::bundle(files, &entry_dir, true);
    // a granted file lies below the pool's manifest and never where the pool started
    project.pool = Some(dir_of(&eval.manifest));
    // a bundle grants through its own manifest and a snippet resolves through the pool's
    project.access = Some(match own_manifest {
        true => eval.untrusted.bundle.clone(),
        false => {
            project.manifest = Some(eval.manifest.clone());
            eval.untrusted.snippet.clone()
        }
    });
    let failed = |e: anyhow::Error| format!("error: {e}");
    // the epoch ticker advances once per `TICK_NS` so the timeout counts in its ticks
    let ticks = eval.timeout * 1_000_000_000 / TICK_NS;
    let mut vm = host.vm(sink, project, Some(ticks), Some(linear_cap(spec.limits.memory))).map_err(failed)?;
    vm.set_preempt_interval(spec.preempt).map_err(failed)?;
    vm.set_limits(&spec.limits).map_err(failed)?;
    // host-side waits count toward the budget the epoch ticker enforces inside the sandbox
    let deadline = now_ns() + ticks * TICK_NS;
    Ok(Box::new(EvalRun { vm, reply: None, id: None, printed, deadline, source, walking: None }))
}

/* a failed run nobody waits on as the pool prints it under its group's name */
fn failure(group: &str, error: &str, unsent: Option<String>) -> String {
    let why = match unsent {
        Some(to) => format!(", it sent to '{to}' and an eval run cannot send"),
        None => String::new(),
    };
    format!("error: an eval run of group '{group}' failed{why}\n{error}")
}

/* decodes a bundled project into an in-memory tree with its entry source and directory */
fn unbundle(body: &str) -> Option<(String, HashMap<String, Vec<u8>>, String)> {
    let b64 = body.strip_prefix(BUNDLE_TAG)?;
    let bundle = Bundle::decode(&base64_decode(b64.trim())?).ok()?;
    let entry = bundle.entry.clone();
    let files = into_files(bundle);
    let source = String::from_utf8_lossy(files.get(&entry)?).into_owned();
    Some((source, files, dir_of(&entry)))
}

/* the output sink the setting of a group names with a file opened for append */
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
