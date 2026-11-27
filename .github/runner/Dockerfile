# ARC runner image for mu's CI: the upstream runner, plus a linker, Rust and `gh`
# (the release job publishes with it; a hosted runner used to supply it), and the tools
# scripts/ci-checks.sh would otherwise skip: shellcheck, jq, node and cargo-deny.
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
    && apt-get install -y --no-install-recommends build-essential pkg-config gh shellcheck jq nodejs \
    && rm -rf /var/lib/apt/lists/*

# cargo-deny has no apt package: the release binary, checked against its published digest.
ARG CARGO_DENY_VERSION=0.22.2
RUN case "$(uname -m)" in \
      x86_64) arch=x86_64; sha=9f831cfb5d03a5093c0810091a7bd3b5e4724a1288cbe5718d8ce8cef2590adf ;; \
      aarch64) arch=aarch64; sha=e35a08517fd126a360361b316af76fbbbb55e608ce765010a9b5a537851b80fd ;; \
      *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /tmp/cargo-deny.tar.gz \
      "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/cargo-deny-${CARGO_DENY_VERSION}-${arch}-unknown-linux-musl.tar.gz" \
    && echo "${sha}  /tmp/cargo-deny.tar.gz" | sha256sum --check --quiet \
    && tar -xzf /tmp/cargo-deny.tar.gz -C /usr/local/bin --strip-components=1 \
      "cargo-deny-${CARGO_DENY_VERSION}-${arch}-unknown-linux-musl/cargo-deny" \
    && rm /tmp/cargo-deny.tar.gz

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
