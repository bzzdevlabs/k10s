//! `/init` of the Linux comparison guest (tools/linux-compare.sh). Static (musl), no shell, no busybox.
//!
//! * `k10mode=boot` (kernel command line → environment): print "linux-guest ready" on the console, power off.
//! * `k10mode=http`: load the virtio-net modules shipped in the initramfs (/lib/*.ko, in order), configure
//!   eth0 = 10.0.2.15/24 (QEMU slirp), print "linux-guest ready", serve HTTP/1.1 keep-alive on :80 with one thread
//!   per connection, answering every request with "hello from linux\n" — the same work as k10-metal's `hello`.

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

fn main() {
    let mode = std::env::var("k10mode").unwrap_or_else(|_| "boot".into());
    if mode == "http" {
        load_modules();
        configure_net();
        let l = TcpListener::bind("0.0.0.0:80").expect("bind :80");
        println!("linux-guest ready (http)");
        for s in l.incoming().flatten() {
            std::thread::spawn(move || serve(s));
        }
    }
    println!("linux-guest ready");
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
}

fn load_modules() {
    let mut mods: Vec<_> = std::fs::read_dir("/lib").map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    mods.sort(); // files are prefixed with their load order
    for m in mods {
        let f = std::fs::File::open(&m).expect("open module");
        let params = CString::new("").unwrap();
        let r = unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), params.as_ptr(), 0) };
        if r != 0 {
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

fn sockaddr(ip: [u8; 4]) -> libc::sockaddr {
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_addr.s_addr = u32::from_ne_bytes(ip);
    unsafe { std::mem::transmute(sin) }
}

fn configure_net() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        // The device may take a moment to appear after the module loads.
        for _ in 0..200 {
            let mut r = ifreq("eth0");
            if libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut r) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut r = ifreq("eth0");
        r.ifr_ifru.ifru_addr = sockaddr([10, 0, 2, 15]);
        if libc::ioctl(fd, libc::SIOCSIFADDR as libc::Ioctl, &r) != 0 {
            eprintln!("SIOCSIFADDR: {}", std::io::Error::last_os_error());
        }
        let mut r = ifreq("eth0");
        r.ifr_ifru.ifru_netmask = sockaddr([255, 255, 255, 0]);
        libc::ioctl(fd, libc::SIOCSIFNETMASK as libc::Ioctl, &r);
        for name in ["lo", "eth0"] {
            let mut r = ifreq(name);
            libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut r);
            r.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            if libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &r) != 0 {
                eprintln!("SIOCSIFFLAGS {name}: {}", std::io::Error::last_os_error());
            }
        }
        libc::close(fd);
    }
}

fn serve(mut s: TcpStream) {
    let _ = s.set_nodelay(true);
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        // Read one request head (+ Content-Length body).
        let (head_len, body_len, close) = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                let cl = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let close = head.lines().any(|l| l.starts_with("connection:") && l.contains("close"));
                break (p + 4, cl, close);
            }
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        while buf.len() < head_len + body_len {
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.drain(..head_len + body_len);
        let body = b"hello from linux\n";
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        if s.write_all(resp.as_bytes()).and_then(|_| s.write_all(body)).is_err() || close {
            return;
        }
    }
}
