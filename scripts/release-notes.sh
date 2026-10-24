#!/usr/bin/env bash
# usage: release-notes.sh TAG [REV] — Markdown notes for TAG built from REV (default HEAD): commits since the previous
# release (nearest ancestor among RELEASE_TAGS, default all tags; stable ones only for a stable TAG), downloads.
set -euo pipefail
cd "$(dirname "$0")/.."
tag=${1:?usage: release-notes.sh TAG [REV]}
rev=$(git rev-parse --verify "${2:-HEAD}^{commit}")
v=${tag#v}
MAX=300

stable='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
dev='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-dev\.[0-9]+$'
if [[ $v == *-* ]]; then
    kind="Development build (pre-release) of the \`main\` branch"
else
    kind="Stable release"
fi
# Stray tags (no release, or not X.Y.Z[-dev.N]) are ignored when RELEASE_TAGS lists the published releases.
prev=
best=-1
while read -r t; do
    [[ $t =~ $stable || ($v == *-* && $t =~ $dev) ]] || continue
    c=$(git rev-parse -q --verify "refs/tags/$t^{commit}") || continue
    [[ $c != "$rev" ]] && git merge-base --is-ancestor "$c" "$rev" || continue
    d=$(git rev-list --count "$c")
    if ((d > best)); then
        best=$d
        prev=$t
    fi
done <<<"${RELEASE_TAGS-$(git tag -l 'v*')}"
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
| \`k10s-$v-x86_64-unknown-linux-gnu.tar.gz\` | \`k10s\` linked against glibc (>= 2.34) |
| \`k10s-$v-aarch64-unknown-linux-musl.tar.gz\` | \`k10s\` for arm64 Linux, static (services on the interpreter) |
| \`k10s-examples-$v.tar.gz\` | example services: Ten sources, compiled \`.k10\` modules, \`app.k10s\` |
| \`k10s-metal-$v-x86_64.tar.gz\` | bare-metal kernel (\`k10-metal.elf\`, \`k10-metal-embedded.elf\`), \`bundle.k10b\`, \`mkbundle\` |
| \`k10s-os-$v-x86_64-vm.img.xz\` | disk image, BIOS + UEFI: \`k10s\` as PID 1 of a Linux kernel, for VMs and clouds |
| \`k10s-os-$v-x86_64-metal.img.xz\` | the same for PCs and servers: signed driver modules, NIC firmware (own licences) |
| \`k10s-os-$v-x86_64-{vm,metal}.efi\` | the UKIs of these images, signed by the k10s release key: OS updates (\`os\` in \`k10s apply\`), PXE, other UEFI loaders |
| \`k10s-os-$v-aarch64-vm.img.xz\`, \`.efi\` | arm64 disk image (UEFI) for VMs and clouds, and its signed UKI |
| \`k10s-os-$v-aarch64-pi.img.xz\`, \`.fat\` | Raspberry Pi 4/5 disk image (Pi firmware under its own licence), and its signed boot slot for OS updates |
| \`k10s-bench-$v-linux-amd64.tar.gz\` | benchmark workloads and \`loadgen\` (static Go) |
| \`k10s-$v-src.tar.gz\` | sources of this commit, version-stamped |

Check downloads with \`sha256sum -c SHA256SUMS --ignore-missing\` and their build provenance with
\`gh attestation verify FILE --repo ${GITHUB_REPOSITORY:-bzzdevlabs/k10s}\`. Every binary reports \`$v+<commit>\`
(\`k10s --version\`, \`mkbundle --version\`, \`loadgen -version\`, \`GET /_k10s/status\`).
EOF
