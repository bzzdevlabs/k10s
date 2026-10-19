//! k10-lang — the compiler for **Ten**, the k10s workload language (docs/TEN.md), targeting TIR (docs/TIR.md).
//!
//! ```
//! let m = k10_lang::compile(r#"export fn handle() { out_str("hello"); }"#, None).unwrap();
//! assert_eq!(m.imports.len(), 1);                // only `output_write` survives dead-code elimination
//! assert_eq!(m.imports[0].name, "output_write");
//! k10_ir::validate(&m).unwrap();
//! ```
//!
//! Pipeline: lexer → parser (recursive descent, precedence climbing) → checker (order-independent items,
//! block-scoped locals, types) → HIR → reachability from the exports → static memory layout → folding
//! (constants, address offsets, strength reduction, forwarder inlining) → reachability again → TIR lowering.
//! Every emitted module is validated with `k10_ir::validate` (a failure is a compiler bug).
//!
//! The standard prelude [`PRELUDE`] (`std.ten`) is always compiled in; only what the program uses is emitted.

mod ast;
mod diag;
mod fold;
mod hir;
mod lexer;
mod lower;
mod parser;
mod sema;

#[cfg(any(test, feature = "testhost"))]
pub mod testhost;

#[cfg(test)]
mod tests;

pub use diag::{render_diagnostics, Diagnostic, Sources};

use diag::{Diags, SourceFile, Span};
use k10_ir::{Data, Export, Import, Module};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The standard prelude, compiled into every program.
pub const PRELUDE: &str = include_str!("std.ten");
/// File name under which the prelude appears in diagnostics.
pub const PRELUDE_NAME: &str = "std.ten";

/// Bytes of heap that `min_mem_log2` guarantees beyond the static area.
pub const MIN_HEAP: u64 = 4096;

/// Compiles Ten source text. `path` (if any) names the file in diagnostics and is the base for `include`
/// (without it, includes are resolved relative to the current directory).
pub fn compile(src: &str, path: Option<&Path>) -> Result<Module, Vec<Diagnostic>> {
    compile_with_sources(src, path, &mut Sources::new())
}

/// Reads and compiles a Ten file (and the files it includes).
pub fn compile_file(path: &Path) -> Result<Module, Vec<Diagnostic>> {
    compile_file_with_sources(path, &mut Sources::new())
}

/// Like [`compile_file`]; also records every source text read into `sources` (for [`Diagnostic::render`]).
pub fn compile_file_with_sources(path: &Path, sources: &mut Sources) -> Result<Module, Vec<Diagnostic>> {
    match std::fs::read_to_string(path) {
        Ok(src) => compile_with_sources(&src, Some(path), sources),
        Err(e) => Err(vec![Diagnostic {
            file: path.display().to_string(),
            line: 1,
            col: 1,
            len: 1,
            message: format!("cannot read `{}`: {e}", path.display()),
            help: None,
        }]),
    }
}

/// Like [`compile`]; also records every source text read into `sources` (for [`Diagnostic::render`]).
pub fn compile_with_sources(src: &str, path: Option<&Path>, sources: &mut Sources) -> Result<Module, Vec<Diagnostic>> {
    Driver::new(false).run(src, path, sources)
}

/// Type-checks the whole prelude (including functions no program uses). For tests.
#[doc(hidden)]
pub fn check_prelude() -> Result<(), Vec<Diagnostic>> {
    Driver::new(true).run("", None, &mut Sources::new()).map(|_| ())
}

/// The prelude, lexed and parsed once per process and shared by every compilation: its AST is borrowed by the
/// checker, its interner is the frozen base of each compilation's interner, its string table is copied.
struct PreludeCache {
    names: lexer::Interner,
    strings: Vec<Vec<u8>>,
    parsed: ast::ParsedFile,
    file: Arc<SourceFile>,
    diags: Vec<(Span, String, Option<String>)>,
}

fn prelude_cache() -> &'static PreludeCache {
    static CACHE: OnceLock<PreludeCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut names = lexer::Interner::default();
        sema::preintern(&mut names);
        let mut strings = Vec::new();
        let mut diags = Diags::default();
        let toks = lexer::lex(PRELUDE, 0, &mut names, &mut strings, &mut diags);
        let parsed = parser::parse_file(&toks, &names, &mut diags);
        let file = Arc::new(SourceFile::new(PRELUDE_NAME.to_string(), None, PRELUDE.to_string(), true));
        PreludeCache { names, strings, parsed, file, diags: diags.list }
    })
}

struct Driver {
    /// File 0 is the prelude.
    files: Vec<Arc<SourceFile>>,
    /// Parsed user files (`parsed[i]` is `files[i + 1]`).
    parsed: Vec<ast::ParsedFile>,
    prelude: &'static ast::ParsedFile,
    names: lexer::Interner,
    strings: Vec<Vec<u8>>,
    diags: Diags,
    loaded: HashMap<PathBuf, u32>,
    check_all: bool,
}

fn align_up(x: u64, a: u64) -> u64 {
    x.div_ceil(a.max(1)) * a.max(1)
}

impl Driver {
    fn new(check_all: bool) -> Driver {
        let cache = prelude_cache();
        Driver {
            files: vec![cache.file.clone()],
            parsed: Vec::new(),
            prelude: &cache.parsed,
            names: lexer::Interner::with_base(&cache.names),
            strings: cache.strings.clone(),
            diags: Diags { list: cache.diags.clone() },
            loaded: HashMap::new(),
            check_all,
        }
    }

    fn run(mut self, src: &str, path: Option<&Path>, sources: &mut Sources) -> Result<Module, Vec<Diagnostic>> {
        let (name, dir) = match path {
            Some(p) => {
                if let Ok(c) = std::fs::canonicalize(p) {
                    self.loaded.insert(c, 1);
                }
                (p.display().to_string(), Some(p.parent().map(Path::to_path_buf).unwrap_or_default()))
            }
            None => ("<input>".to_string(), None),
        };
        self.load(name, dir, src.to_string());
        for f in &self.files[1..] {
            sources.insert(f.name.clone(), f.text.clone());
        }
        let result = self.compile();
        let Driver { diags, files, .. } = self;
        match result {
            Some(m) if diags.is_empty() => Ok(m),
            _ => {
                let d = diags.resolve(&files);
                debug_assert!(!d.is_empty(), "compilation failed without a diagnostic");
                Err(d)
            }
        }
    }

    fn load(&mut self, name: String, dir: Option<PathBuf>, text: String) {
        let id = self.files.len() as u32;
        let toks = lexer::lex(&text, id, &mut self.names, &mut self.strings, &mut self.diags);
        let parsed = parser::parse_file(&toks, &self.names, &mut self.diags);
        let includes: Vec<(u32, Span)> = parsed
            .items
            .iter()
            .filter_map(|it| match it.kind {
                ast::ItemKind::Include(p) => Some((p, it.span)),
                _ => None,
            })
            .collect();
        self.files.push(Arc::new(SourceFile::new(name, dir, text, false)));
        self.parsed.push(parsed);
        for (p, span) in includes {
            self.include(id, p, span);
        }
    }

    fn include(&mut self, from: u32, path_idx: u32, span: Span) {
        let rel = String::from_utf8_lossy(&self.strings[path_idx as usize]).into_owned();
        if self.files[from as usize].prelude {
            self.diags.err(span, "the prelude cannot include files");
            return;
        }
        let base = self.files[from as usize].dir.clone();
        let full = match &base {
            Some(b) if !b.as_os_str().is_empty() => b.join(&rel),
            _ => PathBuf::from(&rel),
        };
        let canon = std::fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
        if self.loaded.contains_key(&canon) {
            return; // each file is included once
        }
        match std::fs::read_to_string(&full) {
            Ok(text) => {
                self.loaded.insert(canon, self.files.len() as u32);
                let dir = full.parent().map(Path::to_path_buf);
                self.load(full.display().to_string(), dir, text);
            }
            Err(e) => {
                self.diags.err_help(
                    span,
                    format!("cannot include `{}`: {e}", full.display()),
                    "include paths are relative to the including file",
                );
            }
        }
    }

    fn compile(&mut self) -> Option<Module> {
        let files = std::iter::once((self.prelude, true)).chain(self.parsed.iter().map(|p| (p, false)));
        let input = sema::Input { files: files.collect() };
        if !self.diags.is_empty() && self.diags.len() >= diag::MAX_ERRORS {
            return None;
        }
        let parse_errors = !self.diags.is_empty();
        let t = std::time::Instant::now();
        let prog = sema::check(&input, &self.files, &mut self.names, &self.strings, &mut self.diags, self.check_all)?;
        timing("check", t);
        if parse_errors {
            return None;
        }
        self.backend(prog)
    }

    fn backend(&mut self, mut prog: sema::Program) -> Option<Module> {
        let nf = prog.funcs.len();
        let ns = prog.statics.len();
        let (live, live_s) = reach(&prog.funcs, ns);

        // Static memory layout: [0,16) reserved · globals · data · string literals · buffers · heap.
        let mut addrs = vec![0i64; ns];
        let mut off: u64 = 16;
        for kind in [sema::StaticKind::Global, sema::StaticKind::Data, sema::StaticKind::Str, sema::StaticKind::Buffer] {
            for (i, s) in prog.statics.iter().enumerate() {
                if live_s[i] && s.kind == kind {
                    off = align_up(off, s.align);
                    addrs[i] = off as i64;
                    off += s.size;
                }
            }
        }
        let heap_base = align_up(off, 16);
        let need = heap_base + MIN_HEAP;
        let mut k = k10_ir::MIN_MEM_LOG2 as u32;
        while (1u64 << k) < need && k < 64 {
            k += 1;
        }
        if k > k10_ir::MAX_MEM_LOG2 as u32 {
            let span = prog.funcs.iter().find(|f| f.export).map(|f| f.span).unwrap_or_default();
            self.diags.err(span, format!("the static data needs {heap_base} bytes, more than the 64 GiB maximum memory"));
            return None;
        }
        let data = data_segments(&prog.statics, &live_s, &addrs);

        // Folding and forwarder inlining.
        let t = std::time::Instant::now();
        let fwd: Vec<Option<fold::Forwarder>> = prog
            .funcs
            .iter()
            .enumerate()
            .map(|(i, f)| if live[i] { f.body.as_ref().and_then(|b| fold::forwarder(b, i as u32)) } else { None })
            .collect();
        let folder = fold::Folder { addrs: &addrs, heap_base: heap_base as i64, fwd: &fwd };
        for (i, f) in prog.funcs.iter_mut().enumerate() {
            if live[i] {
                if let Some(b) = &mut f.body {
                    folder.body(b);
                }
            }
        }
        timing("fold", t);
        let (live, _) = reach(&prog.funcs, ns);

        // Function index space: used imports (deduplicated by name), then used defined functions.
        let mut func_index = vec![u32::MAX; nf];
        let mut imports: Vec<Import> = Vec::new();
        let mut by_name: HashMap<String, u32> = HashMap::new();
        for (i, f) in prog.funcs.iter().enumerate() {
            if live[i] && f.import {
                if let Some(&j) = by_name.get(&f.name) {
                    let im = &imports[j as usize];
                    if im.params as u32 != f.params || im.results != f.results {
                        self.diags.err(f.span, format!("host function `{}` is imported twice with different signatures", f.name));
                    }
                    func_index[i] = j;
                } else {
                    let j = imports.len() as u32;
                    imports.push(Import { name: f.name.clone(), params: f.params as u8, results: f.results });
                    by_name.insert(f.name.clone(), j);
                    func_index[i] = j;
                }
            }
        }
        let mut next = imports.len() as u32;
        let mut order = Vec::new();
        for (i, f) in prog.funcs.iter().enumerate() {
            if live[i] && !f.import {
                func_index[i] = next;
                next += 1;
                order.push(i);
            }
        }
        let results: Vec<u8> = prog.funcs.iter().map(|f| f.results).collect();
        let ctx = lower::LowerCtx { func_index: &func_index, results: &results };
        let t = std::time::Instant::now();
        let funcs: Vec<k10_ir::Func> =
            order.iter().map(|&i| lower::lower(prog.funcs[i].body.as_ref().expect("body"), &ctx)).collect();
        let exports: Vec<Export> = prog
            .funcs
            .iter()
            .enumerate()
            .filter(|(i, f)| f.export && live[*i])
            .map(|(i, f)| Export { name: f.name.clone(), func: func_index[i] })
            .collect();
        timing("lower", t);
        let m = Module { min_mem_log2: k as u8, imports, funcs, exports, data };
        if let Err(e) = k10_ir::validate(&m) {
            let fname = e.func.and_then(|f| order.get(f as usize)).map(|&i| prog.funcs[i].name.clone());
            let span = e.func.and_then(|f| order.get(f as usize)).map(|&i| prog.funcs[i].span).unwrap_or_default();
            if let k10_ir::ValidateErrorKind::StackTooDeep
            | k10_ir::ValidateErrorKind::NestingTooDeep
            | k10_ir::ValidateErrorKind::TooManyLocals(_)
            | k10_ir::ValidateErrorKind::CodeTooLarge(_) = e.kind
            {
                self.diags.err(
                    span,
                    format!("function `{}` exceeds a TIR limit: {}", fname.unwrap_or_default(), e.kind),
                );
                return None;
            }
            debug_assert!(false, "k10-lang emitted an invalid module ({e}) in function {fname:?}");
            self.diags.err(span, format!("internal compiler error: invalid TIR emitted ({e})"));
            return None;
        }
        if !self.diags.is_empty() {
            return None;
        }
        Some(m)
    }
}

/// Prints phase timings to stderr when `K10_LANG_TIMING` is set.
fn timing(what: &str, t: std::time::Instant) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("K10_LANG_TIMING").is_some()) {
        eprintln!("k10-lang: {what} {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    }
}

/// Functions and statics reachable from the exports.
fn reach(funcs: &[sema::FuncInfo], nstatics: usize) -> (Vec<bool>, Vec<bool>) {
    let mut lf = vec![false; funcs.len()];
    let mut ls = vec![false; nstatics];
    let mut work: Vec<usize> = Vec::new();
    for (i, f) in funcs.iter().enumerate() {
        if f.export && !f.prelude {
            lf[i] = true;
            work.push(i);
        }
    }
    while let Some(f) = work.pop() {
        if let Some(b) = &funcs[f].body {
            for s in &b.stmts {
                s.visit_exprs(&mut |e| match e.kind {
                    hir::EK::Call(g, _) => {
                        if !lf[g as usize] {
                            lf[g as usize] = true;
                            work.push(g as usize);
                        }
                    }
                    hir::EK::Static(id) => ls[id as usize] = true,
                    _ => {}
                });
            }
        }
    }
    (lf, ls)
}

/// Data segments for the initialized live statics; runs separated by fewer than 16 zero bytes are merged.
fn data_segments(statics: &[sema::StaticInfo], live: &[bool], addrs: &[i64]) -> Vec<Data> {
    let mut parts: Vec<(u64, &[u8])> = statics
        .iter()
        .enumerate()
        .filter(|(i, s)| live[*i] && !s.init.is_empty())
        .map(|(i, s)| (addrs[i] as u64, s.init.as_slice()))
        .collect();
    parts.sort_by_key(|p| p.0);
    let mut out: Vec<Data> = Vec::new();
    for (addr, bytes) in parts {
        // Trim zeros at both ends.
        let Some(first) = bytes.iter().position(|&b| b != 0) else { continue };
        let last = bytes.iter().rposition(|&b| b != 0).unwrap();
        let (addr, bytes) = (addr + first as u64, &bytes[first..=last]);
        if let Some(prev) = out.last_mut() {
            let end = prev.offset + prev.bytes.len() as u64;
            if addr >= end && addr - end < 16 {
                prev.bytes.resize((addr - prev.offset) as usize, 0);
                prev.bytes.extend_from_slice(bytes);
                continue;
            }
        }
        out.push(Data { offset: addr, bytes: bytes.to_vec() });
    }
    out
}
