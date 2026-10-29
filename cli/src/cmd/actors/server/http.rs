use super::MAX_MESSAGE;

// bytes a request line and its headers may take
pub const HEAD_MAX: usize = 16 << 10;
// what a client that sent `Expect: 100-continue` waits for before its body
pub const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/* a control request read whole */
pub struct Request {
    pub post: bool,
    pub path: String,
    pub body: String,
    // the Host the client named and whether a web page sent it
    pub host: Option<String>,
    pub origin: bool,
}

/* where a request stands in the bytes read so far */
pub enum Parse {
    // more to come and true once the head asks for a 100 Continue
    Partial(bool),
    Done(Request),
    Bad(u16, &'static str),
}

/* reads the head up to the blank line and the body by Content-Length */
pub fn parse(buf: &[u8]) -> Parse {
    let Some(end) = buf.windows(4).take(HEAD_MAX).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > HEAD_MAX { Parse::Bad(431, "headers too large") } else { Parse::Partial(false) };
    };
    let Ok(head) = std::str::from_utf8(&buf[..end]) else { return Parse::Bad(400, "bad request") };
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path)) = (start.next(), start.next()) else { return Parse::Bad(400, "bad request") };
    let (mut length, mut expects, mut host, mut origin) = (0, false, None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { return Parse::Bad(400, "bad request") };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("origin") {
            origin = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            let Ok(n) = value.parse() else { return Parse::Bad(400, "bad request") };
            length = n;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Parse::Bad(411, "length required");
        } else if name.eq_ignore_ascii_case("expect") {
            expects = value.eq_ignore_ascii_case("100-continue");
        }
    }
    if length > MAX_MESSAGE {
        return Parse::Bad(413, "body too large");
    }
    let body = end + 4;
    if buf.len() < body + length {
        return Parse::Partial(expects);
    }
    match String::from_utf8(buf[body..body + length].to_vec()) {
        Ok(body) => Parse::Done(Request { post: method == "POST", path: path.to_string(), body, host, origin }),
        Err(_) => Parse::Bad(400, "body is not utf-8"),
    }
}

pub fn json(status: u16, body: &str) -> Vec<u8> {
    reply(status, "application/json", body)
}

pub fn text(status: u16, body: &str) -> Vec<u8> {
    reply(status, "text/plain; charset=utf-8", body)
}

/* a whole reply after which the connection closes */
fn reply(status: u16, kind: &str, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        421 => "Misdirected Request",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    };
    format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
