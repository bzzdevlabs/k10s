//! Interface configuration through ioctls (no netlink library, no `ip`): link up, IPv4 address and netmask, default
//! route, `/etc/resolv.conf`. IPv6 addresses come from the kernel's SLAAC.

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::{Duration, Instant};

use super::Fd;

fn ifreq(name: &str) -> libc::ifreq {
    // SAFETY: an all-zero ifreq is valid.
    let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, s) in r.ifr_name.iter_mut().zip(name.bytes().take(libc::IFNAMSIZ - 1)) {
        *d = s as libc::c_char;
    }
    r
}

pub fn sockaddr_in(ip: Ipv4Addr, port: u16) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(ip.octets()) },
        sin_zero: [0; 8],
    }
}

fn sa(ip: Ipv4Addr) -> libc::sockaddr {
    // SAFETY: sockaddr_in and sockaddr have the same size and sockaddr has no invalid bit patterns.
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sockaddr_in(ip, 0)) }
}

fn ctl_sock() -> io::Result<Fd> {
    // SAFETY: plain syscall.
    let fd = Fd(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) });
    if fd.0 < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn ioctl<T>(s: &Fd, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: the callers pass the structure each request expects.
    if unsafe { libc::ioctl(s.0, req as _, arg as *mut T) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn link_up(name: &str) -> io::Result<()> {
    let s = ctl_sock()?;
    let mut r = ifreq(name);
    ioctl(&s, libc::SIOCGIFFLAGS, &mut r)?;
    // SAFETY: union member written by SIOCGIFFLAGS.
    unsafe { r.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
    ioctl(&s, libc::SIOCSIFFLAGS, &mut r)
}

/// The first Ethernet interface by name (or `want`), waiting up to `wait` for its driver.
pub fn pick_iface(want: Option<&str>, wait: Duration) -> Option<String> {
    let t = Instant::now();
    loop {
        if let Some(w) = want {
            if Path::new("/sys/class/net").join(w).exists() {
                return Some(w.to_string());
            }
        } else if let Ok(rd) = fs::read_dir("/sys/class/net") {
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != "lo" && fs::read_to_string(format!("/sys/class/net/{n}/type")).is_ok_and(|t| t.trim() == "1")) // ARPHRD_ETHER
                .collect();
            names.sort();
            if let Some(n) = names.into_iter().next() {
                return Some(n);
            }
        }
        if t.elapsed() >= wait {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The interface already has an IPv4 address (the kernel's `ip=` parameter).
pub fn has_address(ifc: &str) -> bool {
    let mut r = ifreq(ifc);
    ctl_sock().and_then(|s| ioctl(&s, libc::SIOCGIFADDR, &mut r)).is_ok()
}

pub fn mac(ifc: &str) -> [u8; 6] {
    let s = fs::read_to_string(format!("/sys/class/net/{ifc}/address")).unwrap_or_default();
    let mut mac = [0u8; 6];
    for (d, h) in mac.iter_mut().zip(s.trim().split(':')) {
        *d = u8::from_str_radix(h, 16).unwrap_or(0);
    }
    mac
}

/// `struct rtentry` (linux/route.h), declared here because libc exposes it only for some targets.
#[repr(C)]
struct RtEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_pad4: *mut libc::c_void,
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

/// Sets the interface's IPv4 address and prefix, and the default route through `gw`.
pub fn set_addr(ifc: &str, addr: Ipv4Addr, prefix: u8, gw: Option<Ipv4Addr>) -> io::Result<()> {
    let s = ctl_sock()?;
    let mut r = ifreq(ifc);
    r.ifr_ifru.ifru_addr = sa(addr);
    ioctl(&s, libc::SIOCSIFADDR, &mut r)?;
    let mut r = ifreq(ifc);
    r.ifr_ifru.ifru_netmask = sa(Ipv4Addr::from(u32::MAX.checked_shl(32 - prefix.min(32) as u32).unwrap_or(0)));
    ioctl(&s, libc::SIOCSIFNETMASK, &mut r)?;
    if let Some(gw) = gw {
        let dev = std::ffi::CString::new(ifc).map_err(io::Error::other)?;
        // SAFETY: an all-zero RtEntry is valid (null pointers, zero addresses).
        let mut rt: RtEntry = unsafe { std::mem::zeroed() };
        rt.rt_dst = sa(Ipv4Addr::UNSPECIFIED);
        rt.rt_genmask = sa(Ipv4Addr::UNSPECIFIED);
        rt.rt_gateway = sa(gw);
        rt.rt_flags = (libc::RTF_UP | libc::RTF_GATEWAY) as libc::c_ushort;
        rt.rt_dev = dev.as_ptr() as *mut libc::c_char;
        match ioctl(&s, libc::SIOCADDRT, &mut rt) {
            Err(e) if e.raw_os_error() != Some(libc::EEXIST) => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Removes the interface's IPv4 address (and with it the routes through it).
pub fn clear_addr(ifc: &str) -> io::Result<()> {
    let s = ctl_sock()?;
    let mut r = ifreq(ifc);
    r.ifr_ifru.ifru_addr = sa(Ipv4Addr::UNSPECIFIED);
    ioctl(&s, libc::SIOCSIFADDR, &mut r)
}

/// A static address: `A.B.C.D/P[,GATEWAY]` (prefix 24 when omitted).
pub fn parse_static(spec: &str) -> Result<(Ipv4Addr, u8, Option<Ipv4Addr>), String> {
    let bad = || format!("invalid ip `{spec}` (want A.B.C.D/P[,GATEWAY])");
    let (cidr, gw) = spec.split_once(',').map_or((spec, None), |(a, g)| (a, Some(g)));
    let (a, p) = cidr.split_once('/').unwrap_or((cidr, "24"));
    let addr = a.parse().map_err(|_| bad())?;
    let prefix = p.parse().ok().filter(|p| *p <= 32).ok_or_else(bad)?;
    let gw = gw.map(|g| g.parse().map_err(|_| bad())).transpose()?;
    Ok((addr, prefix, gw))
}

/// `/etc/resolv.conf` for the node's name lookups (musl reads it on every lookup).
pub fn write_resolv(dns: &[Ipv4Addr], search: Option<&str>) -> io::Result<()> {
    fs::write("/etc/resolv.conf", resolv_conf(dns, search))
}

fn resolv_conf(dns: &[Ipv4Addr], search: Option<&str>) -> String {
    let mut s = String::new();
    // Domain names from DHCP are untrusted input: keep only hostname characters.
    if let Some(d) = search.filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')) {
        s += &format!("search {d}\n");
    }
    for ip in dns.iter().take(3) {
        s += &format!("nameserver {ip}\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_spec() {
        assert_eq!(parse_static("10.0.0.5/16,10.0.0.1"), Ok((Ipv4Addr::new(10, 0, 0, 5), 16, Some(Ipv4Addr::new(10, 0, 0, 1)))));
        assert_eq!(parse_static("192.168.1.9"), Ok((Ipv4Addr::new(192, 168, 1, 9), 24, None)));
        assert!(parse_static("10.0.0.5/33").is_err() && parse_static("10.0.0/8").is_err() && parse_static("1.2.3.4/8,x").is_err());
    }

    #[test]
    fn resolv() {
        let d = [Ipv4Addr::new(10, 0, 2, 3), Ipv4Addr::new(1, 1, 1, 1)];
        assert_eq!(resolv_conf(&d, Some("lan")), "search lan\nnameserver 10.0.2.3\nnameserver 1.1.1.1\n");
        assert_eq!(resolv_conf(&d[..1], Some("x\nnameserver 6.6.6.6")), "nameserver 10.0.2.3\n");
    }
}
