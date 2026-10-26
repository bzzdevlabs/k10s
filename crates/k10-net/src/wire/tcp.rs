//! TCP headers and options (RFC 9293, 7323, 2018).

pub const HEADER_LEN: usize = 20;
pub const MAX_OPTIONS: usize = 40;
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;

/// Largest window shift (RFC 7323 §2.3).
pub const MAX_WSCALE: u8 = 14;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub mss: Option<u16>,
    pub wscale: Option<u8>,
    pub sack_perm: bool,
    /// (TSval, TSecr).
    pub ts: Option<(u32, u32)>,
    pub sack: [(u32, u32); 4],
    pub sack_len: u8,
}

impl Options {
    pub fn sack_blocks(&self) -> &[(u32, u32)] {
        &self.sack[..self.sack_len as usize]
    }

    /// Encoded length in bytes (a multiple of 4).
    pub fn len(&self) -> usize {
        let mut n = 0;
        if self.mss.is_some() {
            n += 4;
        }
        if self.ts.is_some() || self.sack_perm {
            n += if self.ts.is_some() { 12 } else { 4 };
        }
        if self.wscale.is_some() {
            n += 4;
        }
        if self.sack_len > 0 {
            n += 4 + 8 * self.sack_len as usize;
        }
        n
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// SACK blocks that fit next to the other options.
    pub fn max_sack_blocks(ts: bool) -> usize {
        if ts {
            3
        } else {
            4
        }
    }

    pub fn emit(&self, b: &mut [u8]) -> usize {
        let mut i = 0;
        if let Some(m) = self.mss {
            b[i..i + 4].copy_from_slice(&[2, 4, (m >> 8) as u8, m as u8]);
            i += 4;
        }
        match (self.ts, self.sack_perm) {
            (Some((v, e)), sp) => {
                if sp {
                    b[i..i + 2].copy_from_slice(&[4, 2]);
                } else {
                    b[i..i + 2].copy_from_slice(&[1, 1]);
                }
                b[i + 2..i + 4].copy_from_slice(&[8, 10]);
                b[i + 4..i + 8].copy_from_slice(&v.to_be_bytes());
                b[i + 8..i + 12].copy_from_slice(&e.to_be_bytes());
                i += 12;
            }
            (None, true) => {
                b[i..i + 4].copy_from_slice(&[1, 1, 4, 2]);
                i += 4;
            }
            (None, false) => {}
        }
        if let Some(w) = self.wscale {
            b[i..i + 4].copy_from_slice(&[1, 3, 3, w]);
            i += 4;
        }
        if self.sack_len > 0 {
            b[i..i + 4].copy_from_slice(&[1, 1, 5, 2 + 8 * self.sack_len]);
            i += 4;
            for &(l, r) in self.sack_blocks() {
                b[i..i + 4].copy_from_slice(&l.to_be_bytes());
                b[i + 4..i + 8].copy_from_slice(&r.to_be_bytes());
                i += 8;
            }
        }
        i
    }
}

/// Parses options; a malformed option ends parsing (the options before it are kept).
pub fn parse_options(mut b: &[u8]) -> Options {
    let mut o = Options::default();
    while let Some(&kind) = b.first() {
        match kind {
            0 => break,
            1 => {
                b = &b[1..];
                continue;
            }
            _ => {}
        }
        if b.len() < 2 {
            break;
        }
        let len = b[1] as usize;
        if len < 2 || len > b.len() {
            break;
        }
        let v = &b[2..len];
        match (kind, len) {
            (2, 4) => o.mss = Some(u16::from_be_bytes([v[0], v[1]])),
            (3, 3) => o.wscale = Some(v[0].min(MAX_WSCALE)),
            (4, 2) => o.sack_perm = true,
            (5, l) if l >= 10 && (l - 2).is_multiple_of(8) && l <= 34 => {
                for (k, blk) in v.as_chunks::<8>().0.iter().enumerate() {
                    o.sack[k] = (
                        u32::from_be_bytes([blk[0], blk[1], blk[2], blk[3]]),
                        u32::from_be_bytes([blk[4], blk[5], blk[6], blk[7]]),
                    );
                }
                o.sack_len = ((l - 2) / 8) as u8;
            }
            (8, 10) => {
                o.ts =
                    Some((u32::from_be_bytes([v[0], v[1], v[2], v[3]]), u32::from_be_bytes([v[4], v[5], v[6], v[7]])))
            }
            _ => {}
        }
        b = &b[len..];
    }
    o
}

#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub opts: Options,
    pub payload: &'a [u8],
}

impl Segment<'_> {
    pub fn has(&self, f: u8) -> bool {
        self.flags & f != 0
    }
    /// Sequence space the segment occupies (SYN and FIN count one each).
    pub fn seq_len(&self) -> u32 {
        self.payload.len() as u32 + self.has(SYN) as u32 + self.has(FIN) as u32
    }
}

/// Parses a header (checksum verified by the caller over the pseudo-header).
pub fn parse(b: &[u8]) -> Option<Segment<'_>> {
    if b.len() < HEADER_LEN {
        return None;
    }
    let off = (b[12] >> 4) as usize * 4;
    if off < HEADER_LEN || off > b.len() {
        return None;
    }
    let src_port = u16::from_be_bytes([b[0], b[1]]);
    let dst_port = u16::from_be_bytes([b[2], b[3]]);
    if src_port == 0 || dst_port == 0 {
        return None;
    }
    Some(Segment {
        src_port,
        dst_port,
        seq: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        ack: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        flags: b[13],
        window: u16::from_be_bytes([b[14], b[15]]),
        opts: parse_options(&b[HEADER_LEN..off]),
        payload: &b[off..],
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Header {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub opts: Options,
}

impl Header {
    pub fn len(&self) -> usize {
        HEADER_LEN + self.opts.len()
    }
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Writes the header and options with a zero checksum; returns the header length.
    pub fn emit(&self, b: &mut [u8]) -> usize {
        let olen = self.opts.emit(&mut b[HEADER_LEN..]);
        let hl = HEADER_LEN + olen;
        b[0..2].copy_from_slice(&self.src_port.to_be_bytes());
        b[2..4].copy_from_slice(&self.dst_port.to_be_bytes());
        b[4..8].copy_from_slice(&self.seq.to_be_bytes());
        b[8..12].copy_from_slice(&self.ack.to_be_bytes());
        b[12] = ((hl / 4) as u8) << 4;
        b[13] = self.flags;
        b[14..16].copy_from_slice(&self.window.to_be_bytes());
        b[16..20].copy_from_slice(&[0, 0, 0, 0]);
        hl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syn_options_roundtrip() {
        for (ts, sp, ws) in [(true, true, true), (false, true, true), (true, false, false), (false, false, true)] {
            let h = Header {
                src_port: 1000,
                dst_port: 80,
                seq: 1,
                ack: 0,
                flags: SYN,
                window: 65535,
                opts: Options {
                    mss: Some(1460),
                    wscale: ws.then_some(7),
                    sack_perm: sp,
                    ts: ts.then_some((5, 0)),
                    ..Default::default()
                },
            };
            let mut b = [0u8; 60];
            let n = h.emit(&mut b);
            assert_eq!(n % 4, 0);
            assert_eq!(n, h.len());
            let s = parse(&b[..n]).unwrap();
            assert_eq!(s.opts, h.opts);
            assert!(s.has(SYN) && s.payload.is_empty() && s.seq_len() == 1);
        }
    }

    #[test]
    fn sack_blocks_and_limits() {
        let mut o = Options { ts: Some((1, 2)), ..Default::default() };
        o.sack[..3].copy_from_slice(&[(10, 20), (30, 40), (50, 60)]);
        o.sack_len = 3;
        assert_eq!(o.len(), MAX_OPTIONS);
        let mut b = [0u8; 40];
        o.emit(&mut b);
        assert_eq!(parse_options(&b), o);
        // A malformed option stops parsing but keeps what came before.
        let raw = [2, 4, 5, 0xb4, 3, 1];
        let p = parse_options(&raw);
        assert_eq!((p.mss, p.wscale), (Some(1460), None));
        assert_eq!(parse_options(&[3, 3, 30]).wscale, Some(MAX_WSCALE));
    }
}
