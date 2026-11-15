# Worker image for scripts/remote/mu-remote-pod: pinned toolchain, git, shellcheck, sccache, a release `mu` and the
# crates already fetched. The build context is the repository at one commit, streamed as a tar:
#   git archive --format=tar HEAD | docker build -t <registry>/mu-worker:<tag> -f scripts/remote/Dockerfile -
# No secrets are baked in. sccache's bucket comes from SCCACHE_* variables at run time (mu-remote-pod forwards the
# client's own and, on a trusted remote, reads a Kubernetes secret for the key).
FROM debian:bookworm-slim

ARG SCCACHE_VERSION=0.19.0

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUSTC_WRAPPER=sccache \
    CARGO_INCREMENTAL=0

RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential ca-certificates curl git libssl-dev pkg-config procps shellcheck \
 && rm -rf /var/lib/apt/lists/*

# The worker runs as an unprivileged user, so the image builds as that user too (a later chown would copy every
# file into a new layer): cargo needs to write its registry cache, and a root-run test suite hides permission bugs.
RUN useradd --uid 1000 --create-home --shell /bin/sh worker \
 && mkdir /src /usr/local/cargo /usr/local/rustup \
 && chown worker:worker /src /usr/local/cargo /usr/local/rustup
USER worker
WORKDIR /src
COPY --chown=worker:worker . /src

# rustup reads rust-toolchain.toml from /src, so the pinned channel and components are what gets installed.
# RUSTC_WRAPPER is unset for the build steps: there is no bucket at build time.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
 && rustup toolchain install \
 && RUSTC_WRAPPER= cargo install sccache --locked --version "$SCCACHE_VERSION" \
 && RUSTC_WRAPPER= cargo fetch --locked \
 && RUSTC_WRAPPER= cargo build --release --locked \
 && install -m 0755 target/release/mu /usr/local/cargo/bin/mu \
 && rm -rf /src/target

WORKDIR /
RUN find /src -mindepth 1 -delete
CMD ["sleep", "infinity"]
