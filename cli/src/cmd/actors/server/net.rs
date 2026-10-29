use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Registry, Token, Waker};
use slab::Slab;

use super::http::{self, Parse};
use crate::host::hosted_here;
use super::{Inbox, Message, Pool, Refused, Reply, Stats, MAX_MESSAGE};

// open connections across ingress and control with any past the cap closed on accept
const MAX_CONNECTIONS: usize = 10_000;
// unread bytes every connection together may hold before one asking for more is cut
const BUFFERED: usize = 256 << 20;
// how soon a connection held back by a full inbox tries again
const RETRY: Duration = Duration::from_millis(2);
// how often control connections are checked against their deadline
const SWEEP: Duration = Duration::from_secs(1);
// time a control client has to send its request and later to take its reply
const DEADLINE: Duration = Duration::from_secs(30);
// seconds an eval caller waits past the group timeout for the runs queued ahead
const EVAL_QUEUE_WAIT: u64 = 20;
// the listeners and the waker hold the top tokens and connections their slab keys
const INGRESS: Token = Token(usize::MAX);
const CONTROL: Token = Token(usize::MAX - 1);
const WAKE: Token = Token(usize::MAX - 2);

/* what the control port answers with and the groups it routes to */
pub struct Routes {
    pub stats: Arc<Stats>,
    groups: Vec<String>,
    eval: Vec<(String, u64)>,
    // the port it bound which every request's Host must name
    port: u16,
}

impl Routes {
    pub fn of(pool: &Pool) -> Self {
        let groups = pool.groups.iter().map(|g| g.name.clone()).collect();
        let eval = pool.groups.iter().filter_map(|g| Some((g.name.clone(), g.eval.as_ref()?.timeout))).collect();
        Routes { stats: Arc::new(Stats::default()), groups, eval, port: 0 }
    }
}

/* the ingress and the loopback control port bound before anything else of the server opens */
pub struct Bound {
    ingress: std::net::TcpListener,
    control: Option<(std::net::TcpListener, Routes)>,
}

/* binds the ingress and the control port whose failure leaves the ingress serving */
pub fn bind(listen: &str, control: Option<(Vec<SocketAddr>, Routes)>) -> std::io::Result<Bound> {
    let ingress = std::net::TcpListener::bind(listen)?;
    // a wider bind is the choice actor.yml made so the boot names what it exposes
    if let Ok(at) = ingress.local_addr()
        && !at.ip().is_loopback()
    {
        eprintln!("warning: the ingress on {at} takes messages from any host that reaches it, with no authentication");
    }
    let control = control.and_then(|(addrs, routes)| match std::net::TcpListener::bind(&addrs[..]) {
        Ok(listener) => {
            let port = listener.local_addr().map_or(0, |at| at.port());
            Some((listener, Routes { port, ..routes }))
        }
        Err(e) => {
            eprintln!("warning: cannot bind control endpoint '{}': {e}", addrs[0]);
            None
        }
    });
    Ok(Bound { ingress, control })
}

/* serves the bound ingress and control port from one thread */
pub fn spawn(bound: Bound, inbox: Inbox) -> std::io::Result<()> {
    let poll = Poll::new()?;
    let ingress = register(poll.registry(), bound.ingress, INGRESS)?;
    let control = match bound.control {
        Some((listener, routes)) => Some((register(poll.registry(), listener, CONTROL)?, routes)),
        None => None,
    };
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let stopping = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let _ = on_signal(stopping.clone(), waker.clone());
    let (to, answers) = channel();
    let mut net = Net { poll, ingress, control, conns: Slab::new(), stalled: Vec::new(), inbox, to, waker, answers, waiting: HashMap::new(), durable: Vec::new(), buffered: 0, next: 0, swept: Instant::now(), stopping };
    std::thread::spawn(move || net.run());
    Ok(())
}

/* a first SIGTERM or SIGINT lets actors finish what they hold and a second exits */
#[cfg(unix)]
fn on_signal(stopping: Arc<AtomicBool>, wake: Arc<Waker>) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT])?;
    std::thread::spawn(move || {
        for _ in signals.forever() {
            if stopping.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
            let _ = wake.wake();
        }
    });
    Ok(())
}

fn register(registry: &Registry, listener: std::net::TcpListener, token: Token) -> std::io::Result<TcpListener> {
    listener.set_nonblocking(true)?;
    let mut listener = TcpListener::from_std(listener);
    registry.register(&mut listener, token, Interest::READABLE)?;
    Ok(listener)
}

/* all server connections on one thread the OS wakes only for sockets with work */
struct Net {
    poll: Poll,
    ingress: TcpListener,
    control: Option<(TcpListener, Routes)>,
    conns: Slab<Conn>,
    // ingress connections holding a message the inbox had no room for
    stalled: Vec<usize>,
    inbox: Inbox,
    // where eval runs answer and what wakes the loop for it
    to: Sender<(u64, Result<String, String>)>,
    waker: Arc<Waker>,
    answers: Receiver<(u64, Result<String, String>)>,
    // the connection each eval caller waits on by reply id
    waiting: HashMap<u64, usize>,
    // replies held until the fsync that makes their message durable
    durable: Vec<(usize, Vec<u8>)>,
    // the bytes every connection holds unread which `BUFFERED` bounds together
    buffered: usize,
    next: u64,
    swept: Instant,
    stopping: Arc<AtomicBool>,
}

enum Conn {
    Line(Line),
    Http(Http),
}

/* an ingress client sending a message per line and holding the one the inbox refused */
struct Line {
    stream: TcpStream,
    buf: Vec<u8>,
    // where the next line starts and how far past it no newline turned up
    head: usize,
    seen: usize,
    held: Option<Message>,
    eof: bool,
    // set while it waits in the stalled list so it never sits there twice
    stalled: bool,
    // whether a line came yet since a first one reading as HTTP ends it
    lined: bool,
}

/* a control client whose request is read whole and reply written before the close */
struct Http {
    stream: TcpStream,
    buf: Vec<u8>,
    state: State,
    // when the connection is cut or its eval caller hears the wait ran out
    until: Instant,
}

enum State {
    // the request still arriving and true once a 100 Continue went out
    Reading(bool),
    // parked under a reply id until its eval run answers
    Waiting(u64),
    // taken in with its reply waiting on the next fsync
    Syncing,
    // the reply and how much of it the socket took
    Writing(Vec<u8>, usize),
}

/* what a pass over a connection left it as */
enum Flow {
    Open,
    Stalled,
    Closed,
    Down,
}

impl Net {
    fn run(&mut self) {
        let mut events = Events::with_capacity(1024);
        loop {
            let wait = if self.stalled.is_empty() { self.control.as_ref().map(|_| SWEEP) } else { Some(RETRY) };
            if let Err(e) = self.poll.poll(&mut events, wait) {
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            for event in &events {
                let alive = match event.token() {
                    INGRESS | CONTROL => {
                        self.accept(event.token());
                        true
                    }
                    // dropping every connection and the inbox tells the scheduler to finish and exit
                    WAKE if self.stopping.load(Ordering::SeqCst) => return,
                    WAKE => {
                        self.answer();
                        true
                    }
                    Token(key) => self.pump(key),
                };
                if !alive {
                    return;
                }
            }
            for key in std::mem::take(&mut self.stalled) {
                if let Some(Conn::Line(line)) = self.conns.get_mut(key)
                    && line.stalled
                {
                    line.stalled = false;
                    if !self.pump(key) {
                        return;
                    }
                }
            }
            // one fsync covers every message this pass took in before their 202s go out
            if let Err(e) = self.inbox.sync() {
                eprintln!("error: the wal failed an fsync, the ingress stops taking messages: {e}");
                for (key, _) in std::mem::take(&mut self.durable) {
                    self.reply(key, http::text(503, "the message could not be logged"));
                }
                return;
            }
            for (key, reply) in std::mem::take(&mut self.durable) {
                self.reply(key, reply);
            }
            if self.swept.elapsed() >= SWEEP {
                self.sweep();
            }
        }
    }

    /* takes every waiting connection on a listener closing those past the cap */
    fn accept(&mut self, token: Token) {
        loop {
            let listener = match (token, &self.control) {
                (INGRESS, _) => &self.ingress,
                (_, Some((listener, _))) => listener,
                (_, None) => return,
            };
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                // none left or no descriptor free until a connection closes
                Err(_) => return,
            };
            if self.conns.len() >= MAX_CONNECTIONS {
                continue;
            }
            let entry = self.conns.vacant_entry();
            let interest = if token == INGRESS { Interest::READABLE } else { Interest::READABLE | Interest::WRITABLE };
            if self.poll.registry().register(&mut stream, Token(entry.key()), interest).is_ok() {
                entry.insert(match token {
                    INGRESS => Conn::Line(Line { stream, buf: Vec::new(), head: 0, seen: 0, held: None, eof: false, stalled: false, lined: false }),
                    _ => Conn::Http(Http { stream, buf: Vec::new(), state: State::Reading(false), until: Instant::now() + DEADLINE }),
                });
            }
        }
    }

    /* moves one connection on and returns false once the pool is gone */
    fn pump(&mut self, key: usize) -> bool {
        let before = self.conns.get(key).map_or(0, Conn::held);
        // what this connection may hold so every connection together stays inside the budget
        let room = BUFFERED.saturating_sub(self.buffered.saturating_sub(before));
        let flow = match self.conns.get_mut(key) {
            None => return true,
            Some(Conn::Line(line)) => {
                let flow = line.pump(&self.inbox, room);
                if matches!(flow, Flow::Stalled) && !line.stalled {
                    line.stalled = true;
                    self.stalled.push(key);
                }
                flow
            }
            Some(Conn::Http(_)) => self.pump_http(key, room),
        };
        self.buffered = self.buffered.saturating_sub(before) + self.conns.get(key).map_or(0, Conn::held);
        match flow {
            Flow::Down => return false,
            Flow::Closed => self.close(key),
            Flow::Open | Flow::Stalled => {}
        }
        true
    }

    /* reads a control request and routes it once whole then writes its reply */
    fn pump_http(&mut self, key: usize, room: usize) -> Flow {
        let Some(Conn::Http(conn)) = self.conns.get_mut(key) else { return Flow::Open };
        let State::Reading(continued) = conn.state else { return conn.flush() };
        let closed = conn.fill(room);
        let reply = match http::parse(&conn.buf) {
            Parse::Partial(_) if closed => return Flow::Closed,
            // a request past what all connections may hold is refused before it is whole
            Parse::Partial(_) if conn.buf.len() >= room => http::text(503, "the server holds too much unread input, try again"),
            Parse::Partial(expects) => {
                if expects && !continued {
                    let _ = conn.stream.write(http::CONTINUE);
                    conn.state = State::Reading(true);
                }
                return Flow::Open;
            }
            Parse::Bad(status, why) => http::text(status, why),
            Parse::Done(request) => match self.route(key, request) {
                Some(reply) => reply,
                None => return Flow::Open,
            },
        };
        self.reply(key, reply);
        Flow::Open
    }

    /* answers now or parks until the message is on disk or the eval run ends */
    fn route(&mut self, key: usize, request: http::Request) -> Option<Vec<u8>> {
        let routes = &self.control.as_ref()?.1;
        // a page on a rebound name or on any origin never drives the pool
        if !hosted_here(request.host.as_deref(), routes.port) {
            return Some(http::text(421, "the Host names no loopback address of this endpoint"));
        }
        if request.origin {
            return Some(http::text(403, "a request a web page sends is refused"));
        }
        if request.path == "/stats" {
            return Some(http::json(200, &routes.stats.to_json()));
        }
        if request.post
            && let Some(group) = request.path.strip_prefix("/pub/")
            && routes.groups.iter().any(|g| g == group)
        {
            if let Err(refused) = self.inbox.admit(Message::new(group.to_string(), request.body)) {
                return Some(refusal(&refused));
            }
            if let Some(Conn::Http(conn)) = self.conns.get_mut(key) {
                conn.state = State::Syncing;
            }
            self.durable.push((key, http::json(202, "{\"ok\":true}")));
            return None;
        }
        if request.post
            && let Some(group) = request.path.strip_prefix("/eval/")
            && let Some(&(_, timeout)) = routes.eval.iter().find(|(g, _)| g == group)
        {
            let id = self.next;
            self.next += 1;
            let (to, wake) = (self.to.clone(), self.waker.clone());
            let reply: Reply = Box::new(move |outcome| {
                if to.send((id, outcome)).is_ok() {
                    let _ = wake.wake();
                }
            });
            let msg = Message { reply: Some(reply), ..Message::new(group.to_string(), request.body) };
            if let Err(refused) = self.inbox.admit(msg) {
                return Some(refusal(&refused));
            }
            self.waiting.insert(id, key);
            if let Some(Conn::Http(conn)) = self.conns.get_mut(key) {
                conn.state = State::Waiting(id);
                conn.until = Instant::now() + Duration::from_secs(timeout + EVAL_QUEUE_WAIT);
            }
            return None;
        }
        Some(http::text(404, "not found"))
    }

    /* hands each finished eval run to the caller parked on it */
    fn answer(&mut self) {
        while let Ok((id, outcome)) = self.answers.try_recv() {
            let Some(key) = self.waiting.remove(&id) else { continue };
            let reply = match outcome {
                Ok(stdout) => http::json(200, &format!("{{\"ok\":true,\"stdout\":{}}}", http::json_str(&stdout))),
                Err(e) => http::json(500, &format!("{{\"ok\":false,\"error\":{}}}", http::json_str(&e))),
            };
            self.reply(key, reply);
        }
    }

    /* starts writing a reply and the connection closes once it is out */
    fn reply(&mut self, key: usize, reply: Vec<u8>) {
        let Some(Conn::Http(conn)) = self.conns.get_mut(key) else { return };
        conn.state = State::Writing(reply, 0);
        conn.until = Instant::now() + DEADLINE;
        if matches!(conn.flush(), Flow::Closed) {
            self.close(key);
        }
    }

    /* cuts control connections past their deadline and tells an eval caller it timed out */
    fn sweep(&mut self) {
        self.swept = Instant::now();
        let late: Vec<usize> = self.conns.iter().filter(|(_, c)| matches!(c, Conn::Http(h) if h.until <= self.swept)).map(|(k, _)| k).collect();
        for key in late {
            match self.conns[key] {
                Conn::Http(Http { state: State::Waiting(id), .. }) => {
                    self.waiting.remove(&id);
                    self.reply(key, http::text(504, "eval timed out"));
                }
                _ => self.close(key),
            }
        }
    }

    /* drops a connection then takes any a full table turned away */
    fn close(&mut self, key: usize) {
        self.buffered = self.buffered.saturating_sub(self.conns.get(key).map_or(0, Conn::held));
        let mut stream = match self.conns.remove(key) {
            Conn::Line(line) => line.stream,
            Conn::Http(conn) => {
                if let State::Waiting(id) = conn.state {
                    self.waiting.remove(&id);
                }
                conn.stream
            }
        };
        let _ = self.poll.registry().deregister(&mut stream);
        self.accept(INGRESS);
        self.accept(CONTROL);
    }
}

impl Conn {
    /* the bytes the connection holds of what its client sent */
    fn held(&self) -> usize {
        match self {
            Conn::Line(line) => line.buf.len(),
            Conn::Http(http) => http.buf.len(),
        }
    }
}

impl Line {
    /* queues each whole line reading on until the socket runs dry or the inbox fills */
    fn pump(&mut self, inbox: &Inbox, room: usize) -> Flow {
        let mut chunk = [0u8; 64 << 10];
        loop {
            if let Some(msg) = self.held.take() {
                match inbox.admit(msg) {
                    Ok(()) => {}
                    Err(Refused::Full(msg)) => {
                        self.held = Some(msg);
                        return Flow::Stalled;
                    }
                    Err(Refused::Down) => return Flow::Down,
                    // a line client hears of a message the log refused by its connection closing
                    Err(Refused::Unlogged) => return Flow::Closed,
                }
            }
            if let Some(line) = self.line() {
                let Ok(line) = String::from_utf8(line) else { return Flow::Closed };
                // a web page can post to this port and its request is no message
                if !std::mem::replace(&mut self.lined, true) && http_request(&line) {
                    return Flow::Closed;
                }
                if let Some((group, body)) = line.split_once(' ') {
                    self.held = Some(Message::new(group.to_string(), body.to_string()));
                }
                continue;
            }
            // then a closed socket or an overlong line or a spent budget ends the connection
            if self.eof || self.buf.len() - self.head > MAX_MESSAGE || self.buf.len() - self.head >= room {
                return Flow::Closed;
            }
            self.buf.drain(..self.head);
            self.seen -= self.head;
            self.head = 0;
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.eof = true;
                    if !self.buf.is_empty() {
                        self.buf.push(b'\n');
                    }
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Flow::Open,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Flow::Closed,
            }
        }
    }

    /* the next whole line without its line ending and None until its newline arrives */
    fn line(&mut self) -> Option<Vec<u8>> {
        let from = self.seen.max(self.head);
        let Some(at) = self.buf[from..].iter().position(|&b| b == b'\n') else {
            self.seen = self.buf.len();
            return None;
        };
        let line = &self.buf[self.head..from + at];
        let line = line.strip_suffix(b"\r").unwrap_or(line).to_vec();
        self.head = from + at + 1;
        self.seen = self.head;
        Some(line)
    }
}

/* whether a line reads as the request line a browser opens an HTTP request with */
fn http_request(line: &str) -> bool {
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    let version = parts.nth(1).unwrap_or_default();
    !method.is_empty() && method.bytes().all(|b| b.is_ascii_uppercase()) && version.starts_with("HTTP/") && parts.next().is_none()
}

impl Http {
    /* reads what the socket holds up to `room` and is true once the client closed */
    fn fill(&mut self, room: usize) -> bool {
        let mut chunk = [0u8; 64 << 10];
        // a full request never needs more so past it the parse answers or refuses
        while self.buf.len() <= http::HEAD_MAX + MAX_MESSAGE && self.buf.len() < room {
            match self.stream.read(&mut chunk) {
                Ok(0) => return true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return e.kind() != ErrorKind::WouldBlock,
            }
        }
        false
    }

    /* writes what the socket takes of the reply and closes once it is all out */
    fn flush(&mut self) -> Flow {
        let State::Writing(reply, sent) = &mut self.state else { return Flow::Open };
        while *sent < reply.len() {
            match self.stream.write(&reply[*sent..]) {
                Ok(0) => return Flow::Closed,
                Ok(n) => *sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Flow::Open,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Flow::Closed,
            }
        }
        Flow::Closed
    }
}

// a 503 either way so the client of a refused message tries again
fn refusal(refused: &Refused) -> Vec<u8> {
    let why = match refused {
        Refused::Full(_) => "queue is full",
        Refused::Down => "actor is down",
        Refused::Unlogged => "the message could not be logged",
    };
    http::text(503, why)
}
