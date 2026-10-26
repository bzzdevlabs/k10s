//! UDP headers (RFC 768).

pub const HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct Datagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub checksum: u16,
    pub payload: &'a [u8],
}

/// Parses a UDP header; the length field must fit the IP payload (trailing bytes beyond it are ignored).
pub fn parse(b: &[u8]) -> Option<Datagram<'_>> {
    if b.len() < HEADER_LEN {
        return None;
    }
    let len = u16::from_be_bytes([b[4], b[5]]) as usize;
    if len < HEADER_LEN || len > b.len() {
        return None;
    }
    Some(Datagram {
        src_port: u16::from_be_bytes([b[0], b[1]]),
        dst_port: u16::from_be_bytes([b[2], b[3]]),
        checksum: u16::from_be_bytes([b[6], b[7]]),
        payload: &b[HEADER_LEN..len],
    })
}

/// Writes the header with a zero checksum; the caller fills it over the pseudo-header.
pub fn emit(b: &mut [u8], src_port: u16, dst_port: u16, payload_len: usize) {
    b[0..2].copy_from_slice(&src_port.to_be_bytes());
    b[2..4].copy_from_slice(&dst_port.to_be_bytes());
    b[4..6].copy_from_slice(&((HEADER_LEN + payload_len) as u16).to_be_bytes());
    b[6..8].copy_from_slice(&[0, 0]);
}
