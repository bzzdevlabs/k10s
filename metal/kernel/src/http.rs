//! Minimal HTTP/1.1 server framing: request head parsing (request line + the headers we act on), Content-Length
//! bodies, keep-alive, `Expect: 100-continue`, and response serialization. No chunked request bodies (411).

use alloc::vec::Vec;
use core::fmt::Write;

pub const MAX_HEAD: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Other,
}

impl Method {
    /// Host API encoding (`req_method`).
    pub fn code(self) -> i64 {
        match self {
            Method::Get => 0,
            Method::Post => 1,
            Method::Put => 2,
            Method::Delete => 3,
            Method::Other => 4,
        }
    }
}

/// A parsed request head.
#[derive(Debug)]
pub struct Head {
    pub method: Method,
    /// Request target as sent (origin-form: path + optional `?query`).
    pub target: Vec<u8>,
    pub content_length: usize,
    pub keep_alive: bool,
    pub expect_continue: bool,
    /// Bytes of the head including the final CRLFCRLF.
    pub len: usize,
    /// Admin targets (`/_k10s…`) only: byte range of the `Authorization` value in the parsed buffer.
    pub auth: Option<(usize, usize)>,
}

pub enum Parse {
    /// Need more bytes.
    Incomplete,
    /// Malformed or unsupported: respond with this status and close.
    Error(u16),
    Head(Head),
}

fn find_crlfcrlf(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn trim(b: &[u8]) -> &[u8] {
    let s = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let e = b.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(s, |p| p + 1);
    &b[s..e.max(s)]
}

/// Parses the request head at the start of `buf`.
pub fn parse_head(buf: &[u8]) -> Parse {
    let Some(len) = find_crlfcrlf(&buf[..buf.len().min(MAX_HEAD + 4)]) else {
        return if buf.len() > MAX_HEAD { Parse::Error(431) } else { Parse::Incomplete };
    };
    let head = &buf[..len - 4];
    let mut lines = head.split(|&c| c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let line = lines.next().unwrap_or(b"");
    let mut parts = line.split(|&c| c == b' ').filter(|p| !p.is_empty());
    let (Some(m), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return Parse::Error(400);
    };
    let method = match m {
        b"GET" => Method::Get,
        b"POST" => Method::Post,
        b"PUT" => Method::Put,
        b"DELETE" => Method::Delete,
        _ => Method::Other,
    };
    let http11 = match version {
        b"HTTP/1.1" => true,
        b"HTTP/1.0" => false,
        _ => return Parse::Error(505),
    };
    let mut h = Head {
        method,
        target: target.to_vec(),
        content_length: 0,
        keep_alive: http11,
        expect_continue: false,
        len,
        auth: None,
    };
    let admin = h.target.starts_with(b"/_k10s");
    for l in lines {
        let Some(colon) = l.iter().position(|&c| c == b':') else { continue };
        let name = trim(&l[..colon]);
        let value = trim(&l[colon + 1..]);
        if name.eq_ignore_ascii_case(b"content-length") {
            let Some(n) = core::str::from_utf8(value).ok().and_then(|s| s.parse::<usize>().ok()) else {
                return Parse::Error(400);
            };
            h.content_length = n;
        } else if name.eq_ignore_ascii_case(b"connection") {
            for tok in value.split(|&c| c == b',') {
                let tok = trim(tok);
                if tok.eq_ignore_ascii_case(b"close") {
                    h.keep_alive = false;
                } else if tok.eq_ignore_ascii_case(b"keep-alive") {
                    h.keep_alive = true;
                }
            }
        } else if admin && name.eq_ignore_ascii_case(b"authorization") {
            let at = value.as_ptr() as usize - buf.as_ptr() as usize;
            h.auth = Some((at, at + value.len()));
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            return Parse::Error(411);
        } else if name.eq_ignore_ascii_case(b"expect") && value.eq_ignore_ascii_case(b"100-continue") {
            h.expect_continue = true;
        }
    }
    Parse::Head(h)
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

struct VecWriter<'a>(&'a mut Vec<u8>);
impl Write for VecWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Appends a response head for a body of `body_len` bytes.
pub fn write_head(out: &mut Vec<u8>, status: u16, content_type: Option<&str>, body_len: usize, keep_alive: bool) {
    let mut w = VecWriter(out);
    let _ = write!(w, "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n", status, reason(status), body_len);
    if let Some(ct) = content_type {
        let _ = write!(w, "Content-Type: {ct}\r\n");
    }
    if !keep_alive {
        let _ = w.write_str("Connection: close\r\n");
    }
    if status == 401 {
        let _ = w.write_str("WWW-Authenticate: Bearer realm=\"k10s\"\r\n");
    }
    let _ = w.write_str("Server: k10-metal\r\n\r\n");
}

pub const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
