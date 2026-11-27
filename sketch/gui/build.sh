#!/usr/bin/env bash
# Builds the sketch on an offload node and fetches the binary to sketch/gui/target/release/mu-gui-sketch.
# The offload image lacks GPUI's Linux build libraries, so the job installs them first (the sketch's first
# finding: the real app's CI image needs them too). Run from the repository root.
set -euo pipefail

cd "$(dirname "$0")/../.."

if [ "${1:-}" = --on-node ]; then
  need=(libfontconfig-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev libx11-xcb-dev
    libxcb1-dev libxcb-xkb-dev libssl-dev libasound2-dev)
  missing=()
  for p in "${need[@]}"; do
    dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p")
  done
  if [ "${#missing[@]}" -gt 0 ]; then
    sudo apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "${missing[@]}"
  fi
  exec cargo build --release --manifest-path sketch/gui/Cargo.toml
fi

exec offload run --arch amd64 --fetch sketch/gui/target/release/mu-gui-sketch -- \
  sketch/gui/build.sh --on-node
