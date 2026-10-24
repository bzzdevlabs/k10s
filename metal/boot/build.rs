//! Build version, and the extra image keys built in (`K10BOOT_KEYS=FILE`: hex public keys, one per line), which are
//! the only ones besides the release key that k10boot trusts with Secure Boot on.
use std::{env, fs, path::PathBuf};

include!("../../scripts/buildinfo.rs");

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("keys.pub");
    println!("cargo:rerun-if-env-changed=K10BOOT_KEYS");
    let keys = match env::var("K10BOOT_KEYS") {
        Ok(p) if !p.is_empty() => {
            println!("cargo:rerun-if-changed={p}");
            fs::read_to_string(&p).unwrap_or_else(|e| panic!("K10BOOT_KEYS={p}: {e}"))
        }
        _ => String::new(),
    };
    fs::write(out, keys).unwrap();
    buildinfo::emit();
}
