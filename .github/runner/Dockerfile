# ARC runner image for mu's CI: the upstream runner, plus a linker, Rust and `gh`
# (the release job publishes with it; a hosted runner used to supply it).
#
# Build from the repo root, so the toolchain comes from rust-toolchain.toml and
# cannot drift from the one jobs use:
#
#   docker build -f .github/runner/Dockerfile -t <registry>/arc-mu-runner:latest .
# arm64 (offload's arm64 base): build the same on an arm64 host and tag it arc-mu-runner-arm64:latest.
#
# The base is the same Ubuntu 24.04 as the hosted ubuntu-24.04 runner.
FROM ghcr.io/actions/actions-runner:latest

USER root
# No -sys crate in Cargo.lock links OpenSSL or needs cmake, so cc, ld and
# pkg-config are the whole native toolchain.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config gh \
    && rm -rf /var/lib/apt/lists/*

USER runner
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
ENV PATH=/home/runner/.cargo/bin:$PATH

WORKDIR /home/runner
# rustup reads this, then it goes, so the image has no default toolchain and a
# job's checkout supplies the pinned one.
COPY --chown=runner:runner rust-toolchain.toml .
RUN rustup toolchain install \
    && rustup component add rustfmt clippy \
    && rm rust-toolchain.toml
