#!/usr/bin/env bash
# Ships the current branch to origin/main: fetch, rebase, CI checks, push, install, remove the worktree.
# Run from a worktree on a branch, or from the main checkout on main. Commit (and squash) first.
# Usage: ship.sh [--no-install] [--keep] [--no-checks]
set -euo pipefail
install=1 keep=0 checks=1
while [ $# -gt 0 ]; do
  case "$1" in
    --no-install) install=0 ;;
    --keep) keep=1 ;;
    --no-checks) checks=0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf 'ship: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

top="$(git rev-parse --show-toplevel)"
cd "$top"
main_dir="$(cd "$(git rev-parse --git-common-dir)/.." && pwd)"
branch="$(git symbolic-ref --quiet --short HEAD)" || die "detached HEAD; check out a branch"
[ -z "$(git status --porcelain)" ] || die "uncommitted changes in $top; commit them first"

ci_checks() {
  [ "$checks" = 1 ] || return 0
  scripts/ci-checks.sh || die "checks failed"
}

# Another agent may push while the checks run: refetch and redo, a few times.
shipped=0
for attempt in 1 2 3; do
  git fetch --quiet origin
  if ! git merge-base --is-ancestor origin/main HEAD; then
    say "rebasing $branch onto origin/main"
    if ! git rebase --quiet origin/main; then
      git rebase --abort
      die "rebase onto origin/main conflicts; resolve by hand (git rebase origin/main)"
    fi
  fi
  if [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ]; then
    say "nothing to push: $branch is at origin/main"
    shipped=1
    break
  fi
  ci_checks
  git fetch --quiet origin
  if ! git merge-base --is-ancestor origin/main HEAD; then
    say "origin/main moved during the checks (attempt $attempt)"
    continue
  fi
  if git push --quiet origin HEAD:main; then
    say "pushed $(git rev-parse --short HEAD) to origin/main"
    shipped=1
    break
  fi
  say "push rejected (attempt $attempt)"
done
[ "$shipped" = 1 ] || die "gave up after 3 attempts"

git fetch --quiet origin
if [ "$main_dir" != "$top" ]; then
  if [ "$(git -C "$main_dir" symbolic-ref --quiet --short HEAD)" = main ] \
    && git -C "$main_dir" merge --quiet --ff-only origin/main; then
    say "fast-forwarded main in $main_dir"
  else
    say "main in $main_dir not updated (not on main, or diverged)"
  fi
elif [ "$branch" = main ]; then
  git merge --quiet --ff-only origin/main
fi

if [ "$install" = 1 ]; then
  "$main_dir/scripts/install.sh" --from-source "$main_dir"
fi

if [ "$keep" = 0 ] && [ "$main_dir" != "$top" ] && [ "$branch" != main ]; then
  [ -z "$(git cherry origin/main "$branch" | grep '^+' || true)" ] || die "$branch has commits not on origin/main; kept"
  cd "$main_dir"
  git worktree remove "$top"
  git branch --quiet -d "$branch" 2>/dev/null || git branch --quiet -D "$branch"
  say "removed worktree $top and branch $branch"
fi
