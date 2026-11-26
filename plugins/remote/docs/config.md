# Configuration

## Files

| What | Where | Override |
|---|---|---|
| Config | `~/.config/mu-remote/config.toml` (`$XDG_CONFIG_HOME/mu-remote/config.toml`) | `MU_REMOTE_CONFIG` |
| State | `~/.local/state/mu-remote` (`$XDG_STATE_HOME/mu-remote`) | `MU_REMOTE_HOME` |
| `mu` program | `mu` on the `PATH` | `MU_REMOTE_MU` |

Under the state directory: `keys/bridge.key` and `keys/client.key` (the [channel](channel.md) keys, mode 0600 in a 0700 directory), `keys/pins.json` (the pinned keys), `bin/` (adapters written by `install-adapter`), `workers/<id>/` (the records of workers served on this machine) and `launcher-id` (the pod adapter's id of this machine). Nothing here is mu's: mu's own configuration stays in mu's `config.toml`.

A missing config file is an empty config. A remote that cannot be used is dropped with a warning on stderr.

## `[remote.NAME]`

```toml
default_remote = "cluster"      # optional: `mu-remote` with no NAME uses it

[remote.cluster]
adapter = "pod"                 # "ssh" is built in; "pod" is the installed Kubernetes adapter; else a program
trusted = true
state_key = "~/.ssh/id_ed25519"
reconnect_timeout = 600
[remote.cluster.settings]       # passed to the adapter as JSON; mu-remote does not read it (except work_dir)
namespace = "…"

[remote.devbox]
adapter = "ssh"
host = "devbox"                 # ssh's own config does the rest
repos = ["/src/app", "https://example.com/team/app.git"]   # untrusted: only these repositories
```

| Key | Meaning |
|---|---|
| `adapter` | `ssh` (built in, needs `host`), `pod` (the adapter written by `mu-remote install-adapter pod`) or a program on the `PATH` or a path ([adapters](adapters.md)). Required |
| `host` | The ssh host. Must not start with `-`. Used by the `ssh` adapter only |
| `state_key` | The SSH ed25519 private key that unlocks a sealed agent's state, passed to `mu --attach --state-key`. Default `~/.ssh/id_ed25519`. A path, not a key |
| `settings` | A table handed to the adapter as JSON in `MU_REMOTE_SETTINGS`. `work_dir` (default `/work`) is read by `mu-remote` itself: a new agent's repository is cloned to `<work_dir>/<repo name>` |
| `trusted` | `true` marks a remote you control: it may run a main agent, and the pod adapter may mount a secret. Default `false` ([trust](trust.md)) |
| `repos` | On an untrusted remote, the repositories (paths or origin URLs, with or without `.git`) whose tasks may run there |
| `reconnect_timeout` | Seconds a launched task's stream may stay down before the launcher gives up and fetches what the worker has ([tasks](tasks.md)). Default 600 |

`mu-remote NAME` picks a remote; with no name `default_remote` decides. An unknown name is an error that lists the configured ones.

## No secrets

No secret goes in this file: an adapter reads credentials from where its tool keeps them (kubeconfig, ssh-agent). A `settings` key that looks like a secret (a word `token`, `password`, `passwd`, `secret`, `key`, `apikey` or `credential(s)` in its name, at any depth) makes `mu-remote` ignore the remote with a warning, unless the name ends in `_name`, `_ref`, `_file`, `_path`, `_env` or `_var`, which names a secret instead of holding it (`sccache_secret_name`).

## mu's side

`mu-remote` is wired into mu through mu's own `config.toml`:

```toml
[task]
launcher = ["mu-remote", "launch", "cluster"]   # run task subagents on the remote `cluster`
launch = ["task"]                                # which agent kinds use the launcher

[sessions]
sources = [["mu-remote", "rows"]]                # the agents on every remote, in the Sessions tab
```

`mu-remote NAME` itself runs `mu --attach 'mu-remote connect NAME ID' --state-key KEY`, so it needs nothing in mu's config.
