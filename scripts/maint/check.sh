#!/usr/bin/env bash
# Usage: check.sh <worktree> <outdir>   Runs the check stages with the omp pinned by the worktree.
# Writes <outdir>/<stage>.log and <outdir>/check.txt. Exit 0 only when every stage is ok.
set -uo pipefail
wt="$(cd "${1:?usage: check.sh <worktree> <outdir>}" && pwd)"
out="${2:?usage: check.sh <worktree> <outdir>}"
MAINT="${MAINT:-$HOME/.mu/maint}"
: "${CARGO_TARGET_DIR:=$MAINT/target}"
export CARGO_TARGET_DIR
mkdir -p "$out"
out="$(cd "$out" && pwd)"

ver="$(sed -n 's/^omp = "\(.*\)"/\1/p' "$wt/upstream.toml")"
omp="$MAINT/omp/$ver/omp"
if ! "$wt/scripts/fetch-omp.sh" "$ver" "$omp" > "$out/fetch-omp.log" 2>&1; then
  echo "fetch-omp FAILED" > "$out/check.txt"
  cat "$out/fetch-omp.log" >&2
  exit 1
fi
export MU_OMP="$omp"

stages=(build test contract smoke)
cmd_build() { cargo build --release --workspace --locked && scripts/build-mcp-http.sh build && scripts/maint/gen-catalog.sh --check; }
cmd_test() { cargo test --workspace --locked && scripts/build-mcp-http.sh test; }
cmd_contract() { cargo test --workspace --locked -- --ignored contract_; }
cmd_smoke() { scripts/smoke.sh "$CARGO_TARGET_DIR/release/mu" || scripts/smoke.sh "$CARGO_TARGET_DIR/release/mu"; }

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
