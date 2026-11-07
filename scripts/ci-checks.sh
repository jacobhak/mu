#!/usr/bin/env bash
# The checks CI runs, in order: formatting, the workspace built and tested through clippy, the MCP
# bridge's tests and the catalog generator's tests. ci.yml, ship.sh and AGENTS.md all call this script.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
# Lints and code generation in one pass: `cargo clippy` emits metadata only, so running it before
# `cargo test` compiles the whole dependency tree twice. The wrapper makes clippy lints fail the same
# rustc run that produces the test binaries; the workspace has no examples or benches to miss.
RUSTC_WORKSPACE_WRAPPER="$PWD/scripts/clippy-workspace.sh" cargo test --workspace --locked
scripts/build-mcp-http.sh test
node --test scripts/maint/gen-catalog.test.mjs
