#!/usr/bin/env bash
# Installs mu (binaries, plugins) under $MU_HOME (default ~/.mu).
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

# Public URLs first; a private repo (or fork) falls back to an authenticated gh.
has_gh() { command -v gh >/dev/null && gh auth status >/dev/null 2>&1; }

# A file's physical path, following symlinks (macOS has no `readlink -f`) and any `..` or symlinked
# directory in the way (`pwd -P`), so two spellings of the same binary compare equal.
resolved_path() {
  local p="$1" t hops=0
  while [ -L "$p" ] && [ "$hops" -lt 40 ]; do
    t="$(readlink "$p")"
    case "$t" in
      /*) p="$t" ;;
      *) p="$(dirname "$p")/$t" ;;
    esac
    hops=$((hops + 1))
  done
  (cd "$(dirname "$p")" 2>/dev/null && printf '%s/%s\n' "$(pwd -P)" "$(basename "$p")")
}

# 1. Source
# build.rs reruns only when git's HEAD or index change, so a build made from an unstaged edit can carry a
# clean-looking sha. The tree is asked here instead, and the answer recorded for `mu update` and --skip-if-dev.
dirty=0
if [ -n "$src" ] && [ -n "$(git -C "$src" status --porcelain --untracked-files=no 2>/dev/null)" ]; then dirty=1; fi
if [ -n "$src" ]; then
  (cd "$src" && cargo build --release --workspace --locked && scripts/build-mcp-http.sh build)
  . "$src/scripts/lib-layout.sh"
  stage_layout "$src" "${CARGO_TARGET_DIR:-$src/target}/release" "$tmp"
else
  if [ -n "$tag" ]; then url="https://github.com/$REPO/releases/download/$tag/mu-$plat.tar.gz"
  else url="https://github.com/$REPO/releases/latest/download/mu-$plat.tar.gz"; fi
  sums_url="${url%/*}/SHA256SUMS"
  if curl -fsSL -o "$tmp/mu-$plat.tar.gz" "$url" 2>/dev/null && curl -fsSL -o "$tmp/SHA256SUMS" "$sums_url" 2>/dev/null; then :
  elif has_gh; then
    gh release download ${tag:+"$tag"} -R "$REPO" -p "mu-$plat.tar.gz" -p SHA256SUMS -D "$tmp" --clobber
  else
    echo "install.sh: could not download $url (a private repo needs an authenticated gh)" >&2
    exit 1
  fi
  # Refuse to unpack anything that does not match the published checksum.
  want="$(awk -v f="mu-$plat.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS" 2>/dev/null | head -1)"
  if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/mu-$plat.tar.gz" | awk '{print $1}')"
  else got="$(shasum -a 256 "$tmp/mu-$plat.tar.gz" | awk '{print $1}')"; fi
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "install.sh: mu-$plat.tar.gz does not match SHA256SUMS (or the release has none); not installing" >&2
    exit 1
  fi
  # The checksum comes from the same release as the tarball, so it catches a bad download and not a
  # replaced release. A build attestation, signed by the release workflow, catches that too; it exists
  # only where the repo supports them, so a tarball without one is checked by its checksum alone.
  if has_gh; then
    n="$(gh api "repos/$REPO/attestations/sha256:$got" -q '.attestations | length' 2>/dev/null || true)"
    if [ "${n:-0}" -gt 0 ] 2>/dev/null; then
      if ! gh attestation verify "$tmp/mu-$plat.tar.gz" -R "$REPO" >/dev/null 2>&1; then
        echo "install.sh: the attestation for mu-$plat.tar.gz does not verify; not installing" >&2
        exit 1
      fi
      echo "install: build attestation verified"
    else
      echo "install: no build attestation for this release; checked its checksum only" >&2
    fi
  fi
  tar -C "$tmp" -xzf "$tmp/mu-$plat.tar.gz"
fi
stage="$tmp/mu"

# 2. Dev-build guard
if [ "$skip_dev" = 1 ] && [ -x "$home/bin/mu" ]; then
  if [ -e "$home/dev-build" ]; then
    echo "install: skipped, installed mu was built from uncommitted changes"
    exit 0
  fi
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
          case "$st" in "") has_gh && st="$(gh api "repos/$REPO/compare/$sha...main" -q .status 2>/dev/null || true)" ;; esac
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
for b in mu mu-web mu-mcp-http mu-predict mu-memory; do
  [ -e "$stage/bin/$b" ] || continue
  cp "$stage/bin/$b" "$home/bin/.$b.new"
  mv -f "$home/bin/.$b.new" "$home/bin/$b"
done
for p in web predict memory; do
  [ -d "$stage/plugins/$p" ] || continue
  # Swap in whole: a half-copied plugin directory is never visible, and the old one is kept on failure.
  dest="${home:?}/plugins/$p"
  rm -rf "$dest.new" "$dest.old"
  cp -R "$stage/plugins/$p" "$dest.new"
  [ -e "$dest" ] && mv "$dest" "$dest.old"
  mv "$dest.new" "$dest"
  rm -rf "$dest.old"
done
# The gateway plugin left mu: an installed one (and its binary and omp) stays as it is.
if grep -qs 'mu-gateway' "$home/plugins/gateway/plugin.toml"; then
  echo "install: the gateway plugin is no longer part of mu; install ai-provider-gateway to replace it (the old one is left as is)"
fi
# The memory plugin used to be a Bun daemon; its socket and lock are all it left in $MU_HOME/memory.
rm -f "$home/bin/mu-memory-daemon" "$home/memory/sock" "$home/memory/lock"
rmdir "$home/memory" 2>/dev/null || true

if [ "$dirty" = 1 ]; then
  git -C "$src" rev-parse --short=12 HEAD > "$home/dev-build"
else
  rm -f "$home/dev-build"
fi

# 4. Link
if [ "$link" = 1 ]; then
  mkdir -p "$HOME/.local/bin"
  ln -sfn "$home/bin/mu" "$HOME/.local/bin/mu"
  other="$(command -v mu || true)"
  if [ -n "$other" ] && [ "$(resolved_path "$other")" != "$(resolved_path "$HOME/.local/bin/mu")" ]; then
    echo "another mu shadows ~/.local/bin/mu: $other" >&2
  fi
fi

# 5. Report
echo "installed $("$home/bin/mu" --version)"
