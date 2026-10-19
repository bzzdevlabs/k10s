//! Name resolution and type checking: AST → HIR.
//!
//! Items are order-independent. Two item scopes exist: the prelude's and the user's (all user files and their
//! includes). User code sees its own items first, then the prelude's; prelude code only sees the prelude, so user
//! definitions can shadow prelude names without breaking it. Locals are block-scoped and may shadow anything.

use crate::ast::{self, BinOp, DataInit, ExprKind, ItemKind, SizeArg, StmtKind, TypeKind, UnOp};
use crate::diag::{Diags, SourceFile, Span};
use crate::hir::{self, Bin, Expr, MemTy, Un, EK};
use crate::lexer::{FxMap, Interner, Sym};
use std::rc::Rc;

// -------------------------------------------------------------------------------------------------------------
// Types

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ty {
    I64,
    F64,
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    Ptr(Rc<Ty>),
    Array(Rc<Ty>, u64),
    Struct(u32),
    /// A (pointer, length) pair; occupies two values. Parameters and locals only.
    Str,
    Void,
    /// The type of `null`: converts to any pointer type.
    Null,
    /// Poisoned (an error was already reported).
    Error,
}

impl Ty {
    fn ptr(t: Ty) -> Ty {
        Ty::Ptr(Rc::new(t))
    }

    fn is_value(&self) -> bool {
        matches!(self, Ty::I64 | Ty::F64 | Ty::Ptr(_) | Ty::Str | Ty::Error)
    }

    fn is_scalar_storage(&self) -> bool {
        matches!(self, Ty::I64 | Ty::F64 | Ty::U8 | Ty::I8 | Ty::U16 | Ty::I16 | Ty::U32 | Ty::I32 | Ty::Ptr(_))
    }

    fn memty(&self) -> MemTy {
        match self {
            Ty::U8 => MemTy::U8,
            Ty::I8 => MemTy::I8,
            Ty::U16 => MemTy::U16,
            Ty::I16 => MemTy::I16,
            Ty::U32 => MemTy::U32,
            Ty::I32 => MemTy::I32,
            _ => MemTy::I64,
        }
    }

    /// The type of a value loaded from storage of this type.
    fn value_ty(&self) -> Ty {
        match self {
            Ty::U8 | Ty::I8 | Ty::U16 | Ty::I16 | Ty::U32 | Ty::I32 | Ty::I64 => Ty::I64,
            t => t.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CV {
    I(i64),
    F(f64),
}

// -------------------------------------------------------------------------------------------------------------
// Program (checker output)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticKind {
    Global,
    Data,
    Buffer,
    Str,
}

#[derive(Clone, Debug)]
pub(crate) struct StaticInfo {
    pub kind: StaticKind,
    pub size: u64,
    pub align: u64,
    /// Initial bytes (shorter than `size` or empty means zero-filled).
    pub init: Vec<u8>,
}

pub(crate) struct FuncInfo {
    pub name: String,
    pub import: bool,
    pub prelude: bool,
    pub export: bool,
    pub params: u32,
    pub results: u8,
    pub body: Option<hir::Body>,
    pub span: Span,
}

pub(crate) struct Program {
    pub funcs: Vec<FuncInfo>,
    pub statics: Vec<StaticInfo>,
}

// -------------------------------------------------------------------------------------------------------------
// Checker state

#[derive(Clone, Copy, Debug)]
enum ItemRef {
    Func(u32),
    Const(u32),
    Global(u32),
    Data(u32),
    Buffer(u32),
    Struct(u32),
}

struct ItemEntry<'a> {
    kind: ItemRef,
    span: Span,
    item: &'a ast::Item,
    prelude: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Todo,
    Busy,
    Done,
}

struct StructInfo<'a> {
    name: Sym,
    decl: &'a [ast::FieldDecl],
    prelude: bool,
    state: State,
    fields: Vec<(Sym, Ty, u64)>,
    size: u64,
    align: u64,
}

struct ConstInfo<'a> {
    decl: &'a ast::Item,
    prelude: bool,
    state: State,
    value: Option<CV>,
}

struct FuncSig<'a> {
    name: Sym,
    decl: &'a ast::FnDecl,
    span: Span,
    prelude: bool,
    params: Rc<[(Sym, Ty, Span)]>,
    ret: Option<Ty>,
    tir_params: u32,
    body: Option<hir::Body>,
    checked: bool,
}

struct StaticDecl {
    /// Element type for data/buffers, value type for globals.
    ty: Ty,
    count: u64,
    info: StaticInfo,
}

/// Pre-interned names.
struct Names {
    i64_: Sym,
    f64_: Sym,
    u8_: Sym,
    i8_: Sym,
    u16_: Sym,
    i16_: Sym,
    u32_: Sym,
    i32_: Sym,
    str_: Sym,
    ptr: Sym,
    len: Sym,
    heap_base: Sym,
    handle: Sym,
    init: Sym,
}

const BUILTINS: &[&str] = &[
    "memcpy", "memset", "memsize", "sqrt", "fabs", "clz", "ctz", "popcnt", "rotl", "rotr", "ult", "ule", "ugt", "uge",
    "udiv", "urem", "unreachable", "select", "str", "to_bits", "from_bits",
];

#[derive(Clone)]
struct Typed {
    e: Expr,
    ty: Ty,
    /// Second component of a `str` value.
    len: Option<Expr>,
    /// An integer literal (for better error hints).
    lit: bool,
}

impl Typed {
    fn new(e: Expr, ty: Ty) -> Typed {
        Typed { e, ty, len: None, lit: false }
    }

    fn err() -> Typed {
        Typed::new(Expr::c(0), Ty::Error)
    }
}

struct Place {
    addr: Expr,
    ty: Ty,
}

enum PV {
    Place(Place),
    Value(Typed),
}

#[derive(Clone)]
struct LocalVar {
    name: Sym,
    id: hir::LocalId,
    ty: Ty,
    mutable: bool,
}

#[derive(Default)]
struct FnState {
    num_locals: u32,
    scopes: Vec<Vec<LocalVar>>,
    loops: Vec<(bool, bool)>,
    ret: Option<Ty>,
    prelude: bool,
}

pub(crate) struct Checker<'a> {
    names: &'a Interner,
    files: &'a [std::sync::Arc<SourceFile>],
    strings: &'a [Vec<u8>],
    diags: &'a mut Diags,
    n: Names,
    builtins: FxMap<Sym, &'static str>,
    items: Vec<ItemEntry<'a>>,
    scopes: [FxMap<Sym, usize>; 2],
    structs: Vec<StructInfo<'a>>,
    consts: Vec<ConstInfo<'a>>,
    funcs: Vec<FuncSig<'a>>,
    statics: Vec<StaticDecl>,
    str_lits: FxMap<Vec<u8>, u32>,
    worklist: Vec<u32>,
    fs: FnState,
}

fn align_up(x: u64, a: u64) -> u64 {
    x.div_ceil(a.max(1)) * a.max(1)
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let w = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * w];
    for i in 0..=a.len() {
        d[i * w] = i;
    }
    for (j, x) in d.iter_mut().enumerate().take(w) {
        *x = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[(i - 1) * w + j] + 1).min(d[i * w + j - 1] + 1).min(d[(i - 1) * w + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
        }
    }
    d[a.len() * w + b.len()]
}

/// Interns the names the checker looks up on every compilation (done once for the cached prelude interner).
pub(crate) fn preintern(names: &mut Interner) {
    for n in ["i64", "f64", "u8", "i8", "u16", "i16", "u32", "i32", "str", "ptr", "len", "__heap_base", "handle", "init"] {
        names.intern(n);
    }
    for b in BUILTINS {
        names.intern(b);
    }
}

/// Parsed files of one compilation, with their prelude flag.
pub(crate) struct Input<'a> {
    pub files: Vec<(&'a ast::ParsedFile, bool)>,
}

/// Checks everything; returns the program (`None` if errors were reported).
pub(crate) fn check(
    input: &Input<'_>,
    files: &[std::sync::Arc<SourceFile>],
    names: &mut Interner,
    strings: &[Vec<u8>],
    diags: &mut Diags,
    check_all: bool,
) -> Option<Program> {
    let mut i = |s: &str| names.intern(s);
    let n = Names {
        i64_: i("i64"),
        f64_: i("f64"),
        u8_: i("u8"),
        i8_: i("i8"),
        u16_: i("u16"),
        i16_: i("i16"),
        u32_: i("u32"),
        i32_: i("i32"),
        str_: i("str"),
        ptr: i("ptr"),
        len: i("len"),
        heap_base: i("__heap_base"),
        handle: i("handle"),
        init: i("init"),
    };
    let builtins: FxMap<Sym, &'static str> = BUILTINS.iter().map(|b| (names.intern(b), *b)).collect();
    let names: &Interner = names;
    let mut c = Checker {
        names,
        files,
        strings,
        diags,
        n,
        builtins,
        items: Vec::new(),
        scopes: [FxMap::default(), FxMap::default()],
        structs: Vec::new(),
        consts: Vec::new(),
        funcs: Vec::new(),
        statics: Vec::new(),
        str_lits: FxMap::default(),
        worklist: Vec::new(),
        fs: FnState::default(),
    };
    c.collect(input);
    c.resolve_decls();
    // Bodies: every user function, then the prelude functions they (transitively) call.
    for fid in 0..c.funcs.len() as u32 {
        let f = &c.funcs[fid as usize];
        if f.decl.body.is_some() && (!f.prelude || check_all) {
            c.check_fn(fid);
        }
    }
    while let Some(fid) = c.worklist.pop() {
        if !c.funcs[fid as usize].checked {
            c.check_fn(fid);
        }
    }
    if !c.diags.is_empty() {
        return None;
    }
    let funcs = c
        .funcs
        .into_iter()
        .map(|f| FuncInfo {
            name: c.names.name(f.name).to_string(),
            import: f.decl.body.is_none(),
            prelude: f.prelude,
            export: f.decl.export,
            params: f.tir_params,
            results: u8::from(f.ret.is_some()),
            body: f.body,
            span: f.span,
        })
        .collect();
    let statics = c.statics.into_iter().map(|s| s.info).collect();
    Some(Program { funcs, statics })
}

impl<'a> Checker<'a> {
    fn name(&self, s: Sym) -> &'a str {
        self.names.name(s)
    }

    // ---------------------------------------------------------------------------------------------------------
    // Item collection

    fn collect(&mut self, input: &Input<'a>) {
        for &(file, prelude) in &input.files {
            for item in &file.items {
                let kind = match &item.kind {
                    ItemKind::Include(_) => continue,
                    ItemKind::Fn(decl) => {
                        self.funcs.push(FuncSig {
                            name: decl.name,
                            decl,
                            span: item.name_span,
                            prelude,
                            params: Rc::from([]),
                            ret: None,
                            tir_params: 0,
                            body: None,
                            checked: false,
                        });
                        ItemRef::Func(self.funcs.len() as u32 - 1)
                    }
                    ItemKind::Const { .. } => {
                        self.consts.push(ConstInfo { decl: item, prelude, state: State::Todo, value: None });
                        ItemRef::Const(self.consts.len() as u32 - 1)
                    }
                    ItemKind::Global { .. } | ItemKind::Data { .. } | ItemKind::Buffer { .. } => {
                        let kind = match &item.kind {
                            ItemKind::Global { .. } => StaticKind::Global,
                            ItemKind::Data { .. } => StaticKind::Data,
                            _ => StaticKind::Buffer,
                        };
                        self.statics.push(StaticDecl {
                            ty: Ty::Error,
                            count: 0,
                            info: StaticInfo { kind, size: 0, align: 1, init: Vec::new() },
                        });
                        let id = self.statics.len() as u32 - 1;
                        match kind {
                            StaticKind::Global => ItemRef::Global(id),
                            StaticKind::Data => ItemRef::Data(id),
                            _ => ItemRef::Buffer(id),
                        }
                    }
                    ItemKind::Struct { fields } => {
                        self.structs.push(StructInfo {
                            name: item.name,
                            decl: fields,
                            prelude,
                            state: State::Todo,
                            fields: Vec::new(),
                            size: 0,
                            align: 1,
                        });
                        ItemRef::Struct(self.structs.len() as u32 - 1)
                    }
                };
                let scope = usize::from(!prelude);
                let idx = self.items.len();
                self.items.push(ItemEntry { kind, span: item.name_span, item, prelude });
                if let Some(&prev) = self.scopes[scope].get(&item.name) {
                    let prev_span = self.items[prev].span;
                    let name = self.name(item.name);
                    let msg = format!("`{name}` is defined more than once");
                    let help = format!("the first definition is at {}", self.loc(prev_span));
                    self.diags.err_help(item.name_span, msg, help);
                } else {
                    self.scopes[scope].insert(item.name, idx);
                }
                if self.is_prim(item.name) {
                    let name = self.name(item.name);
                    self.diags.err(item.name_span, format!("`{name}` is a built-in type name and cannot be redefined"));
                }
            }
        }
    }

    /// "file:line" of a span, for notes.
    fn loc(&self, span: Span) -> String {
        match self.files.get(span.file as usize) {
            Some(f) => format!("{}:{}", f.name, f.line_col(span.lo).0),
            None => "?".to_string(),
        }
    }

    fn is_prim(&self, s: Sym) -> bool {
        let n = &self.n;
        [n.i64_, n.f64_, n.u8_, n.i8_, n.u16_, n.i16_, n.u32_, n.i32_, n.str_].contains(&s)
    }

    fn lookup(&self, s: Sym, prelude: bool) -> Option<usize> {
        if !prelude {
            if let Some(&i) = self.scopes[1].get(&s) {
                return Some(i);
            }
        }
        self.scopes[0].get(&s).copied()
    }

    // ---------------------------------------------------------------------------------------------------------
    // Declarations

    fn resolve_decls(&mut self) {
        for id in 0..self.structs.len() as u32 {
            self.struct_layout(id);
        }
        for id in 0..self.consts.len() as u32 {
            self.const_value(id);
        }
        for idx in 0..self.items.len() {
            let (kind, item, prelude) = (self.items[idx].kind, self.items[idx].item, self.items[idx].prelude);
            match (kind, &item.kind) {
                (ItemRef::Func(fid), ItemKind::Fn(decl)) => self.resolve_sig(fid, decl, prelude, item),
                (ItemRef::Global(sid), ItemKind::Global { ty, init }) => self.resolve_global(sid, ty, init, prelude, item),
                (ItemRef::Data(sid), ItemKind::Data { ty, init }) => self.resolve_data(sid, ty, init, prelude, item),
                (ItemRef::Buffer(sid), ItemKind::Buffer { ty }) => {
                    let t = self.resolve_type(ty, prelude);
                    let (elem, count) = match t {
                        Ty::Array(e, n) => (Rc::unwrap_or_clone(e), n),
                        Ty::Error => (Ty::Error, 0),
                        t => (t, 1),
                    };
                    if matches!(elem, Ty::Str | Ty::Void) {
                        self.diags.err(ty.span, "buffers hold storage types (integers, f64, pointers, structs, arrays)");
                    }
                    let (sz, al) = self.size_align(&elem);
                    let s = &mut self.statics[sid as usize];
                    s.ty = elem;
                    s.count = count;
                    s.info.size = sz.saturating_mul(count);
                    s.info.align = al.max(8);
                    if s.info.size > (1u64 << 34) {
                        self.diags.err(ty.span, "buffer is larger than 16 GiB");
                    }
                }
                _ => {}
            }
        }
    }

    fn resolve_sig(&mut self, fid: u32, decl: &'a ast::FnDecl, prelude: bool, item: &ast::Item) {
        let mut params = Vec::new();
        let mut tir = 0u32;
        for p in &decl.params {
            let t = self.resolve_type(&p.ty, prelude);
            if !t.is_value() {
                let ts = self.ty_str(&t);
                self.diags.err_help(
                    p.ty.span,
                    format!("parameters must have a value type (`i64`, `f64`, a pointer or `str`), found `{ts}`"),
                    "pass a pointer to storage types and structs, e.g. `*u8` or `*Point`",
                );
            }
            if params.iter().any(|(n, _, _)| *n == p.name) {
                let pn = self.name(p.name);
                self.diags.err(p.span, format!("parameter `{pn}` is declared twice"));
            }
            tir += if t == Ty::Str { 2 } else { 1 };
            params.push((p.name, t, p.span));
        }
        let ret = decl.ret.as_ref().map(|r| {
            let t = self.resolve_type(r, prelude);
            if t == Ty::Str {
                self.diags.err_help(
                    r.span,
                    "functions cannot return `str` (TIR functions return at most one value)",
                    "return a pointer and pass the length separately, or write into a buffer",
                );
            } else if !t.is_value() {
                let ts = self.ty_str(&t);
                self.diags.err(r.span, format!("return type must be `i64`, `f64` or a pointer, found `{ts}`"));
            }
            t
        });
        let fname = self.name(decl.name);
        if decl.body.is_none() && tir > k10_ir::MAX_HOST_PARAMS as u32 {
            self.diags.err(
                item.name_span,
                format!("host function `{fname}` takes {tir} values; host functions take at most 5"),
            );
        }
        if tir > 255 {
            self.diags.err(item.name_span, format!("function `{fname}` takes {tir} values; the limit is 255"));
        }
        if decl.export && (decl.name == self.n.handle || decl.name == self.n.init) {
            if tir != 0 {
                self.diags.err(item.name_span, format!("the exported entry point `{fname}` must take no parameters"));
            }
            if matches!(ret, Some(ref t) if *t != Ty::I64 && *t != Ty::Error) {
                self.diags.err(item.name_span, format!("the exported entry point `{fname}` must return nothing or `i64`"));
            }
        }
        if decl.export && prelude {
            self.diags.err(item.name_span, "the prelude cannot export functions");
        }
        let f = &mut self.funcs[fid as usize];
        f.params = params.into();
        f.ret = ret;
        f.tir_params = tir;
    }

    fn resolve_global(&mut self, sid: u32, ty: &Option<ast::TypeExpr>, init: &Option<ast::Expr>, prelude: bool, item: &ast::Item) {
        let declared = ty.as_ref().map(|t| (self.resolve_type(t, prelude), t.span));
        let value = init.as_ref().map(|e| (self.const_eval_null(e, prelude), e.span));
        let ty = match (&declared, &value) {
            (Some((t, _)), _) => t.clone(),
            (None, Some((Some(CV::F(_)), _))) => Ty::F64,
            (None, Some((Some(CV::I(_)), _))) => Ty::I64,
            (None, Some((None, sp))) => {
                if init.as_ref().is_some_and(|e| matches!(e.kind, ExprKind::Null)) {
                    self.diags.err(*sp, "cannot infer the type of `null`; annotate the global, e.g. `global p: *u8 = null;`");
                }
                Ty::Error
            }
            (None, None) => {
                let name = self.name(item.name);
                self.diags.err_help(
                    item.name_span,
                    format!("global `{name}` needs a type or an initializer"),
                    format!("e.g. `global {name}: i64;` or `global {name} = 0;`"),
                );
                Ty::Error
            }
        };
        if !matches!(ty, Ty::I64 | Ty::F64 | Ty::Ptr(_) | Ty::Error) {
            let ts = self.ty_str(&ty);
            let sp = declared.as_ref().map(|d| d.1).unwrap_or(item.name_span);
            self.diags.err_help(
                sp,
                format!("globals hold one value (`i64`, `f64` or a pointer), found `{ts}`"),
                "use `buffer name: T;` for static storage of other types",
            );
        }
        if let Some(e) = init.as_ref().filter(|e| matches!(e.kind, ExprKind::Null)) {
            if !matches!(ty, Ty::Ptr(_) | Ty::Error) {
                let ts = self.ty_str(&ty);
                self.diags.err(e.span, format!("`null` is not a valid `{ts}`"));
            }
        }
        let mut bytes = [0u8; 8];
        if let Some((v, sp)) = value {
            match (&ty, v) {
                (Ty::I64, Some(CV::I(x))) | (Ty::Ptr(_), Some(CV::I(x))) => bytes = x.to_le_bytes(),
                (Ty::F64, Some(CV::F(x))) => bytes = x.to_bits().to_le_bytes(),
                (Ty::Error, _) | (_, None) => {}
                (t, Some(v)) => {
                    let ts = self.ty_str(t);
                    let vs = if matches!(v, CV::F(_)) { "f64" } else { "i64" };
                    self.diags.err(sp, format!("mismatched types: the global is `{ts}` but its initializer is `{vs}`"));
                }
            }
        }
        let s = &mut self.statics[sid as usize];
        s.ty = ty;
        s.count = 1;
        s.info.size = 8;
        s.info.align = 8;
        s.info.init = if bytes == [0; 8] { Vec::new() } else { bytes.to_vec() };
    }

    /// Like `const_eval`, but `null` yields `None` without an error (the zero initializer of pointer globals).
    fn const_eval_null(&mut self, e: &ast::Expr, prelude: bool) -> Option<CV> {
        if matches!(e.kind, ExprKind::Null) {
            return None;
        }
        self.const_eval(e, prelude, true)
    }

    fn resolve_data(&mut self, sid: u32, ty: &Option<ast::TypeExpr>, init: &DataInit, prelude: bool, item: &ast::Item) {
        let (elem, len) = match ty {
            None => (Ty::U8, None),
            Some(t) => match &t.kind {
                TypeKind::Array(e, n) => {
                    let et = self.resolve_type(e, prelude);
                    let n = n.as_ref().and_then(|n| self.array_len(n, prelude));
                    (et, n)
                }
                _ => {
                    self.diags.err_help(
                        t.span,
                        "`data` needs an array type",
                        "e.g. `data K: [u32; 4] = [1, 2, 3, 4];` or `data MSG = \"text\";`",
                    );
                    (Ty::Error, None)
                }
            },
        };
        let mut bytes: Vec<u8> = Vec::new();
        let width = match &elem {
            Ty::Error => 1,
            t if t.is_scalar_storage() && !matches!(t, Ty::Ptr(_)) => self.size_align(t).0,
            t => {
                let ts = self.ty_str(t);
                self.diags.err(item.name_span, format!("data elements must be integers or `f64`, found `{ts}`"));
                1
            }
        };
        let count = match init {
            DataInit::Str(i) => {
                if !matches!(elem, Ty::U8 | Ty::I8 | Ty::Error) {
                    self.diags.err(item.name_span, "a string initializer needs an element type of `u8` or `i8`");
                }
                bytes.extend_from_slice(&self.strings[*i as usize]);
                bytes.len() as u64
            }
            DataInit::List(list) => {
                for e in list {
                    let v = self.const_eval(e, prelude, true);
                    match (v, &elem) {
                        (_, Ty::Error) | (None, _) => bytes.extend(std::iter::repeat_n(0, width as usize)),
                        (Some(CV::F(f)), Ty::F64) => bytes.extend_from_slice(&f.to_bits().to_le_bytes()),
                        (Some(CV::I(_)), Ty::F64) => {
                            self.diags.err_help(e.span, "expected an `f64` element, found an integer", "write it as a float literal, e.g. `2.0`");
                        }
                        (Some(CV::F(_)), _) => {
                            self.diags.err(e.span, "expected an integer element, found an `f64`");
                        }
                        (Some(CV::I(x)), t) => {
                            let bits = width * 8;
                            if bits < 64 {
                                let fits = (x >= 0 && (x as u64) < (1u64 << bits)) || (x < 0 && x >= -(1i64 << (bits - 1)));
                                if !fits {
                                    let ts = self.ty_str(t);
                                    self.diags.err(e.span, format!("value {x} does not fit in `{ts}`"));
                                }
                            }
                            bytes.extend_from_slice(&x.to_le_bytes()[..width as usize]);
                        }
                    }
                }
                list.len() as u64
            }
        };
        let count = match len {
            Some(n) if n < count => {
                self.diags.err(item.name_span, format!("the initializer has {count} elements but the array holds {n}"));
                n
            }
            Some(n) => n,
            None => count,
        };
        let s = &mut self.statics[sid as usize];
        s.ty = elem;
        s.count = count;
        s.info.size = count * width;
        s.info.align = width;
        if bytes.iter().all(|&b| b == 0) {
            bytes.clear();
        }
        s.info.init = bytes;
    }

    fn array_len(&mut self, e: &ast::Expr, prelude: bool) -> Option<u64> {
        match self.const_eval(e, prelude, true) {
            Some(CV::I(n)) if n >= 0 => Some(n as u64),
            Some(CV::I(n)) => {
                self.diags.err(e.span, format!("array length must not be negative (found {n})"));
                None
            }
            Some(CV::F(_)) => {
                self.diags.err(e.span, "array length must be an integer");
                None
            }
            None => None,
        }
    }

    fn resolve_type(&mut self, t: &ast::TypeExpr, prelude: bool) -> Ty {
        match &t.kind {
            TypeKind::Name(s) => {
                let n = &self.n;
                let s = *s;
                if s == n.i64_ {
                    Ty::I64
                } else if s == n.f64_ {
                    Ty::F64
                } else if s == n.u8_ {
                    Ty::U8
                } else if s == n.i8_ {
                    Ty::I8
                } else if s == n.u16_ {
                    Ty::U16
                } else if s == n.i16_ {
                    Ty::I16
                } else if s == n.u32_ {
                    Ty::U32
                } else if s == n.i32_ {
                    Ty::I32
                } else if s == n.str_ {
                    Ty::Str
                } else {
                    match self.lookup(s, prelude).map(|i| self.items[i].kind) {
                        Some(ItemRef::Struct(id)) => Ty::Struct(id),
                        Some(_) => {
                            let name = self.name(s);
                            self.diags.err(t.span, format!("`{name}` is not a type"));
                            Ty::Error
                        }
                        None => {
                            let name = self.name(s);
                            let mut cands: Vec<&str> =
                                vec!["i64", "f64", "u8", "i8", "u16", "i16", "u32", "i32", "str"];
                            for sc in [&self.scopes[1], &self.scopes[0]] {
                                for (k, &i) in sc {
                                    if matches!(self.items[i].kind, ItemRef::Struct(_)) {
                                        cands.push(self.name(*k));
                                    }
                                }
                            }
                            let msg = format!("unknown type `{name}`");
                            match suggest(name, cands.into_iter()) {
                                Some(c) => self.diags.err_help(t.span, msg, format!("did you mean `{c}`?")),
                                None => self.diags.err(t.span, msg),
                            }
                            Ty::Error
                        }
                    }
                }
            }
            TypeKind::Ptr(inner) => {
                let i = self.resolve_type(inner, prelude);
                if matches!(i, Ty::Str | Ty::Void) {
                    self.diags.err(inner.span, "`str` is a (pointer, length) pair and cannot be pointed to");
                }
                Ty::ptr(i)
            }
            TypeKind::Array(elem, len) => {
                let e = self.resolve_type(elem, prelude);
                if e == Ty::Str {
                    self.diags.err(elem.span, "arrays of `str` are not supported");
                }
                match len {
                    Some(n) => match self.array_len(n, prelude) {
                        Some(n) => Ty::Array(Rc::new(e), n),
                        None => Ty::Error,
                    },
                    None => {
                        self.diags.err_help(
                            t.span,
                            "array types need a length here",
                            "write `[T; N]` (only `data` declarations can infer the length)",
                        );
                        Ty::Error
                    }
                }
            }
        }
    }

    fn struct_layout(&mut self, id: u32) -> Option<(u64, u64)> {
        let s = &self.structs[id as usize];
        match s.state {
            State::Done => return Some((s.size, s.align)),
            State::Busy => {
                let name = self.name(s.name);
                let sp = self.items[self.lookup(s.name, s.prelude).unwrap_or(0)].span;
                self.diags.err_help(
                    sp,
                    format!("struct `{name}` contains itself"),
                    format!("use a pointer field (`*{name}`) for recursive structures"),
                );
                return None;
            }
            State::Todo => {}
        }
        self.structs[id as usize].state = State::Busy;
        let (decl, prelude) = (self.structs[id as usize].decl, self.structs[id as usize].prelude);
        let mut fields: Vec<(Sym, Ty, u64)> = Vec::new();
        let mut off = 0u64;
        let mut align = 1u64;
        let mut ok = true;
        for f in decl {
            let t = self.resolve_type(&f.ty, prelude);
            if matches!(t, Ty::Str | Ty::Void) {
                self.diags.err(f.ty.span, "struct fields must be storage types (`str` is not)");
            }
            if fields.iter().any(|(n, _, _)| *n == f.name) {
                let fname = self.name(f.name);
                self.diags.err(f.span, format!("field `{fname}` is declared twice"));
            }
            let (sz, al) = match &t {
                Ty::Struct(sid) => match self.struct_layout(*sid) {
                    Some(x) => x,
                    None => {
                        ok = false;
                        (0, 1)
                    }
                },
                t => self.size_align(t),
            };
            off = align_up(off, al);
            fields.push((f.name, t, off));
            off += sz;
            align = align.max(al);
        }
        let s = &mut self.structs[id as usize];
        s.fields = fields;
        s.size = align_up(off, align);
        s.align = align;
        s.state = State::Done;
        if ok {
            Some((s.size, s.align))
        } else {
            None
        }
    }

    fn size_align(&mut self, t: &Ty) -> (u64, u64) {
        match t {
            Ty::U8 | Ty::I8 => (1, 1),
            Ty::U16 | Ty::I16 => (2, 2),
            Ty::U32 | Ty::I32 => (4, 4),
            Ty::I64 | Ty::F64 | Ty::Ptr(_) | Ty::Null => (8, 8),
            Ty::Str => (16, 8),
            Ty::Array(e, n) => {
                let (s, a) = self.size_align(e);
                (s.saturating_mul(*n), a)
            }
            Ty::Struct(id) => self.struct_layout(*id).unwrap_or((0, 1)),
            Ty::Void | Ty::Error => (0, 1),
        }
    }

    fn size_of(&mut self, t: &Ty) -> u64 {
        self.size_align(t).0
    }

    pub(crate) fn ty_str(&self, t: &Ty) -> String {
        match t {
            Ty::I64 => "i64".into(),
            Ty::F64 => "f64".into(),
            Ty::U8 => "u8".into(),
            Ty::I8 => "i8".into(),
            Ty::U16 => "u16".into(),
            Ty::I16 => "i16".into(),
            Ty::U32 => "u32".into(),
            Ty::I32 => "i32".into(),
            Ty::Ptr(i) => format!("*{}", self.ty_str(i)),
            Ty::Array(e, n) => format!("[{}; {n}]", self.ty_str(e)),
            Ty::Struct(id) => self.name(self.structs[*id as usize].name).to_string(),
            Ty::Str => "str".into(),
            Ty::Void => "()".into(),
            Ty::Null => "null".into(),
            Ty::Error => "{error}".into(),
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // Constant evaluation

    fn const_value(&mut self, id: u32) -> Option<CV> {
        let c = &self.consts[id as usize];
        match c.state {
            State::Done => return c.value,
            State::Busy => {
                let name = self.name(c.decl.name);
                self.diags.err(c.decl.name_span, format!("constant `{name}` depends on itself"));
                return None;
            }
            State::Todo => {}
        }
        let (decl, prelude) = (c.decl, c.prelude);
        self.consts[id as usize].state = State::Busy;
        let (ty, value) = match &decl.kind {
            ItemKind::Const { ty, value } => (ty, value),
            _ => unreachable!(),
        };
        let mut v = self.const_eval(value, prelude, true);
        if let Some(t) = ty {
            let t = self.resolve_type(t, prelude);
            match (&t, v) {
                (Ty::I64, Some(CV::I(_))) | (Ty::F64, Some(CV::F(_))) | (_, None) | (Ty::Error, _) => {}
                (Ty::I64 | Ty::F64, Some(x)) => {
                    let ts = self.ty_str(&t);
                    let found = if matches!(x, CV::F(_)) { "f64" } else { "i64" };
                    self.diags.err(value.span, format!("mismatched types: expected `{ts}`, found `{found}`"));
                    v = None;
                }
                _ => {
                    let ts = self.ty_str(&t);
                    self.diags.err(decl.name_span, format!("constants are `i64` or `f64`, not `{ts}`"));
                    v = None;
                }
            }
        }
        let c = &mut self.consts[id as usize];
        c.state = State::Done;
        c.value = v;
        v
    }

    /// Evaluates a constant expression. With `report`, errors are reported; otherwise `None` is returned silently.
    fn const_eval(&mut self, e: &ast::Expr, prelude: bool, report: bool) -> Option<CV> {
        macro_rules! fail {
            ($sp:expr, $($msg:tt)*) => {{
                if report {
                    self.diags.err($sp, format!($($msg)*));
                }
                return None;
            }};
        }
        Some(match &e.kind {
            ExprKind::Int(v) => CV::I(*v as i64),
            ExprKind::Float(f) => CV::F(*f),
            ExprKind::Char(c) => CV::I(*c as i64),
            ExprKind::Bool(b) => CV::I(*b as i64),
            ExprKind::Paren(inner) => return self.const_eval(inner, prelude, report),
            ExprKind::Name(s) => match self.lookup(*s, prelude).map(|i| self.items[i].kind) {
                Some(ItemRef::Const(id)) => return self.const_value(id),
                _ => {
                    let name = self.name(*s);
                    fail!(e.span, "`{name}` is not a constant (constant expressions may use literals, `const` items, `sizeof`, `offsetof` and `countof`)")
                }
            },
            ExprKind::Unary(op, a) => {
                let a = self.const_eval(a, prelude, report)?;
                match (op, a) {
                    (UnOp::Neg, CV::I(x)) => CV::I(x.wrapping_neg()),
                    (UnOp::Neg, CV::F(x)) => CV::F(-x),
                    (UnOp::Not, CV::I(x)) => CV::I((x == 0) as i64),
                    (UnOp::BitNot, CV::I(x)) => CV::I(!x),
                    _ => fail!(e.span, "invalid operand for `{}` in a constant expression", match op {
                        UnOp::Neg => "-",
                        UnOp::Not => "!",
                        UnOp::BitNot => "~",
                        UnOp::Deref => "*",
                    }),
                }
            }
            ExprKind::Binary(op, a, b) => {
                let x = self.const_eval(a, prelude, report)?;
                let y = self.const_eval(b, prelude, report)?;
                match (x, y) {
                    (CV::I(x), CV::I(y)) => CV::I(match op {
                        BinOp::Add => x.wrapping_add(y),
                        BinOp::Sub => x.wrapping_sub(y),
                        BinOp::Mul => x.wrapping_mul(y),
                        BinOp::Div | BinOp::Rem if y == 0 => fail!(b.span, "division by zero in a constant expression"),
                        BinOp::Div => x.wrapping_div(y),
                        BinOp::Rem => x.wrapping_rem(y),
                        BinOp::And => x & y,
                        BinOp::Or => x | y,
                        BinOp::Xor => x ^ y,
                        BinOp::Shl => x.wrapping_shl((y & 63) as u32),
                        BinOp::Shr => x.wrapping_shr((y & 63) as u32),
                        BinOp::Ushr => ((x as u64) >> (y & 63)) as i64,
                        BinOp::Eq => (x == y) as i64,
                        BinOp::Ne => (x != y) as i64,
                        BinOp::Lt => (x < y) as i64,
                        BinOp::Le => (x <= y) as i64,
                        BinOp::Gt => (x > y) as i64,
                        BinOp::Ge => (x >= y) as i64,
                        BinOp::AndAnd => (x != 0 && y != 0) as i64,
                        BinOp::OrOr => (x != 0 || y != 0) as i64,
                    }),
                    (CV::F(x), CV::F(y)) => match op {
                        BinOp::Add => CV::F(x + y),
                        BinOp::Sub => CV::F(x - y),
                        BinOp::Mul => CV::F(x * y),
                        BinOp::Div => CV::F(x / y),
                        BinOp::Eq => CV::I((x == y) as i64),
                        BinOp::Ne => CV::I((x != y) as i64),
                        BinOp::Lt => CV::I((x < y) as i64),
                        BinOp::Le => CV::I((x <= y) as i64),
                        BinOp::Gt => CV::I((x > y) as i64),
                        BinOp::Ge => CV::I((x >= y) as i64),
                        _ => fail!(e.span, "`{}` is not defined for `f64`", op.text()),
                    },
                    _ => fail!(e.span, "mismatched operand types `i64` and `f64` for `{}` (convert with `as`)", op.text()),
                }
            }
            ExprKind::Cast(a, t) => {
                let v = self.const_eval(a, prelude, report)?;
                let t = self.resolve_type(t, prelude);
                match (v, &t) {
                    (CV::I(x), Ty::I64) => CV::I(x),
                    (CV::F(x), Ty::I64) => CV::I(k10_ir::sem::f_to_i(x.to_bits() as i64)),
                    (CV::I(x), Ty::F64) => CV::F(x as f64),
                    (CV::F(x), Ty::F64) => CV::F(x),
                    (CV::I(x), Ty::U8) => CV::I(x & 0xFF),
                    (CV::I(x), Ty::I8) => CV::I(x as i8 as i64),
                    (CV::I(x), Ty::U16) => CV::I(x & 0xFFFF),
                    (CV::I(x), Ty::I16) => CV::I(x as i16 as i64),
                    (CV::I(x), Ty::U32) => CV::I(x & 0xFFFF_FFFF),
                    (CV::I(x), Ty::I32) => CV::I(x as i32 as i64),
                    _ => {
                        let ts = self.ty_str(&t);
                        fail!(e.span, "cannot cast to `{ts}` in a constant expression")
                    }
                }
            }
            ExprKind::Sizeof(arg) => CV::I(self.sizeof(arg, prelude, e.span, report)? as i64),
            ExprKind::Offsetof(t, f, fspan) => CV::I(self.offsetof(t, *f, *fspan, prelude)? as i64),
            ExprKind::Countof(a) => CV::I(self.countof(a, prelude)? as i64),
            _ => fail!(e.span, "not a constant expression"),
        })
    }

    fn sizeof(&mut self, arg: &SizeArg, prelude: bool, span: Span, report: bool) -> Option<u64> {
        match arg {
            SizeArg::Type(t) => {
                let t = self.resolve_type(t, prelude);
                Some(self.size_of(&t))
            }
            SizeArg::Expr(e) => {
                let mut inner = &**e;
                while let ExprKind::Paren(i) = &inner.kind {
                    inner = i;
                }
                match &inner.kind {
                    ExprKind::Str(i) => Some(self.strings[*i as usize].len() as u64),
                    ExprKind::Name(s) => {
                        if self.find_local(*s).is_some() {
                            let lv = self.find_local(*s).unwrap();
                            return Some(if lv.ty == Ty::Str { 16 } else { 8 });
                        }
                        match self.lookup(*s, prelude).map(|i| self.items[i].kind) {
                            Some(ItemRef::Struct(id)) => Some(self.struct_layout(id).map(|x| x.0).unwrap_or(0)),
                            Some(ItemRef::Data(id) | ItemRef::Buffer(id)) => Some(self.statics[id as usize].info.size),
                            Some(ItemRef::Global(_) | ItemRef::Const(_)) => Some(8),
                            Some(ItemRef::Func(_)) => {
                                if report {
                                    self.diags.err(span, "`sizeof` of a function is meaningless");
                                }
                                None
                            }
                            None => {
                                if report {
                                    let name = self.name(*s);
                                    self.diags.err(inner.span, format!("cannot find `{name}` (`sizeof` takes a type, a `data`/`buffer` name, a string literal or a variable)"));
                                }
                                None
                            }
                        }
                    }
                    _ => {
                        if report {
                            self.diags.err_help(
                                inner.span,
                                "`sizeof` takes a type, a `data`/`buffer` name, a string literal or a variable",
                                "every value type is 8 bytes",
                            );
                        }
                        None
                    }
                }
            }
        }
    }

    fn offsetof(&mut self, t: &ast::TypeExpr, f: Sym, fspan: Span, prelude: bool) -> Option<u64> {
        let tspan = t.span;
        let ty = self.resolve_type(t, prelude);
        match ty {
            Ty::Struct(id) => {
                self.struct_layout(id);
                match self.structs[id as usize].fields.iter().find(|x| x.0 == f) {
                    Some(x) => Some(x.2),
                    None => {
                        self.no_field(id, f, fspan);
                        None
                    }
                }
            }
            Ty::Error => None,
            t => {
                let ts = self.ty_str(&t);
                self.diags.err(tspan, format!("`offsetof` needs a struct type, found `{ts}`"));
                None
            }
        }
    }

    fn countof(&mut self, e: &ast::Expr, prelude: bool) -> Option<u64> {
        if let ExprKind::Name(s) = &e.kind {
            if let Some(ItemRef::Data(id) | ItemRef::Buffer(id)) = self.lookup(*s, prelude).map(|i| self.items[i].kind) {
                return Some(self.statics[id as usize].count);
            }
        }
        self.diags.err(e.span, "`countof` takes the name of a `data` or `buffer` item");
        None
    }

    fn no_field(&mut self, sid: u32, f: Sym, span: Span) {
        let s = &self.structs[sid as usize];
        let sname = self.name(s.name);
        let fname = self.name(f);
        let fields: Vec<&str> = s.fields.iter().map(|x| self.name(x.0)).collect();
        let msg = format!("struct `{sname}` has no field `{fname}`");
        match suggest(fname, fields.iter().copied()) {
            Some(c) => self.diags.err_help(span, msg, format!("did you mean `{c}`?")),
            None => {
                let list = fields.join("`, `");
                self.diags.err_help(span, msg, format!("its fields are `{list}`"))
            }
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // Functions

    fn check_fn(&mut self, fid: u32) {
        let f = &mut self.funcs[fid as usize];
        f.checked = true;
        let decl = f.decl;
        let Some(body) = &decl.body else { return };
        let params = f.params.clone();
        let ret = f.ret.clone();
        let prelude = f.prelude;
        let tir_params = f.tir_params;
        self.fs = FnState { prelude, ret: ret.clone(), ..FnState::default() };
        self.fs.scopes.push(Vec::new());
        for (name, ty, _) in params.iter() {
            let id = self.fs.num_locals;
            self.fs.num_locals += if *ty == Ty::Str { 2 } else { 1 };
            self.fs.scopes[0].push(LocalVar { name: *name, id, ty: ty.clone(), mutable: true });
        }
        let mut stmts = Vec::new();
        let diverges = self.block_stmts(&body.stmts, &mut stmts);
        if let Some(r) = &ret {
            if !diverges && *r != Ty::Error {
                let fname = self.name(decl.name);
                let rs = self.ty_str(r);
                let end = Span::new(body.span.file, body.span.hi.saturating_sub(1), body.span.hi);
                self.diags.err_help(
                    end,
                    format!("function `{fname}` must return a value of type `{rs}` on every path"),
                    "add a `return` statement before the end of the function",
                );
            }
        }
        let b = hir::Body { params: tir_params, num_locals: self.fs.num_locals, results: u8::from(ret.is_some()), stmts };
        self.funcs[fid as usize].body = Some(b);
    }

    fn find_local(&self, s: Sym) -> Option<&LocalVar> {
        for sc in self.fs.scopes.iter().rev() {
            if let Some(v) = sc.iter().rev().find(|v| v.name == s) {
                return Some(v);
            }
        }
        None
    }

    fn new_local(&mut self, name: Sym, ty: Ty, mutable: bool) -> hir::LocalId {
        let id = self.fs.num_locals;
        self.fs.num_locals += if ty == Ty::Str { 2 } else { 1 };
        self.fs.scopes.last_mut().unwrap().push(LocalVar { name, id, ty, mutable });
        id
    }

    /// Checks statements in a new scope; returns whether they diverge. Statements after a diverging one are
    /// checked but not emitted.
    fn block_stmts(&mut self, stmts: &[ast::Stmt], out: &mut Vec<hir::Stmt>) -> bool {
        self.fs.scopes.push(Vec::new());
        let mut diverges = false;
        let mut sink = Vec::new();
        for s in stmts {
            let target = if diverges { &mut sink } else { &mut *out };
            if self.stmt(s, target) {
                diverges = true;
            }
            sink.clear();
        }
        self.fs.scopes.pop();
        diverges
    }

    fn scoped_block(&mut self, b: &ast::Block) -> (Vec<hir::Stmt>, bool) {
        let mut v = Vec::new();
        let d = self.block_stmts(&b.stmts, &mut v);
        (v, d)
    }

    fn stmt(&mut self, s: &ast::Stmt, out: &mut Vec<hir::Stmt>) -> bool {
        match &s.kind {
            StmtKind::Empty => false,
            StmtKind::Let { name, name_span, ty, init } => {
                let declared = ty.as_ref().map(|t| {
                    let ty = self.resolve_type(t, self.fs.prelude);
                    if !ty.is_value() {
                        let ts = self.ty_str(&ty);
                        let help = if matches!(ty, Ty::U8 | Ty::I8 | Ty::U16 | Ty::I16 | Ty::U32 | Ty::I32) {
                            format!("locals are `i64`; truncate explicitly with `as {ts}` if needed")
                        } else {
                            "locals hold values (`i64`, `f64`, pointers, `str`); use a pointer for structs and arrays".to_string()
                        };
                        self.diags.err_help(t.span, format!("`{ts}` cannot be the type of a local variable"), help);
                        Ty::Error
                    } else {
                        ty
                    }
                });
                let (ty, a, b) = match init {
                    Some(e) => {
                        let v = self.value(e, declared.as_ref());
                        match &declared {
                            Some(Ty::Str) => {
                                let (p, n) = self.str_parts(v, e.span);
                                (Ty::Str, Some(p), Some(n))
                            }
                            Some(t) => {
                                let x = self.coerce(v, t, e.span);
                                (t.clone(), Some(x), None)
                            }
                            None => match v.ty {
                                Ty::Null => {
                                    self.diags.err_help(
                                        e.span,
                                        "cannot infer the type of `null`",
                                        "annotate the variable, e.g. `let p: *u8 = null;`",
                                    );
                                    (Ty::Error, Some(v.e), None)
                                }
                                Ty::Void => {
                                    self.diags.err(e.span, "this expression produces no value");
                                    (Ty::Error, None, None)
                                }
                                Ty::Str => (Ty::Str, Some(v.e), v.len),
                                t => (t, Some(v.e), None),
                            },
                        }
                    }
                    None => match declared {
                        Some(t) => (t, None, None),
                        None => {
                            let n = self.name(*name);
                            self.diags.err_help(
                                *name_span,
                                format!("`let {n};` needs a type or an initializer"),
                                format!("e.g. `let {n}: i64;` (zero-initialized) or `let {n} = 0;`"),
                            );
                            (Ty::Error, None, None)
                        }
                    },
                };
                let id = self.new_local(*name, ty.clone(), true);
                out.push(hir::Stmt::Let(id, a));
                if ty == Ty::Str {
                    out.push(hir::Stmt::Let(id + 1, b));
                }
                false
            }
            StmtKind::Assign { target, op, value } => {
                self.assign(target, *op, value, s.span, out);
                false
            }
            StmtKind::Expr(e) => {
                let mut inner = e;
                while let ExprKind::Paren(i) = &inner.kind {
                    inner = i;
                }
                match &inner.kind {
                    ExprKind::Call { .. } => {}
                    ExprKind::Binary(BinOp::Eq, _, _) => {
                        self.diags.err_help(e.span, "this comparison has no effect", "use `=` to assign");
                        return false;
                    }
                    _ => {
                        self.diags.err_help(
                            e.span,
                            "this expression has no effect",
                            "only function calls can be used as statements",
                        );
                        return false;
                    }
                }
                let v = self.value(e, None);
                if v.ty == Ty::Str {
                    self.diags.err(e.span, "unused `str` value");
                    return false;
                }
                let diverges = matches!(v.e.kind, EK::Unreachable);
                out.push(hir::Stmt::Expr(v.e));
                diverges
            }
            StmtKind::If { cond, then, els } => {
                let c = self.cond(cond);
                let (t, td) = self.scoped_block(then);
                let (e, ed) = match els {
                    Some(st) => {
                        let mut v = Vec::new();
                        self.fs.scopes.push(Vec::new());
                        let d = self.stmt(st, &mut v);
                        self.fs.scopes.pop();
                        (v, d)
                    }
                    None => (Vec::new(), false),
                };
                out.push(hir::Stmt::If(c, t, e));
                td && ed
            }
            StmtKind::While { cond, body } => {
                let c = self.cond(cond);
                let infinite = matches!(self.const_eval(cond, self.fs.prelude, false), Some(CV::I(x)) if x != 0);
                self.fs.loops.push((false, false));
                let (b, _) = self.scoped_block(body);
                let (brk, cont) = self.fs.loops.pop().unwrap();
                out.push(hir::Stmt::While { cond: c, body: b, brk, cont });
                infinite && !brk
            }
            StmtKind::Loop { body } => {
                self.fs.loops.push((false, false));
                let (b, _) = self.scoped_block(body);
                let (brk, _) = self.fs.loops.pop().unwrap();
                out.push(hir::Stmt::Loop { body: b, brk });
                !brk
            }
            StmtKind::For { var, start, end, body } => {
                let st = self.value(start, Some(&Ty::I64));
                let st = self.coerce(st, &Ty::I64, start.span);
                let en = self.value(end, Some(&Ty::I64));
                let en = self.coerce(en, &Ty::I64, end.span);
                self.fs.scopes.push(Vec::new());
                let id = self.new_local(*var, Ty::I64, false);
                self.fs.loops.push((false, false));
                let (b, _) = self.scoped_block(body);
                let (brk, cont) = self.fs.loops.pop().unwrap();
                self.fs.scopes.pop();
                out.push(hir::Stmt::For { var: id, start: st, end: en, body: b, brk, cont });
                false
            }
            StmtKind::Break | StmtKind::Continue => {
                let is_break = matches!(s.kind, StmtKind::Break);
                match self.fs.loops.last_mut() {
                    Some(l) => {
                        if is_break {
                            l.0 = true;
                        } else {
                            l.1 = true;
                        }
                        out.push(if is_break { hir::Stmt::Break } else { hir::Stmt::Continue });
                    }
                    None => {
                        let w = if is_break { "break" } else { "continue" };
                        self.diags.err(s.span, format!("`{w}` outside of a loop"));
                    }
                }
                true
            }
            StmtKind::Return(v) => {
                let ret = self.fs.ret.clone();
                match (v, ret) {
                    (None, None) => out.push(hir::Stmt::Return(None)),
                    (Some(e), Some(r)) => {
                        let v = self.value(e, Some(&r));
                        let x = self.coerce(v, &r, e.span);
                        out.push(hir::Stmt::Return(Some(x)));
                    }
                    (None, Some(r)) => {
                        let rs = self.ty_str(&r);
                        self.diags.err(s.span, format!("this function returns `{rs}`: `return` needs a value"));
                    }
                    (Some(e), None) => {
                        let v = self.value(e, None);
                        if v.ty != Ty::Error {
                            let vs = self.ty_str(&v.ty);
                            self.diags.err_help(
                                e.span,
                                "this function returns no value",
                                format!("declare a return type: `-> {vs}`"),
                            );
                        }
                    }
                }
                true
            }
            StmtKind::Block(b) => {
                let (v, d) = self.scoped_block(b);
                out.push(hir::Stmt::Block(v));
                d
            }
        }
    }

    fn assign(&mut self, target: &ast::Expr, op: Option<BinOp>, value: &ast::Expr, span: Span, out: &mut Vec<hir::Stmt>) {
        let mut t = target;
        while let ExprKind::Paren(i) = &t.kind {
            t = i;
        }
        if let ExprKind::Name(s) = &t.kind {
            if let Some(lv) = self.find_local(*s).cloned() {
                let name = self.name(*s);
                if !lv.mutable {
                    self.diags.err_help(
                        t.span,
                        format!("cannot assign to the loop variable `{name}`"),
                        "use a `while` loop to control the iteration manually",
                    );
                    return;
                }
                if lv.ty == Ty::Str {
                    if op.is_some() {
                        self.diags.err(span, "compound assignment is not defined for `str`");
                        return;
                    }
                    let v = self.value(value, Some(&Ty::Str));
                    let (p, n) = self.str_parts(v, value.span);
                    out.push(hir::Stmt::Set(lv.id, p));
                    out.push(hir::Stmt::Set(lv.id + 1, n));
                    return;
                }
                let cur = Typed::new(Expr::new(EK::Local(lv.id)), lv.ty.clone());
                let v = match op {
                    None => self.value(value, Some(&lv.ty)),
                    Some(op) => {
                        let rhs = self.value(value, Some(&lv.ty));
                        self.binary(op, cur, rhs, span)
                    }
                };
                let x = self.coerce(v, &lv.ty, value.span);
                out.push(hir::Stmt::Set(lv.id, x));
                return;
            }
            match self.lookup(*s, self.fs.prelude).map(|i| self.items[i].kind) {
                Some(ItemRef::Global(_)) | None => {}
                Some(k) => {
                    let name = self.name(*s);
                    let what = match k {
                        ItemRef::Const(_) => "constant",
                        ItemRef::Data(_) => "data item (assign to its elements: `NAME[i] = v`)",
                        ItemRef::Buffer(_) => "buffer (assign to its elements: `NAME[i] = v`)",
                        ItemRef::Func(_) => "function",
                        _ => "type",
                    };
                    self.diags.err(t.span, format!("cannot assign to `{name}`: it is a {what}"));
                    return;
                }
            }
        }
        if !matches!(t.kind, ExprKind::Name(_) | ExprKind::Unary(UnOp::Deref, _) | ExprKind::Index(..) | ExprKind::Field(..)) {
            self.diags.err(t.span, "invalid assignment target (expected a variable, `*p`, `p[i]` or `p.field`)");
            return;
        }
        let place = match self.place_or_value(t, None) {
            PV::Place(p) => p,
            PV::Value(v) => {
                if v.ty != Ty::Error {
                    self.diags.err(t.span, "cannot assign to this expression");
                }
                return;
            }
        };
        if !place.ty.is_scalar_storage() {
            if place.ty != Ty::Error {
                let ts = self.ty_str(&place.ty);
                self.diags.err_help(
                    t.span,
                    format!("cannot assign a whole `{ts}`"),
                    "assign its fields or elements one by one, or use `memcpy`",
                );
            }
            return;
        }
        let vty = place.ty.value_ty();
        let mt = place.ty.memty();
        match op {
            None => {
                let v = self.value(value, Some(&vty));
                let x = self.coerce(v, &vty, value.span);
                out.push(hir::Stmt::Store(mt, place.addr, 0, x));
            }
            Some(op) => {
                let rhs = self.value(value, Some(&vty));
                let rty = rhs.ty.clone();
                match self.binop_rule(op, &vty, &rty) {
                    Ok(Rule::Simple(b, ty)) if ty == vty || ty == Ty::Error => {
                        out.push(hir::Stmt::StoreRmw(mt, place.addr, 0, b, rhs.e));
                    }
                    Ok(Rule::PtrAdd(sz, _)) => {
                        out.push(hir::Stmt::StoreRmw(mt, place.addr, 0, Bin::Add, scale(rhs.e, sz)));
                    }
                    Ok(Rule::PtrSub(sz, _)) => {
                        out.push(hir::Stmt::StoreRmw(mt, place.addr, 0, Bin::Sub, scale(rhs.e, sz)));
                    }
                    Ok(_) => {
                        let ts = self.ty_str(&vty);
                        self.diags.err(span, format!("`{}=` would change the type of this `{ts}` place", op.text()));
                    }
                    Err((msg, help)) => self.op_error(span, msg, help),
                }
            }
        }
    }

    fn op_error(&mut self, span: Span, msg: String, help: Option<String>) {
        match help {
            Some(h) => self.diags.err_help(span, msg, h),
            None => self.diags.err(span, msg),
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // Expressions

    fn cond(&mut self, e: &ast::Expr) -> Expr {
        let v = self.value(e, None);
        match v.ty {
            Ty::I64 | Ty::Ptr(_) | Ty::Null | Ty::Error => v.e,
            Ty::F64 => {
                self.diags.err_help(e.span, "conditions must be `i64` or pointers, found `f64`", "compare explicitly, e.g. `x != 0.0`");
                Expr::c(0)
            }
            ref t => {
                let ts = self.ty_str(t);
                self.diags.err(e.span, format!("conditions must be `i64` or pointers, found `{ts}`"));
                Expr::c(0)
            }
        }
    }

    /// Splits a `str`-typed value into (pointer, length).
    fn str_parts(&mut self, v: Typed, span: Span) -> (Expr, Expr) {
        match v.ty {
            Ty::Str => (v.e, v.len.unwrap_or_else(|| Expr::c(0))),
            Ty::Error => (Expr::c(0), Expr::c(0)),
            ref t => {
                let ts = self.ty_str(t);
                self.diags.err_help(
                    span,
                    format!("expected `str`, found `{ts}`"),
                    "a `str` is a string literal, a byte `data` item, a `str` variable or `str(ptr, len)`",
                );
                (Expr::c(0), Expr::c(0))
            }
        }
    }

    fn coerce(&mut self, v: Typed, want: &Ty, span: Span) -> Expr {
        if v.ty == *want {
            return v.e;
        }
        match (&v.ty, want) {
            (Ty::Error, _) | (_, Ty::Error) => return v.e,
            (Ty::Null, Ty::Ptr(_)) => return v.e,
            (Ty::Ptr(_), Ty::Ptr(t)) if **t == Ty::U8 => return v.e,
            _ => {}
        }
        let (ws, fs) = (self.ty_str(want), self.ty_str(&v.ty));
        let help = match (&v.ty, want) {
            (Ty::I64, Ty::F64) if v.lit => Some("write a float literal, e.g. `2.0`".to_string()),
            (Ty::I64, Ty::F64) => Some("convert explicitly: `x as f64`".to_string()),
            (Ty::F64, Ty::I64) => Some("convert explicitly (truncates toward zero): `x as i64`".to_string()),
            (Ty::I64, Ty::Ptr(_)) if v.lit => Some("use `null` for a null pointer".to_string()),
            (Ty::I64, Ty::Ptr(_)) | (Ty::Ptr(_), Ty::Ptr(_)) => Some(format!("cast explicitly: `x as {ws}`")),
            (Ty::Ptr(_), Ty::I64) => Some("cast explicitly: `p as i64`".to_string()),
            (Ty::Str, _) => Some("use `.ptr` or `.len`".to_string()),
            (Ty::Void, _) => Some("this call returns no value".to_string()),
            _ => None,
        };
        let msg = format!("mismatched types: expected `{ws}`, found `{fs}`");
        self.op_error(span, msg, help);
        v.e
    }

    fn intern_str(&mut self, idx: u32) -> u32 {
        let bytes = &self.strings[idx as usize];
        if let Some(&id) = self.str_lits.get(bytes) {
            return id;
        }
        self.statics.push(StaticDecl {
            ty: Ty::U8,
            count: bytes.len() as u64,
            info: StaticInfo { kind: StaticKind::Str, size: bytes.len() as u64, align: 1, init: bytes.clone() },
        });
        let id = self.statics.len() as u32 - 1;
        self.str_lits.insert(bytes.clone(), id);
        id
    }

    fn value(&mut self, e: &ast::Expr, expect: Option<&Ty>) -> Typed {
        match &e.kind {
            ExprKind::Name(_) | ExprKind::Unary(UnOp::Deref, _) | ExprKind::Index(..) | ExprKind::Field(..) => {
                match self.place_or_value(e, expect) {
                    PV::Place(p) => self.load_place(p, e.span),
                    PV::Value(v) => v,
                }
            }
            ExprKind::Paren(inner) => self.value(inner, expect),
            ExprKind::Int(v) => Typed { lit: true, ..Typed::new(Expr::c(*v as i64), Ty::I64) },
            ExprKind::Float(f) => Typed::new(Expr::c(f.to_bits() as i64), Ty::F64),
            ExprKind::Char(c) => Typed::new(Expr::c(*c as i64), Ty::I64),
            ExprKind::Bool(b) => Typed::new(Expr::c(*b as i64), Ty::I64),
            ExprKind::Null => Typed::new(Expr::c(0), Ty::Null),
            ExprKind::Str(i) => {
                let len = self.strings[*i as usize].len() as i64;
                let id = self.intern_str(*i);
                let p = Expr::new(EK::Static(id));
                if expect == Some(&Ty::Str) {
                    Typed { len: Some(Expr::c(len)), ..Typed::new(p, Ty::Str) }
                } else {
                    Typed::new(p, Ty::ptr(Ty::U8))
                }
            }
            ExprKind::Unary(op, a) => {
                // Negative literals stay literals (for error hints and folding).
                if *op == UnOp::Neg {
                    match &a.kind {
                        ExprKind::Int(v) => {
                            return Typed { lit: true, ..Typed::new(Expr::c((*v as i64).wrapping_neg()), Ty::I64) }
                        }
                        ExprKind::Float(f) => return Typed::new(Expr::c((-f).to_bits() as i64), Ty::F64),
                        _ => {}
                    }
                }
                let v = self.value(a, expect);
                match (op, &v.ty) {
                    (_, Ty::Error) => Typed::err(),
                    (UnOp::Neg, Ty::I64) => Typed::new(Expr::bin(Bin::Sub, Expr::c(0), v.e), Ty::I64),
                    (UnOp::Neg, Ty::F64) => Typed::new(Expr::un(Un::FNeg, v.e), Ty::F64),
                    (UnOp::Not, Ty::I64 | Ty::Ptr(_) | Ty::Null) => Typed::new(Expr::un(Un::Eqz, v.e), Ty::I64),
                    (UnOp::BitNot, Ty::I64) => Typed::new(Expr::bin(Bin::Xor, v.e, Expr::c(-1)), Ty::I64),
                    (UnOp::Not, Ty::F64) => {
                        self.diags.err_help(e.span, "`!` needs an `i64` or a pointer, found `f64`", "compare explicitly: `x == 0.0`");
                        Typed::err()
                    }
                    (op, t) => {
                        let ts = self.ty_str(t);
                        let o = match op {
                            UnOp::Neg => "-",
                            UnOp::Not => "!",
                            UnOp::BitNot => "~",
                            UnOp::Deref => "*",
                        };
                        self.diags.err(e.span, format!("cannot apply unary `{o}` to a value of type `{ts}`"));
                        Typed::err()
                    }
                }
            }
            ExprKind::Binary(op, a, b) => {
                if matches!(op, BinOp::AndAnd | BinOp::OrOr) {
                    let x = self.cond(a);
                    let y = self.cond(b);
                    let k = if *op == BinOp::AndAnd { EK::AndAlso(Box::new(x), Box::new(y)) } else { EK::OrElse(Box::new(x), Box::new(y)) };
                    return Typed::new(Expr::new(k), Ty::I64);
                }
                let l = self.value(a, None);
                // Let a literal on the right follow the left operand's type (`x * 2` with `x: f64` gets a hint).
                let hint = if l.ty == Ty::F64 { Some(&Ty::F64) } else { None };
                let r = self.value(b, hint);
                self.binary(*op, l, r, e.span)
            }
            ExprKind::Call { name, name_span, args } => self.call(*name, *name_span, args, e.span),
            ExprKind::Cast(a, t) => {
                let v = self.value(a, None);
                let target = self.resolve_type(t, self.fs.prelude);
                self.cast(v, &target, e.span)
            }
            ExprKind::Sizeof(arg) => {
                let v = self.sizeof(arg, self.fs.prelude, e.span, true).unwrap_or(0);
                Typed::new(Expr::c(v as i64), Ty::I64)
            }
            ExprKind::Offsetof(t, f, fspan) => {
                let v = self.offsetof(t, *f, *fspan, self.fs.prelude).unwrap_or(0);
                Typed::new(Expr::c(v as i64), Ty::I64)
            }
            ExprKind::Countof(a) => {
                let v = self.countof(a, self.fs.prelude).unwrap_or(0);
                Typed::new(Expr::c(v as i64), Ty::I64)
            }
        }
    }

    fn name_value(&mut self, s: Sym, span: Span, expect: Option<&Ty>) -> Typed {
        if let Some(lv) = self.find_local(s) {
            let e = Expr::new(EK::Local(lv.id));
            return if lv.ty == Ty::Str {
                Typed { len: Some(Expr::new(EK::Local(lv.id + 1))), ..Typed::new(e, Ty::Str) }
            } else {
                Typed::new(e, lv.ty.clone())
            };
        }
        match self.lookup(s, self.fs.prelude).map(|i| self.items[i].kind) {
            Some(ItemRef::Const(id)) => match self.const_value(id) {
                Some(CV::I(v)) => Typed::new(Expr::c(v), Ty::I64),
                Some(CV::F(v)) => Typed::new(Expr::c(v.to_bits() as i64), Ty::F64),
                None => Typed::err(),
            },
            Some(ItemRef::Data(id)) => {
                let st = &self.statics[id as usize];
                let p = Expr::new(EK::Static(id));
                if expect == Some(&Ty::Str) && matches!(st.ty, Ty::U8 | Ty::I8) {
                    Typed { len: Some(Expr::c(st.info.size as i64)), ..Typed::new(p, Ty::Str) }
                } else {
                    Typed::new(p, Ty::ptr(st.ty.clone()))
                }
            }
            Some(ItemRef::Buffer(id)) => {
                let st = &self.statics[id as usize];
                Typed::new(Expr::new(EK::Static(id)), Ty::ptr(st.ty.clone()))
            }
            Some(ItemRef::Global(_)) => unreachable!("globals are places"),
            Some(ItemRef::Func(_)) => {
                let name = self.name(s);
                self.diags.err_help(span, format!("`{name}` is a function, not a value"), format!("call it: `{name}(...)`"));
                Typed::err()
            }
            Some(ItemRef::Struct(_)) => {
                let name = self.name(s);
                self.diags.err(span, format!("`{name}` is a struct type, not a value"));
                Typed::err()
            }
            None => {
                if s == self.n.heap_base {
                    return Typed::new(Expr::new(EK::HeapBase), Ty::I64);
                }
                let name = self.name(s);
                let msg = format!("cannot find `{name}` in this scope");
                let cands = self.value_candidates();
                match suggest(name, cands.iter().map(|s| s.as_str())) {
                    Some(c) => self.diags.err_help(span, msg, format!("did you mean `{c}`?")),
                    None => self.diags.err(span, msg),
                }
                Typed::err()
            }
        }
    }

    fn value_candidates(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for sc in &self.fs.scopes {
            for l in sc {
                v.push(self.name(l.name).to_string());
            }
        }
        let scopes: &[&FxMap<Sym, usize>] =
            if self.fs.prelude { &[&self.scopes[0]] } else { &[&self.scopes[1], &self.scopes[0]] };
        for sc in scopes {
            for (k, &i) in sc.iter() {
                if !matches!(self.items[i].kind, ItemRef::Struct(_) | ItemRef::Func(_)) {
                    v.push(self.name(*k).to_string());
                }
            }
        }
        v.sort();
        v
    }

    fn place_or_value(&mut self, e: &ast::Expr, expect: Option<&Ty>) -> PV {
        match &e.kind {
            ExprKind::Paren(inner) => self.place_or_value(inner, expect),
            ExprKind::Name(s) => {
                if self.find_local(*s).is_none() {
                    if let Some(ItemRef::Global(id)) = self.lookup(*s, self.fs.prelude).map(|i| self.items[i].kind) {
                        let ty = self.statics[id as usize].ty.clone();
                        return PV::Place(Place { addr: Expr::new(EK::Static(id)), ty });
                    }
                }
                PV::Value(self.name_value(*s, e.span, expect))
            }
            ExprKind::Unary(UnOp::Deref, p) => {
                let v = self.value(p, None);
                match v.ty {
                    Ty::Ptr(t) => {
                        if matches!(*t, Ty::Struct(_)) {
                            // `*p` of a struct is only useful as `(*p).field`; keep it as a place.
                        }
                        PV::Place(Place { addr: v.e, ty: Rc::unwrap_or_clone(t) })
                    }
                    Ty::Error => PV::Value(Typed::err()),
                    Ty::Null => {
                        self.diags.err(e.span, "dereferencing `null`");
                        PV::Value(Typed::err())
                    }
                    t => {
                        let ts = self.ty_str(&t);
                        let help = if t == Ty::I64 { Some("cast it to a pointer first, e.g. `*(x as *i64)`".to_string()) } else { None };
                        self.op_error(e.span, format!("cannot dereference a value of type `{ts}`"), help);
                        PV::Value(Typed::err())
                    }
                }
            }
            ExprKind::Index(b, i) => {
                let bv = self.value(b, None);
                let iv = self.value(i, Some(&Ty::I64));
                let iv_e = match iv.ty {
                    Ty::I64 | Ty::Error => iv.e,
                    ref t => {
                        let ts = self.ty_str(t);
                        self.diags.err(i.span, format!("indices must be `i64`, found `{ts}`"));
                        Expr::c(0)
                    }
                };
                match bv.ty {
                    Ty::Ptr(t) => {
                        let sz = self.size_of(&t);
                        PV::Place(Place { addr: Expr::bin(Bin::Add, bv.e, scale(iv_e, sz)), ty: Rc::unwrap_or_clone(t) })
                    }
                    Ty::Error => PV::Value(Typed::err()),
                    Ty::Str => {
                        self.diags.err_help(b.span, "a `str` cannot be indexed directly", "index its pointer: `s.ptr[i]`");
                        PV::Value(Typed::err())
                    }
                    t => {
                        let ts = self.ty_str(&t);
                        self.diags.err(b.span, format!("cannot index into a value of type `{ts}` (expected a pointer)"));
                        PV::Value(Typed::err())
                    }
                }
            }
            ExprKind::Field(b, f, fspan) => {
                let base = self.place_or_value(b, None);
                let (addr, sty) = match base {
                    PV::Place(p) => match p.ty {
                        Ty::Struct(id) => (p.addr, id),
                        Ty::Ptr(t) if matches!(*t, Ty::Struct(_)) => {
                            let Ty::Struct(id) = *t else { unreachable!() };
                            (Expr::new(EK::Load(MemTy::I64, Box::new(p.addr), 0)), id)
                        }
                        Ty::Error => return PV::Value(Typed::err()),
                        t => {
                            let ts = self.ty_str(&t);
                            self.diags.err(*fspan, format!("type `{ts}` has no fields"));
                            return PV::Value(Typed::err());
                        }
                    },
                    PV::Value(v) => match v.ty {
                        Ty::Ptr(t) if matches!(*t, Ty::Struct(_)) => {
                            let Ty::Struct(id) = *t else { unreachable!() };
                            (v.e, id)
                        }
                        Ty::Str => {
                            if *f == self.n.ptr {
                                return PV::Value(Typed::new(v.e, Ty::ptr(Ty::U8)));
                            } else if *f == self.n.len {
                                return PV::Value(Typed::new(v.len.unwrap_or_else(|| Expr::c(0)), Ty::I64));
                            }
                            let fname = self.name(*f);
                            self.diags.err_help(*fspan, format!("`str` has no field `{fname}`"), "its fields are `ptr` and `len`");
                            return PV::Value(Typed::err());
                        }
                        Ty::Error => return PV::Value(Typed::err()),
                        t => {
                            let ts = self.ty_str(&t);
                            let help = match &t {
                                Ty::Ptr(i) if matches!(**i, Ty::Ptr(_)) => Some("dereference it first: `(*p).field`".to_string()),
                                _ => None,
                            };
                            self.op_error(*fspan, format!("type `{ts}` has no fields"), help);
                            return PV::Value(Typed::err());
                        }
                    },
                };
                self.struct_layout(sty);
                match self.structs[sty as usize].fields.iter().find(|x| x.0 == *f) {
                    Some((_, fty, off)) => {
                        let (fty, off) = (fty.clone(), *off);
                        let addr = if off == 0 { addr } else { Expr::bin(Bin::Add, addr, Expr::c(off as i64)) };
                        PV::Place(Place { addr, ty: fty })
                    }
                    None => {
                        self.no_field(sty, *f, *fspan);
                        PV::Value(Typed::err())
                    }
                }
            }
            _ => PV::Value(self.value(e, expect)),
        }
    }

    fn load_place(&mut self, p: Place, span: Span) -> Typed {
        match &p.ty {
            Ty::Array(elem, _) => Typed::new(p.addr, Ty::ptr((**elem).clone())),
            Ty::Struct(id) => {
                let name = self.name(self.structs[*id as usize].name);
                self.diags.err_help(
                    span,
                    format!("a `{name}` struct cannot be used as a value"),
                    "access one of its fields, or take its address with pointer arithmetic (`p + i` instead of `p[i]`)",
                );
                Typed::err()
            }
            Ty::Error => Typed::err(),
            t if t.is_scalar_storage() => {
                let mt = t.memty();
                Typed::new(Expr::new(EK::Load(mt, Box::new(p.addr), 0)), t.value_ty())
            }
            t => {
                let ts = self.ty_str(t);
                self.diags.err(span, format!("cannot load a value of type `{ts}`"));
                Typed::err()
            }
        }
    }

    fn cast(&mut self, v: Typed, t: &Ty, span: Span) -> Typed {
        let e = v.e;
        let r = match (&v.ty, t) {
            (Ty::Error, _) | (_, Ty::Error) => return Typed::err(),
            (Ty::I64 | Ty::Ptr(_) | Ty::Null, Ty::I64) => (e, Ty::I64),
            (Ty::I64 | Ty::Ptr(_) | Ty::Null, Ty::Ptr(_)) => (e, t.clone()),
            (Ty::I64, Ty::F64) => (Expr::un(Un::FFromI, e), Ty::F64),
            (Ty::F64, Ty::F64) => (e, Ty::F64),
            (Ty::F64, Ty::I64) => (Expr::un(Un::FToI, e), Ty::I64),
            (Ty::I64 | Ty::Ptr(_) | Ty::F64, Ty::U8 | Ty::I8 | Ty::U16 | Ty::I16 | Ty::U32 | Ty::I32) => {
                let e = if v.ty == Ty::F64 { Expr::un(Un::FToI, e) } else { e };
                let e = match t {
                    Ty::U8 => Expr::bin(Bin::And, e, Expr::c(0xFF)),
                    Ty::U16 => Expr::bin(Bin::And, e, Expr::c(0xFFFF)),
                    Ty::U32 => Expr::bin(Bin::And, e, Expr::c(0xFFFF_FFFF)),
                    Ty::I8 => Expr::un(Un::Ext8S, e),
                    Ty::I16 => Expr::un(Un::Ext16S, e),
                    _ => Expr::un(Un::Ext32S, e),
                };
                (e, Ty::I64)
            }
            (from, to) => {
                let (fs, ts) = (self.ty_str(from), self.ty_str(to));
                let help = match (from, to) {
                    (Ty::Ptr(_), Ty::F64) | (Ty::F64, Ty::Ptr(_)) => Some("go through `i64` explicitly".to_string()),
                    (Ty::Str, _) => Some("use `.ptr` or `.len`".to_string()),
                    (_, Ty::Str) => Some("build a `str` with `str(ptr, len)`".to_string()),
                    _ => None,
                };
                self.op_error(span, format!("cannot cast `{fs}` to `{ts}`"), help);
                return Typed::err();
            }
        };
        Typed::new(r.0, r.1)
    }

    fn binop_rule(&mut self, op: BinOp, lt: &Ty, rt: &Ty) -> Result<Rule, (String, Option<String>)> {
        use BinOp as B;
        let o = op.text();
        if matches!(lt, Ty::Error) || matches!(rt, Ty::Error) {
            return Ok(Rule::Simple(Bin::Add, Ty::Error));
        }
        let int = |b: Bin| Ok(Rule::Simple(b, Ty::I64));
        match (lt, rt) {
            (Ty::I64, Ty::I64) => match op {
                B::Add => int(Bin::Add),
                B::Sub => int(Bin::Sub),
                B::Mul => int(Bin::Mul),
                B::Div => int(Bin::DivS),
                B::Rem => int(Bin::RemS),
                B::And => int(Bin::And),
                B::Or => int(Bin::Or),
                B::Xor => int(Bin::Xor),
                B::Shl => int(Bin::Shl),
                B::Shr => int(Bin::ShrS),
                B::Ushr => int(Bin::ShrU),
                B::Eq => int(Bin::Eq),
                B::Ne => int(Bin::Ne),
                B::Lt => int(Bin::LtS),
                B::Le => int(Bin::LeS),
                B::Gt => int(Bin::GtS),
                B::Ge => int(Bin::GeS),
                B::AndAnd | B::OrOr => unreachable!(),
            },
            (Ty::F64, Ty::F64) => {
                let f = |b: Bin| Ok(Rule::Simple(b, Ty::F64));
                match op {
                    B::Add => f(Bin::FAdd),
                    B::Sub => f(Bin::FSub),
                    B::Mul => f(Bin::FMul),
                    B::Div => f(Bin::FDiv),
                    B::Eq => int(Bin::FEq),
                    B::Ne => int(Bin::FNe),
                    B::Lt => int(Bin::FLt),
                    B::Le => int(Bin::FLe),
                    B::Gt => int(Bin::FGt),
                    B::Ge => int(Bin::FGe),
                    _ => Err((format!("`{o}` is not defined for `f64`"), None)),
                }
            }
            (Ty::Ptr(t), Ty::I64) if matches!(op, B::Add | B::Sub) => {
                let sz = self.size_of(t);
                Ok(if op == B::Add { Rule::PtrAdd(sz, lt.clone()) } else { Rule::PtrSub(sz, lt.clone()) })
            }
            (Ty::I64, Ty::Ptr(t)) if op == B::Add => Ok(Rule::IntPtrAdd(self.size_of(t), rt.clone())),
            (Ty::Ptr(a), Ty::Ptr(b)) if op == B::Sub => {
                if a != b {
                    let (ls, rs) = (self.ty_str(lt), self.ty_str(rt));
                    return Err((format!("cannot subtract pointers of different types `{ls}` and `{rs}`"), None));
                }
                let sz = self.size_of(a);
                if sz == 0 {
                    return Err(("pointer difference of a zero-sized type".to_string(), None));
                }
                Ok(Rule::PtrDiff(sz))
            }
            (Ty::Ptr(_) | Ty::Null, Ty::Ptr(_) | Ty::Null) if op.is_comparison() => {
                let compatible = match (lt, rt) {
                    (Ty::Ptr(a), Ty::Ptr(b)) => a == b || **a == Ty::U8 || **b == Ty::U8,
                    _ => true,
                };
                if !compatible {
                    let (ls, rs) = (self.ty_str(lt), self.ty_str(rt));
                    return Err((
                        format!("cannot compare `{ls}` with `{rs}`"),
                        Some("cast one side so both pointer types match".to_string()),
                    ));
                }
                int(match op {
                    B::Eq => Bin::Eq,
                    B::Ne => Bin::Ne,
                    B::Lt => Bin::LtU,
                    B::Le => Bin::LeU,
                    B::Gt => Bin::GtU,
                    _ => Bin::GeU,
                })
            }
            (Ty::I64, Ty::F64) | (Ty::F64, Ty::I64) => {
                let (ls, rs) = (self.ty_str(lt), self.ty_str(rt));
                Err((
                    format!("mismatched operand types `{ls}` and `{rs}` for `{o}`"),
                    Some("Ten never converts implicitly: use `x as f64` or `y as i64`".to_string()),
                ))
            }
            (Ty::Ptr(_), Ty::I64) | (Ty::I64, Ty::Ptr(_)) if op.is_comparison() => Err((
                format!("cannot compare a pointer with an integer using `{o}`"),
                Some("compare with `null`, or cast: `p as i64`".to_string()),
            )),
            (Ty::Str, _) | (_, Ty::Str) => Err((
                format!("`{o}` cannot be applied to a `str`"),
                Some("use `.ptr` and `.len`, or `mem_eq`/`str_eq` from the prelude to compare contents".to_string()),
            )),
            (Ty::Void, _) | (_, Ty::Void) => Err(("this call returns no value".to_string(), None)),
            _ => {
                let (ls, rs) = (self.ty_str(lt), self.ty_str(rt));
                let help = match (lt, op) {
                    (Ty::Ptr(_), B::Add | B::Sub) => Some("pointer arithmetic takes an `i64` element offset".to_string()),
                    (Ty::Ptr(_), _) => Some("only `+`, `-` and comparisons apply to pointers; cast with `as i64` for bit operations".to_string()),
                    _ => None,
                };
                Err((format!("cannot apply `{o}` to `{ls}` and `{rs}`"), help))
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: Typed, r: Typed, span: Span) -> Typed {
        if l.ty == Ty::F64 && r.ty == Ty::I64 && r.lit {
            self.diags.err_help(
                span,
                format!("mismatched operand types `f64` and `i64` for `{}`", op.text()),
                "write the literal as a float, e.g. `2.0`",
            );
            return Typed::err();
        }
        match self.binop_rule(op, &l.ty, &r.ty) {
            Ok(Rule::Simple(b, ty)) => {
                if ty == Ty::Error {
                    return Typed::err();
                }
                Typed::new(Expr::bin(b, l.e, r.e), ty)
            }
            Ok(Rule::PtrAdd(sz, ty)) => Typed::new(Expr::bin(Bin::Add, l.e, scale(r.e, sz)), ty),
            Ok(Rule::IntPtrAdd(sz, ty)) => Typed::new(Expr::bin(Bin::Add, scale(l.e, sz), r.e), ty),
            Ok(Rule::PtrSub(sz, ty)) => Typed::new(Expr::bin(Bin::Sub, l.e, scale(r.e, sz)), ty),
            Ok(Rule::PtrDiff(sz)) => {
                let d = Expr::bin(Bin::Sub, l.e, r.e);
                let e = if sz.is_power_of_two() {
                    Expr::bin(Bin::ShrS, d, Expr::c(sz.trailing_zeros() as i64))
                } else {
                    Expr::bin(Bin::DivS, d, Expr::c(sz as i64))
                };
                Typed::new(e, Ty::I64)
            }
            Err((msg, help)) => {
                self.op_error(span, msg, help);
                Typed::err()
            }
        }
    }

    fn call(&mut self, name: Sym, name_span: Span, args: &[ast::Expr], span: Span) -> Typed {
        if self.find_local(name).is_some() {
            let n = self.name(name);
            self.diags.err(name_span, format!("`{n}` is a local variable, not a function"));
            return Typed::err();
        }
        match self.lookup(name, self.fs.prelude).map(|i| self.items[i].kind) {
            Some(ItemRef::Func(fid)) => return self.call_fn(fid, args, span),
            Some(_) => {
                let n = self.name(name);
                self.diags.err(name_span, format!("`{n}` is not a function"));
                return Typed::err();
            }
            None => {}
        }
        if let Some(b) = self.builtins.get(&name).copied() {
            return self.builtin(b, args, span);
        }
        let n = self.name(name);
        let mut cands: Vec<&str> = BUILTINS.to_vec();
        let scopes: Vec<&FxMap<Sym, usize>> =
            if self.fs.prelude { vec![&self.scopes[0]] } else { vec![&self.scopes[1], &self.scopes[0]] };
        for sc in scopes {
            for (k, &i) in sc.iter() {
                if matches!(self.items[i].kind, ItemRef::Func(_)) {
                    cands.push(self.name(*k));
                }
            }
        }
        cands.sort();
        let msg = format!("cannot find function `{n}`");
        match suggest(n, cands.into_iter()) {
            Some(c) => self.diags.err_help(name_span, msg, format!("did you mean `{c}`?")),
            None => self.diags.err(name_span, msg),
        }
        for a in args {
            self.value(a, None);
        }
        Typed::err()
    }

    fn signature(&self, fid: u32) -> String {
        let f = &self.funcs[fid as usize];
        let ps: Vec<String> =
            f.params.iter().map(|(n, t, _)| format!("{}: {}", self.name(*n), self.ty_str(t))).collect();
        let r = f.ret.as_ref().map(|t| format!(" -> {}", self.ty_str(t))).unwrap_or_default();
        format!("fn {}({}){}", self.name(f.name), ps.join(", "), r)
    }

    fn call_fn(&mut self, fid: u32, args: &[ast::Expr], span: Span) -> Typed {
        let params = self.funcs[fid as usize].params.clone();
        let ret = self.funcs[fid as usize].ret.clone();
        if args.len() != params.len() {
            let n = self.name(self.funcs[fid as usize].name);
            let sig = self.signature(fid);
            let pl = if params.len() == 1 { "" } else { "s" };
            let al = if args.len() == 1 { " was" } else { "s were" };
            self.diags.err_help(
                span,
                format!("function `{n}` takes {} argument{pl} but {} argument{al} given", params.len(), args.len()),
                format!("the signature is `{sig}`"),
            );
            for a in args {
                self.value(a, None);
            }
            return Typed::new(Expr::c(0), ret.unwrap_or(Ty::Void));
        }
        let mut out = Vec::with_capacity(args.len());
        for (a, (_, pty, _)) in args.iter().zip(params.iter()) {
            let v = self.value(a, Some(pty));
            if *pty == Ty::Str {
                let (p, n) = self.str_parts(v, a.span);
                out.push(p);
                out.push(n);
            } else {
                out.push(self.coerce(v, pty, a.span));
            }
        }
        let f = &self.funcs[fid as usize];
        if f.prelude && !f.checked && f.decl.body.is_some() {
            self.worklist.push(fid);
        }
        Typed::new(Expr::new(EK::Call(fid, out)), ret.unwrap_or(Ty::Void))
    }

    fn builtin(&mut self, b: &'static str, args: &[ast::Expr], span: Span) -> Typed {
        let arity = match b {
            "memsize" | "unreachable" => 0,
            "sqrt" | "fabs" | "clz" | "ctz" | "popcnt" | "to_bits" | "from_bits" => 1,
            "rotl" | "rotr" | "ult" | "ule" | "ugt" | "uge" | "udiv" | "urem" | "str" => 2,
            _ => 3,
        };
        if args.len() != arity {
            let pl = if arity == 1 { "" } else { "s" };
            self.diags.err(span, format!("`{b}` takes {arity} argument{pl} but {} were given", args.len()));
            return Typed::err();
        }
        let mut vals: Vec<Option<(Typed, Span)>> = args
            .iter()
            .map(|a| {
                let hint = match b {
                    "sqrt" | "fabs" | "to_bits" => Some(&Ty::F64),
                    _ => Some(&Ty::I64),
                };
                Some((self.value(a, hint), a.span))
            })
            .collect();
        let mut take = |i: usize| vals[i].take().unwrap();
        let i64t = Ty::I64;
        let f64t = Ty::F64;
        match b {
            "memcpy" | "memset" => {
                let (d, ds) = take(0);
                let d = self.ptrish(d, ds);
                let (s, ss) = take(1);
                let s = if b == "memcpy" { self.ptrish(s, ss) } else { self.coerce(s, &i64t, ss) };
                let (n, ns) = take(2);
                let n = self.coerce(n, &i64t, ns);
                let k = if b == "memcpy" {
                    EK::MemCopy(Box::new(d), Box::new(s), Box::new(n))
                } else {
                    EK::MemFill(Box::new(d), Box::new(s), Box::new(n))
                };
                Typed::new(Expr::new(k), Ty::Void)
            }
            "memsize" => Typed::new(Expr::new(EK::MemSize), Ty::I64),
            "unreachable" => Typed::new(Expr::new(EK::Unreachable), Ty::Void),
            "sqrt" | "fabs" => {
                let (x, xs) = take(0);
                let x = self.coerce(x, &f64t, xs);
                Typed::new(Expr::un(if b == "sqrt" { Un::FSqrt } else { Un::FAbs }, x), Ty::F64)
            }
            "clz" | "ctz" | "popcnt" => {
                let (x, xs) = take(0);
                let x = self.coerce(x, &i64t, xs);
                let op = match b {
                    "clz" => Un::Clz,
                    "ctz" => Un::Ctz,
                    _ => Un::Popcnt,
                };
                Typed::new(Expr::un(op, x), Ty::I64)
            }
            "to_bits" => {
                let (x, xs) = take(0);
                let x = self.coerce(x, &f64t, xs);
                Typed::new(x, Ty::I64)
            }
            "from_bits" => {
                let (x, xs) = take(0);
                let x = self.coerce(x, &i64t, xs);
                Typed::new(x, Ty::F64)
            }
            "rotl" | "rotr" | "udiv" | "urem" => {
                let (x, xs) = take(0);
                let x = self.coerce(x, &i64t, xs);
                let (y, ys) = take(1);
                let y = self.coerce(y, &i64t, ys);
                let op = match b {
                    "rotl" => Bin::Rotl,
                    "rotr" => Bin::Rotr,
                    "udiv" => Bin::DivU,
                    _ => Bin::RemU,
                };
                Typed::new(Expr::bin(op, x, y), Ty::I64)
            }
            "ult" | "ule" | "ugt" | "uge" => {
                let (x, xs) = take(0);
                let (y, ys) = take(1);
                let x = self.ptrish(x, xs);
                let y = self.ptrish(y, ys);
                let op = match b {
                    "ult" => Bin::LtU,
                    "ule" => Bin::LeU,
                    "ugt" => Bin::GtU,
                    _ => Bin::GeU,
                };
                Typed::new(Expr::bin(op, x, y), Ty::I64)
            }
            "str" => {
                let (p, ps) = take(0);
                let p = self.ptrish(p, ps);
                let (n, ns) = take(1);
                let n = self.coerce(n, &i64t, ns);
                Typed { len: Some(n), ..Typed::new(p, Ty::Str) }
            }
            "select" => {
                let (c, cs) = take(0);
                let c = match c.ty {
                    Ty::I64 | Ty::Ptr(_) | Ty::Error => c.e,
                    ref t => {
                        let ts = self.ty_str(t);
                        self.diags.err(cs, format!("the condition of `select` must be `i64` or a pointer, found `{ts}`"));
                        c.e
                    }
                };
                let (a, _) = take(1);
                let (bv, bs) = take(2);
                let ty = a.ty.clone();
                if !matches!(ty, Ty::I64 | Ty::F64 | Ty::Ptr(_) | Ty::Error) {
                    let ts = self.ty_str(&ty);
                    self.diags.err(span, format!("`select` cannot choose between values of type `{ts}`"));
                    return Typed::err();
                }
                let bv = self.coerce(bv, &ty, bs);
                Typed::new(Expr::new(EK::Select(Box::new(c), Box::new(a.e), Box::new(bv))), ty)
            }
            _ => unreachable!("unknown builtin {b}"),
        }
    }
}

impl Checker<'_> {
    fn ptrish(&mut self, v: Typed, sp: Span) -> Expr {
        match v.ty {
            Ty::Ptr(_) | Ty::I64 | Ty::Null | Ty::Error => v.e,
            ref t => {
                let ts = self.ty_str(t);
                self.diags.err(sp, format!("expected a pointer or an `i64` address, found `{ts}`"));
                v.e
            }
        }
    }
}

enum Rule {
    Simple(Bin, Ty),
    PtrAdd(u64, Ty),
    IntPtrAdd(u64, Ty),
    PtrSub(u64, Ty),
    PtrDiff(u64),
}

fn scale(e: Expr, sz: u64) -> Expr {
    if sz == 1 {
        e
    } else {
        Expr::bin(Bin::Mul, e, Expr::c(sz as i64))
    }
}

/// The closest candidate within a small edit distance.
fn suggest<'b>(name: &str, cands: impl Iterator<Item = &'b str>) -> Option<&'b str> {
    let max = name.len().div_ceil(3).clamp(1, 3);
    let mut best: Option<(usize, &str)> = None;
    for c in cands {
        if c == name {
            continue;
        }
        let d = edit_distance(name, c);
        if d <= max && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|b| b.1)
}
