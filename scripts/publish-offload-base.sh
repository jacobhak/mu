#!/usr/bin/env bash
# Publishes offload's base image for this repository: the CI runner image plus one layer holding the checkout's
# target/ and cargo's registry and git caches, at their absolute paths. A job that runs in that image starts as an
# incremental build from main. Run from main's CI after the checks, in the checkout they ran in; REGISTRY names the
# registry (no scheme) that holds arc-mu-runner and offload-base/mu. Each architecture's CI job publishes
# offload-base/mu:main-<arch>, and `publish-offload-base.sh index` makes the multi-arch main from them. A replaced
# manifest is deleted so the registry's daily garbage collection can free its blobs.
set -euo pipefail

: "${REGISTRY:?REGISTRY is not set}"
: "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is not set}"
cargo_home=${CARGO_HOME:-$HOME/.cargo}

crane_version=v0.22.1
# The base is per architecture too: the arm64 runner image carries an -arm64 suffix.
case $(uname -m) in
  x86_64) crane_arch=x86_64 arch=amd64 suffix="" crane_sha256=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0 ;;
  aarch64 | arm64) crane_arch=arm64 arch=arm64 suffix=-arm64 crane_sha256=898c0cff975f898a33e8c4580bdafb0e7c02c7faa33374e946762f97c4ab7110 ;;
  *) echo "publish-offload-base: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/crane.tar.gz" \
  "https://github.com/google/go-containerregistry/releases/download/$crane_version/go-containerregistry_Linux_$crane_arch.tar.gz"
echo "$crane_sha256  $work/crane.tar.gz" | sha256sum --check --quiet
tar -C "$work" -xzf "$work/crane.tar.gz" crane
crane=$work/crane

image=$REGISTRY/offload-base/mu

if [ "${1:-}" = index ]; then
  old_index=$("$crane" digest "$image:main" || true)
  old_children=()
  if [ -n "$old_index" ]; then
    # An index lists its children; a plain manifest (the amd64-only main of before) is its own.
    children=$("$crane" manifest "$image:main" | jq -r '.manifests[]?.digest')
    if [ -n "$children" ]; then
      while IFS= read -r c; do old_children+=("$c"); done <<<"$children"
    else
      old_children=("$old_index")
    fi
  fi

  args=() new_children=()
  for a in amd64 arm64; do
    if d=$("$crane" digest "$image:main-$a" 2>/dev/null); then
      args+=(-m "$image@$d")
      new_children+=("$d")
    fi
  done
  [ "${#args[@]}" -gt 0 ] || { echo "publish-offload-base: no per-arch base to index" >&2; exit 1; }

  "$crane" index append "${args[@]}" -t "$image:main"
  new_index=$("$crane" digest "$image:main")

  if [ -n "$old_index" ] && [ "$old_index" != "$new_index" ]; then
    # A single-manifest main is also its own child, and may be one of the new children.
    case " ${new_children[*]} " in *" $old_index "*) ;; *) "$crane" delete "$image@$old_index" ;; esac
  fi
  for c in ${old_children[@]+"${old_children[@]}"}; do
    [ "$c" != "$old_index" ] || continue
    case " ${new_children[*]} " in *" $c "*) ;; *) "$crane" delete "$image@$c" || true ;; esac
  done
  exit 0
fi

paths=()
for p in "$GITHUB_WORKSPACE/target" "$cargo_home/registry" "$cargo_home/git"; do
  [ -d "$p" ] && paths+=("${p#/}")
done
[ "${#paths[@]}" -gt 0 ] || { echo "publish-offload-base: nothing to publish" >&2; exit 1; }

# The checkout path and its parents below $HOME go in as directory entries of their own, owned by the CI user:
# extracting the layer would create them as root, and a job could not write its checkout there.
dirs=()
d=$GITHUB_WORKSPACE
case $d in "$HOME"/*) while [ "$d" != "$HOME" ]; do dirs=("${d#/}" ${dirs[@]+"${dirs[@]}"}); d=$(dirname "$d"); done ;; esac

tar -C / --owner="$(id -u)" --group="$(id -g)" --numeric-owner -czf "$work/layer.tar.gz" \
  --no-recursion ${dirs[@]+"${dirs[@]}"} --recursion "${paths[@]}"
du -h "$work/layer.tar.gz"

base=$REGISTRY/arc-mu-runner$suffix@$("$crane" digest "$REGISTRY/arc-mu-runner$suffix:latest")
old=$("$crane" digest "$image:main-$arch" || true)

"$crane" append --base "$base" -f "$work/layer.tar.gz" -t "$image:main-$arch"
new=$("$crane" digest "$image:main-$arch")

# The current main index may still list the replaced manifest; the index step deletes it once main moves on.
if [ -n "$old" ] && [ "$old" != "$new" ]; then
  if ! "$crane" manifest "$image:main" 2>/dev/null | jq -r '.manifests[]?.digest' | grep -qx "$old"; then
    "$crane" delete "$image@$old"
  fi
fi
