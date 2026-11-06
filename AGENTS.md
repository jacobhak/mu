# mu

Rust coding agent. Toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml`.

CI (`.github/workflows/ci.yml`) runs `scripts/ci-checks.sh` before pushing: fmt check, clippy `-D warnings`, the workspace tests, the MCP bridge's tests and the catalog generator's tests. Run the same before pushing:

```sh
scripts/ci-checks.sh
```

Never report a `-p <crate>` failure as pre-existing: run the script, or fix it.

`scripts/ship.sh [--no-install] [--keep] [--no-checks]` ships committed work to `main`: fetch and rebase onto `origin/main`, the checks above, push (redone when main moved meanwhile), install from source, then remove the worktree and branch. `/ship` (`.agents/commands/ship.md`) has the agent commit and squash first and fix what the script reports.

PR checks run on the PR merged with `main`; if CI fails on files you didn't touch, merge `origin/main`, fix, and push.

Release builds also run the tests on macOS (darwin-arm64), but `ci.yml` only runs on Linux, so a macOS-only failure first shows up when a release is tagged. Be careful about platform specifics: `/proc`, `/sys`, `readlink -f`, GNU `sed`/`date`/`stat` flags, `/tmp` vs `$TMPDIR`, case-insensitive filesystems and Linux-only syscalls. mu supports Linux and macOS equally: use portable APIs, and where the platforms differ, implement both rather than skipping one.

Enable the pre-commit fmt check once per clone: `git config core.hooksPath .githooks`.
