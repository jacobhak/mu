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
DEST=cli.diff; d packages/coding-agent/src/cli
DEST=crates.diff; d crates/edit crates/shell crates/diff crates/xfs crates/vendor
# stat files: a short index of every changed file
stat() { git -C "$OMPSRC" diff --stat=200 "v$old" "v$new" -- "${@:2}" > "$out/$1" || true; }
stat cli.stat packages/coding-agent/src/cli
stat crates.stat crates/edit crates/shell crates/diff crates/xfs crates/vendor

cat > "$out/touchpoints.md" <<'TP'
# omp touchpoints in mu

- Rust crates pinned in Cargo.toml by tag: `omp-edit`, `omp-shell`, `omp-diff`, `omp-xfs`, plus the patched `tree-sitter-go`, `grep-searcher`, `jaq-core`, `jaq-json`, `jaq-std` (`[patch.crates-io]`).
- The gateway is a separate tool (`ai-provider-gateway`); mu runs no omp binary.
- Provider APIs reached through a gateway plugin.
- omp config read by mu: `config.yml` status line, theme, skills; `.omp/agents`, `.omp/commands`, `AGENTS.md`.
TP
