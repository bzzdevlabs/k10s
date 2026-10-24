//! External disassembler for the assurance tests: GNU objdump (Intel syntax for x86-64; AArch64 as well),
//! sharing no code with the JIT.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// objdump (`K10_OBJDUMP`, else /usr/bin/objdump), its version logged once. Missing: `None` (the test skips), or a
/// panic when `CI` or `K10_REQUIRE_OBJDUMP` is set.
pub fn tool(test: &str) -> Option<PathBuf> {
    let p = std::env::var_os("K10_OBJDUMP").map_or_else(|| PathBuf::from("/usr/bin/objdump"), PathBuf::from);
    if !p.exists() {
        let required = std::env::var_os("CI").is_some() || std::env::var_os("K10_REQUIRE_OBJDUMP").is_some();
        assert!(!required, "{test}: {} not found (required by CI / K10_REQUIRE_OBJDUMP)", p.display());
        eprintln!("{test}: {} not found: skipped (set K10_OBJDUMP)", p.display());
        return None;
    }
    static VERSION: OnceLock<String> = OnceLock::new();
    let v = VERSION.get_or_init(|| {
        let out = Command::new(&p).arg("--version").output().expect("objdump --version");
        String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("?").to_string()
    });
    eprintln!("{test}: {} = {v}", p.display());
    Some(p)
}

/// One decoded instruction.
#[derive(Clone, Debug)]
pub struct Insn {
    /// Offset in the disassembled blob.
    pub addr: u64,
    pub bytes: Vec<u8>,
    /// The mnemonic with its prefixes (`rep stos`, `rex.W push`).
    pub mnemonic: String,
    /// Operands as printed (`rax,QWORD PTR [rbp+0x0]`), without symbol annotations; empty if none.
    pub operands: String,
    /// Target of a trailing `# 0x…` comment (RIP-relative operands), wrapped to 64 bits.
    pub comment_target: Option<u64>,
}

impl Insn {
    pub fn end(&self) -> u64 {
        self.addr + self.bytes.len() as u64
    }

    /// `mnemonic operands`, single-spaced.
    pub fn text(&self) -> String {
        if self.operands.is_empty() {
            self.mnemonic.clone()
        } else {
            format!("{} {}", self.mnemonic, self.operands)
        }
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Disassembles every blob (one objdump run per 400 blobs); addresses are relative to each blob.
pub fn disassemble_many(objdump: &std::path::Path, blobs: &[&[u8]]) -> Vec<Vec<Insn>> {
    let dir =
        std::env::temp_dir().join(format!("k10-xdis-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::with_capacity(blobs.len());
    for (chunk_i, chunk) in blobs.chunks(400).enumerate() {
        let mut files = Vec::new();
        for (i, b) in chunk.iter().enumerate() {
            let p = dir.join(format!("b{chunk_i}_{i}.bin"));
            std::fs::File::create(&p).unwrap().write_all(b).unwrap();
            files.push(p);
        }
        let res = Command::new(objdump)
            .args(["-D", "-b", "binary", "-m", "i386:x86-64", "-M", "intel", "--insn-width=16", "-z"])
            .args(&files)
            .output()
            .expect("running objdump");
        assert!(res.status.success(), "objdump failed: {}", String::from_utf8_lossy(&res.stderr));
        let mut per_file: Vec<Vec<Insn>> = vec![Vec::new(); chunk.len()];
        let mut cur: Option<usize> = None;
        for line in String::from_utf8(res.stdout).unwrap().lines() {
            if let Some(name) = line.strip_suffix(":     file format binary") {
                cur = Some(files.iter().position(|f| f.to_str() == Some(name)).expect("unknown objdump file header"));
            } else if let (Some(i), Some(insn)) = (cur, parse_line(line)) {
                per_file[i].push(insn);
            }
        }
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        out.extend(per_file);
    }
    let _ = std::fs::remove_dir(&dir);
    out
}

/// AArch64 disassembly of every blob by objdump (one run per 400 blobs): `(offset, word, text)` per instruction,
/// the text as `mnemonic op1, op2, …` without comments (`//`) and symbol annotations.
pub fn disassemble_a64(objdump: &std::path::Path, blobs: &[&[u8]]) -> Vec<Vec<(u64, u32, String)>> {
    let dir =
        std::env::temp_dir().join(format!("k10-xdis-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::with_capacity(blobs.len());
    for (chunk_i, chunk) in blobs.chunks(400).enumerate() {
        let mut files = Vec::new();
        for (i, b) in chunk.iter().enumerate() {
            let p = dir.join(format!("b{chunk_i}_{i}.bin"));
            std::fs::File::create(&p).unwrap().write_all(b).unwrap();
            files.push(p);
        }
        let res = Command::new(objdump)
            .args(["-D", "-b", "binary", "-m", "aarch64", "-z"])
            .args(&files)
            .output()
            .expect("running objdump");
        assert!(res.status.success(), "objdump failed: {}", String::from_utf8_lossy(&res.stderr));
        let mut per_file: Vec<Vec<(u64, u32, String)>> = vec![Vec::new(); chunk.len()];
        let mut cur: Option<usize> = None;
        for line in String::from_utf8(res.stdout).unwrap().lines() {
            if let Some(name) = line.strip_suffix(":     file format binary") {
                cur = Some(files.iter().position(|f| f.to_str() == Some(name)).expect("unknown objdump file header"));
            } else if let (Some(i), Some(insn)) = (cur, parse_line_a64(line)) {
                per_file[i].push(insn);
            }
        }
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        out.extend(per_file);
    }
    let _ = std::fs::remove_dir(&dir);
    out
}

/// `   1c:\t91004020 \tadd\tx0, x1, #0x10  // comment` → `(0x1c, 0x91004020, "add x0, x1, #0x10")`.
fn parse_line_a64(line: &str) -> Option<(u64, u32, String)> {
    let mut parts = line.splitn(3, '\t');
    let addr = u64::from_str_radix(parts.next()?.trim().strip_suffix(':')?, 16).ok()?;
    let word = u32::from_str_radix(parts.next()?.trim(), 16).ok()?;
    let rest = parts.next().unwrap_or("");
    let rest = rest.split("//").next().unwrap_or("");
    let rest = strip_symbols(rest.trim());
    let (m, ops) = rest.split_once(char::is_whitespace).unwrap_or((rest.as_str(), ""));
    let ops = ops.split(',').map(str::trim).collect::<Vec<_>>().join(", ");
    Some((addr, word, if ops.is_empty() { m.to_string() } else { format!("{m} {ops}") }))
}

/// `   1c:\t66 48 0f 6e c0      \tmovq   xmm0,rax` → Insn.
fn parse_line(line: &str) -> Option<Insn> {
    let mut parts = line.splitn(3, '\t');
    let addr = u64::from_str_radix(parts.next()?.trim().strip_suffix(':')?, 16).ok()?;
    let bytes: Vec<u8> =
        parts.next()?.split_whitespace().map(|b| u8::from_str_radix(b, 16)).collect::<Result<_, _>>().ok()?;
    let rest = strip_symbols(parts.next().unwrap_or("").trim());
    let (body, comment) = match rest.split_once(" # ") {
        Some((b, c)) => (b.trim(), Some(c.trim())),
        None => (rest.as_str(), None),
    };
    let comment_target = comment
        .and_then(|c| c.split_whitespace().next())
        .and_then(|c| c.strip_prefix("0x"))
        .and_then(|h| u64::from_str_radix(h, 16).ok());
    let words: Vec<&str> = body.split_whitespace().collect();
    // The mnemonic is the first non-prefix word, with the prefixes before it.
    let main = words.iter().position(|w| !is_prefix_word(w)).map_or(words.len(), |i| i + 1);
    Some(Insn { addr, bytes, mnemonic: words[..main].join(" "), operands: words[main..].join(" "), comment_target })
}

/// Drops ` <sym+off>` annotations (printed by some binutils versions after addresses).
fn strip_symbols(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(" <") {
        match rest[i..].find('>') {
            Some(j) => {
                out.push_str(&rest[..i]);
                rest = &rest[i + j + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Words objdump prints before the mnemonic proper.
fn is_prefix_word(w: &str) -> bool {
    const PREFIX_WORDS: [&str; 11] =
        ["rep", "repz", "repnz", "repe", "repne", "lock", "data16", "addr32", "notrack", "bnd", "xacquire"];
    PREFIX_WORDS.contains(&w) || w.starts_with("rex")
}
