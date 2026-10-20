//! Real-socket integration (std only).
//!
//! * [`UdpDriver`]: a non-blocking `UdpSocket` + [`Gossip`], driven by the caller's own event loop
//!   (`raw_fd` → epoll; `on_readable` when readable; `on_tick` at `next_deadline`); optionally sealed ([`UdpDriver::set_cipher`]).
//! * [`MeshThread`]: a self-contained thread running a `UdpDriver` (tests, examples, tools).

use crate::gossip::{Config, DesiredState, Event, Gossip, Output};
use crate::member::{Addr, Member};
use crate::secure::{GossipCipher, GossipCipherStats, Reject};
use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::net::UdpSocket;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Max datagrams handled per `on_readable` call (keeps one busy socket from starving the caller's loop; with a
/// level-triggered poller the caller is simply woken again).
const READ_BUDGET: usize = 1024;

/// Counters of the driver's own I/O.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub send_errors: u64,
    /// Datagrams dropped by the [`GossipCipher`] (not authentic, stale or replayed).
    pub rejected: u64,
}

/// A UDP socket plus the gossip state machine.
pub struct UdpDriver {
    sock: UdpSocket,
    gossip: Gossip,
    events: VecDeque<Event>,
    rx: Vec<u8>,
    stats: IoStats,
    cipher: Option<GossipCipher>,
    sealed: Vec<u8>,
    last_reject: Option<(Addr, Reject)>,
}

/// Unix time in milliseconds (the gossip cipher's timestamps).
fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl UdpDriver {
    /// Binds `local.gossip` (which is also the address advertised to peers, so it must be reachable: not
    /// `0.0.0.0`). Port 0 picks a free port and updates the advertised address. To bind a wildcard address
    /// while advertising another one, bind the socket yourself and use [`UdpDriver::with_socket`].
    pub fn bind(local: Member, config: Config, seed: u64) -> io::Result<UdpDriver> {
        let sock = UdpSocket::bind(local.gossip)?;
        let mut local = local;
        if local.gossip.port() == 0 {
            local.gossip.set_port(sock.local_addr()?.port());
        }
        Self::with_socket(sock, Gossip::new(local, config, seed))
    }

    /// Wraps an already bound socket (switched to non-blocking mode).
    pub fn with_socket(sock: UdpSocket, gossip: Gossip) -> io::Result<UdpDriver> {
        sock.set_nonblocking(true)?;
        Ok(UdpDriver {
            sock,
            gossip,
            events: VecDeque::new(),
            rx: vec![0u8; 65_536],
            stats: IoStats::default(),
            cipher: None,
            sealed: Vec::new(),
            last_reject: None,
        })
    }

    /// Seals every datagram from now on and drops received ones the cipher rejects.
    pub fn set_cipher(&mut self, cipher: GossipCipher) {
        self.cipher = Some(cipher);
    }

    pub fn cipher_stats(&self) -> Option<GossipCipherStats> {
        self.cipher.as_ref().map(|c| c.stats())
    }

    /// Source and reason of the last rejected datagram since the previous call.
    pub fn take_last_reject(&mut self) -> Option<(Addr, Reject)> {
        self.last_reject.take()
    }

    /// The socket's file descriptor, to register (read interest) with the caller's epoll/kqueue.
    #[cfg(unix)]
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.sock.as_raw_fd()
    }

    /// Drains the socket (up to an internal budget), feeding every datagram to gossip and sending replies.
    /// Returns the number of datagrams processed.
    pub fn on_readable(&mut self, now_ms: u64) -> io::Result<usize> {
        let mut n = 0;
        while n < READ_BUDGET {
            match self.sock.recv_from(&mut self.rx) {
                Ok((len, from)) => {
                    n += 1;
                    self.stats.packets_received += 1;
                    self.stats.bytes_received += len as u64;
                    let pkt = match &mut self.cipher {
                        Some(c) => match c.open(&mut self.rx[..len], wall_ms()) {
                            Ok(p) => p,
                            Err(r) => {
                                self.stats.rejected += 1;
                                self.last_reject = Some((from, r));
                                continue;
                            }
                        },
                        None => &self.rx[..len],
                    };
                    let out = self.gossip.on_packet(from, pkt, now_ms);
                    self.flush(out);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // ICMP errors from earlier sends surface on recv on Linux; they are not fatal for UDP.
                Err(e)
                    if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    /// Runs gossip timers; call when `now_ms >= next_deadline()` (extra calls are harmless).
    pub fn on_tick(&mut self, now_ms: u64) {
        let out = self.gossip.tick(now_ms);
        self.flush(out);
    }

    /// Next time `on_tick` has work (absolute, same clock as `now_ms`).
    pub fn next_deadline(&self) -> u64 {
        self.gossip.next_deadline()
    }

    pub fn join(&mut self, seeds: &[Addr], now_ms: u64) {
        let out = self.gossip.join(seeds, now_ms);
        self.flush(out);
    }

    pub fn leave(&mut self, now_ms: u64) {
        let out = self.gossip.leave(now_ms);
        self.flush(out);
    }

    pub fn set_desired(&mut self, version: u64, hash: [u8; 32]) {
        let out = self.gossip.set_desired(version, hash);
        self.flush(out);
    }

    /// Next pending membership / desired-state event.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn gossip(&self) -> &Gossip {
        &self.gossip
    }

    pub fn io_stats(&self) -> IoStats {
        self.stats
    }

    pub fn local_addr(&self) -> io::Result<Addr> {
        self.sock.local_addr()
    }

    /// Blocks until a datagram is readable or `timeout` elapses (for loops without their own poller).
    pub fn wait_readable(&self, timeout: Duration) -> io::Result<()> {
        self.sock.set_nonblocking(false)?;
        self.sock.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut b = [0u8; 1];
        let _ = self.sock.peek_from(&mut b);
        self.sock.set_nonblocking(true)
    }

    fn flush(&mut self, out: Output) {
        for (to, pkt) in out.packets {
            let bytes: &[u8] = match &mut self.cipher {
                Some(c) => {
                    c.seal(&pkt, wall_ms(), &mut self.sealed);
                    &self.sealed
                }
                None => &pkt,
            };
            match self.sock.send_to(bytes, to) {
                Ok(_) => {
                    self.stats.packets_sent += 1;
                    self.stats.bytes_sent += bytes.len() as u64;
                }
                Err(_) => self.stats.send_errors += 1, // UDP is best effort; gossip tolerates loss
            }
        }
        self.events.extend(out.events);
    }
}

/// State published by a [`MeshThread`] for other threads to read.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub local: Member,
    /// All known members (including self and retained dead ones).
    pub members: Vec<Member>,
    pub desired: Option<DesiredState>,
}

enum Cmd {
    Join(Vec<Addr>),
    SetDesired(u64, [u8; 32]),
    Leave,
    Stop,
}

/// A gossip node on its own thread (real UDP socket, monotonic clock).
pub struct MeshThread {
    cmd: mpsc::Sender<Cmd>,
    events: mpsc::Receiver<Event>,
    shared: Arc<Mutex<Snapshot>>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl MeshThread {
    /// Binds (see [`UdpDriver::bind`]), joins `seeds` and runs until `leave`, `kill` or drop.
    pub fn spawn(local: Member, config: Config, seed: u64, seeds: Vec<Addr>) -> io::Result<MeshThread> {
        let mut d = UdpDriver::bind(local, config, seed)?;
        let start = Instant::now();
        let now = move || start.elapsed().as_millis() as u64;
        if !seeds.is_empty() {
            d.join(&seeds, now());
        }
        let snapshot = |d: &UdpDriver| Snapshot {
            local: d.gossip().local().clone(),
            members: d.gossip().members().cloned().collect(),
            desired: d.gossip().desired(),
        };
        let shared = Arc::new(Mutex::new(snapshot(&d)));
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let sh = shared.clone();
        let handle = std::thread::Builder::new().name("k10-mesh".into()).spawn(move || -> io::Result<()> {
            let mut last_publish = 0u64;
            loop {
                let mut dirty = false;
                loop {
                    match cmd_rx.try_recv() {
                        Ok(Cmd::Join(s)) => d.join(&s, now()),
                        Ok(Cmd::SetDesired(v, h)) => d.set_desired(v, h),
                        Ok(Cmd::Leave) => {
                            d.leave(now());
                            *sh.lock().unwrap() = snapshot(&d);
                            return Ok(());
                        }
                        Ok(Cmd::Stop) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                        Err(mpsc::TryRecvError::Empty) => break,
                    }
                    dirty = true;
                }
                d.on_tick(now());
                d.on_readable(now())?;
                while let Some(e) = d.poll_event() {
                    dirty = true;
                    let _ = ev_tx.send(e);
                }
                let t = now();
                if dirty || t >= last_publish + 200 {
                    *sh.lock().unwrap() = snapshot(&d);
                    last_publish = t;
                }
                let wait = d.next_deadline().saturating_sub(now()).clamp(1, 20);
                d.wait_readable(Duration::from_millis(wait))?;
            }
        })?;
        Ok(MeshThread { cmd: cmd_tx, events: ev_rx, shared, handle: Some(handle) })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().unwrap().clone()
    }

    pub fn local(&self) -> Member {
        self.snapshot().local
    }

    /// Alive + Suspect members including self (placement input).
    pub fn live_members(&self) -> Vec<Member> {
        self.snapshot().members.into_iter().filter(|m| m.is_live()).collect()
    }

    pub fn desired(&self) -> Option<DesiredState> {
        self.snapshot().desired
    }

    pub fn join(&self, seeds: Vec<Addr>) {
        let _ = self.cmd.send(Cmd::Join(seeds));
    }

    pub fn set_desired(&self, version: u64, hash: [u8; 32]) {
        let _ = self.cmd.send(Cmd::SetDesired(version, hash));
    }

    /// Membership and desired-state events, in order.
    pub fn events(&self) -> &mpsc::Receiver<Event> {
        &self.events
    }

    /// Graceful leave, then stop.
    pub fn leave(mut self) -> io::Result<()> {
        let _ = self.cmd.send(Cmd::Leave);
        self.join_thread()
    }

    /// Stop abruptly without telling anyone (simulates a crash).
    pub fn kill(mut self) -> io::Result<()> {
        let _ = self.cmd.send(Cmd::Stop);
        self.join_thread()
    }

    fn join_thread(&mut self) -> io::Result<()> {
        match self.handle.take() {
            Some(h) => h.join().unwrap_or_else(|_| Err(io::Error::other("mesh thread panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for MeshThread {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
        let _ = self.join_thread();
    }
}
