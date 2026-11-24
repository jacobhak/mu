#!/usr/bin/env bash
# Usage: material.sh <old> <new> <outdir> [<pi-old> <pi-new>]   Collects what changed upstream between two omp
# releases (skipped when they are equal) and, when the pi versions differ, the pi release notes in between.
set -euo pipefail
old="${1:?usage: material.sh <old> <new> <outdir> [<pi-old> <pi-new>]}"
new="${2:?}"
out="${3:?}"
pi_old="${4:-}"
pi_new="${5:-}"
MAINT="${MAINT:-$HOME/.mu/maint}"
OMPSRC="${OMPSRC:-$MAINT/oh-my-pi}"
mkdir -p "$out"

if [ -n "$pi_old" ] && [ -n "$pi_new" ] && [ "$pi_old" != "$pi_new" ]; then
  : > "$out/pi-release-notes.md"
  for t in $(gh release list -R earendil-works/pi -L 100 --json tagName -q '.[].tagName' 2>/dev/null | sort -V || true); do
    v="${t#v}"
    [ "$v" != "$pi_old" ] || continue
    [ "$(printf '%s\n%s\n' "$pi_old" "$v" | sort -V | head -1)" = "$pi_old" ] || continue
    [ "$(printf '%s\n%s\n' "$v" "$pi_new" | sort -V | head -1)" = "$v" ] || continue
    { echo "## $t"; echo; gh release view "$t" -R earendil-works/pi --json body -q .body 2>/dev/null || true; echo; } >> "$out/pi-release-notes.md"
  done
fi

omp_material() {
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
}
if [ "$old" != "$new" ]; then omp_material; fi

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
- omp config read by mu: `config.yml` theme, skills; `.omp/agents`, `.omp/commands`, `AGENTS.md`.
- pi (`earendil-works/pi`, pinned as `pi` in `upstream.toml`, installed to `~/.mu/bin/pi` by `scripts/fetch-pi.sh`): the `gateway-pi` plugin (`plugins/gateway-pi`, `mu-gateway --backend pi`) runs `pi --mode rpc --no-session -ne -e proxy.mjs …`; `proxy.mjs` is a pi extension that serves `/v1/messages`, `/v1/models`, `/v1/usage` and `/healthz` over pi's `ctx.modelRegistry` (`getAll`, `streamSimple`, auth and OAuth refresh) and writes the price list in a one-shot mode. Pi release notes, when pi was bumped, are in `pi-release-notes.md`; what matters is extension API (`session_start`, `ctx.modelRegistry`, `ctx.shutdown`), CLI flags (`--mode rpc`, `-ne`, `-e`, `--no-mcp`, `--no-skills`, `--no-prompt-templates`, `--no-themes`, `--no-context-files`, `-na`), model catalog and pricing, and `pi-ai` streaming message and usage shapes.
TP
