# Headless review: `mu review`

`mu review` runs `/review` headlessly, for detached or scripted runs: the same request, `review-lead` and workers, with no TUI and no session. `--base` defaults to `HEAD` (uncommitted changes); `--plan` is the approved plan file.

It prints the verdict as JSON (`verdict`: `pass`, `fail`, `nothing` or `error`; `findings` with a `blocking` flag per finding) to stdout or `--out`, and progress to stderr. Exit codes: 0 pass or nothing to review, 1 fixes needed, 2 the review did not complete. The reviewer runs with the configured `review` role.
