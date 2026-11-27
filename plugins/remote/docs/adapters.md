# Adapters

An adapter moves bytes and runs processes on a remote; `mu-remote` does the rest. `ssh` is built in, `pod` ships with `mu-remote` (`mu-remote install-adapter pod`), and any program that follows the contract below works: `adapter = "/path/to/prog"` or a name on the `PATH`.

## The adapter contract (version 2)

`mu-remote` runs `<adapter> <verb> …`, one process per call. The contract is versioned (below): you can write an adapter in any language, and `mu-remote check NAME --full` runs it through every verb it can without an agent.

### Environment

| Variable | Value |
|---|---|
| `MU_REMOTE_NAME` | the remote's name in the config |
| `MU_REMOTE_SETTINGS` | the `[remote.NAME.settings]` table as one JSON object (`{}` when there is none) |
| `MU_REMOTE_TRUSTED` | `1` for a remote with `trusted = true`, else `0` |
| `MU_REMOTE_CONTRACT` | the contract number `mu-remote` speaks (2) |
| `MU_REMOTE_EXE` | the running `mu-remote`; its hidden `mu-config` prints mu's `config.toml` as the active profile sees it (shared file, profile's own merged over it), for an adapter that sends it to a machine without the profile (optional addition, no new number) |

An adapter must not mount or pass a secret into a remote whose `MU_REMOTE_TRUSTED` is `0`. For an untrusted remote `mu-remote` also removes from the adapter's environment every variable whose name contains `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `CREDENTIAL` or `AUTH` (any case; `SSH_AUTH_SOCK` stays), see [trust](trust.md). stdin is `/dev/null` except for `write` (the bytes) and `connect` (the stream).

### Verbs

Exit codes for every verb: `0` is success; `64` is "not supported here", which only the seven optional verbs (`forward`, `unforward`, `read`, `write`, `stop`, `cancel`, `notify`) may use (`mu-remote` then does without: mu falls back to its own file transfer and says ports cannot be forwarded; `mu-remote stop`, `cancel` and `notify` say the remote cannot); any other code is an error, and the stderr lines are the error text, one per line. A stderr line `{"progress":"pulling image"}` is progress, not an error. Keep stdout for a verb's reply.

| Verb | Does | stdout | Time limit |
|---|---|---|---|
| `version` | the contract version; 1 and 2 are accepted, anything else is refused | `{"contract":N}` | 10 s (`check`, `list`, `rows`); 30 s before a file or port verb |
| `start --kind agent [--resume ID \| -c] [--bundle FILE --dir PATH] [--model M]` | starts a detached `mu agent` there; with `--bundle` it first clones the bundle (the repository's full history) into `PATH` and runs the agent with `PATH` as its directory | `{"id":ID}` once it listens (`mu agent`'s own `{"session":ID,…}` line is accepted too) | none |
| `start --kind worker --launch ID --bundle FILE --dir PATH` | makes a git checkout of the bundle's first ref in `PATH` and starts `mu-remote worker run --launch ID --dir PATH --detach` there | `{"id":ID}` | none |
| `list` | the agents and workers there | one JSON object per line (below) | 30 s (to pick an agent); 10 s (`check`, `list`, `rows`) |
| `connect ID` | a stdio pipe to `mu-remote bridge ID` on the far side | the stream | none; it ending is a lost connection, which `mu-remote` reconnects |
| `fetch ID` | `mu-remote worker bundle ID` there: the worker's result bundle | the bundle's bytes (nothing when unchanged) | none |
| `stop ID` | ends agent or worker ID: a worker gets `mu-remote worker stop ID` there, then is removed; an agent gets `mu agent stop ID` there | nothing | none from the launcher; 60 s from `mu-remote stop` |
| `cancel ID` | cancels agent ID's current turn (`mu agent cancel ID` there); the agent keeps running | nothing | 60 s |
| `notify ID` | leaves agent ID a message: one line of text on stdin (`mu notify ID -` there) | one line saying what happened | 60 s |
| `forward ID PORT LOCAL` | makes `127.0.0.1:LOCAL` on this machine reach `localhost:PORT` where agent `ID` runs, then returns and leaves it open | nothing | 30 s |
| `unforward ID PORT LOCAL` | removes that forward | nothing | 30 s |
| `read ID PATH` | the regular file `PATH` on the agent's machine | the file's bytes | none here; mu ends the helper after 10 minutes |
| `write ID PATH` | creates `PATH` (and its parent directories) from stdin, mode 0600; fails when it exists | nothing | none here; mu ends the helper after 10 minutes |

A call that passes its time limit is killed with its process group. `forward` must not leave a process holding `stdout` or `stderr` open, or `mu-remote` waits for it.

**`list` fields.** `id` is required. `kind` is `agent` or `worker` (default `agent`); the rest are as `mu agent list --json` prints them: `cwd`, `started` (unix seconds), `title`, `client` (`{"host","since"}`), `waiting`, `locked`. Unknown fields are ignored.

**`start --dir PATH`.** `PATH` is where the caller wants the checkout. An adapter may place it elsewhere, as long as `connect`, `fetch` and `stop` find the worker (or agent) by the ID `start` printed. A worker's `PATH` is under the remote's `work_dir` setting (default `/work`) when it has one.

The last four verbs are what [`mu-remote helper`](../README.md#files-and-ports) runs for mu (`MU_ATTACH_HELPER`); `mu-remote` picks `LOCAL`. They run only for a trusted remote ([trust](trust.md)). A version 1 adapter keeps working without them: `mu-remote helper` answers exit code 64 for each, and mu does without.

`connect` carries an opaque byte stream of lines: it must not interpret or buffer it. `mu-remote bridge` on the far side ends the [encrypted channel](channel.md) next to the agent (or the worker), so the adapter, the API server and every hop in between see only ciphertext.

### Negotiation and stability

`version` prints `{"contract":N}`; `mu-remote` accepts 1 and 2 and refuses any other number with a message that names both. The number goes up when an adapter written for the old one could break: a removed or renamed verb, flag or field, a changed meaning, a new required input. An optional addition keeps the number and is listed here. A new environment variable is such an addition, so an adapter ignores variables it does not know and `mu-remote` ignores fields it does not know. Contract 2 added `forward`, `unforward`, `read`, `write` and `MU_REMOTE_CONTRACT`. `stop` of an agent, `cancel` and `notify` were added to contract 2 as optional verbs (exit 64 if not supported).

### Writing an adapter

Start from `adapters/mu-remote-local` (a short POSIX `sh` program that runs everything on this machine; `mu-remote install-adapter local` writes it and `adapter = "local"` runs it) and verify yours with `mu-remote check NAME --full`.

## The ssh adapter

`ssh` needs no program. Under an active mu profile (`MU_PROFILE`, else `OMP_PROFILE`, else `PI_PROFILE`) every command it runs starts with `export MU_PROFILE=NAME;`, so the remote's `mu` uses its own profile of that name (its config, credentials and sessions; a name it has no profile for starts empty, with no credentials) and `list`, `connect` and `stop` find the agents that profile started. It runs `ssh -T -o BatchMode=yes HOST mu agent --detach …` for `start --kind agent`, `… mu agent list --json` for `list` and `… mu-remote bridge ID` for `connect`, with every argument quoted for the remote shell. `mu` and `mu-remote` must be on the `PATH` of a non-interactive ssh login there. `BatchMode=yes` means a host that wants a password fails, with a hint to load your key into ssh-agent. Every call shares one multiplexed connection (`ControlMaster=auto`, `ControlPersist=10m`), so the file and port verbs do not log in again; the sockets live in `$XDG_RUNTIME_DIR/mu-remote` when that is set and absolute, else in `/tmp/mu-remote-<uid>` (not `$TMPDIR`: a unix socket path is limited to about 100 bytes and macOS's temp directory is long). The directory must belong to you with mode 0700, else it is refused (`… is not private to you`). Where it cannot be made, ssh runs without the multiplexing and the four new verbs answer "not supported". `forward` is `ssh -O forward -L 127.0.0.1:LOCAL:localhost:PORT` on that connection (`localhost` as the far end, so a server bound to `::1` works), `unforward` is `-O cancel`, `read` is `cat -- PATH` and `write` is `umask 077; mkdir -p …; [ ! -e PATH ] && cat > PATH`, every path quoted for the remote shell. A new agent is started in the remote's home directory, which has its own checkouts: the ssh adapter takes no bundle. `stop`, `cancel` and `notify` run `mu agent stop`, `mu agent cancel` and `mu notify ID -` on the same connection. `start --kind worker` and `fetch` are not supported over ssh; use another adapter for tasks. `MU_SSH_PROGRAM` replaces the `ssh` program (the tests use a fake).

## The pod adapter

`mu-remote install-adapter pod` writes `mu-remote-pod` and its pod template `mu-remote-pod.yaml` into `<state>/bin/` (running it again replaces them), and `adapter = "pod"` then runs that program. It drives `kubectl` (your kubeconfig is the credential) and needs `jq`, to read the settings. One pod per agent or worker, from the `worker` (or, for the main agent, `agent`) image built with `plugins/remote/Dockerfile` ([kubernetes.md](kubernetes.md)). The image needs `mu` and `mu-remote`; the worker's `mu` must be the version the launching mu runs.

```toml
[remote.cluster]
adapter = "pod"
[remote.cluster.settings]
context = "…"                   # kubectl context
namespace = "…"                 # workers; default "default"
agent_namespace = "…"           # agent pods; default: namespace
work_dir = "/work"              # where a new agent's repository is cloned (<work_dir>/<repo name>)
image = "…"                     # default mu-remote-worker:latest
pull_policy = "IfNotPresent"
cpu = "2"                       # requests
memory = "4Gi"
deadline = 21600                # seconds a worker pod may live
agent_deadline = 0              # the same for an agent pod; unset: none
ready_timeout = "300s"
sccache_secret_name = "mu-sccache"   # trusted remotes only
```

- **`start`** creates the pod, copies the config tar (`config.toml` (under a profile that has its own, the shared one with the profile's merged over it, as `mu-remote mu-config` prints it), `models.toml`, `providers.toml`, `AGENTS.md`, `RULES.md`, `agents/`, `skills/`; never `auth.json` or a token file) and then, for an agent, clones `--bundle` into `--dir` when it has one and runs `mu agent --detach` there, or, for a worker, unpacks the bundle into `--dir` and runs `mu-remote worker run --launch ID --dir DIR --detach`. It reports `{"progress":"scheduling"}`, `"pulling image"`, `"copying repository"` and `"starting"` on stderr. A start that fails deletes its pod.
- **`connect ID`** is `kubectl exec -i POD -- mu-remote bridge ID`. A pod is found by its name (a worker's launch id) or, for an agent, by asking each pod's `mu agent list --json`.
- **`list`** adds `"kind":"agent"` to a line that has no kind.
- **`read ID PATH`** is `kubectl exec POD -- cat -- PATH`; **`write ID PATH`** is `kubectl exec -i POD -- sh -c 'umask 077; mkdir -p …; [ ! -e "$1" ] && cat > "$1"'`.
- **`forward ID PORT LOCAL`** starts `kubectl port-forward --address 127.0.0.1 pod/POD LOCAL:PORT` in the background, with its output in `<state>/forwards/POD-LOCAL.log` and its pid in `<state>/forwards/POD-LOCAL.pid`, and returns once the log says it is forwarding (within 10 seconds; otherwise it kills it and fails with the log's last line). **`unforward ID PORT LOCAL`** kills the recorded process and removes the file; no file is success. A forward lives until then, or until its pod or your login session ends it.
- **`fetch ID`** prints `mu-remote worker bundle ID` and deletes the pod only after that succeeded. **`stop ID`** tells the worker to stop (`mu-remote worker stop`) and deletes the pod. For an agent pod it runs `mu agent stop ID` there and deletes the pod only when this adapter's `start` made it and no agent is left in it; a pod from a manifest (a StatefulSet's) is never deleted, and its liveness probe starts a new agent. **`cancel ID`** and **`notify ID`** run `mu agent cancel ID` and `mu notify ID -` in the agent's pod. A dropped stream deletes nothing: the pod lives until a fetch, a stop, its deadline, or the next `start` from the same machine, which deletes this machine's pods that finished or passed their deadline (matched by a random launcher id kept in `<state>/launcher-id`, not by host name).
- **Untrusted remotes** (`MU_REMOTE_TRUSTED=0`): the adapter mounts and passes no secret and refuses `sccache_secret_name`; the pod gets only `SCCACHE_BUCKET`, `SCCACHE_ENDPOINT`, `SCCACHE_REGION`, `SCCACHE_S3_USE_SSL` and `SCCACHE_S3_KEY_PREFIX` from the launcher's environment, if set. On a trusted remote the secret (default `mu-sccache`, optional) becomes the pod's environment, so workers can write to the shared sccache bucket: one worker can poison another's build.

`/login` in an agent pod runs in the agent process (the client only relays the terminal) and writes `auth.json` into the pod's profile home, never to the client; the config tar sent by `start` holds no credential and the bundle holds only the repository.

For the main agent in a cluster you control, with workers in a second namespace, see [kubernetes.md](kubernetes.md). `scripts/test-pod-run.sh` runs the adapter against a fake `kubectl`; pod scheduling, `kubectl wait`, jsonpath on real labels and exec behaviour on a dropped connection need a real cluster.

## What an adapter needs

`list` and `connect` are enough to attach to agents someone else started; `forward`, `unforward`, `read` and `write` (contract 2) let mu open the agent's files and reach its servers, and `stop`, `cancel` and `notify` let it end, interrupt and message an agent; `start --kind worker`, `fetch` and `stop` make it usable as a task remote. Keep stdout for a verb's reply, send diagnostics to stderr one per line, and never pass a secret into a remote whose `MU_REMOTE_TRUSTED` is `0`. `mu-remote check NAME` runs `version` and `list` and says which failed; `--full` also runs a synthetic worker through `start`, `connect`, `fetch` and `stop`, and with `--agent ID` the file and port verbs.
