# mu

Rust coding agent. Toolchain is pinned in `rust-toolchain.toml`; formatting in `rustfmt.toml` (max_width 120).

## Before every commit

CI (`.github/workflows/ci.yml`) is strict. Run what it runs, in this order, and fix before committing:

```sh
rustup component add rustfmt clippy   # the pinned toolchain may lack them; fmt/clippy then fail to run, not fail loudly
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- Never hand-format long one-liners; run `cargo fmt --all` after any edit to `.rs` files, including test code and `mod` lists (rustfmt sorts them).
- If you touched `plugins/memory`, `plugins/mcp-http` or their scripts: `scripts/build-mcp-http.sh test && scripts/build-mcp-http.sh check`, and `bun test content.test.ts` in `plugins/memory`.
- PR checks run on the PR merged with `main`. If CI fails on files you didn't touch, `main` is broken or ahead: merge `origin/main`, run the commands above, and push.

## Repo hygiene

- `*.bun-build` files are git-ignored build debris.
- Prefer `git add <paths>` over `git commit -a`.
