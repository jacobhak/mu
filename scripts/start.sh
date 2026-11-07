#!/usr/bin/env bash
# Starts new work: a worktree on a new branch from the latest origin/main, next to the main checkout.
# Usage: start.sh <branch> [--base <ref>]
# Prints the worktree path on stdout. The directory is <repo>-<branch>, with any "type/" prefix dropped
# (fix/plugin-reinstall -> ../mu-plugin-reinstall) and other slashes turned into dashes.
#
# The worktree's target/ is a symlink into one cache shared by every worktree of this repository, so a
# check run in a fresh worktree reuses the previous one's artifacts (~160 s cold, ~15 s warm) instead of
# rebuilding them; concurrent builds queue on cargo's lock.
set -euo pipefail
branch="" base=origin/main
while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || { echo "start: --base needs a ref" >&2; exit 2; }; base="$2"; shift ;;
    -*) echo "start: unknown argument: $1" >&2; exit 2 ;;
    *) [ -z "$branch" ] || { echo "start: one branch name only" >&2; exit 2; }; branch="$1" ;;
  esac
  shift
done

say() { printf 'start: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

[ -n "$branch" ] || die "usage: start.sh <branch> [--base <ref>]"
git check-ref-format --branch "$branch" >/dev/null 2>&1 || die "invalid branch name: $branch"
[ "$branch" != main ] || die "pick a branch name other than main"

main_dir="$(cd "$(git rev-parse --git-common-dir)/.." && pwd)"
cd "$main_dir"

git fetch --quiet origin
git rev-parse --verify --quiet "$base^{commit}" >/dev/null || die "unknown base: $base"
! git show-ref --verify --quiet "refs/heads/$branch" || die "branch $branch already exists"

slug="${branch#*/}"
dir="$(dirname "$main_dir")/$(basename "$main_dir")-${slug//\//-}"
[ ! -e "$dir" ] || die "$dir already exists"

# --no-track: the branch must not inherit origin/main as upstream, or a bare `git push` would target main.
git worktree add --quiet --no-track -b "$branch" "$dir" "$base"
cache="$(dirname "$main_dir")/.$(basename "$main_dir")-target"
mkdir -p "$cache"
ln -s "$cache" "$dir/target"
say "created $dir on $branch at $(git rev-parse --short "$base") ($base)"
say "target/ is shared through $cache"
printf '%s\n' "$dir"
