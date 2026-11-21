#!/usr/bin/env bash
# The checks CI runs, in order: formatting, shellcheck, cargo-deny, clippy, the workspace's tests, and the
# MCP bridge's tests. ci.yml, release.yml, ship.sh, the maintenance bot's check.sh and AGENTS.md
# all run this one list. The catalog generator's test is not here: it guards a bump-time transform that
# no runner touches, so `scripts/maint/check.sh` runs it.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
scripts/shellcheck.sh
scripts/cargo-deny.sh
# Plain cargo, no wrappers or env overrides: every build here, in CI and in offloaded jobs is the same build,
# so artifacts made by one are reused by the others. Lint levels live in [workspace.lints] in Cargo.toml.
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
# The terminal UI's tests again with every command and event sent through the wire form (see mu_base::wire).
MU_WIRE_LOOPBACK=1 cargo test -p mu-tui --lib --locked
scripts/build-mcp-http.sh test
