#!/usr/bin/env bash
# Usage: check.sh <worktree> <outdir>   Runs the check stages on the worktree.
# Writes <outdir>/<stage>.log and <outdir>/check.txt. Exit 0 only when every stage is ok.
# A stage that outlives APG_MAINT_STAGE_SECS (default 1800) fails.
set -uo pipefail
wt="$(cd "${1:?usage: check.sh <worktree> <outdir>}" && pwd)"
out="${2:?usage: check.sh <worktree> <outdir>}"
MAINT="${MAINT:-$HOME/.local/state/ai-provider-gateway-maint}"
: "${CARGO_TARGET_DIR:=$MAINT/target}"
export CARGO_TARGET_DIR
mkdir -p "$out"
out="$(cd "$out" && pwd)"
stage_secs="${APG_MAINT_STAGE_SECS:-1800}"

pinned() { sed -n 's/^omp = "\(.*\)"/\1/p' upstream.toml; }

stages=(build test contract)
cmd_build() { cargo build --release --locked; }
# The same list CI and the release run, minus the contract stage, which gets its own entry below.
cmd_test() { SKIP_CONTRACT=1 timeout "$stage_secs" scripts/ci-checks.sh; }
cmd_contract() {
  local ver omp
  ver="$(pinned)"
  omp="$MAINT/omp/$ver/omp"
  timeout "$stage_secs" scripts/fetch-omp.sh "$ver" "$omp" \
    && AI_PROVIDER_GATEWAY_OMP="$omp" timeout "$stage_secs" cargo test --locked -- --ignored contract_
}

: > "$out/check.txt"
failed=0
cd "$wt" || exit 1
for s in "${stages[@]}"; do
  if [ "$failed" = 1 ]; then
    echo "$s skipped" >> "$out/check.txt"
    continue
  fi
  if "cmd_$s" > "$out/$s.log" 2>&1; then
    echo "$s ok" >> "$out/check.txt"
  else
    echo "$s FAILED" >> "$out/check.txt"
    failed=1
  fi
done
cat "$out/check.txt"
exit "$failed"
