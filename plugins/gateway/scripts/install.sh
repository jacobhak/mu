#!/usr/bin/env bash
# Installs ai-provider-gateway and the omp release it is pinned to.
# Usage: install.sh [--from-source DIR] [--tag vX.Y.Z] [--prefix DIR] [--mu-home DIR | --no-mu]
#   --from-source DIR  build DIR with cargo instead of downloading a release
#   --tag vX.Y.Z       install that release (default: the latest)
#   --prefix DIR       the binary goes to DIR/bin (default: ~/.local)
#   --mu-home DIR      also register the gateway plugin in this mu home (default: $MU_HOME or ~/.mu, when it exists)
#   --no-mu            leave mu alone
# Run from an unpacked release tarball with no options, it installs that tarball's binary.
set -euo pipefail
REPO=jacobhak/ai-provider-gateway
tag="" src="" prefix="" mu_home="" no_mu=0
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) tag="${2:?--tag needs a value}"; shift 2 ;;
    --from-source) src="$(cd "${2:?--from-source needs a dir}" && pwd)"; shift 2 ;;
    --prefix) prefix="${2:?--prefix needs a dir}"; shift 2 ;;
    --mu-home) mu_home="${2:?--mu-home needs a dir}"; shift 2 ;;
    --no-mu) no_mu=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [ "$no_mu" = 1 ] && [ -n "$mu_home" ]; then
  echo "install.sh: --mu-home and --no-mu exclude each other" >&2
  exit 2
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${prefix:-$HOME/.local}"
mkdir -p "$prefix/bin"
bindir="$(cd "$prefix/bin" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Public URLs first; a private repo falls back to an authenticated gh.
has_gh() { command -v gh >/dev/null && gh auth status >/dev/null 2>&1; }

# 1. The binary, and the directory that holds upstream.toml and scripts/fetch-omp.sh for it
if [ -n "$src" ]; then
  (cd "$src" && cargo build --release --locked)
  bin="${CARGO_TARGET_DIR:-$src/target}/release/ai-provider-gateway"
  stage="$src"
elif [ -z "$tag" ] && [ -x "$here/../bin/ai-provider-gateway" ]; then
  bin="$here/../bin/ai-provider-gateway"
  stage="$here/.."
else
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) plat=linux-x64 ;;
    Darwin/arm64) plat=darwin-arm64 ;;
    *) echo "unsupported platform" >&2; exit 1 ;;
  esac
  asset="ai-provider-gateway-$plat.tar.gz"
  if [ -n "$tag" ]; then url="https://github.com/$REPO/releases/download/$tag/$asset"
  else url="https://github.com/$REPO/releases/latest/download/$asset"; fi
  sums_url="${url%/*}/SHA256SUMS"
  if curl -fsSL -o "$tmp/$asset" "$url" 2>/dev/null && curl -fsSL -o "$tmp/SHA256SUMS" "$sums_url" 2>/dev/null; then :
  elif has_gh; then
    gh release download ${tag:+"$tag"} -R "$REPO" -p "$asset" -p SHA256SUMS -D "$tmp" --clobber
  else
    echo "install.sh: could not download $url (a private repo needs an authenticated gh)" >&2
    exit 1
  fi
  # Refuse to unpack anything that does not match the published checksum.
  want="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS" 2>/dev/null | head -1)"
  if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
  else got="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"; fi
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "install.sh: $asset does not match SHA256SUMS (or the release has none); not installing" >&2
    exit 1
  fi
  # The checksum comes from the same release as the tarball, so it catches a bad download and not a replaced
  # release. A build attestation, signed by the release workflow, catches that too; it exists only where the
  # repo supports them, so a tarball without one is checked by its checksum alone.
  if has_gh; then
    n="$(gh api "repos/$REPO/attestations/sha256:$got" -q '.attestations | length' 2>/dev/null || true)"
    if [ "${n:-0}" -gt 0 ] 2>/dev/null; then
      if ! gh attestation verify "$tmp/$asset" -R "$REPO" >/dev/null 2>&1; then
        echo "install.sh: the attestation for $asset does not verify; not installing" >&2
        exit 1
      fi
      echo "install: build attestation verified"
    else
      echo "install: no build attestation for this release; checked its checksum only" >&2
    fi
  fi
  tar -C "$tmp" -xzf "$tmp/$asset"
  bin="$tmp/ai-provider-gateway/bin/ai-provider-gateway"
  stage="$tmp/ai-provider-gateway"
fi

# 2. Install the binary atomically
cp "$bin" "$bindir/.ai-provider-gateway.new"
chmod +x "$bindir/.ai-provider-gateway.new"
mv -f "$bindir/.ai-provider-gateway.new" "$bindir/ai-provider-gateway"

# 3. The pinned omp, where the tool looks for it
ver="$(sed -n 's/^omp = "\(.*\)"/\1/p' "$stage/upstream.toml")"
if [ -n "${AI_PROVIDER_GATEWAY_HOME:-}" ]; then state="$AI_PROVIDER_GATEWAY_HOME"
elif [ -n "${XDG_STATE_HOME:-}" ]; then state="$XDG_STATE_HOME/ai-provider-gateway"
else state="$HOME/.local/state/ai-provider-gateway"; fi
"$stage/scripts/fetch-omp.sh" "$ver" "$state/omp/$ver/omp"

# 4. mu: register the plugin
if [ "$no_mu" = 0 ]; then
  mu_home="${mu_home:-${MU_HOME:-$HOME/.mu}}"
  if [ -d "$mu_home" ]; then
    mkdir -p "$mu_home/plugins/gateway"
    cat > "$mu_home/plugins/gateway/plugin.toml.new" <<EOF
name = "gateway"
command = ["$bindir/ai-provider-gateway", "mu-plugin"]
lifetime = "idle:10m"
provider = "gateway"
EOF
    mv -f "$mu_home/plugins/gateway/plugin.toml.new" "$mu_home/plugins/gateway/plugin.toml"
    echo "install: registered the gateway plugin in $mu_home/plugins/gateway"
  fi
fi

echo "installed $("$bindir/ai-provider-gateway" --version)"
