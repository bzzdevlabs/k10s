//! The kernel console: a 16550 UART, polled, no interrupts. COM1 (I/O port 0x3F8) from the first instruction; once
//! the ACPI tables are read, `k10.console=` or else the firmware's SPCR table may move it (servers redirect a BMC's
//! serial-over-LAN to COM2, some boards have only a memory-mapped UART).
//!
//! Each port access is a VM exit under KVM (~7 µs to QEMU userspace on the test host), so output is batched:
//! when the Line Status Register reports the transmit FIFO empty (THRE with FIFOs enabled), up to 16 bytes — the
//! FIFO depth — are written with one `rep outsb`, which KVM forwards to QEMU as a single exit (one byte at a time on a
//! 16450, which has no FIFO). A UART that stops reporting THRE (absent, or stuck) costs one bounded wait, long enough
//! for a 9600-baud FIFO to drain, then its output is dropped without waiting until it reports THRE again.

use core::fmt::{self, Write};

use crate::arch::{inb, outb};

use crate::sync::IrqLock;

const COM1: u16 = 0x3F8;
const FIFO_DEPTH: usize = 16;
/// LSR polls before a batch is dropped (a 16-byte FIFO drains in 1.4 ms at 115200 baud, 17 ms at 9600; a poll takes
/// ~1 µs on hardware).
const THRE_SPINS: u32 = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    None,
    Io(u16),
    /// Memory-mapped 16550 with registers `1 << shift` bytes apart, accessed 8 or 32 bits wide.
    Mmio { base: u64, shift: u8, wide: bool },
}

pub struct Serial {
    port: Port,
    /// Bytes written per THRE: 16 (16550 FIFO) or 1 (16450).
    fifo: usize,
    /// The last batch timed out: drop output until the UART reports THRE again.
    stuck: bool,
    buf: [u8; FIFO_DEPTH],
    len: usize,
}

pub static SERIAL: IrqLock<Serial> =
    IrqLock::new(Serial { port: Port::Io(COM1), fifo: FIFO_DEPTH, stuck: false, buf: [0; FIFO_DEPTH], len: 0 });

impl Port {
    fn read(&self, reg: u16) -> u8 {
        match *self {
            Port::None => 0xFF,
            Port::Io(p) => inb(p + reg),
            Port::Mmio { base, shift, wide } => unsafe {
                let a = base + ((reg as u64) << shift);
                if wide { core::ptr::read_volatile(a as *const u32) as u8 } else { core::ptr::read_volatile(a as *const u8) }
            },
        }
    }
    fn write(&self, reg: u16, v: u8) {
        match *self {
            Port::None => {}
            Port::Io(p) => outb(p + reg, v),
            Port::Mmio { base, shift, wide } => unsafe {
                let a = base + ((reg as u64) << shift);
                if wide { core::ptr::write_volatile(a as *mut u32, v as u32) } else { core::ptr::write_volatile(a as *mut u8, v) }
            },
        }
    }
    /// 8N1, FIFOs on, no interrupts; `baud` 0 keeps the divisor the firmware set.
    fn setup(&self, baud: u32) {
        self.write(1, 0x00); // IER: no interrupts
        if baud != 0 {
            let div = (115_200 / baud).max(1) as u16;
            self.write(3, 0x80); // LCR: DLAB
            self.write(0, div as u8);
            self.write(1, (div >> 8) as u8);
        }
        self.write(3, 0x03); // 8N1
        self.write(2, 0xC7); // FCR: enable + clear FIFOs, 14-byte trigger
        self.write(4, 0x03); // MCR: DTR | RTS
    }
}

pub fn init() {
    Port::Io(COM1).setup(115_200);
}

/// Moves the console to `port` (configured at `baud`, 0 = as the firmware left it; `fifo`: a 16550, not a 16450).
pub fn set_port(port: Port, baud: u32, fifo: bool) {
    if let Port::Mmio { base, .. } = port {
        if !crate::arch::paging::map_mmio(base, 4096) {
            return;
        }
    }
    let mut s = SERIAL.lock();
    s.flush();
    port.setup(baud);
    s.port = port;
    s.fifo = if fifo { FIFO_DEPTH } else { 1 };
    s.stuck = false;
}

/// The console the command line (`k10.console=com1..com4|io:0xPORT|mmio:0xADDR|mmio32:0xADDR|none`) or else the
/// ACPI SPCR table asks for: (port, baud, has a FIFO); `None`: keep COM1. Errors name the bad value.
pub fn choose(arg: Option<&str>, spcr: Option<crate::acpi::Spcr>) -> Result<Option<(Port, u32, bool)>, &'static str> {
    let hex = |v: &str| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok();
    if let Some(v) = arg {
        let port = match v {
            "com1" => Port::Io(0x3F8),
            "com2" => Port::Io(0x2F8),
            "com3" => Port::Io(0x3E8),
            "com4" => Port::Io(0x2E8),
            "none" => Port::None,
            _ => match v.split_once(':') {
                Some(("io", a)) => Port::Io(hex(a).filter(|&p| p <= 0xFFF8).ok_or("io: want a port number")? as u16),
                Some(("mmio", a)) => Port::Mmio { base: hex(a).ok_or("mmio: want an address")?, shift: 0, wide: false },
                Some(("mmio32", a)) => Port::Mmio { base: hex(a).ok_or("mmio32: want an address")?, shift: 2, wide: true },
                _ => return Err("want com1..com4, io:0xPORT, mmio:0xADDR, mmio32:0xADDR or none"),
            },
        };
        return Ok(Some((port, 115_200, true)));
    }
    let Some(s) = spcr else { return Ok(None) };
    // 16550 / 16450, or the 16550-compatible subtype with the register width in the address structure.
    if !matches!(s.kind, 0x00 | 0x01 | 0x12) || !s.base.valid() {
        return Ok(None);
    }
    let port = match s.base.space {
        crate::acpi::SPACE_IO => Port::Io(s.base.addr as u16),
        crate::acpi::SPACE_MEMORY => {
            let wide = s.base.bits == 32 || s.base.access == 3;
            Port::Mmio { base: s.base.addr, shift: if wide { 2 } else { 0 }, wide }
        }
        _ => return Ok(None),
    };
    Ok(Some((port, s.baud, s.kind != 0x01)))
}

impl Serial {
    fn put(&mut self, b: u8) {
        self.buf[self.len] = b;
        self.len += 1;
        if self.len == self.fifo {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        let mut spins = 0;
        while self.port.read(5) & 0x20 == 0 {
            spins += 1;
            if self.stuck || spins == THRE_SPINS {
                self.stuck = true;
                self.len = 0;
                return;
            }
            core::hint::spin_loop();
        }
        self.stuck = false;
        match self.port {
            // SAFETY: rep outsb reads `len` bytes from `buf`.
            Port::Io(p) => unsafe {
                core::arch::asm!(
                    "rep outsb",
                    in("dx") p,
                    inout("rsi") self.buf.as_ptr() => _,
                    inout("rcx") self.len => _,
                    options(nostack, preserves_flags, readonly)
                );
            },
            port => {
                for &b in &self.buf[..self.len] {
                    port.write(0, b);
                }
            }
        }
        self.len = 0;
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
        Ok(())
    }
}

/// The serial port and the other consoles (console/).
struct Tee<'a>(&'a mut Serial);

impl Write for Tee<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::console::write(s);
        self.0.write_str(s)
    }
}

pub fn _print(args: fmt::Arguments) {
    let mut s = SERIAL.lock();
    let _ = Tee(&mut s).write_fmt(args);
    s.flush();
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::serial::_print(format_args!("{}\n", format_args!($($arg)*))) };
}
