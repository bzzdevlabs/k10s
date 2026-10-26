//! What a node can run: its `EXT_CAPS` gossip record (see [`crate::member`]), read by placement version 3.

use crate::codec::{put_varint, DecodeError, Reader, Res};
use alloc::string::String;
use alloc::vec::Vec;

/// CPU architecture. The codes are shared by manifests (image services), `K10I` images and gossip.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Arch {
    X86_64 = 1,
    Aarch64 = 2,
    I686 = 3,
    Armv7 = 4,
    Riscv64 = 5,
}

impl Arch {
    pub const ALL: [Arch; 5] = [Arch::X86_64, Arch::Aarch64, Arch::I686, Arch::Armv7, Arch::Riscv64];

    pub fn from_u8(b: u8) -> Option<Arch> {
        Arch::ALL.into_iter().find(|a| *a as u8 == b)
    }

    /// The name used by Rust target triples and OS images (`x86_64`, `aarch64`, `i686`, `armv7`, `riscv64`).
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
            Arch::I686 => "i686",
            Arch::Armv7 => "armv7",
            Arch::Riscv64 => "riscv64",
        }
    }

    pub fn parse(s: &str) -> Option<Arch> {
        Arch::ALL.into_iter().find(|a| a.name() == s)
    }

    pub fn word_bits(self) -> u8 {
        match self {
            Arch::I686 | Arch::Armv7 => 32,
            _ => 64,
        }
    }

    /// The architecture this code was built for.
    pub fn build() -> Option<Arch> {
        if cfg!(target_arch = "x86_64") {
            Some(Arch::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Arch::Aarch64)
        } else if cfg!(target_arch = "x86") {
            Some(Arch::I686)
        } else if cfg!(target_arch = "arm") {
            Some(Arch::Armv7)
        } else if cfg!(target_arch = "riscv64") {
            Some(Arch::Riscv64)
        } else {
            None
        }
    }
}

/// Why a node runs no image services ([`NodeCaps::lx`] is 0). Codes travel in gossip; unknown ones are kept.
pub mod lx_gap {
    /// The node runs image services.
    pub const NONE: u8 = 0;
    /// A hosted or Tier 1 node: the Linux personality exists only in the native kernel.
    pub const HOSTED: u8 = 1;
    /// A native kernel built without the Linux personality.
    pub const NOT_BUILT: u8 = 2;
    /// The CPU fails the mitigation gate (docs/research/oci.md §5).
    pub const CPU_GATE: u8 = 3;
    /// No Linux personality for this architecture.
    pub const ARCH: u8 = 4;

    pub fn describe(code: u8) -> &'static str {
        match code {
            NONE => "runs image services",
            HOSTED => "hosted node: image services run only on the native kernel",
            NOT_BUILT => "native kernel without the Linux personality",
            CPU_GATE => "the CPU fails the mitigation gate",
            ARCH => "no Linux personality for this architecture",
            _ => "unknown reason",
        }
    }
}

const FLAG_JIT: u8 = 1;

/// A node's capabilities, announced in its `EXT_CAPS` gossip record.
#[derive(Clone, PartialEq, Eq, Debug, Hash, Default)]
pub struct NodeCaps {
    /// [`Arch`] code (codes this build does not know are kept: newer nodes).
    pub arch: u8,
    /// 32 or 64.
    pub word_bits: u8,
    /// Largest native service slot, `2^max_mem_log2` bytes.
    pub max_mem_log2: u8,
    /// Native services are compiled (else interpreted).
    pub jit: bool,
    /// Linux-personality level: 0 none (no image services), 1 stateless images (M1), 2 JVM (M2), 3 volumes (M3).
    pub lx: u8,
    /// Why `lx` is 0 (see [`lx_gap`]).
    pub lx_gap: u8,
    /// Bytes for `K10I` images and for content objects on this node.
    pub image_cache: u64,
    pub object_area: u64,
    /// The only tenant whose image services this node runs (empty: image services without a tenant).
    pub tenant: String,
}

impl NodeCaps {
    /// A node of this build's architecture and word size: native services up to `2^max_mem_log2`, no image services
    /// (`lx_gap` says why).
    pub fn this_build(max_mem_log2: u8, jit: bool, lx_gap: u8) -> NodeCaps {
        let arch = Arch::build();
        NodeCaps {
            arch: arch.map_or(0, |a| a as u8),
            word_bits: usize::BITS as u8,
            max_mem_log2,
            jit,
            lx: 0,
            lx_gap,
            ..Default::default()
        }
    }

    pub fn arch(&self) -> Option<Arch> {
        Arch::from_u8(self.arch)
    }

    /// One line for status output.
    pub fn describe(&self) -> String {
        let arch = self.arch().map_or("unknown", Arch::name);
        let images = if self.lx > 0 {
            alloc::format!("images: level {}", self.lx)
        } else {
            alloc::format!("images: no ({})", lx_gap::describe(self.lx_gap))
        };
        let tenant = if self.tenant.is_empty() { String::new() } else { alloc::format!(", tenant {}", self.tenant) };
        alloc::format!(
            "{arch} {}-bit, slots up to 2^{}, {}, {images}{tenant}",
            self.word_bits,
            self.max_mem_log2,
            if self.jit { "jit" } else { "interpreter" }
        )
    }

    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        let t = &self.tenant.as_bytes()[..self.tenant.len().min(63)];
        out.extend_from_slice(&[self.arch, self.word_bits, self.max_mem_log2, if self.jit { FLAG_JIT } else { 0 }]);
        out.extend_from_slice(&[self.lx, self.lx_gap]);
        put_varint(out, self.image_cache);
        put_varint(out, self.object_area);
        out.push(t.len() as u8);
        out.extend_from_slice(t);
    }

    pub(crate) fn decode(v: &[u8]) -> Res<NodeCaps> {
        let mut r = Reader::new(v);
        let [arch, word_bits, max_mem_log2, flags, lx, lx_gap] = r.array::<6>()?;
        let (image_cache, object_area) = (r.varint()?, r.varint()?);
        let n = r.u8()? as usize;
        let tenant = core::str::from_utf8(r.bytes(n)?).map_err(|_| DecodeError::Invalid("caps tenant"))?;
        if !(tenant.is_empty() || crate::manifest::valid_name(tenant)) {
            return Err(DecodeError::Invalid("caps tenant"));
        }
        // Newer nodes may append fields: ignored.
        Ok(NodeCaps {
            arch,
            word_bits,
            max_mem_log2,
            jit: flags & FLAG_JIT != 0,
            lx,
            lx_gap,
            image_cache,
            object_area,
            tenant: tenant.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_codes_and_record() {
        for a in Arch::ALL {
            assert_eq!((Arch::from_u8(a as u8), Arch::parse(a.name())), (Some(a), Some(a)));
        }
        assert_eq!((Arch::from_u8(0), Arch::parse("x86")), (None, None));
        assert_eq!(Arch::build().map(Arch::word_bits), Some(usize::BITS as u8));
        let c = NodeCaps { lx: 1, lx_gap: lx_gap::NONE, image_cache: 1 << 33, tenant: "acme".into(), ..NodeCaps::this_build(28, false, lx_gap::HOSTED) };
        let mut b = Vec::new();
        c.encode_into(&mut b);
        assert_eq!(NodeCaps::decode(&b).unwrap(), c);
        let mut longer = b.clone();
        longer.extend_from_slice(&[7, 7]);
        assert_eq!(NodeCaps::decode(&longer).unwrap(), c, "fields appended by newer nodes are ignored");
        for n in 0..b.len() {
            assert!(NodeCaps::decode(&b[..n]).is_err());
        }
        let mut bad = b.clone();
        *bad.last_mut().unwrap() = b'A';
        assert!(NodeCaps::decode(&bad).is_err(), "invalid tenant name");
        assert!(NodeCaps::this_build(36, true, lx_gap::HOSTED).describe().contains("hosted node"));
    }
}
