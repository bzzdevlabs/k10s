//! Init configuration: `k10s.KEY=VALUE` on the kernel command line, over `KEY=VALUE` lines of `K10S-BOOT/k10s.conf`,
//! over the image's baked-in `/etc/k10s.conf`. Init consumes its own keys; every other key becomes `--KEY VALUE` of
//! `k10s node`.

use std::collections::{BTreeMap, HashSet};

/// Keys consumed by init (docs/OS.md); all others are node flags.
pub const INIT_KEYS: &[&str] = &[
    "net", "ip", "dns", "iface", "netwait", "ntp", "modules", "data", "bootwait", "grow", "watchdog", "prune_modules", "debug", "slot",
    "os_key", "os_trial",
];

/// The node's identity when `user` is not configured (no account database is needed for numeric ids).
pub const NODE_USER: &str = "65532:65532";

pub struct Config {
    keys: BTreeMap<String, String>,
    /// Keys from the kernel command line: no file overrides them.
    pinned: HashSet<String>,
}

impl Config {
    /// The `k10s.*` parameters of a kernel command line (`/proc/cmdline`), up to `--`.
    pub fn from_cmdline(cmdline: &str) -> Config {
        let mut c = Config { keys: BTreeMap::new(), pinned: HashSet::new() };
        for w in cmdline_words(cmdline) {
            if w == "--" {
                break;
            }
            if let Some(kv) = w.strip_prefix("k10s.") {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                c.keys.insert(k.to_string(), unquote(v).to_string());
                c.pinned.insert(k.to_string());
            }
        }
        c
    }

    /// Merges a configuration file: `KEY=VALUE` lines (optional `k10s.` prefix, `#` comments). A later file overrides
    /// an earlier one; the command line overrides both.
    pub fn merge(&mut self, text: &str) {
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (k, v) = line.split_once('=').unwrap_or((line, ""));
            let k = k.trim();
            let k = k.strip_prefix("k10s.").unwrap_or(k);
            if !self.pinned.contains(k) {
                self.keys.insert(k.to_string(), unquote(v.trim()).to_string());
            }
        }
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.keys.get(k).map(String::as_str)
    }

    /// A boolean key: present and not `0`, `false`, `no` or `off`.
    pub fn flag(&self, k: &str) -> bool {
        self.get(k).is_some_and(|v| !matches!(v, "0" | "false" | "no" | "off"))
    }

    /// Cluster mode with an advertised address derived from the interface (it changes with the DHCP lease).
    pub fn advertises_lease(&self) -> bool {
        ["gossip", "rpc", "join"].iter().any(|k| self.keys.contains_key(*k)) && !self.keys.contains_key("advertise")
    }

    /// `k10s node` arguments: appliance defaults (`--http 0.0.0.0:80`, `--user` [`NODE_USER`]), `--data DATA`, then
    /// every non-init key. `booleans` are the node's valueless flags.
    pub fn node_args(&self, data: &str, booleans: &[&str]) -> Vec<String> {
        let mut args = vec!["node".to_string(), "--data".into(), data.into()];
        if !self.keys.contains_key("http") {
            args.extend(["--http".into(), "0.0.0.0:80".into()]);
        }
        if !self.keys.contains_key("user") {
            args.extend(["--user".into(), NODE_USER.into()]);
        }
        for (k, v) in self.keys.iter().filter(|(k, _)| !INIT_KEYS.contains(&k.as_str())) {
            if booleans.contains(&k.as_str()) {
                if self.flag(k) {
                    args.push(format!("--{k}"));
                }
            } else {
                args.extend([format!("--{k}"), v.clone()]);
            }
        }
        args
    }

    /// The address the node serves HTTP on, for init's health probe (wildcards become loopback).
    pub fn http_probe_addr(&self) -> std::net::SocketAddr {
        let a = self.get("http").and_then(|h| h.parse().ok()).unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 80)));
        match a.ip() {
            std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::SocketAddr::from(([127, 0, 0, 1], a.port())),
            std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, a.port())),
            _ => a,
        }
    }
}

/// Splits a kernel command line like the kernel does: whitespace separates words, except inside double quotes.
fn cmdline_words(s: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let (mut start, mut quoted) = (None, false);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if let Some(st) = start.take() {
                    words.push(&s[st..i]);
                }
                continue;
            }
            _ => {}
        }
        start.get_or_insert(i);
    }
    if let Some(st) = start {
        words.push(&s[st..]);
    }
    words
}

fn unquote(v: &str) -> &str {
    v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOLS: &[&str] = &["log-stderr", "pin", "insecure-unauthenticated-cluster"];

    #[test]
    fn precedence_cmdline_over_files() {
        let mut c = Config::from_cmdline("console=ttyS0 k10s.name=\"edge one\" k10s.shards=2 quiet -- k10s.ignored=1\n");
        c.merge("# baked in\nshards=8\nk10s.mem = 512MiB\n\nlog-stderr\n");
        c.merge("mem=1GiB\n");
        assert_eq!(c.get("name"), Some("edge one"));
        assert_eq!(c.get("shards"), Some("2"));
        assert_eq!(c.get("mem"), Some("1GiB"));
        assert_eq!(c.get("ignored"), None);
        assert!(c.flag("log-stderr") && !c.flag("pin"));
    }

    #[test]
    fn node_args_defaults_and_booleans() {
        let mut c = Config::from_cmdline("k10s.ntp=none k10s.watchdog=1 k10s.pin=0");
        c.merge("log-stderr=1\nadmin-token-file=/boot/admin.key\n");
        let a = c.node_args("/data", BOOLS);
        assert_eq!(
            a,
            ["node", "--data", "/data", "--http", "0.0.0.0:80", "--user", NODE_USER, "--admin-token-file", "/boot/admin.key", "--log-stderr"]
        );
        let c = Config::from_cmdline("k10s.http=0.0.0.0:8080 k10s.user=1000:1000 k10s.join=10.0.0.2:7946");
        let a = c.node_args("/data", BOOLS);
        assert!(!a.contains(&"0.0.0.0:80".to_string()) && a.contains(&"1000:1000".to_string()));
        assert!(c.advertises_lease());
        assert_eq!(c.http_probe_addr().to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn words() {
        assert_eq!(cmdline_words(" a  b=\"c d\" \"e f\"\n"), ["a", "b=\"c d\"", "\"e f\""]);
    }
}
