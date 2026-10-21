#!/usr/bin/env bash
# Builds the checker (k10-lang + k10-mesh + k10-ir as wasm32), tests it, and packages the extension (.vsix).
# Needs: rustup target wasm32-unknown-unknown, node >= 18, npx (fetches @vscode/vsce).
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release --manifest-path checker/Cargo.toml --target wasm32-unknown-unknown
mkdir -p dist
cp checker/target/wasm32-unknown-unknown/release/k10_check_wasm.wasm dist/checker.wasm
node test/checker.test.js
npx --yes @vscode/vsce@4.0.0 package --no-dependencies --out "dist/k10s-ten-$(node -p 'require("./package.json").version').vsix"
