#!/usr/bin/env bash
# Builds and tests the mu-mcp-http bridge on its own. It is a separate Cargo project outside the
# workspace: it pulls in rmcp, which the core does not need.
#   scripts/build-mcp-http.sh [build|test]      (default: build; release profile)
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
target="${CARGO_TARGET_DIR:-$root/target}"
# The tests bake CARGO_BIN_EXE_mu-mcp-http into the binary. In a worktree that target/ is a symlink into
# the shared cache, so name the cache itself: a path through the worktree dies with the worktree, and
# cargo reuses these test binaries across worktrees.
if [ -L "$target" ]; then
  target="$(cd "$target" && pwd -P)"
fi
export CARGO_TARGET_DIR="$target"
manifest="$root/plugins/mcp-http/Cargo.toml"
case "${1:-build}" in
  build) cargo build --release --locked --manifest-path "$manifest" ;;
  test) cargo test --locked --manifest-path "$manifest" ;;
  *) echo "usage: $0 [build|test]" >&2; exit 2 ;;
esac
