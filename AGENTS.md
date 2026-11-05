# mu

Rust coding agent. Toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml`.

CI (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`, tests and the plugin scripts. Run the same before pushing:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
cargo test --workspace --no-default-features --locked
```

The last two build without the default features (the tree-sitter `lang-*` highlighting); code and tests that need a feature must be gated on it (`#[cfg(feature = "lang-rust")]`, `cfg_attr(..., allow(dead_code))`). Never report a `-p <crate>` failure as pre-existing: run the commands above, or fix it.

PR checks run on the PR merged with `main`; if CI fails on files you didn't touch, merge `origin/main`, fix, and push.

Release builds also run the tests on macOS (darwin-arm64), but `ci.yml` only runs on Linux, so a macOS-only failure first shows up when a release is tagged. Be careful about platform specifics: `/proc`, `/sys`, `readlink -f`, GNU `sed`/`date`/`stat` flags, `/tmp` vs `$TMPDIR`, case-insensitive filesystems and Linux-only syscalls. mu supports Linux and macOS equally: use portable APIs, and where the platforms differ, implement both rather than skipping one.

Enable the pre-commit fmt check once per clone: `git config core.hooksPath .githooks`.
