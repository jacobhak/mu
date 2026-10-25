#!/usr/bin/env bash
# Installs mu (binaries, plugins, pinned omp) under $MU_HOME (default ~/.mu).
# Usage: install.sh [--tag <tag>] [--from-source <dir>] [--skip-if-dev]
set -euo pipefail
REPO=jacobhak/mu
tag="" src="" skip_dev=0
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) tag="${2:?--tag needs a value}"; shift 2 ;;
    --from-source) src="$(cd "${2:?--from-source needs a dir}" && pwd)"; shift 2 ;;
    --skip-if-dev) skip_dev=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

link=1
[ -n "${MU_HOME:-}" ] && link=0
home="${MU_HOME:-$HOME/.mu}"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) plat=linux-x64 ;;
  Darwin/arm64) plat=darwin-arm64 ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# 1. Source
if [ -n "$src" ]; then
  (cd "$src" && cargo build --release --workspace --locked)
  . "$src/scripts/lib-layout.sh"
  stage_layout "$src" "${CARGO_TARGET_DIR:-$src/target}/release" "$tmp"
else
  if [ -n "$tag" ]; then url="https://github.com/$REPO/releases/download/$tag/mu-$plat.tar.gz"
  else url="https://github.com/$REPO/releases/latest/download/mu-$plat.tar.gz"; fi
  curl -fsSL -o "$tmp/mu-$plat.tar.gz" "$url" || { echo "install.sh: could not download $url" >&2; exit 1; }
  tar -C "$tmp" -xzf "$tmp/mu-$plat.tar.gz"
fi
stage="$tmp/mu"

# 2. Dev-build guard
if [ "$skip_dev" = 1 ] && [ -x "$home/bin/mu" ]; then
  sha="$("$home/bin/mu" --version 2>/dev/null | sed -n 's/.*(\([0-9a-f]*\(-dirty\)\{0,1\}\|unknown\)).*/\1/p' | head -1)"
  if [ -n "$sha" ]; then
    dev=0
    case "$sha" in
      unknown | *-dirty) dev=1 ;;
      *)
        if [ -n "$src" ]; then
          git -C "$src" merge-base --is-ancestor "$sha" origin/main 2>/dev/null || dev=1
        else
          # GitHub pretty-prints JSON; only the top-level "status" is indented two spaces (file entries have one too).
          st="$(curl -fsSL "https://api.github.com/repos/$REPO/compare/$sha...main?per_page=1" 2>/dev/null \
            | sed -n 's/^  "status": "\([a-z]*\)".*/\1/p' | head -1 || true)"
          case "$st" in ahead | identical) ;; *) dev=1 ;; esac
        fi ;;
    esac
    if [ "$dev" = 1 ]; then
      echo "install: skipped, installed mu is a dev build ($sha)"
      exit 0
    fi
  fi
fi

# 3. Install
mkdir -p "$home/bin" "$home/plugins"
for b in mu mu-gateway mu-web mu-predict; do
  cp "$stage/bin/$b" "$home/bin/.$b.new"
  mv -f "$home/bin/.$b.new" "$home/bin/$b"
done
omp_ver="$(sed -n 's/^omp = "\(.*\)"/\1/p' "$stage/upstream.toml")"
"$stage/scripts/fetch-omp.sh" "$omp_ver" "$home/bin/omp"
for p in gateway web predict memory; do
  rm -rf "${home:?}/plugins/$p"
  cp -R "$stage/plugins/$p" "$home/plugins/$p"
done
if command -v bun >/dev/null; then
  (cd "$home/plugins/memory" && bun install --frozen-lockfile)
else
  rm -rf "$home/plugins/memory"
  echo "memory plugin skipped: needs bun" >&2
fi

# 4. Link
if [ "$link" = 1 ]; then
  mkdir -p "$HOME/.local/bin"
  ln -sfn "$home/bin/mu" "$HOME/.local/bin/mu"
  other="$(command -v mu || true)"
  if [ -n "$other" ] && [ "$other" != "$HOME/.local/bin/mu" ]; then
    echo "another mu shadows ~/.local/bin/mu: $other" >&2
  fi
fi

# 5. Report
echo "installed $("$home/bin/mu" --version)"
