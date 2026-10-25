//! The UDP log console (`k10.console=…,udp:IP[:PORT]`, like Linux's netconsole): everything printed, from the
//! moment the command line is read, is kept in a 32 KiB ring (the oldest bytes give way, and the loss is reported) and
//! sent from local port 6665 to the collector as datagrams of whole lines once the interface has an address.
//! Receive it with e.g. `nc -klu 6666` or `socat -u UDP-RECV:6666 -`.
//!
//! A panic or fatal error on CPU 0 (which owns the NIC) sends what is left directly through the NIC, without the
//! TCP/IP stack: datagrams built from a copy of the headers of the last console datagram that went out (so the
//! collector's or gateway's MAC address is known). Output of a panic before the first datagram went out stays in
//! the ring, unsent.

use core::net::SocketAddrV4;
use core::sync::atomic::{AtomicPtr, Ordering};

use alloc::boxed::Box;

use crate::dev::NetDevice;
use crate::sync::{IrqLock, SetOnce};

pub const LOCAL_PORT: u16 = 6665;
pub const DEFAULT_PORT: u16 = 6666;
/// Payload bytes per datagram (fits a 1500-byte MTU with IPv4 and UDP headers).
pub const DATAGRAM: usize = 1400;
const RING: usize = 32 << 10;
/// Ethernet + IPv4 (no options) + UDP headers.
const HEADERS: usize = 42;

static TARGET: SetOnce<SocketAddrV4> = SetOnce::new();

struct Ring {
    buf: Box<[u8]>,
    start: usize,
    len: usize,
    lost: u64,
}

/// Allocated when a collector is configured.
static LOG: IrqLock<Option<Ring>> = IrqLock::new(None);
/// Headers of the last console datagram sent (for the panic path).
static TEMPLATE: IrqLock<Option<[u8; HEADERS]>> = IrqLock::new(None);
/// The NIC, for the panic path (the `Box` lives in `Net`, which lives as long as the kernel).
static NIC: AtomicPtr<Box<dyn NetDevice>> = AtomicPtr::new(core::ptr::null_mut());

pub fn configure(dst: SocketAddrV4) {
    *LOG.lock() = Some(Ring { buf: alloc::vec![0u8; RING].into_boxed_slice(), start: 0, len: 0, lost: 0 });
    TARGET.set(dst);
}

pub fn target() -> Option<SocketAddrV4> {
    TARGET.get().copied()
}

pub fn set_nic(nic: &mut Box<dyn NetDevice>) {
    NIC.store(nic, Ordering::Release);
}

pub fn write(s: &str) {
    if TARGET.get().is_none() {
        return;
    }
    let mut g = LOG.lock();
    let Some(r) = g.as_mut() else { return };
    for &b in s.as_bytes() {
        if r.len == RING {
            r.start = (r.start + 1) % RING;
            r.len -= 1;
            r.lost += 1;
        }
        let at = (r.start + r.len) % RING;
        r.buf[at] = b;
        r.len += 1;
    }
}

/// Copies the next datagram's worth of text into `out` — whole lines when they fit — and removes it from the
/// ring; 0 when there is nothing to send. A loss is reported first.
pub fn take(out: &mut [u8; DATAGRAM]) -> usize {
    let mut g = LOG.lock();
    let Some(r) = g.as_mut() else { return 0 };
    if r.lost > 0 {
        let lost = core::mem::take(&mut r.lost);
        let mut w = Cursor { b: out, n: 0 };
        let _ = core::fmt::write(&mut w, format_args!("[udp console: {lost} bytes lost]\n"));
        return w.n;
    }
    let n = r.len.min(DATAGRAM);
    if n == 0 {
        return 0;
    }
    for (i, o) in out[..n].iter_mut().enumerate() {
        *o = r.buf[(r.start + i) % RING];
    }
    // Cut after the last newline unless that leaves nothing (a line longer than a datagram).
    let n = match out[..n].iter().rposition(|&b| b == b'\n') {
        Some(p) if n == DATAGRAM || r.len > n => p + 1,
        _ => n,
    };
    r.start = (r.start + n) % RING;
    r.len -= n;
    n
}

/// Puts back what `take` returned when it could not be sent (at the front of the ring).
pub fn untake(data: &[u8]) {
    let mut g = LOG.lock();
    let Some(r) = g.as_mut() else { return };
    for &b in data.iter().rev() {
        if r.len == RING {
            r.lost += 1;
            continue;
        }
        r.start = (r.start + RING - 1) % RING;
        let s = r.start;
        r.buf[s] = b;
        r.len += 1;
    }
}

/// Whether a console datagram went out (the panic path can send).
pub fn sent_one() -> bool {
    TEMPLATE.lock().is_some()
}

pub fn pending() -> bool {
    TARGET.get().is_some() && LOG.lock().as_ref().is_some_and(|r| r.len > 0)
}

struct Cursor<'a> {
    b: &'a mut [u8; DATAGRAM],
    n: usize,
}

impl core::fmt::Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let m = s.len().min(DATAGRAM - self.n);
        self.b[self.n..self.n + m].copy_from_slice(&s.as_bytes()[..m]);
        self.n += m;
        Ok(())
    }
}

/// Watches outgoing frames (net.rs) for console datagrams and keeps their headers.
pub fn observe_tx(frame: &[u8]) {
    let Some(dst) = TARGET.get() else { return };
    // IPv4 without options, UDP, our source port, the collector as destination.
    if frame.len() >= HEADERS
        && frame[12..14] == [0x08, 0x00]
        && frame[14] == 0x45
        && frame[23] == 17
        && frame[30..34] == dst.ip().octets()
        && frame[34..36] == LOCAL_PORT.to_be_bytes()
    {
        let mut h = [0u8; HEADERS];
        h.copy_from_slice(&frame[..HEADERS]);
        *TEMPLATE.lock() = Some(h);
    }
}

fn checksum(b: &[u8]) -> u16 {
    let mut s: u32 = b.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    !(s as u16)
}

/// Sends what the ring holds straight through the NIC (panic and fatal error path, other CPUs stopped).
pub fn panic_flush() {
    if crate::percpu::id() != 0 {
        return;
    }
    let (Some(t), nic) = (*TEMPLATE.lock(), NIC.load(Ordering::Acquire)) else { return };
    if nic.is_null() {
        return;
    }
    // SAFETY: set from the `Net` the main loop owns, which never goes away; the CPU that used it is this one,
    // interrupted by the panic.
    let dev = unsafe { &mut **nic };
    let mut id = u16::from_be_bytes([t[18], t[19]]);
    let mut chunk = [0u8; DATAGRAM];
    for _ in 0..64 {
        let n = take(&mut chunk);
        if n == 0 {
            break;
        }
        let (_, tx) = dev.queue(0);
        let mut tries = 0;
        while !tx.ready() && tries < 100 {
            crate::dev::nic::delay_us(100);
            tries += 1;
        }
        id = id.wrapping_add(1);
        tx.send(HEADERS + n, &mut |b| {
            b[..HEADERS].copy_from_slice(&t);
            b[HEADERS..].copy_from_slice(&chunk[..n]);
            b[16..18].copy_from_slice(&((20 + 8 + n) as u16).to_be_bytes());
            b[18..20].copy_from_slice(&id.to_be_bytes());
            b[24..26].fill(0);
            let c = checksum(&b[14..34]);
            b[24..26].copy_from_slice(&c.to_be_bytes());
            b[38..40].copy_from_slice(&((8 + n) as u16).to_be_bytes());
            // No UDP checksum (allowed over IPv4).
            b[40..42].fill(0);
        });
        dev.flush();
    }
    // Lets the NIC send before the machine resets.
    crate::dev::nic::delay_us(50_000);
}

/// # Safety
/// See [`super::force_unlock`].
pub unsafe fn force_unlock() {
    unsafe {
        LOG.force_unlock();
        TEMPLATE.force_unlock();
    }
}
