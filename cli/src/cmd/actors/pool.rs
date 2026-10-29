use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use edge::embed::Limits;

use crate::host::{Loaded, Untrusted};
use crate::kit::{Kit, Words};

use super::scheduler::{nap, Intake, Pass, Quiesce, Scheduler};

/* every group of a pool with the ceiling on the actors of each */
pub struct Pool {
    pub groups: Vec<GroupSpec>,
    pub max_actors: usize,
}

/* one program run as interchangeable actors with every setting they share */
pub struct GroupSpec {
    pub name: String,
    // the program each actor boots with the directory and manifest it resolves through
    pub source: String,
    pub dir: String,
    pub manifest: Option<String>,
    // a ceiling the group grows to on demand
    pub replicas: usize,
    pub limits: Limits,
    pub preempt: usize,
    pub out: Out,
    // times a crashing message is retried before it is dropped dead
    pub retry: usize,
    // messages delivered before the pool starts
    pub seed: Vec<String>,
    // set when each message is an untrusted program of its own
    pub eval: Option<Eval>,
}

/* what bounds each untrusted run of an eval group */
pub struct Eval {
    // wall-clock seconds a run may last waits included
    pub timeout: u64,
    // what a bundle or a snippet holds in this pool
    pub untrusted: Untrusted,
    // the manifest a snippet that brings none resolves through
    pub manifest: String,
    // how the runtime words an error a run raises
    pub words: Option<Words>,
}

/* where the output of an actor goes */
pub enum Out {
    Stdout,
    File(String),
    Null,
}

/* a message in flight whose body the wait of an actor hands it */
pub struct Message {
    pub group: String,
    pub body: String,
    // failed deliveries so far checked against the retry count of the group
    pub attempts: usize,
    // a caller awaiting what an eval run printed or raised
    pub reply: Option<Reply>,
    // its record in a durable log marked done once an actor finishes it
    pub id: Option<u64>,
}

/* hands the outcome of an eval run to the caller waiting on it */
pub type Reply = Box<dyn FnOnce(Result<String, String>) + Send>;

impl Message {
    pub fn new(group: String, body: String) -> Self {
        Message { group, body, attempts: 0, reply: None, id: None }
    }
}

/* one engine for the whole pool that ticks when untrusted groups need deadlines */
fn runtime(kit: &Kit, pool: &Pool) -> Result<Arc<Loaded>, String> {
    let runtime = kit.load().map_err(|e| e.to_string())?;
    if pool.groups.iter().any(|g| g.eval.is_some()) {
        runtime.start_ticker();
    }
    Ok(runtime)
}

/* runs the pool to quiescence across `threads` schedulers with groups sharded round robin */
pub fn run(kit: &Kit, pool: Pool, threads: usize) -> i32 {
    let runtime = match runtime(kit, &pool) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    // one thread keeps the plain loop with no channels or router
    if threads <= 1 {
        return drive(pool, runtime, Box::new(Quiesce));
    }
    let mut group_shard = HashMap::new();
    let mut shard_groups: Vec<Vec<_>> = (0..threads).map(|_| Vec::new()).collect();
    for (i, g) in pool.groups.into_iter().enumerate() {
        group_shard.insert(g.name.clone(), i % threads);
        shard_groups[i % threads].push(g);
    }
    let (senders, receivers): (Vec<_>, Vec<_>) = (0..threads).map(|_| channel::<Message>()).unzip();
    let router = Router { shards: senders, group_shard, barrier: Arc::new(Barrier::new(threads)) };
    // a scheduler per shard thread with cross-group sends going by channel
    let handles: Vec<_> = shard_groups
        .into_iter()
        .zip(receivers)
        .map(|(groups, rx)| {
            let shard = Shard { rx, router: router.clone() };
            let (runtime, max_actors) = (runtime.clone(), pool.max_actors);
            std::thread::spawn(move || drive(Pool { groups, max_actors }, runtime, Box::new(shard)))
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap_or(1)).max().unwrap_or(0)
}

/* runs the pool on this thread over `intake` which decides when it ends */
pub fn live(kit: &Kit, pool: Pool, intake: Box<dyn Intake>) -> i32 {
    match runtime(kit, &pool) {
        Ok(runtime) => drive(pool, runtime, intake),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/* one scheduler over `intake` on this thread until the intake ends it */
fn drive(pool: Pool, runtime: Arc<Loaded>, intake: Box<dyn Intake>) -> i32 {
    match Scheduler::new(pool, runtime, intake) {
        Ok(mut s) => s.run(),
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/* where sends from other shards arrive and the router their own leave by */
struct Shard {
    rx: Receiver<Message>,
    router: Router,
}

impl Intake for Shard {
    fn pull(&mut self, into: &mut Vec<Message>, _room: usize) {
        while let Ok(m) = self.rx.try_recv() {
            self.router.barrier.consume();
            into.push(m);
        }
    }

    // with no local work it blocks until another shard sends some or the pool quiesces
    fn rest(&mut self, _into: &mut Vec<Message>, pass: Pass) -> bool {
        pass.busy || pass.waiters && nap() || !self.router.barrier.park_until_work_or_done()
    }

    fn away(&mut self, msg: Message) -> Option<Message> {
        self.router.route(msg);
        None
    }
}

// the pool ends when every shard is idle with nothing in flight
struct Barrier {
    // messages routed to a shard but not yet consumed
    inflight: AtomicUsize,
    total: usize,
    idle: Mutex<Quiescence>,
    changed: Condvar,
}

struct Quiescence {
    idle: usize,
    shutdown: bool,
}

// routes a cross-thread message to the shard owning its group
#[derive(Clone)]
struct Router {
    // one sender per shard indexed like the scheduler threads
    shards: Vec<Sender<Message>>,
    group_shard: HashMap<String, usize>,
    barrier: Arc<Barrier>,
}

impl Router {
    // counts the message in flight until its shard consumes it
    fn route(&self, msg: Message) {
        if let Some(&shard) = self.group_shard.get(&msg.group) {
            self.barrier.inflight.fetch_add(1, Ordering::Release);
            if self.shards[shard].send(msg).is_err() {
                self.barrier.inflight.fetch_sub(1, Ordering::Release);
            }
            // wakes an idle shard so it checks its channel again
            self.barrier.changed.notify_all();
        }
    }
}

impl Barrier {
    fn new(total: usize) -> Self {
        Barrier { inflight: AtomicUsize::new(0), total, idle: Mutex::new(Quiescence { idle: 0, shutdown: false }), changed: Condvar::new() }
    }

    // a message pulled off a channel no longer counts as in flight
    fn consume(&self) {
        self.inflight.fetch_sub(1, Ordering::Release);
    }

    /* parks an idle shard until work arrives and returns true once the whole pool idles */
    fn park_until_work_or_done(&self) -> bool {
        let mut q = self.idle.lock().unwrap();
        q.idle += 1;
        if q.idle == self.total && self.inflight.load(Ordering::Acquire) == 0 {
            q.shutdown = true;
            self.changed.notify_all();
        }
        while !q.shutdown && self.inflight.load(Ordering::Acquire) == 0 {
            q = self.changed.wait(q).unwrap();
        }
        if q.shutdown {
            return true;
        }
        q.idle -= 1;
        false
    }
}
