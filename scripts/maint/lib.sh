#!/usr/bin/env bash
# Shared helpers for the maintenance bot. Source it; do not run it.
# Constants come from the environment or ~/.mu/maint/config.env (loaded here and by the systemd unit).

: "${MAINT:=$HOME/.mu/maint}"
if [ -f "$MAINT/config.env" ]; then set -a; . "$MAINT/config.env"; set +a; fi
: "${CLONE:=$MAINT/mu}"
: "${OMPSRC:=$MAINT/oh-my-pi}"
: "${CARGO_TARGET_DIR:=$MAINT/target}"
: "${MU:=$HOME/.mu/bin/mu}"
: "${MU_MAINT_MONTHLY_USD:=40}"
: "${MU_MAINT_TRIAGE_USD:=1.5}"
: "${MU_MAINT_FIX_USD:=5}"
: "${MU_MAINT_DIGEST_USD:=2}"
: "${MU_MAINT_DRY_RUN:=0}"
export MAINT CLONE OMPSRC CARGO_TARGET_DIR MU
REPO=jacobhak/mu
OMP_REPO=can1357/oh-my-pi
MODE="${MODE:-adhoc}"
LOGFILE="$MAINT/logs/$(date +%Y-%m-%d)-$MODE.log"
mkdir -p "$MAINT/logs" "$MAINT/wt" "$MAINT/runs"
touch "$MAINT/spend.tsv"

log() {
  local line
  line="$(date '+%Y-%m-%dT%H:%M:%S') $*"
  echo "$line"
  echo "$line" >> "$LOGFILE"
}

dry() { [ "$MU_MAINT_DRY_RUN" = 1 ]; }

# Goes to stderr + the log file so it never pollutes a $(...) capture.
dry_log() {
  local line
  line="$(date '+%Y-%m-%dT%H:%M:%S') DRY-RUN: $*"
  echo "$line" >&2
  echo "$line" >> "$LOGFILE"
}

# Every GitHub-mutating gh call goes through here.
ghw() {
  if dry; then dry_log "gh $*"; return 0; fi
  gh "$@"
}

# Every git push / tag / install goes through here: pushw <git-dir> <git args...>
pushw() {
  local dir="$1"; shift
  if dry; then dry_log "git -C $dir $*"; return 0; fi
  git -C "$dir" "$@"
}

# Runs a command that publishes something (e.g. install.sh) unless dry.
publishw() {
  if dry; then dry_log "$*"; return 0; fi
  "$@"
}

# issue_upsert <label> <title> <bodyfile>
issue_upsert() {
  local label="$1" title="$2" body="$3" n
  n="$(gh issue list -R "$REPO" -l "$label" --state open --json number -q '.[0].number' 2>/dev/null || true)"
  if [ -n "$n" ]; then
    ghw issue comment "$n" -R "$REPO" --body-file "$body"
  else
    ghw issue create -R "$REPO" --label "$label" --title "$title" --body-file "$body"
  fi
}

month_spend() {
  awk -F'\t' -v m="$(date +%Y-%m)" 'index($1, m) == 1 { s += $4 } END { printf "%.4f", s + 0 }' "$MAINT/spend.tsv"
}

# run_agent <agent> <usd-cap> <cwd> <promptfile> <outfile>
# Runs the installed stable $MU. Returns mu's exit code, or 10 when the monthly budget is used up.
run_agent() {
  local agent="$1" cap="$2" cwd="$3" prompt="$4" out="$5" spent err rc cost
  spent="$(month_spend)"
  if awk -v s="$spent" -v c="$cap" -v m="$MU_MAINT_MONTHLY_USD" 'BEGIN { exit !(s + c > m) }'; then
    local b; b="$(mktemp)"
    printf 'Month spend $%s + cap $%s would exceed the monthly agent budget $%s (MU_MAINT_MONTHLY_USD).\nAgent: %s\n' "$spent" "$cap" "$MU_MAINT_MONTHLY_USD" "$agent" > "$b"
    issue_upsert maint:budget "maint: monthly agent budget reached" "$b"
    rm -f "$b"
    return 10
  fi
  err="$out.err"
  log "agent $agent (cap \$$cap) in $cwd"
  rc=0
  (cd "$cwd" && timeout 45m "$MU" -p "$(cat "$prompt")" --agent "$agent" --max-cost "$cap" > "$out" 2> "$err" 9>&-) || rc=$?
  cost="$(sed -n 's/^mu: cost \$\([0-9.]*\).*/\1/p' "$err" | tail -1)"
  printf '%s\t%s\t%s\t%s\n' "$(date +%F)" "$MODE" "$agent" "${cost:-$cap}" >> "$MAINT/spend.tsv"
  log "agent $agent exit=$rc cost=\$${cost:-$cap}"
  return "$rc"
}

# worktree <name> <ref> [branch]
worktree() {
  local name="$1" ref="$2" branch="${3:-}" p="$MAINT/wt/$1"
  if [ -e "$p" ]; then
    git -C "$CLONE" worktree remove --force "$p" 2>/dev/null || rm -rf "$p"
  fi
  git -C "$CLONE" worktree prune
  if [ -n "$branch" ]; then
    git -C "$CLONE" worktree add -q -B "$branch" "$p" "$ref"
  else
    git -C "$CLONE" worktree add -q --detach "$p" "$ref"
  fi
}

on_err() {
  local rc=$? line=${BASH_LINENO[0]:-?}
  trap - ERR
  log "ERROR: exit $rc at ${BASH_SOURCE[1]:-?}:$line"
  local b; b="$(mktemp)"
  { echo "The maintenance bot failed in mode \`$MODE\` (exit $rc at line $line)."; echo; echo '```'; tail -n 100 "$LOGFILE"; echo '```'; } > "$b"
  issue_upsert maint:bot-error "maint bot failed ($MODE)" "$b" || true
  rm -f "$b"
  exit "$rc"
}
trap on_err ERR
# systemd kills a run that outlives TimeoutStartSec with SIGTERM: leave a trace instead of dying silently.
on_term() {
  trap - ERR TERM
  log "ERROR: terminated (timeout or stop)"
  local b; b="$(mktemp)"
  { echo "The maintenance bot was terminated in mode \`$MODE\` (hung or timed out)."; echo; echo '```'; tail -n 50 "$LOGFILE"; echo '```'; } > "$b"
  timeout 60 gh issue comment "$(gh issue list -R "$REPO" -l maint:bot-error --state open --json number -q '.[0].number' 2>/dev/null)" -R "$REPO" --body-file "$b" 2>/dev/null \
    || timeout 60 gh issue create -R "$REPO" --label maint:bot-error --title "maint bot failed ($MODE)" --body-file "$b" || true
  rm -f "$b"
  exit 143
}
trap on_term TERM
