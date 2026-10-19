//! Lexer: source text → tokens.

use crate::diag::{Diags, Span};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// FxHash (as in rustc): very fast on short keys. Not DoS-resistant, which is fine for compiler-internal maps.
#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher(u64);

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, r) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !r.is_empty() {
            let mut b = [0u8; 8];
            b[..r.len()].copy_from_slice(r);
            self.add(u64::from_le_bytes(b));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// An interned identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Sym(pub u32);

/// Keywords, interned first so that `Sym(i)` for `i < KEYWORDS.len()` is keyword `i`.
const KEYWORDS: [(&str, Tok); 26] = [
    ("fn", Tok::Fn),
    ("export", Tok::Export),
    ("import", Tok::Import),
    ("const", Tok::Const),
    ("global", Tok::Global),
    ("data", Tok::Data),
    ("buffer", Tok::Buffer),
    ("struct", Tok::Struct),
    ("include", Tok::Include),
    ("let", Tok::Let),
    ("if", Tok::If),
    ("else", Tok::Else),
    ("while", Tok::While),
    ("loop", Tok::Loop),
    ("for", Tok::For),
    ("in", Tok::In),
    ("break", Tok::Break),
    ("continue", Tok::Continue),
    ("return", Tok::Return),
    ("as", Tok::As),
    ("null", Tok::Null),
    ("true", Tok::True),
    ("false", Tok::False),
    ("sizeof", Tok::Sizeof),
    ("offsetof", Tok::Offsetof),
    ("countof", Tok::Countof),
];

/// Identifier interner of one compilation. It can extend a frozen `base` interner (the cached prelude's), so that
/// symbols of the shared prelude AST stay valid without copying its table.
pub(crate) struct Interner {
    base: Option<&'static Interner>,
    base_len: u32,
    map: FxMap<Box<str>, Sym>,
    names: Vec<Box<str>>,
}

impl Default for Interner {
    fn default() -> Self {
        let mut i = Interner { base: None, base_len: 0, map: FxMap::default(), names: Vec::new() };
        for (kw, _) in KEYWORDS {
            i.intern(kw);
        }
        i
    }
}

impl Interner {
    /// An empty interner on top of `base` (which must not itself have a base).
    pub fn with_base(base: &'static Interner) -> Interner {
        debug_assert!(base.base.is_none());
        Interner { base: Some(base), base_len: base.names.len() as u32, map: FxMap::default(), names: Vec::new() }
    }

    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(b) = self.base {
            if let Some(&x) = b.map.get(s) {
                return x;
            }
        }
        if let Some(&x) = self.map.get(s) {
            return x;
        }
        let sym = Sym(self.base_len + self.names.len() as u32);
        self.names.push(s.into());
        self.map.insert(s.into(), sym);
        sym
    }

    pub fn get(&self, s: &str) -> Option<Sym> {
        if let Some(b) = self.base {
            if let Some(&x) = b.map.get(s) {
                return Some(x);
            }
        }
        self.map.get(s).copied()
    }

    pub fn name(&self, s: Sym) -> &str {
        if s.0 < self.base_len {
            self.base.unwrap().name(s)
        } else {
            &self.names[(s.0 - self.base_len) as usize]
        }
    }

    fn keyword(s: Sym) -> Option<Tok> {
        KEYWORDS.get(s.0 as usize).map(|k| k.1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Tok {
    Ident(Sym),
    Int(u64),
    Float(f64),
    /// Index into the compilation's string table.
    Str(u32),
    Char(u8),
    // keywords
    Fn,
    Export,
    Import,
    Const,
    Global,
    Data,
    Buffer,
    Struct,
    Include,
    Let,
    If,
    Else,
    While,
    Loop,
    For,
    In,
    Break,
    Continue,
    Return,
    As,
    Null,
    True,
    False,
    Sizeof,
    Offsetof,
    Countof,
    // punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Colon,
    Dot,
    DotDot,
    Arrow,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Bang,
    Assign,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Shl,
    Shr,
    Ushr,
    AndAnd,
    OrOr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
    UshrEq,
    Eof,
}

impl Tok {
    /// Human-readable description for error messages.
    pub fn describe(&self, names: &Interner) -> String {
        let s = match self {
            Tok::Ident(s) => return format!("identifier `{}`", names.name(*s)),
            Tok::Int(_) => "integer literal",
            Tok::Float(_) => "float literal",
            Tok::Str(_) => "string literal",
            Tok::Char(_) => "character literal",
            Tok::Eof => "end of file",
            t => return format!("`{}`", t.text()),
        };
        s.to_string()
    }

    pub fn text(&self) -> &'static str {
        match self {
            Tok::Fn => "fn",
            Tok::Export => "export",
            Tok::Import => "import",
            Tok::Const => "const",
            Tok::Global => "global",
            Tok::Data => "data",
            Tok::Buffer => "buffer",
            Tok::Struct => "struct",
            Tok::Include => "include",
            Tok::Let => "let",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::While => "while",
            Tok::Loop => "loop",
            Tok::For => "for",
            Tok::In => "in",
            Tok::Break => "break",
            Tok::Continue => "continue",
            Tok::Return => "return",
            Tok::As => "as",
            Tok::Null => "null",
            Tok::True => "true",
            Tok::False => "false",
            Tok::Sizeof => "sizeof",
            Tok::Offsetof => "offsetof",
            Tok::Countof => "countof",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Semi => ";",
            Tok::Colon => ":",
            Tok::Dot => ".",
            Tok::DotDot => "..",
            Tok::Arrow => "->",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Amp => "&",
            Tok::Pipe => "|",
            Tok::Caret => "^",
            Tok::Tilde => "~",
            Tok::Bang => "!",
            Tok::Assign => "=",
            Tok::EqEq => "==",
            Tok::Ne => "!=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::Shl => "<<",
            Tok::Shr => ">>",
            Tok::Ushr => ">>>",
            Tok::AndAnd => "&&",
            Tok::OrOr => "||",
            Tok::PlusEq => "+=",
            Tok::MinusEq => "-=",
            Tok::StarEq => "*=",
            Tok::SlashEq => "/=",
            Tok::PercentEq => "%=",
            Tok::AmpEq => "&=",
            Tok::PipeEq => "|=",
            Tok::CaretEq => "^=",
            Tok::ShlEq => "<<=",
            Tok::ShrEq => ">>=",
            Tok::UshrEq => ">>>=",
            Tok::Ident(_) | Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::Char(_) | Tok::Eof => "?",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Token {
    pub tok: Tok,
    pub span: Span,
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    file: u32,
    names: &'a mut Interner,
    strings: &'a mut Vec<Vec<u8>>,
    diags: &'a mut Diags,
}

/// Tokenizes `text` (file index `file`). Always ends with an `Eof` token; errors go to `diags`.
pub(crate) fn lex(
    text: &str,
    file: u32,
    names: &mut Interner,
    strings: &mut Vec<Vec<u8>>,
    diags: &mut Diags,
) -> Vec<Token> {
    let mut lx = Lexer { src: text.as_bytes(), text, pos: 0, file, names, strings, diags };
    let mut out = Vec::with_capacity(text.len() / 4 + 1);
    loop {
        let t = lx.next();
        let eof = t.tok == Tok::Eof;
        out.push(t);
        if eof {
            break;
        }
    }
    out
}

impl<'a> Lexer<'a> {
    fn peek(&self, k: usize) -> u8 {
        *self.src.get(self.pos + k).unwrap_or(&0)
    }

    fn span(&self, lo: usize) -> Span {
        Span::new(self.file, lo as u32, self.pos as u32)
    }

    fn err(&mut self, lo: usize, hi: usize, msg: impl Into<String>) {
        self.diags.err(Span::new(self.file, lo as u32, hi as u32), msg);
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek(0) {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'/' if self.peek(1) == b'/' => {
                    while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                b'/' if self.peek(1) == b'*' => {
                    let lo = self.pos;
                    self.pos += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        if self.pos >= self.src.len() {
                            self.err(lo, lo + 2, "unterminated block comment");
                            return;
                        }
                        if self.peek(0) == b'*' && self.peek(1) == b'/' {
                            depth -= 1;
                            self.pos += 2;
                        } else if self.peek(0) == b'/' && self.peek(1) == b'*' {
                            depth += 1;
                            self.pos += 2;
                        } else {
                            self.pos += 1;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn next(&mut self) -> Token {
        self.skip_trivia();
        let lo = self.pos;
        if self.pos >= self.src.len() {
            return Token { tok: Tok::Eof, span: self.span(lo) };
        }
        let c = self.src[self.pos];
        let tok = match c {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while matches!(self.peek(0), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
                    self.pos += 1;
                }
                let sym = self.names.intern(&self.text[lo..self.pos]);
                Interner::keyword(sym).unwrap_or(Tok::Ident(sym))
            }
            b'0'..=b'9' => self.number(lo),
            b'"' => self.string(lo),
            b'\'' => self.char_lit(lo),
            _ => {
                let (tok, n) = self.punct();
                if n == 0 {
                    // Skip one (possibly multi-byte) character.
                    let ch = self.text[lo..].chars().next().unwrap_or('?');
                    self.pos += ch.len_utf8().max(1);
                    self.err(lo, self.pos, format!("unexpected character `{}`", ch.escape_default()));
                    return self.next();
                }
                self.pos += n;
                tok
            }
        };
        Token { tok, span: self.span(lo) }
    }

    fn punct(&self) -> (Tok, usize) {
        let (a, b, c, d) = (self.peek(0), self.peek(1), self.peek(2), self.peek(3));
        match (a, b, c, d) {
            (b'>', b'>', b'>', b'=') => (Tok::UshrEq, 4),
            (b'>', b'>', b'>', _) => (Tok::Ushr, 3),
            (b'>', b'>', b'=', _) => (Tok::ShrEq, 3),
            (b'<', b'<', b'=', _) => (Tok::ShlEq, 3),
            (b'>', b'>', _, _) => (Tok::Shr, 2),
            (b'<', b'<', _, _) => (Tok::Shl, 2),
            (b'>', b'=', _, _) => (Tok::Ge, 2),
            (b'<', b'=', _, _) => (Tok::Le, 2),
            (b'=', b'=', _, _) => (Tok::EqEq, 2),
            (b'!', b'=', _, _) => (Tok::Ne, 2),
            (b'&', b'&', _, _) => (Tok::AndAnd, 2),
            (b'|', b'|', _, _) => (Tok::OrOr, 2),
            (b'+', b'=', _, _) => (Tok::PlusEq, 2),
            (b'-', b'=', _, _) => (Tok::MinusEq, 2),
            (b'*', b'=', _, _) => (Tok::StarEq, 2),
            (b'/', b'=', _, _) => (Tok::SlashEq, 2),
            (b'%', b'=', _, _) => (Tok::PercentEq, 2),
            (b'&', b'=', _, _) => (Tok::AmpEq, 2),
            (b'|', b'=', _, _) => (Tok::PipeEq, 2),
            (b'^', b'=', _, _) => (Tok::CaretEq, 2),
            (b'-', b'>', _, _) => (Tok::Arrow, 2),
            (b'.', b'.', _, _) => (Tok::DotDot, 2),
            (b'(', ..) => (Tok::LParen, 1),
            (b')', ..) => (Tok::RParen, 1),
            (b'{', ..) => (Tok::LBrace, 1),
            (b'}', ..) => (Tok::RBrace, 1),
            (b'[', ..) => (Tok::LBracket, 1),
            (b']', ..) => (Tok::RBracket, 1),
            (b',', ..) => (Tok::Comma, 1),
            (b';', ..) => (Tok::Semi, 1),
            (b':', ..) => (Tok::Colon, 1),
            (b'.', ..) => (Tok::Dot, 1),
            (b'+', ..) => (Tok::Plus, 1),
            (b'-', ..) => (Tok::Minus, 1),
            (b'*', ..) => (Tok::Star, 1),
            (b'/', ..) => (Tok::Slash, 1),
            (b'%', ..) => (Tok::Percent, 1),
            (b'&', ..) => (Tok::Amp, 1),
            (b'|', ..) => (Tok::Pipe, 1),
            (b'^', ..) => (Tok::Caret, 1),
            (b'~', ..) => (Tok::Tilde, 1),
            (b'!', ..) => (Tok::Bang, 1),
            (b'=', ..) => (Tok::Assign, 1),
            (b'<', ..) => (Tok::Lt, 1),
            (b'>', ..) => (Tok::Gt, 1),
            _ => (Tok::Eof, 0),
        }
    }

    fn number(&mut self, lo: usize) -> Tok {
        let radix = match (self.peek(0), self.peek(1)) {
            (b'0', b'x' | b'X') => 16,
            (b'0', b'b' | b'B') => 2,
            (b'0', b'o' | b'O') => 8,
            _ => 10,
        };
        if radix != 10 {
            self.pos += 2;
            let mut v: u64 = 0;
            let mut digits = 0;
            let mut overflow = false;
            loop {
                let c = self.peek(0);
                if c == b'_' {
                    self.pos += 1;
                    continue;
                }
                let d = match c {
                    b'0'..=b'9' => (c - b'0') as u32,
                    b'a'..=b'f' => (c - b'a' + 10) as u32,
                    b'A'..=b'F' => (c - b'A' + 10) as u32,
                    _ => break,
                };
                if d >= radix {
                    self.pos += 1;
                    self.err(lo, self.pos, format!("invalid digit `{}` in base-{radix} literal", c as char));
                    continue;
                }
                self.pos += 1;
                digits += 1;
                match v.checked_mul(radix as u64).and_then(|v| v.checked_add(d as u64)) {
                    Some(n) => v = n,
                    None => overflow = true,
                }
            }
            if digits == 0 {
                self.err(lo, self.pos, "missing digits after the base prefix");
            }
            if overflow {
                self.err(lo, self.pos, "integer literal does not fit in 64 bits");
            }
            self.check_suffix(lo);
            return Tok::Int(v);
        }
        let mut is_float = false;
        let mut v: u64 = 0;
        let mut overflow = false;
        while let c @ (b'0'..=b'9' | b'_') = self.peek(0) {
            if c != b'_' {
                match v.checked_mul(10).and_then(|v| v.checked_add((c - b'0') as u64)) {
                    Some(n) => v = n,
                    None => overflow = true,
                }
            }
            self.pos += 1;
        }
        if self.peek(0) == b'.' && self.peek(1).is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek(0), b'e' | b'E')
            && (self.peek(1).is_ascii_digit() || (matches!(self.peek(1), b'+' | b'-') && self.peek(2).is_ascii_digit()))
        {
            is_float = true;
            self.pos += 2;
            self.digits();
        }
        let end = self.pos;
        self.check_suffix(lo);
        if is_float {
            let text: String = self.text[lo..end].chars().filter(|&c| c != '_').collect();
            match text.parse::<f64>() {
                Ok(f) => Tok::Float(f),
                Err(_) => {
                    self.err(lo, end, "invalid float literal");
                    Tok::Float(0.0)
                }
            }
        } else {
            if overflow {
                self.err(lo, end, "integer literal does not fit in 64 bits");
            }
            Tok::Int(v)
        }
    }

    fn digits(&mut self) {
        while self.peek(0).is_ascii_digit() || self.peek(0) == b'_' {
            self.pos += 1;
        }
    }

    fn check_suffix(&mut self, lo: usize) {
        if matches!(self.peek(0), b'a'..=b'z' | b'A'..=b'Z' | b'_') {
            let s = self.pos;
            while matches!(self.peek(0), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
                self.pos += 1;
            }
            let suffix = self.text[s..self.pos].to_string();
            self.err(lo, self.pos, format!("invalid suffix `{suffix}` on number literal"));
        }
    }

    /// Parses one escape sequence after the backslash; returns the byte.
    fn escape(&mut self, quote_lo: usize) -> Option<u8> {
        let lo = self.pos - 1;
        if self.pos >= self.src.len() {
            self.err(quote_lo, self.pos, "unterminated literal");
            return None;
        }
        let c = self.peek(0);
        self.pos += 1;
        Some(match c {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'0' => 0,
            b'\\' => b'\\',
            b'"' => b'"',
            b'\'' => b'\'',
            b'x' => {
                let h = |c: u8| (c as char).to_digit(16);
                match (h(self.peek(0)), h(self.peek(1))) {
                    (Some(a), Some(b)) => {
                        self.pos += 2;
                        (a * 16 + b) as u8
                    }
                    _ => {
                        self.err(lo, self.pos, "`\\x` must be followed by two hex digits");
                        return None;
                    }
                }
            }
            _ => {
                let ch = self.text[self.pos - 1..].chars().next().unwrap_or('?');
                self.pos += ch.len_utf8() - 1;
                self.err(lo, self.pos, format!("unknown escape `\\{ch}`"));
                return None;
            }
        })
    }

    fn string(&mut self, lo: usize) -> Tok {
        self.pos += 1;
        let mut bytes = Vec::new();
        loop {
            if self.pos >= self.src.len() {
                self.err(lo, lo + 1, "unterminated string literal");
                break;
            }
            let c = self.src[self.pos];
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    if let Some(b) = self.escape(lo) {
                        bytes.push(b);
                    }
                }
                _ => bytes.push(c),
            }
        }
        self.strings.push(bytes);
        Tok::Str(self.strings.len() as u32 - 1)
    }

    fn char_lit(&mut self, lo: usize) -> Tok {
        self.pos += 1;
        let c = self.peek(0);
        let v = match c {
            b'\\' => {
                self.pos += 1;
                self.escape(lo).unwrap_or(0)
            }
            b'\'' | b'\n' | 0 => {
                self.err(lo, self.pos, "empty character literal");
                0
            }
            _ if c >= 0x80 => {
                let ch = self.text[self.pos..].chars().next().unwrap_or('?');
                self.pos += ch.len_utf8();
                self.err(lo, self.pos, "character literals must be a single byte (ASCII)");
                if self.peek(0) == b'\'' {
                    self.pos += 1;
                }
                return Tok::Char(0);
            }
            _ => {
                self.pos += 1;
                c
            }
        };
        if self.peek(0) == b'\'' {
            self.pos += 1;
        } else {
            self.err(lo, self.pos, "unterminated character literal (expected `'`)");
        }
        Tok::Char(v)
    }
}
