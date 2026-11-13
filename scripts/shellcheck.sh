#!/usr/bin/env bash
# Lints every tracked shell script with shellcheck at warning level. A missing shellcheck is skipped with a note,
# so a checkout or runner without it can still run ci-checks.sh; MU_REQUIRE_SHELLCHECK=1 makes it a failure.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v shellcheck > /dev/null; then
  echo "shellcheck: not installed" >&2
  [ -z "${MU_REQUIRE_SHELLCHECK:-}" ] || exit 1
  exit 0
fi
git ls-files -z '*.sh' .githooks | xargs -0 shellcheck --external-sources --source-path=SCRIPTDIR --severity=warning
