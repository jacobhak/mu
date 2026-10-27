#!/usr/bin/env bash
# Usage: run.sh nightly|digest
# nightly: baseline check (S0), omp bump (S1/S2/S2r), publish. digest: weekly parity digest (S3).
set -Eeuo pipefail
MODE="${1:?usage: run.sh nightly|digest}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$here/lib.sh"

[ -x "$MU" ] || { echo "run.sh: $MU is not installed (run scripts/install.sh first)" >&2; exit 1; }
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
tree_changed() { [ -n "$(git -C "$1" status --porcelain)" ]; }
first_line() { sed -n '/[^[:space:]]/{p;q}' "$1" | cut -c1-100; }
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

nightly() {
  local result=uptodate published=no checked_sha pinned latest wt rc

  # --- Baseline (S0)
  worktree main origin/main
  wt="$MAINT/wt/main"
  checked_sha="$(git -C "$wt" rev-parse HEAD)"
  mkdir -p "$RUN/main"
  log "baseline check of ${checked_sha:0:7}"
  if ! "$here/check.sh" "$wt" "$RUN/main" 9>&- > "$RUN/main/check.out" 2>&1; then
    result=s0
    log "baseline red"
    local body="$RUN/s0-issue.md" branch="maint/fix-main-$(date +%Y%m%d)"
    { echo "\`check.sh\` fails on origin/main (${checked_sha:0:7}) with the pinned omp."; echo; failing_tail "$RUN/main"; } > "$body"
    issue_upsert maint:s0 "S0: main is broken" "$body"
    worktree fix origin/main "$branch"
    local fixwt="$MAINT/wt/fix" prompt="$RUN/s0-prompt.md" fout="$RUN/s0-fixer.md"
    cat > "$prompt" <<P
Fix main. The check run is in $RUN/main (check.txt and one log per stage). The pinned omp for contract tests is at $MAINT/omp/$(pinned_ver)/omp (set MU_OMP to it). Work in the current directory.
P
    rc=0; run_agent maint-fixer "$MU_MAINT_FIX_USD" "$fixwt" "$prompt" "$fout" || rc=$?
    local green=0
    if [ "$rc" = 0 ] && tree_changed "$fixwt"; then
      mkdir -p "$RUN/fix"
      if "$here/check.sh" "$fixwt" "$RUN/fix" 9>&- > "$RUN/fix/check.out" 2>&1; then green=1; fi
    fi
    if [ "$green" = 1 ]; then
      local title="Fix main: $(first_line "$fout")"
      git -C "$fixwt" add -A
      git -C "$fixwt" commit -q -m "$title" -m "$(cat "$fout")"
      pushw "$fixwt" push -f -u origin "$branch"
      { echo "## Severity"; echo "S0 — main was red; fixer branch is green."; echo; echo "## Checks"; cat "$RUN/fix/check.txt"; echo; echo "## Fixer"; cat "$fout"; echo; echo "## Agent spend"; run_spend_rows; } > "$RUN/s0-pr.md"
      local url; url="$(pr_create "$branch" "$title" "$RUN/s0-pr.md" maint:needs-review)"
      echo "Proposed fix: ${url:-$branch}" > "$RUN/s0-comment.md"
      issue_upsert maint:s0 "S0: main is broken" "$RUN/s0-comment.md"
    else
      { echo "fixer could not make main green"; echo; cat "$fout" 2>/dev/null || true; } > "$RUN/s0-comment.md"
      issue_upsert maint:s0 "S0: main is broken" "$RUN/s0-comment.md"
    fi
    log "maint: result=$result published=$published spend=\$$(run_spend)"
    return 0
  fi
  log "baseline green"

  # --- Detect a release
  pinned="$(pinned_ver)"
  latest="${MU_MAINT_FORCE_LATEST:-$(latest_omp)}"
  local go=0
  if [ -n "$latest" ] && [ "$latest" != "$pinned" ]; then
    if [ -n "${MU_MAINT_FORCE_LATEST:-}" ] || [ "$(printf '%s\n%s\n' "$pinned" "$latest" | sort -V | tail -1)" = "$latest" ]; then go=1; fi
  fi
  if [ "$go" = 1 ]; then
    local open head n
    open="$(gh pr list -R "$REPO" -l maint:bump --state open --json number,headRefName -q '.[] | "\(.number) \(.headRefName)"')"
    while read -r n head; do
      [ -z "$n" ] && continue
      if [ "$head" = "maint/omp-$latest" ]; then
        log "bump PR #$n for v$latest already open"
        go=0
      fi
    done <<< "$open"
    # A bump PR for this version that was closed unmerged is a rejection: do not reopen it every night.
    if [ "$go" = 1 ]; then
      n="$(gh pr list -R "$REPO" --state closed --head "maint/omp-$latest" --json number,mergedAt -q '[.[] | select(.mergedAt == null)][0].number // empty')"
      if [ -n "$n" ]; then
        log "bump PR #$n for v$latest was closed unmerged; skipping until a newer omp release"
        go=0
      fi
    fi
    if [ "$go" = 1 ]; then
      while read -r n head; do
        [ -z "$n" ] && continue
        ghw pr close "$n" -R "$REPO" --comment "Superseded by v$latest" --delete-branch
      done <<< "$open"
    fi
  fi
  # Run in a background subshell so its failure (ERR trap, set -e) is reported but must not stop
  # the publish below: a broken bump must never block the nightly build of main.
  if [ "$go" = 1 ]; then
    rc=0; ( bump_flow "$pinned" "$latest" ) & wait $! || rc=$?
    [ -f "$RUN/result" ] && result="$(cat "$RUN/result")"
    if [ "$rc" != 0 ]; then result=bump-error; log "bump flow failed (exit $rc); publishing main anyway"; fi
  fi

  publish
  log "maint: result=$result published=$published spend=\$$(run_spend)"
}

# bump_flow <pinned> <latest>; sets result
bump_flow() {
  local pinned="$1" latest="$2" branch="maint/omp-$2" bwt="$MAINT/wt/bump" rc
  worktree bump origin/main "$branch"
  rc=0; "$here/bump.sh" "$bwt" "$latest" > "$RUN/bump.log" 2>&1 || rc=$?
  if [ "$rc" = 75 ]; then
    log "npm package not published yet; retry next run"
    result=waiting-npm; echo "$result" > "$RUN/result"
    return 0
  elif [ "$rc" != 0 ]; then
    tail -n 50 "$RUN/bump.log" >&2
    return "$rc"
  fi
  git -C "$bwt" add -A
  git -C "$bwt" commit -q -m "Bump oh-my-pi to v$latest"

  mkdir -p "$RUN/bump"
  local green=0
  "$here/check.sh" "$bwt" "$RUN/bump" 9>&- > "$RUN/bump/check.out" 2>&1 && green=1

  "$here/material.sh" "$pinned" "$latest" "$RUN/material"
  local tprompt="$RUN/triage-prompt.md" tout="$RUN/triage.md" change=true summary="triage unavailable" risk=unknown
  cat > "$tprompt" <<P
Triage the oh-my-pi upgrade v$pinned -> v$latest for mu. The material directory is $RUN/material. The mu checkout with the bump applied is the current directory.
P
  rc=0; run_agent maint-triage "$MU_MAINT_TRIAGE_USD" "$bwt" "$tprompt" "$tout" || rc=$?
  if [ "$rc" = 0 ]; then
    local lastline
    lastline="$(grep -v '^[[:space:]]*$' "$tout" | tail -n 1)"
    if echo "$lastline" | jq -e '.behaviour_change | type == "boolean"' >/dev/null 2>&1; then
      change="$(echo "$lastline" | jq -r '.behaviour_change')"
      summary="$(echo "$lastline" | jq -r '.summary // ""')"
      risk="$(echo "$lastline" | jq -r '.risk // "unknown"')"
    fi
  fi

  local labels sev fixout=""
  if [ "$green" = 1 ] && [ "$change" = false ]; then
    result=s2; sev="S2 — green bump, no behaviour change; auto-merged"; labels=(maint:bump maint:auto)
  elif [ "$green" = 1 ]; then
    result=s2r; sev="S2r — green bump, behaviour change; needs review"; labels=(maint:bump maint:needs-review)
  else
    result=s1; sev="S1 — bump fails checks"; labels=(maint:bump maint:needs-review)
    local fprompt="$RUN/fix-prompt.md" fout="$RUN/fixer.md"
    cat > "$fprompt" <<P
Adapt mu to oh-my-pi v$latest (bumped from v$pinned). The failing check run is in $RUN/bump (check.txt and one log per stage). Upstream change material is in $RUN/material. The pinned omp for contract tests is at $MAINT/omp/$latest/omp (set MU_OMP to it). Work in the current directory.
P
    rc=0; run_agent maint-fixer "$MU_MAINT_FIX_USD" "$bwt" "$fprompt" "$fout" || rc=$?
    fixout="$(cat "$fout" 2>/dev/null || true)"
    if [ "$rc" = 0 ] && tree_changed "$bwt"; then
      git -C "$bwt" add -A
      git -C "$bwt" commit -q -m "Adapt to oh-my-pi v$latest" -m "$fixout"
      mkdir -p "$RUN/bump-fixed"
      if "$here/check.sh" "$bwt" "$RUN/bump-fixed" 9>&- > "$RUN/bump-fixed/check.out" 2>&1; then
        green=1
      fi
      cp "$RUN/bump-fixed/check.txt" "$RUN/bump/check.txt"
    fi
    [ "$green" = 1 ] || labels+=(maint:red)
  fi

  {
    echo "## Severity"; echo "$sev"; echo
    echo "## Checks"; echo '```'; cat "$RUN/bump/check.txt"; echo '```'; echo
    echo "## Upstream changes"; echo "$summary"; echo; echo "Risk: $risk"; echo
    if [ -n "$fixout" ]; then echo "## Fixer"; echo "$fixout"; echo; fi
    echo "## Agent spend"; echo '```'; run_spend_rows; echo '```'
  } > "$RUN/pr-body.md"

  echo "$result" > "$RUN/result"
  pushw "$bwt" push -f -u origin "$branch"
  pr_create "$branch" "Bump oh-my-pi to v$latest" "$RUN/pr-body.md" "${labels[@]}"
  if [ "$result" = s2 ]; then
    ghw pr merge "$branch" -R "$REPO" --squash --delete-branch
  fi
}

publish() {
  local sha sha7 last tag wt="$MAINT/wt/main"
  git -C "$CLONE" fetch -q origin
  sha="$(git -C "$CLONE" rev-parse origin/main)"
  sha7="${sha:0:7}"
  last="$(gh release list -R "$REPO" -L 1 --json tagName -q '.[0].tagName' 2>/dev/null || true)"
  if [ -n "$last" ] && [[ "$last" == *-"$sha7" ]]; then
    log "release $last already covers ${sha7}"
    return 0
  fi
  if [ "$sha" != "$checked_sha" ]; then
    worktree main origin/main
    mkdir -p "$RUN/publish"
    if ! "$here/check.sh" "$wt" "$RUN/publish" 9>&- > "$RUN/publish/check.out" 2>&1; then
      log "origin/main ${sha7} is red; not publishing"
      return 0
    fi
    checked_sha="$sha"
  fi
  tag="mu-$(date +%Y%m%d)-$sha7"
  pushw "$CLONE" tag "$tag" origin/main
  pushw "$CLONE" push origin "$tag"
  published="$tag"
  publishw "$wt/scripts/install.sh" --from-source "$wt" --skip-if-dev
}

digest() {
  local from to wt="$MAINT/wt/main" dir="$RUN/material" out="$RUN/digest.md" rc=0
  from="$(cat "$MAINT/digest-last")"
  to="$(latest_omp)"
  if [ "$from" = "$to" ]; then
    log "digest: omp still at v$to"
    return 0
  fi
  worktree main origin/main
  "$here/material.sh" "$from" "$to" "$dir"
  local prompt="$RUN/digest-prompt.md"
  cat > "$prompt" <<P
Write the parity digest for oh-my-pi v$from -> v$to. The material directory is $dir and the omp clone is $OMPSRC. Work in the current directory (the mu checkout).
P
  run_agent maint-digest "$MU_MAINT_DIGEST_USD" "$wt" "$prompt" "$out" || rc=$?
  if [ "$rc" != 0 ]; then
    log "digest agent failed (exit $rc)"
    return 0
  fi
  if [ "$(tr -d '[:space:]' < "$out")" = NOTHING ]; then
    log "digest: NOTHING"
  else
    ghw issue create -R "$REPO" --label maint:digest --title "omp parity digest v$from → v$to" --body-file "$out"
  fi
  if ! dry; then echo "$to" > "$MAINT/digest-last"; fi
  log "maint: digest v$from -> v$to spend=\$$(run_spend)"
}

case "$MODE" in
  nightly) nightly ;;
  digest) digest ;;
  *) echo "unknown mode: $MODE" >&2; exit 2 ;;
esac
