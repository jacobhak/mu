#!/usr/bin/env bash
# Builds and tests the mu-mcp-http bridge on its own. It is a separate Cargo project outside the
# workspace: it pulls in rmcp, which the core does not need.
#   scripts/build-mcp-http.sh [build|test]      (default: build; release profile)
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
manifest="$root/plugins/mcp-http/Cargo.toml"
case "${1:-build}" in
  build) cargo build --release --locked --manifest-path "$manifest" ;;
  test) cargo test --locked --manifest-path "$manifest" ;;
  *) echo "usage: $0 [build|test]" >&2; exit 2 ;;
esac
