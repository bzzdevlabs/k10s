// Build-script helper: `include!("../../scripts/buildinfo.rs")`, then `buildinfo::emit()`. Runs scripts/version.sh
// (K10S_BUILD_VERSION/K10S_BUILD_COMMIT win) and sets K10S_VERSION (0.1.1-dev.45), K10S_BUILD (1a2b3c4[.dirty]),
// K10S_VERSION_FULL (0.1.1-dev.45+1a2b3c4) and K10S_TARGET for env!(); fails if the two workspace versions differ.
#[allow(dead_code)]
mod buildinfo {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// [workspace.package] version of one of our Cargo.toml files.
    fn workspace_version(manifest: &Path) -> Option<String> {
        let text = std::fs::read_to_string(manifest).ok()?;
        let mut in_section = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_section = line.split('#').next().map(str::trim) == Some("[workspace.package]");
            } else if in_section && line.starts_with("version") {
                let v = line.split_once('=')?.1.trim();
                return Some(v.split('"').nth(1)?.to_string());
            }
        }
        None
    }

    fn repo_root() -> PathBuf {
        // Every caller is two levels below the repository root.
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..")
    }

    pub fn emit() {
        let root = repo_root();
        let pkg = env::var("CARGO_PKG_VERSION").unwrap();
        println!("cargo:rerun-if-env-changed=K10S_BUILD_VERSION");
        println!("cargo:rerun-if-env-changed=K10S_BUILD_COMMIT");

        // Both workspaces must carry the same X.Y.Z.
        let main = root.join("Cargo.toml");
        let metal = root.join("metal/Cargo.toml");
        for m in [&main, &metal] {
            if m.exists() {
                println!("cargo:rerun-if-changed={}", m.display());
            }
        }
        let (vm, vk) = (workspace_version(&main), workspace_version(&metal));
        if vm.is_some() && vk.is_some() && vm != vk {
            panic!(
                "k10s version mismatch: Cargo.toml says {:?} but metal/Cargo.toml says {:?}; give both workspaces the \
                 same [workspace.package] version",
                vm.unwrap(),
                vk.unwrap()
            );
        }

        let mut version = String::new();
        let mut build = String::new();
        if let Ok(v) = env::var("K10S_BUILD_VERSION") {
            if !v.is_empty() {
                let (v, meta) = v.split_once('+').map_or((v.as_str(), ""), |(a, b)| (a, b));
                version = v.to_string();
                build =
                    env::var("K10S_BUILD_COMMIT").ok().filter(|c| !c.is_empty()).unwrap_or_else(|| meta.to_string());
            }
        }
        let script = root.join("scripts/version.sh");
        if version.is_empty() && script.exists() {
            let out = Command::new("bash").arg(&script).args(["--env", "--watch"]).output();
            match out {
                Ok(o) if o.status.success() => {
                    for line in String::from_utf8_lossy(&o.stderr).lines() {
                        println!("cargo:warning={line}"); // e.g. shallow clone
                    }
                    for line in String::from_utf8_lossy(&o.stdout).lines() {
                        if let Some(v) = line.strip_prefix("K10S_BUILD_VERSION=") {
                            version = v.to_string();
                        } else if let Some(c) = line.strip_prefix("K10S_BUILD_COMMIT=") {
                            build = c.to_string();
                        } else if line.starts_with('/') {
                            println!("cargo:rerun-if-changed={line}");
                        }
                    }
                }
                Ok(o) => panic!("{} failed:\n{}", script.display(), String::from_utf8_lossy(&o.stderr)),
                Err(e) => println!("cargo:warning=cannot run {} ({e}): version {pkg}-dev.0+unknown", script.display()),
            }
        }
        if version.is_empty() {
            version = format!("{pkg}-dev.0");
        }
        if build.is_empty() {
            build = "unknown".into();
        }
        // X.Y must match the crate (Z may be the next patch after a release).
        let core = version.split('-').next().unwrap_or_default();
        if !core.split('.').take(2).eq(pkg.split('.').take(2)) {
            println!("cargo:warning=k10s build version {version} does not match the crate version {pkg}");
        }
        println!("cargo:rustc-env=K10S_VERSION={version}");
        println!("cargo:rustc-env=K10S_BUILD={build}");
        println!("cargo:rustc-env=K10S_VERSION_FULL={version}+{build}");
        println!("cargo:rustc-env=K10S_TARGET={}", env::var("TARGET").unwrap_or_default());
    }
}
