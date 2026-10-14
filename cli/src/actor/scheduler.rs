use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slab::Slab;

use compiler::vm::Limits;

use crate::host::{now_ns, Host, Instance, Project, Runtime, Vm};

use super::actor::{sink_for, Actor, Context, Step};
use super::config::{ActorConfig, Group, Message};
use super::pool::Router;
use super::server::{Intake, Wal};

// How long an idle scheduler naps before re-checking sleeping and blocked actors.
const NAP: Duration = Duration::from_millis(2);
// How long a closing server lets its actors finish, inside the 30 seconds an orchestrator waits before it kills.
const DRAIN: Duration = Duration::from_secs(25);
// Actors per compiler instance, a 4 GiB memory holds this many interpreters with room to spare.
const SLOTS_PER_INSTANCE: usize = 4096;
// Messages a live pool holds undelivered or in mailboxes before it stops draining its inbox.
const BACKLOG: usize = 1 << 16;

// A group ready to boot actors from, its replicas share compiler instances and the parsed source.
struct GroupState {
    name: String,
    ctx: Context,
    dir: String,
    manifest: Option<String>,
    retry: usize,
    // Actor ceiling, actors spawn lazily up to this instead of all at boot.
    max: usize,
    limits: Limits,
    preempt: usize,
    eval: bool,
    timeout: u64,
    // Stable keys survive removal, so the work queues below never dangle.
    actors: Slab<Actor>,
    // Keys with work to run, drained each tick instead of scanning every actor.
    ready: VecDeque<usize>,
    // Keys parked in receive(), popped first when a message needs an actor.
    idle_free: Vec<usize>,
    // Keys parked on a timer, a host call or an open stream, re-checked each tick.
    waiting: Vec<usize>,
    // Compiler instances hosting the group's actors as slots.
    instances: Vec<Rc<RefCell<Instance>>>,
}

// The single-threaded cooperative loop, one instance owns every actor in the shard.
pub struct Scheduler {
    groups: Vec<GroupState>,
    by_name: HashMap<String, usize>,
    pending: Vec<Message>,
    // Messages sitting in mailboxes, kept as they come and go instead of summed per actor.
    queued: usize,
    // Tracebacks of actors that raised uncaught and were retired.
    crashes: Vec<String>,
    // Set in sharded mode, routes sends whose group lives on another thread.
    router: Option<Router>,
    // Live counters published for the control endpoint, None when no control port is set.
    stats: Option<Arc<super::server::Stats>>,
    // The durable log of a live server, which hears each logged message that is done.
    wal: Option<Arc<Mutex<Wal>>>,
}

impl Scheduler {
    pub fn new(config: ActorConfig, runtime: Arc<Runtime>) -> Result<Self, String> {
        let host = Host::new(runtime).map_err(|e| e.to_string())?;
        let mut groups = Vec::new();
        let mut by_name = HashMap::new();
        let mut pending = Vec::new();
        for g in config.groups {
            by_name.insert(g.name.clone(), groups.len());
            pending.extend(g.inbox.iter().map(|m| Message::new(m.group.clone(), m.body.clone())));
            groups.push(GroupState::boot(g, config.max_actors, host.clone()));
        }
        Ok(Scheduler { groups, by_name, pending, queued: 0, crashes: Vec::new(), router: None, stats: None, wal: None })
    }

    // Wires the shared counters the control endpoint reads, published each tick.
    pub fn set_stats(&mut self, stats: Option<Arc<super::server::Stats>>) {
        self.stats = stats;
    }

    // Pumps messages and runs actors until nothing is left to deliver or run.
    pub fn run(&mut self) -> i32 {
        self.spawn_seed();
        loop {
            self.route_pending();
            if self.tick() || !self.pending.is_empty() {
                continue;
            }
            if self.has_waiters() {
                std::thread::sleep(NAP);
                continue;
            }
            break;
        }
        self.report()
    }

    // A live server, runs local work then blocks on the ingress, and once it closes finishes what it holds.
    pub fn run_serving(&mut self, recovered: Vec<Message>, rx: Intake, wal: Arc<Mutex<Wal>>) -> i32 {
        self.pending.extend(recovered);
        self.wal = Some(wal.clone());
        self.spawn_seed();
        let mut closed: Option<Instant> = None;
        loop {
            // Past the backlog the inbox fills, and the ingress and control hold their clients back.
            while closed.is_none() && !self.full() {
                match rx.try_recv() {
                    Ok(m) => self.pending.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => closed = Some(Instant::now()),
                }
            }
            self.route_pending();
            let busy = self.tick() || !self.pending.is_empty();
            if let Some(at) = closed {
                if at.elapsed() >= DRAIN || !busy && !self.has_waiters() {
                    break;
                }
                if !busy {
                    std::thread::sleep(NAP);
                }
                continue;
            }
            if busy {
                continue;
            }
            if self.full() {
                std::thread::sleep(NAP);
                continue;
            }
            // Actors parked on timers or host calls keep the loop polling.
            if self.has_waiters() {
                match rx.recv_timeout(NAP) {
                    Ok(m) => self.pending.push(m),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => closed = Some(Instant::now()),
                }
                continue;
            }
            self.publish_stats();
            match rx.recv() {
                Ok(m) => self.pending.push(m),
                Err(_) => closed = Some(Instant::now()),
            }
        }
        wal.lock().unwrap().sync();
        self.report()
    }

    // Boots one actor per group so producers run, the pool then grows lazily on demand.
    fn spawn_seed(&mut self) {
        for g in &mut self.groups {
            if g.max > 0 {
                g.spawn();
            }
        }
    }

    // Same loop across shards, cross-group sends leave by the router, others arrive by rx.
    pub fn run_sharded(&mut self, router: Router, rx: std::sync::mpsc::Receiver<Message>) -> i32 {
        let barrier = router.barrier();
        self.router = Some(router);
        self.spawn_seed();
        loop {
            // Drain what arrived from other shards before running.
            while let Ok(m) = rx.try_recv() {
                barrier.consume();
                self.pending.push(m);
            }
            self.route_pending();
            if self.tick() || !self.pending.is_empty() {
                continue;
            }
            if self.has_waiters() {
                std::thread::sleep(NAP);
                continue;
            }
            // No local work, block until another shard sends some or the whole pool quiesces.
            if barrier.park_until_work_or_done() {
                break;
            }
        }
        self.report()
    }

    fn report(&self) -> i32 {
        for f in &self.crashes {
            eprintln!("{f}");
        }
        i32::from(!self.crashes.is_empty())
    }

    fn has_waiters(&self) -> bool {
        self.groups.iter().any(|g| !g.waiting.is_empty())
    }

    fn full(&self) -> bool {
        self.pending.len() + self.queued >= BACKLOG
    }

    // Writes the live counts to the shared stats so the control endpoint can read them.
    fn publish_stats(&self) {
        let Some(stats) = &self.stats else { return };
        let mut actors = 0;
        let mut idle = 0;
        for g in &self.groups {
            for (_, n) in &g.actors {
                actors += 1;
                if n.idle {
                    idle += 1;
                }
            }
        }
        stats.set(actors, actors - idle, idle, self.pending.len() + self.queued, self.crashes.len());
    }

    // Delivers each queued message to an actor of its target group, spawning on demand.
    fn route_pending(&mut self) {
        let msgs = core::mem::take(&mut self.pending);
        for m in msgs {
            let Some(&gi) = self.by_name.get(&m.group) else {
                self.ack(m.id);
                continue;
            };
            let g = &mut self.groups[gi];
            match g.pick() {
                Some(key) => {
                    g.actors[key].deliver(m);
                    g.schedule(key);
                    self.queued += 1;
                }
                None => self.ack(m.id),
            }
        }
    }

    // Runs each queued actor once, collecting what they send, false when none progressed.
    fn tick(&mut self) -> bool {
        let mut progressed = false;
        let now = now_ns();
        for gi in 0..self.groups.len() {
            self.groups[gi].wake(now);
            for _ in 0..self.groups[gi].ready.len() {
                let Some(key) = self.groups[gi].ready.pop_front() else { break };
                let Some(actor) = self.groups[gi].actors.get_mut(key) else { continue };
                actor.scheduled = false;
                if actor.done || (actor.mailbox.is_empty() && actor.ran && !actor.runnable) {
                    continue;
                }
                progressed = true;
                let before = actor.mailbox.len();
                let ctx = self.groups[gi].ctx.clone();
                let step = self.groups[gi].actors[key].step(&ctx);
                // A step only takes from the mailbox, nothing reaches it mid-step.
                self.queued -= before - self.groups[gi].actors[key].mailbox.len();
                let finished = self.groups[gi].actors[key].take_finished();
                self.ack(finished);
                self.collect_sends(gi, key);
                self.settle(gi, key, step);
            }
        }
        self.publish_stats();
        progressed
    }

    // Re-queues an actor by its step outcome, retiring it from the slab on a crash.
    fn settle(&mut self, gi: usize, key: usize, step: Step) {
        let g = &mut self.groups[gi];
        match step {
            Step::Failed(tb, msg) => {
                self.release(gi, key);
                self.crashes.push(tb);
                self.handle_crash(gi, msg);
                self.crash_trapped(gi);
            }
            Step::Sleeping(deadline) => {
                g.actors[key].wake_at = Some(deadline);
                g.waiting.push(key);
            }
            Step::Blocked(until) => {
                g.actors[key].wake_at = until;
                g.actors[key].blocked = true;
                g.waiting.push(key);
            }
            // Handed the thread on, it runs again from the back of the queue even with an empty mailbox.
            Step::Yield => {
                g.actors[key].runnable = true;
                g.schedule(key);
            }
            _ if g.actors[key].done => self.release(gi, key),
            _ if g.actors[key].idle => g.idle_free.push(key),
            _ => g.schedule(key),
        }
    }

    // A trap poisons its whole instance, the actors living there crash and their queues move on.
    fn crash_trapped(&mut self, gi: usize) {
        let g = &mut self.groups[gi];
        if !g.instances.iter().any(|i| i.borrow().poisoned()) {
            return;
        }
        g.instances.retain(|i| !i.borrow().poisoned());
        let doomed: Vec<usize> = g.actors.iter().filter(|(_, a)| a.trapped()).map(|(k, _)| k).collect();
        for key in doomed {
            let (in_flight, leftover) = self.retire(gi, key).into_messages();
            self.pending.extend(leftover);
            self.crashes.push(format!("error: group '{}' lost an actor, its interpreter instance trapped", self.groups[gi].name));
            self.handle_crash(gi, in_flight);
        }
    }

    /* Takes an actor out of its group, its mailbox leaving the count with it. */
    fn retire(&mut self, gi: usize, key: usize) -> Actor {
        let actor = self.groups[gi].actors.remove(key);
        self.queued -= actor.mailbox.len();
        actor
    }

    /* Retires an actor that ended, its queued messages going back out, or done when it never read them. */
    fn release(&mut self, gi: usize, key: usize) {
        let (back, dropped) = self.retire(gi, key).leftover();
        self.pending.extend(back);
        self.ack(dropped.into_iter().filter_map(|m| m.id));
    }

    // Retries a crashed message on another actor up to the group's retry count, else drops it dead.
    fn handle_crash(&mut self, gi: usize, msg: Option<Message>) {
        let Some(mut msg) = msg else { return };
        if msg.attempts < self.groups[gi].retry {
            msg.attempts += 1;
            self.pending.push(msg);
            return;
        }
        if let Some(stats) = &self.stats {
            stats.add_dead();
        }
        self.ack(msg.id);
    }

    /* Marks logged messages done, so a restart never replays them. */
    fn ack(&self, ids: impl IntoIterator<Item = u64>) {
        let Some(wal) = &self.wal else { return };
        let mut ids = ids.into_iter().peekable();
        if ids.peek().is_some() {
            let mut wal = wal.lock().unwrap();
            ids.for_each(|id| wal.ack(id));
        }
    }

    // Drains what the actor just sent, routing cross-shard groups out and keeping local ones.
    fn collect_sends(&mut self, gi: usize, key: usize) {
        let sent = match self.groups[gi].actors.get_mut(key) {
            Some(actor) => actor.take_sends(),
            None => return,
        };
        for (group, body) in sent {
            let msg = Message::new(group, body);
            match &self.router {
                Some(r) if !self.by_name.contains_key(&msg.group) => r.route(msg),
                _ => self.pending.push(msg),
            }
        }
    }
}

impl GroupState {
    fn boot(g: Group, max_actors: usize, host: Rc<Host>) -> Self {
        // Eval snippets that bring no edge.json resolve through the group's own.
        let manifest = g.manifest.clone().unwrap_or_else(|| format!("{}edge.json", g.dir));
        GroupState {
            name: g.name,
            ctx: Context { host, source: Rc::new(g.source), out: g.out, manifest, ceiling: g.ceiling },
            dir: g.dir,
            manifest: g.manifest,
            retry: g.retry,
            max: g.replicas.min(max_actors),
            limits: g.limits,
            preempt: g.preempt,
            eval: g.eval,
            timeout: g.timeout,
            actors: Slab::new(),
            ready: VecDeque::new(),
            idle_free: Vec::new(),
            waiting: Vec::new(),
            instances: Vec::new(),
        }
    }

    /* Boots a fresh actor, an interpreter slot unless the group evals each message apart. */
    fn spawn(&mut self) -> Option<usize> {
        let actor = if self.eval {
            Actor::eval(self.limits, self.preempt, self.timeout)
        } else {
            let mut vm = self.slot().ok()?;
            vm.set_preempt_interval(self.preempt).ok()?;
            vm.set_limits(&self.limits).ok()?;
            Actor::fixed(vm)
        };
        let key = self.actors.insert(actor);
        self.schedule(key);
        Some(key)
    }

    /* Queues an actor to run, once however often it is woken before its turn. */
    fn schedule(&mut self, key: usize) {
        if let Some(actor) = self.actors.get_mut(key)
            && !actor.scheduled
        {
            actor.scheduled = true;
            self.ready.push_back(key);
        }
    }

    /* A slot in an instance with room, a new instance registers the group's modules on first boot. */
    fn slot(&mut self) -> anyhow::Result<Vm> {
        self.instances.retain(|i| !i.borrow().poisoned());
        if let Some(inst) = self.instances.iter().find(|i| i.borrow().slots() < SLOTS_PER_INSTANCE) {
            return Instance::slot(inst);
        }
        let project = Project::disk(&self.dir, self.manifest.as_deref());
        // Every slot of the instance may hold its limit, the linear memory caps them together.
        let memory = crate::host::linear_cap(self.limits.memory.saturating_mul(SLOTS_PER_INSTANCE));
        let inst = self.ctx.host.instance(sink_for(&self.ctx.out), project, None, memory)?;
        inst.borrow_mut().accept_sends();
        let vm = Instance::slot(&inst)?;
        self.instances.push(inst);
        Ok(vm)
    }

    /* Picks an idle actor, else a fresh spawn under the ceiling, else the least-loaded live one. */
    fn pick(&mut self) -> Option<usize> {
        while let Some(key) = self.idle_free.pop() {
            if self.actors.get(key).is_some_and(|n| n.idle && !n.done) {
                return Some(key);
            }
        }
        if self.actors.len() < self.max
            && let Some(key) = self.spawn()
        {
            return Some(key);
        }
        self.actors.iter().filter(|(_, n)| !n.done).min_by_key(|(_, n)| n.mailbox.len()).map(|(k, _)| k)
    }

    // Moves sleepers past their deadline and actors whose host calls or streams answered back to ready.
    fn wake(&mut self, now: u64) {
        let waiting = core::mem::take(&mut self.waiting);
        for key in waiting {
            let Some(actor) = self.actors.get_mut(key) else { continue };
            let awake = actor.wake_at.is_some_and(|deadline| deadline <= now) || (actor.blocked && actor.poll());
            if awake {
                actor.wake_at = None;
                actor.blocked = false;
                actor.runnable = true;
                self.schedule(key);
            } else if !actor.idle {
                // An idle actor waits on its mailbox alone.
                self.waiting.push(key);
            }
        }
    }
}
