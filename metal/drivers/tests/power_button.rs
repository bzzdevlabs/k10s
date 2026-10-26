//! Control-method power buttons in hand-assembled AML: the GPE methods that notify a PNP0C0C device are found
//! across tables, nested devices and long packages; other notifies, values and method kinds are not.

use k10_drivers::power_button::{gpes, Gpe};

/// A package: PkgLength (one or two bytes, counting itself) and `body`.
fn pkg(body: &[u8]) -> Vec<u8> {
    let mut v = if body.len() + 1 <= 0x3f {
        vec![(body.len() + 1) as u8]
    } else {
        let n = body.len() + 2;
        vec![0x40 | (n & 0xf) as u8, (n >> 4) as u8]
    };
    v.extend_from_slice(body);
    v
}

/// A NameString: `\` for the root, then segments (dual or multi-name prefix when there are several).
fn name(path: &str) -> Vec<u8> {
    let (root, rest) = path.strip_prefix('\\').map_or((false, path), |r| (true, r));
    let segs: Vec<&str> = rest.split('.').collect();
    let mut v = if root { vec![b'\\'] } else { vec![] };
    match segs.len() {
        1 => {}
        2 => v.push(0x2e),
        n => v.extend([0x2f, n as u8]),
    }
    for s in segs {
        v.extend(format!("{s:_<4}").bytes());
    }
    v
}

fn device(path: &str, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0x5b, 0x82];
    v.extend(pkg(&[name(path), body.to_vec()].concat()));
    v
}

fn scope(path: &str, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0x10];
    v.extend(pkg(&[name(path), body.to_vec()].concat()));
    v
}

fn method(n: &str, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0x14];
    v.extend(pkg(&[name(n), vec![0], body.to_vec()].concat()));
    v
}

fn notify(target: &str, value: u8) -> Vec<u8> {
    [vec![0x86], name(target), vec![0x0a, value]].concat()
}

const HID_PWRB: &[u8] = b"\x08_HID\x0c\x41\xd0\x0c\x0c";
const HID_PWRB_STR: &[u8] = b"\x08_HID\x0dPNP0C0C\x00";
const HID_SLPB: &[u8] = b"\x08_HID\x0c\x41\xd0\x0c\x0e";

#[test]
fn the_qemu_test_table() {
    // What os/test.py and platform_test.py hand QEMU: Device (\_SB.PWRB), Scope (\_GPE) { Method (_E01) {...} }.
    let ssdt = [device("\\_SB.PWRB", HID_PWRB), scope("\\_GPE", &method("_E01", &notify("\\_SB.PWRB", 0x80)))].concat();
    assert_eq!(gpes(&[&ssdt]), [Gpe { number: 1, level: false }]);
    assert_eq!(gpes(&[&[]]), []);
}

#[test]
fn nested_devices_long_packages_and_several_tables() {
    let filler = [0xa3u8; 200]; // NoopOps: two-byte PkgLengths around them
    let pwrb = device("PWRB", &[HID_PWRB_STR, &filler].concat());
    let slpb = device("SLPB", HID_SLPB);
    let dsdt = scope("\\_SB", &device("PCI0", &device("LPCB", &[pwrb, slpb, filler.to_vec()].concat())));
    let gpe = scope(
        "\\_GPE",
        &[
            method("_L1D", &[filler.to_vec(), notify("\\_SB.PCI0.LPCB.SLPB", 0x80), notify("\\_SB.PCI0.LPCB.PWRB", 0x80)].concat()),
            method("_L0B", &notify("\\_SB.PCI0.LPCB.SLPB", 0x80)),
            method("_E2A", &notify("^PWRB", 0x02)),
            method("_Q12", &notify("PWRB", 0x80)),
        ]
        .concat(),
    );
    assert_eq!(gpes(&[&dsdt, &gpe]), [Gpe { number: 0x1d, level: true }]);
    // Only the sleep button: nothing.
    let only_sleep = scope("\\_SB", &device("SLPB", HID_SLPB));
    assert_eq!(gpes(&[&only_sleep, &gpe]), []);
}

#[test]
fn truncated_tables_do_not_panic() {
    let ssdt = [device("\\_SB.PWRB", HID_PWRB), scope("\\_GPE", &method("_E01", &notify("\\_SB.PWRB", 0x80)))].concat();
    for n in 0..ssdt.len() {
        let _ = gpes(&[&ssdt[..n]]);
    }
}
