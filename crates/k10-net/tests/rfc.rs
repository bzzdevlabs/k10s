//! TCP behavior per RFC, segment by segment against one stack instance and a scripted peer.

mod common;
use common::peer::*;
use k10_net::tcp::{ready, Keepalive, TcpConfig};
use k10_net::wire::tcp::{Header, Options, ACK, FIN, PSH, RST, SYN};
use k10_net::{Error, Event, ListenerConfig, ListenerId, TcpHandle, TcpState, ViewId};
use std::net::SocketAddr;
use std::time::Duration;

struct Conn {
    h: TcpHandle,
    /// Peer's next sequence number.
    pseq: u32,
    /// Stack's next sequence number (what the peer acknowledges).
    sseq: u32,
    ts: Option<u32>,
}

fn listen(b: &mut Bench, cfg: ListenerConfig) -> ListenerId {
    b.h.listen(80, cfg)
}

fn open(b: &mut Bench, l: ListenerId, sport: u16, ts: Option<u32>) -> Conn {
    b.tcp(Header { opts: syn_opts(ts), ..hdr(sport, 80, 1000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().into_iter().find(|s| s.has(SYN)).expect("SYN-ACK");
    assert!(sa.has(ACK) && sa.ack == 1001);
    let mut a = hdr(sport, 80, 1001, sa.seq.wrapping_add(1), ACK, 512);
    a.opts.ts = sa.opts.ts.map(|(v, _)| (ts.unwrap_or(0) + 1, v));
    b.tcp(a, &[]);
    let h = b.h.stack.tcp_accept(l).expect("accepted");
    Conn { h, pseq: 1001, sseq: sa.seq.wrapping_add(1), ts: ts.map(|t| t + 1) }
}

fn data_hdr(c: &Conn, sport: u16, flags: u8) -> Header {
    let mut h = hdr(sport, 80, c.pseq, c.sseq, flags, 512);
    h.opts.ts = c.ts.map(|t| (t, 0));
    h
}

#[test]
fn handshake_negotiates_options() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    b.tcp(Header { opts: syn_opts(Some(100)), ..hdr(5000, 80, 1000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    assert!(sa.has(SYN) && sa.has(ACK) && sa.ack == 1001);
    assert_eq!(sa.opts.mss, Some(1460));
    assert_eq!(sa.opts.wscale, Some(7), "4 MiB receive buffer needs shift 7");
    assert!(sa.opts.sack_perm);
    assert_eq!(sa.opts.ts.unwrap().1, 100);
    assert!(b.h.stack.tcp_accept(l).is_none());
    let mut a = hdr(5000, 80, 1001, sa.seq + 1, ACK, 512);
    a.opts.ts = Some((101, sa.opts.ts.unwrap().0));
    b.tcp(a, &[]);
    assert!(matches!(b.h.stack.poll_event(), Some(Event::Accept { listener }) if listener == l));
    let h = b.h.stack.tcp_accept(l).unwrap();
    let i = b.h.stack.tcp_info(h).unwrap();
    assert!(i.sack_enabled && i.ts_enabled && i.snd_wscale == 7 && i.snd_wnd == 512 << 7);
    // Options the peer did not offer are not used.
    b.tcp(Header { opts: Options { mss: Some(1200), ..Default::default() }, ..hdr(5001, 80, 7, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    assert_eq!((sa.opts.wscale, sa.opts.sack_perm, sa.opts.ts), (None, false, None));
}

#[test]
fn closed_port_resets() {
    let mut b = Bench::new(|_| {});
    b.tcp(hdr(5000, 81, 1000, 0, SYN, 65535), &[]);
    let r = b.segs().pop().unwrap();
    assert!(r.has(RST) && r.has(ACK) && r.seq == 0 && r.ack == 1001);
    b.tcp(hdr(5000, 81, 1000, 777, ACK, 65535), b"x");
    let r = b.segs().pop().unwrap();
    assert!(r.has(RST) && !r.has(ACK) && r.seq == 777);
    b.tcp(hdr(5000, 81, 1000, 777, RST, 65535), &[]);
    assert!(b.segs().is_empty(), "never answer a reset");
}

#[test]
fn rfc5961_blind_reset_and_syn_get_challenge_acks() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    b.flush();
    // In-window but not exact: challenge ACK, connection kept.
    let mut r = data_hdr(&c, 5000, RST);
    r.seq = c.pseq + 100;
    b.tcp(r, &[]);
    let s = b.segs();
    assert!(s.len() == 1 && s[0].has(ACK) && !s[0].has(RST) && s[0].ack == c.pseq);
    assert_eq!(b.h.stack.tcp_state(c.h).unwrap(), TcpState::Established);
    // SYN on an established connection: challenge ACK.
    b.tcp(hdr(5000, 80, c.pseq + 5, 0, SYN, 512), &[]);
    let s = b.segs();
    assert!(s.len() == 1 && s[0].has(ACK) && s[0].ack == c.pseq);
    // An ACK far behind SND.UNA is dropped with a challenge ACK.
    b.tcp(hdr(5000, 80, c.pseq, c.sseq.wrapping_sub(1 << 30), ACK, 512), &[]);
    assert_eq!(b.segs().len(), 1);
    // Exact sequence number: reset.
    b.tcp(data_hdr(&c, 5000, RST), &[]);
    assert_eq!(b.h.stack.tcp_state(c.h).unwrap(), TcpState::Closed);
    assert_eq!(b.h.stack.tcp_error(c.h).unwrap(), Some(Error::Reset));
    let mut buf = [0u8; 4];
    assert_eq!(b.h.stack.tcp_recv(b.now, c.h, &mut buf), Err(Error::Reset));
}

#[test]
fn paws_drops_old_timestamps() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let mut c = open(&mut b, l, 5000, Some(1000));
    b.tcp(data_hdr(&c, 5000, ACK | PSH), b"fresh");
    c.pseq += 5;
    let mut old = data_hdr(&c, 5000, ACK | PSH);
    old.opts.ts = Some((500, 0));
    b.tcp(old, b"stale");
    let mut buf = [0u8; 16];
    assert_eq!(b.h.stack.tcp_recv(b.now, c.h, &mut buf), Ok(5));
    assert_eq!(&buf[..5], b"fresh");
    assert!(b.segs().iter().any(|s| s.has(ACK) && s.ack == c.pseq));
}

#[test]
fn out_of_order_data_is_sacked_and_duplicates_dsacked() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    let p = c.pseq;
    let seg = |off: u32| Header { seq: p + off, ..data_hdr(&c, 5000, ACK) };
    b.tcp(seg(1000), &[2u8; 1000]);
    let s = b.segs();
    assert_eq!(s.len(), 1, "out-of-order data is acknowledged at once");
    assert_eq!(s[0].ack, p);
    assert_eq!(s[0].opts.sack_blocks(), &[(p + 1000, p + 2000)]);
    b.tcp(seg(3000), &[4u8; 1000]);
    let s = b.segs();
    assert_eq!(s[0].opts.sack_blocks(), &[(p + 3000, p + 4000), (p + 1000, p + 2000)], "newest block first");
    b.tcp(seg(0), &[1u8; 1000]);
    let s = b.segs();
    assert_eq!(s[0].ack, p + 2000);
    assert_eq!(s[0].opts.sack_blocks(), &[(p + 3000, p + 4000)]);
    b.tcp(seg(0), &[1u8; 1000]);
    let s = b.segs();
    assert_eq!(s[0].opts.sack_blocks()[0], (p, p + 1000), "D-SACK reports the duplicate first");
    let mut buf = vec![0u8; 4000];
    assert_eq!(b.h.stack.tcp_recv(b.now, c.h, &mut buf), Ok(2000));
}

#[test]
fn delayed_ack_every_second_segment_or_timer() {
    let mut b = Bench::new(|c| c.tcp.quickack = 0);
    let l = listen(&mut b, ListenerConfig { tcp: TcpConfig { quickack: 0, ..Default::default() }, ..Default::default() });
    let mut c = open(&mut b, l, 5000, None);
    b.tcp(data_hdr(&c, 5000, ACK), &[0u8; 1460]);
    c.pseq += 1460;
    assert!(b.segs().is_empty(), "one segment: ACK delayed");
    b.advance(Duration::from_millis(39));
    assert!(b.segs().is_empty());
    b.advance(Duration::from_millis(2));
    let s = b.segs();
    assert!(s.len() == 1 && s[0].ack == c.pseq);
    b.tcp(data_hdr(&c, 5000, ACK), &[0u8; 1460]);
    c.pseq += 1460;
    b.tcp(data_hdr(&c, 5000, ACK), &[0u8; 1460]);
    c.pseq += 1460;
    let s = b.segs();
    assert!(s.len() == 1 && s[0].ack == c.pseq, "second full segment: immediate ACK");
}

#[test]
fn nagle_holds_small_segments_unless_nodelay() {
    for nodelay in [false, true] {
        let mut b = Bench::new(|_| {});
        let l = listen(&mut b, ListenerConfig::default());
        let c = open(&mut b, l, 5000, None);
        b.h.stack.tcp_set_nodelay(c.h, nodelay).unwrap();
        b.h.stack.tcp_send(c.h, b"hello").unwrap();
        assert_eq!(b.segs().len(), 1);
        b.h.stack.tcp_send(c.h, b"world").unwrap();
        let s = b.segs();
        assert_eq!(s.len(), nodelay as usize, "nodelay {nodelay}");
        if !nodelay {
            b.tcp(hdr(5000, 80, c.pseq, c.sseq + 5, ACK, 512), &[]);
            let s = b.segs();
            assert!(s.len() == 1 && s[0].data == b"world");
        }
    }
}

#[test]
fn zero_window_is_probed_until_it_opens() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    b.tcp(hdr(5000, 80, c.pseq, c.sseq, ACK, 0), &[]);
    b.h.stack.tcp_send(c.h, &[9u8; 100]).unwrap();
    assert!(b.segs().is_empty(), "zero window: nothing sent");
    let mut probes = Vec::new();
    for _ in 0..30 {
        b.advance(Duration::from_millis(100));
        probes.extend(b.segs());
    }
    assert!(probes.len() >= 2, "window probes with backoff");
    assert!(probes.iter().all(|p| p.seq == c.sseq.wrapping_sub(1) && p.data.is_empty()));
    b.tcp(hdr(5000, 80, c.pseq, c.sseq, ACK, 512), &[]);
    let s = b.segs();
    assert!(s.len() == 1 && s[0].data.len() == 100);
}

#[test]
fn syn_cookies_beyond_the_syn_backlog() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig { syn_backlog: 1, ..Default::default() });
    b.tcp(Header { opts: syn_opts(Some(50)), ..hdr(6000, 80, 1000, 0, SYN, 65535) }, &[]);
    assert_eq!(b.segs().len(), 1);
    b.tcp(Header { opts: syn_opts(Some(50)), ..hdr(6001, 80, 2000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    assert!(sa.has(SYN) && sa.has(ACK));
    assert_eq!(b.h.stack.stats.syn_cookies_sent, 1);
    assert_eq!(b.h.stack.tcp_count(), 1, "no state for the cookie");
    // A forged ACK is refused.
    b.tcp(hdr(6001, 80, 2001, sa.seq.wrapping_add(7), ACK, 512), &[]);
    assert!(b.segs().iter().any(|s| s.has(RST)));
    assert_eq!(b.h.stack.stats.syn_cookies_bad, 1);
    // The real one completes the handshake with the options the cookie kept.
    let mut a = hdr(6001, 80, 2001, sa.seq.wrapping_add(1), ACK | PSH, 512);
    a.opts.ts = Some((51, sa.opts.ts.unwrap().0));
    b.tcp(a, b"GET");
    let h = b.h.stack.tcp_accept(l).unwrap();
    let i = b.h.stack.tcp_info(h).unwrap();
    assert!(i.sack_enabled && i.ts_enabled && i.snd_wscale == 7 && i.mss == 1448);
    let mut buf = [0u8; 8];
    assert_eq!(b.h.stack.tcp_recv(b.now, h, &mut buf), Ok(3));
    assert_eq!(b.h.stack.stats.syn_cookies_ok, 1);
}

#[test]
fn accept_backlog_is_capped() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig { backlog: 1, ..Default::default() });
    let _c = open(&mut b, l, 5000, None);
    // `open` accepted it; fill the queue with one more, then a third SYN is dropped.
    b.tcp(Header { opts: syn_opts(None), ..hdr(5001, 80, 1000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    b.tcp(hdr(5001, 80, 1001, sa.seq + 1, ACK, 512), &[]);
    b.tcp(Header { opts: syn_opts(None), ..hdr(5002, 80, 1000, 0, SYN, 65535) }, &[]);
    assert!(b.segs().is_empty());
    assert_eq!(b.h.stack.ports().listener(l).unwrap().stats().overflows, 1);
}

#[test]
fn time_wait_ignores_resets_and_accepts_a_newer_syn() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    b.h.stack.tcp_shutdown(b.now, c.h).unwrap();
    let fin = b.segs().pop().unwrap();
    assert!(fin.has(FIN));
    b.tcp(hdr(5000, 80, c.pseq, c.sseq + 1, ACK | FIN, 512), &[]);
    assert_eq!(b.h.stack.tcp_state(c.h).unwrap(), TcpState::TimeWait);
    assert!(b.segs().iter().any(|s| s.has(ACK) && s.ack == c.pseq + 1));
    b.tcp(hdr(5000, 80, c.pseq + 1, 0, RST, 0), &[]);
    assert_eq!(b.h.stack.tcp_state(c.h).unwrap(), TcpState::TimeWait, "RFC 1337");
    // A retransmitted FIN is acknowledged again.
    b.tcp(hdr(5000, 80, c.pseq, c.sseq + 1, ACK | FIN, 512), &[]);
    assert!(b.segs().iter().any(|s| s.has(ACK) && s.ack == c.pseq + 1));
    b.h.stack.tcp_close(b.now, c.h).unwrap();
    // A new SYN above the old sequence space reopens the port pair.
    b.tcp(Header { opts: syn_opts(None), ..hdr(5000, 80, c.pseq + 100_000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    assert!(sa.has(SYN) && sa.has(ACK) && sa.ack == c.pseq + 100_001);
}

#[test]
fn retransmission_backoff_then_timeout() {
    let mut b = Bench::new(|_| {});
    let tcp = TcpConfig { max_retries: 3, ..Default::default() };
    let l = listen(&mut b, ListenerConfig { tcp, ..Default::default() });
    let c = open(&mut b, l, 5000, None);
    b.h.stack.tcp_send(c.h, &[1u8; 500]).unwrap();
    assert_eq!(b.segs().len(), 1);
    let start = b.now;
    let mut times = Vec::new();
    for _ in 0..200 {
        b.advance(Duration::from_millis(50));
        for s in b.segs() {
            if !s.data.is_empty() && s.seq == c.sseq {
                times.push(b.now.0 - start.0);
            }
        }
    }
    // Probe at 2·SRTT, then RTO doubling: the gaps grow.
    assert!(times.len() >= 3, "{times:?}");
    assert!(times.windows(3).all(|w| w[2] - w[1] >= w[1] - w[0]), "{times:?}");
    assert_eq!(b.h.stack.tcp_state(c.h).unwrap(), TcpState::Closed);
    assert_eq!(b.h.stack.tcp_error(c.h).unwrap(), Some(Error::TimedOut));
}

#[test]
fn syn_retries_give_up() {
    let mut b = Bench::new(|c| c.tcp.syn_retries = 2);
    let h = b.h.stack.tcp_connect(b.now, ViewId::NODE, None, SocketAddr::new(Host9::ip().into(), 80), None).unwrap();
    let mut syns = Vec::new();
    for _ in 0..80 {
        for s in b.segs() {
            if s.has(SYN) {
                syns.push(b.now.millis());
            }
        }
        b.advance(Duration::from_millis(100));
    }
    assert_eq!(syns.len(), 3, "{syns:?}");
    assert_eq!(syns[1] - syns[0], 1000);
    assert_eq!(syns[2] - syns[1], 2000);
    assert_eq!(b.h.stack.tcp_error(h).unwrap(), Some(Error::TimedOut));
}

struct Host9;
impl Host9 {
    fn ip() -> std::net::Ipv4Addr {
        std::net::Ipv4Addr::new(10, 0, 0, PEER)
    }
}

#[test]
fn active_open_refused_by_reset() {
    let mut b = Bench::new(|_| {});
    let h = b.h.stack.tcp_connect(b.now, ViewId::NODE, None, SocketAddr::new(Host9::ip().into(), 80), None).unwrap();
    let syn = b.segs().pop().unwrap();
    b.tcp(hdr(80, syn.sport, 0, syn.seq + 1, RST | ACK, 0), &[]);
    assert_eq!(b.h.stack.tcp_error(h).unwrap(), Some(Error::Refused));
    let ev: Vec<_> = std::iter::from_fn(|| b.h.stack.poll_event()).collect();
    assert!(ev.iter().any(|e| matches!(e, Event::Tcp { ready: r, .. } if r & ready::ERROR != 0)));
}

#[test]
fn close_with_unread_data_resets() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    b.tcp(data_hdr(&c, 5000, ACK | PSH), b"unread");
    b.flush();
    b.h.stack.tcp_close(b.now, c.h).unwrap();
    let s = b.segs();
    assert!(s.iter().any(|s| s.has(RST)), "{s:?}");
    assert_eq!(b.h.stack.tcp_count(), 0);
}

#[test]
fn data_after_close_resets() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    b.h.stack.tcp_close(b.now, c.h).unwrap();
    assert!(b.segs().iter().any(|s| s.has(FIN)));
    b.tcp(data_hdr(&c, 5000, ACK), b"late");
    assert!(b.segs().iter().any(|s| s.has(RST)));
}

#[test]
fn keepalive_probes_then_drops() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    let k = Keepalive { idle: Duration::from_secs(10), interval: Duration::from_secs(2), count: 3 };
    b.h.stack.tcp_set_keepalive(b.now, c.h, Some(k)).unwrap();
    let mut probes = 0;
    let mut reset = false;
    for _ in 0..300 {
        b.advance(Duration::from_millis(100));
        for s in b.segs() {
            if s.has(RST) {
                // Like Linux, the connection is reset when the probes go unanswered.
                reset = true;
                continue;
            }
            assert_eq!(s.seq, c.sseq.wrapping_sub(1));
            probes += 1;
            if probes == 1 {
                // The first probe is answered: the idle period starts again.
                b.tcp(hdr(5000, 80, c.pseq, c.sseq, ACK, 512), &[]);
            }
        }
    }
    assert_eq!(probes, 4, "one answered, then `count` unanswered");
    assert!(reset);
    assert_eq!(b.h.stack.tcp_error(c.h).unwrap(), Some(Error::TimedOut));
}

#[test]
fn peer_window_scale_bounds_sending() {
    let mut b = Bench::new(|_| {});
    let l = listen(&mut b, ListenerConfig::default());
    let c = open(&mut b, l, 5000, None);
    b.tcp(hdr(5000, 80, c.pseq, c.sseq, ACK, 30), &[]);
    b.h.stack.tcp_set_nodelay(c.h, true).unwrap();
    b.h.stack.tcp_send(c.h, &[0u8; 10000]).unwrap();
    // 30 << 7 = 3840 bytes of window: two full segments; the 920-byte rest waits (sender SWS avoidance).
    let sent: usize = b.segs().iter().map(|s| s.data.len()).sum();
    assert_eq!(sent, 2 * 1460);
    b.tcp(hdr(5000, 80, c.pseq, c.sseq + 2920, ACK, 100), &[]);
    let sent: usize = b.segs().iter().map(|s| s.data.len()).sum();
    assert_eq!(sent, 10000 - 2920);
}
