#!/usr/bin/env bash
# Usage: test-only.sh <worktree>   Exit 0 when the changes staged against HEAD touch only tests or docs.
# It gates an unreviewed merge, so anything it cannot place counts as "not test-only" (exit 1, reasons on stderr).
# A change that removes a test, an assertion or adds an #[ignore] does not count as test-only either.
set -euo pipefail
wt="$(cd "${1:?usage: test-only.sh <worktree>}" && pwd)"
cd "$wt"

# Reads a Rust file; prints the line of the last top-level `#[cfg(test)] mod x {` when that block runs to the end
# of the file, else nothing.
test_mod_start() {
  awk '
    prev ~ /^#\[cfg\(test\)\][[:space:]]*$/ && /^(pub(\([a-z]+\))? )?mod [a-z0-9_]+ \{[[:space:]]*$/ { start = NR - 1; endl = 0 }
    start && !endl && /^\}/ { endl = NR }
    /[^[:space:]]/ { last = NR }
    { prev = $0 }
    END { if (start && endl == last) print start }
  '
}

# rs_ok <path>: every changed line lies inside the trailing test module, in the old file and in the new one.
rs_ok() {
  local f="$1" old new
  old="$(git show "HEAD:$f" | test_mod_start)"
  new="$(git show ":$f" | test_mod_start)"
  [ -n "$old" ] && [ -n "$new" ] || return 1
  git diff --cached -U0 HEAD -- "$f" | awk -v old="$old" -v new="$new" '
    /^@@ / {
      split($2, a, ","); split($3, b, ",")
      os = substr(a[1], 2) + 0; oc = (a[2] == "" ? 1 : a[2] + 0)
      ns = substr(b[1], 2) + 0; nc = (b[2] == "" ? 1 : b[2] + 0)
      if (oc > 0 && os <= old) bad = 1
      if (nc > 0 && ns <= new) bad = 1
    }
    END { exit bad }
  '
}

# A diff that deletes a test, drops an assertion or ignores a test has the same shape as one that adds
# tests, so counts decide. They do not catch an assertion swapped for a weaker one; review does.
TEST_RE='^[[:space:]]*#\[([a-z_]+::)*test(\(|\])'
ASSERT_RE='(^|[^a-z_])(debug_)?assert[a-z_]*!'
IGNORE_RE='^[[:space:]]*#\[ignore'
count() { git show "$1" | grep -cE "$2" || true; }

# not_weaker <path>: the new file has no fewer tests or assertions than HEAD's, and no more ignored tests.
not_weaker() {
  local f="$1" what re
  for what in test assert; do
    case "$what" in test) re="$TEST_RE" ;; assert) re="$ASSERT_RE" ;; esac
    if [ "$(count ":$f" "$re")" -lt "$(count "HEAD:$f" "$re")" ]; then
      echo "weakened: $f has fewer ${what}s" >&2
      return 1
    fi
  done
  if [ "$(count ":$f" "$IGNORE_RE")" -gt "$(count "HEAD:$f" "$IGNORE_RE")" ]; then
    echo "weakened: $f ignores a test" >&2
    return 1
  fi
}

n=0
bad=0
while IFS=$'\t' read -r st f; do
  [ -n "$st" ] || continue
  n=$((n + 1))
  ok=0
  if [ "$st" = A ] || [ "$st" = M ]; then
    case "$f" in
      README.md | docs/*) ok=1 ;;
      */tests/* | tests/* | *_tests.rs | */testdata/*) ok=1 ;;
      *.rs) if [ "$st" = M ] && rs_ok "$f"; then ok=1; fi ;;
    esac
    if [ "$ok" = 1 ] && [ "$st" = M ] && [[ "$f" == *.rs ]] && ! not_weaker "$f"; then ok=0; fi
  fi
  if [ "$ok" = 0 ]; then
    echo "not test-only: $st $f" >&2
    bad=1
  fi
done < <(git diff --cached --no-renames --name-status HEAD)

if [ "$n" = 0 ]; then
  echo "not test-only: nothing staged" >&2
  exit 1
fi
exit "$bad"
