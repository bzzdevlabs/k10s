//! TCP/IP (smoltcp over the virtio-net driver) and the HTTP/1.1 ingress connections on port 80.
//!
//! * Static configuration matching QEMU user networking (slirp): 10.0.2.15/24, gateway 10.0.2.2.
//! * smoltcp needs one socket in LISTEN state per connection it can accept. Instead of a large idle pool, the
//!   kernel processes ingress one packet at a time (`poll_ingress_single`) and tops the listening sockets back
//!   up after every packet, so a burst of SYNs never meets an empty backlog.
//! * Each accepted socket becomes a `Conn`: bytes are accumulated until a full request (head + Content-Length
//!   body) is present, the request goes to the runtime, and its response is written back. One request per
//!   connection is in flight at a time (pipelined requests wait in the input buffer).

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address};

use crate::http::{self, Method, Parse};
use crate::sync::Global;
use crate::time;
use crate::virtio_net::VirtioNet;

pub const HTTP_PORT: u16 = 80;
pub const IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
pub const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);

pub static NET_IRQS: AtomicU64 = AtomicU64::new(0);

/// Copy of the network counters, refreshed by the main loop (read by `/_k10s/status`, which runs while `Net`
/// itself is borrowed).
pub static SNAPSHOT: Global<Snapshot> = Global::new(Snapshot::ZERO);

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub kicks: u64,
    pub tx_full: u64,
    pub accepted: u64,
    pub closed: u64,
    pub open_conns: u64,
    pub http_requests: u64,
    pub bad_requests: u64,
    pub nic_ram: u64,
    pub msix: bool,
}

impl Snapshot {
    const ZERO: Snapshot = Snapshot {
        rx_packets: 0,
        tx_packets: 0,
        rx_bytes: 0,
        tx_bytes: 0,
        kicks: 0,
        tx_full: 0,
        accepted: 0,
        closed: 0,
        open_conns: 0,
        http_requests: 0,
        bad_requests: 0,
        nic_ram: 0,
        msix: false,
    };
}

impl core::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "\"net\":{{\"rx_packets\":{},\"tx_packets\":{},\"rx_bytes\":{},\"tx_bytes\":{},\"kicks\":{},\"tx_full\":{},\"accepted\":{},\"closed\":{},\"open_conns\":{},\"http_requests\":{},\"bad_requests\":{},\"nic_ram\":{},\"msix\":{}}}",
            self.rx_packets,
            self.tx_packets,
            self.rx_bytes,
            self.tx_bytes,
            self.kicks,
            self.tx_full,
            self.accepted,
            self.closed,
            self.open_conns,
            self.http_requests,
            self.bad_requests,
            self.nic_ram,
            self.msix
        )
    }
}

/// virtio-net MSI-X interrupt. When idle it only wakes the CPU from `hlt` (the main loop polls the rings). While
/// a CPU-bound invocation runs, RX interrupts are enabled too, and a packet requests preemption so that network
/// I/O and short requests do not wait for the end of the hog's time slice.
pub fn on_irq() {
    NET_IRQS.fetch_add(1, Ordering::Relaxed);
    if PREEMPT_ON_RX.load(Ordering::Relaxed) {
        crate::time::PREEMPT.store(true, Ordering::Relaxed);
        crate::virtio_net::set_rx_interrupts(false);
    }
}

/// Set by the scheduler while a long-running (already preempted once) invocation runs.
pub static PREEMPT_ON_RX: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Sizing, chosen from the amount of RAM at boot.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub rx_buf: usize,
    pub tx_buf: usize,
    pub spare_listeners: usize,
    pub max_conns: usize,
    pub max_body: usize,
    pub nic_bufs: u16,
}

impl Limits {
    pub fn for_ram(ram: u64) -> Limits {
        if ram >= 48 << 20 {
            Limits { rx_buf: 8192, tx_buf: 16384, spare_listeners: 4, max_conns: 512, max_body: 32 << 20, nic_bufs: 128 }
        } else if ram >= 12 << 20 {
            Limits { rx_buf: 4096, tx_buf: 8192, spare_listeners: 2, max_conns: 64, max_body: 2 << 20, nic_bufs: 32 }
        } else {
            Limits { rx_buf: 2048, tx_buf: 4096, spare_listeners: 2, max_conns: 32, max_body: 256 << 10, nic_bufs: 16 }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConnId {
    idx: u32,
    gen: u32,
}

/// A complete HTTP request handed to the runtime.
pub struct Request {
    pub conn: ConnId,
    pub method: Method,
    pub target: Vec<u8>,
    pub body: Vec<u8>,
}

struct Conn {
    handle: SocketHandle,
    gen: u32,
    inbuf: Vec<u8>,
    sent_continue: bool,
    /// A request from this connection is with the runtime.
    busy: bool,
    keep_alive: bool,
    out: Vec<u8>,
    out_pos: usize,
    /// Close once `out` is flushed.
    close_after: bool,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub accepted: u64,
    pub closed: u64,
    pub requests: u64,
    pub bad_requests: u64,
}

pub struct Net {
    pub dev: VirtioNet,
    iface: Interface,
    sockets: SocketSet<'static>,
    listeners: Vec<SocketHandle>,
    conns: Vec<Option<Conn>>,
    free_conns: Vec<u32>,
    next_gen: u32,
    pub limits: Limits,
    pub stats: Stats,
}

fn now() -> Instant {
    Instant::from_micros(time::now_ns() as i64 / 1000)
}

impl Net {
    pub fn new(mut dev: VirtioNet, limits: Limits, seed: u64) -> Net {
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(dev.mac)));
        cfg.random_seed = seed;
        let mut iface = Interface::new(cfg, &mut dev, now());
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IP.into(), 24));
        });
        let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);
        let mut net = Net {
            dev,
            iface,
            sockets: SocketSet::new(vec![]),
            listeners: Vec::new(),
            conns: Vec::new(),
            free_conns: Vec::new(),
            next_gen: 1,
            limits,
            stats: Stats::default(),
        };
        net.refill_listeners();
        net
    }

    pub fn open_conns(&self) -> usize {
        self.conns.len() - self.free_conns.len()
    }

    fn refill_listeners(&mut self) {
        while self.listeners.len() < self.limits.spare_listeners {
            if self.open_conns() + self.listeners.len() >= self.limits.max_conns {
                break;
            }
            // Leave the heap its reserve — except for the last listener, so the node stays reachable (e.g. for
            // /_k10s/status) as long as the buffers physically fit.
            let bufs = (self.limits.rx_buf + self.limits.tx_buf) as u64;
            let reserve = if self.listeners.is_empty() { 64 << 10 } else { crate::reserve_bytes() };
            if crate::mem::frames::free_bytes() < bufs + reserve {
                break;
            }
            let mut s = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; self.limits.rx_buf]),
                tcp::SocketBuffer::new(vec![0u8; self.limits.tx_buf]),
            );
            s.set_nagle_enabled(false);
            s.set_ack_delay(None);
            s.set_timeout(Some(Duration::from_secs(120)));
            if s.listen(HTTP_PORT).is_err() {
                break;
            }
            self.listeners.push(self.sockets.add(s));
        }
    }

    /// Moves listeners that received a SYN into the connection table.
    fn accept(&mut self) {
        let mut i = 0;
        while i < self.listeners.len() {
            let h = self.listeners[i];
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            if s.is_listening() {
                i += 1;
                continue;
            }
            self.listeners.swap_remove(i);
            self.stats.accepted += 1;
            let gen = self.next_gen;
            self.next_gen = self.next_gen.wrapping_add(1).max(1);
            let c = Conn {
                handle: h,
                gen,
                inbuf: Vec::new(),
                sent_continue: false,
                busy: false,
                keep_alive: true,
                out: Vec::new(),
                out_pos: 0,
                close_after: false,
            };
            match self.free_conns.pop() {
                Some(idx) => self.conns[idx as usize] = Some(c),
                None => self.conns.push(Some(c)),
            }
        }
    }

    /// Processes received packets (one at a time, refilling the listen backlog in between).
    pub fn poll_ingress(&mut self) -> bool {
        let mut any = false;
        let t = now();
        self.iface.poll_maintenance(t);
        loop {
            self.refill_listeners();
            match self.iface.poll_ingress_single(t, &mut self.dev, &mut self.sockets) {
                smoltcp::iface::PollIngressSingleResult::None => break,
                _ => any = true,
            }
            self.accept();
        }
        any
    }

    /// Sends what the sockets have queued and notifies the device.
    pub fn poll_egress(&mut self) {
        let t = now();
        while self.iface.poll_egress(t, &mut self.dev, &mut self.sockets) != smoltcp::iface::PollResult::None {}
        self.dev.flush();
    }

    /// When smoltcp next needs to run (retransmissions, delayed ACKs, keep-alive), in ns from now.
    pub fn poll_delay_ns(&mut self) -> Option<u64> {
        self.iface.poll_delay(now(), &self.sockets).map(|d| d.total_micros() * 1000)
    }

    /// Reads requests from connections, writes pending responses, closes finished connections.
    /// Complete requests are passed to `submit`.
    pub fn service(&mut self, submit: &mut dyn FnMut(Request)) {
        for idx in 0..self.conns.len() {
            let Some(c) = self.conns[idx].as_mut() else { continue };
            let s = self.sockets.get_mut::<tcp::Socket>(c.handle);

            // Flush output.
            if c.out_pos < c.out.len() && s.can_send() {
                if let Ok(n) = s.send_slice(&c.out[c.out_pos..]) {
                    c.out_pos += n;
                }
                if c.out_pos == c.out.len() {
                    c.out.clear();
                    c.out_pos = 0;
                    if c.close_after {
                        s.close();
                    }
                }
            }

            // Read input.
            while s.can_recv() && c.inbuf.len() <= self.limits.max_body + http::MAX_HEAD {
                let r = s.recv(|b| {
                    let n = if c.inbuf.try_reserve(b.len()).is_ok() { b.len() } else { 0 };
                    c.inbuf.extend_from_slice(&b[..n]);
                    (n, n)
                });
                if !matches!(r, Ok(n) if n > 0) {
                    break;
                }
            }

            // Parse at most one request when idle.
            if !c.busy && !c.close_after && !c.inbuf.is_empty() {
                match http::parse_head(&c.inbuf) {
                    Parse::Incomplete => {}
                    Parse::Error(status) => {
                        self.stats.bad_requests += 1;
                        http::write_head(&mut c.out, status, None, 0, false);
                        c.inbuf.clear();
                        c.close_after = true;
                    }
                    Parse::Head(h) => {
                        if h.content_length > self.limits.max_body {
                            self.stats.bad_requests += 1;
                            http::write_head(&mut c.out, 413, None, 0, false);
                            c.inbuf.clear();
                            c.close_after = true;
                        } else if c.inbuf.len() >= h.len + h.content_length {
                            let body = c.inbuf[h.len..h.len + h.content_length].to_vec();
                            c.inbuf.drain(..h.len + h.content_length);
                            c.busy = true;
                            c.keep_alive = h.keep_alive;
                            c.sent_continue = false;
                            self.stats.requests += 1;
                            submit(Request {
                                conn: ConnId { idx: idx as u32, gen: c.gen },
                                method: h.method,
                                target: h.target,
                                body,
                            });
                        } else if h.expect_continue && !c.sent_continue {
                            c.sent_continue = true;
                            c.out.extend_from_slice(http::CONTINUE);
                        }
                    }
                }
            }

            // Peer finished sending and nothing left to do → close our side.
            let s = self.sockets.get_mut::<tcp::Socket>(c.handle);
            if !s.may_recv() && !c.busy && c.out.is_empty() && s.state() == tcp::State::CloseWait {
                s.close();
            }
            if matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) {
                let h = c.handle;
                self.sockets.remove(h);
                self.conns[idx] = None;
                self.free_conns.push(idx as u32);
                self.stats.closed += 1;
            }
        }
    }

    /// Queues the response to a request; dropped if the connection went away meanwhile.
    pub fn respond(&mut self, id: ConnId, status: u16, content_type: Option<&str>, body: &[u8]) {
        let Some(Some(c)) = self.conns.get_mut(id.idx as usize) else { return };
        if c.gen != id.gen {
            return;
        }
        c.busy = false;
        let keep = c.keep_alive;
        if c.out.try_reserve(body.len() + 128).is_err() {
            c.close_after = true;
            return;
        }
        http::write_head(&mut c.out, status, content_type, body.len(), keep);
        c.out.extend_from_slice(body);
        if !keep {
            c.close_after = true;
        }
        // Try to send right away (the rest goes out in `service`).
        let s = self.sockets.get_mut::<tcp::Socket>(c.handle);
        if s.can_send() {
            if let Ok(n) = s.send_slice(&c.out[c.out_pos..]) {
                c.out_pos += n;
            }
            if c.out_pos == c.out.len() {
                c.out.clear();
                c.out_pos = 0;
                if c.close_after {
                    s.close();
                }
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            rx_packets: self.dev.rx.packets,
            tx_packets: self.dev.tx.packets,
            rx_bytes: self.dev.rx.bytes,
            tx_bytes: self.dev.tx.bytes,
            kicks: self.dev.kicks,
            tx_full: self.dev.tx.full,
            accepted: self.stats.accepted,
            closed: self.stats.closed,
            open_conns: self.open_conns() as u64,
            http_requests: self.stats.requests,
            bad_requests: self.stats.bad_requests,
            nic_ram: self.dev.footprint() as u64,
            msix: self.dev.msix,
        }
    }

    /// True if some connection has bytes to write or a request ready to parse (the main loop should not halt).
    pub fn has_work(&mut self) -> bool {
        self.conns.iter().flatten().any(|c| {
            let s = self.sockets.get::<tcp::Socket>(c.handle);
            (c.out_pos < c.out.len() && s.can_send()) || (!c.busy && s.can_recv())
        })
    }
}
