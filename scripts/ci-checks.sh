#!/usr/bin/env bash
# The checks CI runs, in order: formatting, the workspace built and tested through clippy, the MCP
# bridge's tests and the catalog generator's tests. ci.yml, ship.sh and AGENTS.md all call this script.
set -euo pipefail
cd "$(dirname "$0")/.."
# Tests bake CARGO_BIN_EXE_mu into their binaries, and in a worktree target/ is a symlink into the shared
# cache: build against the cache's own path, which outlives the worktree this run happens in.
if [ -L target ]; then
  CARGO_TARGET_DIR="$(cd target && pwd -P)"
  export CARGO_TARGET_DIR
fi

cargo fmt --all --check
# Lints and code generation in one pass: `cargo clippy` emits metadata only, so running it before
# `cargo test` compiles the whole dependency tree twice. The wrapper makes clippy lints fail the same
# rustc run that produces the test binaries; the workspace has no examples or benches to miss.
RUSTC_WORKSPACE_WRAPPER="$PWD/scripts/clippy-workspace.sh" cargo test --workspace --locked
scripts/build-mcp-http.sh test
node --test scripts/maint/gen-catalog.test.mjs
