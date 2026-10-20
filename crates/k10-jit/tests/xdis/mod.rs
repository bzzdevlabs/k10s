//! External disassembler (GNU objdump, Intel syntax) for the assurance tests: shares no code with the JIT.
//! Tests skip when objdump is absent; `K10_OBJDUMP` overrides the path.
#![allow(dead_code)]

pub mod sfi;

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// The verifier's [`sfi::Mode`] for an indirect-branch option.
pub fn mode(i: k10_jit::IndirectBranch) -> sfi::Mode {
    match i {
        k10_jit::IndirectBranch::Jump => sfi::Mode::Jump,
        k10_jit::IndirectBranch::Lfence => sfi::Mode::Lfence,
        k10_jit::IndirectBranch::Retpoline => sfi::Mode::Retpoline,
    }
}

/// [`sfi::Layout`] of a blob compiled with `indirect`.
pub fn layout(b: &k10_jit::CodeBlob, indirect: k10_jit::IndirectBranch) -> sfi::Layout {
    let m = b.meta();
    sfi::Layout {
        funcs: m.funcs().iter().map(|f| (f.offset, f.params)).collect(),
        helper_table: m.helper_table_offset(),
        num_helpers: m.num_helpers(),
        num_imports: m.num_imports(),
        min_mem_log2: m.min_mem_log2(),
        mode: mode(indirect),
    }
}

/// Verifies compiled blobs (each with its indirect-branch option); preludes must be identical within a mode.
pub fn verify_compiled(objdump: &std::path::Path, blobs: &[(&k10_jit::CodeBlob, k10_jit::IndirectBranch)]) -> Result<sfi::Stats, String> {
    let mut bs: Vec<sfi::Blob> = blobs.iter().map(|(b, i)| sfi::Blob::new(b.code(), layout(b, *i))).collect();
    let (stats, preludes) = sfi::verify_blobs(objdump, &mut bs)?;
    let mut first: std::collections::HashMap<sfi::Mode, usize> = Default::default();
    for (k, (_, i)) in blobs.iter().enumerate() {
        let r = *first.entry(mode(*i)).or_insert(k);
        if preludes[k] != preludes[r] {
            return Err(format!("prelude of blob {k} differs from blob {r}'s ({:?} mode)", mode(*i)));
        }
    }
    Ok(stats)
}

/// The objdump binary to use, if any.
pub fn objdump() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("K10_OBJDUMP") {
        return Some(PathBuf::from(p));
    }
    let p = PathBuf::from("/usr/bin/objdump");
    p.exists().then_some(p)
}

/// One decoded instruction.
#[derive(Clone, Debug)]
pub struct Insn {
    /// Offset in the disassembled blob.
    pub addr: u64,
    pub bytes: Vec<u8>,
    /// Normalized text: single spaces, no comment (`rex.W push QWORD PTR [rsp]`, `mov rax,QWORD PTR [rbp+0x0]`).
    pub text: String,
    /// The mnemonic, prefixes included (`rep stos`, `rex.W push`).
    pub mnemonic: String,
    /// Operands as printed (`rax,QWORD PTR [rbp+0x0]`), empty if none.
    pub operands: String,
    /// Target printed in a trailing `# 0x…` comment (RIP-relative operands), wrapped to 64 bits.
    pub comment_target: Option<u64>,
}

impl Insn {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn end(&self) -> u64 {
        self.addr + self.bytes.len() as u64
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Disassembles every blob (one objdump process per 400 blobs). Blob `i`'s instructions have addresses relative
/// to the start of blob `i`.
pub fn disassemble_many(objdump: &std::path::Path, blobs: &[&[u8]]) -> Vec<Vec<Insn>> {
    let dir = std::env::temp_dir().join(format!("k10-xdis-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::with_capacity(blobs.len());
    for (chunk_i, chunk) in blobs.chunks(400).enumerate() {
        let mut files = Vec::new();
        for (i, b) in chunk.iter().enumerate() {
            let p = dir.join(format!("b{chunk_i}_{i}.bin"));
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(b).unwrap();
            files.push(p);
        }
        let res = Command::new(objdump)
            .args(["-D", "-b", "binary", "-m", "i386:x86-64", "-M", "intel", "--insn-width=16", "-z"])
            .args(&files)
            .output()
            .expect("running objdump");
        assert!(res.status.success(), "objdump failed: {}", String::from_utf8_lossy(&res.stderr));
        let text = String::from_utf8(res.stdout).unwrap();
        let mut per_file: Vec<Vec<Insn>> = vec![Vec::new(); chunk.len()];
        let mut cur: Option<usize> = None;
        for line in text.lines() {
            if let Some(name) = line.strip_suffix(":     file format binary") {
                let idx = files.iter().position(|f| f.to_str() == Some(name)).expect("unknown objdump file header");
                cur = Some(idx);
                continue;
            }
            if let (Some(i), Some(insn)) = (cur, parse_line(line)) {
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

pub fn disassemble(objdump: &std::path::Path, blob: &[u8]) -> Vec<Insn> {
    disassemble_many(objdump, &[blob]).pop().unwrap()
}

/// `   1c:\t66 48 0f 6e c0      \tmovq   xmm0,rax` → Insn.
fn parse_line(line: &str) -> Option<Insn> {
    let mut parts = line.splitn(3, '\t');
    let addr = parts.next()?.trim().strip_suffix(':')?;
    let addr = u64::from_str_radix(addr, 16).ok()?;
    let bytes: Vec<u8> = parts.next()?.split_whitespace().map(|b| u8::from_str_radix(b, 16)).collect::<Result<_, _>>().ok()?;
    let rest = parts.next().unwrap_or("").trim();
    let (body, comment) = match rest.split_once(" # ") {
        Some((b, c)) => (b.trim(), Some(c.trim())),
        None => (rest, None),
    };
    let comment_target = comment.and_then(|c| c.split_whitespace().next()).and_then(|c| c.strip_prefix("0x")).and_then(|h| u64::from_str_radix(h, 16).ok());
    let words: Vec<&str> = body.split_whitespace().collect();
    // The mnemonic is the first word that is not a prefix (`rep`, `rex.W`, `lock`, …), together with those prefixes.
    let main = words.iter().position(|w| !is_prefix_word(w)).map_or(words.len(), |i| i + 1);
    let mnemonic = words[..main].join(" ");
    let operands = words[main..].join(" ");
    let text = if operands.is_empty() { mnemonic.clone() } else { format!("{mnemonic} {operands}") };
    Some(Insn { addr, bytes, text, mnemonic, operands, comment_target })
}

/// Words objdump prints before the mnemonic proper.
fn is_prefix_word(w: &str) -> bool {
    const PREFIX_WORDS: [&str; 11] = ["rep", "repz", "repnz", "repe", "repne", "lock", "data16", "addr32", "notrack", "bnd", "xacquire"];
    PREFIX_WORDS.contains(&w) || w.starts_with("rex")
}
