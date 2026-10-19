//! Links the kernel with `linker.ld` (fixed load address, PVH note kept) and optionally embeds a bundle.
use std::{env, fs, path::PathBuf};

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let target = env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        println!("cargo:rustc-link-arg-bins=-T{}", dir.join("linker.ld").display());
    }
    println!("cargo:rerun-if-changed=linker.ld");

    // Optional bundle baked into the image (`K10_EMBED_BUNDLE=path`). An `-initrd` bundle given at boot wins.
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("embedded_bundle.bin");
    println!("cargo:rerun-if-env-changed=K10_EMBED_BUNDLE");
    let bytes = match env::var("K10_EMBED_BUNDLE") {
        Ok(p) if !p.is_empty() => {
            println!("cargo:rerun-if-changed={p}");
            fs::read(&p).unwrap_or_else(|e| panic!("K10_EMBED_BUNDLE={p}: {e}"))
        }
        _ => Vec::new(),
    };
    fs::write(out, bytes).unwrap();
}
