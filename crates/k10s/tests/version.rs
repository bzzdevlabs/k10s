//! `--version`, `version`, `-V`, the startup line and `GET /_k10s/status` all report the embedded build version.

mod support;
use support::*;

use std::process::Command;

const FULL: &str = env!("K10S_VERSION_FULL");

/// `X.Y.Z[-dev.N]+<sha7>[.dirty]` or `X.Y.Z[-dev.N]+unknown`.
fn is_build_version(v: &str) -> bool {
    let Some((version, build)) = v.split_once('+') else { return false };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    let (core, pre) = version.split_once('-').map_or((version, None), |(c, p)| (c, Some(p)));
    let core_ok = core.split('.').count() == 3 && core.split('.').all(number);
    let pre_ok = pre.is_none_or(|p| p.strip_prefix("dev.").is_some_and(number));
    let (sha, dirty) = build.split_once('.').unwrap_or((build, ""));
    let build_ok = build == "unknown"
        || (sha.len() >= 7
            && sha.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && matches!(dirty, "" | "dirty"));
    core_ok && pre_ok && build_ok
}

#[test]
fn version_format() {
    for ok in ["0.1.0+1a2b3c4", "0.1.1-dev.45+1a2b3c4", "1.20.3-dev.0+unknown", "0.1.1-dev.45+1a2b3c4.dirty"] {
        assert!(is_build_version(ok), "{ok}");
    }
    for bad in ["0.1.0", "0.1+1a2b3c4", "0.1.0-rc.1+1a2b3c4", "0.1.0+xyz", "01.1.0+1a2b3c4", "0.1.0-dev.4+1a2b3c4.x"] {
        assert!(!is_build_version(bad), "{bad}");
    }
    assert!(is_build_version(FULL), "embedded version `{FULL}` is not X.Y.Z[-dev.N]+BUILD");
}

#[test]
fn cli_startup_line_and_status_report_the_build_version() {
    for arg in ["--version", "version", "-V"] {
        let out = Command::new(bin()).arg(arg).output().unwrap();
        assert!(out.status.success(), "k10s {arg}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), format!("k10s {FULL}"), "k10s {arg}");
    }
    let (_dir, node) = single("version", 1, &[]);
    let line = format!("k10s {FULL}: node `version` listening on");
    wait_for("startup line in the log", std::time::Duration::from_secs(10), || node.log().contains(&line));
    let status = node.status();
    assert!(status.contains(&format!("\"version\":\"{FULL}\"")), "{status}");
}
