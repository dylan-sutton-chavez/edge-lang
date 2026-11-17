use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::kit::Kit;

use super::scheduler::{nap, NAP};
use super::{live, Intake, Message, Pass, Pool, Reply, Stats};

mod http;
mod net;
// the durable log a restart replays the unfinished messages from
mod wal;

use wal::Wal;

// bytes one message may carry as an ingress line or a control body alike
const MAX_MESSAGE: usize = 16 << 20;
// messages sent but not yet taken by the scheduler past which the inbox refuses more
const INBOX: usize = 1024;
// the drain of a closing server stays inside the 30 seconds orchestrators wait before killing
const DRAIN: Duration = Duration::from_secs(25);

/* where a live pool takes messages and logs them */
pub struct Serve {
    // the ingress as actor.yml names it which reaches past this machine only by that choice
    pub listen: String,
    // the loopback addresses the control endpoint binds
    pub control: Option<Vec<SocketAddr>>,
    pub durable: PathBuf,
}

/* runs the pool live with ingress on `listen` and HTTP control on `control` */
pub fn serve(kit: &Kit, pool: Pool, serve: Serve) -> i32 {
    let control = serve.control.map(|addrs| (addrs, net::Routes::of(&pool)));
    let stats = control.as_ref().map(|(_, routes)| routes.stats.clone());
    // the ports bind first so a second server leaves the log of the first alone
    let bound = match net::bind(&serve.listen, control) {
        Ok(bound) => bound,
        Err(e) => {
            eprintln!("error: cannot bind ingress '{}': {e}", serve.listen);
            return 1;
        }
    };
    let held = match wal::hold(&serve.durable) {
        Ok(held) => held,
        Err(e) => {
            eprintln!("error: cannot hold wal '{}': {e}", serve.durable.display());
            return 1;
        }
    };
    let (wal, recovered) = match Wal::open(&serve.durable) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("error: cannot open wal '{}': {e}", serve.durable.display());
            return 1;
        }
    };
    let wal = Arc::new(Mutex::new(wal));
    let (tx, rx) = channel();
    let transit = Arc::new(AtomicUsize::new(0));
    let intake = Live { rx, transit: transit.clone(), wal: wal.clone(), stats, recovered, closed: None };
    // what the ingress admits before the pool boots waits in the log and the channel
    if let Err(e) = net::spawn(bound, Inbox { tx, wal: wal.clone(), transit }) {
        eprintln!("error: cannot serve ingress '{}': {e}", serve.listen);
        return 1;
    }
    let code = live(kit, pool, Box::new(intake));
    let synced = wal.lock().unwrap().sync();
    drop(held);
    match synced {
        Ok(()) => code,
        Err(e) => {
            eprintln!("error: the wal '{}' failed its last fsync: {e}", serve.durable.display());
            1
        }
    }
}

/* the way into a live pool logging each message once the pool has room */
#[derive(Clone)]
struct Inbox {
    tx: Sender<Message>,
    wal: Arc<Mutex<Wal>>,
    // sent but not yet taken as the count that holds a flood back
    transit: Arc<AtomicUsize>,
}

/* why the inbox turned a message away where a full one hands it back */
enum Refused {
    Full(Message),
    Down,
    // the log failed to take it so no client may hear it is on disk
    Unlogged,
}

impl Inbox {
    /* logs and queues a message or hands it back while `INBOX` are already in transit */
    fn admit(&self, mut msg: Message) -> Result<(), Refused> {
        if self.transit.fetch_add(1, Ordering::Relaxed) >= INBOX {
            self.transit.fetch_sub(1, Ordering::Relaxed);
            return Err(Refused::Full(msg));
        }
        // a caller awaiting a reply retries on failure so only fire-and-forget messages get logged
        if msg.reply.is_none()
            && let Err(e) = self.wal.lock().unwrap().append(&mut msg)
        {
            self.transit.fetch_sub(1, Ordering::Relaxed);
            eprintln!("error: the wal could not log a message for '{}': {e}", msg.group);
            return Err(Refused::Unlogged);
        }
        self.tx.send(msg).map_err(|_| Refused::Down)
    }

    fn sync(&self) -> std::io::Result<()> {
        self.wal.lock().unwrap().sync()
    }
}

/* the scheduler side of the inbox which closes once the ingress drops it */
struct Live {
    rx: Receiver<Message>,
    transit: Arc<AtomicUsize>,
    wal: Arc<Mutex<Wal>>,
    stats: Option<Arc<Stats>>,
    // messages a previous run logged and never finished
    recovered: Vec<Message>,
    closed: Option<Instant>,
}

impl Live {
    /* each take makes room in the inbox for one more */
    fn took(&self, msg: Message, into: &mut Vec<Message>) {
        self.transit.fetch_sub(1, Ordering::Relaxed);
        into.push(msg);
    }
}

impl Intake for Live {
    fn pull(&mut self, into: &mut Vec<Message>, room: usize) {
        // recovered messages are in the log already so they go straight to the scheduler
        into.append(&mut self.recovered);
        // past the backlog the inbox fills and the ingress and control hold their clients back
        for _ in 0..room {
            if self.closed.is_some() {
                return;
            }
            match self.rx.try_recv() {
                Ok(m) => self.took(m, into),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => self.closed = Some(Instant::now()),
            }
        }
    }

    // a closed inbox drains what the actors hold within `DRAIN` and then ends the loop
    fn rest(&mut self, into: &mut Vec<Message>, pass: Pass) -> bool {
        if let Some(at) = self.closed {
            return at.elapsed() < DRAIN && (pass.busy || pass.waiters && nap());
        }
        if pass.busy {
            return true;
        }
        if pass.full {
            return nap();
        }
        // actors parked on timers or host calls keep the loop polling
        let got = match pass.waiters {
            true => self.rx.recv_timeout(NAP).map_err(|e| e == RecvTimeoutError::Disconnected),
            false => self.rx.recv().map_err(|_| true),
        };
        match got {
            Ok(m) => self.took(m, into),
            Err(true) => self.closed = Some(Instant::now()),
            Err(false) => {}
        }
        true
    }

    fn done(&mut self, ids: &[u64]) {
        let mut wal = self.wal.lock().unwrap();
        ids.iter().for_each(|&id| wal.ack(id));
    }

    fn stats(&self) -> Option<&Stats> {
        self.stats.as_deref()
    }
}
