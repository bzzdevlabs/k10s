//! The ACPI Watchdog Action Table (WDAT, Microsoft "Hardware Watchdog Timers Design Specification"): the firmware
//! describes its watchdog as actions (reset the countdown, set the countdown period, start, stop, set the reboot
//! behavior, read and clear the "watchdog fired" status), each a list of register instructions — read a value
//! and compare it, read the countdown, write a value, write the countdown — on I/O ports or memory, with masks and
//! an optional preserve-the-other-bits flag. No AML is involved.

use alloc::vec::Vec;

use super::{Platform, Watchdog};

pub const RESET: u8 = 1;
pub const QUERY_CURRENT_COUNTDOWN: u8 = 4;
pub const QUERY_COUNTDOWN: u8 = 5;
pub const SET_COUNTDOWN: u8 = 6;
pub const QUERY_RUNNING: u8 = 8;
pub const SET_RUNNING: u8 = 9;
pub const QUERY_STOPPED: u8 = 10;
pub const SET_STOPPED: u8 = 11;
pub const QUERY_REBOOT: u8 = 14;
pub const SET_REBOOT: u8 = 15;
pub const QUERY_SHUTDOWN: u8 = 16;
pub const SET_SHUTDOWN: u8 = 17;
pub const QUERY_STATUS: u8 = 32;
pub const SET_STATUS: u8 = 33;

pub const READ_VALUE: u8 = 0;
pub const READ_COUNTDOWN: u8 = 1;
pub const WRITE_VALUE: u8 = 2;
pub const WRITE_COUNTDOWN: u8 = 3;
pub const PRESERVE: u8 = 0x80;

pub const FLAG_ENABLED: u8 = 1 << 0;
pub const SPACE_MEMORY: u8 = 0;
pub const SPACE_IO: u8 = 1;

const HEADER: usize = 36;
const ENTRY: usize = 24;

/// A generic address structure (ACPI GAS) as WDAT entries use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg {
    pub space: u8,
    pub bit_width: u8,
    pub bit_offset: u8,
    /// 1 = byte, 2 = word, 3 = dword (0: from the bit width).
    pub access: u8,
    pub addr: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub action: u8,
    pub instruction: u8,
    pub reg: Reg,
    pub value: u32,
    pub mask: u32,
}

#[derive(Debug)]
pub struct Wdat {
    /// Milliseconds per countdown unit.
    pub period_ms: u32,
    pub max_count: u32,
    pub min_count: u32,
    pub flags: u8,
    pub entries: Vec<Entry>,
    fired: bool,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl Reg {
    fn bytes(&self) -> Option<u8> {
        let a = match self.access {
            0 => match self.bit_width {
                8 => 1,
                16 => 2,
                32 => 3,
                _ => return None,
            },
            a => a,
        };
        match a {
            1 => Some(1),
            2 => Some(2),
            3 => Some(4),
            _ => None,
        }
    }
    fn read(&self, p: &dyn Platform) -> u32 {
        match (self.space, self.bytes()) {
            (SPACE_IO, Some(1)) => p.inb(self.addr as u16) as u32,
            (SPACE_IO, Some(2)) => p.inw(self.addr as u16) as u32,
            (SPACE_IO, Some(4)) => p.inl(self.addr as u16),
            (SPACE_MEMORY, Some(1)) => p.mmio_read8(self.addr) as u32,
            (SPACE_MEMORY, Some(2)) => p.mmio_read16(self.addr) as u32,
            (SPACE_MEMORY, Some(4)) => p.mmio_read32(self.addr),
            _ => 0,
        }
    }
    fn write(&self, p: &dyn Platform, v: u32) {
        match (self.space, self.bytes()) {
            (SPACE_IO, Some(1)) => p.outb(self.addr as u16, v as u8),
            (SPACE_IO, Some(2)) => p.outw(self.addr as u16, v as u16),
            (SPACE_IO, Some(4)) => p.outl(self.addr as u16, v),
            (SPACE_MEMORY, Some(1)) => p.mmio_write8(self.addr, v as u8),
            (SPACE_MEMORY, Some(2)) => p.mmio_write16(self.addr, v as u16),
            (SPACE_MEMORY, Some(4)) => p.mmio_write32(self.addr, v),
            _ => {}
        }
    }
}

impl Wdat {
    /// Parses the table (header included). Entries on address spaces other than memory and I/O, or with an access
    /// size other than byte, word or dword, make the table unusable.
    pub fn parse(t: &[u8]) -> Result<Wdat, &'static str> {
        if t.len() < HEADER + 32 || &t[..4] != b"WDAT" {
            return Err("not a WDAT table");
        }
        let n = u32_at(t, 64) as usize;
        let first = HEADER + 32;
        if t.len() < first + n * ENTRY {
            return Err("WDAT shorter than its entries");
        }
        let mut entries = Vec::with_capacity(n);
        for e in t[first..first + n * ENTRY].chunks_exact(ENTRY) {
            let reg = Reg { space: e[4], bit_width: e[5], bit_offset: e[6], access: e[7], addr: u64::from_le_bytes(e[8..16].try_into().unwrap()) };
            if !matches!(reg.space, SPACE_MEMORY | SPACE_IO) || reg.bytes().is_none() || reg.bit_offset >= 32 {
                return Err("WDAT register outside memory and I/O space, or of an unsupported width");
            }
            entries.push(Entry { action: e[0], instruction: e[1], reg, value: u32_at(e, 16), mask: u32_at(e, 20) });
        }
        Ok(Wdat {
            period_ms: u32_at(t, 48),
            max_count: u32_at(t, 52),
            min_count: u32_at(t, 56),
            flags: t[60],
            entries,
            fired: false,
        })
    }

    /// The memory ranges its registers use (for the caller to map).
    pub fn memory(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.entries.iter().filter(|e| e.reg.space == SPACE_MEMORY).map(|e| (e.reg.addr, e.reg.bytes().unwrap_or(4) as u64))
    }

    pub fn has(&self, action: u8) -> bool {
        self.entries.iter().any(|e| e.action == action)
    }

    /// Runs `action` with `param` (for WRITE_COUNTDOWN); returns the last read result (READ_VALUE: 1 if equal;
    /// READ_COUNTDOWN: the value), `None` if the table has no such action.
    pub fn run(&self, p: &dyn Platform, action: u8, param: u32) -> Option<u32> {
        let mut ret = None;
        for e in self.entries.iter().filter(|e| e.action == action) {
            let r = &e.reg;
            let shift = r.bit_offset as u32;
            let out = |v: u32| {
                let mut x = (v & e.mask) << shift;
                if e.instruction & PRESERVE != 0 {
                    x |= r.read(p) & !(e.mask << shift);
                }
                r.write(p, x);
            };
            match e.instruction & !PRESERVE {
                READ_VALUE => ret = Some(((r.read(p) >> shift) & e.mask == e.value) as u32),
                READ_COUNTDOWN => ret = Some((r.read(p) >> shift) & e.mask),
                WRITE_VALUE => {
                    out(e.value);
                    ret.get_or_insert(0);
                }
                WRITE_COUNTDOWN => {
                    out(param);
                    ret.get_or_insert(0);
                }
                _ => {}
            }
        }
        ret
    }

    /// Checks the table can drive a watchdog (enabled, the actions to start, stop, pet and time it), reads and
    /// clears the "fired" status, stops the countdown.
    pub fn init(mut self, p: &dyn Platform) -> Result<Wdat, &'static str> {
        if self.flags & FLAG_ENABLED == 0 {
            return Err("watchdog disabled by the firmware (WDAT flags)");
        }
        if ![RESET, SET_COUNTDOWN, SET_RUNNING, SET_STOPPED].iter().all(|&a| self.has(a)) || self.period_ms == 0 {
            return Err("WDAT lacks a required action");
        }
        self.fired = self.run(p, QUERY_STATUS, 0) == Some(1);
        self.run(p, SET_STATUS, 0);
        self.run(p, SET_STOPPED, 0);
        Ok(self)
    }
}

impl Watchdog for Wdat {
    fn name(&self) -> &'static str {
        "ACPI WDAT"
    }
    fn arm(&mut self, p: &dyn Platform, seconds: u32) -> Result<u32, &'static str> {
        let count = (seconds.saturating_mul(1000) / self.period_ms).clamp(self.min_count.max(1), self.max_count.max(1));
        self.run(p, SET_REBOOT, 0);
        self.run(p, SET_COUNTDOWN, count);
        self.run(p, RESET, 0);
        self.run(p, SET_RUNNING, 0);
        if self.run(p, QUERY_RUNNING, 0) == Some(0) {
            return Err("the watchdog did not start");
        }
        Ok(count * self.period_ms / 1000)
    }
    fn pet(&mut self, p: &dyn Platform) {
        self.run(p, RESET, 0);
    }
    fn disarm(&mut self, p: &dyn Platform) {
        self.run(p, SET_STOPPED, 0);
    }
    fn fired_before(&self) -> bool {
        self.fired
    }
}
