#!/usr/bin/env bash
# Starts new work: a worktree on a new branch from the latest origin/main, next to the main checkout.
# Usage: start.sh <branch> [--base <ref>]
# Prints the worktree path on stdout. The directory is <repo>-<branch>, with any "type/" prefix dropped
# (fix/plugin-reinstall -> ../mu-plugin-reinstall) and other slashes turned into dashes.
#
# Every worktree builds into its own target/: cargo keys artifacts by workspace-relative paths, so one
# target dir shared between worktrees hands one branch's crates to another, and a build then fails on a
# field only the other branch has (or silently reuses a stale crate). sccache keeps a fresh worktree
# fast instead: it caches compiled crates by their inputs, so only the worktree's own are rebuilt.
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
# The worktree's own .cargo/config.toml, which .gitignore keeps out of commits: the only place a build
# here picks up its compiler wrapper.
sccache_ok=0
if command -v sccache >/dev/null 2>&1; then
  sccache_ok=1
  mkdir -p "$dir/.cargo"
  # incremental = false, because sccache cannot cache incremental builds: with it on a fresh worktree
  # compiled from scratch (214 s against 89 s), and the price here is a few seconds per edit rebuild.
  printf '[build]\nrustc-wrapper = "sccache"\nincremental = false\n' > "$dir/.cargo/config.toml"
fi
say "created $dir on $branch at $(git rev-parse --short "$base") ($base)"
if [ "$sccache_ok" = 1 ]; then
  say "builds here use sccache; target/ is the worktree's own"
else
  say "sccache is not installed, so the first build here is cold (cargo install sccache --locked)"
fi
printf '%s\n' "$dir"
