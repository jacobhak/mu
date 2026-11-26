#!/usr/bin/env bash
# The checks CI runs, in order: formatting, shellcheck, cargo-deny, the pi gateway's translation tests, clippy,
# the workspace's tests, the MCP bridge's tests, and the remote pod adapter's fake-kubectl test.
# ci.yml, release.yml, ship.sh, the maintenance bot's check.sh and AGENTS.md
# all run this one list. The catalog generator's test is not here: it guards a bump-time transform that
# no runner touches, so `scripts/maint/check.sh` runs it.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
scripts/shellcheck.sh
scripts/cargo-deny.sh
# Skipped with a note when node is missing; MU_REQUIRE_NODE=1 makes that a failure.
if command -v node > /dev/null 2>&1; then
  node --test plugins/gateway-pi/test/translate.test.mjs
else
  echo "node: not installed" >&2
  [ -z "${MU_REQUIRE_NODE:-}" ] || exit 1
fi
# Plain cargo, no wrappers or env overrides: every build here, in CI and in offloaded jobs is the same build,
# so artifacts made by one are reused by the others. Lint levels live in [workspace.lints] in Cargo.toml.
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
# The terminal UI's tests again with every command and event sent through the wire form (see mu_base::wire).
MU_WIRE_LOOPBACK=1 cargo test -p mu-tui --lib --locked
scripts/build-mcp-http.sh test
# Skipped with a note when jq is missing; MU_REQUIRE_JQ=1 makes that a failure.
if command -v jq > /dev/null 2>&1; then
  plugins/remote/scripts/test-pod-run.sh
else
  echo "jq: not installed" >&2
  [ -z "${MU_REQUIRE_JQ:-}" ] || exit 1
fi
