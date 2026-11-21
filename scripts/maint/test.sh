#!/usr/bin/env bash
# Usage: scripts/maint/test.sh   Dry-run smoke of the nightly bot against a scratch MAINT and a scratch clone of
# HEAD: with the latest omp forced to the pinned one it must exit 0 and log `uptodate`. The check stage is a stub,
# so this neither builds nor recurses into ci-checks.sh. Needs flock(1) and jq (Linux).
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

head="$(git -C "$root" rev-parse HEAD)"
pinned="$(git -C "$root" show HEAD:upstream.toml | sed -n 's/^omp = "\(.*\)"/\1/p')"
export MAINT="$tmp/maint" CLONE="$tmp/maint/repo"
mkdir -p "$MAINT"
git clone -q --no-hardlinks "$root" "$CLONE"
git -C "$CLONE" update-ref refs/remotes/origin/main "$head"

cat > "$tmp/check-stub.sh" <<'EOF'
#!/usr/bin/env bash
mkdir -p "$2"
printf 'build ok\ntest ok\ncontract ok\n' > "$2/check.txt"
EOF
printf '#!/bin/sh\nexit 0\n' > "$tmp/mu"
chmod +x "$tmp/check-stub.sh" "$tmp/mu"

out="$tmp/out.txt"
MU="$tmp/mu" APG_MAINT_CHECK="$tmp/check-stub.sh" APG_MAINT_DRY_RUN=1 APG_MAINT_FORCE_LATEST="$pinned" \
  "$root/scripts/maint/run.sh" nightly > "$out" 2>&1 || { cat "$out" >&2; echo "maint test: nightly dry run failed" >&2; exit 1; }
grep -q 'result=uptodate' "$out" || { cat "$out" >&2; echo "maint test: expected result=uptodate" >&2; exit 1; }
echo "maint test: ok"
