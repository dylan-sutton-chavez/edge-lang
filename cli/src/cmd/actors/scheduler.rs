use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slab::Slab;

use crate::host::{linear_cap, now_ns, Host, Instance, Loaded, Project, Vm};

use super::actor::{sink_for, Actor, Step};
use super::pool::{GroupSpec, Message, Pool};

// how long an idle scheduler naps before checking blocked actors again
pub const NAP: Duration = Duration::from_millis(2);
// actors per runtime instance which a 4 GiB memory holds with room to spare
const SLOTS_PER_INSTANCE: usize = 4096;
// messages a pool holds undelivered or in mailboxes before it stops pulling from its intake
const BACKLOG: usize = 1 << 16;

/* what one pass of the loop left behind for the intake to decide on */
pub struct Pass {
    pub busy: bool,
    // actors parked on a timer or a host call that keep the loop polling
    pub waiters: bool,
    pub full: bool,
}

/* where a pool takes messages from past its seeds and what it tells of them */
pub trait Intake {
    /* moves up to `room` arrivals into `into` without waiting */
    fn pull(&mut self, into: &mut Vec<Message>, room: usize);

    /* whether the loop goes on after `pass` waiting for work first when it was idle */
    fn rest(&mut self, into: &mut Vec<Message>, pass: Pass) -> bool;

    /* logged messages now done so a restart never replays them */
    fn done(&mut self, _ids: &[u64]) {}

    /* a send to a group this scheduler does not run handed on or given back */
    fn away(&mut self, msg: Message) -> Option<Message> {
        Some(msg)
    }

    /* the live counts a control endpoint reads or none */
    fn stats(&self) -> Option<&Stats> {
        None
    }
}

/* a pool with nothing past its seeds which ends once nothing is left to run */
pub struct Quiesce;

impl Intake for Quiesce {
    fn pull(&mut self, _into: &mut Vec<Message>, _room: usize) {}

    fn rest(&mut self, _into: &mut Vec<Message>, pass: Pass) -> bool {
        pass.busy || pass.waiters && nap()
    }
}

/* sleeps one nap and goes on */
pub fn nap() -> bool {
    std::thread::sleep(NAP);
    true
}

// live actor counters the scheduler writes and a control endpoint reads
#[derive(Default)]
pub struct Stats {
    actors: AtomicUsize,
    active: AtomicUsize,
    idle: AtomicUsize,
    pending: AtomicUsize,
    crashes: AtomicUsize,
    dead: AtomicUsize,
}

impl Stats {
    fn set(&self, actors: usize, idle: usize, pending: usize, crashes: usize) {
        self.actors.store(actors, Ordering::Relaxed);
        self.active.store(actors - idle, Ordering::Relaxed);
        self.idle.store(idle, Ordering::Relaxed);
        self.pending.store(pending, Ordering::Relaxed);
        self.crashes.store(crashes, Ordering::Relaxed);
    }

    /* the counts as a json object */
    pub fn to_json(&self) -> String {
        let [actors, active, idle, pending, crashes, dead] = [&self.actors, &self.active, &self.idle, &self.pending, &self.crashes, &self.dead].map(|n| n.load(Ordering::Relaxed));
        format!("{{\"actors\":{actors},\"active\":{active},\"idle\":{idle},\"pending\":{pending},\"crashes\":{crashes},\"dead\":{dead}}}")
    }
}

// a group as it runs whose replicas share runtime instances
struct Group {
    spec: GroupSpec,
    // actors spawn lazily up to this ceiling instead of all at boot
    max: usize,
    // stable keys survive removal so the work queues below never dangle
    actors: Slab<Actor>,
    // keys with work to run drained each tick instead of scanning every actor
    ready: VecDeque<usize>,
    // keys waiting on a message popped first when a message needs an actor
    idle_free: Vec<usize>,
    // keys parked on a timer or host call checked again each tick
    waiting: Vec<usize>,
    instances: Vec<Rc<RefCell<Instance>>>,
}

// the single-threaded cooperative loop owning every actor of its groups
pub struct Scheduler {
    host: Rc<Host>,
    groups: Vec<Group>,
    by_name: HashMap<String, usize>,
    pending: Vec<Message>,
    // messages sitting in mailboxes counted as they come and go instead of summed per actor
    queued: usize,
    // tracebacks of actors that raised uncaught and were retired
    crashes: Vec<String>,
    intake: Box<dyn Intake>,
}

impl Scheduler {
    pub fn new(pool: Pool, runtime: Arc<Loaded>, intake: Box<dyn Intake>) -> Result<Self, String> {
        let host = Host::new(runtime).map_err(|e| e.to_string())?;
        let mut groups = Vec::with_capacity(pool.groups.len());
        let mut by_name = HashMap::new();
        let mut pending = Vec::new();
        for mut spec in pool.groups {
            by_name.insert(spec.name.clone(), groups.len());
            pending.extend(std::mem::take(&mut spec.seed).into_iter().map(|body| Message::new(spec.name.clone(), body)));
            let max = spec.replicas.min(pool.max_actors);
            groups.push(Group { spec, max, actors: Slab::new(), ready: VecDeque::new(), idle_free: Vec::new(), waiting: Vec::new(), instances: Vec::new() });
        }
        Ok(Scheduler { host, groups, by_name, pending, queued: 0, crashes: Vec::new(), intake })
    }

    // pumps messages and runs actors until the intake ends the loop
    pub fn run(&mut self) -> i32 {
        // one actor per group so producers run while the pool grows lazily on demand
        for g in &mut self.groups {
            if g.max > 0 {
                g.spawn(&self.host);
            }
        }
        loop {
            let room = BACKLOG.saturating_sub(self.pending.len() + self.queued);
            self.intake.pull(&mut self.pending, room);
            self.route_pending();
            let busy = self.tick() || !self.pending.is_empty();
            let pass = Pass { busy, waiters: self.groups.iter().any(|g| !g.waiting.is_empty()), full: self.pending.len() + self.queued >= BACKLOG };
            if !self.intake.rest(&mut self.pending, pass) {
                break;
            }
        }
        for f in &self.crashes {
            eprintln!("{f}");
        }
        i32::from(!self.crashes.is_empty())
    }

    fn publish_stats(&self) {
        let Some(stats) = self.intake.stats() else { return };
        let actors = self.groups.iter().map(|g| g.actors.len()).sum();
        let idle = self.groups.iter().flat_map(|g| g.actors.iter()).filter(|(_, a)| a.idle).count();
        stats.set(actors, idle, self.pending.len() + self.queued, self.crashes.len());
    }

    // delivers each queued message to an actor of its group spawning on demand
    fn route_pending(&mut self) {
        for m in std::mem::take(&mut self.pending) {
            let picked = self.by_name.get(&m.group).and_then(|&gi| Some((gi, self.groups[gi].pick(&self.host)?)));
            let Some((gi, key)) = picked else {
                self.ack(m.id.as_slice());
                continue;
            };
            let g = &mut self.groups[gi];
            g.actors[key].deliver(m);
            g.schedule(key);
            self.queued += 1;
        }
    }

    // runs each queued actor once and returns false when none progressed
    fn tick(&mut self) -> bool {
        let mut progressed = false;
        let now = now_ns();
        for gi in 0..self.groups.len() {
            self.groups[gi].wake(now);
            for _ in 0..self.groups[gi].ready.len() {
                let g = &mut self.groups[gi];
                let Some(key) = g.ready.pop_front() else { break };
                let Some(actor) = g.actors.get_mut(key) else { continue };
                actor.scheduled = false;
                if actor.done || (actor.mailbox.is_empty() && actor.ran && !actor.runnable) {
                    continue;
                }
                progressed = true;
                let before = actor.mailbox.len();
                let step = actor.step(&g.spec, &self.host);
                // a step only takes from the mailbox since nothing reaches it mid-step
                self.queued -= before - actor.mailbox.len();
                let finished = actor.take_finished();
                let failed = actor.take_failed();
                self.ack(&finished);
                self.crashes.extend(failed);
                self.collect_sends(gi, key);
                self.settle(gi, key, step);
            }
        }
        self.publish_stats();
        progressed
    }

    // queues an actor again by its step outcome and retires it on a crash
    fn settle(&mut self, gi: usize, key: usize, step: Step) {
        let g = &mut self.groups[gi];
        match step {
            Step::Failed(tb, msg) => {
                self.release(gi, key);
                self.crashes.push(tb);
                self.handle_crash(gi, msg);
                self.crash_trapped(gi);
            }
            Step::Blocked(until) => {
                g.actors[key].wake_at = until;
                g.actors[key].blocked = true;
                g.waiting.push(key);
            }
            // it runs again from the back of the queue even with an empty mailbox
            Step::Yield => {
                g.actors[key].runnable = true;
                g.schedule(key);
            }
            _ if g.actors[key].done => self.release(gi, key),
            _ if g.actors[key].idle => g.idle_free.push(key),
            _ => g.schedule(key),
        }
    }

    // a trap poisons its whole instance so its actors crash and their queues move on
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
            self.crashes.push(format!("error: group '{}' lost an actor, its interpreter instance trapped", self.groups[gi].spec.name));
            self.handle_crash(gi, in_flight);
        }
    }

    /* takes an actor out of its group and its mailbox out of the count */
    fn retire(&mut self, gi: usize, key: usize) -> Actor {
        let actor = self.groups[gi].actors.remove(key);
        self.queued -= actor.mailbox.len();
        actor
    }

    /* retires an ended actor whose queue goes back out or is done when never read */
    fn release(&mut self, gi: usize, key: usize) {
        let (back, dropped) = self.retire(gi, key).leftover();
        self.pending.extend(back);
        let ids: Vec<u64> = dropped.iter().filter_map(|m| m.id).collect();
        self.ack(&ids);
    }

    // a crashed message goes back up to the retry count and then counts as dead
    fn handle_crash(&mut self, gi: usize, msg: Option<Message>) {
        let Some(mut msg) = msg else { return };
        if msg.attempts < self.groups[gi].spec.retry {
            msg.attempts += 1;
            self.pending.push(msg);
            return;
        }
        if let Some(stats) = self.intake.stats() {
            stats.dead.fetch_add(1, Ordering::Relaxed);
        }
        self.ack(msg.id.as_slice());
    }

    /* marks logged messages done so a restart never replays them */
    fn ack(&mut self, ids: &[u64]) {
        if !ids.is_empty() {
            self.intake.done(ids);
        }
    }

    // drains what the actor just sent keeping local groups and handing others to the intake
    fn collect_sends(&mut self, gi: usize, key: usize) {
        let Some(actor) = self.groups[gi].actors.get_mut(key) else { return };
        for (group, body) in actor.take_sends() {
            let msg = Message::new(group, body);
            let msg = if self.by_name.contains_key(&msg.group) { Some(msg) } else { self.intake.away(msg) };
            self.pending.extend(msg);
        }
    }
}

impl Group {
    /* boots a fresh actor in an interpreter slot unless the group evals each message apart */
    fn spawn(&mut self, host: &Rc<Host>) -> Option<usize> {
        let actor = match self.spec.eval {
            Some(_) => Actor::eval(),
            None => {
                let mut vm = self.slot(host).ok()?;
                vm.set_preempt_interval(self.spec.preempt).ok()?;
                vm.set_limits(&self.spec.limits).ok()?;
                Actor::fixed(vm)
            }
        };
        let key = self.actors.insert(actor);
        self.schedule(key);
        Some(key)
    }

    /* queues an actor to run once however often it is woken before its turn */
    fn schedule(&mut self, key: usize) {
        if let Some(actor) = self.actors.get_mut(key)
            && !actor.scheduled
        {
            actor.scheduled = true;
            self.ready.push_back(key);
        }
    }

    /* a slot in an instance with room or else a fresh instance */
    fn slot(&mut self, host: &Rc<Host>) -> anyhow::Result<Vm> {
        self.instances.retain(|i| !i.borrow().poisoned());
        if let Some(inst) = self.instances.iter().find(|i| i.borrow().slots() < SLOTS_PER_INSTANCE) {
            return Instance::slot(inst);
        }
        let project = Project::disk(&self.spec.dir, self.spec.manifest.as_deref());
        // every slot may hold its limit and the linear memory caps them together
        let memory = linear_cap(self.spec.limits.memory.saturating_mul(SLOTS_PER_INSTANCE));
        let inst = host.instance(sink_for(&self.spec.out), project, None, memory)?;
        inst.borrow_mut().accept_sends();
        let vm = Instance::slot(&inst)?;
        self.instances.push(inst);
        Ok(vm)
    }

    /* picks an idle actor then a fresh spawn under the ceiling then the least-loaded one */
    fn pick(&mut self, host: &Rc<Host>) -> Option<usize> {
        while let Some(key) = self.idle_free.pop() {
            if self.actors.get(key).is_some_and(|n| n.idle && !n.done) {
                return Some(key);
            }
        }
        if self.actors.len() < self.max
            && let Some(key) = self.spawn(host)
        {
            return Some(key);
        }
        self.actors.iter().filter(|(_, n)| !n.done).min_by_key(|(_, n)| n.mailbox.len()).map(|(k, _)| k)
    }

    // moves actors past their deadline or with answered host calls back to ready
    fn wake(&mut self, now: u64) {
        for key in std::mem::take(&mut self.waiting) {
            let Some(actor) = self.actors.get_mut(key) else { continue };
            let awake = actor.wake_at.is_some_and(|deadline| deadline <= now) || (actor.blocked && actor.poll());
            if awake {
                actor.wake_at = None;
                actor.blocked = false;
                actor.runnable = true;
                self.schedule(key);
            } else if !actor.idle {
                // an idle actor waits on its mailbox alone
                self.waiting.push(key);
            }
        }
    }
}
