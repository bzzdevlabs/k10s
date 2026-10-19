//! The desired-state document: a versioned list of services. It is small, content-addressed
//! (`hash = sha256(encode())`) and spread by gossip as a pointer `(version, hash, origin)`; the bytes themselves are
//! fetched over the peer protocol.
//!
//! Binary format (canonical: services sorted by name, so equal manifests have equal hashes):
//! ```text
//! "K10M" | format:u8=1 | version:u64 LE | count:varint | service*
//! service = name_len:u8 name | code_hash:[32] | replicas:varint | mem_log2:u8 | max_instances:varint
//!           | cpu_ms:varint | flags:u8 (bit0 fresh) | config_len:varint config
//! ```

use crate::codec::{put_u64, put_varint, DecodeError, Reader};
use crate::sha256::sha256;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

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

const MAGIC: &[u8; 4] = b"K10M";
const FORMAT: u8 = 1;
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

/// The desired state of the cluster.
#[derive(Clone, PartialEq, Eq, Debug, Default, Hash)]
pub struct Manifest {
    pub version: u64,
    pub services: Vec<ServiceSpec>,
}

impl Manifest {
    /// Canonical encoding (services sorted by name).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let mut order: Vec<&ServiceSpec> = self.services.iter().collect();
        order.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend_from_slice(MAGIC);
        out.push(FORMAT);
        put_u64(out, self.version);
        put_varint(out, order.len() as u64);
        for s in order {
            let name = s.name.as_bytes();
            let n = name.len().min(255);
            out.push(n as u8);
            out.extend_from_slice(&name[..n]);
            out.extend_from_slice(&s.code_hash);
            put_varint(out, s.replicas as u64);
            out.push(s.mem_log2);
            put_varint(out, s.max_instances as u64);
            put_varint(out, s.cpu_ms as u64);
            out.push(s.fresh as u8);
            put_varint(out, s.config.len() as u64);
            out.extend_from_slice(&s.config);
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
        if r.u8()? != FORMAT {
            return Err(DecodeError::Invalid("manifest format"));
        }
        let version = r.u64()?;
        let count = r.varint()?;
        if count > MAX_SERVICES as u64 {
            return Err(DecodeError::TooLarge("service count"));
        }
        let count = count as usize;
        if count > r.remaining() / MIN_SERVICE_BYTES {
            return Err(DecodeError::Truncated);
        }
        let mut services: Vec<ServiceSpec> = Vec::with_capacity(count);
        for _ in 0..count {
            let n = r.u8()? as usize;
            let name = core::str::from_utf8(r.bytes(n)?).map_err(|_| DecodeError::Invalid("service name"))?;
            if !valid_name(name) {
                return Err(DecodeError::Invalid("service name"));
            }
            if let Some(prev) = services.last() {
                if prev.name.as_str() >= name {
                    return Err(DecodeError::Invalid("service order (must be sorted, unique)"));
                }
            }
            let code_hash = r.array::<32>()?;
            let replicas = r.varint_u32()?;
            let mem_log2 = r.u8()?;
            let max_instances = r.varint_u32()?;
            let cpu_ms = r.varint_u32()?;
            let flags = r.u8()?;
            if flags & !1 != 0 {
                return Err(DecodeError::Invalid("service flags"));
            }
            let clen = r.varint()?;
            if clen > MAX_CONFIG_BYTES as u64 {
                return Err(DecodeError::TooLarge("config"));
            }
            let config = r.bytes(clen as usize)?.to_vec();
            let s = ServiceSpec {
                name: name.to_string(),
                code_hash,
                replicas,
                mem_log2,
                max_instances,
                cpu_ms,
                fresh: flags & 1 != 0,
                config,
            };
            s.validate().map_err(|_| DecodeError::Invalid("service field"))?;
            services.push(s);
        }
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes"));
        }
        Ok(Manifest { version, services })
    }

    /// Content hash: SHA-256 of the canonical encoding.
    pub fn hash(&self) -> [u8; 32] {
        sha256(&self.encode())
    }

    /// Checks names, uniqueness and field ranges (what [`Manifest::decode`] would reject).
    pub fn validate(&self) -> Result<(), String> {
        let mut names: Vec<&str> = Vec::with_capacity(self.services.len());
        for s in &self.services {
            s.validate()?;
            names.push(&s.name);
        }
        if self.services.len() > MAX_SERVICES {
            return Err("too many services".to_string());
        }
        names.sort_unstable();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("duplicate service `{}`", w[0]));
        }
        Ok(())
    }

    pub fn service(&self, name: &str) -> Option<&ServiceSpec> {
        self.services.iter().find(|s| s.name == name)
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
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
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

/// Parses `64KiB`, `1MiB`, `4096`, `64 KiB` (the unit may be a separate word) into log2(bytes).
fn parse_memory(lx: &mut Lexer<'_>, word: &str, line: u32) -> Result<u8, ParseError> {
    let split = word.find(|c: char| !c.is_ascii_digit() && c != '_').unwrap_or(word.len());
    let (num, mut unit) = (&word[..split], word[split..].to_string());
    let digits: String = num.chars().filter(|&c| c != '_').collect();
    if digits.is_empty() {
        return err(line, format!("`memory` expects a size like 64KiB, got `{word}`"));
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
        return err(line, format!("unknown memory unit `{unit}` (use B, KiB, MiB or GiB)"));
    };
    let Ok(n) = digits.parse::<u64>() else { return err(line, format!("memory value `{word}` out of range")) };
    let Some(bytes) = n.checked_mul(mult) else { return err(line, format!("memory value `{word}` out of range")) };
    if !bytes.is_power_of_two() {
        return err(line, format!("memory must be a power of two, got {bytes} bytes"));
    }
    let log2 = bytes.trailing_zeros() as u8;
    if !(MIN_MEM_LOG2..=MAX_MEM_LOG2).contains(&log2) {
        return err(line, format!("memory must be between 4KiB and 64GiB, got {bytes} bytes"));
    }
    Ok(log2)
}

/// Parses the operator text format into service declarations (defaults applied, names validated and unique).
pub fn parse_text(src: &str) -> Result<Vec<ServiceDecl>, ParseError> {
    let mut lx = Lexer::new(src);
    let mut out: Vec<ServiceDecl> = Vec::new();
    while let Some((tok, line)) = lx.next()? {
        match tok {
            Tok::Semi => continue,
            Tok::Word(w) if w == "service" => {}
            other => return err(line, format!("expected `service`, found {}", describe(&other))),
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
        if out.iter().any(|d| d.name == name) {
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
            line,
        };
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
                ("memory", Tok::Word(w)) => d.mem_log2 = parse_memory(&mut lx, &w, vline)?,
                ("fresh", Tok::Word(w)) => {
                    d.fresh = match w.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => return err(vline, format!("`fresh` expects true or false, got `{w}`")),
                    }
                }
                ("code" | "config", t) => return err(vline, format!("`{key}` expects a quoted string, found {}", describe(&t))),
                ("replicas" | "max_instances" | "cpu_ms" | "memory" | "fresh", t) => {
                    return err(vline, format!("bad value for `{key}`: {}", describe(&t)))
                }
                (_, _) => {
                    return err(
                        kline,
                        format!("unknown key `{key}` (expected code, replicas, memory, max_instances, cpu_ms, fresh, config)"),
                    )
                }
            }
            seen.push(key);
        }
        if d.code.is_empty() {
            return err(d.line, format!("service `{}` has no `code`", d.name));
        }
        out.push(d);
    }
    Ok(out)
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
        }
    }

    #[test]
    fn binary_roundtrip_canonical_hash() {
        let a = Manifest { version: 7, services: vec![spec("web", 3), spec("echo", 1), spec("db-1", 2)] };
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
        let bytes = Manifest { version: 1, services: vec![spec("a", 1), spec("b", 2)] }.encode();
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
        let dup = Manifest { version: 1, services: vec![spec("a", 1), spec("a", 2)] };
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
}
