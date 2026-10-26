#!/usr/bin/env bash
set -uo pipefail
bin="$(readlink -f "${1:?usage: measure-idle.sh <binary>}")"
sleep 20 | script -qfc "$bin" /dev/null >/dev/null &
spid=$!
sleep 10
pid="$(pgrep -n -f "^$bin" || true)"
if [[ -n "$pid" ]]; then
  awk '/^(Pss|Pss_Anon):/ {printf "%s %.1f MB\n", $1, $2/1024}' "/proc/$pid/smaps_rollup"
else
  echo "mu process not found" >&2
fi
kill -- "-$spid" 2>/dev/null || kill "$spid" 2>/dev/null || true
[[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
