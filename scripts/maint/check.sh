#!/usr/bin/env bash
# Usage: check.sh <worktree> <outdir>   Runs the check stages on the worktree.
# Writes <outdir>/<stage>.log and <outdir>/check.txt. Exit 0 only when every stage is ok.
set -uo pipefail
wt="$(cd "${1:?usage: check.sh <worktree> <outdir>}" && pwd)"
out="${2:?usage: check.sh <worktree> <outdir>}"
MAINT="${MAINT:-$HOME/.mu/maint}"
: "${CARGO_TARGET_DIR:=$MAINT/target}"
export CARGO_TARGET_DIR
mkdir -p "$out"
out="$(cd "$out" && pwd)"

# The fmt/clippy/test stage is the heavy one: with offload installed and its build nodes reachable it runs
# there (`offload run -- <command>`, as AGENTS.md says) instead of on the bot's box. Build, contract and
# smoke stay local: they need a live model, API keys, tmux and the release binary. MU_MAINT_OFFLOAD=0 forces
# everything local. A stage that outlives
# MU_MAINT_STAGE_SECS fails: a hung test (issue #21) once held the nightly run until systemd killed it after 3 h.
stage_secs="${MU_MAINT_STAGE_SECS:-2400}"
# Cargo jobs for the stages that compile on the bot's own box. The box is small (2 cores, ~3.4 GB): a bump
# recompiles gix, jj-lib and reqwest, and two rustc at once ran it out of memory (issue #46).
local_jobs="${MU_MAINT_LOCAL_JOBS:-1}"
use_offload() {
  [ "${MU_MAINT_OFFLOAD:-1}" != 0 ] && [ -f offload.toml ] && command -v offload > /dev/null \
    && timeout 300 offload run -- true > /dev/null 2>&1
}
# offload_run <command>...: retried once when the connection to the job drops (the command itself did not fail).
offload_run() {
  local log rc
  log="$(mktemp)"
  timeout "$stage_secs" offload run -- "$@" 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  if [ "$rc" != 0 ] && grep -q 'offload: connection to the job lost' "$log"; then
    echo "offload: retrying once after a lost connection"
    timeout "$stage_secs" offload run -- "$@"
    rc=$?
  fi
  rm -f "$log"
  return "$rc"
}

stages=(build test contract smoke)
cmd_build() { CARGO_BUILD_JOBS="$local_jobs" cargo build --release --workspace --locked && scripts/build-mcp-http.sh build && scripts/maint/gen-catalog.sh --check && node --test scripts/maint/gen-catalog.test.mjs && scripts/maint/test-only.test.sh; }
# The same list CI, ship.sh and the release run: fmt, clippy, the workspace tests, the MCP bridge's tests.
cmd_test() {
  # Two jobs, as ci.yml's runner: x86_64 build nodes peak near their 6 GiB at four (each of the five test
  # binaries links in 1.5 GB), and 2 costs little. aarch64 needs .cargo/config.toml's lld flags as well.
  if [ "$offloaded" = 1 ]; then offload_run env CARGO_BUILD_JOBS="${MU_MAINT_OFFLOAD_JOBS:-2}" scripts/ci-checks.sh; else CARGO_BUILD_JOBS="$local_jobs" timeout "$stage_secs" scripts/ci-checks.sh; fi
}
# The gateway plugins' contract tests run the real omp and the real pi, pinned by the worktree's upstream.toml.
cmd_contract() {
  local ver omp pi_ver pi
  ver="$(sed -n 's/^omp = "\(.*\)"/\1/p' upstream.toml)"
  omp="$MAINT/omp/$ver/omp"
  timeout "$stage_secs" scripts/fetch-omp.sh "$ver" "$omp" \
    && CARGO_BUILD_JOBS="$local_jobs" MU_GATEWAY_OMP="$omp" timeout "$stage_secs" cargo test --workspace --locked -- --ignored contract_ \
    || return 1
  command -v node > /dev/null || { echo "contract: node is not installed (needed for the pi proxy tests)" >&2; return 1; }
  pi_ver="$(sed -n 's/^pi = "\(.*\)"/\1/p' upstream.toml)"
  pi="$MAINT/pi/$pi_ver/pi"
  timeout "$stage_secs" scripts/fetch-pi.sh "$pi_ver" "$pi" \
    && MU_GATEWAY_PI="$pi" timeout "$stage_secs" node --test plugins/gateway-pi/test/*.test.mjs
}
# The smoke stage drives a live model and flakes: it gets one retry, and a pass on the retry is recorded as such.
cmd_smoke() {
  scripts/smoke.sh "$CARGO_TARGET_DIR/release/mu" && return 0
  touch "$out/smoke.retried"
  scripts/smoke.sh "$CARGO_TARGET_DIR/release/mu"
}

rm -f "$out"/*.retried
: > "$out/check.txt"
failed=0
cd "$wt" || exit 1
offloaded=0
if use_offload; then offloaded=1; fi
echo "test stage: $([ "$offloaded" = 1 ] && echo offload || echo local)" >&2
for s in "${stages[@]}"; do
  if [ "$failed" = 1 ]; then
    echo "$s skipped" >> "$out/check.txt"
    continue
  fi
  if "cmd_$s" > "$out/$s.log" 2>&1; then
    if [ -e "$out/$s.retried" ]; then echo "$s ok (retried)" >> "$out/check.txt"; else echo "$s ok" >> "$out/check.txt"; fi
  else
    echo "$s FAILED" >> "$out/check.txt"
    failed=1
  fi
done
cat "$out/check.txt"
exit "$failed"
