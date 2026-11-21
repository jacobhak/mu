# mu

Rust coding agent. Toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml`.

CI (`.github/workflows/ci.yml`) runs `scripts/ci-checks.sh` before pushing: fmt check, shellcheck (warnings and above, over every tracked shell script; skipped with a note when it is not installed, a failure when `MU_REQUIRE_SHELLCHECK=1`, which the Linux release build sets), cargo-deny (`licenses bans sources` from `deny.toml`; skipped with a note when not installed, a failure when `MU_REQUIRE_CARGO_DENY=1`; run `cargo deny check advisories` by hand now and then), clippy `-D warnings`, the workspace tests and the MCP bridge's tests. The release workflow and the maintenance bot's `scripts/maint/check.sh` run the same script, so every gate runs one list. The catalog generator's test is a bump-time guard and runs in `scripts/maint/check.sh` instead. Run the same before pushing:

```sh
scripts/ci-checks.sh
```

Never report a `-p <crate>` failure as pre-existing: run the script, or fix it.

A full run is ~90 s with sccache warm (~240 s with its cache empty), almost all of it compiling; the last stretch of that runs on one core because the heaviest crates are a chain (`mu-tools` -> `mu-agent` -> `mu-tui`), so the box looks idle while it works. Every worktree builds into its own `target/`: cargo keys artifacts by workspace-relative paths, so a target dir shared between worktrees hands one branch's crates to another (the build then fails on a field only the other branch has, or silently reuses a stale crate). Instead, `scripts/start.sh` writes `rustc-wrapper = "sccache"` and `incremental = false` into the new worktree's git-ignored `.cargo/local.toml`, which the tracked `.cargo/config.toml` includes: compiled crates come from sccache's cache (`~/.cache/sccache`) rather than from a neighbour's target dir, which brings a fresh worktree's full check run to ~90 s cold instead of 240 s. `incremental = false` is what makes that work (sccache cannot cache incremental builds) and costs a few seconds on an edit rebuild; without sccache installed a worktree builds incrementally as before. A worktree made before `.cargo/config.toml` was tracked has those settings in an untracked `.cargo/config.toml` that blocks a rebase: `mv .cargo/config.toml .cargo/local.toml` there first. Worktrees older still have a `target` symlink into `.mu-target`: `rm` the symlink there, and delete `.mu-target` once no worktree points at it.

PR checks run on the PR merged with `main`; if CI fails on files you didn't touch, merge `origin/main`, fix, and push.

`ci.yml` has a `macos` job on the self-hosted Mac as well as the Linux one, and release builds run `scripts/ci-checks.sh` on both platforms. Nothing gates a merge on either job (branch protection needs a paid plan on a private repo), so a red job is a signal to act on, not a block. Be careful about platform specifics: `/proc`, `/sys`, `readlink -f`, GNU `sed`/`date`/`stat` flags, `/tmp` vs `$TMPDIR`, case-insensitive filesystems and Linux-only syscalls. mu supports Linux and macOS equally: use portable APIs, and where the platforms differ, implement both rather than skipping one.

Enable the pre-commit fmt check once per clone: `git config core.hooksPath .githooks`.
