#!/usr/bin/env bash
# Usage: scripts/package.sh <plat>   Builds dist/mu-<plat>.tar.gz from an existing target/release.
set -euo pipefail
plat="${1:?usage: package.sh <plat>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
. "$root/scripts/lib-layout.sh"
rel="${CARGO_TARGET_DIR:-$root/target}/release"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
stage_layout "$root" "$rel" "$tmp"
mkdir -p "$root/dist"
tar -C "$tmp" -czf "$root/dist/mu-$plat.tar.gz" mu
echo "dist/mu-$plat.tar.gz"
