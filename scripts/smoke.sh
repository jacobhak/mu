#!/usr/bin/env bash
# Live end-to-end checks against the omp gateway (spends a few cents of haiku). Opt-in; not part of `cargo test`.
# Usage: scripts/smoke.sh [path-to-mu]   Needs: tmux, jq, omp on PATH, plugins/gateway built next to mu.
set -uo pipefail
mu="$(readlink -f "${1:-target/release/mu}")"
root="$(cd "$(dirname "$0")/.." && pwd)"
export MU_HOME="$(mktemp -d)"
mkdir -p "$MU_HOME/plugins" && ln -s "$root/plugins/gateway" "$MU_HOME/plugins/gateway"
work="$(mktemp -d)"
tm="mu-smoke-$$"
fail=0
pass() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
# Kill only gateway daemons that were started with this run's MU_HOME.
kill_daemons() {
  local p
  for p in $(pgrep -f "mu-gateway daemon" 2>/dev/null); do
    tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qxF "MU_HOME=$MU_HOME" && kill "$p" 2>/dev/null
  done
}
cleanup() { tmux kill-session -t "$tm" 2>/dev/null; kill_daemons; rm -rf "$MU_HOME" "$work"; }
trap cleanup EXIT

screen() { tmux capture-pane -pt "$tm" -S -200; }
wait_for() { # pattern timeout_s
  local end=$((SECONDS + ${2:-30}))
  while ((SECONDS < end)); do screen | grep -qF -- "$1" && return 0; sleep 0.5; done
  return 1
}
session_file() { ls -t "$MU_HOME"/sessions/*/*.jsonl | head -1; }
check() { # name command...
  local name="$1"; shift
  if "$@"; then pass "$name"; else bad "$name"; fi
}

cd "$work" || exit 1

# headless: bash output over the cap spills to an artifact that `read` can slice
out="$("$mu" -p 'Run bash: seq 1 40000. Then read artifact://1:-3 and reply with only the numbers, comma separated.' --model @smol 2>/dev/null)"
check "artifact spill + read selector" test "$(echo "$out" | tr -d ' \n')" = "39998,39999,40000"

# headless: auto-compaction fires mid-turn under a low fixed threshold
printf '[compaction.model_thresholds]\n"anthropic/claude-haiku-5-5" = "f24000"\n' > "$MU_HOME/config.toml"
files=$(ls -S "$root"/src/*.rs "$root"/src/*/*.rs | head -8 | tr '\n' ' ')
"$mu" -p "Read each of these files in separate read calls, then say DONE: $files" --model @smol >/dev/null 2>"$work/err"
check "auto-compaction" grep -q "compacted" "$work/err"
has_compaction() { jq -e -s 'any(.[]; .type == "compaction")' "$(session_file)" >/dev/null; }
check "compaction entry persisted" has_compaction
rm -f "$MU_HOME/config.toml"

# subagents: two scouts, child sessions linked to the parent
"$mu" -p 'Use the task tool with two scout tasks: (1) say the word ALPHA, (2) say the word BETA. Then reply with both words.' --model @smol 2>/dev/null | grep -q ALPHA
check "task: two scouts" test "$(grep -l parentSession "$MU_HOME"/sessions/*/*.jsonl | wc -l)" -ge 2

# TUI
tmux new -d -s "$tm" -x 150 -y 40 "cd $work && MU_HOME=$MU_HOME OMP_PROFILE=${OMP_PROFILE:-} $mu --model @smol; echo EXIT=\$?; sleep 120"
wait_for "μ" 20 || bad "tui starts"
tmux send-keys -t "$tm" 'run bash: sleep 8; echo done. Then reply FIN.' Enter
wait_for "\$ sleep 8" 30
tmux send-keys -t "$tm" 'also run date' Enter
check "steer shows in queue bar" wait_for "↳ steer: also run date" 5
check "steer reaches the model" wait_for "\$ date" 60
wait_for "FIN" 60

tmux send-keys -t "$tm" 'run bash (no pty, not async): sleep 60' Enter
wait_for "● bash sleep 60" 30
tmux send-keys -t "$tm" 'queued text' Enter
tmux send-keys -t "$tm" Escape
check "abort" wait_for "[aborted]" 15
check "queued text returns to composer" wait_for "❯ queued text" 5
tmux send-keys -t "$tm" C-u

seq 1 40 > "$work/paste.txt"
tmux load-buffer -b smoke "$work/paste.txt"
tmux paste-buffer -p -b smoke -t "$tm"
check "paste marker" wait_for "[Paste #1" 5
tmux send-keys -t "$tm" Enter
wait_for "ctx" 60; sleep 3
check "paste expanded in submitted prompt" grep -q '"text":"1\\n2\\n3' "$(session_file)"

tmux send-keys -t "$tm" 'Use the task tool to start one scout that replies ALPHA. Do not wait; reply STARTED immediately.' Enter
check "async task delivers notice" wait_for "job results delivered" 90
wait_for "ctx" 60; sleep 3

# mu notify: a real SIGUSR2 reaches the live TUI, which wakes by itself and answers
sid="$(basename "$(session_file)" .jsonl)"; sid="${sid##*_}"
out="$("$mu" notify "$(session_file)" --from smoke 'Reply with the word PONG spelled backwards, in capitals, and nothing else.')"
check "notify delivered" test "${out#delivered (pid }" != "$out"
check "notify wakes the TUI" wait_for "mu notify from smoke" 10
check "notify answered" wait_for "GNOP" 60
wait_for "ctx" 60; sleep 3
check "inbox drained" test -z "$(ls "$MU_HOME/inbox/$sid" 2>/dev/null)"

tmux send-keys -t "$tm" C-c
tmux send-keys -t "$tm" C-c
check "double Ctrl-C exits 0" wait_for "EXIT=0" 20

exit $fail
