# mu

Rust coding agent. Toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml`.

CI (`.github/workflows/ci.yml`) runs `scripts/ci-checks.sh` before pushing: fmt check, clippy `-D warnings`, the workspace tests, the MCP bridge's tests and the catalog generator's tests. Run the same before pushing:

```sh
scripts/ci-checks.sh
```

Never report a `-p <crate>` failure as pre-existing: run the script, or fix it.

A full run is ~130 s cold, almost all of it compiling; the last stretch of that runs on one core because the heaviest crates are a chain (`mu-tools` -> `mu-agent` -> `mu-tui`), so the box looks idle while it works. Worktrees from `scripts/start.sh` share one target cache (`.mu-target` next to the main checkout), which brings that to ~40 s for the first run in a new worktree and ~20 s for later ones. Each worktree that builds adds its own copy of the workspace's artifacts, and `scripts/start.sh` wipes the cache with `cargo clean` once it passes 20 GB (`MU_TARGET_CACHE_MAX_GB`).

PR checks run on the PR merged with `main`; if CI fails on files you didn't touch, merge `origin/main`, fix, and push.

Release builds also run the tests on macOS (darwin-arm64), but `ci.yml` only runs on Linux, so a macOS-only failure first shows up when a release is tagged. Be careful about platform specifics: `/proc`, `/sys`, `readlink -f`, GNU `sed`/`date`/`stat` flags, `/tmp` vs `$TMPDIR`, case-insensitive filesystems and Linux-only syscalls. mu supports Linux and macOS equally: use portable APIs, and where the platforms differ, implement both rather than skipping one.

Enable the pre-commit fmt check once per clone: `git config core.hooksPath .githooks`.
