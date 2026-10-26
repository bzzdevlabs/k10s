//! TCP (RFC 9293) with window scaling and timestamps (RFC 7323), SACK (RFC 2018, 2883, 6675), RACK-TLP
//! (RFC 8985), NewReno (RFC 6582) and CUBIC (RFC 9438) with PRR (RFC 6937), delayed ACKs, Nagle, keepalive,
//! TIME-WAIT (RFC 1337, 6191), RFC 5961 hardening, SYN cookies and RFC 6528 ISNs.

pub mod cc;
pub mod rtt;
pub mod secrets;
pub mod sendq;
pub mod seq;
pub(crate) mod tcb;

pub use cc::CcAlgo;

use core::net::IpAddr;
use core::time::Duration;

/// A connection's addresses and ports (as seen from this host).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlowKey {
    pub local: IpAddr,
    pub remote: IpAddr,
    pub lport: u16,
    pub rport: u16,
}

pub(crate) fn ip16(a: &IpAddr) -> [u8; 16] {
    match a {
        IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        IpAddr::V6(v) => v.octets(),
    }
}

impl FlowKey {
    /// Local address, remote address, local port, remote port (IPv4 as IPv4-mapped).
    pub fn bytes(&self) -> [u8; 36] {
        let mut b = [0u8; 36];
        b[..16].copy_from_slice(&ip16(&self.local));
        b[16..32].copy_from_slice(&ip16(&self.remote));
        b[32..34].copy_from_slice(&self.lport.to_be_bytes());
        b[34..36].copy_from_slice(&self.rport.to_be_bytes());
        b
    }
    pub fn is_v6(&self) -> bool {
        self.local.is_ipv6()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keepalive {
    pub idle: Duration,
    pub interval: Duration,
    pub count: u32,
}

impl Default for Keepalive {
    fn default() -> Self {
        Keepalive { idle: Duration::from_secs(7200), interval: Duration::from_secs(75), count: 9 }
    }
}

/// Per-connection settings. Buffers start at `*_init` and grow on demand up to `*_max`, each byte charged to
/// the connection's quotas.
#[derive(Clone, Debug)]
pub struct TcpConfig {
    pub rx_init: usize,
    pub rx_max: usize,
    pub tx_init: usize,
    pub tx_max: usize,
    /// TCP_NODELAY: no Nagle.
    pub nodelay: bool,
    pub cc: CcAlgo,
    pub sack: bool,
    pub timestamps: bool,
    pub window_scaling: bool,
    pub keepalive: Option<Keepalive>,
    pub delayed_ack: Duration,
    /// Segments acknowledged immediately at the start of a connection (receiver quick-ACK).
    pub quickack: u32,
    pub init_rto: Duration,
    pub min_rto: Duration,
    pub max_rto: Duration,
    pub syn_retries: u32,
    pub synack_retries: u32,
    /// Retransmission timeouts (or unanswered window probes) before the connection is dropped.
    pub max_retries: u32,
    /// An orphaned FIN-WAIT-2 connection is dropped after this long.
    pub fin_timeout: Duration,
    pub time_wait: Duration,
    /// Out-of-order ranges held in the receive buffer.
    pub max_ooo_ranges: usize,
    /// Segments tracked in flight (bounds the scoreboard).
    pub max_inflight_segs: usize,
    pub initial_window_segs: u32,
    pub rack: bool,
    pub tlp: bool,
    /// Challenge ACKs (RFC 5961) per connection per second.
    pub challenge_acks_per_s: u32,
}

impl Default for TcpConfig {
    fn default() -> Self {
        TcpConfig {
            rx_init: 64 * 1024,
            rx_max: 4 * 1024 * 1024,
            tx_init: 16 * 1024,
            tx_max: 4 * 1024 * 1024,
            nodelay: false,
            cc: CcAlgo::Cubic,
            sack: true,
            timestamps: true,
            window_scaling: true,
            keepalive: None,
            delayed_ack: Duration::from_millis(40),
            quickack: 16,
            init_rto: Duration::from_secs(1),
            min_rto: Duration::from_millis(200),
            max_rto: Duration::from_secs(120),
            syn_retries: 6,
            synack_retries: 5,
            max_retries: 15,
            fin_timeout: Duration::from_secs(60),
            time_wait: Duration::from_secs(60),
            max_ooo_ranges: 32,
            max_inflight_segs: 16384,
            initial_window_segs: 10,
            rack: true,
            tlp: true,
            challenge_acks_per_s: 10,
        }
    }
}

impl TcpConfig {
    /// The receive window shift that covers `rx_max`.
    pub fn wscale(&self) -> u8 {
        let mut s = 0u8;
        while (self.rx_max >> s) > 65535 && s < crate::wire::tcp::MAX_WSCALE {
            s += 1;
        }
        s
    }
}

/// Connection counters and estimator state (`TCP_INFO`).
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpInfo {
    pub state: Option<State>,
    pub snd_una: u32,
    pub snd_nxt: u32,
    pub rcv_nxt: u32,
    pub snd_wnd: u32,
    pub rcv_wnd: u32,
    pub mss: u32,
    pub cwnd: u32,
    pub ssthresh: u32,
    pub srtt_us: u64,
    pub rttvar_us: u64,
    pub min_rtt_us: u64,
    pub rto_us: u64,
    pub in_flight: u32,
    pub retransmits: u64,
    pub fast_recoveries: u64,
    pub timeouts: u64,
    pub tlp_probes: u64,
    pub dsacks: u64,
    pub sack_enabled: bool,
    pub ts_enabled: bool,
    pub snd_wscale: u8,
    pub rcv_wscale: u8,
    pub rx_buffer: usize,
    pub tx_buffer: usize,
    pub bytes_acked: u64,
    pub bytes_received: u64,
}

/// Readiness bits reported with [`crate::Event::Tcp`].
pub mod ready {
    pub const READABLE: u8 = 1;
    pub const WRITABLE: u8 = 2;
    pub const CONNECTED: u8 = 4;
    /// The peer closed its side (end of stream once the data is read).
    pub const HUP: u8 = 8;
    pub const ERROR: u8 = 16;
    pub const CLOSED: u8 = 32;
}
