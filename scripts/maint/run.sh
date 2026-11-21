#!/usr/bin/env bash
# Usage: run.sh nightly
# nightly: baseline check (S0), then bump the pinned omp when a newer release exists (S1/S2), and release a patch
# version after an automatic merge.
set -Eeuo pipefail
MODE="${1:?usage: run.sh nightly}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$here/lib.sh"
CHECK="${APG_MAINT_CHECK:-$here/check.sh}"

[ -x "$MU" ] || { echo "run.sh: $MU is not installed (install mu first)" >&2; exit 1; }
exec 9> "$MAINT/lock"
if ! flock -n 9; then
  log "another maint run holds the lock; exiting"
  exit 0
fi

TS="$(date +%Y%m%d-%H%M%S)"
RUN="$MAINT/runs/$TS-$MODE"
mkdir -p "$RUN"
SPEND_START="$(wc -l < "$MAINT/spend.tsv")"
git -C "$CLONE" fetch -q --tags origin

run_spend_rows() { tail -n +"$((SPEND_START + 1))" "$MAINT/spend.tsv"; }
run_spend() { run_spend_rows | awk -F'\t' '{ s += $4 } END { printf "%.4f", s + 0 }'; }
pinned_ver() { git -C "$CLONE" show origin/main:upstream.toml | sed -n 's/^omp = "\(.*\)"/\1/p'; }
latest_omp() { gh release view -R "$OMP_REPO" --json tagName -q .tagName | sed 's/^v//'; }

# failing_tail <rundir>: check.txt plus the tail of the first failing stage's log
failing_tail() {
  local dir="$1" stage
  echo '```'; cat "$dir/check.txt"; echo '```'
  stage="$(awk '$2 == "FAILED" { print $1; exit }' "$dir/check.txt")"
  if [ -n "$stage" ] && [ -f "$dir/$stage.log" ]; then
    echo; echo "Tail of \`$stage.log\`:"; echo '```'; tail -n 200 "$dir/$stage.log"; echo '```'
  fi
}

# pr_create <branch> <title> <bodyfile> <label>...
pr_create() {
  local branch="$1" title="$2" body="$3"; shift 3
  local labels=() l
  for l in "$@"; do labels+=(--label "$l"); done
  ghw pr create -R "$REPO" --base main --head "$branch" --title "$title" --body-file "$body" "${labels[@]}"
}

# pr_update <number> <title> <bodyfile> <label>...: retitle an open PR and swap its maint status labels.
# REST, not `gh pr edit`: older gh releases fail that command on the retired classic-projects query.
pr_update() {
  local n="$1" title="$2" body="$3"; shift 3
  local add=() l
  ghw api --silent -X PATCH "repos/$REPO/pulls/$n" -f title="$title" -F body=@"$body"
  for l in maint:auto maint:needs-review maint:red; do
    [[ " $* " == *" $l "* ]] || ghw api --silent -X DELETE "repos/$REPO/issues/$n/labels/$l" 2>/dev/null || true
  done
  for l in "$@"; do add+=(-f "labels[]=$l"); done
  ghw api --silent -X POST "repos/$REPO/issues/$n/labels" "${add[@]}"
}

# ci_wait <branch>: waits for the branch's `check` job (the macos job gates nothing). Exit 0 when it passed,
# 1 when it ended any other way, 2 when it had not finished within APG_MAINT_CI_WAIT_SECS.
# The check-runs REST API, not `gh pr checks --json`: gh 2.46 (Ubuntu 24.04) has no --json there.
ci_wait() {
  local branch="$1" deadline sha state
  deadline=$((SECONDS + ${APG_MAINT_CI_WAIT_SECS:-3600}))
  while :; do
    sha="$(gh api "repos/$REPO/git/ref/heads/$branch" -q .object.sha 2> /dev/null || true)"
    state=""
    if [ -n "$sha" ]; then
      state="$(gh api "repos/$REPO/commits/$sha/check-runs?per_page=100" -q '[.check_runs[] | select(.name == "check")] | sort_by(.id) | last | if . == null then "" elif .status != "completed" then "pending" else .conclusion end' 2> /dev/null || true)"
    fi
    case "$state" in
      success) return 0 ;;
      "" | pending) ;;
      *) return 1 ;;
    esac
    [ "$SECONDS" -lt "$deadline" ] || return 2
    sleep 30
  done
}

# Tags the next patch version on origin/main; the release workflow publishes it.
release_patch() {
  local last next
  git -C "$CLONE" fetch -q --tags origin
  last="$(git -C "$CLONE" tag -l 'v*' | sort -V | tail -n 1)"
  if [ -z "$last" ]; then
    log "no v* tag yet; the first release is cut by hand"
    return 0
  fi
  next="${last%.*}.$(( ${last##*.} + 1 ))"
  log "releasing $next (after $last)"
  pushw "$CLONE" tag "$next" origin/main
  pushw "$CLONE" push origin "$next"
  echo "$next" > "$RUN/published"
}

nightly() {
  local result=uptodate published=no checked_sha pinned latest wt rc

  # --- Baseline (S0)
  worktree main origin/main
  wt="$MAINT/wt/main"
  checked_sha="$(git -C "$wt" rev-parse HEAD)"
  mkdir -p "$RUN/main"
  log "baseline check of ${checked_sha:0:7}"
  if ! "$CHECK" "$wt" "$RUN/main" 9>&- > "$RUN/main/check.out" 2>&1; then
    result=s0
    log "baseline red"
    local body="$RUN/s0-issue.md"
    { echo "\`check.sh\` fails on origin/main (${checked_sha:0:7}) with the pinned omp."; echo; failing_tail "$RUN/main"; } > "$body"
    issue_upsert maint:s0 "S0: main is broken" "$body"
    log "maint: result=$result published=$published spend=\$$(run_spend)"
    return 0
  fi
  log "baseline green"

  # --- Detect a release
  pinned="$(pinned_ver)"
  latest="${APG_MAINT_FORCE_LATEST:-$(latest_omp)}"
  local go=0
  if [ -n "$latest" ] && [ "$latest" != "$pinned" ]; then
    if [ -n "${APG_MAINT_FORCE_LATEST:-}" ] || [ "$(printf '%s\n%s\n' "$pinned" "$latest" | sort -V | tail -1)" = "$latest" ]; then go=1; fi
  fi
  # One bump PR at a time: a newer release rebuilds the open PR's branch instead of opening another.
  local pr="" branch="maint/omp-bump" had=""
  if [ "$go" = 1 ]; then
    local open head n
    open="$(gh pr list -R "$REPO" -l maint:bump --state open --json number,headRefName -q 'sort_by(-.number) | .[] | select(.headRefName | startswith("maint/omp")) | "\(.number) \(.headRefName)"')"
    while read -r n head; do
      [ -z "$n" ] && continue
      if [ -z "$pr" ]; then
        pr="$n"; branch="$head"
      else
        ghw pr close "$n" -R "$REPO" --comment "Superseded by #$pr" --delete-branch
      fi
    done <<< "$open"
    if [ -n "$pr" ]; then
      had="$(git -C "$CLONE" show "origin/$branch:upstream.toml" 2>/dev/null | sed -n 's/^omp = "\(.*\)"/\1/p' || true)"
      if [ "$had" = "$latest" ]; then
        log "bump PR #$pr for v$latest already open"
        go=0
      fi
    fi
  fi
  # A background subshell: its failure (ERR trap, set -e) is reported without ending the run.
  if [ "$go" = 1 ]; then
    rc=0; ( bump_flow "$pinned" "$latest" "$branch" "$pr" "$had" ) & wait $! || rc=$?
    [ -f "$RUN/result" ] && result="$(cat "$RUN/result")"
    [ -f "$RUN/published" ] && published="$(cat "$RUN/published")"
    if [ "$rc" != 0 ]; then result=bump-error; log "bump flow failed (exit $rc)"; fi
  fi
  log "maint: result=$result published=$published spend=\$$(run_spend)"
}

# bump_flow <pinned> <latest> <branch> [open-pr] [its-omp-version]; writes $RUN/result
bump_flow() {
  local pinned="$1" latest="$2" branch="$3" pr="${4:-}" had="${5:-}" bwt="$MAINT/wt/bump" rc pinfail=0
  worktree bump origin/main "$branch"
  rc=0; "$here/bump.sh" "$bwt" "$latest" > "$RUN/bump.log" 2>&1 || rc=$?
  if [ "$rc" = 75 ]; then
    log "omp release not published yet; retry next run"
    echo waiting-release > "$RUN/result"
    return 0
  elif [ "$rc" = 76 ]; then
    log "the omp v$latest binary cannot be fetched and checked"
    pinfail=1
  elif [ "$rc" != 0 ]; then
    tail -n 50 "$RUN/bump.log" >&2
    return "$rc"
  fi
  git -C "$bwt" add -A
  git -C "$bwt" commit -q -m "Bump omp to v$latest"

  mkdir -p "$RUN/bump"
  local green=0
  if [ "$pinfail" = 1 ]; then
    printf 'bump FAILED\nbuild skipped\ntest skipped\ncontract skipped\n' > "$RUN/bump/check.txt"
    cp "$RUN/bump.log" "$RUN/bump/bump.log"
  elif "$CHECK" "$bwt" "$RUN/bump" 9>&- > "$RUN/bump/check.out" 2>&1; then
    green=1
  fi

  "$here/material.sh" "$pinned" "$latest" "$RUN/material"
  local tprompt="$RUN/triage-prompt.md" tout="$RUN/triage.md" change=true summary="triage unavailable" risk=unknown fyi=""
  cat > "$tprompt" <<P
Triage the omp upgrade v$pinned -> v$latest for ai-provider-gateway. The material directory is $RUN/material. The ai-provider-gateway checkout with the bump applied is the current directory.
P
  rc=0; run_agent maint-triage "$APG_MAINT_TRIAGE_USD" "$bwt" "$tprompt" "$tout" || rc=$?
  if [ "$rc" = 0 ]; then
    local lastline
    lastline="$(grep -v '^[[:space:]]*$' "$tout" | tail -n 1)"
    if echo "$lastline" | jq -e '.behaviour_change | type == "boolean"' >/dev/null 2>&1; then
      change="$(echo "$lastline" | jq -r '.behaviour_change')"
      summary="$(echo "$lastline" | jq -r '.summary // ""')"
      risk="$(echo "$lastline" | jq -r '.risk // "unknown"')"
      fyi="$(echo "$lastline" | jq -r '.fyi // ""')"
    fi
  fi

  local labels sev result
  if [ "$green" = 1 ] && [ "$change" = false ]; then
    result=s2; sev="S2 — green bump, nothing here needs to change; auto-merged"; labels=(maint:bump maint:auto)
  else
    result=s1; sev="S1 — needs review"; labels=(maint:bump maint:needs-review)
    if [ "$green" != 1 ]; then
      sev="S1 — bump fails checks"
      labels+=(maint:red)
    fi
  fi

  {
    echo "## Severity"; echo "$sev"; echo
    echo "## Checks"
    if [ "$pinfail" = 1 ]; then failing_tail "$RUN/bump"; else echo '```'; cat "$RUN/bump/check.txt"; echo '```'; fi
    echo
    echo "## Upstream changes"; echo "$summary"; echo
    [ -z "$fyi" ] || { echo "## Also in this release (nothing to change here)"; echo "$fyi"; echo; }
    echo "Risk: $risk"; echo
    echo "## Agent spend"; echo '```'; run_spend_rows; echo '```'
  } > "$RUN/pr-body.md"

  echo "$result" > "$RUN/result"
  pushw "$bwt" push -f -u origin "$branch"
  local url
  if [ -n "$pr" ]; then
    pr_update "$pr" "Bump omp to v$latest" "$RUN/pr-body.md" "${labels[@]}"
    ghw pr comment "$pr" -R "$REPO" --body "Rebuilt for v$latest${had:+ (was v$had)}. The branch was reset to main, so earlier commits on it are gone."
    url="$(gh pr view "$pr" -R "$REPO" --json url -q .url)"
  else
    url="$(pr_create "$branch" "Bump omp to v$latest" "$RUN/pr-body.md" "${labels[@]}")"
  fi
  [ -z "$url" ] || echo "$url"
  if [ "$result" = s2 ]; then
    # check.sh already ran the same list as CI, but on this machine only: the PR's own run is the second opinion.
    local ci=0
    dry || ci_wait "$branch" || ci=$?
    if [ "$ci" = 0 ]; then
      ghw pr merge "$branch" -R "$REPO" --squash --delete-branch
      git -C "$CLONE" fetch -q origin
      release_patch
    else
      local why="the PR's CI run failed"
      [ "$ci" = 1 ] || why="the PR's CI run did not finish within ${APG_MAINT_CI_WAIT_SECS:-3600} s"
      log "bump PR $branch not merged: $why"
      echo s2-ci > "$RUN/result"
      local num="${pr:-${url##*/}}"
      ghw api --silent -X POST "repos/$REPO/issues/$num/labels" -f "labels[]=maint:needs-review"
      ghw api --silent -X DELETE "repos/$REPO/issues/$num/labels/maint:auto" 2> /dev/null || true
      ghw pr comment "$num" -R "$REPO" --body "Not auto-merged: $why. The local checks were green."
    fi
  fi
}

case "$MODE" in
  nightly) nightly ;;
  *) echo "unknown mode: $MODE" >&2; exit 2 ;;
esac
