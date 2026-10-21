// Checks the wasm checker against the repository's Ten sources and manifests: `node test/checker.test.js`.
'use strict';

const assert = require('assert');
const fs = require('fs');
const path = require('path');
const { Checker, readFile, utf16Column, NOT_FOUND } = require('../src/checker');

const repo = path.resolve(__dirname, '../../..');
const checker = new Checker(path.join(__dirname, '../dist/checker.wasm'));
const disk = (p) => readFile(p);
let n = 0;
const test = (name, f) => {
    f();
    n++;
    console.log(`ok - ${name}`);
};

test('repository Ten sources compile', () => {
    for (const dir of ['examples', 'bench/k10s/ten']) {
        for (const f of fs.readdirSync(path.join(repo, dir)).filter((f) => f.endsWith('.ten'))) {
            const p = path.join(repo, dir, f);
            assert.deepStrictEqual(checker.checkTen(fs.readFileSync(p, 'utf8'), p, disk), [], p);
        }
    }
});

test('errors carry position, length and help', () => {
    const src = 'export fn handle() -> i64 {\n    let total = 1;\n    return totl + 1;\n}\n';
    const d = checker.checkTen(src, '/w/demo.ten', disk);
    assert.strictEqual(d.length, 1);
    assert.deepStrictEqual(
        { file: d[0].file, line: d[0].line, col: d[0].col, len: d[0].len },
        { file: '/w/demo.ten', line: 3, col: 12, len: 4 },
    );
    assert.match(d[0].message, /cannot find `totl`/);
    assert.match(d[0].help, /total/);
});

test('syntax errors recover and report several', () => {
    const d = checker.checkTen('fn a( { }\nfn b() -> i64 { return 1 }\n', '', disk);
    assert.ok(d.length >= 2, JSON.stringify(d));
    assert.ok(d.every((x) => x.file === '<input>'));
});

test('includes are read through the host, unsaved text first', () => {
    const files = { '/w/lib/util.ten': 'fn twice(x: i64) -> i64 { return x * 2 + MISSING; }' };
    const read = (p) => files[path.posix.normalize(p)] ?? NOT_FOUND;
    let d = checker.checkTen('include "lib/util.ten";\nexport fn f() -> i64 { return twice(2); }', '/w/main.ten', read);
    assert.strictEqual(d.length, 1);
    assert.strictEqual(d[0].file, '/w/lib/util.ten');
    files['/w/lib/util.ten'] = 'fn twice(x: i64) -> i64 { return x * 2; }';
    d = checker.checkTen('include "lib/util.ten";\nexport fn f() -> i64 { return twice(2); }', '/w/main.ten', read);
    assert.deepStrictEqual(d, []);
    d = checker.checkTen('include "nope.ten";\n', '/w/main.ten', read);
    assert.match(d[0].message, /cannot include `\/w\/nope.ten`: no such file/);
});

test('UTF-16 columns', () => {
    assert.strictEqual(utf16Column('abc', 2), 1);
    assert.strictEqual(utf16Column('𝄞x', 2), 2);
    assert.strictEqual(utf16Column('é', 3), 2);
});

test('manifests', () => {
    const app = checker.checkK10s(fs.readFileSync(path.join(repo, 'examples/app.k10s'), 'utf8'));
    assert.strictEqual(app.services.length, 8);
    assert.deepStrictEqual(app.services[0], { name: 'hello', code: 'hello.ten', line: 4 });
    const bad = checker.checkK10s('service a { code = "a.ten" }\n\nservice b { code = "b.ten" memory = 3KiB }\n');
    assert.deepStrictEqual(bad.error, { line: 3, message: 'memory must be a power of two, got 3072 bytes' });
    const callee = checker.checkK10s('service a { code = "a.ten" calls = ["nope"] }');
    assert.match(callee.error.message, /not declared in this manifest/);
});

test('TIR modules', () => {
    assert.match(checker.checkTir(new Uint8Array([1, 2, 3])), /^not a TIR module/);
});

console.log(`${n} tests passed`);
