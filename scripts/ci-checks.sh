#!/usr/bin/env bash
# The checks CI runs, in order: formatting, clippy, the workspace tests, the MCP bridge's tests and the
# catalog generator's tests. ci.yml, ship.sh and AGENTS.md all call this one script.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/build-mcp-http.sh test
node --test scripts/maint/gen-catalog.test.mjs
