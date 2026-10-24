//! Tier 1 OS updates, node side (docs/OS.md, "Updates"). Init passes one end of a socket pair as `--os-agent-fd N`
//! and writes lines about the running image:
//!
//! ```text
//! status PLATFORM IMAGE committed|trial updatable|fixed FAILED|-
//! error TEXT
//! ```
//!
//! The node publishes the report in gossip ([`k10_mesh::OsInfo`]), follows the rollout ([`k10_mesh::rollout`]) of
//! the desired state's OS plan, and answers:
//!
//! ```text
//! update IMAGE   this node's turn: install IMAGE (its blob is DATA/blobs/IMAGE)
//! ready IMAGE    the trial boot of IMAGE rejoined the cluster and serves its placement: commit it
//! ```
//!
//! Images are 64 hex digits. Init verifies every image itself: the node is sandboxed and less trusted.

use super::ClusterShared;
use k10_mesh::rollout::{self, Step};
use k10_mesh::sha256::to_hex;
use k10_mesh::OsInfo;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What init reports about the running image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsStatus {
    pub platform: String,
    pub image: [u8; 32],
    pub committed: bool,
    pub updatable: bool,
    pub failed: Option<[u8; 32]>,
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

impl OsStatus {
    pub fn parse(line: &str) -> Option<OsStatus> {
        let w: Vec<&str> = line.split(' ').collect();
        let [ "status", platform, image, state, upd, failed] = w.as_slice() else { return None };
        if !k10_mesh::manifest::valid_platform(platform) {
            return None;
        }
        Some(OsStatus {
            platform: platform.to_string(),
            image: parse_hex32(image)?,
            committed: match *state {
                "committed" => true,
                "trial" => false,
                _ => return None,
            },
            updatable: match *upd {
                "updatable" => true,
                "fixed" => false,
                _ => return None,
            },
            failed: match *failed {
                "-" => None,
                h => Some(parse_hex32(h)?),
            },
        })
    }

    pub fn line(&self) -> String {
        format!(
            "status {} {} {} {} {}\n",
            self.platform,
            to_hex(&self.image),
            if self.committed { "committed" } else { "trial" },
            if self.updatable { "updatable" } else { "fixed" },
            self.failed.map_or_else(|| "-".to_string(), |f| to_hex(&f))
        )
    }

    /// The gossip form (8-byte image ids).
    pub fn info(&self) -> OsInfo {
        OsInfo {
            platform: self.platform.clone(),
            image: OsInfo::id(&self.image),
            committed: self.committed,
            updatable: self.updatable,
            failed: self.failed.as_ref().map_or([0; 8], OsInfo::id),
        }
    }
}

/// The node's end of the socket pair, line-buffered.
pub struct OsLink {
    stream: UnixStream,
    buf: Vec<u8>,
    /// Lines read past the first status.
    pending: Vec<String>,
}

impl OsLink {
    /// Takes ownership of the inherited descriptor (made close-on-exec).
    pub fn from_fd(fd: RawFd) -> io::Result<OsLink> {
        // SAFETY: fcntl on a descriptor number; failure means it is not open.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: init handed this descriptor to us; nothing else owns it.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        Ok(OsLink { stream, buf: Vec::new(), pending: Vec::new() })
    }

    /// The complete lines received within `timeout` (an error once init closed its end).
    pub fn read_lines(&mut self, timeout: Duration) -> io::Result<Vec<String>> {
        if !self.pending.is_empty() {
            return Ok(std::mem::take(&mut self.pending));
        }
        self.stream.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "init closed the update channel")),
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
            Err(e) => return Err(e),
        }
        if self.buf.len() > 1 << 16 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "update channel line too long"));
        }
        let mut lines = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let l: Vec<u8> = self.buf.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&l[..i]).into_owned());
        }
        Ok(lines)
    }

    /// Waits up to `timeout` for the first status line (init writes it right after starting the node).
    pub fn first_status(&mut self, timeout: Duration) -> io::Result<OsStatus> {
        let t = Instant::now();
        while t.elapsed() < timeout {
            let mut lines = self.read_lines(timeout.saturating_sub(t.elapsed()))?.into_iter();
            while let Some(l) = lines.next() {
                if let Some(s) = OsStatus::parse(&l) {
                    self.pending = lines.collect();
                    return Ok(s);
                }
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "no OS status from init"))
    }

    fn send(&mut self, line: &str) -> io::Result<()> {
        self.stream.write_all(line.as_bytes())
    }
}

/// The trial boot may be committed: the node holds and applied the cluster's desired state and, if it had peers,
/// sees one again.
fn commit_ready(cs: &ClusterShared) -> bool {
    let Some(cur) = cs.store.current() else { return false };
    let applied = {
        let av = cs.apply_view.lock().unwrap_or_else(|e| e.into_inner());
        av.applied == Some(cur.key()) && av.last_error.is_none()
    };
    let mv = cs.mesh_view.lock().unwrap_or_else(|e| e.into_inner());
    let current = mv.desired.is_some_and(|d| (d.version, d.hash) == cur.key());
    let peers = !cs.expects_peers() || mv.members.iter().any(|m| m.id != cs.id && m.is_live());
    applied && current && peers && !cs.leaving()
}

fn describe(step: &Step) -> String {
    let short = |h: &[u8; 32]| to_hex(&h[..4]);
    match step {
        Step::Idle => "idle".into(),
        Step::Update { image, wave } => format!("installing {} (wave {wave})", short(image)),
        Step::Wait { image, wave, pending } => format!("{} waits for {pending} node(s) of earlier waves (wave {wave})", short(image)),
        Step::Halted { image, node } => format!("rollout of {} stopped: node {node} failed to commit it", short(image)),
        Step::Unknown { image, members } => format!("{} waits for the OS report of {members} member(s)", short(image)),
    }
}

/// A started node that had peers follows the rollout only after this long, so that it judges its turn on the
/// cluster's membership (learned by gossip sync within seconds) rather than on its own record alone.
const SETTLE: Duration = Duration::from_secs(15);

/// Runs the agent: `status` is the first report (already in this node's gossip record).
pub fn spawn(mut link: OsLink, mut status: OsStatus, cs: Option<Arc<ClusterShared>>, grace: Duration) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("k10s-os".into()).spawn(move || {
        let mut good_since: Option<Instant> = None;
        let mut asked_ready = false;
        let mut asked_update: Option<[u8; 32]> = None;
        let mut last = String::new();
        let started = Instant::now();
        loop {
            let lines = match link.read_lines(Duration::from_secs(1)) {
                Ok(l) => l,
                Err(e) => return eprintln!("k10s: os: {e}; updates are off"),
            };
            for l in lines {
                if let Some(s) = OsStatus::parse(&l) {
                    if s.image != status.image || s.committed != status.committed {
                        asked_ready = false;
                        good_since = None;
                    }
                    status = s;
                    if let Some(cs) = &cs {
                        cs.inbox.send(super::router::MeshMsg::SetOs(Some(status.info())));
                    }
                } else if let Some(e) = l.strip_prefix("error ") {
                    eprintln!("k10s: os: init: {e}");
                }
            }
            if !status.committed {
                if cs.as_deref().map_or(true, commit_ready) {
                    let since = *good_since.get_or_insert_with(Instant::now);
                    if !asked_ready && since.elapsed() >= grace {
                        eprintln!("k10s: os: trial image {} serves; asking init to commit it", to_hex(&status.image[..4]));
                        if link.send(&format!("ready {}\n", to_hex(&status.image))).is_err() {
                            return;
                        }
                        asked_ready = true;
                    }
                } else {
                    good_since = None;
                }
                continue;
            }
            let Some(cs) = &cs else { continue };
            if !status.updatable || cs.leaving() {
                continue;
            }
            if cs.expects_peers()
                && (started.elapsed() < SETTLE
                    || !cs.mesh_view.lock().unwrap_or_else(|e| e.into_inner()).members.iter().any(|m| m.id != cs.id && m.is_live()))
            {
                continue;
            }
            let plan = cs.store.current().map(|c| c.manifest.os.clone()).unwrap_or_default();
            let members = cs.mesh_view.lock().unwrap_or_else(|e| e.into_inner()).members.clone();
            let step = rollout::step(&plan, &members, cs.id);
            let text = describe(&step);
            if text != last {
                if step != Step::Idle || !last.is_empty() {
                    eprintln!("k10s: os: {text}");
                }
                *cs.os_step.lock().unwrap_or_else(|e| e.into_inner()) = text.clone();
                last = text;
            }
            if let Step::Update { image, .. } = step {
                if asked_update != Some(image) && cs.store.blob_on_disk(&image) {
                    if link.send(&format!("update {}\n", to_hex(&image))).is_err() {
                        return;
                    }
                    asked_update = Some(image);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        let s = OsStatus { platform: "x86_64-vm".into(), image: [0xab; 32], committed: false, updatable: true, failed: Some([1; 32]) };
        let line = s.line();
        assert_eq!(OsStatus::parse(line.trim_end()), Some(s.clone()));
        let none = OsStatus { failed: None, committed: true, updatable: false, ..s.clone() };
        assert_eq!(OsStatus::parse(none.line().trim_end()), Some(none.clone()));
        assert_eq!(none.info().failed, [0; 8]);
        for bad in ["status x86_64-vm ab trial updatable -", "status X committed", "update 00", "status x86_64-vm "] {
            assert!(OsStatus::parse(bad).is_none(), "{bad}");
        }
        let (a, b) = UnixStream::pair().unwrap();
        let mut link = OsLink { stream: a, buf: Vec::new(), pending: Vec::new() };
        (&b).write_all(line.as_bytes()).unwrap();
        (&b).write_all(b"error x\nstat").unwrap();
        assert_eq!(link.first_status(Duration::from_secs(1)).unwrap(), s);
        assert_eq!(link.read_lines(Duration::from_millis(10)).unwrap(), vec!["error x".to_string()]);
        drop(b);
        assert!(link.read_lines(Duration::from_millis(10)).is_err());
    }
}
