#!/usr/bin/env bash
# Usage: bump.sh <worktree> <version>   Pins every omp artifact in the worktree to <version>.
# Exit 75 when @oh-my-pi/pi-mnemopi <version> is not on npm yet.
set -euo pipefail
wt="$(cd "${1:?usage: bump.sh <worktree> <version>}" && pwd)"
ver="${2:?usage: bump.sh <worktree> <version>}"

code="$(curl -sI --max-time 30 -o /dev/null -w '%{http_code}' "https://registry.npmjs.org/@oh-my-pi/pi-mnemopi/$ver" || true)"
case "$code" in
  200) ;;
  404) echo "npm package not published yet" >&2; exit 75 ;;
  *) echo "npm registry check failed (http $code)" >&2; exit 1 ;;
esac

cd "$wt"
sed -i -E "s/^omp = \"[^\"]*\"/omp = \"$ver\"/" upstream.toml
sed -i -E "/can1357\/oh-my-pi/ s/(rev|tag) = \"[^\"]*\"/tag = \"v$ver\"/" Cargo.toml
sed -i -E "s|(\"@oh-my-pi/pi-mnemopi\": *\")[^\"]*\"|\1$ver\"|" plugins/memory/package.json
CARGO_NET_GIT_FETCH_WITH_CLI=true timeout 600 cargo fetch
(cd plugins/memory && timeout 600 bun install)
