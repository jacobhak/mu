# Two images from one file (always pass --target). The build context is this repository plus the programs that are
# not built here, which you put in the context root first:
#   mu   a mu release binary for linux (the one inside a mu release tarball). Use the version the machine that
#        launches tasks runs: mu refuses a worker of another version. A mu you built yourself links the glibc of
#        the machine that built it, and may need a newer one than this debian base has (check with
#        `objdump -T mu | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1`): then change the `worker` stage's base
#        image to one with at least that glibc, and make sure the `useradd` below still runs (ubuntu images
#        already have a user with uid 1000: `userdel -r ubuntu` first).
#   omp  (agent target only) the omp binary mu's `/login` needs.
# Then, from the repository root:
#   docker build --target worker -t <registry>/mu-remote-worker:<tag> .
#   docker build --target agent  -t <registry>/mu-remote-agent:<tag>  .
# `--build-arg MU_FILE=path` and `--build-arg OMP_FILE=path` name other files in the context (default `mu`, `omp`).
#
# `worker`: debian slim, git, ca-certificates, procps, mu and mu-remote (built here with the toolchain pinned in
# rust-toolchain.toml); it is what the pod adapter starts for a task. It carries no language toolchain: extend it with
# your own, as the base of your project's image:
#   FROM <registry>/mu-remote-worker:<tag>
#   USER root
#   RUN apt-get update && apt-get install -y --no-install-recommends build-essential && rm -rf /var/lib/apt/lists/*
#   USER worker
# (a compiler cache such as sccache belongs there too, with its settings in the pod adapter's environment).
# `agent`: the worker plus what a main agent in a pod needs: omp, kubectl, jq, tini as PID 1 and the pod adapter,
# in /usr/local/lib/mu-remote/ (`mu-remote install-adapter pod` also works). See docs/kubernetes.md.
# Both run as uid 1000. No secrets are baked in. Build with a new tag each time: a reused tag is not pulled again by
# nodes that have it.

FROM debian:bookworm-slim AS builder
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . /src
# rustup reads rust-toolchain.toml from /src, so the pinned channel and components are what gets installed.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none \
 && rustup toolchain install \
 && cargo build --release --locked \
 && install -m 0755 target/release/mu-remote /usr/local/bin/mu-remote

FROM debian:bookworm-slim AS worker
ARG MU_FILE=mu
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git procps \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 1000 --create-home --shell /bin/sh worker
COPY --chmod=0755 ${MU_FILE} /usr/local/bin/mu
COPY --from=builder /usr/local/bin/mu-remote /usr/local/bin/mu-remote
USER worker
WORKDIR /home/worker
CMD ["sleep", "infinity"]

FROM worker AS agent
# The kubectl client may be one minor version off the API server's; override with --build-arg for another cluster.
ARG KUBECTL_VERSION=1.31.14
ARG OMP_FILE=omp
USER root
COPY --chmod=0755 ${OMP_FILE} /usr/local/bin/omp
COPY --chmod=0755 adapters/mu-remote-pod /usr/local/lib/mu-remote/mu-remote-pod
COPY adapters/mu-remote-pod.yaml /usr/local/lib/mu-remote/mu-remote-pod.yaml
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
