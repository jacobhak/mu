#!/usr/bin/env bash
# Usage: bump.sh <worktree> <omp-version|-> [<pi-version>]   Pins every omp artifact (and the pi release) in the
# worktree. `-` or an empty version leaves that upstream alone; a pi-only bump runs no cargo and no catalog step.
# Exit 75 when a requested release has no assets yet, 76 when the omp pin is written but cargo cannot resolve it.
set -euo pipefail
wt="$(cd "${1:?usage: bump.sh <worktree> <omp-version|-> [<pi-version>]}" && pwd)"
ver="${2:?usage: bump.sh <worktree> <omp-version|-> [<pi-version>]}"
pi_ver="${3:--}"
[ "$ver" != - ] || ver=""
[ "$pi_ver" != - ] || pi_ver=""

# release_published <name> <url>: 0 when published, 75 when not, exit 1 on any other answer.
release_published() {
  local code
  code="$(curl -sIL --max-time 30 -o /dev/null -w '%{http_code}' "$2" || true)"
  case "$code" in
    200) ;;
    404) echo "$1 release not published yet" >&2; exit 75 ;;
    *) echo "release check failed (http $code)" >&2; exit 1 ;;
  esac
}

[ -z "$ver" ] || release_published "omp v$ver" "https://github.com/can1357/oh-my-pi/releases/download/v$ver/SHA256SUMS.txt"
[ -z "$pi_ver" ] || release_published "pi v$pi_ver" "https://github.com/earendil-works/pi/releases/download/v$pi_ver/SHA256SUMS"

cd "$wt"
[ -z "$pi_ver" ] || sed -i -E "s/^pi = \"[^\"]*\"/pi = \"$pi_ver\"/" upstream.toml
[ -n "$ver" ] || exit 0
sed -i -E "s/^omp = \"[^\"]*\"/omp = \"$ver\"/" upstream.toml
# The notices name the release and its commit: the peeled commit of an annotated tag, else the tag's own (a lightweight tag).
sha7="$(git ls-remote https://github.com/can1357/oh-my-pi "refs/tags/v$ver^{}" "refs/tags/v$ver" |
  awk '/\^\{\}$/ { p = substr($1, 1, 7) } !/\^\{\}$/ { t = substr($1, 1, 7) } END { print (p != "" ? p : t) }')"
[ -z "$sha7" ] || sed -i -E "s/release v[0-9][0-9.]* \(commit [0-9a-f]+\)/release v$ver (commit $sha7)/" THIRD-PARTY-NOTICES.md
sed -i -E "/can1357\/oh-my-pi/ s/(rev|tag) = \"[^\"]*\"/tag = \"v$ver\"/" Cargo.toml
# A timeout is the network; any other failure is a manifest the new tag no longer satisfies, which the fixer can repair.
rc=0
CARGO_NET_GIT_FETCH_WITH_CLI=true timeout 600 cargo fetch || rc=$?
case "$rc" in
  0) ;;
  124) exit 124 ;;
  *) exit 76 ;;
esac
"$wt/scripts/maint/gen-catalog.sh" "$ver"
