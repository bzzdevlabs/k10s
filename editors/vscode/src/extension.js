// @ts-check
'use strict';

const vscode = require('vscode');
const fs = require('fs');
const path = require('path');
const { Checker, readFile, utf16Column, NOT_FOUND } = require('./checker');
const material = require('./material');

/** Delay between the last edit and a check. */
const DELAY_MS = 250;

/** @typedef {import('./checker').TenDiagnostic} TenDiagnostic */

/**
 * @typedef {object} Result
 * @property {Set<string>} deps  URIs of the files the check read or tried to read
 * @property {Map<string, vscode.Diagnostic[]>} diags  diagnostics per file URI
 */

/** @param {vscode.ExtensionContext} context */
function activate(context) {
    material.setup(context);
    /** @type {Checker | undefined} */
    let instance;
    // Compiled on first use: the extension also activates at startup (folder icons) without Ten files open.
    const checker = () => (instance ??= new Checker(context.asAbsolutePath('dist/checker.wasm')));
    const collections = {
        ten: vscode.languages.createDiagnosticCollection('ten'),
        k10s: vscode.languages.createDiagnosticCollection('k10s'),
    };
    /** Latest result per checked document (URI string), per language. */
    const results = { ten: new Map(), k10s: new Map() };
    /** @type {Map<string, NodeJS.Timeout>} */
    const timers = new Map();

    /** @param {string} key */
    const openDocument = (key) => vscode.workspace.textDocuments.find((d) => d.uri.toString() === key);

    /** @param {string} posixPath */
    const fileUri = (posixPath) => vscode.Uri.from({ scheme: 'file', path: path.posix.normalize(posixPath) });

    /** Language of a document this extension checks (on disk or untitled; not diff or VCS views). */
    /** @param {vscode.TextDocument} doc @returns {'ten' | 'k10s' | undefined} */
    const languageOf = (doc) =>
        (doc.languageId === 'ten' || doc.languageId === 'k10s') && (doc.uri.scheme === 'file' || doc.uri.scheme === 'untitled')
            ? doc.languageId
            : undefined;

    /** @param {vscode.TextDocument} doc @param {'ten' | 'k10s'} lang */
    const enabled = (doc, lang) => vscode.workspace.getConfiguration(lang, doc).get('validate', true);

    /**
     * Reads a file for the checker, preferring the unsaved text of an open editor; records it in `deps` and its
     * text in `texts`.
     * @param {Set<string>} deps @param {Map<string, string>} texts
     * @returns {import('./checker').Reader}
     */
    const reader = (deps, texts) => (posixPath) => {
        if (!path.posix.isAbsolute(posixPath)) {
            return NOT_FOUND;
        }
        const uri = fileUri(posixPath);
        const key = uri.toString();
        deps.add(key);
        const open = openDocument(key);
        const text = open ? open.getText() : readFile(uri.fsPath);
        if (typeof text === 'string') {
            texts.set(key, text);
        }
        return text;
    };

    /**
     * Converts a compiler diagnostic into an editor range on `text` (1-based line, character columns).
     * @param {string | undefined} text @param {TenDiagnostic} d
     */
    const tenRange = (text, d) => {
        const line = Math.max(0, d.line - 1);
        const lineText = text?.split('\n')[line]?.replace(/\r$/, '') ?? '';
        const start = utf16Column(lineText, d.col);
        return new vscode.Range(line, start, line, Math.max(start + 1, utf16Column(lineText, d.col + d.len)));
    };

    /** @param {vscode.Range} range @param {string} message @param {string} source */
    const error = (range, message, source) => {
        const diag = new vscode.Diagnostic(range, message, vscode.DiagnosticSeverity.Error);
        diag.source = source;
        return diag;
    };

    /** @param {vscode.TextDocument} doc @returns {Result} */
    const checkTen = (doc) => {
        const key = doc.uri.toString();
        const file = doc.uri.scheme === 'file' ? doc.uri.path : '';
        const deps = new Set([key]);
        const texts = new Map([[key, doc.getText()]]);
        /** @type {TenDiagnostic[]} */
        let list;
        try {
            list = checker().checkTen(doc.getText(), file, reader(deps, texts));
        } catch (err) {
            list = [{ file, line: 1, col: 1, len: 1, message: `internal checker error: ${err}`, help: null }];
        }
        /** @type {Map<string, vscode.Diagnostic[]>} */
        const diags = new Map();
        for (const d of list) {
            let target = key;
            let message = d.message + (d.help ? `\nhelp: ${d.help}` : '');
            let range;
            if (d.file !== file && d.file !== '<input>' && path.posix.isAbsolute(d.file)) {
                target = fileUri(d.file).toString();
                range = tenRange(texts.get(target), d);
            } else if (d.file === file || d.file === '<input>') {
                range = tenRange(texts.get(key), d);
            } else {
                message = `${d.file}:${d.line}:${d.col}: ${message}`;
                range = new vscode.Range(0, 0, 0, 0);
            }
            const out = diags.get(target) ?? [];
            out.push(error(range, message, 'ten'));
            diags.set(target, out);
        }
        return { deps, diags };
    };

    /** @param {vscode.TextDocument} doc @returns {Result} */
    const checkK10s = (doc) => {
        const key = doc.uri.toString();
        const deps = new Set([key]);
        /** @type {vscode.Diagnostic[]} */
        const list = [];
        /** @param {number} line 1-based */
        const lineRange = (line) => {
            const l = doc.lineAt(Math.min(Math.max(0, line - 1), doc.lineCount - 1));
            return new vscode.Range(l.lineNumber, l.firstNonWhitespaceCharacterIndex, l.lineNumber, l.text.length);
        };
        let res;
        try {
            res = checker().checkK10s(doc.getText());
        } catch (err) {
            res = { error: { line: 1, message: `internal checker error: ${err}` } };
        }
        if (res.error) {
            list.push(error(lineRange(res.error.line), res.error.message, 'k10s'));
        }
        if (doc.uri.scheme === 'file') {
            const base = path.posix.dirname(doc.uri.path);
            for (const svc of res.services ?? []) {
                const range = codeRange(doc, svc.line, svc.code) ?? lineRange(svc.line);
                const problem = checkCode(path.posix.resolve(base, svc.code), deps);
                if (problem) {
                    list.push(error(range, `service \`${svc.name}\`: ${problem}`, 'k10s'));
                }
            }
        }
        return { deps, diags: new Map([[key, list]]) };
    };

    /**
     * What `k10s apply` would reject in a service's code file, or undefined.
     * @param {string} posixPath @param {Set<string>} deps
     */
    const checkCode = (posixPath, deps) => {
        const uri = fileUri(posixPath);
        const key = uri.toString();
        deps.add(key);
        const shown = vscode.workspace.asRelativePath(uri);
        if (posixPath.endsWith('.ten')) {
            const texts = new Map();
            const text = reader(deps, texts)(posixPath);
            if (typeof text !== 'string') {
                return `cannot read \`${shown}\` (\`code\` paths are relative to the manifest)`;
            }
            let list;
            try {
                list = checker().checkTen(text, uri.path, reader(deps, texts));
            } catch (err) {
                return `internal checker error on \`${shown}\`: ${err}`;
            }
            if (list.length === 0) {
                return undefined;
            }
            const d = list[0];
            const where = `${vscode.workspace.asRelativePath(fileUri(d.file))}:${d.line}:${d.col}`;
            const more = list.length > 1 ? ` (and ${list.length - 1} more error${list.length > 2 ? 's' : ''})` : '';
            return `\`${shown}\` does not compile: ${d.message} (${where})${more}`;
        }
        let bytes;
        try {
            bytes = fs.readFileSync(uri.fsPath);
        } catch {
            return `cannot read \`${shown}\` (\`code\` paths are relative to the manifest)`;
        }
        try {
            const err = checker().checkTir(bytes);
            return err ? `\`${shown}\`: ${err}` : undefined;
        } catch (err) {
            return `internal checker error on \`${shown}\`: ${err}`;
        }
    };

    /** Publishes the merged results of one language. @param {'ten' | 'k10s'} lang */
    const publish = (lang) => {
        /** @type {Map<string, Result>} */
        const byDoc = results[lang];
        /** @type {Map<string, Map<string, vscode.Diagnostic>>} */
        const merged = new Map();
        for (const [key, r] of byDoc) {
            // A file included by another open file is checked as part of that program (it may use its names).
            if (lang === 'ten' && [...byDoc].some(([k, o]) => k !== key && o.deps.has(key) && !r.deps.has(k))) {
                continue;
            }
            for (const [file, list] of r.diags) {
                const out = merged.get(file) ?? new Map();
                for (const d of list) {
                    out.set(`${d.range.start.line}:${d.range.start.character}:${d.range.end.character}:${d.message}`, d);
                }
                merged.set(file, out);
            }
        }
        collections[lang].clear();
        for (const [file, list] of merged) {
            collections[lang].set(vscode.Uri.parse(file), [...list.values()]);
        }
    };

    /** @param {vscode.TextDocument} doc */
    const check = (doc) => {
        const lang = languageOf(doc);
        if (!lang || doc.isClosed) {
            return;
        }
        const key = doc.uri.toString();
        if (enabled(doc, lang)) {
            results[lang].set(key, lang === 'ten' ? checkTen(doc) : checkK10s(doc));
        } else {
            results[lang].delete(key);
        }
        publish(lang);
    };

    /** @param {vscode.TextDocument} doc @param {number} delay */
    const schedule = (doc, delay) => {
        const key = doc.uri.toString();
        clearTimeout(timers.get(key));
        timers.set(key, setTimeout(() => {
            timers.delete(key);
            check(doc);
        }, delay));
    };

    /** Re-checks every open document that depends on `uri` (but not `uri` itself). @param {vscode.Uri} uri */
    const dependentsChanged = (uri) => {
        const key = uri.toString();
        for (const doc of vscode.workspace.textDocuments) {
            const lang = languageOf(doc);
            const k = doc.uri.toString();
            if (lang && k !== key && results[lang].get(k)?.deps.has(key)) {
                schedule(doc, DELAY_MS);
            }
        }
    };

    const watcher = vscode.workspace.createFileSystemWatcher('**/*.{ten,k10}');
    context.subscriptions.push(
        collections.ten,
        collections.k10s,
        watcher,
        watcher.onDidChange(dependentsChanged),
        watcher.onDidCreate(dependentsChanged),
        watcher.onDidDelete(dependentsChanged),
        vscode.workspace.onDidOpenTextDocument((doc) => {
            schedule(doc, 0);
            dependentsChanged(doc.uri);
        }),
        vscode.workspace.onDidChangeTextDocument((e) => {
            if (e.contentChanges.length > 0) {
                schedule(e.document, DELAY_MS);
                dependentsChanged(e.document.uri);
            }
        }),
        vscode.workspace.onDidCloseTextDocument((doc) => {
            const lang = languageOf(doc);
            const key = doc.uri.toString();
            clearTimeout(timers.get(key));
            timers.delete(key);
            if (lang && results[lang].delete(key)) {
                publish(lang);
            }
            // Its dependents now read the file from disk.
            dependentsChanged(doc.uri);
        }),
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration('ten.validate') || e.affectsConfiguration('k10s.validate')) {
                vscode.workspace.textDocuments.forEach((doc) => schedule(doc, 0));
            }
        }),
        { dispose: () => timers.forEach((t) => clearTimeout(t)) },
    );
    vscode.workspace.textDocuments.forEach((doc) => schedule(doc, 0));
}

/**
 * Range of the `code = "…"` string of the service declared on `line` (1-based), if found.
 * @param {vscode.TextDocument} doc @param {number} line @param {string} code
 */
function codeRange(doc, line, code) {
    const re = /\bcode\s*=\s*("(?:[^"\\\n]|\\.)*")/g;
    for (let l = Math.max(0, line - 1); l < doc.lineCount; l++) {
        const text = doc.lineAt(l).text;
        if (l >= line && /^\s*service\b/.test(text)) {
            break;
        }
        for (const m of text.matchAll(re)) {
            const value = m[1];
            if (value.slice(1, -1).replace(/\\(.)/g, '$1') === code) {
                const start = (m.index ?? 0) + m[0].length - value.length;
                return new vscode.Range(l, start, l, start + value.length);
            }
        }
    }
    return undefined;
}

function deactivate() {}

module.exports = { activate, deactivate };
