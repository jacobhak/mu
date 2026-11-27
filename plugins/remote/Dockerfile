# Two images from one file (always pass --target), built from the mu repository root so that mu and mu-remote come
# from one commit: mu refuses a worker whose `muVersion` is not its own, so build the image from the commit the
# machine that launches tasks runs. From the repository root:
#   docker build -f plugins/remote/Dockerfile --target worker --build-arg MU_BUILD_SHA=$(git rev-parse --short=12 HEAD) -t <registry>/mu-remote-worker:<tag> .
#   docker build -f plugins/remote/Dockerfile --target agent  --build-arg MU_BUILD_SHA=$(git rev-parse --short=12 HEAD) -t <registry>/mu-remote-agent:<tag>  .
# MU_BUILD_SHA is the sha `mu --version` reports (the build has no .git, so it is `unknown` without it).
#
# `worker`: debian slim, git, ca-certificates, procps, mu and mu-remote (built here with the toolchain pinned in
# rust-toolchain.toml); it is what the pod adapter starts for a task. It carries no language toolchain: extend it with
# your own, as the base of your project's image:
#   FROM <registry>/mu-remote-worker:<tag>
#   USER root
#   RUN apt-get update && apt-get install -y --no-install-recommends build-essential && rm -rf /var/lib/apt/lists/*
#   USER worker
# (a compiler cache such as sccache belongs there too, with its settings in the pod adapter's environment).
# `agent`: the worker plus what a main agent in a pod needs: kubectl, jq, tini as PID 1 and the pod adapter, in
# /usr/local/lib/mu-remote/ (`mu-remote install-adapter pod` also works). mu runs no omp; a gateway user adds it in
# a derived image. See docs/kubernetes.md.
# Both run as uid 1000. No secrets are baked in. Build with a new tag each time: a reused tag is not pulled again by
# nodes that have it.

FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e AS builder
ARG MU_BUILD_SHA=
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    MU_BUILD_SHA=${MU_BUILD_SHA}
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential ca-certificates curl git pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . /src
# rustup reads rust-toolchain.toml from /src, so the pinned channel and components are what gets installed.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
 && rustup toolchain install \
 && cargo build --release --locked -p mu -p mu-remote \
 && install -m 0755 target/release/mu /usr/local/bin/mu \
 && install -m 0755 target/release/mu-remote /usr/local/bin/mu-remote

FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e AS worker
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git procps \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 1000 --create-home --shell /bin/sh worker
COPY --from=builder /usr/local/bin/mu /usr/local/bin/mu
COPY --from=builder /usr/local/bin/mu-remote /usr/local/bin/mu-remote
USER worker
WORKDIR /home/worker
CMD ["sleep", "infinity"]

FROM worker AS agent
# The kubectl client may be one minor version off the API server's; override with --build-arg for another cluster.
ARG KUBECTL_VERSION=1.31.14
USER root
COPY --chmod=0755 plugins/remote/adapters/mu-remote-pod /usr/local/lib/mu-remote/mu-remote-pod
COPY plugins/remote/adapters/mu-remote-pod.yaml /usr/local/lib/mu-remote/mu-remote-pod.yaml
COPY --chmod=0755 plugins/remote/adapters/mu-remote-local /usr/local/lib/mu-remote/mu-remote-local
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl jq tini \
 && rm -rf /var/lib/apt/lists/* \
 && arch="$(dpkg --print-architecture)" \
 && url="https://dl.k8s.io/release/v${KUBECTL_VERSION}/bin/linux/${arch}/kubectl" \
 && curl -fsSL -o /usr/local/bin/kubectl "$url" \
 && echo "$(curl -fsSL "$url.sha256")  /usr/local/bin/kubectl" | sha256sum -c - \
 && chmod 0755 /usr/local/bin/kubectl \
 && apt-get purge -y curl \
 && apt-get autoremove -y
USER worker
