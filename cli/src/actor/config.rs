use compiler::embed::Limits;

// The whole pool, groups plus the runtime knobs the CLI resolved from actor.yml.
pub struct ActorConfig {
    pub groups: Vec<Group>,
    pub max_actors: usize,
}

// One program run as a pool of actors, with its limits and output.
pub struct Group {
    pub name: String,
    // Source plus its directory, the scheduler boots a fresh interpreter per actor from these.
    pub source: String,
    pub dir: String,
    // A --manifest override, the only manifest every actor of the group resolves through.
    pub manifest: Option<String>,
    pub replicas: usize,
    pub limits: Limits,
    pub preempt: usize,
    pub out: Out,
    // Untrusted mode, actors compile each message as code and cannot send to other groups.
    pub eval: bool,
    // Wall-clock seconds an eval run may last, its waits included.
    pub timeout: u64,
    // What the pool grants eval, the most an untrusted bundle may grant.
    pub ceiling: Vec<String>,
    // Times a crashing message is retried before it is dropped to the dead count.
    pub retry: usize,
    // Seed messages delivered before the actor starts, the test and CLI entry points.
    pub inbox: Vec<Message>,
}

// Where an actor's print output goes.
#[derive(Clone)]
pub enum Out {
    Stdout,
    File(String),
    Null,
}

// A message in flight, body is the string receive() hands the actor.
#[derive(Clone)]
pub struct Message {
    pub group: String,
    pub body: String,
    // Failed delivery attempts so far, a retrying group drops it past its retry count.
    pub attempts: usize,
    // A live caller waiting on the result, set only by the control endpoint for eval runs.
    pub reply: Option<Reply>,
    // Its record in the durable log, which marks it done once an actor finishes it.
    pub id: Option<u64>,
}

impl Message {
    pub fn new(group: String, body: String) -> Self {
        Message { group, body, attempts: 0, reply: None, id: None }
    }
}

/* Where an eval run answers, one channel for every caller, the id that tells them apart, and the waker of the server loop. */
#[derive(Clone)]
pub struct Reply {
    pub id: u64,
    pub to: std::sync::mpsc::Sender<(u64, Result<String, String>)>,
    pub wake: std::sync::Arc<mio::Waker>,
}

impl Reply {
    pub fn send(&self, outcome: Result<String, String>) {
        if self.to.send((self.id, outcome)).is_ok() {
            let _ = self.wake.wake();
        }
    }
}
