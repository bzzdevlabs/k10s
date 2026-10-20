// Build-script helper: `include!` it, call `buildinfo::emit()`, read `env!("K10S_VERSION_FULL")` (scripts/version.sh).
mod buildinfo {
    use std::{env, path::Path, process::Command};

    pub fn emit() {
        println!("cargo:rerun-if-env-changed=K10S_BUILD_VERSION");
        println!("cargo:rerun-if-env-changed=K10S_BUILD_COMMIT");
        // Own edits rebuild this crate anyway, so `.dirty` is refreshed then; other changes once HEAD moves.
        println!("cargo:rerun-if-changed=src");
        let script = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../scripts/version.sh");
        let full = match Command::new("bash").arg(&script).args(["--full", "--watch"]).output() {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stderr).lines().for_each(|l| println!("cargo:warning={l}"));
                let out = String::from_utf8_lossy(&o.stdout).into_owned();
                let mut lines = out.lines();
                let full = lines.next().unwrap_or_default().to_owned();
                lines.for_each(|path| println!("cargo:rerun-if-changed={path}"));
                full
            }
            Ok(o) => panic!("{}: {}", script.display(), String::from_utf8_lossy(&o.stderr)),
            Err(e) => {
                println!("cargo:warning=cannot run {}: {e}", script.display());
                format!("{}-dev.0+unknown", env::var("CARGO_PKG_VERSION").unwrap())
            }
        };
        println!("cargo:rustc-env=K10S_VERSION_FULL={full}");
    }
}
