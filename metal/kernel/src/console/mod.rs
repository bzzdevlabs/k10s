//! Consoles besides the serial port (serial.rs), fed by the same `kprint!`: the text console on the UEFI GOP
//! framebuffer k10boot hands over ([`fb`]; desktops and mini-PCs have no serial port), and a UDP log stream to a
//! collector ([`udp`], `k10.console=…,udp:IP[:PORT]`), which also carries a panic's last words.

pub mod fb;
pub mod font;
pub mod udp;

/// Everything printed goes here too (serial.rs, with the serial console's lock held).
pub fn write(s: &str) {
    fb::write(s);
    udp::write(s);
}

/// Frees the consoles' locks whoever holds them (panic path, other CPUs stopped).
///
/// # Safety
/// The previous holders must never run again.
pub unsafe fn force_unlock() {
    unsafe {
        fb::force_unlock();
        udp::force_unlock();
    }
}

/// The consoles `k10.console` asks for besides the serial port: its comma-separated items other than the serial
/// port's — `udp:A.B.C.D[:PORT]` (default port 6666) and `nofb` (no framebuffer console). Returns the serial part
/// (`None` if there is none) or an error naming the bad item.
pub fn configure(arg: Option<&str>) -> Result<Option<&str>, &'static str> {
    let Some(arg) = arg else { return Ok(None) };
    let mut serial = None;
    for item in arg.split(',').filter(|i| !i.is_empty()) {
        if let Some(dst) = item.strip_prefix("udp:") {
            let (ip, port) = match dst.split_once(':') {
                Some((ip, p)) => (ip, p.parse().map_err(|_| "udp:IP:PORT: bad port")?),
                None => (dst, udp::DEFAULT_PORT),
            };
            let ip: core::net::Ipv4Addr = ip.parse().map_err(|_| "udp:IP[:PORT]: want an IPv4 address")?;
            if udp::target().is_some() {
                return Err("at most one udp: console");
            }
            udp::configure(core::net::SocketAddrV4::new(ip, port));
        } else if item == "nofb" {
            fb::disable();
        } else if serial.replace(item).is_some() {
            return Err("at most one serial port");
        }
    }
    Ok(serial)
}
