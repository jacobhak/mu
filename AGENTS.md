# mu

Rust coding agent. Toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml`.

CI (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`, tests and the plugin scripts. Run the same before pushing:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

PR checks run on the PR merged with `main`; if CI fails on files you didn't touch, merge `origin/main`, fix, and push.

Enable the pre-commit fmt check once per clone: `git config core.hooksPath .githooks`.
