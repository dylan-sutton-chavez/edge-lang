use cli::host::{Calls, Fault, Held, HostModule, Reply, Serve, WireValue};
use flate2::bufread::GzDecoder;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport};

/* requests and sockets to the hosts a package holds as `net:<host>` or `net:<host>/<prefix>` */
pub struct Net;

const CALLS: [&str; 6] = ["request", "response", "read", "close", "connect", "send"];

impl HostModule for Net {
    fn name(&self) -> &str {
        "net"
    }

    fn scope(&self, scope: &str) -> bool {
        let (host, prefix) = scope.split_at(scope.find('/').unwrap_or(scope.len()));
        let plain = |segment: &str| !segment.is_empty() && segment != "." && segment != ".." && segment.chars().all(|c| c.is_ascii_alphanumeric() || "-._~!$&'()*+,=:@".contains(c));
        // one trailing slash stays a whole host and every segment before it is plain
        let body = prefix.strip_suffix('/').unwrap_or(prefix);
        is_host(host) && plain_host(host) && body.split('/').skip(1).all(plain)
    }

    // a request or a socket reaches the host and the resolved path of its url
    fn reaches(&self, call: &str, args: &[WireValue]) -> Result<Option<String>, Fault> {
        let url = match call {
            "request" => args.get(1),
            "connect" => args.first(),
            _ => return Ok(None),
        };
        let place = plain(text(url.unwrap_or(&WireValue::None), "a url")?)?;
        Ok(Some(format!("{}{}", place.host, place.path)))
    }

    // a scope reaches its whole host or the paths under its prefix in every reading
    fn covers(&self, scope: &str, reached: &str) -> bool {
        let (host, path) = reached.split_at(reached.find('/').unwrap_or(reached.len()));
        if scope.split('/').next() != Some(host) {
            return false;
        }
        let prefix = &scope[host.len()..];
        let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
        prefix.is_empty() || readings(path).iter().all(|read| read == prefix || read.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
    }

    // the path shows only once the host is held so a refused host reads alone
    fn asked(&self, held: &[String], reached: &str) -> String {
        let host = reached.split('/').next().unwrap_or(reached);
        match held.iter().any(|scope| scope.split('/').next() == Some(host)) {
            true => reached.to_string(),
            false => host.to_string(),
        }
    }

    // the kit checks every url against the grant before a call arrives here
    fn open(&self, inst: &mut dyn Serve, spec: &str, _pkg: &str, _held: Held) -> Result<(), String> {
        inst.serve(spec, self.name(), &CALLS, Box::new(Opened { streams: HashMap::new() }))
    }
}

// one counter for every package so an id never names two streams
static NEXT: AtomicI64 = AtomicI64::new(1);

// requests and sockets one package may hold open at once
const OPEN: usize = 128;

// the seconds a request waits for its answer to begin by default and at most
const TIMEOUT: f64 = 30.0;
const TIMEOUT_MAX: f64 = 86400.0;

// messages and their bytes a socket keeps unread before it closes rather than hold more
const QUEUED: usize = 1024;
const QUEUED_BYTES: usize = 16 << 20;

/* the calls of one package and the requests and sockets it opened */
struct Opened {
    streams: HashMap<i64, Stream>,
}

enum Stream {
    Request(Arc<Exchange>),
    Socket(Socket),
}

impl Calls for Opened {
    fn call(&mut self, index: usize, args: Vec<WireValue>, reply: Reply) {
        let arg = |at: usize| args.get(at).unwrap_or(&WireValue::None);
        match CALLS[index] {
            "request" => reply.answer(self.request(arg(0), arg(1), arg(2), arg(3), arg(4), arg(5))),
            "response" => self.response(arg(0), reply),
            "read" => self.read(arg(0), reply),
            "close" => reply.answer(self.close_one(arg(0))),
            "connect" => self.connect(arg(0), reply),
            _ => reply.answer(self.send(arg(0), arg(1))),
        }
    }

    fn close(&mut self) {
        for (_, stream) in self.streams.drain() {
            stream.abort();
        }
    }
}

impl Opened {
    /* the id at once while the head and the body arrive through `response` and `read` */
    fn request(&mut self, method: &WireValue, url: &WireValue, headers: &WireValue, body: &WireValue, inflate: &WireValue, timeout: &WireValue) -> Result<WireValue, Fault> {
        let place = plain(text(url, "a url")?)?;
        let method = match method {
            WireValue::None => "GET",
            method => text(method, "a method")?,
        };
        let headers = pairs(headers)?;
        let body = content(body)?;
        let inflate = match inflate {
            WireValue::None => false,
            WireValue::Bytes(b) if b == b"gzip" => true,
            _ => return Err(Fault::invalid("inflate must be None or 'gzip'")),
        };
        let timeout = seconds(timeout)?;
        self.room()?;
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let exchange = Arc::new(Exchange::new(&place.target));
        let (asked, at) = (Asked { method: method.to_string(), headers, body, inflate, timeout }, exchange.clone());
        std::thread::spawn(move || fetch(asked, &place.target, &at));
        self.streams.insert(id, Stream::Request(exchange));
        Ok(WireValue::Int(id as i128))
    }

    /* the status and headers of a request once they arrive */
    fn response(&mut self, id: &WireValue, reply: Reply) {
        match self.stream(id) {
            Ok(Stream::Request(exchange)) => exchange.head(reply),
            Ok(Stream::Socket(_)) => reply.fail(Fault::invalid("response takes a request from net.request")),
            Err(fault) => reply.fail(fault),
        }
    }

    /* the next body chunk or socket message or None once it ended */
    fn read(&mut self, id: &WireValue, reply: Reply) {
        match self.stream(id) {
            Ok(Stream::Request(exchange)) => exchange.read(reply),
            Ok(Stream::Socket(socket)) => socket.read(reply),
            Err(fault) => reply.fail(fault),
        }
    }

    /* the socket id once it opens while its messages arrive through `read` */
    fn connect(&mut self, url: &WireValue, reply: Reply) {
        let place = match text(url, "a url").and_then(plain) {
            Ok(place) => place,
            Err(fault) => return reply.fail(fault),
        };
        if let Err(fault) = self.room() {
            return reply.fail(fault);
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (flow, (outgoing, queued)) = (Arc::new(Mutex::new(Flow::default())), channel());
        let (at, target) = (flow.clone(), place.target.clone());
        std::thread::spawn(move || serve(&place.target, id, reply, &at, &queued));
        self.streams.insert(id, Stream::Socket(Socket { flow, outgoing, target }));
    }

    fn send(&mut self, id: &WireValue, data: &WireValue) -> Result<WireValue, Fault> {
        let Stream::Socket(socket) = self.stream(id)? else { return Err(Fault::invalid("send takes a socket from net.connect")) };
        if !lock(&socket.flow).open {
            return Err(Fault::failed("the socket is not open"));
        }
        let message = match content(data)? {
            Some((text, true)) => Message::text(String::from_utf8_lossy(&text).into_owned()),
            Some((bytes, false)) => Message::binary(bytes),
            None => Message::binary(Vec::new()),
        };
        socket.outgoing.send(Outgoing::Message(message)).map_err(|_| Fault::failed("the socket is not open"))?;
        Ok(WireValue::None)
    }

    /* aborts a request or closes a socket */
    fn close_one(&mut self, id: &WireValue) -> Result<WireValue, Fault> {
        self.stream(id)?;
        if let Some(stream) = id_of(id).and_then(|id| self.streams.remove(&id)) {
            stream.abort();
        }
        Ok(WireValue::None)
    }

    /* refuses another request or socket while the package holds `OPEN` of them */
    fn room(&self) -> Result<(), Fault> {
        match self.streams.len() < OPEN {
            true => Ok(()),
            false => Err(Fault::failed(format!("net holds {OPEN} open requests and sockets for this package, close one first"))),
        }
    }

    /* what an id names among the streams this package opened */
    fn stream(&self, id: &WireValue) -> Result<&Stream, Fault> {
        let Some(id) = id_of(id) else { return Err(Fault::invalid("an id is an int from net.request or net.connect")) };
        match self.streams.get(&id) {
            Some(Stream::Socket(socket)) if lock(&socket.flow).gone => None,
            found => found,
        }
        .ok_or_else(|| Fault::invalid(format!("no open request or socket {id}")))
    }
}

impl Stream {
    fn abort(self) {
        match self {
            Stream::Request(exchange) => exchange.close(),
            Stream::Socket(socket) => drop(socket.outgoing.send(Outgoing::Close)),
        }
    }
}

// a whole number a page holds exactly names an id
fn id_of(value: &WireValue) -> Option<i64> {
    const SAFE: i64 = (1 << 53) - 1;
    match value {
        WireValue::Int(id) => i64::try_from(*id).ok().filter(|id| id.abs() <= SAFE),
        WireValue::Float(f) if f.fract() == 0.0 && f.abs() <= SAFE as f64 => Some(*f as i64),
        _ => None,
    }
}

/* the seconds a request waits for its answer where none gives the default */
fn seconds(value: &WireValue) -> Result<f64, Fault> {
    let seconds = match value {
        WireValue::None => return Ok(TIMEOUT),
        WireValue::Int(n) => *n as f64,
        WireValue::Float(f) => *f,
        _ => f64::NAN,
    };
    match seconds > 0.0 && seconds <= TIMEOUT_MAX {
        true => Ok(seconds),
        false => Err(Fault::invalid("timeout must be a number of seconds above 0 and up to 86400")),
    }
}

fn text<'a>(value: &'a WireValue, what: &str) -> Result<&'a str, Fault> {
    match value {
        WireValue::Bytes(b) => std::str::from_utf8(b).map_err(|_| Fault::invalid(format!("{what} must be a str"))),
        _ => Err(Fault::invalid(format!("{what} must be a str"))),
    }
}

/* a body as its bytes and whether it was text or none when absent */
fn content(value: &WireValue) -> Result<Option<(Vec<u8>, bool)>, Fault> {
    match value {
        WireValue::None => Ok(None),
        WireValue::Bytes(b) => Ok(Some((b.clone(), true))),
        WireValue::Raw(b) => Ok(Some((b.clone(), false))),
        _ => Err(Fault::invalid("a body must be bytes, a str or None")),
    }
}

// headers fetch keeps for the host are refused not dropped so both hosts answer alike
const FORBIDDEN: [&str; 21] = [
    "accept-charset", "accept-encoding", "access-control-request-headers", "access-control-request-method", "connection", "content-length",
    "cookie", "cookie2", "date", "dnt", "expect", "host", "keep-alive", "origin", "referer", "set-cookie", "te", "trailer",
    "transfer-encoding", "upgrade", "via",
];

/* the headers a program sends as pairs each checked in order */
fn pairs(value: &WireValue) -> Result<Vec<(String, String)>, Fault> {
    let shape = || Fault::invalid("headers are [name, value] pairs or a dict");
    let entries: Vec<Option<(&WireValue, &WireValue)>> = match value {
        WireValue::None => return Ok(Vec::new()),
        WireValue::List(items) => items.iter().map(|pair| match pair {
            WireValue::List(pair) if pair.len() == 2 => Some((&pair[0], &pair[1])),
            _ => None,
        }).collect(),
        WireValue::Dict(pairs) => pairs.iter().map(|(name, value)| Some((name, value))).collect(),
        _ => return Err(shape()),
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let (name, value) = entry.ok_or_else(shape)?;
        let name = text(name, "a header name")?;
        let lower = name.to_ascii_lowercase();
        if FORBIDDEN.contains(&lower.as_str()) || lower.starts_with("proxy-") || lower.starts_with("sec-") {
            return Err(Fault::invalid(format!("the header '{name}' belongs to the host, a program cannot set it")));
        }
        out.push((name.to_string(), text(value, "a header value")?.to_string()));
    }
    Ok(out)
}

/* a url as its lowercase host and the escaped resolved path both hosts send */
struct Place {
    host: String,
    path: String,
    target: String,
}

// a dotted lowercase host as a scope names it and a url reaches it
fn is_host(host: &str) -> bool {
    !host.is_empty() && host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

/* whether a host reads alike everywhere since browsers take a numeric ending as an ip4 */
fn plain_host(host: &str) -> bool {
    let last = host.split('.').rfind(|label| !label.is_empty()).unwrap_or_default();
    let hex = last.get(..2).is_some_and(|x| x.eq_ignore_ascii_case("0x")) && last[2..].bytes().all(|b| b.is_ascii_hexdigit());
    let numeric = hex || (!last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()));
    !numeric || (host.split('.').count() == 4 && host.split('.').all(|octet| octet.parse::<u8>().is_ok() && (octet == "0" || !octet.starts_with('0'))))
}

/* the parts of a url every parser reads alike or the refusal naming it */
fn plain(url: &str) -> Result<Place, Fault> {
    let refused = || Fault::invalid(format!("'{url}' is not a plain url, net takes scheme://host/path with no user, backslash or space"));
    // these characters can move a host or end the match early
    if !url.chars().all(|c| c > ' ' && c != '\\' && c != '\x7f' && c != '\u{2028}' && c != '\u{2029}') {
        return Err(refused());
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(refused)?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss") {
        return Err(refused());
    }
    let named = rest.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-').count();
    let (name, rest) = rest.split_at(named);
    let digits = rest.strip_prefix(':').map(|after| after.bytes().take_while(u8::is_ascii_digit).count());
    let (port, rest) = match digits {
        Some(1..=5) => rest.split_at(1 + digits.unwrap_or(0)),
        Some(_) => return Err(refused()),
        None => ("", rest),
    };
    let host = name.to_ascii_lowercase();
    if host.is_empty() || !(rest.is_empty() || rest.starts_with(['/', '?', '#'])) || !plain_host(&host) {
        return Err(refused());
    }
    // a fragment never leaves the host so a grant never reaches it
    let addressed = rest.split('#').next().unwrap_or_default();
    let (path, query) = addressed.split_at(addressed.find('?').unwrap_or(addressed.len()));
    let path: String = resolve(path).chars().map(escape).collect();
    Ok(Place { target: format!("{scheme}://{host}{port}{path}{query}"), host, path })
}

// what a path may carry as written while the rest crosses as escaped UTF-8 bytes
fn escape(c: char) -> String {
    if c.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:@/%".contains(c) {
        return c.to_string();
    }
    c.to_string().bytes().map(|b| format!("%{b:02X}")).collect()
}

// a segment of `count` dots each spelled plainly or escaped
fn dots(segment: &str, count: usize) -> bool {
    let (mut rest, mut seen) = (segment, 0);
    while !rest.is_empty() {
        rest = match rest.strip_prefix('.') {
            Some(after) => after,
            None if rest.get(..3).is_some_and(|x| x.eq_ignore_ascii_case("%2e")) => &rest[3..],
            None => return false,
        };
        seen += 1;
    }
    seen == count
}

/* applies dot segments however they are spelled and never climbs above the root */
fn resolve(path: &str) -> String {
    let segments: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    for (at, segment) in segments.iter().enumerate() {
        let above = dots(segment, 2);
        if above {
            out.pop();
        }
        if !above && !dots(segment, 1) {
            out.push(segment);
        } else if at == segments.len() - 1 {
            out.push("");
        }
    }
    format!("/{}", out.join("/"))
}

// an escape a server may undo before routing where every server decodes the unreserved ones
fn decode(text: &str, unreserved_only: bool) -> String {
    let (mut out, mut rest) = (String::with_capacity(text.len()), text);
    while let Some(c) = rest.chars().next() {
        let hex = rest.strip_prefix('%').and_then(|after| after.get(..2)).filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()));
        match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
            Some(b) if !unreserved_only || b.is_ascii_alphanumeric() || b"-._~".contains(&b) => {
                out.push(char::from(b));
                rest = &rest[3..];
            }
            _ => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/* every path a server might route `path` to decoded up to twice */
fn readings(path: &str) -> [String; 3] {
    let all = decode(path, false);
    [decode(path, true), decode(&all, false), all].map(|read| {
        let mut plain = String::with_capacity(read.len());
        let mut parameter = false;
        for c in read.chars().map(|c| if c == '\\' { '/' } else { c }) {
            parameter = c == ';' || (parameter && c != '/');
            if !parameter {
                plain.push(c);
            }
        }
        resolve(&plain)
    })
}

/* refuses unless the grant is unbounded or a scope reaches the host within its prefix */
#[cfg(test)]
fn reach(held: &Held, place: &Place) -> Result<(), Fault> {
    held.reach(&Net, &format!("{}{}", place.host, place.path))
}

// statuses that hand a request elsewhere which only a new request may follow
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];

// how much of a body one read hands over
const CHUNK: usize = 64 << 10;

fn lock<T>(at: &Mutex<T>) -> MutexGuard<'_, T> {
    at.lock().unwrap_or_else(|e| e.into_inner())
}

/* what a request asked for carried to the thread that sends it */
struct Asked {
    method: String,
    headers: Vec<(String, String)>,
    body: Option<(Vec<u8>, bool)>,
    inflate: bool,
    // the seconds the request waits for its answer to begin
    timeout: f64,
}

/* one request between the package and the thread reading its body a chunk per ask */
struct Exchange {
    state: Mutex<Exchanged>,
    asked: Condvar,
    target: String,
}

#[derive(Default)]
struct Exchanged {
    head: Option<Result<WireValue, Fault>>,
    heads: Vec<Reply>,
    // each read waiting with the moment the program asked for it
    reads: VecDeque<(Reply, Instant)>,
    // how the body stopped once it did
    ended: Option<Result<(), Fault>>,
    closed: bool,
}

impl Exchange {
    fn new(target: &str) -> Exchange {
        Exchange { state: Mutex::new(Exchanged::default()), asked: Condvar::new(), target: target.to_string() }
    }

    // a known head still parks the run as the page host does
    fn head(&self, reply: Reply) {
        let mut state = lock(&self.state);
        match &state.head {
            Some(head) => reply.later(head.clone()),
            None => state.heads.push(reply),
        }
    }

    fn read(&self, reply: Reply) {
        let mut state = lock(&self.state);
        match &state.ended {
            Some(Ok(())) => reply.later(Ok(WireValue::None)),
            Some(Err(fault)) => reply.later(Err(fault.clone())),
            None => {
                state.reads.push_back((reply, Instant::now()));
                self.asked.notify_one();
            }
        }
    }

    /* the head arrived or failed and every call waiting on it hears so */
    fn arrived(&self, head: Result<WireValue, Fault>) {
        let mut state = lock(&self.state);
        for reply in state.heads.drain(..) {
            reply.later(head.clone());
        }
        if let Err(fault) = &head {
            Exchanged::end(&mut state, Err(fault.clone()));
        }
        state.head = Some(head);
    }

    /* the next read asked and when or none once the program closed the request */
    fn next(&self) -> Option<(Reply, Instant)> {
        let mut state = lock(&self.state);
        loop {
            if state.closed || state.ended.is_some() {
                return None;
            }
            if let Some(read) = state.reads.pop_front() {
                return Some(read);
            }
            state = self.asked.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn end(&self, how: Result<(), Fault>) {
        Exchanged::end(&mut lock(&self.state), how);
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        let fault = Fault::failed(format!("net.request to {} failed, the request was closed", self.target));
        for reply in state.heads.drain(..) {
            reply.later(Err(fault.clone()));
        }
        Exchanged::end(&mut state, Err(fault));
        self.asked.notify_one();
    }
}

impl Exchanged {
    // the first end of a body stands and every read waiting on it hears it
    fn end(&mut self, how: Result<(), Fault>) {
        let how = self.ended.get_or_insert(how).clone();
        for (reply, _) in self.reads.drain(..) {
            reply.later(how.clone().map(|()| WireValue::None));
        }
    }
}

/* sends one request and hands its body over a chunk per read asked */
fn fetch(asked: Asked, target: &str, exchange: &Exchange) {
    let seconds = asked.timeout;
    let mut body = match start(asked, target) {
        Ok((head, body)) => {
            exchange.arrived(Ok(head));
            body
        }
        Err(fault) => return exchange.arrived(Err(fault)),
    };
    while let Some((reply, at)) = exchange.next() {
        match body.chunk(at) {
            Ok(Some(chunk)) => reply.later(Ok(WireValue::Raw(chunk))),
            Ok(None) => {
                reply.later(Ok(WireValue::None));
                return exchange.end(Ok(()));
            }
            Err(why) => {
                let fault = match stalled(&why) {
                    true => Fault::failed(format!("reading the response failed, nothing arrived within {seconds} s")),
                    false => Fault::failed(format!("reading the response failed, {why}")),
                };
                reply.later(Err(fault.clone()));
                return exchange.end(Err(fault));
            }
        }
    }
}

/* the head and body reader of a request or why they never arrived */
fn start(asked: Asked, target: &str) -> Result<(WireValue, Body), Fault> {
    let failed = |why: String| Fault::failed(format!("net.request to {target} failed, {why}"));
    let method = method(&asked.method).map_err(failed)?;
    if asked.body.is_some() && matches!(method.as_str(), "GET" | "HEAD") {
        return Err(failed(format!("a {method} request carries no body")));
    }
    // each step up to the head and each body read gets the timeout
    let wait = Duration::from_secs_f64(asked.timeout);
    let config = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0);
    let config = config.timeout_resolve(Some(wait)).timeout_connect(Some(wait)).timeout_send_request(Some(wait)).timeout_send_body(Some(wait)).timeout_recv_response(Some(wait));
    let by: Deadline = Arc::new(Mutex::new(None));
    // a redirect comes back as it is so the program requests the new address itself
    let agent = ureq::Agent::with_parts(config.build(), DefaultConnector::new().chain(Paced(by.clone())), DefaultResolver::default());
    let mut request = ureq::http::Request::builder().method(method.as_str()).uri(wire(target));
    let typed = asked.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    for (name, value) in asked.headers {
        request = request.header(name, value);
    }
    let (bytes, text) = asked.body.unwrap_or_default();
    // text goes out as fetch sends a string body when the program names no type
    if text && !typed {
        request = request.header("content-type", "text/plain;charset=UTF-8");
    }
    let timeout = asked.timeout;
    let response = match request.body(bytes) {
        Ok(request) => agent.run(request).map_err(|e| match e {
            ureq::Error::Timeout(_) => late(target, timeout),
            e => failed(e.to_string()),
        })?,
        Err(e) => return Err(failed(e.to_string())),
    };
    let status = response.status().as_u16();
    if REDIRECTS.contains(&status) {
        return Err(Fault::failed(format!("net.request to {target} was redirected, request the new address with its own net.request")));
    }
    let head = WireValue::List(vec![WireValue::Int(status.into()), received(response.headers())]);
    let reader = response.into_body().into_reader();
    let reader = match asked.inflate {
        true => Reader::Gzip(Box::new(GzDecoder::new(BufReader::new(reader)))),
        false => Reader::Plain(reader),
    };
    Ok((head, Body { reader, by, wait }))
}

// fetch normalizes these six and refuses the three that tunnel or echo
fn method(method: &str) -> Result<String, String> {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if method.is_empty() || !method.bytes().all(token) {
        return Err(format!("'{method}' is not a valid method"));
    }
    let upper = method.to_ascii_uppercase();
    match upper.as_str() {
        "CONNECT" | "TRACE" | "TRACK" => Err(format!("the method '{method}' is forbidden")),
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" => Ok(upper),
        _ => Ok(method.to_string()),
    }
}

// the query crosses escaped as a page sends it while the path was escaped already
fn wire(target: &str) -> String {
    let (head, query) = target.split_at(target.find('?').unwrap_or(target.len()));
    let escaped: String = query.chars().map(|c| match c {
        '"' | '<' | '>' | '\'' => format!("%{:02X}", c as u32),
        c if !c.is_ascii() => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        c => c.to_string(),
    }).collect();
    format!("{head}{escaped}")
}

/* the response headers as fetch lists them sorted and joined with no cookie */
fn received(headers: &ureq::http::HeaderMap) -> WireValue {
    let mut named: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.as_str();
        if name == "set-cookie" || name == "set-cookie2" {
            continue;
        }
        // header bytes read one character each as a page reads them
        let value: String = value.as_bytes().iter().map(|&b| char::from(b)).collect();
        match named.iter_mut().find(|(seen, _)| seen == name) {
            Some((_, joined)) => {
                joined.push_str(", ");
                joined.push_str(&value);
            }
            None => named.push((name.to_string(), value)),
        }
    }
    named.sort_by(|a, b| a.0.cmp(&b.0));
    WireValue::List(named.into_iter().map(|(name, value)| WireValue::List(vec![WireValue::Bytes(name.into_bytes()), WireValue::Bytes(value.into_bytes())])).collect())
}

enum Reader {
    Plain(ureq::BodyReader<'static>),
    Gzip(Box<GzDecoder<BufReader<ureq::BodyReader<'static>>>>),
}

/* a body reader and the deadline its connection holds the read in progress to */
struct Body {
    reader: Reader,
    by: Deadline,
    wait: Duration,
}

impl Body {
    /* the next chunk inflated when asked or none at the end */
    fn chunk(&mut self, at: Instant) -> std::io::Result<Option<Vec<u8>>> {
        // the timeout counts from the moment the program asked and only while it waits
        *lock(&self.by) = Some(at + self.wait);
        let mut buf = vec![0u8; CHUNK];
        let n = match &mut self.reader {
            Reader::Plain(reader) => reader.read(&mut buf),
            Reader::Gzip(reader) => reader.read(&mut buf),
        }?;
        if n > 0 {
            buf.truncate(n);
            return Ok(Some(buf));
        }
        // bytes after the gzip stream fail the body as a page decoder does
        if let Reader::Gzip(reader) = &mut self.reader
            && !reader.get_mut().fill_buf()?.is_empty()
        {
            return Err(std::io::Error::other("junk found after the end of the gzip stream"));
        }
        Ok(None)
    }
}

/* the moment the body read in progress gives up and none before the first read */
type Deadline = Arc<Mutex<Option<Instant>>>;

/* wraps each connection of a request so a body read waits only until its deadline */
#[derive(Debug)]
struct Paced(Deadline);

impl<In: Transport> Connector<In> for Paced {
    type Out = PacedTransport<In>;

    fn connect(&self, _: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| PacedTransport { inner, by: self.0.clone() }))
    }
}

/* a connection whose waits for bytes stop at the deadline of the read that asked */
#[derive(Debug)]
struct PacedTransport<T> {
    inner: T,
    by: Deadline,
}

impl<T: Transport> Transport for PacedTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let Some(by) = *lock(&self.by) else { return self.inner.await_input(timeout) };
        let left = by.saturating_duration_since(Instant::now());
        // a socket never waits for no time so a spent read fails here
        if left.is_zero() {
            return Err(ureq::Error::Timeout(ureq::Timeout::RecvBody));
        }
        let left = ureq::unversioned::transport::time::Duration::Exact(left);
        match left < timeout.after {
            true => self.inner.await_input(NextTimeout { after: left, reason: ureq::Timeout::RecvBody }),
            false => self.inner.await_input(timeout),
        }
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/* whether a body read failed since nothing arrived before its deadline */
fn stalled(why: &std::io::Error) -> bool {
    why.get_ref().and_then(|inner| inner.downcast_ref::<ureq::Error>()).is_some_and(|e| matches!(e, ureq::Error::Timeout(_)))
}

/* a request whose answer did not begin within the seconds it was given */
fn late(target: &str, seconds: f64) -> Fault {
    Fault::failed(format!("net.request to {target} failed, no answer within {seconds} s"))
}

enum Outgoing {
    Message(Message),
    Close,
}

/* a socket the package opened with the messages it was sent */
struct Socket {
    flow: Arc<Mutex<Flow>>,
    outgoing: Sender<Outgoing>,
    target: String,
}

#[derive(Default)]
struct Flow {
    messages: VecDeque<WireValue>,
    reads: VecDeque<Reply>,
    open: bool,
    ended: bool,
    // a socket that never opened names no stream
    gone: bool,
    // the bytes the queued messages hold and whether the queue filled so the socket closed
    bytes: usize,
    flooded: bool,
}

impl Socket {
    fn read(&self, reply: Reply) {
        let mut flow = lock(&self.flow);
        match flow.messages.pop_front() {
            Some(message) => {
                flow.bytes -= size(&message);
                reply.ok(message)
            }
            None if flow.flooded => reply.fail(Fault::failed(format!("the socket to {} closed once the messages nobody read filled its queue", self.target))),
            None if flow.ended => reply.ok(WireValue::None),
            None => flow.reads.push_back(reply),
        }
    }
}

impl Flow {
    /* hands a message to a waiting read or queues it and is false once full */
    fn deliver(&mut self, message: WireValue) -> bool {
        match self.reads.pop_front() {
            Some(reply) => reply.later(Ok(message)),
            None if self.messages.len() >= QUEUED || self.bytes + size(&message) > QUEUED_BYTES => {
                self.flooded = true;
                self.end();
                return false;
            }
            None => {
                self.bytes += size(&message);
                self.messages.push_back(message);
            }
        }
        true
    }

    // every later read answers None so a closed socket never hangs a reader
    fn end(&mut self) {
        self.open = false;
        self.ended = true;
        for reply in self.reads.drain(..) {
            reply.later(Ok(WireValue::None));
        }
    }
}

/* the bytes a socket message holds as both hosts count them */
fn size(message: &WireValue) -> usize {
    match message {
        WireValue::Bytes(bytes) | WireValue::Raw(bytes) => bytes.len(),
        _ => 0,
    }
}

// how long a socket waits on a message before it looks for one to send
const SOCKET_POLL: Duration = Duration::from_millis(20);

/* keeps one socket open sending what the program queues and handing on what arrives */
fn serve(target: &str, id: i64, opened: Reply, flow: &Mutex<Flow>, queued: &Receiver<Outgoing>) {
    // an http url opens its ws twin as a page socket does
    let url = target.strip_prefix("http").map_or_else(|| target.to_string(), |rest| format!("ws{rest}"));
    // redirects fail the socket as in a browser since the check read only this url
    let mut socket = match tungstenite::client::connect_with_config(wire(&url).as_str(), None, 0) {
        Ok((socket, _)) => socket,
        Err(_) => {
            let mut flow = lock(flow);
            flow.gone = true;
            flow.end();
            return opened.later(Err(Fault::failed(format!("the socket to {target} failed"))));
        }
    };
    let tcp: Option<&TcpStream> = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => Some(tcp),
        MaybeTlsStream::Rustls(tls) => Some(tls.get_ref()),
        _ => None,
    };
    if let Some(tcp) = tcp {
        let _ = tcp.set_read_timeout(Some(SOCKET_POLL));
    }
    lock(flow).open = true;
    opened.later(Ok(WireValue::Int(id.into())));
    loop {
        loop {
            let sent = match queued.try_recv() {
                Ok(Outgoing::Message(message)) => socket.send(message),
                Ok(Outgoing::Close) | Err(TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    let _ = socket.flush();
                    return lock(flow).end();
                }
                Err(TryRecvError::Empty) => break,
            };
            if sent.is_err() {
                return lock(flow).end();
            }
        }
        let delivered = match socket.read() {
            Ok(Message::Text(text)) => lock(flow).deliver(WireValue::Bytes(text.as_bytes().to_vec())),
            Ok(Message::Binary(bytes)) => lock(flow).deliver(WireValue::Raw(bytes.to_vec())),
            Ok(Message::Close(_)) => return lock(flow).end(),
            Ok(_) => true,
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => true,
            Err(_) => return lock(flow).end(),
        };
        // a program that reads nothing while messages pile up gets the socket closed
        if !delivered {
            let _ = socket.close(None);
            let _ = socket.flush();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cli::host::{Class, Scopes};

    fn text(s: &str) -> WireValue {
        WireValue::Bytes(s.as_bytes().to_vec())
    }

    /* what a grant of `scopes` to main gives net worded as the grant logic refuses */
    fn granted(scopes: &[&str]) -> Held {
        let scopes: Vec<String> = scopes.iter().map(|scope| scope.to_string()).collect();
        let refusal = edge::deps::perms::refusal("main", "net", &scopes);
        Held::Scopes(Scopes::new(scopes, refusal))
    }

    #[test]
    fn a_scope_is_a_lowercase_host_and_plain_segments() {
        for good in ["api.example.com", "10.0.0.255", "api.example.com/v2/items", "a.test/", "a.test/v1.2/@dylan", "127.0.0.1/api"] {
            assert!(Net.scope(good), "{good}");
        }
        for bad in ["https://api.example.com/", "a.test;script-src", "user@a.test", "API.example.com", "a.test/api/..", "a.test/caf%C3%A9", "a.test//api", "[::1]", "0177.0.0.1", "2130706433", "0x7f.1", "", "a.test/./x", "a.test/api//"] {
            assert!(!Net.scope(bad), "{bad}");
        }
    }

    #[test]
    fn a_url_reads_as_every_parser_does() {
        for url in ["http://evil.test\\@127.0.0.1/", "http://user@127.0.0.1/", "http://127.0.0.1:8@evil.test/", "http://127.0.0.1/a b", "http://12%37.0.0.1/", "127.0.0.1/", "http://[::1]/", "http://0x7f.1/", "http://2130706433/", "http://cañón.example/", "ftp://a.test/", "http://a.test:123456/", "http://a.test:/", "http://a.test/\u{2028}"] {
            let refused = plain(url).err().map(|fault| fault.text);
            assert_eq!(refused.as_deref(), Some(format!("'{url}' is not a plain url, net takes scheme://host/path with no user, backslash or space").as_str()), "{url}");
        }
        let sent = |url: &str| plain(url).map(|place| place.target).unwrap();
        assert_eq!(sent("HTTP://API.Example.com:8080"), "http://api.example.com:8080/");
        for (asked, want) in [
            ("/a/b/../c", "/a/c"),
            ("/a/./b", "/a/b"),
            ("/a/b/..", "/a/"),
            ("/..", "/"),
            ("/a/%2e%2e/b", "/b"),
            ("/Español", "/Espa%C3%B1ol"),
            ("/@dylan", "/@dylan"),
            ("/items?q=a/../b", "/items?q=a/../b"),
            ("/keep#gone", "/keep"),
            ("/../@evil.test/x", "/@evil.test/x"),
            ("/%2E%2E/.evil.test/x", "/.evil.test/x"),
            ("/a{b}", "/a%7Bb%7D"),
        ] {
            assert_eq!(sent(&format!("http://127.0.0.1:1{asked}")), format!("http://127.0.0.1:1{want}"), "{asked}");
        }
        assert_eq!(wire("http://a.test/x?q=é<\"'>{|}"), "http://a.test/x?q=%C3%A9%3C%22%27%3E{|}");
    }

    #[test]
    fn a_path_scope_reaches_only_under_its_prefix() {
        let held = granted(&["a.test/api", "b.test"]);
        let reached = |url: &str| reach(&held, &plain(url).unwrap()).map_err(|fault| (fault.class, fault.text));
        let refused = |what: &str| Err((Some(Class::Denied), format!("'main' has no net:{what}, edge.json grants it net:a.test/api, net:b.test")));
        for path in ["/api", "/api/", "/api/items", "/api/v2/items?q=1", "/api/a%5cb", "/api/x%2e", "/api/group%2Fproject", "/%61pi/x"] {
            assert_eq!(reached(&format!("http://a.test{path}")), Ok(()), "{path}");
        }
        assert_eq!(reached("http://b.test/anything"), Ok(()));
        assert_eq!(reached("http://a.test/"), refused("a.test/"));
        assert_eq!(reached("http://a.test/apixyz"), refused("a.test/apixyz"));
        assert_eq!(reached("http://a.test/other/api"), refused("a.test/other/api"));
        assert_eq!(reached("http://c.test/api"), refused("c.test"));
        for path in ["/api/../secret", "/api/%2e%2e/secret", "/api/.%2e/secret", "/api/./../secret"] {
            assert_eq!(reached(&format!("http://a.test{path}")), refused("a.test/secret"), "{path}");
        }
        for path in ["/api/..%2fsecret", "/api/%2E%2E%2Fsecret", "/api/..;/secret", "/api/%252e%252e/secret"] {
            assert_eq!(reached(&format!("http://a.test{path}")), refused(&format!("a.test{path}")), "{path}");
        }
        assert!(reach(&granted(&["a.test/api/"]), &plain("http://a.test/api").unwrap()).is_ok());
        assert_eq!(reach(&granted(&[]), &plain("http://a.test/").unwrap()).unwrap_err().text, "'main' has no net:a.test, edge.json grants it nothing");
    }

    // a composition without grants hands every package every host
    #[test]
    fn an_unbounded_grant_reaches_any_host() {
        for url in ["http://evil.example/", "http://a.test/api/../secret", "ws://127.0.0.1:1/"] {
            assert!(reach(&Held::All, &plain(url).unwrap()).is_ok(), "{url}");
        }
    }

    #[test]
    fn the_headers_fetch_keeps_for_the_host_are_refused() {
        for name in ["Host", "cookie", "Origin", "Content-Length", "Sec-Fetch-Site", "Proxy-Authorization"] {
            let refused = pairs(&WireValue::List(vec![WireValue::List(vec![text(name), text("x")])])).unwrap_err().text;
            assert_eq!(refused, format!("the header '{name}' belongs to the host, a program cannot set it"));
        }
        let dict = WireValue::Dict(vec![(text("x-probe"), text("1"))]);
        assert_eq!(pairs(&dict).unwrap(), [("x-probe".to_string(), "1".to_string())]);
        for bad in [text("x-probe"), WireValue::Int(1), WireValue::List(vec![text("x-probe")])] {
            assert_eq!(pairs(&bad).unwrap_err().text, "headers are [name, value] pairs or a dict");
        }
    }

    #[test]
    fn a_timeout_is_seconds_above_zero_and_none_waits_the_default() {
        assert_eq!(seconds(&WireValue::None).unwrap(), TIMEOUT);
        assert_eq!(seconds(&WireValue::Int(2)).unwrap(), 2.0);
        assert_eq!(seconds(&WireValue::Float(0.25)).unwrap(), 0.25);
        for bad in [WireValue::Int(0), WireValue::Float(-1.0), WireValue::Float(f64::INFINITY), WireValue::Float(f64::NAN), WireValue::Int(86401), WireValue::Bool(true), text("5")] {
            assert_eq!(seconds(&bad).unwrap_err().text, "timeout must be a number of seconds above 0 and up to 86400");
        }
        assert_eq!(late("http://a.test/x", 0.3).text, "net.request to http://a.test/x failed, no answer within 0.3 s");
    }

    // a socket nobody reads closes once its queue is full and its reads then fail
    #[test]
    fn a_full_socket_queue_closes_the_socket() {
        let mut flow = Flow::default();
        for n in 0..QUEUED {
            assert!(flow.deliver(WireValue::Int(n as i128)));
        }
        assert!(!flow.deliver(WireValue::Int(-1)) && flow.flooded && flow.ended);
        assert_eq!(flow.messages.len(), QUEUED);
        let mut flow = Flow::default();
        assert!(flow.deliver(WireValue::Raw(vec![0; QUEUED_BYTES - 1])) && flow.deliver(WireValue::Bytes(b"x".to_vec())));
        assert!(!flow.deliver(WireValue::Bytes(b"y".to_vec())) && flow.flooded);
    }

    #[test]
    fn a_method_is_normalized_as_fetch_does() {
        assert_eq!(method("get").as_deref(), Ok("GET"));
        assert_eq!(method("patch").as_deref(), Ok("patch"));
        assert!(method("TRACE").is_err() && method("connect").is_err() && method("GE T").is_err() && method("").is_err());
    }
}
