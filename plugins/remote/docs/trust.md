# Trust

Every remote is **untrusted** unless its `[remote.NAME]` says `trusted = true`.

- **Trusted**: a remote you control. It may run the main agent (which holds your credentials: `auth.json`, the agent's key, deploy keys, object-store keys), and the pod adapter may mount a secret there. Namespaces, least-privilege service accounts, network policies and volumes encrypted at rest protect against other tenants and against stolen disks or backups, not against whoever administers the cluster.
- **Untrusted** (the default): it runs tasks for the repositories in its `repos` only, and never a main agent. Its operator can read the repository with its full history, the task text, the worker's transcript and the build cache. Nothing secret is sent to it.

## The main agent

`mu-remote NAME` starts or joins a main agent, which holds credentials, so it works only on a trusted remote. On any other it refuses, before asking the adapter for anything:

```
remote NAME is not trusted to run a main agent (set trusted = true on a remote you control)
```

## Files and ports

The helper verbs of `mu --attach` (`mu-remote helper`: `read`, `write`, `forward`, `unforward`) run only for a trusted remote. On any other the helper exits 1 with `remote NAME is not trusted` and the adapter is never called.

## Tasks

An untrusted remote runs a launch (`mu-remote launch NAME`) only when both hold:

1. The repository is listed in `repos`: its root path, or its origin URL, with or without `.git`. Otherwise the launcher exits without starting anything and prints `remote NAME is not trusted for <repo>; list it in [remote.NAME] repos to allow it`.
2. `$MU_TASK_AGENT_DIR/agents` and `$MU_TASK_AGENT_DIR/skills` (your agent directory: what the config sent to a remote carries) hold no file named like a credential: `.env*`, `*.pem`, `*.key`, `id_*`, or with `credential`, `secret` or `token` in the name. Otherwise the launch is refused, naming them: `…the config sent to it would carry credential-looking files: …`.

A trusted remote skips both checks.

## What is sent, and what is not

- The launcher forwards mu's stream unchanged. The task payload carries no secret and the bundle holds only the repository (full history, with the working tree's changes, untracked files included, as one commit).
- `auth.json` and token files are never sent to a pod; the pod adapter's config tar leaves them out on every remote.
- The adapter of an untrusted remote runs with every environment variable whose name has `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `CREDENTIAL` or `AUTH` in it removed (`SSH_AUTH_SOCK` stays: it names a socket, not a key). The adapter contract also forbids it to mount or pass a secret when `MU_REMOTE_TRUSTED` is `0`.
- A launched worker holds no model credential: its model and search calls come back through your mu session as relay lines, and it may spend only on its task's models up to mu's `[task] max_cost`.

The repository itself, with its full history, the task text and the transcript of the worker are on the remote: that is accepted when you list the repository. See [data-flow.md](data-flow.md) for where each kind of data ends up, and [channel.md](channel.md) for what the encrypted channel does and does not protect.
