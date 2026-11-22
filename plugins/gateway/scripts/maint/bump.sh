#!/usr/bin/env bash
# Usage: bump.sh <worktree> <version>   Pins the worktree to omp <version> (upstream.toml only).
# Exit 75 when the omp <version> release has no assets yet, 76 when its binary cannot be fetched and checked.
set -euo pipefail
wt="$(cd "${1:?usage: bump.sh <worktree> <version>}" && pwd)"
ver="${2:?usage: bump.sh <worktree> <version>}"
MAINT="${MAINT:-$HOME/.local/state/ai-provider-gateway-maint}"

code="$(curl -sIL --max-time 30 -o /dev/null -w '%{http_code}' "https://github.com/can1357/oh-my-pi/releases/download/v$ver/SHA256SUMS.txt" || true)"
case "$code" in
  200) ;;
  404) echo "omp release v$ver not published yet" >&2; exit 75 ;;
  *) echo "release check failed (http $code)" >&2; exit 1 ;;
esac

cd "$wt"
sed -i.bak -E "s/^omp = \"[^\"]*\"/omp = \"$ver\"/" upstream.toml
rm -f upstream.toml.bak
# Proves the asset downloads and matches its checksum.
scripts/fetch-omp.sh "$ver" "$MAINT/omp/$ver/omp" || exit 76
