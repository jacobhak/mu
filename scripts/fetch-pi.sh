#!/usr/bin/env bash
# Usage: fetch-pi.sh <version> <dest>   Installs the pinned pi release at <dest> (idempotent).
# pi reads its version, theme and native assets from the directory its binary sits in (a lone pi/pi reports
# 0.0.0), so the release's pi/ directory goes to <dest>.pkg and <dest> is a launcher that runs it from there.
set -euo pipefail
ver="${1:?usage: fetch-pi.sh <version> <dest>}"
dest="${2:?usage: fetch-pi.sh <version> <dest>}"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) plat=linux-x64 ;;
  Linux/aarch64) plat=linux-arm64 ;;
  Darwin/arm64) plat=darwin-arm64 ;;
  Darwin/x86_64) plat=darwin-x64 ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

if [ -x "$dest" ] && [ "$("$dest" --version 2>/dev/null || true)" = "$ver" ]; then
  exit 0
fi

asset="pi-$plat.tar.gz"
base="https://github.com/earendil-works/pi/releases/download/v$ver"
mkdir -p "$(dirname "$dest")"
tmp="$(mktemp -d "$(dirname "$dest")/.pi-fetch.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$asset" "$base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

want="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS" | head -1)"
if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
else got="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"; fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "$asset checksum mismatch" >&2
  exit 1
fi
mkdir "$tmp/x"
tar -C "$tmp/x" -xzf "$tmp/$asset"
[ -x "$tmp/x/pi/pi" ] || { echo "$asset has no pi/pi" >&2; exit 1; }
printf '#!/bin/sh\nexec "$0.pkg/pi" "$@"\n' > "$tmp/launcher"
chmod +x "$tmp/launcher"
rm -rf "$dest.pkg.old"
[ ! -e "$dest.pkg" ] || mv "$dest.pkg" "$dest.pkg.old"
mv "$tmp/x/pi" "$dest.pkg"
mv -f "$tmp/launcher" "$dest"
rm -rf "$dest.pkg.old"
