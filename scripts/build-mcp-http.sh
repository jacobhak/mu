#!/usr/bin/env bash
# Builds and tests the mu-mcp-http bridge on its own, then checks the core stayed free of TLS.
# The bridge lives outside the workspace: Cargo unifies features across workspace members, so
# building it together with `mu` would give the core's reqwest rustls (and a panic at its first
# request, since the core installs no crypto provider).
#   scripts/build-mcp-http.sh [build|test|check]      (default: build; release profile)
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
manifest="$root/plugins/mcp-http/Cargo.toml"
case "${1:-build}" in
  build) cargo build --release --locked --manifest-path "$manifest" ;;
  test) cargo test --locked --manifest-path "$manifest" ;;
  check)
    # No TLS stack in the core, however the workspace was built.
    if cargo tree --locked --manifest-path "$root/Cargo.toml" -p mu -e normal | grep -q rustls; then
      echo "the core depends on rustls; keep TLS in plugins/mcp-http (outside the workspace)" >&2
      exit 1
    fi
    ;;
  *) echo "usage: $0 [build|test|check]" >&2; exit 2 ;;
esac
