#!/usr/bin/env bash
# clippy's driver as a rustc wrapper, so workspace crates are linted and code-generated in one rustc
# run: ci-checks.sh builds the tests through it instead of running `cargo clippy` and then `cargo test`,
# which compile the dependency tree twice (55 s of a cold run, 137 of 470 units).
#
# Cargo records the wrapper's path in the fingerprint, so changing the flags below needs a fresh target
# directory (or `touch` of the crate) to take effect. The wrapper does not stop plain `cargo build` from
# reusing what it produced, but a plain build does make the next wrapped one recompile the workspace.
#
# Lint levels live in [workspace.lints] in Cargo.toml, so an editor and a plain `cargo clippy` see the
# same policy; -Dwarnings only promotes warnings to errors here.
exec clippy-driver "$@" -Dwarnings
