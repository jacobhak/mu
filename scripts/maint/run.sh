#!/usr/bin/env bash
# Usage: run.sh nightly|digest
# nightly: baseline check (S0), omp and pi bump (S1/S2/S2r), publish. digest: weekly parity digest (S3).
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
pinned_pi() { git -C "$CLONE" show origin/main:upstream.toml | sed -n 's/^pi = "\(.*\)"/\1/p'; }
latest_pi() { gh release view -R earendil-works/pi --json tagName -q .tagName | sed 's/^v//'; }
# newer <pinned> <latest> <forced>: 0 when <latest> is set, differs and is newer (or forced).
newer() {
  [ -n "$2" ] && [ "$2" != "$1" ] || return 1
  [ -n "$3" ] || [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$2" ]
}
# bump_what <omp-old> <omp-new> <pi-old> <pi-new>: "oh-my-pi to v1", "pi to v2" or both, for titles.
bump_what() {
  local w=()
  [ "$1" = "$2" ] || w+=("oh-my-pi to v$2")
  [ "$3" = "$4" ] || w+=("pi to v$4")
  case "${#w[@]}" in 1) echo "${w[0]}" ;; *) echo "${w[0]} and ${w[1]}" ;; esac
}

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

nightly() {
  local result=uptodate published=no checked_sha pinned latest wt rc pi_pinned pi_latest

  # --- Baseline (S0)
  worktree main origin/main
  wt="$MAINT/wt/main"
  checked_sha="$(git -C "$wt" rev-parse HEAD)"
  mkdir -p "$RUN/main"
  log "baseline check of ${checked_sha:0:7}"
  if ! "$here/check.sh" "$wt" "$RUN/main" 9>&- > "$RUN/main/check.out" 2>&1; then
    result=s0
    log "baseline red"
    local body="$RUN/s0-issue.md" branch
    branch="maint/fix-main-$(date +%Y%m%d)"
    { echo "\`check.sh\` fails on origin/main (${checked_sha:0:7}) with the pinned omp."; echo; failing_tail "$RUN/main"; } > "$body"
    issue_upsert maint:s0 "S0: main is broken" "$body"
    worktree fix origin/main "$branch"
    local fixwt="$MAINT/wt/fix" prompt="$RUN/s0-prompt.md" fout="$RUN/s0-fixer.md"
    cat > "$prompt" <<P
Fix main. The check run is in $RUN/main (check.txt and one log per stage). The pinned omp for contract tests is at $MAINT/omp/$(pinned_ver)/omp (set MU_GATEWAY_OMP to it), and the pinned pi at $MAINT/pi/$(pinned_pi)/pi (set MU_GATEWAY_PI to it). Work in the current directory.
P
    rc=0; run_agent maint-fixer "$MU_MAINT_FIX_USD" "$fixwt" "$prompt" "$fout" || rc=$?
    local green=0
    if [ "$rc" = 0 ] && tree_changed "$fixwt"; then
      mkdir -p "$RUN/fix"
      if "$here/check.sh" "$fixwt" "$RUN/fix" 9>&- > "$RUN/fix/check.out" 2>&1; then green=1; fi
    fi
    if [ "$green" = 1 ]; then
      local title
      title="Fix main: $(first_line "$fout")"
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
  pi_pinned="$(pinned_pi)"
  pi_latest="${MU_MAINT_FORCE_LATEST_PI:-$(latest_pi || true)}"
  local go=0
  # An upstream with nothing newer stays at its pin, so one PR can carry either bump or both.
  if newer "$pinned" "$latest" "${MU_MAINT_FORCE_LATEST:-}"; then go=1; else latest="$pinned"; fi
  if newer "$pi_pinned" "$pi_latest" "${MU_MAINT_FORCE_LATEST_PI:-}"; then go=1; else pi_latest="$pi_pinned"; fi
  # One bump PR at a time (omp and pi share it): a newer release rebuilds the open PR's branch instead of opening another.
  local pr="" branch="maint/omp-bump" had="" pi_had=""
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
      pi_had="$(git -C "$CLONE" show "origin/$branch:upstream.toml" 2>/dev/null | sed -n 's/^pi = "\(.*\)"/\1/p' || true)"
      if [ "$had" = "$latest" ] && [ "$pi_had" = "$pi_latest" ]; then
        log "bump PR #$pr for omp v$latest, pi v$pi_latest already open"
        go=0
      fi
    fi
  fi
  # Run in a background subshell so its failure (ERR trap, set -e) is reported but must not stop
  # the publish below: a broken bump must never block the nightly build of main.
  if [ "$go" = 1 ]; then
    rc=0; ( bump_flow "$pinned" "$latest" "$branch" "$pr" "$had" "$pi_pinned" "$pi_latest" "$pi_had" ) & wait $! || rc=$?
    [ -f "$RUN/result" ] && result="$(cat "$RUN/result")"
    if [ "$rc" != 0 ]; then result=bump-error; log "bump flow failed (exit $rc); publishing main anyway"; fi
  fi

  publish
  log "maint: result=$result published=$published spend=\$$(run_spend)"
}

# ci_wait <branch>: waits for the branch's `check` job (the macos job gates nothing). Exit 0 when it passed,
# 1 when it ended any other way, 2 when it had not finished within MU_MAINT_CI_WAIT_SECS.
# The check-runs REST API, not `gh pr checks --json`: gh 2.46 (Ubuntu 24.04) has no --json there, and the
# error it printed was swallowed, so the wait always ran out and no bump ever auto-merged.
ci_wait() {
  local branch="$1" deadline sha state
  deadline=$((SECONDS + ${MU_MAINT_CI_WAIT_SECS:-3600}))
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

# bump_flow <pinned> <latest> <branch> [open-pr] [its-omp-version] <pi-pinned> <pi-latest> [its-pi-version]; writes $RUN/result
# An upstream whose latest equals its pin is not part of this bump.
bump_flow() {
  local pinned="$1" latest="$2" branch="$3" pr="${4:-}" had="${5:-}" pi_pinned="$6" pi_latest="$7" pi_had="${8:-}" bwt="$MAINT/wt/bump" rc pinfail=0
  local what bargs=(-)
  what="$(bump_what "$pinned" "$latest" "$pi_pinned" "$pi_latest")"
  [ "$latest" = "$pinned" ] || bargs=("$latest")
  if [ "$pi_latest" = "$pi_pinned" ]; then bargs+=(-); else bargs+=("$pi_latest"); fi
  worktree bump origin/main "$branch"
  rc=0; "$here/bump.sh" "$bwt" "${bargs[@]}" > "$RUN/bump.log" 2>&1 || rc=$?
  if [ "$rc" = 75 ]; then
    log "a release is not published yet; retry next run"
    result=waiting-release; echo "$result" > "$RUN/result"
    return 0
  elif [ "$rc" = 76 ]; then
    log "cargo cannot resolve the omp v$latest pin; handing it to the fixer"
    pinfail=1
  elif [ "$rc" != 0 ]; then
    tail -n 50 "$RUN/bump.log" >&2
    return "$rc"
  fi
  git -C "$bwt" add -A
  git -C "$bwt" commit -q -m "Bump $what"

  mkdir -p "$RUN/bump"
  local green=0 smokeonly=0
  if [ "$pinfail" = 1 ]; then
    printf 'bump FAILED\nbuild skipped\ntest skipped\ncontract skipped\nsmoke skipped\n' > "$RUN/bump/check.txt"
    cp "$RUN/bump.log" "$RUN/bump/bump.log"
  elif "$here/check.sh" "$bwt" "$RUN/bump" 9>&- > "$RUN/bump/check.out" 2>&1; then
    green=1
  elif [ "$(tr '\n' ' ' < "$RUN/bump/check.txt")" = "build ok test ok contract ok smoke FAILED " ]; then
    # The smoke stage drives a live model and flakes; nothing for the fixer to repair from a log alone.
    smokeonly=1
  fi

  "$here/material.sh" "$pinned" "$latest" "$RUN/material" "$pi_pinned" "$pi_latest"
  local tprompt="$RUN/triage-prompt.md" tout="$RUN/triage.md" change=true summary="triage unavailable" risk=unknown fyi=""
  cat > "$tprompt" <<P
Triage the upgrade ($what; from omp v$pinned, pi v$pi_pinned) for mu. The material directory is $RUN/material (an upstream that did not change has no material). The mu checkout with the bump applied is the current directory.
P
  rc=0; run_agent maint-triage "$MU_MAINT_TRIAGE_USD" "$bwt" "$tprompt" "$tout" || rc=$?
  if [ "$rc" = 0 ]; then
    local lastline
    # The last JSON-object line: a model may wrap it in a code fence, which then is the last line.
    lastline="$(grep -E '^[[:space:]]*\{.*\}[[:space:]]*$' "$tout" | tail -n 1)"
    if echo "$lastline" | jq -e '.behaviour_change | type == "boolean"' >/dev/null 2>&1; then
      change="$(echo "$lastline" | jq -r '.behaviour_change')"
      summary="$(echo "$lastline" | jq -r '.summary // ""')"
      risk="$(echo "$lastline" | jq -r '.risk // "unknown"')"
      fyi="$(echo "$lastline" | jq -r '.fyi // ""')"
    fi
  fi

  local labels sev fixout="" fixrc=0 edited=0 testonly=0 review=0 findings="$RUN/fixer-findings.md"
  local fprompt="$RUN/fix-prompt.md" fout="$RUN/fixer.md"
  if [ "$green" = 1 ] && [ "$change" = false ]; then
    result=s2; sev="S2 — green bump, nothing in mu needs to change; auto-merged"; labels=(maint:bump maint:auto)
  elif [ "$smokeonly" = 1 ]; then
    result=s1-smoke; sev="S1 — bump fails only the live smoke stage; the fixer was not run"; labels=(maint:bump maint:needs-review maint:red)
  else
    if [ "$green" = 1 ]; then
      result=s2r; review=1; sev="S2r — green bump, the release leaves something in mu wrong; needs review"; labels=(maint:bump maint:needs-review)
      cat > "$fprompt" <<P
MODE: behaviour review (checks are GREEN; do not go looking for failures). Triage found that these changes in the upstream bump ($what; from omp v$pinned, pi v$pi_pinned) leave something in mu wrong:

$summary

Upstream change material is in $RUN/material. The pinned omp for contract tests is at $MAINT/omp/$latest/omp (set MU_GATEWAY_OMP to it), the pinned pi at $MAINT/pi/$pi_latest/pi (set MU_GATEWAY_PI to it). Work in the current directory.

For each listed change, decide whether mu's own code, prompts, tool descriptions or docs are now wrong, stale or incomplete because of it, and fix those. If nothing in mu is, make no change: do not add a test that only pins upstream behaviour. Follow the final-message format for behaviour review in your agent instructions.
P
    else
      result=s1; sev="S1 — bump fails checks"; labels=(maint:bump maint:needs-review)
      [ "$pinfail" = 0 ] || sev="S1 — cargo cannot resolve the v$latest pin"
      cat > "$fprompt" <<P
Adapt mu to the upstream bump ($what; from omp v$pinned, pi v$pi_pinned). The failing check run is in $RUN/bump (check.txt and one log per stage). Upstream change material is in $RUN/material. The pinned omp for contract tests is at $MAINT/omp/$latest/omp (set MU_GATEWAY_OMP to it), the pinned pi at $MAINT/pi/$pi_latest/pi (set MU_GATEWAY_PI to it). Work in the current directory.
P
      if [ "$change" = true ]; then
        cat >> "$fprompt" <<P

Triage also found that these changes in the bump leave something in mu wrong, apart from the failing checks:

$summary

First make the checks pass. Then, for each listed change, decide whether mu's own code, prompts, tool descriptions or docs are wrong, stale or incomplete because of it, and fix those too (no test that only pins upstream behaviour). Report each in the final message as in the behaviour review format of your agent instructions, after the account of the build fix.
P
      fi
    fi
    rc=0; run_agent maint-fixer "$MU_MAINT_FIX_USD" "$bwt" "$fprompt" "$fout" || rc=$?
    fixrc=$rc
    : > "$RUN/fixer-body.md"; : > "$findings"
    [ ! -f "$fout" ] || awk -v b="$RUN/fixer-body.md" -v f="$findings" '/^## Findings[[:space:]]*$/ { inf = 1; next } { print > (inf ? f : b) }' "$fout"
    fixout="$(cat "$RUN/fixer-body.md")"
    if [ "$rc" = 0 ] && tree_changed "$bwt"; then
      edited=1
      if [ "$pinfail" = 1 ]; then
        # The first run stopped at cargo fetch: finish the pin (lockfile, catalog) on the fixer's manifest.
        rc=0; "$here/bump.sh" "$bwt" "${bargs[@]}" > "$RUN/bump/bump.log" 2>&1 || rc=$?
        if [ "$rc" = 0 ]; then pinfail=0; fi
      fi
      git -C "$bwt" add -A
      # $here, not the worktree's copy: the fixer must not be able to edit the gate that merges its own diff.
      if [ "$review" = 1 ] && "$here/test-only.sh" "$bwt" 2> "$RUN/test-only.log"; then testonly=1; fi
      git -C "$bwt" commit -q -m "Adapt to $what" -m "$fixout"
      if [ "$pinfail" = 0 ]; then
        mkdir -p "$RUN/bump-fixed"
        if "$here/check.sh" "$bwt" "$RUN/bump-fixed" 9>&- > "$RUN/bump-fixed/check.out" 2>&1; then
          green=1
        else
          green=0
        fi
        cp "$RUN/bump-fixed/check.txt" "$RUN/bump/check.txt"
      fi
    fi
    [ "$green" = 1 ] || labels+=(maint:red)
    if [ "$review" = 1 ] && [ "$green" = 1 ] && [ "$fixrc" = 0 ]; then
      if [ "$edited" = 0 ]; then
        result=s2; sev="S2 — green bump; the fixer reviewed the changes and found no mu change needed; auto-merged"; labels=(maint:bump maint:auto)
      elif [ "$testonly" = 1 ]; then
        result=s2; sev="S2 — green bump; the fixer changed only tests or docs; auto-merged"; labels=(maint:bump maint:auto)
      fi
    fi
  fi

  {
    echo "## Severity"; echo "$sev"; echo
    echo "## Checks"
    if [ "$smokeonly" = 1 ] || [ "$pinfail" = 1 ]; then failing_tail "$RUN/bump"; else echo '```'; cat "$RUN/bump/check.txt"; echo '```'; fi
    echo
    echo "## Upstream changes"; echo "$summary"; echo
    [ -z "$fyi" ] || { echo "## Also in this release (no mu change needed)"; echo "$fyi"; echo; }
    echo "Risk: $risk"; echo
    if [ "$fixrc" != 0 ]; then
      echo "## Fixer"; echo "**The fixer did not finish (exit $fixrc; 10 = monthly budget, 124 = timeout). No adaptation was made; any partial edits were discarded.**"; echo
      [ -z "$fixout" ] || { echo "Partial output:"; echo; echo "$fixout"; echo; }
    elif [ -n "$fixout" ]; then echo "## Fixer"; echo "$fixout"; echo; fi
    echo "## Agent spend"; echo '```'; run_spend_rows; echo '```'
  } > "$RUN/pr-body.md"

  echo "$result" > "$RUN/result"
  pushw "$bwt" push -f -u origin "$branch"
  local url
  if [ -n "$pr" ]; then
    pr_update "$pr" "Bump $what" "$RUN/pr-body.md" "${labels[@]}"
    ghw pr comment "$pr" -R "$REPO" --body "Rebuilt for $what${had:+ (omp was v$had${pi_had:+, pi was v$pi_had})}. The branch was reset to main, so earlier commits on it are gone."
    url="$(gh pr view "$pr" -R "$REPO" --json url -q .url)"
  else
    url="$(pr_create "$branch" "Bump $what" "$RUN/pr-body.md" "${labels[@]}")"
  fi
  [ -z "$url" ] || echo "$url"
  if [ -s "$findings" ]; then
    { echo "Found by the maintenance bot while reviewing the bump ($what) (${url:-$branch}). The bump did not cause it."; echo; cat "$findings"; } > "$RUN/finding.md"
    ghw issue create -R "$REPO" --label maint:finding --title "maint finding (found while bumping: $what)" --body-file "$RUN/finding.md"
  fi
  if [ "$result" = s2 ]; then
    # check.sh already ran the same list as CI, but on this machine only: the PR's own run is the second opinion.
    local ci=0
    dry || ci_wait "$branch" || ci=$?
    if [ "$ci" = 0 ]; then
      ghw pr merge "$branch" -R "$REPO" --squash --delete-branch
    else
      local why="the PR's CI run failed"
      [ "$ci" = 1 ] || why="the PR's CI run did not finish within ${MU_MAINT_CI_WAIT_SECS:-3600} s"
      log "bump PR $branch not merged: $why"
      result=s2-ci
      echo "$result" > "$RUN/result"
      local num="${pr:-${url##*/}}"
      ghw api --silent -X POST "repos/$REPO/issues/$num/labels" -f "labels[]=maint:needs-review"
      ghw api --silent -X DELETE "repos/$REPO/issues/$num/labels/maint:auto" 2> /dev/null || true
      ghw pr comment "$num" -R "$REPO" --body "Not auto-merged: $why. The local checks were green."
    fi
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
