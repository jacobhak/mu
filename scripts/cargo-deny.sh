#!/usr/bin/env bash
# cargo-deny over the dependency tree: licenses, duplicate-version bans and sources (the git-pinned oh-my-pi
# crates are the only git source). Advisories are left out: they change without a commit. A missing
# cargo-deny is skipped with a note; MU_REQUIRE_CARGO_DENY=1 makes it a failure.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! cargo deny --version > /dev/null 2>&1; then
  echo "cargo-deny: not installed" >&2
  [ -z "${MU_REQUIRE_CARGO_DENY:-}" ] || exit 1
  exit 0
fi
cargo deny --locked check licenses bans sources
