# Worker image for scripts/remote/pod-run.sh: pinned toolchain, git, shellcheck, sccache, a release `mu` and the
# crates already fetched. The build context is the repository at one commit, streamed as a tar:
#   git archive --format=tar HEAD | docker build -t <registry>/mu-worker:<tag> -f scripts/remote/Dockerfile -
# No secrets are baked in. sccache's bucket comes from SCCACHE_* variables at run time (pod-run.sh forwards the
# launcher's own and reads a Kubernetes secret for the key).
FROM debian:bookworm-slim

ARG SCCACHE_VERSION=0.8.2

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUSTC_WRAPPER=sccache \
    CARGO_INCREMENTAL=0

RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential ca-certificates curl git pkg-config shellcheck \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . /src

# rustup reads rust-toolchain.toml from /src, so the pinned channel and components are what gets installed.
# RUSTC_WRAPPER is unset for the build steps: there is no bucket at build time.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
 && rustup toolchain install \
 && RUSTC_WRAPPER= cargo install sccache --locked --version "$SCCACHE_VERSION" \
 && RUSTC_WRAPPER= cargo fetch --locked \
 && RUSTC_WRAPPER= cargo build --release --locked \
 && install -m 0755 target/release/mu /usr/local/bin/mu \
 && rm -rf /src/target

WORKDIR /
RUN rm -rf /src
CMD ["sleep", "infinity"]
