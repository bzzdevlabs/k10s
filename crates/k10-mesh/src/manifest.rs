//! The desired-state document: a versioned list of services. It is small, content-addressed
//! (`hash = sha256(encode())`) and spread by gossip as a pointer `(version, hash, origin)`; the bytes themselves are
//! fetched over the peer protocol.
//!
//! Binary format (canonical: services sorted by name, so equal manifests have equal hashes):
//! ```text
//! "K10M" | format:u8 (1 or 2) | version:u64 LE | count:varint | service*
//! service = name_len:u8 name | code_hash:[32] | replicas:varint | mem_log2:u8 | max_instances:varint
//!           | cpu_ms:varint | flags:u8 (bit0 fresh) | config_len:varint config
//!           | calls_count:varint (call_len:u8 call)*            -- format 2 only
//! ```
//!
//! Format 2 adds the `calls` allowlists. A manifest without any encodes as format 1 (the original bytes): one encoding
//! per manifest, older manifests keep their hash, and format 1 decodes to empty lists (deny by default).

use crate::codec::{put_u64, put_varint, DecodeError, Reader};
use crate::sha256::sha256;
use alloc::collections::BTreeSet;
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
/// Maximum `calls` entries in one manifest (bounds what [`Manifest::decode`] allocates).
pub const MAX_CALL_ENTRIES: usize = 1 << 20;
/// `calls = ["*"]`: may call any service (discouraged: ambient authority again); valid only alone.
pub const CALL_ANY: &str = "*";

const MAGIC: &[u8; 4] = b"K10M";
/// The original format (no `calls`): used whenever no service may call anything.
const FORMAT_V1: u8 = 1;
/// Format 1 + a `calls` list per service.
const FORMAT_CALLS: u8 = 2;
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
    /// Services this one may `call`; empty = none, not even itself. Canonical: sorted, unique services of this
    /// manifest, or exactly `["*"]` ([`CALL_ANY`]).
    pub calls: Vec<String>,
}

impl ServiceSpec {
    /// Slot size of one instance in bytes.
    pub fn mem_bytes(&self) -> u64 {
        1u64 << self.mem_log2.min(63)
    }

    /// Whether `name` (a call target's name part) is granted: binary search, no allocation, no hashing.
    #[inline]
    pub fn may_call(&self, name: &[u8]) -> bool {
        match self.calls.first() {
            None => false,
            Some(first) if first.as_str() == CALL_ANY => true,
            Some(_) => self.calls.binary_search_by(|c| c.as_bytes().cmp(name)).is_ok(),
        }
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
        check_calls(&self.calls).map_err(|e| format!("service `{}`: {e}", self.name))
    }
}

/// Checks that a `calls` list is canonical: valid names, strictly sorted, `*` alone (existence: per manifest).
fn check_calls(calls: &[String]) -> Result<(), String> {
    if calls.len() == 1 && calls[0] == CALL_ANY {
        return Ok(());
    }
    if calls.len() > MAX_SERVICES {
        return Err("`calls` lists more services than a manifest can hold".to_string());
    }
    for (i, c) in calls.iter().enumerate() {
        if c == CALL_ANY {
            return Err("`\"*\"` (any service) must be the only entry of `calls`".to_string());
        }
        if !valid_name(c) {
            return Err(format!("invalid service name `{c}` in `calls`"));
        }
        if i > 0 && calls[i - 1] >= *c {
            return Err("`calls` must be sorted and without duplicates (see `canonical_calls`)".to_string());
        }
    }
    Ok(())
}

/// Sorts and deduplicates a `calls` list.
pub fn canonical_calls<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    let mut v: Vec<String> = names.iter().map(|n| n.as_ref().to_string()).collect();
    v.sort_unstable();
    v.dedup();
    v
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
        let format = if order.iter().any(|s| !s.calls.is_empty()) { FORMAT_CALLS } else { FORMAT_V1 };
        out.extend_from_slice(MAGIC);
        out.push(format);
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
            if format == FORMAT_CALLS {
                // Canonical bytes even from an unsorted list (which `validate` rejects anyway).
                let mut calls: Vec<&str> = s.calls.iter().map(String::as_str).collect();
                if !calls.windows(2).all(|w| w[0] < w[1]) {
                    calls.sort_unstable();
                    calls.dedup();
                }
                put_varint(out, calls.len() as u64);
                for c in calls {
                    let c = c.as_bytes();
                    let n = c.len().min(255);
                    out.push(n as u8);
                    out.extend_from_slice(&c[..n]);
                }
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
        if format != FORMAT_V1 && format != FORMAT_CALLS {
            return Err(DecodeError::Invalid("manifest format"));
        }
        let version = r.u64()?;
        let count = r.varint()?;
        if count > MAX_SERVICES as u64 {
            return Err(DecodeError::TooLarge("service count"));
        }
        let count = count as usize;
        let min_service = MIN_SERVICE_BYTES + (format == FORMAT_CALLS) as usize;
        if count > r.remaining() / min_service {
            return Err(DecodeError::Truncated);
        }
        let mut call_entries = 0usize;
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
            let mut calls = Vec::new();
            if format == FORMAT_CALLS {
                let n = r.varint()?;
                // Entries take >= 2 bytes: bound the count before allocating.
                if n > (r.remaining() / 2) as u64 {
                    return Err(DecodeError::Truncated);
                }
                call_entries += n as usize;
                if call_entries > MAX_CALL_ENTRIES {
                    return Err(DecodeError::TooLarge("calls"));
                }
                calls.reserve_exact(n as usize);
                for _ in 0..n {
                    let len = r.u8()? as usize;
                    let c = core::str::from_utf8(r.bytes(len)?).map_err(|_| DecodeError::Invalid("calls entry"))?;
                    calls.push(c.to_string());
                }
            }
            let s = ServiceSpec {
                name: name.to_string(),
                code_hash,
                replicas,
                mem_log2,
                max_instances,
                cpu_ms,
                fresh: flags & 1 != 0,
                config,
                calls,
            };
            s.validate().map_err(|_| DecodeError::Invalid("service field"))?;
            services.push(s);
        }
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes"));
        }
        if format == FORMAT_CALLS && services.iter().all(|s| s.calls.is_empty()) {
            // Without any `calls` the canonical encoding is format 1.
            return Err(DecodeError::Invalid("manifest format (2 without calls)"));
        }
        // Every callee must be a service of this manifest (services are sorted).
        for s in &services {
            for c in &s.calls {
                if c != CALL_ANY && services.binary_search_by(|t| t.name.as_str().cmp(c)).is_err() {
                    return Err(DecodeError::Invalid("calls entry (not a service of the manifest)"));
                }
            }
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
        // The manifest is the complete desired state: an undeclared callee is a mistake.
        let mut entries = 0usize;
        for s in &self.services {
            entries += s.calls.len();
            for c in &s.calls {
                if c != CALL_ANY && names.binary_search(&c.as_str()).is_err() {
                    return Err(format!("service `{}` calls `{c}`, which is not a service of this manifest", s.name));
                }
            }
        }
        if entries > MAX_CALL_ENTRIES {
            return Err(format!("too many `calls` entries ({entries} > {MAX_CALL_ENTRIES})"));
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
    /// `calls = [...]`, canonical (sorted, unique; every name declared in the same text).
    pub calls: Vec<String>,
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

/// Parses a `calls` list after its `[`: quoted names, comma-separated (trailing comma allowed); returns it canonical.
fn parse_calls(lx: &mut Lexer<'_>, line: u32) -> Result<Vec<String>, ParseError> {
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
    let list = canonical_calls(&list);
    if list.len() > 1 && list[0] == CALL_ANY {
        return err(line, "`\"*\"` (any service) must be the only entry of `calls`");
    }
    Ok(list)
}

/// Parses the operator text format into service declarations (defaults applied, names unique, callees declared).
pub fn parse_text(src: &str) -> Result<Vec<ServiceDecl>, ParseError> {
    let mut lx = Lexer::new(src);
    let mut out: Vec<ServiceDecl> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // (declaration index, `calls` line): callees are checked once every service is known.
    let mut calls_at: Vec<(usize, u32)> = Vec::new();
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
            calls: Vec::new(),
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
                ("calls", Tok::LBracket) => {
                    d.calls = parse_calls(&mut lx, vline)?;
                    calls_at.push((out.len(), vline));
                }
                ("code" | "config", t) => return err(vline, format!("`{key}` expects a quoted string, found {}", describe(&t))),
                ("calls", t) => {
                    let found = describe(&t);
                    return err(vline, format!("`calls` expects a list like [\"echo\", \"kv-store\"], found {found}"));
                }
                ("replicas" | "max_instances" | "cpu_ms" | "memory" | "fresh", t) => {
                    return err(vline, format!("bad value for `{key}`: {}", describe(&t)))
                }
                (_, _) => {
                    let keys = "code, replicas, memory, max_instances, cpu_ms, fresh, config, calls";
                    return err(kline, format!("unknown key `{key}` (expected {keys})"));
                }
            }
            seen.push(key);
        }
        if d.code.is_empty() {
            return err(d.line, format!("service `{}` has no `code`", d.name));
        }
        out.push(d);
    }
    // The text is the complete desired state: an undeclared callee is a mistake (typo).
    for (i, line) in calls_at {
        let d = &out[i];
        if let Some(c) = d.calls.iter().find(|c| c.as_str() != CALL_ANY && !seen.contains(c.as_str())) {
            return err(line, format!("service `{}` calls `{c}`, which is not declared in this manifest", d.name));
        }
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
            calls: Vec::new(),
        }
    }

    fn calling(name: &str, calls: &[&str]) -> ServiceSpec {
        ServiceSpec { calls: canonical_calls(calls), ..spec(name, 1) }
    }

    /// Format 1 encoding, written exactly as the pre-`calls` encoder did.
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
        };
        a.validate().unwrap();
        let bytes = a.encode();
        assert_eq!(bytes[4], 2, "a manifest with calls is format 2");
        let back = Manifest::decode(&bytes).unwrap();
        assert_eq!(sha256(&bytes), back.hash());
        assert_eq!(back.hash(), a.hash());
        let web = back.service("web").unwrap();
        assert_eq!(web.calls, ["echo", "kv-store"]);
        assert!(web.may_call(b"echo") && web.may_call(b"kv-store"));
        assert!(!web.may_call(b"web") && !web.may_call(b"gateway") && !web.may_call(b"nosuch") && !web.may_call(b""));
        assert!(!back.service("echo").unwrap().may_call(b"echo"), "no calls = none, not even itself");
        assert!(back.service("kv-store").unwrap().may_call(b"kv-store"), "self-calls need an entry");
        let gw = back.service("gateway").unwrap();
        assert!(gw.may_call(b"web") && gw.may_call(b"nosuch"));
        // The hash covers the allowlist, not the order it was written in.
        let mut b = a.clone();
        b.services[0].calls = vec!["kv-store".into(), "echo".into(), "echo".into()];
        assert_eq!(b.hash(), a.hash(), "encoding sorts and deduplicates");
        assert!(b.validate().is_err(), "but the in-memory list must be canonical (binary search)");
        let mut c = a.clone();
        c.services[0].calls = vec!["echo".into()];
        assert_ne!(c.hash(), a.hash());
        // Bit flips and truncations: never a panic.
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
        let m = Manifest { version: 9, services: vec![spec("web", 2), spec("echo", 1)] };
        let legacy = legacy_encode(&m);
        // Without calls: the same bytes as before; a stored format-1 manifest decodes to deny-all.
        assert_eq!(m.encode(), legacy);
        let back = Manifest::decode(&legacy).unwrap();
        assert_eq!(back, Manifest { version: 9, services: vec![spec("echo", 1), spec("web", 2)] });
        assert!(back.services.iter().all(|s| s.calls.is_empty() && !s.may_call(b"echo")));
        assert_eq!(back.hash(), sha256(&legacy));
        // Format 2 with only empty lists is not canonical; unknown formats are refused.
        let mut f2 = b"K10M\x02".to_vec();
        put_u64(&mut f2, 9);
        put_varint(&mut f2, 2);
        for s in [spec("echo", 1), spec("web", 2)] {
            let one = legacy_encode(&Manifest { version: 0, services: vec![s] });
            f2.extend_from_slice(&one[5 + 8 + 1..]); // the service record (after magic, format, version, count)
            f2.push(0);
        }
        assert_eq!(Manifest::decode(&f2), Err(DecodeError::Invalid("manifest format (2 without calls)")));
        let mut f3 = legacy;
        f3[4] = 3;
        assert!(Manifest::decode(&f3).is_err());
    }

    #[test]
    fn calls_validation() {
        let ok = |svcs: Vec<ServiceSpec>| Manifest { version: 1, services: svcs }.validate();
        assert!(ok(vec![calling("a", &["b"]), spec("b", 1)]).is_ok());
        let e = ok(vec![calling("a", &["b", "typo"]), spec("b", 1)]).unwrap_err();
        assert!(e.contains("calls `typo`") && e.contains("not a service"), "{e}");
        assert!(ok(vec![calling("a", &["*", "b"]), spec("b", 1)]).unwrap_err().contains("only entry"));
        assert!(ok(vec![calling("a", &["B"])]).unwrap_err().contains("invalid service name"));
        let unsorted = ServiceSpec { calls: vec!["b".into(), "a".into()], ..spec("a", 1) };
        assert!(ok(vec![unsorted, spec("b", 1)]).unwrap_err().contains("sorted"));
        // The decoder refuses what validate refuses.
        let typo = Manifest { version: 1, services: vec![calling("a", &["nosuch"])] };
        assert!(Manifest::decode(&typo.encode()).is_err());
        let mixed = Manifest { version: 1, services: vec![calling("a", &["*", "a"])] };
        assert!(Manifest::decode(&mixed.encode()).is_err());
        // A huge calls count with no data: clean error, no allocation.
        let mut evil = Manifest { version: 1, services: vec![calling("a", &["a"])] }.encode();
        let n = evil.len();
        evil.truncate(n - 3); // drop count=1, len=1, "a"
        evil.extend_from_slice(&[0xff, 0xff, 0xff, 0x0f]);
        assert!(Manifest::decode(&evil).is_err());
    }

    #[test]
    fn text_calls() {
        let src = r#"
service echo { code = "e.ten" }
service chain {
  code = "c.ten"
  calls = ["kv-store", "echo", "echo",]   # sorted + deduplicated
}
service kv-store { code = "k.ten" calls = [] }
service gw { code = "g.ten" calls = ["*"] }
"#;
        let d = parse_text(src).unwrap();
        assert!(d[0].calls.is_empty());
        assert_eq!(d[1].calls, ["echo", "kv-store"]);
        assert!(d[2].calls.is_empty());
        assert_eq!(d[3].calls, ["*"]);
        let m = Manifest { version: 1, services: d.iter().map(|d| d.to_spec([0; 32])).collect() };
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
        // A service may list itself (self-calls follow the same rule).
        assert_eq!(parse_text("service a { code = \"a\" calls = [\"a\"] }").unwrap()[0].calls, ["a"]);
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
