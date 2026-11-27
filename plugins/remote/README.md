# mu-remote

Remotes for mu, the coding agent: agents and tasks on other machines, over an encrypted channel.

mu itself knows nothing about ssh, Kubernetes or any other way to reach a machine. It publishes versioned contracts ([docs/remote-contracts.md](../../docs/remote-contracts.md)), and `mu-remote` is a program built only on them. It is not a `plugin.toml` plugin: it depends on no mu crate, runs `mu` and talks to it over mu's documented commands and stdio protocols. It lives in mu's repository and ships in every mu release, as the worked example of a third-party transport.

What it does:

- **Named remotes** in `~/.config/mu-remote/config.toml`: `mu-remote NAME` starts or finds an agent on the remote and attaches your terminal to it (`mu --attach`), over an end-to-end encrypted channel.
- **Tasks on a remote.** As mu's `[task] launcher`, it runs a task subagent as a worker on a remote, in a copy of your repository, and brings the result back as a git bundle. The stream survives dropped connections.
- **Adapters** move the bytes: `ssh` is built in; a Kubernetes (`kubectl`) adapter ships with it; anything else is a program that follows the [adapter contract](docs/adapters.md).
- **Sessions tab rows** for the agents on your remotes, as a mu `[sessions] sources` command.

## Install

`mu-remote` comes with mu: mu's `scripts/install.sh` installs it under `~/.mu/bin` and links `~/.local/bin/mu-remote` (see mu's README). It needs `mu` on the `PATH` (or `MU_REMOTE_MU` naming it), on your machine and on every remote that runs an agent or a worker, and `mu-remote` itself on the remote too (the far end of the channel is `mu-remote bridge`). A worker's `mu` must be the same build as the launching mu.

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

The image comes from `plugins/remote/Dockerfile`, built from mu's repository root (`docker build -f plugins/remote/Dockerfile --target worker …`); see [docs/kubernetes.md](docs/kubernetes.md) for the images and for running the main agent itself in a cluster (`examples/kubernetes/`).

## Commands

| Command | Does |
|---|---|
| `mu-remote [NAME] [--resume ID \| -c] [--model M]` | pick or start an agent on NAME and attach to it |
| `mu-remote connect NAME ID` | the encrypted stream to agent ID, for `mu --attach` |
| `mu-remote bridge ID` | on the remote: the far end of `connect` |
| `mu-remote key` | this machine's bridge key fingerprint |
| `mu-remote launch NAME` | mu's `[task] launcher` |
| `mu-remote worker run \| bundle \| stop` | the remote end of a launched task (run by adapters) |
| `mu-remote list`, `check NAME [--full [--agent ID]]` | the remotes; what fails on one (`--full` runs a synthetic worker, and with an agent the file and port verbs, through its adapter) |
| `mu-remote rows [NAME]` | agents as JSON lines, for `[sessions] sources = [["mu-remote","rows"]]` |
| `mu-remote agents [NAME] [--json]` | the agents and workers on every remote, or on NAME |
| `mu-remote stop NAME ID` | end agent (or worker) ID on NAME |
| `mu-remote cancel NAME ID` | cancel the current turn of agent ID on NAME |
| `mu-remote notify NAME ID TEXT\|-` | leave agent ID on NAME a message |
| `mu-remote install-adapter pod \| local` | write a shipped adapter into the state directory (`local` is the reference adapter) |
| `mu-remote helper NAME ID VERB …` | what `mu-remote NAME` gives mu as `MU_ATTACH_HELPER` (not for typing; see below) |

### Managing agents

`mu-remote agents` lists them; `mu-remote stop|cancel|notify NAME ID` ends one, cancels its turn or messages it (the remote's adapter must support it; `ssh` and the shipped pod and local adapters do). In mu, rows from `mu-remote rows` get the same on the Sessions tab (`s` cancels the turn, `S` stops after `y`, `n` messages) and `/agents` does it from the composer (`/agents stop|cancel|notify|attach ID`). `/quit` inside an attached agent ends it too.

### Files and ports

`mu-remote NAME` starts `mu --attach` with `MU_ATTACH_HELPER` set to `mu-remote helper NAME ID`, so that a remote agent feels local: mu opens files the agent mentions (the helper `read`s them from the agent's machine into a private temporary directory here), uploads files from your machine for the agent (`write`, into the session's `uploads/` directory there) and forwards the ports of servers the agent starts to `127.0.0.1` here (`forward`). mu knows nothing about ssh or kubectl: the helper asks the remote's adapter ([contract](docs/adapters.md)), which does it over the same ssh connection or `kubectl`. It works only for a trusted remote, and an adapter that does not support it just makes mu do without.

## Documentation

- [docs/config.md](docs/config.md): the config file and `[remote.NAME]`
- [docs/adapters.md](docs/adapters.md): the adapter contract, the ssh and pod adapters; `adapters/mu-remote-local` is the reference adapter
- [docs/trust.md](docs/trust.md): trusted and untrusted remotes
- [docs/channel.md](docs/channel.md): the encrypted channel and the worker link
- [docs/tasks.md](docs/tasks.md): how a task runs on a remote
- [docs/data-flow.md](docs/data-flow.md): where your data goes
- [docs/kubernetes.md](docs/kubernetes.md): the main agent in a cluster

mu's side of this (the launcher contract, `mu --attach`, `[sessions] sources`, the attach helper) is in mu's [docs/remote-contracts.md](../../docs/remote-contracts.md).

## Development

mu's `scripts/ci-checks.sh` runs the checks for the whole repository, this directory included: its tests run against the `mu` the workspace builds (`target/debug/mu`; `MU_REMOTE_TEST_MU` names another), and `scripts/test-pod-run.sh` runs the pod adapter against a fake `kubectl` (needs `jq`). A test (`tests/standalone.rs`) fails if `Cargo.toml` names a mu crate or a path dependency.
