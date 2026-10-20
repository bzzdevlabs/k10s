// @ts-check
'use strict';

const fs = require('fs');

/** `host_read` error codes (see checker/src/lib.rs). */
const NOT_FOUND = -1;
const NOT_UTF8 = -2;
const READ_ERROR = -3;

const utf8 = new TextDecoder('utf-8', { fatal: true });
const encoder = new TextEncoder();

/**
 * @typedef {{ file: string, line: number, col: number, len: number, message: string, help: string | null }} TenDiagnostic
 * @typedef {{ error?: { line: number, message: string }, services?: { name: string, code: string, line: number }[] }} K10sResult
 * @typedef {(posixPath: string) => string | number} Reader  returns the text, or NOT_FOUND / NOT_UTF8 / READ_ERROR
 */

/** The k10-lang compiler and the k10-mesh manifest parser, compiled to WebAssembly (checker/). */
class Checker {
    /** @param {string} wasmPath */
    constructor(wasmPath) {
        this.module = new WebAssembly.Module(fs.readFileSync(wasmPath));
        /** @type {WebAssembly.Instance | undefined} */
        this.instance = undefined;
        /** @type {Reader} */
        this.read = () => NOT_FOUND;
        /** @type {Uint8Array | undefined} */
        this.staged = undefined;
    }

    exports() {
        if (!this.instance) {
            this.instance = new WebAssembly.Instance(this.module, {
                host: {
                    host_read: (/** @type {number} */ ptr, /** @type {number} */ len) => this.hostRead(ptr, len),
                    host_take: (/** @type {number} */ dst) => this.hostTake(dst),
                },
            });
        }
        return /** @type {any} */ (this.instance.exports);
    }

    /** @param {number} ptr @param {number} len */
    hostRead(ptr, len) {
        const mem = new Uint8Array(this.exports().memory.buffer, ptr, len);
        const p = utf8.decode(mem.slice());
        const r = this.read(p);
        if (typeof r === 'number') {
            return r;
        }
        this.staged = encoder.encode(r);
        return this.staged.length;
    }

    /** @param {number} dst */
    hostTake(dst) {
        const s = this.staged ?? new Uint8Array();
        new Uint8Array(this.exports().memory.buffer, dst, s.length).set(s);
        this.staged = undefined;
    }

    /**
     * Writes `bytes` into the module's input buffer, runs `run`, and parses the JSON output. A trap (a compiler
     * panic) discards the instance so the next call starts fresh.
     * @param {Uint8Array} bytes @param {(e: any) => number} run
     */
    call(bytes, run) {
        try {
            const e = this.exports();
            // `input` may grow the memory (detaching earlier views): take the view after it.
            const ptr = e.input(bytes.length);
            new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
            const n = run(e);
            const out = e.output_ptr();
            return JSON.parse(new TextDecoder().decode(new Uint8Array(e.memory.buffer, out, n)));
        } catch (err) {
            this.instance = undefined;
            throw err;
        } finally {
            this.staged = undefined;
        }
    }

    /**
     * Compiles Ten source. `file` is the POSIX-style absolute path of the source (base of `include`), or '' if it
     * has none; included files are read with `read`.
     * @param {string} src @param {string} file @param {Reader} read
     * @returns {TenDiagnostic[]}
     */
    checkTen(src, file, read) {
        const s = encoder.encode(src);
        const p = encoder.encode(file);
        const bytes = new Uint8Array(s.length + p.length);
        bytes.set(s);
        bytes.set(p, s.length);
        this.read = read;
        try {
            return this.call(bytes, (e) => e.check_ten(s.length));
        } finally {
            this.read = () => NOT_FOUND;
        }
    }

    /**
     * Decodes and validates a precompiled TIR module (`.k10`); returns the error, or '' if it is valid.
     * @param {Uint8Array} bytes
     * @returns {string}
     */
    checkTir(bytes) {
        return this.call(bytes, (e) => e.check_tir());
    }

    /**
     * Parses a `.k10s` manifest.
     * @param {string} src
     * @returns {K10sResult}
     */
    checkK10s(src) {
        return this.call(encoder.encode(src), (e) => e.check_k10s());
    }
}

/**
 * Reads a file from disk for the checker.
 * @param {string} osPath
 * @returns {string | number}
 */
function readFile(osPath) {
    let bytes;
    try {
        bytes = fs.readFileSync(osPath);
    } catch (err) {
        const code = /** @type {NodeJS.ErrnoException} */ (err).code;
        return code === 'ENOENT' || code === 'ENOTDIR' ? NOT_FOUND : READ_ERROR;
    }
    try {
        return utf8.decode(bytes);
    } catch {
        return NOT_UTF8;
    }
}

/**
 * 0-based UTF-16 column of the 1-based character (code point) column `col` on `line`.
 * @param {string} line @param {number} col
 */
function utf16Column(line, col) {
    let units = 0;
    let chars = 1;
    for (const ch of line) {
        if (chars >= col) {
            break;
        }
        units += ch.length;
        chars++;
    }
    return units + Math.max(0, col - chars);
}

module.exports = { Checker, readFile, utf16Column, NOT_FOUND, NOT_UTF8, READ_ERROR };
