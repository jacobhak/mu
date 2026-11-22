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
GW=(packages/ai/src/auth-broker packages/ai/src/auth-gateway)
GWCLI=(packages/coding-agent/src/commands)
DEST=changelog.diff; d 'packages/*/CHANGELOG.md'
DEST=cli.diff; d packages/coding-agent/src/cli
DEST=crates.diff; d crates/edit crates/shell crates/diff crates/xfs crates/vendor
DEST=gateway.diff; d "${GW[@]}"
DEST=gateway-cli.diff; d "${GWCLI[@]}"
# stat files: a short index of every changed file
stat() { git -C "$OMPSRC" diff --stat=200 "v$old" "v$new" -- "${@:2}" > "$out/$1" || true; }
stat cli.stat packages/coding-agent/src/cli
stat crates.stat crates/edit crates/shell crates/diff crates/xfs crates/vendor
stat gateway.stat "${GW[@]}"
stat gateway-cli.stat "${GWCLI[@]}"

cat > "$out/touchpoints.md" <<'TP'
# omp touchpoints in mu

- Rust crates pinned in Cargo.toml by tag: `omp-edit`, `omp-shell`, `omp-diff`, `omp-xfs`, plus the patched `tree-sitter-go`, `grep-searcher`, `jaq-core`, `jaq-json`, `jaq-std` (`[patch.crates-io]`).
- The gateway plugin (`plugins/gateway`, binary `mu-gateway`) runs the pinned omp CLI (`~/.mu/bin/omp`, or `MU_GATEWAY_OMP`) as a subprocess. mu itself runs no omp binary. Commands it runs:
  - `omp auth-broker serve --bind=127.0.0.1:0`: prints the URL it listens on (any `http://…` in its output).
  - `omp auth-gateway token`: creates `auth-broker.token` and `auth-gateway.token` in omp's config root.
  - `omp auth-gateway serve --bind=127.0.0.1:0` with `OMP_AUTH_BROKER_URL` and `OMP_AUTH_BROKER_TOKEN`: prints its URL, answers `/healthz`, serves `/v1/messages`, `/v1/models` and `/v1/usage` to clients with the gateway token.
  - `omp models ls --json`: parsed as `{"models":[{provider,id,name?,reasoning?,pricingStatus?,cost:{input,output,cacheRead,cacheWrite,longContext?}}]}` and handed to mu as the price list.
  - `omp login [provider]`: run with stdio inherited (`mu-gateway login`).
  - `omp --version`: checked by `scripts/fetch-omp.sh` (`omp/<ver>`).
- Files and environment the plugin relies on: `<config root>/auth-gateway.token` and `auth-broker.token` (config root: `~/.omp` or `~/.omp/profiles/<p>`); `agent/agent.db` (SQLite credential store, read by the plugin's sealing code in `plugins/gateway/src/sealed_state.rs`); `PI_CONFIG_DIR` (relative to `$HOME`) and `PI_CODING_AGENT_DIR`, which the plugin sets to point omp at a memory copy; `OMP_PROFILE` / `PI_PROFILE`.
- Provider APIs reached through the gateway plugin; the gateway's request and response shapes (`/v1/messages`, `/v1/models`, `/v1/usage`) are parsed by `crates/mu-provider`.
- omp config read by mu: `config.yml` status line, theme, skills; `.omp/agents`, `.omp/commands`, `AGENTS.md`.
TP
