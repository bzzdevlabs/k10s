//! COM1 (16550 UART at I/O port 0x3F8): the kernel console. Polled, no interrupts.
//!
//! Each port access is a VM exit under KVM (~7 µs to QEMU userspace on the test host), so output is batched:
//! when the Line Status Register reports the transmit FIFO empty (THRE with FIFOs enabled), up to 16 bytes — the
//! FIFO depth — are written with one `rep outsb`, which KVM forwards to QEMU as a single exit.

use core::fmt::{self, Write};

use crate::arch::{inb, outb};

use crate::sync::IrqLock;

const COM1: u16 = 0x3F8;
const FIFO_DEPTH: usize = 16;

pub struct Serial {
    buf: [u8; FIFO_DEPTH],
    len: usize,
}

pub static SERIAL: IrqLock<Serial> = IrqLock::new(Serial { buf: [0; FIFO_DEPTH], len: 0 });

pub fn init() {
    outb(COM1 + 1, 0x00); // IER: no interrupts
    outb(COM1 + 3, 0x80); // LCR: DLAB
    outb(COM1, 0x01); // divisor 1 = 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1
    outb(COM1 + 2, 0xC7); // FCR: enable + clear FIFOs, 14-byte trigger
    outb(COM1 + 4, 0x03); // MCR: DTR | RTS
}

impl Serial {
    fn put(&mut self, b: u8) {
        self.buf[self.len] = b;
        self.len += 1;
        if self.len == FIFO_DEPTH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        while inb(COM1 + 5) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        // SAFETY: rep outsb reads `len` bytes from `buf`.
        unsafe {
            core::arch::asm!(
                "rep outsb",
                in("dx") COM1,
                inout("rsi") self.buf.as_ptr() => _,
                inout("rcx") self.len => _,
                options(nostack, preserves_flags, readonly)
            );
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

pub fn _print(args: fmt::Arguments) {
    let mut s = SERIAL.lock();
    let _ = s.write_fmt(args);
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
