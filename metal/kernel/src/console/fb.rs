//! A text console on the linear framebuffer of the UEFI GOP mode k10boot left (module `k10s.framebuffer`; the
//! mode cannot change after ExitBootServices). 8×8 glyphs ([`super::font`]) scaled by an integer factor so that a
//! 1280×800 screen shows 80×40 characters, in cells of 8×10 scaled pixels (a blank pixel row above and below).
//! The framebuffer is mapped write-combining; nothing is read back from it: a copy of the text decides what to
//! redraw. When the cursor passes the last row the text jumps up by a quarter of the screen, and only cells whose
//! character changed are drawn again.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use k10_bootproto::{Framebuffer, PixelFormat};

use super::font::GLYPHS;
use crate::sync::IrqLock;

const FG: (u8, u8, u8) = (0xC0, 0xC0, 0xC0);
const BG: (u8, u8, u8) = (0x00, 0x00, 0x00);

struct Console {
    base: u64,
    stride: u32,
    fg: u32,
    bg: u32,
    scale: u32,
    cols: u32,
    rows: u32,
    x: u32,
    y: u32,
    /// The text, and what is on the screen.
    text: Vec<u8>,
    shown: Vec<u8>,
}

static FB: IrqLock<Option<Console>> = IrqLock::new(None);
static DISABLED: AtomicBool = AtomicBool::new(false);

/// `k10.console=…,nofb`.
pub fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

/// A pixel value in the framebuffer's format.
fn pixel(f: &PixelFormat, (r, g, b): (u8, u8, u8)) -> u32 {
    let (r, g, b) = (r as u32, g as u32, b as u32);
    match *f {
        PixelFormat::Rgbx => r | g << 8 | b << 16,
        PixelFormat::Bgrx => b | g << 8 | r << 16,
        PixelFormat::Bitmask { red, green, blue, .. } => {
            // Scales an 8-bit channel into the mask's bits.
            let put = |v: u32, mask: u32| {
                if mask == 0 {
                    return 0;
                }
                let shift = mask.trailing_zeros();
                let bits = (mask >> shift).count_ones();
                ((v >> (8u32.saturating_sub(bits))) << shift) & mask
            };
            put(r, red) | put(g, green) | put(b, blue)
        }
    }
}

/// Takes over the framebuffer described by module `bytes`; returns (width, height, columns, rows, cell width,
/// cell height) for the boot log.
pub fn init(bytes: &[u8]) -> Option<(u32, u32, u32, u32, u32, u32)> {
    if DISABLED.load(Ordering::Relaxed) {
        return None;
    }
    let f = Framebuffer::decode(bytes)?;
    let len = f.stride as u64 * f.height as u64 * 4;
    if f.width == 0 || f.height == 0 || f.stride < f.width || (f.size != 0 && f.size < len) || f.base == 0 {
        return None;
    }
    if !crate::arch::paging::map_wc(f.base, len) {
        return None;
    }
    let scale = (f.width / 640).min(f.height / 400).clamp(1, 4);
    let (cw, ch) = (8 * scale, 10 * scale);
    let (cols, rows) = (f.width / cw, f.height / ch);
    if cols == 0 || rows == 0 {
        return None;
    }
    let mut c = Console {
        base: f.base,
        stride: f.stride,
        fg: pixel(&f.format, FG),
        bg: pixel(&f.format, BG),
        scale,
        cols,
        rows,
        x: 0,
        y: 0,
        text: vec![b' '; (cols * rows) as usize],
        shown: vec![b' '; (cols * rows) as usize],
    };
    c.clear(f.width, f.height);
    *FB.lock() = Some(c);
    Some((f.width, f.height, cols, rows, cw, ch))
}

impl Console {
    fn put_px(&self, x: u32, y: u32, v: u32) {
        // SAFETY: inside the mapped framebuffer (x < width <= stride, y < height).
        unsafe { core::ptr::write_volatile((self.base + (y as u64 * self.stride as u64 + x as u64) * 4) as *mut u32, v) }
    }

    fn clear(&mut self, w: u32, h: u32) {
        for y in 0..h {
            for x in 0..w {
                self.put_px(x, y, self.bg);
            }
        }
    }

    fn draw(&mut self, col: u32, row: u32) {
        let i = (row * self.cols + col) as usize;
        let ch = self.text[i];
        if self.shown[i] == ch {
            return;
        }
        self.shown[i] = ch;
        let s = self.scale;
        let (x0, y0) = (col * 8 * s, row * 10 * s);
        let g = &GLYPHS[(ch & 0x7F) as usize];
        for py in 0..10 * s {
            // Glyph rows 0-7 sit between one blank scaled row above and one below.
            let bits = match py / s {
                0 | 9 => 0,
                r => g[(r - 1) as usize],
            };
            for px in 0..8 * s {
                let on = bits >> (px / s) & 1 != 0;
                self.put_px(x0 + px, y0 + py, if on { self.fg } else { self.bg });
            }
        }
    }

    fn newline(&mut self) {
        self.x = 0;
        self.y += 1;
        if self.y < self.rows {
            return;
        }
        let jump = (self.rows / 4).max(1);
        let keep = ((self.rows - jump) * self.cols) as usize;
        self.text.copy_within((jump * self.cols) as usize.., 0);
        self.text[keep..].fill(b' ');
        self.y = self.rows - jump;
        for row in 0..self.rows {
            for col in 0..self.cols {
                self.draw(col, row);
            }
        }
    }

    fn put(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.x = 0,
            b'\t' => {
                for _ in 0..8 - self.x % 8 {
                    self.put(b' ');
                }
            }
            // UTF-8 continuation bytes: the leading byte already drew a '?'.
            0x80..=0xBF => {}
            _ => {
                let c = if (0x20..0x7F).contains(&b) { b } else { b'?' };
                if self.x == self.cols {
                    self.newline();
                }
                let i = (self.y * self.cols + self.x) as usize;
                self.text[i] = c;
                self.draw(self.x, self.y);
                self.x += 1;
            }
        }
    }
}

pub fn write(s: &str) {
    let mut g = FB.lock();
    if let Some(c) = g.as_mut() {
        for b in s.bytes() {
            c.put(b);
        }
    }
}

/// # Safety
/// See [`super::force_unlock`].
pub unsafe fn force_unlock() {
    unsafe { FB.force_unlock() }
}
