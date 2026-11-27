#!/usr/bin/env bash
# Memory of the omp broker + gateway pair that mu-gateway's daemon starts, idle and with requests in flight. Linux only (/proc).
# Usage: scripts/measure-gateway.sh [REQUESTS]      (default: 1)
# Runs in an empty HOME. The broker gets only a fake Anthropic key, so the requests go to the real api.anthropic.com
# and are rejected at once (401): this needs the network and sends no real credential, and it does not hold a model stream
# open. The gateway cannot be pointed at a fake provider (it ignores ANTHROPIC_BASE_URL and the proxy variables, and lists
# no models for a keyless llama.cpp), so a held-open stream cannot be measured this way.
# Samples Pss / Pss_Anon of each process (and its children) 8 s after start, then 6 s after sending the requests.
# Needs: omp (or MU_GATEWAY_OMP), curl.
set -uo pipefail
omp="${MU_GATEWAY_OMP:-omp}"
reqs="${1:-1}"
H="$(mktemp -d)"
export HOME="$H" XDG_CONFIG_HOME="$H/.config" XDG_DATA_HOME="$H/.local/share" XDG_CACHE_HOME="$H/.cache"
unset MU_PROFILE OMP_PROFILE OMP_AUTH_BROKER_URL OMP_AUTH_BROKER_TOKEN
bp="" gp=""
trap 'kill $bp $gp 2>/dev/null; wait 2>/dev/null; rm -rf "$H"' EXIT

"$omp" auth-broker serve --bind=127.0.0.1:0 >"$H/b.log" 2>&1 & bp=$!
for _ in $(seq 50); do grep -q 'http://' "$H/b.log" && break; sleep 0.2; done
burl="$(grep -o 'http://[^ "]*' "$H/b.log" | head -1)"
"$omp" auth-gateway token >/dev/null 2>&1
btok="$(tr -d '[:space:]' <"$(find "$H" -name auth-broker.token | head -1)")"
gtok="$(tr -d '[:space:]' <"$(find "$H" -name auth-gateway.token | head -1)")"
[[ -n "$burl" && -n "$btok" && -n "$gtok" ]] || { echo "broker did not start" >&2; exit 1; }
# Only the fake key reaches the broker: the rest of the environment is dropped.
env -i HOME="$HOME" PATH="$PATH" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" XDG_DATA_HOME="$XDG_DATA_HOME" XDG_CACHE_HOME="$XDG_CACHE_HOME" \
  OMP_AUTH_BROKER_URL="$burl" OMP_AUTH_BROKER_TOKEN="$btok" ANTHROPIC_API_KEY=sk-ant-fake \
  "$omp" auth-broker migrate --from-local --include-env >/dev/null 2>&1
OMP_AUTH_BROKER_URL="$burl" OMP_AUTH_BROKER_TOKEN="$btok" "$omp" auth-gateway serve --bind=127.0.0.1:0 >"$H/g.log" 2>&1 & gp=$!
for _ in $(seq 50); do grep -q 'http://' "$H/g.log" && break; sleep 0.2; done
gurl="$(grep -o 'http://[^ "]*' "$H/g.log" | head -1)"
sleep 8

tree_mem() { # pid -> "Pss Pss_Anon" in kB, summed over the process and its children
  local p
  for p in $1 $(pgrep -P "$1"); do
    awk '/^Pss:/{a=$2} /^Pss_Anon:/{b=$2} END{print a, b}' "/proc/$p/smaps_rollup" 2>/dev/null
  done | awk '{t+=$1; p+=$2} END{print t+0, p+0}'
}
report() {
  read -r bt bpv < <(tree_mem "$bp"); read -r gt gpv < <(tree_mem "$gp")
  awk -v l="$1" -v bt="$bt" -v bp="$bpv" -v gt="$gt" -v gp="$gpv" 'BEGIN{
    printf "%-10s broker %3.0f/%3.0f  gateway %3.0f/%3.0f  pair %3.0f/%3.0f MB (total/private)\n",
      l, bt/1024, bp/1024, gt/1024, gp/1024, (bt+gt)/1024, (bp+gp)/1024}'
}
report idle
for i in $(seq "$reqs"); do
  curl -sN -m 25 -H "Authorization: Bearer $gtok" -H 'content-type: application/json' \
    -d '{"model":"anthropic/claude-sonnet-5-5","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"say pong"}]}' \
    "$gurl/v1/chat/completions" >"$H/resp$i.txt" 2>&1 &
done
sleep 6
report "$reqs request(s)"
grep -q authentication_error "$H/resp1.txt" || echo "warning: the request was not rejected as expected: $(head -c 200 "$H/resp1.txt")" >&2
