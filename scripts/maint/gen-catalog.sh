#!/usr/bin/env bash
# Usage: gen-catalog.sh [--check] [<omp version>]
# Regenerates crates/mu-provider/src/model/catalog.json (mu's snapshot of omp's model catalog for the providers it
# drives natively) from the omp release the repo pins (upstream.toml), or <omp version>.
# With --check, fails when the committed file differs from a fresh run.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
check=0
if [ "${1:-}" = "--check" ]; then check=1; shift; fi
ver="${1:-$(sed -n 's/^omp = "\(.*\)"/\1/p' "$root/upstream.toml")}"
: "${OMPSRC:=${MAINT:-$HOME/.mu/maint}/oh-my-pi}"
js="$(command -v node || command -v bun)" || { echo "gen-catalog: need node or bun" >&2; exit 1; }
if ! git -C "$OMPSRC" rev-parse -q --verify "refs/tags/v$ver" >/dev/null; then
  git -C "$OMPSRC" fetch -q --tags origin
fi
out="$root/crates/mu-provider/src/model/catalog.json"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
git -C "$OMPSRC" show "v$ver:packages/catalog/src/models.json" | "$js" "$root/scripts/maint/gen-catalog.mjs" > "$tmp"
[ -s "$tmp" ] || { echo "gen-catalog: generated an empty catalog" >&2; exit 1; }
if [ "$check" = 1 ]; then
  cmp -s "$tmp" "$out" || {
    echo "crates/mu-provider/src/model/catalog.json is stale for omp $ver; run scripts/maint/gen-catalog.sh" >&2
    exit 1
  }
else
  mv "$tmp" "$out"
  trap - EXIT
fi
