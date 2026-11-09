# mu

Rust coding agent. Toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml`.

CI (`.github/workflows/ci.yml`) runs `scripts/ci-checks.sh` before pushing: fmt check, clippy `-D warnings`, the workspace tests and the MCP bridge's tests. The catalog generator's test is a bump-time guard and runs in `scripts/maint/check.sh` instead. Run the same before pushing:

```sh
scripts/ci-checks.sh
```

Never report a `-p <crate>` failure as pre-existing: run the script, or fix it.

A full run is ~90 s with sccache warm (~240 s with its cache empty), almost all of it compiling; the last stretch of that runs on one core because the heaviest crates are a chain (`mu-tools` -> `mu-agent` -> `mu-tui`), so the box looks idle while it works. Every worktree builds into its own `target/`: cargo keys artifacts by workspace-relative paths, so a target dir shared between worktrees hands one branch's crates to another (the build then fails on a field only the other branch has, or silently reuses a stale crate). Instead, `scripts/start.sh` writes `rustc-wrapper = "sccache"` and `incremental = false` into the new worktree's git-ignored `.cargo/config.toml`: compiled crates come from sccache's cache (`~/.cache/sccache`) rather than from a neighbour's target dir, which brings a fresh worktree's full check run to ~90 s cold instead of 240 s. `incremental = false` is what makes that work (sccache cannot cache incremental builds) and costs a few seconds on an edit rebuild; without sccache installed a worktree builds incrementally as before. Worktrees made before this change still have a `target` symlink into `.mu-target`: `rm` the symlink there, and delete `.mu-target` once no worktree points at it.

PR checks run on the PR merged with `main`; if CI fails on files you didn't touch, merge `origin/main`, fix, and push.

Release builds also run the tests on macOS (darwin-arm64), but `ci.yml` only runs on Linux, so a macOS-only failure first shows up when a release is tagged. Be careful about platform specifics: `/proc`, `/sys`, `readlink -f`, GNU `sed`/`date`/`stat` flags, `/tmp` vs `$TMPDIR`, case-insensitive filesystems and Linux-only syscalls. mu supports Linux and macOS equally: use portable APIs, and where the platforms differ, implement both rather than skipping one.

Enable the pre-commit fmt check once per clone: `git config core.hooksPath .githooks`.
