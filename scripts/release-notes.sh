#!/usr/bin/env bash
# usage: release-notes.sh TAG [REV] — Markdown notes: commits since the previous stable tag (stable TAG) or any
# release tag (dev TAG) reachable from REV (default HEAD), and the archive list. Needs full history and tags.
set -euo pipefail
cd "$(dirname "$0")/.."
tag=${1:?usage: release-notes.sh TAG [REV]}
rev=$(git rev-parse --verify "${2:-HEAD}^{commit}")
v=${tag#v}
MAX=300

match=(--match 'v[0-9]*')
if [[ $v == *-* ]]; then
    kind="Development build (pre-release) of the \`main\` branch"
else
    kind="Stable release"
    match+=(--exclude 'v*-*') # previous *stable* release only
fi
prev=
if git rev-parse -q --verify "$rev^" >/dev/null; then
    prev=$(git describe --tags --abbrev=0 "${match[@]}" "$rev^" 2>/dev/null || true)
fi
repo=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-bzzdevlabs/k10s}
if [[ -n $prev ]]; then
    range=$prev..$rev
    since=" since [\`$prev\`]($repo/releases/tag/$prev) ([diff]($repo/compare/$prev...$tag))"
else
    range=$rev
    since=" (first release)"
fi
n=$(git rev-list --count "$range")

echo "**k10s $v** — $kind, built from commit \`$(git rev-parse --short=7 "$rev")\`."
echo
echo "### Changes$since: $n commit$( ((n == 1)) || echo s)"
echo
git log --no-merges --format='- %h %s' -n "$MAX" "$range"
((n <= MAX)) || echo "- … and $((n - MAX)) more"
cat <<EOF

### Downloads
| Archive | Contents |
|---|---|
| \`k10s-$v-x86_64-unknown-linux-musl.tar.gz\` | \`k10s\` CLI + node daemon, static (any x86-64 Linux) |
| \`k10s-$v-x86_64-unknown-linux-gnu.tar.gz\` | \`k10s\` linked against glibc |
| \`k10s-examples-$v.tar.gz\` | example services: Ten sources, compiled \`.k10\` modules, \`app.k10s\` |
| \`k10s-metal-$v-x86_64.tar.gz\` | bare-metal kernel (\`k10-metal.elf\`, \`k10-metal-embedded.elf\`), \`bundle.k10b\`, \`mkbundle\` |
| \`k10s-bench-$v-linux-amd64.tar.gz\` | benchmark workloads and \`loadgen\` (static Go) |
| \`k10s-$v-src.tar.gz\` | sources of this commit, version-stamped |

Check downloads with \`sha256sum -c SHA256SUMS --ignore-missing\`. Every binary reports \`$v+<commit>\`
(\`k10s --version\`, \`mkbundle --version\`, \`loadgen -version\`, \`GET /_k10s/status\`).
EOF
