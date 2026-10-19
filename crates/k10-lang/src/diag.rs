//! Source files, spans and diagnostics.

use std::fmt;
use std::path::PathBuf;

/// A byte range in one source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub(crate) struct Span {
    pub file: u32,
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn new(file: u32, lo: u32, hi: u32) -> Span {
        Span { file, lo, hi }
    }

    /// The smallest span covering `self` and `other` (same file).
    pub fn to(self, other: Span) -> Span {
        Span { file: self.file, lo: self.lo.min(other.lo), hi: self.hi.max(other.hi) }
    }
}

/// One loaded source file.
pub(crate) struct SourceFile {
    /// Display name (the path as given or joined, `std.ten` for the prelude, `<input>` for anonymous input).
    pub name: String,
    /// Directory used to resolve `include` paths.
    pub dir: Option<PathBuf>,
    pub text: String,
    pub prelude: bool,
    line_starts: Vec<u32>,
}

impl SourceFile {
    pub fn new(name: String, dir: Option<PathBuf>, text: String, prelude: bool) -> SourceFile {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile { name, dir, text, prelude, line_starts }
    }

    /// 1-based (line, column); the column counts characters, not bytes.
    pub fn line_col(&self, off: u32) -> (u32, u32) {
        let line = match self.line_starts.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.line_starts[line] as usize;
        let off = (off as usize).min(self.text.len());
        let col = self.text.get(start..off).map(|s| s.chars().count()).unwrap_or(off - start);
        (line as u32 + 1, col as u32 + 1)
    }
}

/// An error found while compiling. `line`/`col` are 1-based; `col` and `len` count characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// Length of the offending range on its line, in characters (at least 1).
    pub len: u32,
    pub message: String,
    /// An optional suggestion on how to fix the problem.
    pub help: Option<String>,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}: error: {}", self.file, self.line, self.col, self.message)?;
        if let Some(h) = &self.help {
            write!(f, " (help: {h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

/// Source texts by file name, used to print the offending line of a [`Diagnostic`].
///
/// [`crate::compile_with_sources`] fills it with every file it read (the embedded prelude is always available
/// under the name `std.ten`). Files missing from the map are read from disk when rendering.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    files: Vec<(String, String)>,
}

impl Sources {
    pub fn new() -> Sources {
        Sources::default()
    }

    /// Adds (or replaces) the text of file `name`.
    pub fn insert(&mut self, name: impl Into<String>, text: impl Into<String>) {
        let name = name.into();
        let text = text.into();
        if let Some(e) = self.files.iter_mut().find(|(n, _)| *n == name) {
            e.1 = text;
        } else {
            self.files.push((name, text));
        }
    }

    /// The text of file `name`, if known (`std.ten` always is).
    pub fn get(&self, name: &str) -> Option<&str> {
        if let Some((_, t)) = self.files.iter().find(|(n, _)| n == name) {
            return Some(t);
        }
        if name == crate::PRELUDE_NAME {
            return Some(crate::PRELUDE);
        }
        None
    }

    /// Names of the files known.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|(n, _)| n.as_str())
    }
}

impl Diagnostic {
    /// Renders the diagnostic like rustc: message, location, the offending line and a caret under the range.
    pub fn render(&self, sources: &Sources) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "error: {}", self.message);
        let owned;
        let text = match sources.get(&self.file) {
            Some(t) => Some(t),
            None => {
                owned = std::fs::read_to_string(&self.file).ok();
                owned.as_deref()
            }
        };
        let gutter = self.line.to_string().len();
        let pad = " ".repeat(gutter);
        let _ = writeln!(out, "{pad}--> {}:{}:{}", self.file, self.line, self.col);
        if let Some(line) = text.and_then(|t| t.lines().nth(self.line as usize - 1)) {
            let line = line.replace('\t', " ");
            let _ = writeln!(out, "{pad} |");
            let _ = writeln!(out, "{} | {}", self.line, line);
            let avail = line.chars().count().saturating_sub(self.col as usize - 1).max(1);
            let len = (self.len as usize).clamp(1, avail);
            let _ = writeln!(out, "{pad} | {}{}", " ".repeat(self.col as usize - 1), "^".repeat(len));
        }
        if let Some(h) = &self.help {
            let _ = writeln!(out, "{pad} = help: {h}");
        }
        out
    }
}

/// Renders several diagnostics, separated by blank lines.
pub fn render_diagnostics(diags: &[Diagnostic], sources: &Sources) -> String {
    let mut s = String::new();
    for d in diags {
        s.push_str(&d.render(sources));
        s.push('\n');
    }
    s
}

/// Collects diagnostics during compilation (spans are resolved to lines at the end).
#[derive(Default)]
pub(crate) struct Diags {
    pub list: Vec<(Span, String, Option<String>)>,
}

/// Compilation stops collecting after this many errors.
pub(crate) const MAX_ERRORS: usize = 100;

impl Diags {
    pub fn err(&mut self, span: Span, msg: impl Into<String>) {
        if self.list.len() < MAX_ERRORS {
            self.list.push((span, msg.into(), None));
        }
    }

    pub fn err_help(&mut self, span: Span, msg: impl Into<String>, help: impl Into<String>) {
        if self.list.len() < MAX_ERRORS {
            self.list.push((span, msg.into(), Some(help.into())));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn resolve(self, files: &[std::sync::Arc<SourceFile>]) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = Vec::with_capacity(self.list.len());
        for (span, message, help) in self.list {
            let (file, line, col, len) = match files.get(span.file as usize) {
                Some(f) => {
                    let (line, col) = f.line_col(span.lo);
                    let (l2, c2) = f.line_col(span.hi.max(span.lo));
                    let len = if l2 == line { (c2 - col).max(1) } else { 1 };
                    (f.name.clone(), line, col, len)
                }
                None => ("<unknown>".to_string(), 1, 1, 1),
            };
            let d = Diagnostic { file, line, col, len, message, help };
            if !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }
}
