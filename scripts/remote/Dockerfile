# Two images from one file, built from the repository at one commit streamed as a tar (always pass --target):
#   git archive --format=tar HEAD | docker build --target worker -t <registry>/mu-worker:<tag> -f scripts/remote/Dockerfile -
#   git archive --format=tar HEAD | docker build --target agent  -t <registry>/mu-agent:<tag>  -f scripts/remote/Dockerfile -
# `worker`: pinned toolchain, git, shellcheck, sccache, a release `mu` and the crates already fetched; it is what
# scripts/remote/mu-remote-pod starts for a task. `agent`: the worker plus what a main agent in a pod needs (omp for
# `/login`, kubectl and jq and the pod adapter for the agent's own workers, tini as PID 1); see docs/remote-kubernetes.md.
# Both run as uid 1000. No secrets are baked in. sccache's bucket comes from SCCACHE_* variables at run time
# (mu-remote-pod forwards the client's own and, on a trusted remote, reads a Kubernetes secret for the key).
# Build with a new tag each time: a reused tag is not pulled again by nodes that have it.
FROM debian:bookworm-slim AS worker

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

FROM worker AS agent
# The kubectl client may be one minor version off the API server's; override with --build-arg for another cluster.
ARG KUBECTL_VERSION=1.31.14
USER root
COPY scripts/fetch-omp.sh upstream.toml /tmp/mu-build/
COPY scripts/remote/mu-remote-pod scripts/remote/mu-remote-pod.yaml /usr/local/lib/mu/
RUN apt-get update \
 && apt-get install -y --no-install-recommends jq tini \
 && rm -rf /var/lib/apt/lists/* \
 && arch="$(dpkg --print-architecture)" \
 && url="https://dl.k8s.io/release/v${KUBECTL_VERSION}/bin/linux/${arch}/kubectl" \
 && curl -fsSL -o /usr/local/bin/kubectl "$url" \
 && echo "$(curl -fsSL "$url.sha256")  /usr/local/bin/kubectl" | sha256sum -c - \
 && chmod 0755 /usr/local/bin/kubectl /usr/local/lib/mu/mu-remote-pod \
 && omp="$(sed -n 's/^omp = "\(.*\)"$/\1/p' /tmp/mu-build/upstream.toml)" \
 && bash /tmp/mu-build/fetch-omp.sh "$omp" /usr/local/bin/omp \
 && rm -rf /tmp/mu-build
USER worker
