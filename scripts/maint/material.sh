#!/usr/bin/env bash
# Usage: material.sh <old> <new> <outdir>   Collects what changed upstream between two omp releases.
set -euo pipefail
old="${1:?usage: material.sh <old> <new> <outdir>}"
new="${2:?}"
out="${3:?}"
MAINT="${MAINT:-$HOME/.mu/maint}"
OMPSRC="${OMPSRC:-$MAINT/oh-my-pi}"
mkdir -p "$out"

if [ -d "$OMPSRC/.git" ] || [ -f "$OMPSRC/HEAD" ]; then
  timeout 600 git -C "$OMPSRC" fetch -q --tags
else
  timeout 1200 git clone -q --filter=blob:none https://github.com/can1357/oh-my-pi "$OMPSRC"
fi

d() { git -C "$OMPSRC" diff "v$old" "v$new" -- "${@}" > "$out/$DEST" || true; }
DEST=changelog.diff; d 'packages/*/CHANGELOG.md'
DEST=cli.diff; d packages/coding-agent/src/cli packages/mnemopi/src
DEST=crates.diff; d crates/pi-edit crates/pi-shell crates/pi-diff crates/pi-walker crates/vendor
# stat files: a short index of every changed file
stat() { git -C "$OMPSRC" diff --stat=200 "v$old" "v$new" -- "${@:2}" > "$out/$1" || true; }
stat cli.stat packages/coding-agent/src/cli packages/mnemopi/src
stat crates.stat crates/pi-edit crates/pi-shell crates/pi-diff crates/pi-walker crates/vendor

cat > "$out/touchpoints.md" <<'TP'
# omp touchpoints in mu

- Rust crates pinned in Cargo.toml by tag: `pi-edit`, `pi-shell`, `pi-diff`, `pi-walker`, plus patched `brush-core`, `brush-parser`, `tree-sitter-go`.
- The `omp` CLI, run as a subprocess (the pinned copy at ~/.mu/bin/omp): `auth-broker`, `auth-gateway`, `models ls --json`, `login`, `search`, `read`, `--version`.
- `@oh-my-pi/pi-mnemopi` in `plugins/memory` (mnemopi storage format).
- Provider APIs reached through the omp gateway.
- omp config read by mu: `config.yml` model roles, status line, theme, skills; `.omp/agents`, `.omp/commands`, `AGENTS.md`.
TP
