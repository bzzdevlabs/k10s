#!/usr/bin/env bash
# usage: prune-dev.sh KEEP releases | KEEP images PACKAGE... — keeps the KEEP newest (by N) vX.Y.Z-dev.N pre-releases
# (older ones are deleted with their tags) or GHCR versions tagged only X.Y.Z-dev.N (and `dev`). env: GITHUB_REPOSITORY.
set -euo pipefail
usage="usage: prune-dev.sh KEEP releases | KEEP images PACKAGE..."
keep=${1:?$usage}
what=${2:?$usage}
shift 2
repo=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY=owner/repo}
dev='^v?(0|[1-9][0-9]*)[.](0|[1-9][0-9]*)[.](0|[1-9][0-9]*)-dev[.]([0-9]+)$'

# "N ID" lines -> the IDs beyond the KEEP highest N.
beyond_keep() { sort -k1,1nr | tail -n +$((keep + 1)) | cut -d' ' -f2; }

case $what in
releases)
    gh release list --repo "$repo" --limit 1000 --json tagName,isPrerelease --jq '.[] | select(.isPrerelease) | .tagName' |
        while read -r t; do
            if [[ $t =~ $dev ]]; then echo "${BASH_REMATCH[4]} $t"; fi
        done | beyond_keep |
        while read -r t; do
            echo "deleting pre-release $t"
            gh release delete "$t" --repo "$repo" --cleanup-tag --yes
        done
    ;;
images)
    owner=${repo%%/*}
    scope=$(gh api "users/$owner" --jq 'if .type == "Organization" then "orgs" else "users" end')
    for pkg; do
        gh api --paginate "$scope/$owner/packages/container/$pkg/versions" \
            --jq '.[] | [.id, (.metadata.container.tags | join(" "))] | @tsv' |
            while IFS=$'\t' read -r id tags; do
                n=-1
                for t in $tags; do
                    if [[ $t =~ $dev ]]; then
                        if ((BASH_REMATCH[4] > n)); then n=${BASH_REMATCH[4]}; fi
                    elif [[ $t != dev ]]; then
                        n=-1 # also carries a stable tag: keep
                        break
                    fi
                done
                if ((n >= 0)); then echo "$n $id"; fi
            done | beyond_keep |
            while read -r id; do
                echo "deleting $pkg version $id"
                gh api -X DELETE "$scope/$owner/packages/container/$pkg/versions/$id" >/dev/null
            done
    done
    ;;
*)
    echo "$usage" >&2
    exit 2
    ;;
esac
