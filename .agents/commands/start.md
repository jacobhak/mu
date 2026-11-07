---
description: Start new work in a fresh worktree on a new branch from the latest origin/main
argument-hint: "<what you are going to do>"
---
Do this task in a new worktree of its own: $@

1. Pick a short kebab-case branch name with a type prefix in the style of `git log` and existing branches (`fix/…`, `feat/…`, `docs/…`, `chore/…`). Ask me only if the task is too vague to name; with no task given, ask what to work on.
2. Run `scripts/start.sh <branch>`. It fetches, then adds a worktree next to the main checkout (`../mu-<name>`) on the new branch from `origin/main`, and prints the path. If it fails because the branch or directory exists, tell me and ask whether to reuse it; never remove it yourself.
3. Work in that worktree from now on: `cd` into it in the shell and use absolute paths under it for every read and edit. Never touch the main checkout.
4. Report the branch and path in one line, then carry out the task.
