# Contributing

Issues and pull requests are welcome. For anything larger than a small fix, open an
issue first so the direction can be agreed before you spend time on it.

## Build and check

The Rust toolchain (with rustfmt and clippy) is pinned in `rust-toolchain.toml`, and
formatting in `rustfmt.toml`. Before opening a pull request run the same checks CI runs:

```sh
scripts/ci-checks.sh
```

`scripts/ci-checks.sh` skips shellcheck and cargo-deny with a note when they are not
installed. To format on every commit, enable the hook once per clone:

```sh
git config core.hooksPath .githooks
```

mu supports Linux and macOS equally: use portable APIs, and where the platforms
differ, implement both.

## Pull requests from forks

CI for a fork's pull request runs on a GitHub-hosted runner (`check-fork`) after a
maintainer approves the run. The project's own build machines, including the macOS
job, run only for branches in this repository, so a macOS-specific failure may show
up only after a maintainer pushes your change to a branch here.

## Security issues

Report them privately; see `SECURITY.md`.
