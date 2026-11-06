#!/usr/bin/env bash
# Memory of N interactive mu sessions at once, against a loopback fake Anthropic server. Linux only (/proc).
# Usage: scripts/measure-sessions.sh [-m native|bypass] [path-to-mu] [N ...]     (default: native, target/release/mu, 1 2 4 8)
#   native: a fake API key stored in mu; no omp process is involved.
#   bypass: MU_GATEWAY_URL points at the fake, as mu-bench does; the real gateway pair is not started.
# Each session is a TUI in its own tmux session and directory, all sharing one scratch HOME and MU_HOME. Samples the mu
# processes' Pss and Pss_Anon (/proc/<pid>/smaps_rollup) 9 s after start, then 6 s into a request that the fake holds
# open for 15 s. Prints the sum over the sessions and the per-session mean.
# Needs: tmux, python3. Any other process running the same binary would share its pages and lower Pss, so it aborts.
set -uo pipefail
mode=native
if [[ "${1:-}" == -m ]]; then mode="$2"; shift 2; fi
[[ "$mode" == native || "$mode" == bypass ]] || { echo "mode must be native or bypass" >&2; exit 2; }
mu="$(readlink -f "${1:-target/release/mu}")"; shift || true
counts=("$@"); [[ ${#counts[@]} -gt 0 ]] || counts=(1 2 4 8)
here="$(cd "$(dirname "$0")" && pwd)"

mu_pids() {
  local p
  for p in /proc/[0-9]*; do
    [[ "$(readlink "$p/exe" 2>/dev/null)" == "$mu" ]] && echo "${p#/proc/}"
  done
}

run() { # n
  local n="$1" i p T fp url envs pids
  if [[ -n "$(mu_pids)" ]]; then echo "other processes run $mu; stop them first" >&2; return 1; fi
  T="$(mktemp -d)"
  export HOME="$T/home" MU_HOME="$T/mu" PI_CODING_AGENT_DIR="$T/home/agent"
  mkdir -p "$HOME/agent" "$MU_HOME"
  unset OMP_PROFILE MU_PROFILE ANTHROPIC_API_KEY MU_GATEWAY_URL MU_GATEWAY_TOKEN
  python3 "$here/fake-anthropic.py" "$T/port" 15 "$T/req.log" & fp=$!
  for _ in $(seq 40); do [[ -s "$T/port" ]] && break; sleep 0.2; done
  url="http://127.0.0.1:$(cat "$T/port")"
  envs="HOME=$HOME MU_HOME=$MU_HOME PI_CODING_AGENT_DIR=$PI_CODING_AGENT_DIR"
  if [[ "$mode" == native ]]; then
    printf 'sk-fake-native' | "$mu" auth add anthropic --chat --base-url "$url" --no-validate >/dev/null 2>&1
  else
    # A fresh price catalogue, as a normal session has; without it mu starts `omp models ls`.
    echo '{"models":[{"provider":"anthropic","id":"claude-sonnet-5-5","name":"Sonnet","pricingStatus":"fixed","reasoning":false,"cost":{"input":3.0,"output":15.0,"cacheRead":0.3,"cacheWrite":3.75}}]}' >"$MU_HOME/pricing.json"
    envs="$envs MU_GATEWAY_URL=$url MU_GATEWAY_TOKEN=bench"
  fi
  for i in $(seq "$n"); do
    mkdir -p "$T/work$i"
    tmux new -d -s "mums-$$-$i" -x 150 -y 40 \
      "cd $T/work$i && env -u OMP_PROFILE -u MU_PROFILE -u ANTHROPIC_API_KEY -u MU_GATEWAY_URL -u MU_GATEWAY_TOKEN $envs $mu --model anthropic/claude-sonnet-5-5; sleep 60"
  done
  sleep 9
  pids="$(mu_pids | tr '\n' ' ')"
  sample() {
    for p in $pids; do awk '/^Pss:/{a=$2} /^Pss_Anon:/{b=$2} END{print a, b}' "/proc/$p/smaps_rollup"; done |
      awk -v l="$1" -v n="$n" -v mode="$mode" '{t+=$1; p+=$2; c++} END{
        printf "%-6s n=%-2d %-9s %d procs  sum %6.1f MB total %6.1f MB private   per session %5.1f total %4.1f private\n",
          mode, n, l, c, t/1024, p/1024, t/1024/n, p/1024/n}'
  }
  sample idle
  for i in $(seq "$n"); do tmux send-keys -t "mums-$$-$i" 'say pong' Enter; done
  sleep 6
  sample "in flight"
  [[ "$(grep -c '^POST' "$T/req.log")" == "$n" ]] || echo "  warning: the fake saw $(grep -c '^POST' "$T/req.log") requests, not $n" >&2
  for p in $(mu_pids); do kill -9 "$p" 2>/dev/null; done
  for i in $(seq "$n"); do tmux kill-session -t "mums-$$-$i" 2>/dev/null; done
  kill "$fp" 2>/dev/null; wait 2>/dev/null
  rm -rf "$T"
}

for n in "${counts[@]}"; do run "$n" || exit 1; done
