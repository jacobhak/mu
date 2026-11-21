#!/usr/bin/env bash
# Usage: fetch-omp.sh <version> <dest>   Installs the pinned omp release binary at <dest> (idempotent).
set -euo pipefail
ver="${1:?usage: fetch-omp.sh <version> <dest>}"
dest="${2:?usage: fetch-omp.sh <version> <dest>}"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) plat=linux-x64 ;;
  Linux/aarch64) plat=linux-arm64 ;;
  Darwin/arm64) plat=darwin-arm64 ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

if [ -x "$dest" ] && [ "$("$dest" --version 2>/dev/null || true)" = "omp/$ver" ]; then
  exit 0
fi

base="https://github.com/can1357/oh-my-pi/releases/download/v$ver"
mkdir -p "$(dirname "$dest")"
tmp="$(mktemp -d "$(dirname "$dest")/.omp-fetch.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/omp-$plat" "$base/omp-$plat"
curl -fsSL -o "$tmp/SHA256SUMS.txt" "$base/SHA256SUMS.txt"

want="$(awk -v f="omp-$plat" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS.txt" | head -1)"
if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/omp-$plat" | awk '{print $1}')"
else got="$(shasum -a 256 "$tmp/omp-$plat" | awk '{print $1}')"; fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  rm -f "$tmp/omp-$plat"
  echo "omp-$plat checksum mismatch" >&2
  exit 1
fi
chmod +x "$tmp/omp-$plat"
mv -f "$tmp/omp-$plat" "$dest"
