//! Interop with Linux's TCP/IP stack, unprivileged: a QEMU guest (the host's kernel, an initramfs whose /init is
//! interop/guest) on a UDP socket netdev, and k10-net driven here on real time with raw Ethernet frames.
//! `interop/run.sh` builds the guest and runs this test.

use k10_net::sim::{stream_byte, Host, Rng};
use k10_net::tcp::ready;
use k10_net::wire::{ethernet, ipv4, ipv6, tcp};
use k10_net::{Error, Instant, ListenerConfig, TcpHandle, TcpState, UdpOptions, ViewId};
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant as Wall};

const GUEST4: [u8; 4] = [10, 0, 0, 2];

fn guest6() -> Ipv6Addr {
    "fd00:9::2".parse().unwrap()
}
fn ours6() -> Ipv6Addr {
    "fd00:9::1".parse().unwrap()
}

#[derive(Default, Debug)]
struct Counts {
    rx: u64,
    tx: u64,
    dropped_in: u64,
    dropped_out: u64,
    /// Segments from Linux carrying SACK blocks.
    sack_in: u64,
    /// Segments from us carrying SACK blocks.
    sack_out: u64,
    rst_in: u64,
}

struct Net {
    host: Host,
    sock: UdpSocket,
    qemu: SocketAddr,
    t0: Wall,
    rng: Rng,
    drop_in: f64,
    drop_out: f64,
    c: Counts,
}

fn tcp_of(frame: &[u8]) -> Option<(u8, u8)> {
    let e = ethernet::parse(frame)?;
    let l4 = match e.ethertype {
        ethernet::ETHERTYPE_IPV4 => ipv4::parse(e.payload).ok().filter(|p| p.proto == 6)?.payload,
        ethernet::ETHERTYPE_IPV6 => ipv6::parse(e.payload).ok().filter(|p| p.next == 6)?.payload,
        _ => return None,
    };
    let s = tcp::parse(l4)?;
    Some((s.opts.sack_len, s.flags))
}

impl Net {
    fn now(&self) -> Instant {
        Instant(self.t0.elapsed().as_nanos() as u64 + 1_000_000)
    }

    fn step(&mut self) {
        let now = self.now();
        let next = self.host.stack.poll_at(now).unwrap_or(Instant(now.0 + 20_000_000));
        let wait = Duration::from_nanos(next.0.saturating_sub(now.0)).clamp(Duration::from_micros(50), Duration::from_millis(20));
        self.sock.set_read_timeout(Some(wait)).unwrap();
        let mut buf = [0u8; 2048];
        let mut first = true;
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let f = &buf[..n];
                    self.c.rx += 1;
                    if let Some((sack, flags)) = tcp_of(f) {
                        self.c.sack_in += (sack > 0) as u64;
                        self.c.rst_in += (flags & tcp::RST != 0) as u64;
                        if self.drop_in > 0.0 && self.rng.f64() < self.drop_in {
                            self.c.dropped_in += 1;
                            continue;
                        }
                    }
                    let now = self.now();
                    self.host.stack.receive(now, f);
                }
                Err(_) => break,
            }
            if first {
                first = false;
                self.sock.set_nonblocking(true).unwrap();
            }
        }
        self.sock.set_nonblocking(false).unwrap();
        let mut out: Vec<Vec<u8>> = Vec::new();
        let now = self.now();
        self.host.stack.transmit(now, &mut out);
        for f in out {
            if let Some((sack, _)) = tcp_of(&f) {
                self.c.sack_out += (sack > 0) as u64;
                if self.drop_out > 0.0 && self.rng.f64() < self.drop_out {
                    self.c.dropped_out += 1;
                    continue;
                }
            }
            self.c.tx += 1;
            self.sock.send_to(&f, self.qemu).unwrap();
        }
    }

    /// Steps until `f` is true; panics after `limit`.
    fn until(&mut self, what: &str, limit: Duration, mut f: impl FnMut(&mut Net) -> bool) {
        let end = Wall::now() + limit;
        while !f(self) {
            assert!(Wall::now() < end, "timed out: {what} ({:?})", self.c);
            self.step();
        }
    }

    fn connect(&mut self, to: SocketAddr) -> TcpHandle {
        let now = self.now();
        let h = self.host.stack.tcp_connect(now, ViewId::NODE, None, to, None).unwrap();
        self.until("connect", Duration::from_secs(10), |n| n.host.stack.tcp_state(h).unwrap() != TcpState::SynSent);
        assert_eq!(self.host.stack.tcp_state(h).unwrap(), TcpState::Established, "{:?}", self.host.stack.tcp_error(h));
        h
    }
}

/// Sends `len` bytes of `seed`'s stream (after `prefix`) while reading what comes back into `got`.
fn stream(net: &mut Net, h: TcpHandle, prefix: &[u8], seed: u64, len: u64, got: &mut Vec<u8>, shut: bool, limit: Duration) {
    let mut sent_prefix = 0;
    let mut off = 0u64;
    let mut shut_done = false;
    net.until("stream", limit, |n| {
        let now = n.now();
        let s = &mut n.host.stack;
        while sent_prefix < prefix.len() {
            match s.tcp_send(h, &prefix[sent_prefix..]) {
                Ok(k) if k > 0 => sent_prefix += k,
                _ => break,
            }
        }
        while sent_prefix == prefix.len() && off < len {
            let rem = (len - off) as usize;
            let o = off;
            match s.tcp_send_with(h, rem, |b| {
                let k = b.len().min(rem);
                for (i, x) in b[..k].iter_mut().enumerate() {
                    *x = stream_byte(seed, o + i as u64);
                }
                k
            }) {
                Ok(k) if k > 0 => off += k as u64,
                _ => break,
            }
        }
        if shut && off == len && !shut_done && sent_prefix == prefix.len() {
            s.tcp_shutdown(now, h).unwrap();
            shut_done = true;
        }
        let mut b = [0u8; 65536];
        loop {
            match s.tcp_recv(now, h, &mut b) {
                Ok(0) => return off == len,
                Ok(k) => got.extend_from_slice(&b[..k]),
                Err(Error::WouldBlock) => break,
                Err(e) => panic!("recv: {e:?}"),
            }
        }
        false
    });
}

fn bulk_to_linux(net: &mut Net, to: IpAddr, len: u64, loss: f64) {
    net.drop_out = loss;
    let h = net.connect(SocketAddr::new(to, 7002));
    let mut hdr = 99u64.to_le_bytes().to_vec();
    hdr.extend_from_slice(&len.to_le_bytes());
    let mut reply = Vec::new();
    stream(net, h, &hdr, 99, len, &mut reply, true, Duration::from_secs(120));
    assert_eq!(String::from_utf8_lossy(&reply), format!("OK {len}\n"));
    let i = net.host.stack.tcp_info(h).unwrap();
    println!("to {to}: {len} bytes, {loss} loss: {} retransmits, {} recoveries, {} timeouts, {} TLP", i.retransmits, i.fast_recoveries, i.timeouts, i.tlp_probes);
    if loss > 0.0 {
        assert!(i.retransmits > 0 && i.fast_recoveries > 0, "{i:?}");
    }
    net.host.stack.tcp_close(net.now(), h).unwrap();
    net.drop_out = 0.0;
}

fn bulk_from_linux(net: &mut Net, from: IpAddr, len: u64, loss: f64) {
    net.drop_in = loss;
    let sack_before = net.c.sack_out;
    let h = net.connect(SocketAddr::new(from, 7003));
    let mut hdr = 7u64.to_le_bytes().to_vec();
    hdr.extend_from_slice(&len.to_le_bytes());
    let mut got = Vec::new();
    stream(net, h, &hdr, 0, 0, &mut got, false, Duration::from_secs(120));
    assert_eq!(got.len() as u64, len);
    assert!(got.iter().enumerate().all(|(i, &b)| b == stream_byte(7, i as u64)), "data from Linux differs");
    println!("from {from}: {len} bytes, {loss} loss: {} segments with our SACK blocks", net.c.sack_out - sack_before);
    if loss > 0.0 {
        assert!(net.c.sack_out > sack_before);
    }
    net.host.stack.tcp_close(net.now(), h).unwrap();
    net.drop_in = 0.0;
}

/// Kills the guest when the test ends, panics included.
struct Guest(Child);

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn boot() -> (Guest, Net) {
    let kernel = std::env::var("K10_NET_KERNEL").expect("K10_NET_KERNEL (run interop/run.sh)");
    let initrd = std::env::var("K10_NET_INITRAMFS").expect("K10_NET_INITRAMFS (run interop/run.sh)");
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let ours = sock.local_addr().unwrap().port();
    let qport = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut child = Command::new("qemu-system-x86_64")
        .args(["-M", "pc,accel=kvm:tcg", "-cpu", "max", "-m", "256M", "-smp", "2", "-nodefaults", "-display", "none"])
        .args(["-serial", "stdio", "-no-reboot", "-kernel", &kernel, "-initrd", &initrd])
        .args(["-append", "console=ttyS0 quiet panic=-1"])
        .args(["-netdev", &format!("socket,id=n0,udp=127.0.0.1:{ours},localaddr=127.0.0.1:{qport}")])
        .args(["-device", "virtio-net-pci,netdev=n0,mac=52:54:00:00:00:02,romfile="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .expect("qemu-system-x86_64");
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            println!("guest: {l}");
            let _ = tx.send(l);
        }
    });
    let end = Wall::now() + Duration::from_secs(120);
    loop {
        let l = rx.recv_timeout(end.saturating_duration_since(Wall::now())).expect("guest did not start");
        if l.contains("k10-net-guest ready") {
            break;
        }
    }
    let mut cfg = Host::config(1);
    cfg.tcp.max_retries = 8;
    let host = Host::new(1, cfg);
    let net = Net {
        host,
        sock,
        qemu: SocketAddr::from(([127, 0, 0, 1], qport)),
        t0: Wall::now(),
        rng: Rng(1),
        drop_in: 0.0,
        drop_out: 0.0,
        c: Counts::default(),
    };
    (Guest(child), net)
}

#[test]
#[ignore = "needs QEMU and the interop guest: crates/k10-net/interop/run.sh"]
fn linux_interop() {
    let (_guest, mut net) = boot();
    let g4 = IpAddr::from(GUEST4);
    let g6 = IpAddr::V6(guest6());
    let now = net.now();
    net.host.stack.add_ipv6(now, ours6(), 64);
    net.until("IPv6 addresses", Duration::from_secs(10), |n| n.host.stack.iface().is_local_v6(&ours6()));

    // Handshake options and an echo over IPv4.
    let h = net.connect(SocketAddr::new(g4, 7001));
    let i = net.host.stack.tcp_info(h).unwrap();
    assert!(i.sack_enabled && i.ts_enabled && i.snd_wscale > 0 && i.rcv_wscale == 7 && i.mss == 1448, "{i:?}");
    let mut echo = Vec::new();
    let n = 256 * 1024;
    let mut want = Vec::new();
    for k in 0..n {
        want.push(stream_byte(5, k as u64));
    }
    let mut sent = 0;
    net.until("echo", Duration::from_secs(20), |n| {
        let now = n.now();
        while sent < want.len() {
            match n.host.stack.tcp_send(h, &want[sent..]) {
                Ok(k) if k > 0 => sent += k,
                _ => break,
            }
        }
        let mut b = [0u8; 65536];
        while let Ok(k) = n.host.stack.tcp_recv(now, h, &mut b) {
            if k == 0 {
                break;
            }
            echo.extend_from_slice(&b[..k]);
        }
        echo.len() >= want.len()
    });
    assert_eq!(echo, want);
    net.host.stack.tcp_close(net.now(), h).unwrap();

    // Bulk both ways, clean and lossy (2 %: SACK recovery on both sides).
    bulk_to_linux(&mut net, g4, 8 << 20, 0.0);
    bulk_to_linux(&mut net, g4, 16 << 20, 0.02);
    assert!(net.c.sack_in > 0, "Linux sent SACK blocks for our losses");
    bulk_from_linux(&mut net, g4, 8 << 20, 0.0);
    bulk_from_linux(&mut net, g4, 16 << 20, 0.02);

    // Reset paths: Linux resets (SO_LINGER 0), and a closed port refuses.
    let h = net.connect(SocketAddr::new(g4, 7004));
    net.host.stack.tcp_send(h, b"x").unwrap();
    net.until("reset", Duration::from_secs(10), |n| n.host.stack.tcp_state(h).unwrap() == TcpState::Closed);
    assert_eq!(net.host.stack.tcp_error(h).unwrap(), Some(Error::Reset));
    let now = net.now();
    let h = net.host.stack.tcp_connect(now, ViewId::NODE, None, SocketAddr::new(g4, 7999), None).unwrap();
    net.until("refused", Duration::from_secs(10), |n| n.host.stack.tcp_state(h).unwrap() == TcpState::Closed);
    assert_eq!(net.host.stack.tcp_error(h).unwrap(), Some(Error::Refused));

    // FIN paths: we close first and end in TIME-WAIT after Linux's FIN.
    let h = net.connect(SocketAddr::new(g4, 7005));
    let mut reply = Vec::new();
    stream(&mut net, h, b"", 3, 100_000, &mut reply, true, Duration::from_secs(20));
    assert_eq!(reply, b"BYE 100000\n");
    net.until("time-wait", Duration::from_secs(10), |n| n.host.stack.tcp_state(h).unwrap() == TcpState::TimeWait);

    // IPv6: neighbor discovery with Linux, bulk both ways with loss.
    bulk_to_linux(&mut net, g6, 8 << 20, 0.01);
    bulk_from_linux(&mut net, g6, 8 << 20, 0.01);

    // Linux opens connections to us, over both families.
    let l = net.host.listen(8001, ListenerConfig::default());
    let ctl = net.connect(SocketAddr::new(g4, 7008));
    for (addr, seed) in [(IpAddr::from([10, 0, 0, 1]), 77u64), (IpAddr::V6(ours6()), 88)] {
        net.host.stack.tcp_send(ctl, format!("connect {addr} 8001 {seed} 1048576\n").as_bytes()).unwrap();
        let mut acc = None;
        net.until("accept", Duration::from_secs(10), |n| {
            acc = n.host.stack.tcp_accept(l);
            acc.is_some()
        });
        let h = acc.unwrap();
        let (_, peer) = net.host.stack.tcp_endpoints(h).unwrap();
        assert_eq!(peer.is_ipv6(), addr.is_ipv6());
        let mut got = Vec::new();
        let mut eof = false;
        net.until("passive read", Duration::from_secs(30), |n| {
            let now = n.now();
            let mut b = [0u8; 65536];
            loop {
                match n.host.stack.tcp_recv(now, h, &mut b) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(k) => got.extend_from_slice(&b[..k]),
                    Err(_) => break,
                }
            }
            eof
        });
        assert_eq!(got.len(), 1 << 20);
        assert!(got.iter().enumerate().all(|(i, &b)| b == stream_byte(seed, i as u64)));
        stream(&mut net, h, b"", seed + 1, 512 * 1024, &mut Vec::new(), true, Duration::from_secs(30));
        let mut line = Vec::new();
        net.until("control reply", Duration::from_secs(30), |n| {
            let now = n.now();
            let mut b = [0u8; 256];
            if let Ok(k) = n.host.stack.tcp_recv(now, ctl, &mut b) {
                line.extend_from_slice(&b[..k]);
            }
            line.ends_with(b"\n")
        });
        assert_eq!(String::from_utf8_lossy(&line), "DONE 524288 OK\n");
        net.until("passive close", Duration::from_secs(10), |n| n.host.stack.tcp_state(h).unwrap() == TcpState::Closed);
        assert!(net.host.stack.tcp_error(h).unwrap().is_none());
    }

    // UDP echo over both families.
    let b = net.host.ports.bind_udp(ViewId::NODE, None, 0, 0, UdpOptions::default()).unwrap();
    net.host.stack.set_ports(net.host.ports.snapshot());
    let u = net.host.stack.udp_open(b, None).unwrap();
    for dst in [g4, g6] {
        let now = net.now();
        net.host.stack.udp_send_to(now, u, SocketAddr::new(dst, 7007), b"udp-echo").unwrap();
        let mut got = None;
        net.until("udp echo", Duration::from_secs(10), |n| {
            n.host.stack.udp_recv_from(u, |d, src, _| got = Some((d.to_vec(), src))).unwrap();
            got.is_some()
        });
        let (d, src) = got.unwrap();
        assert_eq!((d.as_slice(), src), (&b"udp-echo"[..], SocketAddr::new(dst, 7007)));
    }
    let ev: Vec<_> = std::iter::from_fn(|| net.host.stack.poll_event()).collect();
    assert!(!ev.is_empty());
    let _ = ready::READABLE;
    println!("frames {:?}", net.c);
}
