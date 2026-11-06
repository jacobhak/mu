---
description: Commit, squash and ship this branch to main (checks, push, install, remove worktree)
argument-hint: "[--no-install] [--keep] [--no-checks]"
---
Ship the current work to main.

1. Commit any uncommitted changes that belong to this work; ask me about files you are unsure of.
2. On a branch other than main with more than one commit since origin/main, squash them into one (`git reset --soft $(git merge-base HEAD origin/main)`) with a message describing the whole change, in the style of `git log`.
3. Run `scripts/ship.sh $@`. It fetches, rebases onto origin/main, runs fmt, clippy and tests, pushes, fast-forwards main in the main checkout, installs from source and removes the worktree and branch.
4. If it fails, fix the cause (rebase conflict, formatting, clippy, a failing test), amend it into the same commit and run it again. A test that passes on a rerun is flaky: say which. Never force-push or skip hooks.

Report the pushed commit and anything you had to fix.
