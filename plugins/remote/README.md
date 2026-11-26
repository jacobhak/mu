# mu-remote

Remotes for mu, the coding agent: agents and tasks on other machines, over an encrypted channel.

mu itself knows nothing about ssh, Kubernetes or any other way to reach a machine. It publishes four entry points (see "For extension authors" in mu's `docs/remote.md`), and `mu-remote` is a separate program built only on them. It depends on no mu crate; it runs `mu` and talks to it over mu's documented commands and stdio protocols.

What it does:

- **Named remotes** in `~/.config/mu-remote/config.toml`: `mu-remote NAME` starts or finds an agent on the remote and attaches your terminal to it (`mu --attach`), over an end-to-end encrypted channel.
- **Tasks on a remote.** As mu's `[task] launcher`, it runs a task subagent as a worker on a remote, in a copy of your repository, and brings the result back as a git bundle. The stream survives dropped connections.
- **Adapters** move the bytes: `ssh` is built in; a Kubernetes (`kubectl`) adapter ships with it; anything else is a program that follows the [adapter contract](docs/adapters.md).
- **Sessions tab rows** for the agents on your remotes, as a mu `[sessions] sources` command.

## Install

```sh
scripts/install.sh                          # the latest release into ~/.local/bin (needs an authenticated gh while the repo is private)
scripts/install.sh --tag mu-remote-20261115-abc1234
scripts/install.sh --from-source .          # build this checkout
```

The script checks the tarball against the release's `SHA256SUMS`, and against its build attestation when the release has one. A release names the mu it was tested against in its notes (`mu-version` in the repository). The pinned nightly toolchain in `rust-toolchain.toml` is installed by `rustup` on the first source build. `mu-remote` needs `mu` on the `PATH` (or `MU_REMOTE_MU` naming it), on your machine and on every remote that runs an agent or a worker, and `mu-remote` itself on the remote too (the far end of the channel is `mu-remote bridge`).

## Quick start: ssh

On the remote, install `mu` and `mu-remote` so that a non-interactive ssh login finds them. On your machine:

```toml
# ~/.config/mu-remote/config.toml
[remote.devbox]
adapter = "ssh"
host = "devbox"          # ssh's own config does the rest
trusted = true           # a machine you control: it may run a main agent
```

```sh
mu-remote devbox         # a new agent there, your terminal attached
mu-remote devbox -c      # the latest agent there
mu-remote list           # the remotes, and whether they answer
mu-remote check devbox   # what fails, if something does
```

The first attach prints `first contact with agent ID on devbox: its key is SHA256:…, now pinned`. Compare it with `mu-remote key` run on the remote some other way.

## Quick start: pods

```sh
mu-remote install-adapter pod        # writes the adapter into ~/.local/state/mu-remote/bin
```

```toml
[remote.cluster]
adapter = "pod"
trusted = true
[remote.cluster.settings]
context = "<kube-context>"
namespace = "<workers namespace>"
image = "<registry>/mu-remote-worker:<tag>"
```

To run mu's task subagents there, tell mu to use `mu-remote` as the launcher, in mu's `config.toml`:

```toml
[task]
launcher = ["mu-remote", "launch", "cluster"]
launch = ["task"]
```

The image comes from this repository's `Dockerfile` (`docker build --target worker …`); see [docs/kubernetes.md](docs/kubernetes.md) for the images and for running the main agent itself in a cluster (`examples/kubernetes/`).

## Commands

| Command | Does |
|---|---|
| `mu-remote [NAME] [--resume ID \| -c] [--model M]` | pick or start an agent on NAME and attach to it |
| `mu-remote connect NAME ID` | the encrypted stream to agent ID, for `mu --attach` |
| `mu-remote bridge ID` | on the remote: the far end of `connect` |
| `mu-remote key` | this machine's bridge key fingerprint |
| `mu-remote launch NAME` | mu's `[task] launcher` |
| `mu-remote worker run \| bundle \| stop` | the remote end of a launched task (run by adapters) |
| `mu-remote list`, `check NAME` | the remotes; what fails on one |
| `mu-remote rows [NAME]` | agents as JSON lines, for `[sessions] sources = [["mu-remote","rows"]]` |
| `mu-remote install-adapter pod` | write the shipped pod adapter into the state directory |
| `mu-remote helper NAME ID VERB …` | what `mu-remote NAME` gives mu as `MU_ATTACH_HELPER` (not for typing; see below) |

### Files and ports

`mu-remote NAME` starts `mu --attach` with `MU_ATTACH_HELPER` set to `mu-remote helper NAME ID`, so that a remote agent feels local: mu opens files the agent mentions (the helper `read`s them from the agent's machine into a private temporary directory here), uploads files from your machine for the agent (`write`, into the session's `uploads/` directory there) and forwards the ports of servers the agent starts to `127.0.0.1` here (`forward`). mu knows nothing about ssh or kubectl: the helper asks the remote's adapter ([contract](docs/adapters.md)), which does it over the same ssh connection or `kubectl`. It works only for a trusted remote, and an adapter that does not support it just makes mu do without.

## Documentation

- [docs/config.md](docs/config.md): the config file and `[remote.NAME]`
- [docs/adapters.md](docs/adapters.md): the adapter contract, the ssh and pod adapters
- [docs/trust.md](docs/trust.md): trusted and untrusted remotes
- [docs/channel.md](docs/channel.md): the encrypted channel and the worker link
- [docs/tasks.md](docs/tasks.md): how a task runs on a remote
- [docs/data-flow.md](docs/data-flow.md): where your data goes
- [docs/kubernetes.md](docs/kubernetes.md): the main agent in a cluster

mu's own side of this (the launcher contract, `mu --attach`, `[sessions] sources`) is in mu's `docs/remote.md`.

## Development

`scripts/ci-checks.sh` runs the checks (format, shellcheck, cargo-deny, clippy, tests, and `scripts/test-pod-run.sh`, which runs the pod adapter against a fake `kubectl`; needs `jq`). Integration tests that need a real `mu` read its path from `MU_REMOTE_TEST_MU` and skip without it.

### CI and releases

- **CI** (`.github/workflows/ci.yml`) runs `scripts/ci-checks.sh` on Linux (`arc-mu-remote`) and macOS (`mu-remote-ci`) runners. Nothing gates a merge on it: branch protection needs a paid plan on a private repository.
- **mu for the tests.** `mu-version` holds the full sha of the mu commit the integration tests run against. `scripts/fetch-mu.sh DIR` checks it out and `scripts/build-mu.sh DIR` builds it and prints the binary's path (`MU_REMOTE_TEST_MU`); `.github/actions/mu` does both in CI, with the build cached. mu is private, so CI authenticates with the repository secret `MU_DEPLOY_KEY`, the private half of a read-only deploy key on `jacobhak/mu` (created with `gh api -X POST repos/jacobhak/mu/keys -F read_only=true`). It always names a commit on mu's main; bump it in the same change as a protocol or contract change that needs a newer mu.
- **Releases** are tag-driven: push a tag `mu-remote-<YYYYMMDD>-<short sha>`. `.github/workflows/release.yml` runs the checks on both platforms (Linux in `ubuntu:22.04`, so the glibc floor is 2.35), packages `mu-remote-<plat>.tar.gz` (`linux-x64`, `darwin-arm64`), publishes them with `SHA256SUMS` and a best-effort build attestation (it needs a public repository or Enterprise Cloud; on a private one the release goes out with the checksum only). Run the workflow by hand (`workflow_dispatch`) for a dry run that publishes nothing.
- **Runners.** Linux jobs use the `arc-mu-remote` scale set of the Actions Runner Controller, macOS jobs the `mu-remote-ci` label. Both are registered to this repository only (a repository's runners are invisible to others); the machines' setup is documented where they are managed.
