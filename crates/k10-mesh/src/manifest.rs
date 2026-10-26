//! The desired-state document: a versioned list of services. It is small, content-addressed
//! (`hash = sha256(encode())`) and spread by gossip as a pointer `(version, hash, origin)`; the bytes themselves are
//! fetched over the peer protocol.
//!
//! Binary format (canonical: services sorted by name, minimal varints, so equal manifests have equal hashes):
//! ```text
//! "K10M" | format:u8 | version:u64 LE | count:varint | service*
//! service = name_len:u8 name | code_hash:[32] | replicas:varint | mem_log2:u8 | max_instances:varint
//!           | cpu_ms:varint | flags:u8 | config_len:varint config | calls
//! format 1: flags = fresh (bit0); no calls
//! format 2: flags = fresh (bit0), any (bit1); calls = n:varint index:varint* (strictly increasing service indices)
//! format 3: as format 2, then the OS plan: n:varint (platform_len:u8 platform image:[32])* wave:varint
//!           (platforms sorted, unique; n >= 1, wave >= 1)
//! format 4: native and image services in one name order, each `name | kind:u8 | body` (kind 0: the format-2
//!           service fields; kind 1: an image service, see [`crate::image`]); calls index that common order; then
//!           the OS plan with n >= 0 (no wave when n = 0); at least one image service
//! ```
//! The encoder emits the oldest format that expresses the manifest ([`Manifest::format`]): 4 with image services,
//! else 3 when it names OS images, else 1 when no service may call anything (such manifests keep their earlier
//! hash), else 2.

use crate::codec::{put_u64, put_varint, DecodeError, Reader};
use crate::caps::Arch;
use crate::image::{self, ImageService};
use crate::sha256::sha256;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};

/// Maximum encoded manifest size accepted by [`Manifest::decode`].
pub const MAX_MANIFEST_BYTES: usize = 16 << 20;
/// Maximum number of services in one manifest.
pub const MAX_SERVICES: usize = 100_000;
/// Maximum size of one service's config blob.
pub const MAX_CONFIG_BYTES: usize = 1 << 20;
/// Maximum replicas per service.
pub const MAX_REPLICAS: u32 = 100_000;
/// Smallest / largest memory slot (log2 bytes): 4 KiB .. 64 GiB.
pub const MIN_MEM_LOG2: u8 = 12;
pub const MAX_MEM_LOG2: u8 = 36;
/// `calls = ["*"]`: any service; valid only alone.
pub const CALL_ANY: &str = "*";

const MAGIC: &[u8; 4] = b"K10M";
const FORMAT_V1: u8 = 1;
const FORMAT_V2: u8 = 2;
const FORMAT_V3: u8 = 3;
const FORMAT_V4: u8 = 4;
const KIND_NATIVE: u8 = 0;
const KIND_IMAGE: u8 = 1;
/// Maximum OS images (platforms) in one manifest.
pub const MAX_OS_IMAGES: usize = 64;
const FLAG_FRESH: u8 = 1;
const FLAG_ANY: u8 = 2;
const MIN_SERVICE_BYTES: usize = 1 + 1 + 32 + 1 + 1 + 1 + 1 + 1 + 1;

/// One service of the desired state.
#[derive(Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct ServiceSpec {
    /// `[a-z0-9-]{1,63}`, unique within a manifest.
    pub name: String,
    /// SHA-256 of the service's compiled module (TIR blob).
    pub code_hash: [u8; 32],
    /// Number of replicas placed in the cluster.
    pub replicas: u32,
    /// Memory slot size of one instance = `2^mem_log2` bytes.
    pub mem_log2: u8,
    /// Maximum concurrent instances per node.
    pub max_instances: u32,
    /// Per-request CPU budget (ms).
    pub cpu_ms: u32,
    /// Every request starts from the post-`init` snapshot.
    pub fresh: bool,
    /// Opaque bytes returned by the `config` host call.
    pub config: Vec<u8>,
    /// Services this one may `call` (none by default, not even itself).
    pub calls: Calls,
}

impl ServiceSpec {
    /// Slot size of one instance in bytes.
    pub fn mem_bytes(&self) -> u64 {
        1u64 << self.mem_log2.min(63)
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err(format!("invalid service name `{}` (want [a-z0-9-], 1..=63 chars)", self.name));
        }
        if !(MIN_MEM_LOG2..=MAX_MEM_LOG2).contains(&self.mem_log2) {
            return Err(format!("service `{}`: memory must be 4KiB..64GiB", self.name));
        }
        if self.replicas > MAX_REPLICAS {
            return Err(format!("service `{}`: too many replicas", self.name));
        }
        if self.max_instances == 0 || self.cpu_ms == 0 {
            return Err(format!("service `{}`: max_instances and cpu_ms must be >= 1", self.name));
        }
        if self.config.len() > MAX_CONFIG_BYTES {
            return Err(format!("service `{}`: config larger than 1 MiB", self.name));
        }
        Ok(())
    }
}

/// The services a service may `call`: none (the default), any, or a set of names. Canonical by construction.
#[derive(Clone, Default)]
pub struct Calls(Grant);

#[derive(Clone, Default)]
enum Grant {
    #[default]
    None,
    Any,
    /// Non-empty, strictly increasing indices into a sorted name table (one table per decoded manifest).
    Names { table: Arc<[Box<str>]>, idx: Box<[u32]> },
}

impl Calls {
    pub fn none() -> Calls {
        Calls(Grant::None)
    }

    pub fn any() -> Calls {
        Calls(Grant::Any)
    }

    /// The text form: service names in any order (duplicates ignored), or `"*"` alone.
    pub fn from_list<S: AsRef<str>>(list: &[S]) -> Result<Calls, String> {
        let mut names: Vec<&str> = list.iter().map(AsRef::as_ref).collect();
        names.sort_unstable();
        names.dedup();
        match names.as_slice() {
            [] => return Ok(Calls::none()),
            [CALL_ANY] => return Ok(Calls::any()),
            _ => {}
        }
        if let Some(bad) = names.iter().find(|n| !valid_name(n)) {
            return Err(match *bad {
                CALL_ANY => "`\"*\"` (any service) must be the only entry of `calls`".to_string(),
                _ => format!("invalid service name `{bad}` in `calls`"),
            });
        }
        let table: Arc<[Box<str>]> = names.iter().map(|&n| Box::from(n)).collect();
        let idx = (0..table.len() as u32).collect();
        Ok(Calls(Grant::Names { table, idx }))
    }

    pub fn is_none(&self) -> bool {
        matches!(self.0, Grant::None)
    }

    pub fn is_any(&self) -> bool {
        matches!(self.0, Grant::Any)
    }

    /// The granted names, sorted (none for `none` and `any`).
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        let (table, idx): (&[Box<str>], &[u32]) = match &self.0 {
            Grant::Names { table, idx } => (table, idx),
            _ => (&[], &[]),
        };
        idx.iter().map(move |&i| &*table[i as usize])
    }

    /// Whether a call to `name` (the name part of a call target) is granted.
    #[inline]
    pub fn allows(&self, name: &[u8]) -> bool {
        match &self.0 {
            Grant::None => false,
            Grant::Any => true,
            Grant::Names { table, idx } => idx.binary_search_by(|&i| table[i as usize].as_bytes().cmp(name)).is_ok(),
        }
    }

    fn kind(&self) -> u8 {
        match self.0 {
            Grant::None => 0,
            Grant::Any => 1,
            Grant::Names { .. } => 2,
        }
    }
}

impl PartialEq for Calls {
    fn eq(&self, other: &Self) -> bool {
        self.kind() == other.kind() && self.names().eq(other.names())
    }
}

impl Eq for Calls {}

impl Ord for Calls {
    fn cmp(&self, other: &Self) -> Ordering {
        self.kind().cmp(&other.kind()).then_with(|| self.names().cmp(other.names()))
    }
}

impl PartialOrd for Calls {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for Calls {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.kind().hash(h);
        self.names().for_each(|n| n.hash(h));
    }
}

impl fmt::Debug for Calls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_any() {
            f.debug_list().entry(&CALL_ANY).finish()
        } else {
            f.debug_list().entries(self.names()).finish()
        }
    }
}

/// A varint in its shortest form (another encoding of the same value would change the hash).
pub(crate) fn varint(r: &mut Reader<'_>) -> Result<u64, DecodeError> {
    let before = r.remaining();
    let v = r.varint()?;
    if before - r.remaining() != (64 - (v | 1).leading_zeros() as usize).div_ceil(7) {
        return Err(DecodeError::Invalid("varint (not minimal)"));
    }
    Ok(v)
}

pub(crate) fn varint_u32(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    u32::try_from(varint(r)?).map_err(|_| DecodeError::Invalid("u32 varint"))
}

/// One OS image of the desired state: Tier 1 nodes whose image is for `platform` update to `image`.
#[derive(Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct OsImage {
    /// `ARCH-PROFILE` of the signed image (`x86_64-vm`, `x86_64-metal`), see [`crate::osimage`].
    pub platform: String,
    /// SHA-256 of the signed image blob.
    pub image: [u8; 32],
}

/// The OS part of the desired state (docs/OS.md, "Updates"): which image each platform runs, rolled out in waves.
#[derive(Clone, PartialEq, Eq, Debug, Default, Hash)]
pub struct OsPlan {
    /// Unique platforms (encoded sorted).
    pub images: Vec<OsImage>,
    /// Nodes of a platform in the first update wave; each following wave is twice as large. 0 without images.
    pub wave: u32,
}

impl OsPlan {
    pub fn image_for(&self, platform: &str) -> Option<&[u8; 32]> {
        self.images.iter().find(|i| i.platform == platform).map(|i| &i.image)
    }

    fn validate(&self) -> Result<(), String> {
        if self.images.len() > MAX_OS_IMAGES {
            return Err(format!("more than {MAX_OS_IMAGES} OS images"));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.images.len());
        for i in &self.images {
            if !valid_platform(&i.platform) {
                return Err(format!("invalid OS platform `{}` (want ARCH-PROFILE of [a-z0-9_.-])", i.platform));
            }
            if seen.contains(&i.platform.as_str()) {
                return Err(format!("two OS images for platform `{}`", i.platform));
            }
            seen.push(&i.platform);
        }
        match (self.images.is_empty(), self.wave) {
            (false, 0) => Err("the OS update wave must be >= 1".to_string()),
            (true, w) if w != 0 => Err("an OS update wave without OS images".to_string()),
            _ => Ok(()),
        }
    }
}

/// `ARCH-PROFILE`: 3..=64 characters of `[a-z0-9_.-]` with a `-` that is neither first nor last.
pub fn valid_platform(p: &str) -> bool {
    (3..=64).contains(&p.len())
        && p.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(&c))
        && p.find('-').is_some_and(|i| i > 0 && i + 1 < p.len())
}

/// The desired state of the cluster.
#[derive(Clone, PartialEq, Eq, Debug, Default, Hash)]
pub struct Manifest {
    pub version: u64,
    /// Native services.
    pub services: Vec<ServiceSpec>,
    /// Image services (format 4); names are unique among all services.
    pub image_services: Vec<ImageService>,
    pub os: OsPlan,
}

/// A service of either kind, for code that walks every service in name order.
#[derive(Clone, Copy, Debug)]
pub enum AnyService<'a> {
    Native(&'a ServiceSpec),
    Image(&'a ImageService),
}

impl<'a> AnyService<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            AnyService::Native(s) => &s.name,
            AnyService::Image(s) => &s.name,
        }
    }

    pub fn calls(&self) -> &'a Calls {
        match self {
            AnyService::Native(s) => &s.calls,
            AnyService::Image(s) => &s.calls,
        }
    }
    /// By name, native before image, then by every field: a total order, so equal inputs sort alike in any order.
    fn order(&self, other: &Self) -> Ordering {
        self.name().cmp(other.name()).then_with(|| match (self, other) {
            (AnyService::Native(a), AnyService::Native(b)) => a.cmp(b),
            (AnyService::Image(a), AnyService::Image(b)) => a.cmp(b),
            (AnyService::Native(_), AnyService::Image(_)) => Ordering::Less,
            (AnyService::Image(_), AnyService::Native(_)) => Ordering::Greater,
        })
    }
}

impl Manifest {
    /// Canonical encoding (services sorted by name).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// The binary format of [`Manifest::encode`]: the oldest one that expresses the manifest, so manifests that
    /// use no newer feature keep their bytes and hash.
    pub fn format(&self) -> u8 {
        if !self.image_services.is_empty() {
            FORMAT_V4
        } else if !self.os.images.is_empty() {
            FORMAT_V3
        } else if self.services.iter().all(|s| s.calls.is_none()) {
            FORMAT_V1
        } else {
            FORMAT_V2
        }
    }

    /// Every service, native and image, sorted by name (a total order: duplicates, which `validate` refuses, sort
    /// the same whatever the input order).
    pub fn all_services(&self) -> Vec<AnyService<'_>> {
        let mut v: Vec<AnyService<'_>> = self.services.iter().map(AnyService::Native).collect();
        v.extend(self.image_services.iter().map(AnyService::Image));
        v.sort_by(AnyService::order);
        v
    }

    /// Callees that are not services of the manifest cannot be encoded (`validate` rejects such manifests).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let format = self.format();
        out.extend_from_slice(MAGIC);
        out.push(format);
        put_u64(out, self.version);
        let order = self.all_services();
        put_varint(out, order.len() as u64);
        for s in &order {
            let name = s.name().as_bytes();
            let n = name.len().min(255);
            out.push(n as u8);
            out.extend_from_slice(&name[..n]);
            let idx: Vec<usize> =
                s.calls().names().filter_map(|c| order.binary_search_by(|t| t.name().cmp(c)).ok()).collect();
            match s {
                AnyService::Native(s) => {
                    if format == FORMAT_V4 {
                        out.push(KIND_NATIVE);
                    }
                    out.extend_from_slice(&s.code_hash);
                    put_varint(out, s.replicas as u64);
                    out.push(s.mem_log2);
                    put_varint(out, s.max_instances as u64);
                    put_varint(out, s.cpu_ms as u64);
                    out.push(if s.fresh { FLAG_FRESH } else { 0 } | if s.calls.is_any() { FLAG_ANY } else { 0 });
                    put_varint(out, s.config.len() as u64);
                    out.extend_from_slice(&s.config);
                    if format != FORMAT_V1 {
                        put_varint(out, idx.len() as u64);
                        idx.into_iter().for_each(|i| put_varint(out, i as u64));
                    }
                }
                AnyService::Image(s) => {
                    out.push(KIND_IMAGE);
                    s.encode_body(out, &idx);
                }
            }
        }
        if format >= FORMAT_V3 {
            let mut images: Vec<&OsImage> = self.os.images.iter().collect();
            images.sort();
            put_varint(out, images.len() as u64);
            for i in &images {
                let p = i.platform.as_bytes();
                let n = p.len().min(255);
                out.push(n as u8);
                out.extend_from_slice(&p[..n]);
                out.extend_from_slice(&i.image);
            }
            if !images.is_empty() {
                put_varint(out, self.os.wave as u64);
            }
        }
    }

    /// Strict, bounded decoder: never panics, never allocates more than a small multiple of the input size, and
    /// only accepts canonical encodings (so `sha256(bytes) == decode(bytes).hash()`).
    pub fn decode(bytes: &[u8]) -> Result<Manifest, DecodeError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(DecodeError::TooLarge("manifest"));
        }
        let mut r = Reader::new(bytes);
        if r.bytes(4)? != MAGIC {
            return Err(DecodeError::Invalid("manifest magic"));
        }
        let format = r.u8()?;
        if !(FORMAT_V1..=FORMAT_V4).contains(&format) {
            return Err(DecodeError::Invalid("manifest format"));
        }
        let version = r.u64()?;
        let count = varint(&mut r)?;
        if count > MAX_SERVICES as u64 {
            return Err(DecodeError::TooLarge("service count"));
        }
        let count = count as usize;
        let min_service = MIN_SERVICE_BYTES + (format != FORMAT_V1) as usize + (format == FORMAT_V4) as usize;
        if count > r.remaining() / min_service {
            return Err(DecodeError::Truncated);
        }
        let flag_mask = if format == FORMAT_V1 { FLAG_FRESH } else { FLAG_FRESH | FLAG_ANY };
        let mut services: Vec<ServiceSpec> = Vec::with_capacity(count);
        let mut image_services: Vec<ImageService> = Vec::new();
        // Every name in order, and the callee indices per (kind, position), turned into grants at the end.
        let mut names: Vec<Box<str>> = Vec::with_capacity(count);
        let mut pending: Vec<(u8, usize, Vec<u32>)> = Vec::new();
        for _ in 0..count {
            let n = r.u8()? as usize;
            let name = core::str::from_utf8(r.bytes(n)?).map_err(|_| DecodeError::Invalid("service name"))?;
            if !valid_name(name) {
                return Err(DecodeError::Invalid("service name"));
            }
            if names.last().is_some_and(|prev| &**prev >= name) {
                return Err(DecodeError::Invalid("service order (must be sorted, unique)"));
            }
            names.push(Box::from(name));
            let kind = if format == FORMAT_V4 { r.u8()? } else { KIND_NATIVE };
            if kind == KIND_IMAGE {
                let (s, idx) = ImageService::decode_body(&mut r, name, count)?;
                if !idx.is_empty() {
                    pending.push((KIND_IMAGE, image_services.len(), idx));
                }
                image_services.push(s);
                continue;
            }
            if kind != KIND_NATIVE {
                return Err(DecodeError::Invalid("service kind"));
            }
            let code_hash = r.array::<32>()?;
            let replicas = varint_u32(&mut r)?;
            let mem_log2 = r.u8()?;
            let max_instances = varint_u32(&mut r)?;
            let cpu_ms = varint_u32(&mut r)?;
            let flags = r.u8()?;
            if flags & !flag_mask != 0 {
                return Err(DecodeError::Invalid("service flags"));
            }
            let clen = varint(&mut r)?;
            if clen > MAX_CONFIG_BYTES as u64 {
                return Err(DecodeError::TooLarge("config"));
            }
            let config = r.bytes(clen as usize)?.to_vec();
            let calls = if flags & FLAG_ANY != 0 { Calls::any() } else { Calls::none() };
            if format != FORMAT_V1 {
                let n = varint(&mut r)?;
                // Strictly increasing indices below `count`, at least a byte each: bounds n before allocating.
                if n > count as u64 || n > r.remaining() as u64 {
                    return Err(DecodeError::Invalid("calls count"));
                }
                if n > 0 && calls.is_any() {
                    return Err(DecodeError::Invalid("calls (any with a list)"));
                }
                let mut idx: Vec<u32> = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let x = varint(&mut r)?;
                    if x >= count as u64 || idx.last().is_some_and(|&p| x <= p as u64) {
                        return Err(DecodeError::Invalid("calls index"));
                    }
                    idx.push(x as u32);
                }
                if n > 0 {
                    pending.push((KIND_NATIVE, services.len(), idx));
                }
            }
            let s = ServiceSpec {
                name: name.to_string(),
                code_hash,
                replicas,
                mem_log2,
                max_instances,
                cpu_ms,
                fresh: flags & FLAG_FRESH != 0,
                config,
                calls,
            };
            s.validate().map_err(|_| DecodeError::Invalid("service field"))?;
            services.push(s);
        }
        let mut os = OsPlan::default();
        if format >= FORMAT_V3 {
            let n = varint(&mut r)?;
            if (n == 0 && format == FORMAT_V3) || n > MAX_OS_IMAGES as u64 {
                return Err(DecodeError::Invalid("OS image count"));
            }
            for _ in 0..n {
                let len = r.u8()? as usize;
                let platform = core::str::from_utf8(r.bytes(len)?).map_err(|_| DecodeError::Invalid("OS platform"))?;
                if !valid_platform(platform) || os.images.last().is_some_and(|p: &OsImage| p.platform.as_str() >= platform) {
                    return Err(DecodeError::Invalid("OS platform (must be valid, sorted, unique)"));
                }
                os.images.push(OsImage { platform: platform.to_string(), image: r.array::<32>()? });
            }
            if n > 0 {
                os.wave = varint_u32(&mut r)?;
                if os.wave == 0 {
                    return Err(DecodeError::Invalid("OS update wave"));
                }
            }
        }
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes"));
        }
        if !pending.is_empty() {
            let table: Arc<[Box<str>]> = names.into();
            for (kind, i, idx) in pending {
                let calls = Calls(Grant::Names { table: table.clone(), idx: idx.into_boxed_slice() });
                match kind {
                    KIND_NATIVE => services[i].calls = calls,
                    _ => image_services[i].calls = calls,
                }
            }
        }
        if format == FORMAT_V2 && services.iter().all(|s| s.calls.is_none()) {
            return Err(DecodeError::Invalid("manifest format (2 without calls)"));
        }
        if format == FORMAT_V4 && image_services.is_empty() {
            return Err(DecodeError::Invalid("manifest format (4 without image services)"));
        }
        let m = Manifest { version, services, image_services, os };
        m.validate_global().map_err(|_| DecodeError::Invalid("manifest (routes or public ports)"))?;
        Ok(m)
    }

    /// Content hash: SHA-256 of the canonical encoding.
    pub fn hash(&self) -> [u8; 32] {
        sha256(&self.encode())
    }

    /// Checks names, uniqueness and field ranges (what [`Manifest::decode`] would reject).
    pub fn validate(&self) -> Result<(), String> {
        if self.services.len() + self.image_services.len() > MAX_SERVICES {
            return Err("too many services".to_string());
        }
        let mut names: Vec<&str> = Vec::with_capacity(self.services.len() + self.image_services.len());
        for s in &self.services {
            s.validate()?;
            names.push(&s.name);
        }
        for s in &self.image_services {
            s.validate()?;
            names.push(&s.name);
        }
        names.sort_unstable();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("duplicate service `{}`", w[0]));
        }
        // The manifest is the complete desired state: an undeclared callee is a mistake.
        for s in self.all_services() {
            if let Some(c) = s.calls().names().find(|c| names.binary_search(c).is_err()) {
                return Err(format!("service `{}` calls `{c}`, which is not a service of this manifest", s.name()));
            }
        }
        self.validate_global()?;
        self.os.validate()
    }

    /// Routes and public ports are cluster-wide: one service each.
    fn validate_global(&self) -> Result<(), String> {
        if self.image_services.len() < 2 {
            return Ok(());
        }
        // (port, route, service): a public port, or a route with port 0.
        let mut claims: Vec<(u16, &str, &str)> = Vec::new();
        for s in &self.image_services {
            claims.extend(s.routes.iter().map(|r| (0, r.as_str(), s.name.as_str())));
            claims.extend(s.public.iter().map(|p| (p.port, "", s.name.as_str())));
        }
        claims.sort_unstable();
        match claims.windows(2).find(|w| (w[0].0, w[0].1) == (w[1].0, w[1].1)) {
            Some(w) if w[0].0 == 0 => Err(format!("route `{}` is claimed by `{}` and `{}`", w[0].1, w[0].2, w[1].2)),
            Some(w) => Err(format!("public port {} is claimed by `{}` and `{}`", w[0].0, w[0].2, w[1].2)),
            None => Ok(()),
        }
    }

    /// Every blob the manifest references: code (in service order), then OS images; deduplicated. The `K10I`
    /// images of image services are content objects, not bundle blobs.
    pub fn blob_hashes(&self) -> Vec<[u8; 32]> {
        let mut v: Vec<[u8; 32]> = Vec::new();
        for h in self.services.iter().map(|s| &s.code_hash).chain(self.os.images.iter().map(|i| &i.image)) {
            if !v.contains(h) {
                v.push(*h);
            }
        }
        v
    }

    pub fn service(&self, name: &str) -> Option<&ServiceSpec> {
        self.services.iter().find(|s| s.name == name)
    }

    pub fn image_service(&self, name: &str) -> Option<&ImageService> {
        self.image_services.iter().find(|s| s.name == name)
    }

    /// Every secret is sealed with one of the cluster's keys (`None`: an unauthenticated cluster, which has none).
    pub fn check_secrets(&self, keys: Option<&crate::secure::ClusterKeys>) -> Result<(), String> {
        for s in &self.image_services {
            for (var, sealed) in &s.secrets {
                let why = match keys {
                    None => "this cluster has no key (it runs unauthenticated)",
                    Some(k) if !k.holds_secret_key(sealed) => "not sealed with this cluster's key (pass it to --cluster-key-file)",
                    Some(_) => continue,
                };
                return Err(format!("secret `{var}` of image service `{}`: {why}", s.name));
            }
        }
        Ok(())
    }
}

/// Service names: `[a-z0-9-]`, 1..=63 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

// ---------------------------------------------------------------------------------------------------------------
// Operator text format (`k10s apply`)
// ---------------------------------------------------------------------------------------------------------------

/// One `service NAME { ... }` block of the operator text format, with defaults applied. `code` is the path as
/// written; the CLI resolves/compiles it and calls [`ServiceDecl::to_spec`] with the resulting blob hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServiceDecl {
    pub name: String,
    pub code: String,
    pub replicas: u32,
    pub mem_log2: u8,
    pub max_instances: u32,
    pub cpu_ms: u32,
    pub fresh: bool,
    pub config: Vec<u8>,
    pub calls: Calls,
    /// Line of the `service` keyword (for diagnostics).
    pub line: u32,
}

impl ServiceDecl {
    pub fn to_spec(&self, code_hash: [u8; 32]) -> ServiceSpec {
        ServiceSpec {
            name: self.name.clone(),
            code_hash,
            replicas: self.replicas,
            mem_log2: self.mem_log2,
            max_instances: self.max_instances,
            cpu_ms: self.cpu_ms,
            fresh: self.fresh,
            config: self.config.clone(),
            calls: self.calls.clone(),
        }
    }
}

/// Text-format error with a 1-based line number.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseError {
    pub line: u32,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Word(String),
    Str(Vec<u8>),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Eq,
    Semi,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    peeked: Option<Option<(Tok, u32)>>,
}

fn err<T>(line: u32, msg: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { line, msg: msg.into() })
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer { src: src.as_bytes(), pos: 0, line: 1, peeked: None }
    }

    fn peek(&mut self) -> Result<Option<&(Tok, u32)>, ParseError> {
        if self.peeked.is_none() {
            let t = self.lex()?;
            self.peeked = Some(t);
        }
        Ok(self.peeked.as_ref().unwrap().as_ref())
    }

    fn next(&mut self) -> Result<Option<(Tok, u32)>, ParseError> {
        if let Some(t) = self.peeked.take() {
            return Ok(t);
        }
        self.lex()
    }

    fn lex(&mut self) -> Result<Option<(Tok, u32)>, ParseError> {
        loop {
            let Some(&c) = self.src.get(self.pos) else { return Ok(None) };
            match c {
                b'\n' => {
                    self.line += 1;
                    self.pos += 1;
                }
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'#' => {
                    while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        let line = self.line;
        let c = self.src[self.pos];
        let tok = match c {
            b'{' => {
                self.pos += 1;
                Tok::LBrace
            }
            b'}' => {
                self.pos += 1;
                Tok::RBrace
            }
            b'[' => {
                self.pos += 1;
                Tok::LBracket
            }
            b']' => {
                self.pos += 1;
                Tok::RBracket
            }
            b',' => {
                self.pos += 1;
                Tok::Comma
            }
            b'=' => {
                self.pos += 1;
                Tok::Eq
            }
            b';' => {
                self.pos += 1;
                Tok::Semi
            }
            b'"' => {
                self.pos += 1;
                let mut s = Vec::new();
                loop {
                    let Some(&c) = self.src.get(self.pos) else { return err(line, "unterminated string") };
                    self.pos += 1;
                    match c {
                        b'"' => break,
                        b'\n' => return err(line, "unterminated string (newline in string)"),
                        b'\\' => {
                            let Some(&e) = self.src.get(self.pos) else { return err(line, "unterminated string") };
                            self.pos += 1;
                            match e {
                                b'n' => s.push(b'\n'),
                                b't' => s.push(b'\t'),
                                b'r' => s.push(b'\r'),
                                b'0' => s.push(0),
                                b'\\' => s.push(b'\\'),
                                b'"' => s.push(b'"'),
                                b'x' => {
                                    let hex = self.src.get(self.pos..self.pos + 2).and_then(|h| core::str::from_utf8(h).ok());
                                    let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) else {
                                        return err(line, "bad \\x escape (want two hex digits)");
                                    };
                                    self.pos += 2;
                                    s.push(v);
                                }
                                _ => return err(line, format!("unknown escape `\\{}`", e as char)),
                            }
                        }
                        _ => s.push(c),
                    }
                }
                Tok::Str(s)
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' => {
                let start = self.pos;
                while self.pos < self.src.len() {
                    let c = self.src[self.pos];
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Tok::Word(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
            }
            _ => {
                let ch = core::str::from_utf8(&self.src[self.pos..]).ok().and_then(|s| s.chars().next()).unwrap_or('?');
                return err(line, format!("unexpected character `{ch}`"));
            }
        };
        Ok(Some((tok, line)))
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Word(w) => format!("`{w}`"),
        Tok::Str(_) => "a string".to_string(),
        Tok::LBrace => "`{`".to_string(),
        Tok::RBrace => "`}`".to_string(),
        Tok::LBracket => "`[`".to_string(),
        Tok::RBracket => "`]`".to_string(),
        Tok::Comma => "`,`".to_string(),
        Tok::Eq => "`=`".to_string(),
        Tok::Semi => "`;`".to_string(),
    }
}

fn parse_u32(word: &str, line: u32, key: &str) -> Result<u32, ParseError> {
    let digits: String = word.chars().filter(|&c| c != '_').collect();
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return err(line, format!("`{key}` expects an integer, got `{word}`"));
    }
    digits.parse::<u32>().or_else(|_| err(line, format!("`{key}` value `{word}` out of range")))
}

fn unit_multiplier(unit: &str) -> Option<u64> {
    match unit.to_ascii_lowercase().as_str() {
        "" | "b" => Some(1),
        "kib" => Some(1 << 10),
        "mib" => Some(1 << 20),
        "gib" => Some(1 << 30),
        _ => None,
    }
}

/// Parses `64KiB`, `1MiB`, `4096`, `64 KiB` (the unit may be a separate word) into bytes.
fn parse_size(lx: &mut Lexer<'_>, word: &str, line: u32, key: &str) -> Result<u64, ParseError> {
    let split = word.find(|c: char| !c.is_ascii_digit() && c != '_').unwrap_or(word.len());
    let (num, mut unit) = (&word[..split], word[split..].to_string());
    let digits: String = num.chars().filter(|&c| c != '_').collect();
    if digits.is_empty() {
        return err(line, format!("`{key}` expects a size like 64KiB, got `{word}`"));
    }
    if unit.is_empty() {
        if let Some((Tok::Word(w), _)) = lx.peek()? {
            if unit_multiplier(w).is_some() && !w.is_empty() {
                unit = w.clone();
                lx.next()?;
            }
        }
    }
    let Some(mult) = unit_multiplier(&unit) else {
        return err(line, format!("unknown {key} unit `{unit}` (use B, KiB, MiB or GiB)"));
    };
    let Ok(n) = digits.parse::<u64>() else { return err(line, format!("{key} value `{word}` out of range")) };
    let Some(bytes) = n.checked_mul(mult) else { return err(line, format!("{key} value `{word}` out of range")) };
    Ok(bytes)
}

/// `10s`, `500ms`, `2m` into milliseconds.
fn parse_duration_ms(word: &str, line: u32, key: &str) -> Result<u32, ParseError> {
    let split = word.find(|c: char| !c.is_ascii_digit()).unwrap_or(word.len());
    let mult: u64 = match &word[split..] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        _ => return err(line, format!("`{key}` expects a duration like 10s, 500ms or 2m, got `{word}`")),
    };
    let ms = word[..split].parse::<u64>().ok().and_then(|n| n.checked_mul(mult)).filter(|&ms| ms <= u32::MAX as u64);
    ms.map(|ms| ms as u32).ok_or_else(|| ParseError { line, msg: format!("`{key}` value `{word}` out of range") })
}

/// `0.5`, `2`, `1.25` cores into thousandths.
fn parse_cpu(word: &str, line: u32) -> Result<u32, ParseError> {
    let bad = || err(line, format!("`cpu` expects cores like 0.5 or 2 (at most 3 decimals), got `{word}`"));
    let (int, frac) = word.split_once('.').unwrap_or((word, ""));
    if int.is_empty() || int.len() > 4 || frac.len() > 3 || !(int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit())) {
        return bad();
    }
    let frac_milli = format!("{frac:0<3}").parse::<u32>().unwrap_or(0);
    let milli = int.parse::<u32>().map(|i| i * 1000 + frac_milli).or_else(|_| bad())?;
    if milli > image::MAX_CPU_MILLI {
        return err(line, "`cpu` above 1024 cores");
    }
    Ok(milli)
}

fn parse_bool(word: &str, line: u32, key: &str) -> Result<bool, ParseError> {
    match word {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => err(line, format!("`{key}` expects true or false, got `{word}`")),
    }
}

fn parse_port(word: &str, line: u32, key: &str) -> Result<u16, ParseError> {
    match parse_u32(word, line, key)? {
        p @ 1..=65535 => Ok(p as u16),
        _ => err(line, format!("`{key}` must be a port in 1..65535, got `{word}`")),
    }
}

fn utf8(s: Vec<u8>, line: u32, what: &str) -> Result<String, ParseError> {
    String::from_utf8(s).or_else(|_| err(line, format!("{what} is not UTF-8")))
}

/// Parses a `calls` list after its `[`: quoted names, comma-separated (trailing comma allowed).
fn parse_calls(lx: &mut Lexer<'_>, line: u32) -> Result<Calls, ParseError> {
    let unterminated = || err(line, "unterminated `calls` list (missing `]`)");
    let mut list: Vec<String> = Vec::new();
    loop {
        match lx.next()? {
            Some((Tok::RBracket, _)) => break,
            Some((Tok::Str(s), l)) => {
                let name = String::from_utf8(s).or_else(|_| err(l, "`calls` entry is not UTF-8"))?;
                if name != CALL_ANY && !valid_name(&name) {
                    let want = "want [a-z0-9-], 1..=63 chars, or \"*\"";
                    return err(l, format!("invalid service name `{name}` in `calls` ({want})"));
                }
                list.push(name);
                match lx.next()? {
                    Some((Tok::Comma, _)) => {}
                    Some((Tok::RBracket, _)) => break,
                    Some((t, l)) => return err(l, format!("expected `,` or `]` in `calls`, found {}", describe(&t))),
                    None => return unterminated(),
                }
            }
            Some((t, l)) => return err(l, format!("`calls` expects quoted service names, found {}", describe(&t))),
            None => return unterminated(),
        }
    }
    Calls::from_list(&list).or_else(|e| err(line, e))
}

/// A list of quoted strings after its `[`, comma-separated (trailing comma allowed).
fn parse_strings(lx: &mut Lexer<'_>, key: &str, line: u32) -> Result<Vec<(String, u32)>, ParseError> {
    let mut out = Vec::new();
    loop {
        match lx.next()? {
            Some((Tok::RBracket, _)) => return Ok(out),
            Some((Tok::Str(s), l)) => {
                if out.len() == image::MAX_LIST {
                    return err(l, format!("more than {} entries in `{key}`", image::MAX_LIST));
                }
                out.push((utf8(s, l, &format!("a `{key}` entry"))?, l));
                match lx.next()? {
                    Some((Tok::Comma, _)) => {}
                    Some((Tok::RBracket, _)) => return Ok(out),
                    Some((t, l)) => return err(l, format!("expected `,` or `]` in `{key}`, found {}", describe(&t))),
                    None => return err(line, format!("unterminated `{key}` list (missing `]`)")),
                }
            }
            Some((t, l)) => return err(l, format!("`{key}` expects quoted strings, found {}", describe(&t))),
            None => return err(line, format!("unterminated `{key}` list (missing `]`)")),
        }
    }
}

/// `{ KEY = VALUE, … }` after its `{`: keys are words or quoted strings, values words or quoted strings; entries
/// are separated by `,`, `;` or nothing. Duplicate keys are refused.
fn parse_map(lx: &mut Lexer<'_>, key: &str, line: u32) -> Result<Vec<(String, Tok, u32)>, ParseError> {
    let mut out: Vec<(String, Tok, u32)> = Vec::new();
    loop {
        let (k, l) = match lx.next()? {
            Some((Tok::RBrace, _)) => return Ok(out),
            Some((Tok::Comma | Tok::Semi, _)) => continue,
            Some((Tok::Word(w), l)) => (w, l),
            Some((Tok::Str(s), l)) => (utf8(s, l, &format!("a `{key}` key"))?, l),
            Some((t, l)) => return err(l, format!("expected a key or `}}` in `{key}`, found {}", describe(&t))),
            None => return err(line, format!("unterminated `{key}` (missing `}}`)")),
        };
        if out.iter().any(|(x, _, _)| *x == k) {
            return err(l, format!("`{k}` set twice in `{key}`"));
        }
        if out.len() == image::MAX_LIST {
            return err(l, format!("more than {} entries in `{key}`", image::MAX_LIST));
        }
        match lx.next()? {
            Some((Tok::Eq, _)) => {}
            Some((t, l)) => return err(l, format!("expected `=` after `{k}` in `{key}`, found {}", describe(&t))),
            None => return err(line, format!("unterminated `{key}` (missing `}}`)")),
        }
        match lx.next()? {
            Some((v @ (Tok::Word(_) | Tok::Str(_)), vl)) => out.push((k, v, vl)),
            Some((t, l)) => return err(l, format!("bad value for `{k}` in `{key}`: {}", describe(&t))),
            None => return err(line, format!("unterminated `{key}` (missing `}}`)")),
        }
    }
}

/// `[{ port = 5432, to = 5432 }, …]` after its `[` (`to` defaults to `port`).
fn parse_public(lx: &mut Lexer<'_>, line: u32) -> Result<Vec<image::PublicPort>, ParseError> {
    let mut out = Vec::new();
    loop {
        match lx.next()? {
            Some((Tok::RBracket, _)) => return Ok(out),
            Some((Tok::Comma, _)) => continue,
            Some((Tok::LBrace, l)) => {
                let (mut port, mut to) = (None, None);
                for (k, v, vl) in parse_map(lx, "public", l)? {
                    let p = match &v {
                        Tok::Word(w) => parse_port(w, vl, &k)?,
                        t => return err(vl, format!("bad value for `{k}` in `public`: {}", describe(t))),
                    };
                    match k.as_str() {
                        "port" => port = Some(p),
                        "to" => to = Some(p),
                        _ => return err(vl, format!("unknown key `{k}` in `public` (expected port, to)")),
                    }
                }
                let Some(port) = port else { return err(l, "`public` entries need `port`") };
                out.push(image::PublicPort { port, to: to.unwrap_or(port) });
            }
            Some((t, l)) => return err(l, format!("`public` expects entries like {{ port = 5432, to = 5432 }}, found {}", describe(&t))),
            None => return err(line, "unterminated `public` list (missing `]`)"),
        }
    }
}

/// The `os { … }` block of the operator text format: signed image files (paths as written) and the first wave.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OsDecl {
    pub images: Vec<String>,
    pub wave: u32,
    pub line: u32,
}

/// One image service block of the operator text format, with defaults applied. Secret values are files (paths as
/// written in `secret_files`) that the CLI reads and seals with the cluster key into `spec.secrets`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImageDecl {
    pub spec: ImageService,
    /// `(variable, file)` pairs.
    pub secret_files: Vec<(String, String)>,
    /// Line of the `service` keyword.
    pub line: u32,
}

/// A parsed operator text file.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Document {
    pub services: Vec<ServiceDecl>,
    pub images: Vec<ImageDecl>,
    pub os: Option<OsDecl>,
}

/// Parses a text file that declares native services only (an `os` block or an image service is an error here; see
/// [`parse_document`]).
pub fn parse_text(src: &str) -> Result<Vec<ServiceDecl>, ParseError> {
    let doc = parse_document(src)?;
    if let Some(i) = doc.images.first() {
        return err(i.line, "image services are not allowed here");
    }
    match doc.os {
        Some(os) => err(os.line, "`os` blocks are not allowed here"),
        None => Ok(doc.services),
    }
}

fn parse_os(lx: &mut Lexer<'_>, line: u32) -> Result<OsDecl, ParseError> {
    match lx.next()? {
        Some((Tok::LBrace, _)) => {}
        Some((t, l)) => return err(l, format!("expected `{{` after `os`, found {}", describe(&t))),
        None => return err(line, "expected `{` after `os`, found end of input"),
    }
    let mut d = OsDecl { images: Vec::new(), wave: 1, line };
    let mut seen: Vec<String> = Vec::new();
    loop {
        let (key, kline) = match lx.next()? {
            Some((Tok::RBrace, _)) => break,
            Some((Tok::Semi, _)) => continue,
            Some((Tok::Word(k), l)) => (k, l),
            Some((t, l)) => return err(l, format!("expected a key or `}}`, found {}", describe(&t))),
            None => return err(line, "unterminated `os` block (missing `}`)"),
        };
        if seen.contains(&key) {
            return err(kline, format!("duplicate key `{key}`"));
        }
        match lx.next()? {
            Some((Tok::Eq, _)) => {}
            Some((t, l)) => return err(l, format!("expected `=` after `{key}`, found {}", describe(&t))),
            None => return err(kline, format!("expected `=` after `{key}`")),
        }
        let Some((val, vline)) = lx.next()? else { return err(kline, format!("missing value for `{key}`")) };
        match (key.as_str(), val) {
            ("images", Tok::LBracket) => loop {
                match lx.next()? {
                    Some((Tok::RBracket, _)) => break,
                    Some((Tok::Str(s), l)) => {
                        let path = String::from_utf8(s).or_else(|_| err(l, "image path is not UTF-8"))?;
                        if path.is_empty() {
                            return err(l, "empty image path");
                        }
                        d.images.push(path);
                        match lx.next()? {
                            Some((Tok::Comma, _)) => {}
                            Some((Tok::RBracket, _)) => break,
                            Some((t, l)) => return err(l, format!("expected `,` or `]` in `images`, found {}", describe(&t))),
                            None => return err(vline, "unterminated `images` list (missing `]`)"),
                        }
                    }
                    Some((t, l)) => return err(l, format!("`images` expects quoted file names, found {}", describe(&t))),
                    None => return err(vline, "unterminated `images` list (missing `]`)"),
                }
            },
            ("wave", Tok::Word(w)) => {
                d.wave = parse_u32(&w, vline, "wave")?;
                if d.wave == 0 {
                    return err(vline, "`wave` must be >= 1");
                }
            }
            ("images" | "wave", t) => return err(vline, format!("bad value for `{key}`: {}", describe(&t))),
            (_, _) => return err(kline, format!("unknown key `{key}` in `os` (expected images, wave)")),
        }
        seen.push(key);
    }
    if d.images.is_empty() {
        return err(line, "`os` needs `images = [\"FILE.efi\", …]`");
    }
    if d.images.len() > MAX_OS_IMAGES {
        return err(line, format!("more than {MAX_OS_IMAGES} OS images"));
    }
    Ok(d)
}

const NATIVE_KEYS: &[&str] = &["code", "config", "max_instances", "cpu_ms", "fresh"];
const IMAGE_KEYS: &[&str] = &[
    "images", "entrypoint", "args", "workdir", "user", "env", "secrets", "cpu", "pids_max", "fds_max", "http", "routes",
    "public", "egress", "health", "stop_grace", "scratch", "restart", "init", "tenant",
];

/// Parses the operator text format: service declarations (defaults applied, names unique, callees declared) and
/// at most one `os` block. A block with `images` is an image service, otherwise a native one (`code`).
pub fn parse_document(src: &str) -> Result<Document, ParseError> {
    let mut lx = Lexer::new(src);
    let mut out: Vec<ServiceDecl> = Vec::new();
    let mut images: Vec<ImageDecl> = Vec::new();
    let mut os: Option<OsDecl> = None;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // (image service?, declaration index, `calls` line): callees are checked once every service is known.
    let mut calls_at: Vec<(bool, usize, u32)> = Vec::new();
    while let Some((tok, line)) = lx.next()? {
        match tok {
            Tok::Semi => continue,
            Tok::Word(w) if w == "service" => {}
            Tok::Word(w) if w == "os" => {
                if os.is_some() {
                    return err(line, "more than one `os` block");
                }
                os = Some(parse_os(&mut lx, line)?);
                continue;
            }
            other => return err(line, format!("expected `service` or `os`, found {}", describe(&other))),
        }
        let name = match lx.next()? {
            Some((Tok::Word(w), _)) => w,
            Some((Tok::Str(s), l)) => String::from_utf8(s).or_else(|_| err(l, "service name is not UTF-8"))?,
            Some((t, l)) => return err(l, format!("expected a service name, found {}", describe(&t))),
            None => return err(line, "expected a service name, found end of input"),
        };
        if !valid_name(&name) {
            return err(line, format!("invalid service name `{name}` (want [a-z0-9-], 1..=63 chars)"));
        }
        if !seen.insert(name.clone()) {
            return err(line, format!("duplicate service `{name}`"));
        }
        match lx.next()? {
            Some((Tok::LBrace, _)) => {}
            Some((t, l)) => return err(l, format!("expected `{{` after service name, found {}", describe(&t))),
            None => return err(line, "expected `{` after service name, found end of input"),
        }
        let mut d = ServiceDecl {
            name,
            code: String::new(),
            replicas: 1,
            mem_log2: 20,
            max_instances: 64,
            cpu_ms: 1000,
            fresh: false,
            config: Vec::new(),
            calls: Calls::none(),
            line,
        };
        let mut img = ImageService::new(&d.name, Vec::new(), 0);
        let mut secret_files: Vec<(String, String)> = Vec::new();
        let (mut mem, mut scratch): (Option<(u64, u32)>, Option<(u64, u32)>) = (None, None);
        let (mut native_key, mut image_key): (Option<(String, u32)>, Option<(String, u32)>) = (None, None);
        let mut calls_line = None;
        let mut seen: Vec<String> = Vec::new();
        loop {
            let (key, kline) = match lx.next()? {
                Some((Tok::RBrace, _)) => break,
                Some((Tok::Semi, _)) => continue,
                Some((Tok::Word(k), l)) => (k, l),
                Some((t, l)) => return err(l, format!("expected a key or `}}`, found {}", describe(&t))),
                None => return err(line, format!("unterminated block for service `{}` (missing `}}`)", d.name)),
            };
            if seen.contains(&key) {
                return err(kline, format!("duplicate key `{key}`"));
            }
            match lx.next()? {
                Some((Tok::Eq, _)) => {}
                Some((t, l)) => return err(l, format!("expected `=` after `{key}`, found {}", describe(&t))),
                None => return err(kline, format!("expected `=` after `{key}`")),
            }
            let Some((val, vline)) = lx.next()? else { return err(kline, format!("missing value for `{key}`")) };
            if NATIVE_KEYS.contains(&key.as_str()) && native_key.is_none() {
                native_key = Some((key.clone(), kline));
            }
            if IMAGE_KEYS.contains(&key.as_str()) && image_key.is_none() {
                image_key = Some((key.clone(), kline));
            }
            match (key.as_str(), val) {
                ("code", Tok::Str(s)) => {
                    d.code = String::from_utf8(s).or_else(|_| err(vline, "`code` path is not UTF-8"))?;
                    if d.code.is_empty() {
                        return err(vline, "`code` must not be empty");
                    }
                }
                ("config", Tok::Str(s)) => {
                    if s.len() > MAX_CONFIG_BYTES {
                        return err(vline, "`config` larger than 1 MiB");
                    }
                    d.config = s;
                }
                ("replicas", Tok::Word(w)) => {
                    d.replicas = parse_u32(&w, vline, "replicas")?;
                    if d.replicas > MAX_REPLICAS {
                        return err(vline, format!("`replicas` must be <= {MAX_REPLICAS}"));
                    }
                }
                ("max_instances", Tok::Word(w)) => {
                    d.max_instances = parse_u32(&w, vline, "max_instances")?;
                    if d.max_instances == 0 {
                        return err(vline, "`max_instances` must be >= 1");
                    }
                }
                ("cpu_ms", Tok::Word(w)) => {
                    d.cpu_ms = parse_u32(&w, vline, "cpu_ms")?;
                    if d.cpu_ms == 0 {
                        return err(vline, "`cpu_ms` must be >= 1");
                    }
                }
                ("memory", Tok::Word(w)) => {
                    let bytes = parse_size(&mut lx, &w, vline, "memory")?;
                    if !bytes.is_power_of_two() && bytes % image::PAGE != 0 {
                        let image = "a multiple of 4KiB for image services";
                        return err(vline, format!("memory must be a power of two ({image}), got {bytes} bytes"));
                    }
                    if !(1u64 << MIN_MEM_LOG2..=1u64 << MAX_MEM_LOG2).contains(&bytes) {
                        return err(vline, format!("memory must be between 4KiB and 64GiB, got {bytes} bytes"));
                    }
                    mem = Some((bytes, vline));
                }
                ("fresh", Tok::Word(w)) => d.fresh = parse_bool(&w, vline, "fresh")?,
                ("calls", Tok::LBracket) => {
                    d.calls = parse_calls(&mut lx, vline)?;
                    calls_line = Some(vline);
                }
                ("images", Tok::LBrace) => {
                    for (k, v, l) in parse_map(&mut lx, "images", vline)? {
                        let arch = Arch::parse(&k).ok_or_else(|| ParseError {
                            line: l,
                            msg: format!("unknown architecture `{k}` in `images` (want x86_64, aarch64, i686, armv7 or riscv64)"),
                        })?;
                        let hash = match &v {
                            Tok::Str(s) => s.strip_prefix(b"sha256:").and_then(|h| core::str::from_utf8(h).ok()),
                            _ => None,
                        };
                        let Some(hash) = hash.filter(|h| h.len() == 64).and_then(crate::sha256::from_hex) else {
                            return err(l, format!("`images` entry `{k}` wants \"sha256:\" and 64 hex digits (a K10I image)"));
                        };
                        img.images.push(image::ImageRef { arch, hash });
                    }
                    if img.images.is_empty() {
                        return err(vline, "`images` is empty");
                    }
                }
                ("entrypoint" | "args", Tok::LBracket) => {
                    let list = parse_strings(&mut lx, &key, vline)?.into_iter().map(|(s, _)| s).collect();
                    *(if key == "entrypoint" { &mut img.entrypoint } else { &mut img.args }) = Some(list);
                }
                ("workdir", Tok::Str(s)) => {
                    img.workdir = utf8(s, vline, "`workdir`")?;
                    if !img.workdir.starts_with('/') || img.workdir.len() > image::MAX_PATH || img.workdir.contains('\0') {
                        return err(vline, "`workdir` must be an absolute path");
                    }
                }
                ("user", Tok::Str(s)) => {
                    img.user = utf8(s, vline, "`user`")?;
                    if !image::valid_user(&img.user) {
                        return err(vline, format!("invalid user `{}` (want NAME or UID, optionally :GROUP)", img.user));
                    }
                }
                ("tenant", Tok::Str(s)) => {
                    img.tenant = utf8(s, vline, "`tenant`")?;
                    if !valid_name(&img.tenant) {
                        return err(vline, format!("invalid tenant `{}` (want [a-z0-9-], 1..=63 chars)", img.tenant));
                    }
                }
                ("env" | "secrets", Tok::LBrace) => {
                    for (k, v, l) in parse_map(&mut lx, &key, vline)? {
                        if !image::valid_env_key(&k) {
                            return err(l, format!("invalid variable name `{k}` in `{key}`"));
                        }
                        let Tok::Str(v) = v else { return err(l, format!("`{key}` values are quoted strings")) };
                        let v = utf8(v, l, &format!("`{k}` in `{key}`"))?;
                        if key == "env" {
                            if v.contains('\0') {
                                return err(l, format!("`{k}` in `env` contains a NUL byte"));
                            }
                            img.env.push((k, v));
                        } else if v.is_empty() {
                            return err(l, format!("`{k}` in `secrets` needs a file name"));
                        } else {
                            secret_files.push((k, v));
                        }
                    }
                }
                ("cpu", Tok::Word(w)) => img.cpu_milli = parse_cpu(&w, vline)?,
                ("pids_max" | "fds_max", Tok::Word(w)) => {
                    let n = parse_u32(&w, vline, &key)?;
                    let (min, max) = if key == "pids_max" { (1, image::MAX_PIDS) } else { (3, image::MAX_FDS) };
                    if !(min..=max).contains(&n) {
                        return err(vline, format!("`{key}` must be {min}..={max}"));
                    }
                    *(if key == "pids_max" { &mut img.pids_max } else { &mut img.fds_max }) = n;
                }
                ("http", Tok::Word(w)) => img.http = parse_port(&w, vline, "http")?,
                ("routes", Tok::LBracket) => {
                    for (r, l) in parse_strings(&mut lx, "routes", vline)? {
                        if !image::valid_route(&r) {
                            return err(l, format!("invalid route `{r}` (want host[/path] or tls:host, lowercase)"));
                        }
                        if img.routes.contains(&r) {
                            return err(l, format!("route `{r}` listed twice"));
                        }
                        img.routes.push(r);
                    }
                }
                ("egress", Tok::LBracket) => {
                    for (e, l) in parse_strings(&mut lx, "egress", vline)? {
                        let e = image::Egress::parse(&e).or_else(|m| err(l, m))?;
                        if img.egress.contains(&e) {
                            return err(l, format!("egress `{e}` listed twice"));
                        }
                        img.egress.push(e);
                    }
                }
                ("public", Tok::LBracket) => {
                    img.public = parse_public(&mut lx, vline)?;
                    if let Some(p) = img.public.iter().enumerate().find(|(i, p)| img.public[..*i].iter().any(|q| q.port == p.port)) {
                        return err(vline, format!("public port {} listed twice", p.1.port));
                    }
                }
                ("health", Tok::Str(s)) => img.health = image::Health::parse(&utf8(s, vline, "`health`")?).or_else(|m| err(vline, m))?,
                ("stop_grace", Tok::Word(w)) => {
                    img.stop_grace_ms = parse_duration_ms(&w, vline, "stop_grace")?;
                    if img.stop_grace_ms > image::MAX_STOP_GRACE_MS {
                        return err(vline, "`stop_grace` must be at most 1h");
                    }
                }
                ("scratch", Tok::Word(w)) => scratch = Some((parse_size(&mut lx, &w, vline, "scratch")?, vline)),
                ("restart", Tok::Word(w)) => {
                    img.restart = image::Restart::parse(&w)
                        .ok_or_else(|| ParseError { line: vline, msg: format!("`restart` expects always, on-failure or never, got `{w}`") })?;
                }
                ("init", Tok::Word(w)) => img.init = parse_bool(&w, vline, "init")?,
                ("code" | "config" | "workdir" | "user" | "tenant" | "health", t) => {
                    return err(vline, format!("`{key}` expects a quoted string, found {}", describe(&t)))
                }
                ("calls", t) => {
                    let found = describe(&t);
                    return err(vline, format!("`calls` expects a list like [\"echo\", \"kv-store\"], found {found}"));
                }
                ("entrypoint" | "args" | "routes" | "egress" | "public", t) => {
                    return err(vline, format!("`{key}` expects a list like [\"…\"], found {}", describe(&t)))
                }
                ("images" | "env" | "secrets", t) => {
                    return err(vline, format!("`{key}` expects a map like {{ KEY = \"value\" }}, found {}", describe(&t)))
                }
                (k, t) if k == "memory" || NATIVE_KEYS.contains(&k) || IMAGE_KEYS.contains(&k) || k == "replicas" => {
                    return err(vline, format!("bad value for `{key}`: {}", describe(&t)))
                }
                (_, _) => {
                    let keys = "native: code, replicas, memory, max_instances, cpu_ms, fresh, config, calls; image: images, \
                                replicas, memory, cpu, entrypoint, args, env, secrets, workdir, user, http, routes, public, \
                                calls, egress, health, stop_grace, scratch, restart, init, pids_max, fds_max, tenant";
                    return err(kline, format!("unknown key `{key}` (expected {keys})"));
                }
            }
            seen.push(key);
        }
        if img.images.is_empty() {
            if let Some((k, l)) = image_key {
                return err(l, format!("`{k}` is a key of image services, which need `images`"));
            }
            if d.code.is_empty() {
                return err(d.line, format!("service `{}` has no `code` (or `images`, for an image service)", d.name));
            }
            if let Some((bytes, l)) = mem {
                if !bytes.is_power_of_two() {
                    return err(l, format!("memory must be a power of two, got {bytes} bytes"));
                }
                d.mem_log2 = bytes.trailing_zeros() as u8;
            }
            if let Some(l) = calls_line {
                calls_at.push((false, out.len(), l));
            }
            out.push(d);
            continue;
        }
        if let Some((k, l)) = native_key {
            return err(l, format!("`{k}` is a key of native services; `{}` has `images`", d.name));
        }
        let Some((bytes, l)) = mem else {
            return err(d.line, format!("image service `{}` needs `memory` (its exact limit, like 256MiB)", d.name));
        };
        if bytes % image::PAGE != 0 {
            return err(l, format!("image service memory must be a multiple of 4KiB, got {bytes} bytes"));
        }
        img.mem_bytes = bytes;
        img.scratch_bytes = (bytes / 4) & !(image::PAGE - 1);
        if let Some((s, l)) = scratch {
            if s % image::PAGE != 0 || s > bytes {
                return err(l, format!("scratch must be a multiple of 4KiB, at most `memory`, got {s} bytes"));
            }
            img.scratch_bytes = s;
        }
        img.replicas = d.replicas;
        img.calls = d.calls;
        // Remaining cross-field checks (sizes, `routes` without `http`, …); secrets are checked once sealed.
        img.canonicalize();
        img.validate().or_else(|e| err(d.line, e))?;
        if let Some(l) = calls_line {
            calls_at.push((true, images.len(), l));
        }
        images.push(ImageDecl { spec: img, secret_files, line: d.line });
    }
    // The text is the complete desired state: an undeclared callee is a mistake (typo).
    for (is_image, i, line) in calls_at {
        let (name, calls) = if is_image { (&images[i].spec.name, &images[i].spec.calls) } else { (&out[i].name, &out[i].calls) };
        if let Some(c) = calls.names().find(|c| !seen.contains(*c)) {
            return err(line, format!("service `{name}` calls `{c}`, which is not declared in this manifest"));
        }
    }
    Ok(Document { services: out, images, os })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn spec(name: &str, replicas: u32) -> ServiceSpec {
        ServiceSpec {
            name: name.to_string(),
            code_hash: sha256(name.as_bytes()),
            replicas,
            mem_log2: 16,
            max_instances: 64,
            cpu_ms: 50,
            fresh: false,
            config: b"k=v".to_vec(),
            calls: Calls::none(),
        }
    }

    fn calling(name: &str, calls: &[&str]) -> ServiceSpec {
        ServiceSpec { calls: Calls::from_list(calls).unwrap(), ..spec(name, 1) }
    }

    fn names(c: &Calls) -> Vec<&str> {
        c.names().collect()
    }

    /// Format 1 encoding, written exactly as the encoder before `calls` did.
    fn legacy_encode(m: &Manifest) -> Vec<u8> {
        let mut order: Vec<&ServiceSpec> = m.services.iter().collect();
        order.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = b"K10M\x01".to_vec();
        put_u64(&mut out, m.version);
        put_varint(&mut out, order.len() as u64);
        for s in order {
            out.push(s.name.len() as u8);
            out.extend_from_slice(s.name.as_bytes());
            out.extend_from_slice(&s.code_hash);
            put_varint(&mut out, s.replicas as u64);
            out.push(s.mem_log2);
            put_varint(&mut out, s.max_instances as u64);
            put_varint(&mut out, s.cpu_ms as u64);
            out.push(s.fresh as u8);
            put_varint(&mut out, s.config.len() as u64);
            out.extend_from_slice(&s.config);
        }
        out
    }

    /// A format-2 manifest of services named `a`, `b`, ... with the given flags and raw `calls` varints.
    fn format2(records: &[(u8, &[u64])]) -> Vec<u8> {
        let mut out = b"K10M\x02".to_vec();
        put_u64(&mut out, 1);
        put_varint(&mut out, records.len() as u64);
        for (i, (flags, calls)) in records.iter().enumerate() {
            out.extend_from_slice(&[1, b'a' + i as u8]);
            out.extend_from_slice(&[0; 32]);
            out.extend_from_slice(&[1, 16, 1, 1, *flags, 0]);
            put_varint(&mut out, calls.len() as u64);
            calls.iter().for_each(|&c| put_varint(&mut out, c));
        }
        out
    }

    #[test]
    fn calls_roundtrip_format_and_canonical_hash() {
        let a = Manifest {
            version: 3,
            services: vec![
                calling("web", &["kv-store", "echo"]),
                spec("echo", 1),
                calling("kv-store", &["kv-store"]),
                calling("gateway", &["*"]),
            ],
            ..Default::default()
        };
        a.validate().unwrap();
        let bytes = a.encode();
        assert_eq!(bytes[4], 2, "a manifest with calls is format 2");
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(sha256(&bytes), back.hash());
        assert_eq!(back.hash(), a.hash());
        assert!(back.services.iter().all(|s| a.service(&s.name) == Some(s)), "same specs, same grants");
        let web = &back.service("web").unwrap().calls;
        assert_eq!(names(web), ["echo", "kv-store"]);
        assert!(web.allows(b"echo") && web.allows(b"kv-store"));
        assert!(!web.allows(b"web") && !web.allows(b"gateway") && !web.allows(b"nosuch") && !web.allows(b""));
        assert!(!back.service("echo").unwrap().calls.allows(b"echo"), "no calls = none, not even itself");
        assert!(back.service("kv-store").unwrap().calls.allows(b"kv-store"), "self-calls need an entry");
        let gw = &back.service("gateway").unwrap().calls;
        assert!(gw.is_any() && gw.allows(b"web") && gw.allows(b"nosuch") && names(gw).is_empty());
        // Decoded grants are indices into one name table shared by the manifest.
        match (&back.service("web").unwrap().calls.0, &back.service("kv-store").unwrap().calls.0) {
            (Grant::Names { table: t1, idx }, Grant::Names { table: t2, .. }) => {
                assert!(Arc::ptr_eq(t1, t2));
                assert_eq!(&**idx, &[0, 2]);
            }
            _ => panic!("named grants"),
        }
        // Order and duplicates do not matter; the hash covers the grants.
        assert_eq!(Calls::from_list(&["kv-store", "echo", "echo"]).unwrap(), *web);
        let mut c = a.clone();
        c.services[0].calls = Calls::from_list(&["echo"]).unwrap();
        assert_ne!(c.hash(), a.hash());
        // Bit flips and truncations: never a panic; whatever decodes is canonical and valid.
        for n in 0..bytes.len() {
            assert!(Manifest::decode(&bytes[..n]).is_err());
        }
        for i in 0..bytes.len() {
            for bit in 0..8 {
                let mut x = bytes.clone();
                x[i] ^= 1 << bit;
                if let Ok(m) = Manifest::decode(&x) {
                    assert_eq!(m.hash(), sha256(&x), "every accepted encoding is canonical");
                    m.validate().unwrap();
                }
            }
        }
    }

    #[test]
    fn format1_still_decodes_and_keeps_its_hash() {
        let m = Manifest { version: 9, services: vec![spec("web", 2), spec("echo", 1)], ..Default::default() };
        let legacy = legacy_encode(&m);
        assert_eq!(m.encode(), legacy, "without calls: the same bytes as before");
        let back = Manifest::decode(&legacy).unwrap();
        assert_eq!(back, Manifest { version: 9, services: vec![spec("echo", 1), spec("web", 2)], ..Default::default() });
        assert!(back.services.iter().all(|s| s.calls.is_none() && !s.calls.allows(b"echo")));
        assert_eq!(back.hash(), sha256(&legacy));
        // Non-minimal varints and unknown formats are refused (they would not be canonical).
        let mut long = legacy.clone();
        let at = 4 + 1 + 8;
        long.splice(at..at + 1, [0x82, 0x00]);
        assert_eq!(Manifest::decode(&long), Err(DecodeError::Invalid("varint (not minimal)")));
        let mut f3 = legacy;
        f3[4] = 3;
        assert!(Manifest::decode(&f3).is_err());
    }

    #[test]
    fn calls_decode_is_strict() {
        let ok = |b: &[u8]| Manifest::decode(b).map(|m| m.services.iter().map(|s| names(&s.calls).join(",")).collect::<Vec<_>>());
        assert_eq!(ok(&format2(&[(0, &[0, 1]), (0, &[])])), Ok(vec!["a,b".to_string(), String::new()]));
        assert!(Manifest::decode(&format2(&[(FLAG_ANY, &[]), (0, &[])])).unwrap().services[0].calls.is_any());
        let bad = |records: &[(u8, &[u64])], why: &'static str| {
            assert_eq!(Manifest::decode(&format2(records)), Err(DecodeError::Invalid(why)))
        };
        bad(&[(0, &[]), (0, &[])], "manifest format (2 without calls)");
        bad(&[(0, &[2]), (0, &[])], "calls index");
        bad(&[(0, &[1, 1]), (0, &[])], "calls index");
        bad(&[(0, &[1, 0]), (0, &[])], "calls index");
        bad(&[(0, &[0, 1, 1]), (0, &[])], "calls count");
        bad(&[(FLAG_ANY, &[0]), (0, &[])], "calls (any with a list)");
        bad(&[(4, &[0]), (0, &[])], "service flags");
        // A huge count with no data: refused before allocating.
        let mut evil = format2(&[(0, &[])]);
        evil.pop();
        evil.extend_from_slice(&[0xff, 0xff, 0xff, 0x0f]);
        assert!(Manifest::decode(&evil).is_err());
        // An index written in more bytes than needed.
        let mut long = format2(&[(0, &[0])]);
        long.pop();
        long.extend_from_slice(&[0x80, 0x00]);
        assert_eq!(Manifest::decode(&long), Err(DecodeError::Invalid("varint (not minimal)")));
    }

    #[test]
    fn calls_validation() {
        let check = |svcs: Vec<ServiceSpec>| Manifest { version: 1, services: svcs, ..Default::default() }.validate();
        assert!(check(vec![calling("a", &["b"]), spec("b", 1)]).is_ok());
        let e = check(vec![calling("a", &["b", "typo"]), spec("b", 1)]).unwrap_err();
        assert!(e.contains("calls `typo`") && e.contains("not a service"), "{e}");
        assert!(Calls::from_list(&["*", "b"]).unwrap_err().contains("only entry"));
        assert!(Calls::from_list(&["B"]).unwrap_err().contains("invalid service name"));
        assert!(Calls::from_list::<&str>(&[]).unwrap().is_none());
        assert!(Calls::from_list(&["*", "*"]).unwrap().is_any());
    }

    #[test]
    fn text_calls() {
        let src = r#"
service echo { code = "e.ten" }
service chain {
  code = "c.ten"
  calls = ["kv-store", "echo", "echo",]
}
service kv-store { code = "k.ten" calls = [] }
service gw { code = "g.ten" calls = ["*"] }
"#;
        let d = parse_text(src).unwrap();
        assert!(d[0].calls.is_none());
        assert_eq!(names(&d[1].calls), ["echo", "kv-store"]);
        assert!(d[2].calls.is_none());
        assert!(d[3].calls.is_any());
        let m = Manifest { version: 1, services: d.iter().map(|d| d.to_spec([0; 32])).collect(), ..Default::default() };
        m.validate().unwrap();
        assert_eq!(Manifest::decode(&m.encode()).unwrap().hash(), m.hash());
        let cases: &[(&str, u32, &str)] = &[
            ("service a {\n code = \"a\"\n calls = [\"b\"]\n}", 3, "calls `b`, which is not declared"),
            ("service a { code = \"a\" }\nservice b {\n code = \"b\"\n calls = [\"a\", \"ehco\"]\n}", 4, "`ehco`"),
            ("service a {\n code = \"a\"\n calls = [\"*\", \"a\"]\n}", 3, "only entry"),
            ("service a {\n code = \"a\"\n calls = [a]\n}", 3, "quoted service names"),
            ("service a {\n code = \"a\"\n calls = \"a\"\n}", 3, "expects a list"),
            ("service a {\n code = \"a\"\n calls = [\"A\"]\n}", 3, "invalid service name"),
            ("service a {\n code = \"a\"\n calls = [\"a\" \"a\"]\n}", 3, "expected `,` or `]`"),
            ("service a {\n code = \"a\"\n calls = [\"a\",\n", 3, "missing `]`"),
            ("service a {\n code = \"a\"\n calls = [\"a\"]\n calls = []\n}", 4, "duplicate key"),
        ];
        for (src, line, needle) in cases {
            let e = parse_text(src).unwrap_err();
            assert_eq!(e.line, *line, "{src:?} -> {e}");
            assert!(e.msg.contains(needle), "{src:?} -> {e}");
        }
        assert_eq!(names(&parse_text("service a { code = \"a\" calls = [\"a\"] }").unwrap()[0].calls), ["a"]);
    }

    #[test]
    fn binary_roundtrip_canonical_hash() {
        let a = Manifest { version: 7, services: vec![spec("web", 3), spec("echo", 1), spec("db-1", 2)], ..Default::default() };
        let bytes = a.encode();
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.version, 7);
        assert_eq!(back.services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["db-1", "echo", "web"]);
        assert_eq!(back.hash(), a.hash());
        assert_eq!(sha256(&bytes), a.hash());
        // order independence of the hash
        let mut b = a.clone();
        b.services.reverse();
        assert_eq!(b.hash(), a.hash());
        assert!(a.validate().is_ok());
    }

    #[test]
    fn decode_never_panics_on_garbage_or_truncation() {
        let bytes = Manifest { version: 1, services: vec![spec("a", 1), spec("b", 2)], ..Default::default() }.encode();
        for n in 0..bytes.len() {
            assert!(Manifest::decode(&bytes[..n]).is_err());
        }
        // bit flips: must not panic (may or may not decode)
        for i in 0..bytes.len() {
            for bit in 0..8 {
                let mut b = bytes.clone();
                b[i] ^= 1 << bit;
                let _ = Manifest::decode(&b);
            }
        }
        // huge count with no data: no huge allocation, clean error
        let mut evil = b"K10M\x01".to_vec();
        evil.extend_from_slice(&0u64.to_le_bytes());
        evil.extend_from_slice(&[0xff, 0xff, 0x03]);
        assert!(Manifest::decode(&evil).is_err());
        // duplicates rejected
        let dup = Manifest { version: 1, services: vec![spec("a", 1), spec("a", 2)], ..Default::default() };
        assert!(Manifest::decode(&dup.encode()).is_err());
        assert!(dup.validate().is_err());
    }

    #[test]
    fn text_format_full_example() {
        let src = r#"
# comment
service echo {
  code = "echo.ten"      # path; the CLI resolves/compiles it
  replicas = 3
  memory = 64KiB         # power of two
  max_instances = 64
  cpu_ms = 50
  fresh = false
  config = "greeting=hi"
}
service web-2 { code = "web.k10"; memory = 1 MiB; fresh = true }
service minimal {
  code = "m.ten"
}
"#;
        let d = parse_text(src).unwrap();
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].name, "echo");
        assert_eq!(d[0].code, "echo.ten");
        assert_eq!((d[0].replicas, d[0].mem_log2, d[0].max_instances, d[0].cpu_ms, d[0].fresh), (3, 16, 64, 50, false));
        assert_eq!(d[0].config, b"greeting=hi");
        assert_eq!(d[0].line, 3);
        assert_eq!((d[1].mem_log2, d[1].fresh), (20, true));
        // defaults
        assert_eq!((d[2].replicas, d[2].mem_log2, d[2].max_instances, d[2].cpu_ms, d[2].fresh), (1, 20, 64, 1000, false));
        assert!(d[2].config.is_empty());
        let s = d[0].to_spec([1; 32]);
        assert_eq!(s.mem_bytes(), 65536);
    }

    #[test]
    fn text_format_errors_have_line_numbers() {
        let cases: &[(&str, u32, &str)] = &[
            ("service echo {\n code = \"a\"\n memory = 3KiB\n}", 3, "power of two"),
            ("service echo {\n code = \"a\"\n memory = 2KiB\n}", 3, "between 4KiB"),
            ("service echo {\n code = \"a\"\n memory = 128GiB\n}", 3, "between 4KiB"),
            ("service echo {\n code = \"a\"\n memory = 64KB\n}", 3, "unknown memory unit"),
            ("service Echo { code = \"a\" }", 1, "invalid service name"),
            ("service echo { code = \"a\" }\n\nservice echo { code = \"b\" }", 3, "duplicate service"),
            ("service echo {\n replicas = 2\n}", 1, "no `code`"),
            ("service echo {\n code = \"a\"\n color = 1\n}", 3, "unknown key"),
            ("service echo {\n code = \"a\"\n replicas = x\n}", 3, "integer"),
            ("service echo {\n code = \"a\"\n replicas = 1\n replicas = 2\n}", 4, "duplicate key"),
            ("service echo {\n code = \"a\n}", 2, "unterminated string"),
            ("service echo {\n code = \"a\"\n", 1, "missing `}`"),
            ("servise echo {}", 1, "expected `service`"),
            ("service echo {\n code = \"a\"\n fresh = yes\n}", 3, "true or false"),
            ("service echo {\n code = 5\n}", 2, "quoted string"),
            ("service echo {\n code = \"a\"\n cpu_ms = 0\n}", 3, ">= 1"),
            ("service echo {\n code = \"a\"\n replicas = 99999999999\n}", 3, "out of range"),
            ("service echo {\n code = \"a\" @\n}", 2, "unexpected character"),
            ("service aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa { code = \"a\" }", 1, "invalid service name"),
        ];
        for (src, line, needle) in cases {
            let e = parse_text(src).unwrap_err();
            assert_eq!(e.line, *line, "{src:?} -> {e}");
            assert!(e.msg.contains(needle), "{src:?} -> {e}");
        }
    }

    #[test]
    fn text_escapes_and_memory_units() {
        let d = parse_text("service a { code = \"x\" config = \"a\\x00\\\"b\\n\" memory = 4096 }").unwrap();
        assert_eq!(d[0].config, b"a\0\"b\n");
        assert_eq!(d[0].mem_log2, 12);
        let d = parse_text("service a { code = \"x\" memory = 64GiB }").unwrap();
        assert_eq!(d[0].mem_log2, 36);
        assert!(parse_text("").unwrap().is_empty());
        assert!(parse_text("# only a comment\n").unwrap().is_empty());
    }

    #[test]
    fn os_plan_format3_and_text() {
        let img = |p: &str, b: u8| OsImage { platform: p.to_string(), image: [b; 32] };
        let base = Manifest { version: 4, services: vec![spec("web", 2)], ..Default::default() };
        let m = Manifest { os: OsPlan { images: vec![img("x86_64-vm", 1), img("aarch64-pi", 2)], wave: 2 }, ..base.clone() };
        m.validate().unwrap();
        let bytes = m.encode();
        assert_eq!(bytes[4], FORMAT_V3);
        let back = Manifest::decode(&bytes).unwrap();
        // Decoded images come back sorted by platform; the hash does not depend on the order given.
        assert_eq!(back.os.images, vec![img("aarch64-pi", 2), img("x86_64-vm", 1)]);
        assert_eq!((back.os.wave, back.hash(), back.blob_hashes().len()), (2, m.hash(), 3));
        assert_eq!(back.os.image_for("x86_64-vm"), Some(&[1; 32]));
        // Without OS images the earlier formats (and hashes) stay.
        assert_eq!(base.encode()[4], FORMAT_V1);
        for bad in [
            OsPlan { images: vec![img("x86_64-vm", 1)], wave: 0 },
            OsPlan { images: vec![], wave: 1 },
            OsPlan { images: vec![img("x86_64-vm", 1), img("x86_64-vm", 2)], wave: 1 },
            OsPlan { images: vec![img("x86_64", 1)], wave: 1 },
            OsPlan { images: vec![img("X86-vm", 1)], wave: 1 },
        ] {
            assert!(Manifest { os: bad, ..base.clone() }.validate().is_err());
        }
        // Non-canonical encodings are refused: unsorted platforms, wave 0, no images.
        let enc = |images: &[(&str, u8)], wave: u64| {
            let mut out = base.encode();
            out[4] = FORMAT_V3;
            let mut v = Vec::new();
            put_varint(&mut v, 0); // web calls nothing
            let svc_end = out.len();
            out.splice(svc_end..svc_end, v);
            put_varint(&mut out, images.len() as u64);
            for (p, b) in images {
                out.push(p.len() as u8);
                out.extend_from_slice(p.as_bytes());
                out.extend_from_slice(&[*b; 32]);
            }
            put_varint(&mut out, wave);
            out
        };
        assert!(Manifest::decode(&enc(&[("a-b", 1), ("c-d", 2)], 1)).is_ok());
        assert!(Manifest::decode(&enc(&[("c-d", 1), ("a-b", 2)], 1)).is_err());
        assert!(Manifest::decode(&enc(&[("a-b", 1)], 0)).is_err());
        assert!(Manifest::decode(&enc(&[], 1)).is_err());
        for n in 0..bytes.len() {
            assert!(Manifest::decode(&bytes[..n]).is_err());
        }

        let doc = parse_document("service web { code = \"w.k10\" }\nos {\n images = [\"a.efi\", \"b.efi\"]\n wave = 3\n}\n").unwrap();
        assert_eq!(doc.services.len(), 1);
        assert_eq!(doc.os, Some(OsDecl { images: vec!["a.efi".into(), "b.efi".into()], wave: 3, line: 2 }));
        assert_eq!(parse_document("os { images = [\"a.efi\"] }").unwrap().os.unwrap().wave, 1);
        for (src, line, msg) in [
            ("os { wave = 1 }", 1, "needs `images"),
            ("os { images = [\"a\"] wave = 0 }", 1, "`wave` must be >= 1"),
            ("os { images = [\"a\"] }\nos { images = [\"b\"] }", 2, "more than one `os`"),
            ("os { image = [\"a\"] }", 1, "unknown key `image`"),
            ("os { images = [a] }", 1, "quoted file names"),
        ] {
            let e = parse_document(src).unwrap_err();
            assert!(e.line == line && e.msg.contains(msg), "{src}: {e}");
        }
        assert!(parse_text("os { images = [\"a\"] }").is_err());
    }

    fn image_svc(name: &str) -> ImageService {
        ImageService::new(name, vec![image::ImageRef { arch: Arch::X86_64, hash: sha256(name.as_bytes()) }], 64 << 20)
    }

    /// An image service using every field.
    fn full_image(name: &str) -> ImageService {
        let k = crate::secure::ClusterKey::new(&[7; 32]);
        let mut s = ImageService {
            images: vec![
                image::ImageRef { arch: Arch::Aarch64, hash: [2; 32] },
                image::ImageRef { arch: Arch::X86_64, hash: [1; 32] },
            ],
            replicas: 3,
            cpu_milli: 500,
            pids_max: 200,
            fds_max: 70_000,
            entrypoint: Some(vec![]),
            args: Some(vec!["--port".into(), "8080".into(), "".into()]),
            workdir: "/srv/app".into(),
            user: "1000:1000".into(),
            env: vec![("ZED".into(), "z".into()), ("LOG_LEVEL".into(), "info".into()), ("EMPTY".into(), String::new())],
            secrets: vec![("DB_PASSWORD".into(), k.seal_secret(name, "DB_PASSWORD", b"pw"))],
            http: 8080,
            routes: vec!["tls:secure.example.com".into(), "api.example.com".into(), "example.com/api".into()],
            public: vec![image::PublicPort { port: 5432, to: 5433 }, image::PublicPort { port: 22, to: 2222 }],
            calls: Calls::from_list(&["db", name]).unwrap(),
            egress: vec![
                image::Egress::parse("10.1.2.0/24:5432").unwrap(),
                image::Egress::Dns,
                image::Egress::parse("[2001:db8::/32]:443").unwrap(),
                image::Egress::parse("0.0.0.0/0:443").unwrap(),
            ],
            health: image::Health::parse("http:8080/healthz").unwrap(),
            stop_grace_ms: 30_000,
            scratch_bytes: 16 << 20,
            restart: image::Restart::OnFailure,
            init: true,
            tenant: "acme".into(),
            ..ImageService::new(name, vec![], 384 << 20)
        };
        s.canonicalize();
        s
    }

    fn format4_sample() -> Manifest {
        let mut web = calling("web", &["api", "echo"]);
        web.replicas = 2;
        Manifest {
            version: 11,
            services: vec![web, spec("echo", 1), spec("db", 1)],
            image_services: vec![full_image("api"), ImageService { calls: Calls::any(), ..image_svc("cache") }],
            os: OsPlan::default(),
        }
    }

    #[test]
    fn format4_roundtrip_and_canonical_hash() {
        let m = format4_sample();
        m.validate().unwrap();
        let bytes = m.encode();
        assert_eq!((bytes[4], m.format()), (FORMAT_V4, FORMAT_V4));
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(back.hash(), m.hash());
        assert_eq!(sha256(&bytes), back.hash());
        assert_eq!(back.services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["db", "echo", "web"]);
        assert_eq!(back.image_services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["api", "cache"]);
        let api = back.image_service("api").unwrap();
        assert_eq!(api.env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["EMPTY", "LOG_LEVEL", "ZED"], "sorted");
        assert_eq!(api.entrypoint, Some(vec![]), "an empty entrypoint override is kept");
        assert_eq!(api.args.as_ref().unwrap()[0], "--port", "argument order is kept");
        assert_eq!(names(&api.calls), ["api", "db"]);
        assert!(back.image_service("cache").unwrap().calls.is_any());
        assert_eq!(names(&back.service("web").unwrap().calls), ["api", "echo"], "native services call image services");
        assert!(back.service("web").unwrap().calls.allows(b"api"));
        // Calls index one name table shared by both kinds.
        match (&back.service("web").unwrap().calls.0, &api.calls.0) {
            (Grant::Names { table: t1, idx: i1 }, Grant::Names { table: t2, idx: i2 }) => {
                assert!(Arc::ptr_eq(t1, t2));
                assert_eq!((&**i1, &**i2), (&[0u32, 3][..], &[0u32, 2][..]));
            }
            _ => panic!("named grants"),
        }
        let mut shuffled = m.clone();
        shuffled.services.reverse();
        shuffled.image_services.reverse();
        assert_eq!(shuffled.hash(), m.hash(), "service order does not change the hash");
        let api = &mut shuffled.image_services[1];
        api.env.reverse();
        api.routes.reverse();
        api.public.reverse();
        api.egress.reverse();
        api.images.reverse();
        assert!(shuffled.validate().unwrap_err().contains("canonical order"), "lists are canonical in memory");
        shuffled.image_services[1].canonicalize();
        assert_eq!(shuffled.hash(), m.hash());
        let mut other = m.clone();
        other.image_services[0].args = None;
        assert_ne!(other.hash(), m.hash(), "None (the image's Cmd) differs from an override");
        // With an OS plan too, and with no native service at all.
        let with_os = Manifest { os: OsPlan { images: vec![OsImage { platform: "x86_64-vm".into(), image: [9; 32] }], wave: 2 }, ..m.clone() };
        assert_eq!(Manifest::decode(&with_os.encode()).unwrap(), Manifest::decode(&with_os.encode()).unwrap());
        assert_eq!(Manifest::decode(&with_os.encode()).unwrap().os, with_os.os);
        let only = Manifest { version: 1, image_services: vec![image_svc("a")], ..Default::default() };
        assert_eq!(Manifest::decode(&only.encode()).unwrap(), only);
        for n in 0..bytes.len() {
            assert!(Manifest::decode(&bytes[..n]).is_err());
        }
    }

    /// Random mutations of format-4 encodings: never a panic, and whatever decodes is canonical and valid.
    #[test]
    fn format4_canonicality_fuzz() {
        let seeds = [format4_sample().encode(), Manifest { version: 1, image_services: vec![image_svc("a")], ..Default::default() }.encode()];
        let mut rng = crate::codec::Rng::new(0x6b31_3034);
        let (mut accepted, mut tries) = (0, 0);
        for seed in &seeds {
            for i in 0..seed.len() {
                for bit in 0..8 {
                    let mut x = seed.clone();
                    x[i] ^= 1 << bit;
                    tries += 1;
                    if let Ok(m) = Manifest::decode(&x) {
                        accepted += 1;
                        assert_eq!(m.encode(), x, "every accepted encoding is canonical");
                        m.validate().unwrap();
                    }
                }
            }
            for _ in 0..20_000 {
                let mut x = seed.clone();
                for _ in 0..1 + rng.below(3) {
                    let at = rng.below(x.len());
                    match rng.below(4) {
                        0 => x[at] = rng.next_u64() as u8,
                        1 => x.insert(at, rng.next_u64() as u8),
                        2 => drop(x.remove(at)),
                        _ => x[at] = x[at].wrapping_add(1),
                    }
                }
                tries += 1;
                if let Ok(m) = Manifest::decode(&x) {
                    accepted += 1;
                    assert_eq!(m.encode(), x, "every accepted encoding is canonical");
                    m.validate().unwrap();
                }
            }
        }
        assert!(accepted > 0 && accepted < tries / 2, "{accepted} of {tries}");
    }

    #[test]
    fn format4_decode_is_strict() {
        let m = Manifest { version: 1, image_services: vec![ImageService { env: vec![("AA".into(), "1".into()), ("BB".into(), "2".into())], ..image_svc("a") }], ..Default::default() };
        let bytes = m.encode();
        assert!(Manifest::decode(&bytes).is_ok());
        // The same entries out of order.
        let at = bytes.windows(2).position(|w| w == b"AA").unwrap();
        let bt = bytes.windows(2).position(|w| w == b"BB").unwrap();
        let mut swapped = bytes.clone();
        swapped[at..at + 2].copy_from_slice(b"BB");
        swapped[bt..bt + 2].copy_from_slice(b"AA");
        swapped[at + 3] = b'2';
        swapped[bt + 3] = b'1';
        assert_eq!(Manifest::decode(&swapped), Err(DecodeError::Invalid("image service field")));
        // Format 4 is only for manifests with image services; formats 1-3 do not carry them.
        let natives = Manifest { version: 1, services: vec![spec("a", 1)], ..Default::default() };
        let mut f4 = natives.encode();
        f4[4] = FORMAT_V4;
        assert!(Manifest::decode(&f4).is_err());
        let mut f3 = bytes.clone();
        f3[4] = FORMAT_V3;
        assert!(Manifest::decode(&f3).is_err());
        // Unknown kinds, and route or public port clashes between services.
        let kind_at = 4 + 1 + 8 + 1 + 1 + 1;
        let mut k = bytes.clone();
        k[kind_at] = 2;
        assert_eq!(Manifest::decode(&k), Err(DecodeError::Invalid("service kind")));
        let routed = |n: &str| ImageService { http: 80, routes: vec!["a.example".into()], ..image_svc(n) };
        let clash = Manifest { version: 1, image_services: vec![routed("a"), routed("b")], ..Default::default() };
        assert!(clash.validate().unwrap_err().contains("route `a.example` is claimed by `a` and `b`"));
        assert_eq!(Manifest::decode(&clash.encode()), Err(DecodeError::Invalid("manifest (routes or public ports)")));
        let port = |n: &str| ImageService { public: vec![image::PublicPort { port: 443, to: 8443 }], ..image_svc(n) };
        let clash = Manifest { version: 1, image_services: vec![port("a"), port("b")], ..Default::default() };
        assert!(clash.validate().unwrap_err().contains("public port 443"));
        // One name space for both kinds.
        let dup = Manifest { version: 1, services: vec![spec("a", 1)], image_services: vec![image_svc("a")], ..Default::default() };
        assert!(dup.validate().unwrap_err().contains("duplicate service `a`"));
        assert!(Manifest::decode(&dup.encode()).is_err());
        let typo = Manifest { version: 1, image_services: vec![ImageService { calls: Calls::from_list(&["nope"]).unwrap(), ..image_svc("a") }], ..Default::default() };
        assert!(typo.validate().unwrap_err().contains("calls `nope`"));
    }

    #[test]
    fn image_service_validation() {
        let ok = |s: ImageService| Manifest { version: 1, image_services: vec![s], ..Default::default() }.validate();
        assert!(ok(full_image("api")).is_err(), "calls `db`, which is not in this manifest");
        let full = ImageService { calls: Calls::none(), ..full_image("api") };
        ok(full.clone()).unwrap();
        let bad: Vec<(ImageService, &str)> = vec![
            (ImageService { images: vec![], ..full.clone() }, "no image"),
            (ImageService { images: vec![full.images[0], full.images[0]], ..full.clone() }, "two images for x86_64"),
            (ImageService { mem_bytes: (64 << 20) + 1, ..full.clone() }, "multiple of 4KiB"),
            (ImageService { mem_bytes: 128 << 30, ..full.clone() }, "multiple of 4KiB between"),
            (ImageService { scratch_bytes: 512 << 20, ..full.clone() }, "scratch"),
            (ImageService { cpu_milli: image::MAX_CPU_MILLI + 1, ..full.clone() }, "cpu"),
            (ImageService { fds_max: 2, ..full.clone() }, "fds_max"),
            (ImageService { workdir: "srv".into(), ..full.clone() }, "absolute"),
            (ImageService { user: "a:b:c".into(), ..full.clone() }, "invalid user"),
            (ImageService { env: vec![("A=B".into(), "x".into())], ..full.clone() }, "variable name"),
            (ImageService { env: vec![("A".into(), "x".into()), ("A".into(), "y".into())], ..full.clone() }, "set twice"),
            (ImageService { env: vec![("B".into(), "x".into()), ("A".into(), "y".into())], ..full.clone() }, "canonical order"),
            (ImageService { env: vec![("DB_PASSWORD".into(), "x".into())], ..full.clone() }, "both in env and in secrets"),
            (ImageService { secrets: vec![("S".into(), vec![1, 2, 3])], ..full.clone() }, "invalid secret"),
            (ImageService { args: Some(vec!["a\0b".into()]), ..full.clone() }, "NUL"),
            (ImageService { args: Some(vec!["x".repeat(300 << 10)]), ..full.clone() }, "exceed 256 KiB"),
            (ImageService { routes: vec!["API.example.com".into()], ..full.clone() }, "invalid route"),
            (ImageService { routes: vec!["tls:a.example/x".into()], ..full.clone() }, "invalid route"),
            (ImageService { http: 0, ..full.clone() }, "routes need `http`"),
            (ImageService { public: vec![image::PublicPort { port: 1, to: 0 }], ..full.clone() }, "out of range"),
            (ImageService { egress: vec![image::Egress::Dns, image::Egress::Dns], ..full.clone() }, "listed twice"),
            (ImageService { tenant: "Acme".into(), ..full.clone() }, "invalid tenant"),
            (ImageService { stop_grace_ms: image::MAX_STOP_GRACE_MS + 1, ..full.clone() }, "stop_grace"),
        ];
        for (s, needle) in bad {
            let e = ok(s).unwrap_err();
            assert!(e.contains(needle) && e.starts_with("image service `api`"), "{needle}: {e}");
        }
        for (src, want) in [
            ("process", image::Health::Process),
            ("tcp:5432", image::Health::Tcp { port: 5432 }),
            ("http:80", image::Health::Http { port: 80, path: "/".into() }),
            ("exec:/bin/check  --quick", image::Health::Exec(vec!["/bin/check".into(), "--quick".into()])),
        ] {
            assert_eq!(image::Health::parse(src).unwrap(), want);
            assert_eq!(image::Health::parse(&want.to_string()).unwrap(), want);
        }
        for bad in ["http:0/x", "tcp:x", "exec:", "http:80/a b", "ping"] {
            assert!(image::Health::parse(bad).is_err(), "{bad}");
        }
        for (src, shown) in [
            ("dns", "dns"),
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("10.1.2.3:53", "10.1.2.3/32:53"),
            ("2001:db8::/32", "2001:db8::/32"),
            ("[::1]:443", "[::1/128]:443"),
        ] {
            let e = image::Egress::parse(src).unwrap();
            assert_eq!(e.to_string(), shown);
            assert_eq!(image::Egress::parse(shown).unwrap(), e);
        }
        for bad in ["10.0.0.1/8", "10.0.0.0/33", "10.0.0.0/8:0", "host:80", "[::1]", "::1:80:x"] {
            assert!(image::Egress::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn text_image_services() {
        let h = |b: u8| alloc::format!("sha256:{}", crate::sha256::to_hex(&[b; 32]));
        let src = alloc::format!(
            r#"
service api {{
  images = {{ x86_64 = "{}", aarch64 = "{}" }}
  replicas = 3
  memory = 384MiB                              # exact
  cpu = 0.5
  args = ["--port", "8080"]
  entrypoint = []
  env = {{ LOG_LEVEL = "info", "my.var" = "x" }}
  secrets = {{ DB_PASSWORD = "secrets/db-password" }}
  http = 8080
  routes = ["api.example.com", "tls:secure.example.com"]
  public = [{{ port = 5432, to = 5433 }}, {{ port = 22 }}]
  calls = ["db"]
  egress = ["0.0.0.0/0:443", "dns"]
  health = "http:8080/healthz"
  stop_grace = 30s
  scratch = 64MiB
  restart = on-failure
  init = true
  workdir = "/srv"
  user = "app:app"
  pids_max = 100
  fds_max = 65536
  tenant = "acme"
}}
service db {{ code = "db.ten" calls = ["api"] }}
service tiny {{ images = {{ x86_64 = "{}" }}; memory = 6MiB }}
"#,
            h(1),
            h(2),
            h(3)
        );
        let doc = parse_document(&src).unwrap();
        assert_eq!((doc.services.len(), doc.images.len()), (1, 2));
        let api = &doc.images[0];
        assert_eq!((api.line, api.spec.mem_bytes, api.spec.cpu_milli, api.spec.replicas), (2, 384 << 20, 500, 3));
        assert_eq!(api.spec.image_for(Arch::Aarch64 as u8), Some(&[2; 32]));
        assert_eq!(api.secret_files, [("DB_PASSWORD".to_string(), "secrets/db-password".to_string())]);
        assert!(api.spec.secrets.is_empty(), "sealed by the CLI");
        assert_eq!(api.spec.public, [image::PublicPort { port: 22, to: 22 }, image::PublicPort { port: 5432, to: 5433 }]);
        assert_eq!((api.spec.stop_grace_ms, api.spec.scratch_bytes, api.spec.restart), (30_000, 64 << 20, image::Restart::OnFailure));
        assert_eq!(api.spec.env[1], ("my.var".to_string(), "x".to_string()));
        assert_eq!(names(&api.spec.calls), ["db"]);
        let tiny = &doc.images[1].spec;
        assert_eq!((tiny.mem_bytes, tiny.scratch_bytes, tiny.pids_max, tiny.args.is_none()), (6 << 20, 1536 << 10, image::DEFAULT_PIDS, true));
        let mut m = Manifest {
            version: 1,
            services: doc.services.iter().map(|d| d.to_spec([0; 32])).collect(),
            image_services: doc.images.iter().map(|d| d.spec.clone()).collect(),
            ..Default::default()
        };
        m.image_services[0].secrets = vec![("DB_PASSWORD".into(), crate::secure::ClusterKey::new(&[1; 32]).seal_secret("api", "DB_PASSWORD", b"x"))];
        m.validate().unwrap();
        assert_eq!(Manifest::decode(&m.encode()).unwrap().hash(), m.hash());
        let img = |body: &str| alloc::format!("service a {{\n images = {{ x86_64 = \"{}\" }}\n memory = 64MiB\n{body}\n}}", h(1));
        let cases: Vec<(String, u32, &str)> = vec![
            (img(" code = \"a.ten\""), 4, "key of native services"),
            ("service a {\n code = \"a\"\n http = 80\n}".into(), 3, "key of image services"),
            ("service a {\n images = { x86_64 = \"sha256:00\" }\n memory = 1MiB\n}".into(), 2, "64 hex digits"),
            ("service a {\n images = { sparc = \"x\" }\n}".into(), 2, "unknown architecture `sparc`"),
            ("service a {\n images = {}\n}".into(), 2, "`images` is empty"),
            (alloc::format!("service a {{\n images = {{ x86_64 = \"{}\" }}\n}}", h(1)), 1, "needs `memory`"),
            (img(" memory = 1MiB"), 4, "duplicate key"),
            (alloc::format!("service a {{\n images = {{ x86_64 = \"{}\" }}\n memory = 6000\n}}", h(1)), 3, "multiple of 4KiB"),
            (img(" cpu = 0.0005"), 4, "at most 3 decimals"),
            (img(" cpu = 2000"), 4, "above 1024 cores"),
            (img(" env = { A = \"1\" A = \"2\" }"), 4, "set twice in `env`"),
            (img(" env = { \"A=B\" = \"1\" }"), 4, "invalid variable name"),
            (img(" env = [\"A\"]"), 4, "expects a map"),
            (img(" routes = [\"Bad\"]"), 4, "invalid route"),
            (img(" routes = [\"a.example\"]"), 1, "routes need `http`"),
            (img(" egress = [\"10.0.0.1/8\"]"), 4, "host bits set"),
            (img(" public = [{ to = 5 }]"), 4, "need `port`"),
            (img(" public = [{ port = 5 }, { port = 5 }]"), 4, "listed twice"),
            (img(" public = [{ port = 70000 }]"), 4, "1..65535"),
            (img(" health = \"ping\""), 4, "unknown health check"),
            (img(" stop_grace = 10"), 4, "duration"),
            (img(" stop_grace = 2h"), 4, "duration"),
            (img(" scratch = 128MiB"), 4, "at most `memory`"),
            (img(" restart = sometimes"), 4, "always, on-failure or never"),
            (img(" workdir = \"tmp\""), 4, "absolute path"),
            (img(" user = \"a b\""), 4, "invalid user"),
            (img(" tenant = \"A\""), 4, "invalid tenant"),
            (img(" pids_max = 0"), 4, "`pids_max` must be"),
            (img(" calls = [\"b\"]"), 4, "calls `b`, which is not declared"),
            (img(" secrets = { S = \"\" }"), 4, "needs a file name"),
        ];
        for (src, line, needle) in &cases {
            let e = parse_document(src).unwrap_err();
            assert!(e.line == *line && e.msg.contains(needle), "{src:?} -> {e}");
        }
        assert!(parse_text(&img("")).unwrap_err().msg.contains("image services are not allowed here"));
    }
}
