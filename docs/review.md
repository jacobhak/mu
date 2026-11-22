# Headless review: `mu review`

`mu review` runs `/review` headlessly, for detached or scripted runs: the same request and `review-lead`, with no TUI and no session. `--base` defaults to `HEAD` (uncommitted changes); `--plan` is the approved plan file. For a large diff the lead splits it between ordinary `scout` subagents, which run on the `scout` model role like any other scout; only the lead uses the `review` role.

It prints the verdict as JSON (`verdict`: `pass`, `fail`, `nothing` or `error`; `findings` with a `blocking` flag per finding) to stdout or `--out`, and progress to stderr. Exit codes: 0 pass or nothing to review, 1 fixes needed, 2 the review did not complete. The reviewer runs with the configured `review` role.

In the TUI, `/review` opens the report in the review dialog with the actions to send the blocking or all findings, choose findings, or dismiss; `n` adds a note that goes to the implementer with them.
