#!/usr/bin/env bash
# Usage: material.sh <old> <new> <outdir>   Collects what changed upstream between two omp releases.
set -euo pipefail
old="${1:?usage: material.sh <old> <new> <outdir>}"
new="${2:?}"
out="${3:?}"
MAINT="${MAINT:-$HOME/.local/state/ai-provider-gateway-maint}"
OMPSRC="${OMPSRC:-$MAINT/oh-my-pi}"
mkdir -p "$out"

if [ -d "$OMPSRC/.git" ] || [ -f "$OMPSRC/HEAD" ]; then
  timeout 600 git -C "$OMPSRC" fetch -q --tags
else
  timeout 1200 git clone -q --filter=blob:none https://github.com/can1357/oh-my-pi "$OMPSRC"
fi

GW=(packages/ai/src/auth-broker packages/ai/src/auth-gateway)
CLI=(packages/coding-agent/src/cli packages/coding-agent/src/commands)
git -C "$OMPSRC" diff "v$old" "v$new" -- 'packages/*/CHANGELOG.md' > "$out/changelog.diff" || true
git -C "$OMPSRC" diff "v$old" "v$new" -- "${GW[@]}" > "$out/gateway.diff" || true
git -C "$OMPSRC" diff --stat=200 "v$old" "v$new" -- "${GW[@]}" > "$out/gateway.stat" || true
git -C "$OMPSRC" diff "v$old" "v$new" -- "${CLI[@]}" > "$out/cli.diff" || true
git -C "$OMPSRC" diff --stat=200 "v$old" "v$new" -- "${CLI[@]}" > "$out/cli.stat" || true

cat > "$out/touchpoints.md" <<'TP'
# omp touchpoints in ai-provider-gateway

Commands it runs (with the pinned omp, found by `AI_PROVIDER_GATEWAY_OMP`, then `<home>/omp/<ver>/omp`, then PATH):
- `omp auth-broker serve --bind=127.0.0.1:0`: prints the URL it listens on (any `http://…` in its output).
- `omp auth-gateway token`: creates `auth-broker.token` and `auth-gateway.token` in omp's config root.
- `omp auth-gateway serve --bind=127.0.0.1:0` with `OMP_AUTH_BROKER_URL` and `OMP_AUTH_BROKER_TOKEN`: prints its URL, answers `/healthz`, serves `/v1/messages`, `/v1/models` and `/v1/usage` to clients with the gateway token.
- `omp models ls --json`: parsed as `{"models":[{provider,id,name?,reasoning?,pricingStatus?,cost:{input,output,cacheRead,cacheWrite,longContext?}}]}` and handed to clients as the price list.
- `omp login [provider]`: run with stdio inherited.
- `omp --version`: checked by `scripts/fetch-omp.sh` (`omp/<ver>`).

Files and environment it relies on:
- `<config root>/auth-gateway.token` and `auth-broker.token` (config root: `~/.omp` or `~/.omp/profiles/<p>`).
- `agent/agent.db` (SQLite credential store) in omp's agent directory: read by the mu adapter's sealing code.
- `PI_CONFIG_DIR` (relative to `$HOME`) and `PI_CODING_AGENT_DIR`: set by the mu adapter to point omp at a memory copy.
- `OMP_PROFILE` / `PI_PROFILE`: select the profile.
TP
