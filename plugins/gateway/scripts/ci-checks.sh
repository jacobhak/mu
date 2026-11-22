#!/usr/bin/env bash
# The checks CI runs, in order: formatting, shellcheck, clippy, the tests, and the contract tests against the
# pinned omp. ci.yml, release.yml and the maintenance bot all run this one list.
# REQUIRE_SHELLCHECK=1 makes a missing shellcheck a failure; SKIP_CONTRACT=1 skips the contract stage (the
# bot's `contract` stage runs it on its own); CONTRACT_OMP_DIR is where the pinned omp is fetched to.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check

if command -v shellcheck > /dev/null; then
  git ls-files -z '*.sh' .githooks | xargs -0 shellcheck --external-sources --source-path=SCRIPTDIR --severity=warning
else
  echo "shellcheck: not installed" >&2
  [ -z "${REQUIRE_SHELLCHECK:-}" ] || exit 1
fi

cargo clippy --all-targets --locked -- -D warnings
cargo test --locked

if [ -n "${SKIP_CONTRACT:-}" ]; then
  echo "contract: skipped (SKIP_CONTRACT)" >&2
else
  ver="$(sed -n 's/^omp = "\(.*\)"/\1/p' upstream.toml)"
  omp="${CONTRACT_OMP_DIR:-$PWD/target/omp}/$ver/omp"
  scripts/fetch-omp.sh "$ver" "$omp"
  AI_PROVIDER_GATEWAY_OMP="$omp" cargo test --locked -- --ignored contract_
fi

# The maintenance bot's dry-run smoke needs an authenticated gh and flock(1); without them it is skipped.
if command -v gh > /dev/null && gh auth status > /dev/null 2>&1 && command -v flock > /dev/null; then
  scripts/maint/test.sh
else
  echo "maint test: skipped (needs an authenticated gh and flock)" >&2
fi
