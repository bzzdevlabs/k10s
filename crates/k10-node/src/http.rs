//! Minimal, strict HTTP/1.1 request parsing and response encoding for the ingress, plus a tiny blocking client
//! (CLI and tests).
//!
//! Supported: `Content-Length` bodies, keep-alive (HTTP/1.1 default, HTTP/1.0 with `Connection: keep-alive`),
//! pipelining (requests are answered in order, one at a time), `Expect: 100-continue`.
//! Rejected: `Transfer-Encoding: chunked` (411), other transfer codings (501), heads over the limit (431), bodies over
//! the limit (413), conflicting `Content-Length`s, whitespace before `:`, obsolete line folding and malformed request
//! lines (400), versions other than 1.0/1.1 (505), unknown expectations (417).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_head: usize,
    pub max_headers: usize,
    pub max_body: usize,
    /// Body limit for `POST /_k10s/apply` (bundles carry code blobs).
    pub max_admin_body: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_head: 16 << 10, max_headers: 100, max_body: 16 << 20, max_admin_body: 256 << 20 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Other,
}

impl Method {
    /// Code returned by the `req_method` host function.
    pub fn code(self) -> i64 {
        match self {
            Method::Get => 0,
            Method::Post => 1,
            Method::Put => 2,
            Method::Delete => 3,
            Method::Head | Method::Other => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestHead {
    pub method: Method,
    /// Origin-form request target (`/path?query`).
    pub target: String,
    /// 0 for HTTP/1.0, 1 for HTTP/1.1.
    pub minor: u8,
    pub content_length: usize,
    pub keep_alive: bool,
    pub expect_continue: bool,
    /// Bytes of the head including the final empty line.
    pub head_len: usize,
    /// Admin targets (`/_k10s…`) only: byte range of the `Authorization` value in the parsed buffer.
    pub auth: Option<(usize, usize)>,
    /// Admin targets only: a `Forwarded` / `X-Forwarded-For` header is present (a proxy relays the client).
    pub forwarded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parse {
    /// More bytes are needed.
    Incomplete,
    Complete(RequestHead),
    /// Answer with this status and close the connection.
    Error(u16, &'static str),
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    // Accept CRLF CRLF, and bare LF LF (RFC 9112 §2.2 lets recipients accept LF line endings).
    let mut i = 0;
    while i < buf.len() {
        let p = i + memchr(b'\n', &buf[i..])?;
        if p + 1 < buf.len() && buf[p + 1] == b'\n' {
            return Some(p + 2);
        }
        if p + 2 < buf.len() && buf[p + 1] == b'\r' && buf[p + 2] == b'\n' {
            return Some(p + 3);
        }
        i = p + 1;
    }
    None
}

#[inline]
fn memchr(c: u8, s: &[u8]) -> Option<usize> {
    s.iter().position(|&b| b == c)
}

fn is_tchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

fn trim_ows(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t') {
        a += 1;
    }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t') {
        b -= 1;
    }
    &s[a..b]
}

/// Parses a request head at the start of `buf`. `body_limit(target)` gives the body limit for a target.
pub fn parse_head(buf: &[u8], lim: &Limits) -> Parse {
    let end = match find_head_end(buf) {
        Some(e) => e,
        None => {
            if buf.len() > lim.max_head {
                return Parse::Error(431, "request head too large");
            }
            // Ignore leading empty lines (RFC 9112 §2.2) only once a full head is present.
            return Parse::Incomplete;
        }
    };
    if end > lim.max_head {
        return Parse::Error(431, "request head too large");
    }
    let head = &buf[..end];
    let mut lines = head.split(|&c| c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    // request line (skip at most a few leading empty lines)
    let mut rl = lines.next().unwrap_or(b"");
    let mut skipped = 0;
    while rl.is_empty() && skipped < 4 {
        rl = lines.next().unwrap_or(b"");
        skipped += 1;
    }
    let mut parts = rl.split(|&c| c == b' ');
    let (m, t, v) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) if !m.is_empty() && !t.is_empty() => (m, t, v),
        _ => return Parse::Error(400, "malformed request line"),
    };
    if !m.iter().all(|&c| is_tchar(c)) {
        return Parse::Error(400, "malformed method");
    }
    let method = match m {
        b"GET" => Method::Get,
        b"POST" => Method::Post,
        b"PUT" => Method::Put,
        b"DELETE" => Method::Delete,
        b"HEAD" => Method::Head,
        _ => Method::Other,
    };
    let minor = match v {
        b"HTTP/1.1" => 1,
        b"HTTP/1.0" => 0,
        _ if v.starts_with(b"HTTP/") => return Parse::Error(505, "HTTP version not supported"),
        _ => return Parse::Error(400, "malformed request line"),
    };
    if t.iter().any(|&c| c <= b' ' || c == 0x7F) {
        return Parse::Error(400, "malformed request target");
    }
    let target: &[u8] = if t[0] == b'/' {
        t
    } else if t.len() > 7 && (t[..7].eq_ignore_ascii_case(b"http://") || t.starts_with(b"https://")) {
        // absolute-form: drop scheme and authority
        let rest = &t[if t[4] == b's' { 8 } else { 7 }..];
        match memchr(b'/', rest) {
            Some(p) => &rest[p..],
            None => b"/",
        }
    } else {
        return Parse::Error(400, "unsupported request target form");
    };
    let target = match std::str::from_utf8(target) {
        Ok(s) => s.to_string(),
        Err(_) => return Parse::Error(400, "request target is not UTF-8"),
    };

    let mut content_length: Option<usize> = None;
    let mut keep_alive = minor == 1;
    let mut expect_continue = false;
    let mut te = false;
    let mut te_chunked = false;
    let mut n_headers = 0;
    // Credentials only matter for the admin API (no extra work on service traffic).
    let admin = target.starts_with("/_k10s");
    let mut auth = None;
    let mut forwarded = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        n_headers += 1;
        if n_headers > lim.max_headers {
            return Parse::Error(431, "too many headers");
        }
        if line[0] == b' ' || line[0] == b'\t' {
            return Parse::Error(400, "obsolete header line folding");
        }
        let Some(colon) = memchr(b':', line) else { return Parse::Error(400, "malformed header") };
        let name = &line[..colon];
        if name.is_empty() || !name.iter().all(|&c| is_tchar(c)) {
            return Parse::Error(400, "malformed header name");
        }
        let value = trim_ows(&line[colon + 1..]);
        if name.eq_ignore_ascii_case(b"content-length") {
            // RFC 9110 §8.6: a list of identical values may be accepted; anything else is an error.
            for part in value.split(|&c| c == b',') {
                let part = trim_ows(part);
                if part.is_empty() || part.len() > 19 || !part.iter().all(|c| c.is_ascii_digit()) {
                    return Parse::Error(400, "invalid content-length");
                }
                let n: usize = std::str::from_utf8(part).unwrap().parse().unwrap_or(usize::MAX);
                if content_length.is_some_and(|c| c != n) {
                    return Parse::Error(400, "conflicting content-length");
                }
                content_length = Some(n);
            }
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            te = true;
            if value.split(|&c| c == b',').any(|p| trim_ows(p).eq_ignore_ascii_case(b"chunked")) {
                te_chunked = true;
            }
        } else if name.eq_ignore_ascii_case(b"connection") {
            for tok in value.split(|&c| c == b',') {
                let tok = trim_ows(tok);
                if tok.eq_ignore_ascii_case(b"close") {
                    keep_alive = false;
                } else if tok.eq_ignore_ascii_case(b"keep-alive") {
                    keep_alive = true;
                }
            }
        } else if admin && name.eq_ignore_ascii_case(b"authorization") {
            let at = value.as_ptr() as usize - buf.as_ptr() as usize;
            auth = Some((at, at + value.len()));
        } else if admin && (name.eq_ignore_ascii_case(b"forwarded") || name.eq_ignore_ascii_case(b"x-forwarded-for")) {
            forwarded = true;
        } else if name.eq_ignore_ascii_case(b"expect") {
            if value.eq_ignore_ascii_case(b"100-continue") {
                expect_continue = minor == 1;
            } else {
                return Parse::Error(417, "unsupported expectation");
            }
        }
    }
    if te {
        return if te_chunked {
            Parse::Error(411, "chunked request bodies are not supported; send Content-Length")
        } else {
            Parse::Error(501, "transfer coding not implemented")
        };
    }
    let content_length = content_length.unwrap_or(0);
    // Only `/_k10s/apply` carries a large body (a bundle with code blobs); every other endpoint (status, metrics,
    // logs, health) and every service gets the ordinary body limit, so a big body cannot be aimed at them.
    let is_apply = target.split('?').next() == Some("/_k10s/apply");
    let limit = if is_apply { lim.max_admin_body } else { lim.max_body };
    if content_length > limit {
        return Parse::Error(413, "request body too large");
    }
    Parse::Complete(RequestHead { method, target, minor, content_length, keep_alive, expect_continue, head_len: end, auth, forwarded })
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        417 => "Expectation Failed",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// Appends a response head. `body_len` is the Content-Length.
pub fn write_head(out: &mut Vec<u8>, status: u16, body_len: usize, keep_alive: bool, content_type: Option<&str>) {
    let status = if (100..=999).contains(&status) { status } else { 500 };
    out.extend_from_slice(b"HTTP/1.1 ");
    push_u64(out, status as u64);
    out.push(b' ');
    out.extend_from_slice(reason(status).as_bytes());
    out.extend_from_slice(b"\r\ncontent-length: ");
    push_u64(out, body_len as u64);
    if let Some(ct) = content_type {
        out.extend_from_slice(b"\r\ncontent-type: ");
        out.extend_from_slice(ct.as_bytes());
    }
    if !keep_alive {
        out.extend_from_slice(b"\r\nconnection: close");
    }
    if status == 401 {
        out.extend_from_slice(b"\r\nwww-authenticate: Bearer realm=\"k10s\"");
    }
    out.extend_from_slice(b"\r\n\r\n");
}

pub fn push_u64(out: &mut Vec<u8>, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&tmp[i..]);
}

// ------------------------------------------------------------------------------------------------------------------
// Blocking client (CLI, tests)
// ------------------------------------------------------------------------------------------------------------------

/// A response read by [`Client`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub close: bool,
}

impl ClientResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A keep-alive HTTP/1.1 client connection.
pub struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    host: String,
}

impl Client {
    pub fn connect(addr: &str, timeout: Duration) -> io::Result<Client> {
        let sa: SocketAddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cannot resolve address"))?;
        let stream = TcpStream::connect_timeout(&sa, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Ok(Client { stream, buf: Vec::new(), host: addr.to_string() })
    }

    pub fn set_timeout(&self, t: Duration) {
        let _ = self.stream.set_read_timeout(Some(t));
        let _ = self.stream.set_write_timeout(Some(t));
    }

    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// Sends one request and reads its response.
    pub fn request(&mut self, method: &str, path: &str, body: &[u8]) -> io::Result<ClientResponse> {
        self.request_with(method, path, body, None)
    }

    /// [`Client::request`] with an optional admin bearer token.
    pub fn request_with(&mut self, method: &str, path: &str, body: &[u8], token: Option<&str>) -> io::Result<ClientResponse> {
        let mut req = Vec::with_capacity(128 + body.len());
        write!(req, "{method} {path} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\n", self.host, body.len())?;
        if let Some(t) = token {
            write!(req, "authorization: Bearer {t}\r\n")?;
        }
        req.extend_from_slice(b"\r\n");
        req.extend_from_slice(body);
        self.stream.write_all(&req)?;
        self.read_response()
    }

    /// Reads one response (for pipelining tests: write several requests with `stream()`, then call this).
    pub fn read_response(&mut self) -> io::Result<ClientResponse> {
        let mut tmp = [0u8; 16384];
        loop {
            if let Some(end) = find_head_end(&self.buf) {
                let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
                let mut lines = head.lines();
                let sl = lines.next().unwrap_or("");
                let status: u16 = sl
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad status line {sl:?}")))?;
                let mut cl = 0usize;
                let mut close = false;
                for l in lines {
                    if let Some((k, v)) = l.split_once(':') {
                        let v = v.trim();
                        if k.eq_ignore_ascii_case("content-length") {
                            cl = v.parse().unwrap_or(0);
                        } else if k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close") {
                            close = true;
                        }
                    }
                }
                if status == 100 {
                    self.buf.drain(..end);
                    continue;
                }
                while self.buf.len() < end + cl {
                    let n = self.stream.read(&mut tmp)?;
                    if n == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-body"));
                    }
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                let body = self.buf[end..end + cl].to_vec();
                self.buf.drain(..end + cl);
                return Ok(ClientResponse { status, body, close });
            }
            let n = self.stream.read(&mut tmp)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
}

/// One-shot request on a new connection.
pub fn request(addr: &str, method: &str, path: &str, body: &[u8], timeout: Duration) -> io::Result<ClientResponse> {
    Client::connect(addr, timeout)?.request(method, path, body)
}

/// One-shot request with an optional admin bearer token.
pub fn request_auth(addr: &str, method: &str, path: &str, body: &[u8], timeout: Duration, token: Option<&str>) -> io::Result<ClientResponse> {
    Client::connect(addr, timeout)?.request_with(method, path, body, token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[u8]) -> Parse {
        parse_head(s, &Limits::default())
    }

    fn ok(s: &[u8]) -> RequestHead {
        match p(s) {
            Parse::Complete(h) => h,
            other => panic!("expected complete, got {other:?} for {:?}", String::from_utf8_lossy(s)),
        }
    }

    #[test]
    fn simple_get() {
        let h = ok(b"GET /echo/x?y=1 HTTP/1.1\r\nHost: a\r\n\r\n");
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.target, "/echo/x?y=1");
        assert!(h.keep_alive);
        assert_eq!(h.content_length, 0);
        assert_eq!(h.head_len, 37);
    }

    #[test]
    fn post_with_body_and_connection_close() {
        let h = ok(b"POST /a HTTP/1.1\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
        assert_eq!(h.method, Method::Post);
        assert_eq!(h.content_length, 5);
        assert!(!h.keep_alive);
    }

    #[test]
    fn http10_keepalive_rules() {
        assert!(!ok(b"GET / HTTP/1.0\r\n\r\n").keep_alive);
        assert!(ok(b"GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n").keep_alive);
        assert!(!ok(b"GET / HTTP/1.0\r\nExpect: 100-continue\r\n\r\n").expect_continue);
    }

    #[test]
    fn partial_input() {
        let full = b"POST /svc/p HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc";
        let head_end = full.len() - 3;
        for cut in 0..head_end {
            assert_eq!(p(&full[..cut]), Parse::Incomplete, "cut {cut}");
        }
        for cut in head_end..=full.len() {
            assert!(matches!(p(&full[..cut]), Parse::Complete(_)));
        }
    }

    #[test]
    fn pipelined() {
        let two = b"GET /a HTTP/1.1\r\n\r\nGET /b HTTP/1.1\r\n\r\n";
        let h = ok(two);
        assert_eq!(h.target, "/a");
        let h2 = ok(&two[h.head_len..]);
        assert_eq!(h2.target, "/b");
    }

    #[test]
    fn lf_only_and_absolute_form() {
        let h = ok(b"GET http://example.com/x/y?q HTTP/1.1\nHost: e\n\n");
        assert_eq!(h.target, "/x/y?q");
        assert_eq!(ok(b"GET http://example.com HTTP/1.1\r\n\r\n").target, "/");
    }

    #[test]
    fn rejects() {
        let cases: &[(&[u8], u16)] = &[
            (b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", 411),
            (b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n", 411),
            (b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n", 501),
            (b"GET / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nContent-Length: -1\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nContent-Length: 1x\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nContent-Length: 16777217\r\n\r\n", 413),
            (b"GET / HTTP/1.1\r\nHost : x\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nX: a\r\n  folded\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nnocolon\r\n\r\n", 400),
            (b"GET /\r\n\r\n", 400),
            (b"GET  / HTTP/1.1\r\n\r\n", 400),
            (b"GET / HTTP/2.0\r\n\r\n", 505),
            (b"GET / FTP/1.0\r\n\r\n", 400),
            (b"G(T / HTTP/1.1\r\n\r\n", 400),
            (b"GET foo HTTP/1.1\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nExpect: something\r\n\r\n", 417),
            (b"\r\n\r\n", 400),
        ];
        for (req, st) in cases {
            match p(req) {
                Parse::Error(s, _) => assert_eq!(s, *st, "{:?}", String::from_utf8_lossy(req)),
                other => panic!("expected {st}, got {other:?} for {:?}", String::from_utf8_lossy(req)),
            }
        }
        // identical duplicate content-length is fine
        assert_eq!(ok(b"POST / HTTP/1.1\r\nContent-Length: 3, 3\r\n\r\n").content_length, 3);
        // admin apply gets a larger limit
        assert_eq!(ok(b"POST /_k10s/apply HTTP/1.1\r\nContent-Length: 33554432\r\n\r\n").content_length, 32 << 20);
    }

    #[test]
    fn admin_credentials_are_captured_for_admin_targets_only() {
        let req = b"GET /_k10s/status HTTP/1.1\r\nAuthorization:  Bearer abc \r\nX-Forwarded-For: 1.2.3.4\r\n\r\n";
        let h = ok(req);
        let (a, b) = h.auth.unwrap();
        assert_eq!(&req[a..b], b"Bearer abc");
        assert!(h.forwarded);
        let h = ok(b"GET /echo HTTP/1.1\r\nAuthorization: Bearer abc\r\nForwarded: for=x\r\n\r\n");
        assert_eq!((h.auth, h.forwarded), (None, false));
    }

    #[test]
    fn head_limits() {
        let mut big = b"GET / HTTP/1.1\r\nX: ".to_vec();
        big.extend(std::iter::repeat_n(b'a', 20000));
        assert_eq!(p(&big), Parse::Error(431, "request head too large"));
        big.extend_from_slice(b"\r\n\r\n");
        assert_eq!(p(&big), Parse::Error(431, "request head too large"));
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..150 {
            many.extend_from_slice(format!("h{i}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(p(&many), Parse::Error(431, "too many headers"));
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x1234_5678_9abc_def0u64;
        let base = b"POST /svc/a?b HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\nConnection: keep-alive\r\n\r\nbody";
        for _ in 0..50_000 {
            let mut m = base.to_vec();
            for _ in 0..4 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % m.len();
                m[i] = (x >> 40) as u8;
            }
            let _ = p(&m);
        }
    }

    #[test]
    fn response_head() {
        let mut v = Vec::new();
        write_head(&mut v, 200, 5, true, None);
        assert_eq!(v, b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\n");
        v.clear();
        write_head(&mut v, 503, 0, false, Some("text/plain"));
        assert_eq!(
            String::from_utf8(v).unwrap(),
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n"
        );
    }
}
