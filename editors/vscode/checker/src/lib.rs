//! Ten (k10-lang), `.k10s` manifest (k10-mesh) and TIR module (k10-ir) checks for the VS Code extension, as a wasm32 module.
//!
//! Protocol: the host writes inputs into [`input`]'s buffer, calls a `check_*` export, and reads the JSON result
//! (`output_len()` bytes at `output_ptr()`). Included Ten files are fetched from the host: `host_read(path, len)`
//! stages the file and returns its byte length (or a negative error), `host_take(dst)` copies the staged bytes.

use std::cell::RefCell;
use std::fmt::Write;
use std::path::Path;

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<String> = const { RefCell::new(String::new()) };
}

#[link(wasm_import_module = "host")]
extern "C" {
    fn host_read(path: *const u8, len: usize) -> i32;
    fn host_take(dst: *mut u8);
}

/// Error codes of `host_read`.
const NOT_FOUND: i32 = -1;
const NOT_UTF8: i32 = -2;

fn read(path: &Path) -> std::io::Result<String> {
    let p = path.to_string_lossy();
    // SAFETY: the host reads `p.len()` bytes at `p.as_ptr()` and writes exactly the length it returned into `buf`.
    let n = unsafe { host_read(p.as_ptr(), p.len()) };
    match n {
        NOT_FOUND => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such file")),
        NOT_UTF8 => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8")),
        n if n < 0 => Err(std::io::Error::other("cannot read file")),
        n => {
            let mut buf = vec![0u8; n as usize];
            unsafe { host_take(buf.as_mut_ptr()) };
            String::from_utf8(buf).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8"))
        }
    }
}

/// Resizes the input buffer to `len` bytes and returns its address.
#[no_mangle]
pub extern "C" fn input(len: usize) -> *mut u8 {
    INPUT.with_borrow_mut(|b| {
        b.clear();
        b.resize(len, 0);
        b.as_mut_ptr()
    })
}

#[no_mangle]
pub extern "C" fn output_ptr() -> *const u8 {
    OUTPUT.with_borrow(|o| o.as_ptr())
}

#[no_mangle]
pub extern "C" fn output_len() -> usize {
    OUTPUT.with_borrow(|o| o.len())
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn take_input() -> String {
    INPUT.with_borrow_mut(|b| String::from_utf8_lossy(&std::mem::take(b)).into_owned())
}

fn set_output(s: String) -> usize {
    let n = s.len();
    OUTPUT.set(s);
    n
}

/// Compiles Ten source (the first `src_len` input bytes; the rest is its absolute path). Result:
/// `[{"file","line","col","len","message","help"}]` (1-based line/col, `col`/`len` in characters).
#[no_mangle]
pub extern "C" fn check_ten(src_len: usize) -> usize {
    let input = take_input();
    let (src, path) = input.split_at(src_len.min(input.len()));
    let path = (!path.is_empty()).then(|| Path::new(path));
    let diags = match k10_lang::compile_with_reader(src, path, &mut k10_lang::Sources::new(), &mut read) {
        Ok(_) => Vec::new(),
        Err(d) => d,
    };
    let mut out = String::from("[");
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"file\":");
        json_str(&mut out, &d.file);
        let _ = write!(out, ",\"line\":{},\"col\":{},\"len\":{},\"message\":", d.line, d.col, d.len);
        json_str(&mut out, &d.message);
        out.push_str(",\"help\":");
        match &d.help {
            Some(h) => json_str(&mut out, h),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push(']');
    set_output(out)
}

/// Parses a `.k10s` manifest (the whole input). Result: `{"error":{"line","message"}}` or
/// `{"services":[{"name","code","line"}]}`.
#[no_mangle]
pub extern "C" fn check_k10s() -> usize {
    let src = take_input();
    let mut out = String::new();
    match k10_mesh::manifest::parse_text(&src) {
        Err(e) => {
            let _ = write!(out, "{{\"error\":{{\"line\":{},\"message\":", e.line);
            json_str(&mut out, &e.msg);
            out.push_str("}}");
        }
        Ok(decls) => {
            out.push_str("{\"services\":[");
            for (i, d) in decls.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                json_str(&mut out, &d.name);
                out.push_str(",\"code\":");
                json_str(&mut out, &d.code);
                let _ = write!(out, ",\"line\":{}}}", d.line);
            }
            out.push_str("]}");
        }
    }
    set_output(out)
}

/// Decodes and validates a precompiled TIR module (the whole input, raw bytes). Result: a JSON string, empty if
/// the module is valid.
#[no_mangle]
pub extern "C" fn check_tir() -> usize {
    let bytes = INPUT.with_borrow_mut(std::mem::take);
    let err = match k10_ir::decode(&bytes) {
        Err(e) => format!("not a TIR module: {e}"),
        Ok(m) => match k10_ir::validate(&m) {
            Err(e) => format!("invalid module: {e}"),
            Ok(_) => String::new(),
        },
    };
    let mut out = String::new();
    json_str(&mut out, &err);
    set_output(out)
}
