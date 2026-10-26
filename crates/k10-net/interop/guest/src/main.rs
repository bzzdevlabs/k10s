//! `/init` of the interop guest: Linux's own TCP/IP stack on eth0 = 10.0.0.2/24 and fd00:9::2/64, with
//! test servers (both families):
//!
//! * 7001 echo; 7002 sink (16-byte header seed+len, verifies the stream, answers `OK n` or `BAD offset`);
//!   7003 source (header seed+len, sends the stream then FIN); 7004 reads one byte then resets (SO_LINGER 0);
//!   7005 reads to EOF, answers `BYE n`, closes; 7008 control: `connect ADDR PORT SEED LEN` makes Linux open a
//!   connection, send the stream, half-close, verify the stream `SEED+1` coming back, answer `DONE n OK|BAD`.
//! * UDP 7007 echo.

use k10_net::sim::stream_byte;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::thread;

fn main() {
    let _ = std::fs::create_dir("/proc");
    let src = CString::new("proc").unwrap();
    let dst = CString::new("/proc").unwrap();
    unsafe {
        libc::mount(src.as_ptr(), dst.as_ptr(), src.as_ptr(), 0, std::ptr::null());
    }
    load_modules();
    configure();
    for (port, f) in [
        (7001u16, echo as fn(TcpStream)),
        (7002, sink),
        (7003, source),
        (7004, reset),
        (7005, half_close),
        (7008, control),
    ] {
        let l = TcpListener::bind(SocketAddr::from(([0u16; 8], port))).expect("bind");
        thread::spawn(move || {
            for s in l.incoming().flatten() {
                thread::spawn(move || f(s));
            }
        });
    }
    thread::spawn(|| {
        let u = UdpSocket::bind(SocketAddr::from(([0u16; 8], 7007))).expect("udp");
        let mut b = [0u8; 2048];
        while let Ok((n, from)) = u.recv_from(&mut b) {
            let _ = u.send_to(&b[..n], from);
        }
    });
    println!("k10-net-guest ready");
    loop {
        thread::park();
    }
}

fn load_modules() {
    let mut mods: Vec<_> =
        std::fs::read_dir("/lib").map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    mods.sort();
    for m in mods {
        let f = std::fs::File::open(&m).expect("module");
        let p = CString::new("").unwrap();
        if unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), p.as_ptr(), 0) } != 0 {
            eprintln!("finit_module {}: {}", m.display(), std::io::Error::last_os_error());
        }
    }
}

fn ifreq(name: &str) -> libc::ifreq {
    let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, s) in r.ifr_name.iter_mut().zip(name.bytes()) {
        *d = s as libc::c_char;
    }
    r
}

fn sin(ip: [u8; 4]) -> libc::sockaddr {
    let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    s.sin_family = libc::AF_INET as libc::sa_family_t;
    s.sin_addr.s_addr = u32::from_ne_bytes(ip);
    unsafe { std::mem::transmute(s) }
}

#[repr(C)]
struct In6Ifreq {
    addr: [u8; 16],
    prefixlen: u32,
    ifindex: libc::c_int,
}

fn configure() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        for _ in 0..400 {
            let mut r = ifreq("eth0");
            if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut r) == 0 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        // No duplicate address detection delay on the test link.
        let _ = std::fs::write("/proc/sys/net/ipv6/conf/eth0/accept_dad", "0");
        let _ = std::fs::write("/proc/sys/net/ipv6/conf/eth0/accept_ra", "0");
        let mut r = ifreq("eth0");
        r.ifr_ifru.ifru_addr = sin([10, 0, 0, 2]);
        libc::ioctl(fd, libc::SIOCSIFADDR as _, &r);
        let mut r = ifreq("eth0");
        r.ifr_ifru.ifru_netmask = sin([255, 255, 255, 0]);
        libc::ioctl(fd, libc::SIOCSIFNETMASK as _, &r);
        for name in ["lo", "eth0"] {
            let mut r = ifreq(name);
            libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut r);
            r.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &r);
        }
        let mut r = ifreq("eth0");
        libc::ioctl(fd, libc::SIOCGIFINDEX as _, &mut r);
        let index = r.ifr_ifru.ifru_ifindex;
        libc::close(fd);
        let fd6 = libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0);
        let mut a = [0u8; 16];
        a[0] = 0xfd;
        a[1] = 0;
        a[3] = 9;
        a[15] = 2;
        let req = In6Ifreq { addr: a, prefixlen: 64, ifindex: index };
        if libc::ioctl(fd6, libc::SIOCSIFADDR as _, &req) != 0 {
            eprintln!("SIOCSIFADDR v6: {}", std::io::Error::last_os_error());
        }
        libc::close(fd6);
    }
}

fn header(s: &mut TcpStream) -> Option<(u64, u64)> {
    let mut h = [0u8; 16];
    s.read_exact(&mut h).ok()?;
    Some((u64::from_le_bytes(h[..8].try_into().unwrap()), u64::from_le_bytes(h[8..].try_into().unwrap())))
}

fn echo(mut s: TcpStream) {
    let mut b = vec![0u8; 65536];
    loop {
        match s.read(&mut b) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if s.write_all(&b[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

fn sink(mut s: TcpStream) {
    let Some((seed, len)) = header(&mut s) else { return };
    let mut b = vec![0u8; 65536];
    let mut off = 0u64;
    let mut bad = None;
    while off < len {
        match s.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if bad.is_none() {
                    bad = (0..n).find(|&i| b[i] != stream_byte(seed, off + i as u64)).map(|i| off + i as u64);
                }
                off += n as u64;
            }
        }
    }
    let msg = match (bad, off == len) {
        (None, true) => format!("OK {off}\n"),
        (Some(o), _) => format!("BAD {o}\n"),
        (None, false) => format!("SHORT {off}\n"),
    };
    let _ = s.write_all(msg.as_bytes());
}

fn fill(seed: u64, off: u64, b: &mut [u8]) {
    for (i, x) in b.iter_mut().enumerate() {
        *x = stream_byte(seed, off + i as u64);
    }
}

fn source(mut s: TcpStream) {
    let Some((seed, len)) = header(&mut s) else { return };
    let mut b = vec![0u8; 65536];
    let mut off = 0u64;
    while off < len {
        let n = (len - off).min(b.len() as u64) as usize;
        fill(seed, off, &mut b[..n]);
        if s.write_all(&b[..n]).is_err() {
            return;
        }
        off += n as u64;
    }
}

fn reset(mut s: TcpStream) {
    let mut b = [0u8; 1];
    let _ = s.read(&mut b);
    let l = libc::linger { l_onoff: 1, l_linger: 0 };
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &l as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}

fn half_close(mut s: TcpStream) {
    let mut b = vec![0u8; 65536];
    let mut n = 0usize;
    loop {
        match s.read(&mut b) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return,
        }
    }
    let _ = s.write_all(format!("BYE {n}\n").as_bytes());
}

fn control(s: TcpStream) {
    let mut out = s.try_clone().unwrap();
    for line in BufReader::new(s).lines().map_while(Result::ok) {
        let w: Vec<&str> = line.split_whitespace().collect();
        let reply = match w.as_slice() {
            ["connect", addr, port, seed, len] => connect_back(addr, port, seed, len),
            _ => "ERR\n".to_string(),
        };
        if out.write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}

fn connect_back(addr: &str, port: &str, seed: &str, len: &str) -> String {
    let (Ok(ip), Ok(port), Ok(seed), Ok(len)) =
        (addr.parse::<std::net::IpAddr>(), port.parse::<u16>(), seed.parse::<u64>(), len.parse::<u64>())
    else {
        return "ERR\n".into();
    };
    let mut s = match TcpStream::connect((ip, port)) {
        Ok(s) => s,
        Err(e) => return format!("DONE 0 CONNECT {e}\n"),
    };
    let mut r = s.try_clone().unwrap();
    let reader = thread::spawn(move || {
        let mut b = vec![0u8; 65536];
        let mut off = 0u64;
        let mut ok = true;
        loop {
            match r.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    ok &= (0..n).all(|i| b[i] == stream_byte(seed + 1, off + i as u64));
                    off += n as u64;
                }
            }
        }
        (off, ok)
    });
    let mut b = vec![0u8; 65536];
    let mut off = 0u64;
    while off < len {
        let n = (len - off).min(b.len() as u64) as usize;
        fill(seed, off, &mut b[..n]);
        if s.write_all(&b[..n]).is_err() {
            break;
        }
        off += n as u64;
    }
    let _ = s.shutdown(Shutdown::Write);
    let (n, ok) = reader.join().unwrap();
    format!("DONE {n} {}\n", if ok { "OK" } else { "BAD" })
}
