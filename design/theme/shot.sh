#!/usr/bin/env bash
# Screenshots of omp and mu rendering the same transcript, from a private tmux server (-L mushot; the default
# server is never touched).
#
#   design/theme/shot.sh live OUTDIR                 # run mu once on the scripted task; saves OUTDIR/session.jsonl
#   design/theme/shot.sh resume OUTDIR [SESSION] [AGENT...]
#       AGENT: omp | mu (bundled tokyo-night) | mu-term (Ghostty TokyoNight as `terminal` sees it) | mu:<theme>
#       SESSION defaults to design/theme/fixture.jsonl. Writes OUTDIR/<agent>.{ansi,html,png}.
#
# `live` costs one small model run; `resume` costs nothing.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
mode=${1:?live|resume}
out=${2:?outdir}
mkdir -p "$out"
cols=${COLS:-110}
rows=${ROWS:-60}
model=${MODEL:-anthropic/claude-sonnet-5-5}
sock=mushot
mu=$root/target/release/mu

run() { # name cwd cmd
  tmux -L $sock -f /dev/null kill-session -t "$1" 2>/dev/null || true
  tmux -L $sock -f /dev/null new-session -d -s "$1" -x "$cols" -y "$rows" -c "$2" \
    -e COLORTERM=truecolor -e OMP_PROFILE="${OMP_PROFILE:-}" -e PATH="$HOME/.mu/bin:$PATH" "$3"
  tmux -L $sock set -g history-limit 20000 >/dev/null
}

settle() { # name; waits until the pane is unchanged for 8s
  local prev="" cur stable=0
  for _ in $(seq 1 150); do
    sleep 2
    cur=$(tmux -L $sock capture-pane -p -t "$1" | md5sum)
    if [[ $cur == "$prev" ]]; then stable=$((stable + 1)); else stable=0; fi
    prev=$cur
    [[ $stable -ge 4 ]] && return
  done
}

capture() { # name
  tmux -L $sock capture-pane -e -p -S - -t "$1" >"$out/$1.ansi"
  tmux -L $sock kill-session -t "$1"
  python3 "$here/ansi2png.py" "$out/$1.ansi" "$out/$1.png" --cols "$cols" --title "$1"
  echo "$out/$1.png"
}

case $mode in
live)
  work=/tmp/themeshot-fixture
  rm -rf "$work" && mkdir -p "$work"
  cp "$here/fixture/calc.py" "$work/calc.py"
  prompt=$(cat "$here/fixture/prompt.txt")
  run live "$work" "$mu --model $model --thinking medium --theme tokyo-night"
  sleep 4
  tmux -L $sock send-keys -t live -l "$prompt"
  sleep 0.5
  tmux -L $sock send-keys -t live Enter
  settle live
  capture live
  profile="${OMP_PROFILE:-${PI_PROFILE:-}}"
  s=$(ls -t "${MU_HOME:-$HOME/.mu}${profile:+/profiles/$profile}"/sessions/*themeshot-fixture*/*.jsonl | head -1)
  cp "$s" "$out/session.jsonl"
  echo "session: $out/session.jsonl (from $s)"
  ;;
resume)
  session=$(realpath "${3:-$here/fixture.jsonl}")
  shift 3 || shift $#
  agents=("$@")
  [[ ${#agents[@]} -gt 0 ]] || agents=(omp mu mu-term)
  for agent in "${agents[@]}"; do
    case $agent in
      omp) cmd="omp --resume $session" ;;
      mu) cmd="$mu --resume $session --theme tokyo-night" ;;
      mu-term) cmd="$mu --resume $session --theme $here/ghostty-tokyo.toml" ;;
      mu:*) cmd="$mu --resume $session --theme ${agent#mu:}" ;;
      *) echo "unknown agent $agent" >&2; exit 2 ;;
    esac
    name=${agent//[:\/.]/-}
    run "$name" /tmp "$cmd"
    settle "$name"
    capture "$name"
  done
  ;;
*) echo "mode: live | resume" >&2; exit 2 ;;
esac
