#!/usr/bin/env bash
# Publishes offload's base image for this repository: the CI runner image plus one layer holding the checkout's
# target/ and cargo's registry and git caches, at their absolute paths. A job that runs in that image starts as an
# incremental build from main. Run from main's CI after the checks, in the checkout they ran in; REGISTRY names the
# registry (no scheme) that holds arc-mu-runner and offload-base/mu. Pushing replaces the tag; the previous manifest
# is deleted so the registry's weekly garbage collection can free its blobs.
set -euo pipefail

: "${REGISTRY:?REGISTRY is not set}"
: "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is not set}"
cargo_home=${CARGO_HOME:-$HOME/.cargo}

crane_version=v0.22.1
crane_sha256=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/crane.tar.gz" \
  "https://github.com/google/go-containerregistry/releases/download/$crane_version/go-containerregistry_Linux_x86_64.tar.gz"
echo "$crane_sha256  $work/crane.tar.gz" | sha256sum --check --quiet
tar -C "$work" -xzf "$work/crane.tar.gz" crane
crane=$work/crane

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

image=$REGISTRY/offload-base/mu
base=$REGISTRY/arc-mu-runner@$("$crane" digest "$REGISTRY/arc-mu-runner:latest")
old=$("$crane" digest "$image:main" || true)

"$crane" append --base "$base" -f "$work/layer.tar.gz" -t "$image:main"
new=$("$crane" digest "$image:main")

if [ -n "$old" ] && [ "$old" != "$new" ]; then
  "$crane" delete "$image@$old"
fi
