//! Per-service log ring buffers (`log` host function, `GET /_k10s/logs/<svc>`).
//!
//! Bounded by line count and bytes; the oldest lines are dropped first. Lines longer than [`MAX_LINE`] are
//! truncated. Optionally echoed to stderr (`--log-stderr`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const MAX_LINE: usize = k10_host::MAX_LOG_LINE;

pub struct LogRing {
    service: String,
    inner: Mutex<Ring>,
    max_lines: usize,
    max_bytes: usize,
    to_stderr: bool,
    pub total: AtomicU64,
    pub dropped: AtomicU64,
}

#[derive(Default)]
struct Ring {
    lines: VecDeque<(u64, String)>,
    bytes: usize,
}

impl LogRing {
    pub fn new(service: &str, max_lines: usize, max_bytes: usize, to_stderr: bool) -> Self {
        LogRing {
            service: service.to_string(),
            inner: Mutex::new(Ring::default()),
            max_lines,
            max_bytes,
            to_stderr,
            total: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn push(&self, bytes: &[u8]) {
        let bytes = &bytes[..bytes.len().min(MAX_LINE)];
        let mut line = String::from_utf8_lossy(bytes).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        let ts = crate::sys::wall_ms();
        if self.to_stderr {
            eprintln!("[{}] {}", self.service, line);
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.bytes += line.len();
        g.lines.push_back((ts, line));
        while g.lines.len() > self.max_lines || g.bytes > self.max_bytes {
            match g.lines.pop_front() {
                Some((_, l)) => {
                    g.bytes -= l.len();
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                None => break,
            }
        }
    }

    /// The buffered lines, oldest first: `(unix_ms, line)`.
    pub fn snapshot(&self) -> Vec<(u64, String)> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).lines.iter().cloned().collect()
    }

    /// Text rendering: one `unix_ms line` per line.
    pub fn render(&self) -> String {
        let mut s = String::new();
        for (ts, l) in self.snapshot() {
            s.push_str(&ts.to_string());
            s.push(' ');
            s.push_str(&l);
            s.push('\n');
        }
        s
    }
}

/// Log rings by service name (they survive redeployments of a service).
pub struct Logs {
    rings: Mutex<HashMap<String, Arc<LogRing>>>,
    max_lines: usize,
    max_bytes: usize,
    to_stderr: bool,
}

impl Logs {
    pub fn new(max_lines: usize, max_bytes: usize, to_stderr: bool) -> Self {
        Logs { rings: Mutex::new(HashMap::new()), max_lines, max_bytes, to_stderr }
    }

    pub fn ring(&self, service: &str) -> Arc<LogRing> {
        let mut g = self.rings.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(service.to_string())
            .or_insert_with(|| Arc::new(LogRing::new(service, self.max_lines, self.max_bytes, self.to_stderr)))
            .clone()
    }

    pub fn get(&self, service: &str) -> Option<Arc<LogRing>> {
        self.rings.lock().unwrap_or_else(|e| e.into_inner()).get(service).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_by_lines_and_bytes() {
        let r = LogRing::new("s", 3, 1000, false);
        for i in 0..10 {
            r.push(format!("line {i}\n").as_bytes());
        }
        let s = r.snapshot();
        assert_eq!(s.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(), ["line 7", "line 8", "line 9"]);
        assert_eq!(r.dropped.load(Ordering::Relaxed), 7);
        let r = LogRing::new("s", 100, 10, false);
        r.push(b"12345");
        r.push(b"67890");
        r.push(b"abc");
        assert_eq!(r.snapshot().len(), 2);
        r.push(&vec![b'x'; 10_000]);
        assert!(r.snapshot().is_empty() || r.snapshot()[0].1.len() <= MAX_LINE);
    }
}
