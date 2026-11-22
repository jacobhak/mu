#!/usr/bin/env bash
# Usage: scripts/package.sh <plat>   Builds dist/ai-provider-gateway-<plat>.tar.gz from an existing target/release.
set -euo pipefail
plat="${1:?usage: package.sh <plat>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
rel="${CARGO_TARGET_DIR:-$root/target}/release"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
d="$tmp/ai-provider-gateway"
mkdir -p "$d/bin" "$d/scripts"
cp "$rel/ai-provider-gateway" "$d/bin/"
cp "$root/scripts/install.sh" "$root/scripts/fetch-omp.sh" "$d/scripts/"
cp "$root/upstream.toml" "$root/LICENSE" "$root/README.md" "$d/"
mkdir -p "$root/dist"
tar -C "$tmp" -czf "$root/dist/ai-provider-gateway-$plat.tar.gz" ai-provider-gateway
echo "dist/ai-provider-gateway-$plat.tar.gz"
