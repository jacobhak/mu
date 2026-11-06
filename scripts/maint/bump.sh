#!/usr/bin/env bash
# Usage: bump.sh <worktree> <version>   Pins every omp artifact in the worktree to <version>.
# Exit 75 when the omp <version> release has no assets yet.
set -euo pipefail
wt="$(cd "${1:?usage: bump.sh <worktree> <version>}" && pwd)"
ver="${2:?usage: bump.sh <worktree> <version>}"

code="$(curl -sIL --max-time 30 -o /dev/null -w '%{http_code}' "https://github.com/can1357/oh-my-pi/releases/download/v$ver/SHA256SUMS.txt" || true)"
case "$code" in
  200) ;;
  404) echo "omp release v$ver not published yet" >&2; exit 75 ;;
  *) echo "release check failed (http $code)" >&2; exit 1 ;;
esac

cd "$wt"
sed -i -E "s/^omp = \"[^\"]*\"/omp = \"$ver\"/" upstream.toml
sed -i -E "/can1357\/oh-my-pi/ s/(rev|tag) = \"[^\"]*\"/tag = \"v$ver\"/" Cargo.toml
CARGO_NET_GIT_FETCH_WITH_CLI=true timeout 600 cargo fetch
"$wt/scripts/maint/gen-catalog.sh" "$ver"
