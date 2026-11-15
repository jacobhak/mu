# Remote use: `mu agent`, `mu -r`

The agent and the terminal UI can run in two processes, on two machines. `mu agent` runs a session with no terminal and keeps running when you close the laptop; plain `mu` with a [remote](#remotes) in effect (`mu -r NAME`, or `default_remote`) runs the usual UI on your terminal as a client of it, starting the agent there when it needs one. Everything the UI does works the same attached: the transcript, approvals and questions, plan review, `/model`, `/login`, custom providers, MCP servers, the Sessions tab, `$EDITOR` for a plan or a memory file, terminal (`pty`) tool calls. Without a remote in effect (`mu --local`, or none configured) plain `mu` is unchanged.

## Start an agent

```sh
mu agent [--model M] [--thinking T] [--permissions P] [--resume SESSION | -c] [--detach]
mu agent list [--json]
```

It prints one line, `{"session":"<id>","socket":"<path>"}`, and serves a client on that Unix socket: `<profile home>/run/<session id>.sock`, mode 0600, beside the session's owner record (kind `agent` in the [jobs panel](background-jobs.md#sessions-tab)). There is no network listener: who may connect is who may reach the socket, and reaching one on another machine is `ssh` or `kubectl exec` (below). With no client, the agent goes on with what it was doing; a tool approval or a question waits for a client. A `-p` run holding the session you `--resume` is stopped after its current step first, as `mu take` does, without asking.

`--detach` starts the agent in the background, in a session of its own, with SIGHUP ignored, so closing the terminal or the ssh connection that started it does not end it. Its output goes to `<profile home>/logs/agent-<id>.log`. The command returns once the socket listens and prints one line, `{"id":"<id>"}`; `mu agent key` prints the fingerprint of the key this profile's agents show their clients (see [Encryption](#encryption)). `mu agent list` shows the agent and `mu agent list --json` prints one JSON object per line per live agent: `{"id","cwd","started" (unix seconds),"title" (or null),"client" ({"host","since"} or null),"waiting"}`. `client` and `waiting` come from the agent's owner record, so the Sessions tab shows them too (`waiting` is added to the agent's kind).

`/new`, `/resume` and `/fork` inside the session move the socket to the new session's id; the client stays connected. `--permissions` takes effect for tool calls as usual, so an unattended agent should not be in `ask` mode unless you will attach to answer.

The agent ends on `/quit` or Ctrl-D in a client, on `SIGTERM`, `SIGHUP` or `SIGINT` (it ends the session's jobs and plugins first; a second signal exits at once), and from the Sessions tab (`S`). It does not end when a client goes away.

## Attach

```sh
mu -r devbox                   # a new session: starts an agent there, then joins it
mu -r devbox -c                # the latest agent there; none: starts one that continues the latest session
mu -r devbox --resume ID       # the agent that has session ID; none: starts one that resumes it
mu -r devbox --resume          # lists the agents there to pick from; or a new agent, which opens its saved sessions
mu --local                     # run here even when default_remote is set
```

The client asks the remote's adapter (see [Remotes](#remotes)) for its agents (`list`, agents only: workers are not sessions), starts one when needed (`start --kind agent`, with `--resume ID`, `-c` and `--model M` as given) and runs the protocol over `connect ID`; the adapter's stderr lines show as notices. A new agent (no `-c`, no `--resume`) started from inside a git repository on an adapter that takes a bundle (not `ssh`, whose host has its own checkouts) is given the repository: mu bundles its full history (`HEAD` and the current branch; no working-tree changes) into a private temporary file, passes it as `--bundle FILE --dir <work_dir>/<repo name>` and deletes the file afterwards; the adapter clones it there and starts the agent in that directory. `work_dir` is a key of the remote's `settings` (default `/work`). Outside a git repository, with `-c` or `--resume`, or on `ssh`, starting passes no directory: the agent runs where the adapter puts it (the ssh adapter: your home directory there). `--thinking` and `--permissions` are the agent's own settings and are not sent. With `-c` or `--resume`, plain `mu` never starts a second agent on a session an agent already has. `/resume` and the session picker inside the client list the sessions on the agent's machine.

The connection is `mu attach --stdio [SESSION]` on the agent's machine, a hidden subcommand that copies lines between its stdin and stdout and the agent's socket and knows nothing of them (with no session it finds the one agent running in the current directory, `--socket PATH` names a socket); the built-in ssh adapter and a `kubectl exec -i pod -- mu attach --stdio ID` adapter run it.

A main agent holds your credentials, so it runs only on a remote with `trusted = true`: on any other remote plain `mu` refuses with `remote NAME is not trusted to run a main agent (set trusted = true on a remote you control)`, before asking the adapter for anything. A remote that does not answer is refused with the adapter's error; plain `mu` does not fall back to running here (`mu --local` does).

Both ends must run the same mu build: the first line the client sends names a wire number (`WIRE`, now 3), and a different one is refused with a message in the clear. Everything after that line is [encrypted](#encryption). Display settings (`status_line`, `theme`, `notify`, `bell`, `hide_thinking_block`, `smooth_streaming`, the `[status]` levels) and composer history come from the client's own config and profile home; everything about models, keys, MCP servers, sessions and plugins is the agent's.

A client sees what it missed when it attaches: the stored conversation up to the turn in progress, that turn's events (up to 20 000 events or 16 MB; past that the oldest blocks are dropped with a notice), the latest status, jobs, queue and todo, and every question, approval, plan review or terminal request still waiting. Nothing rings until it is caught up.

- **One client at a time.** The client's first encrypted line says its host name and a client id that stays the same across its connections. An agent that already has another client answers `Busy` instead of letting go of it. Attaching by hand then asks `<host> has this session since <time>: [t]ake it over, [q]uit` before the screen is drawn; a client that lost its connection and finds the session taken stops retrying and asks in the UI (`y` takes it over, `n` quits). A taken-over client prints why and exits. The agent records the client's host and the time in its owner record, so `mu agent list` shows who has it.
- **`/detach`** closes the client and leaves the agent running. `/quit` ends the agent. A plain `mu` answers `/detach` with `not attached`.
- **A lost connection** keeps the UI up, says `connection lost; reconnecting`, and runs the command again after 1 s, then 2, 4 … up to 30 s. When it connects, the screen is cleared and the conversation repainted from the agent's catch-up. Ctrl-C while reconnecting quits.
- **Dialogs opened before the loss** stay on screen after a reconnect and are not opened a second time for the same request. The catch-up lists the requests the agent still waits for; a dialog for any other one was answered elsewhere and is closed.

### When nobody is attached

A tool approval or a question that comes up with no client is kept by the agent. At once, it sets `waiting` in its owner record (the Sessions tab and `mu agent list` show it) and runs `[agent] notify_command`, if set, under `sh -c` with the session id as `$1` and a one-line summary as `$2`. With `[agent] approval_timeout = N` (seconds; off by default) an approval still waiting after `N` seconds is answered as unavailable, so the model is told no one could approve the call, a question is cancelled, and the agent notes it in its log. `waiting` clears when the request is answered or a client attaches.

### Encrypted state

With `[agent] state_key = "ssh"` (or a key source, below) the agent keeps its credentials sealed at rest (`auth.json`, and omp's logins, see [Where your data goes](#where-your-data-goes)) under a data key stored only wrapped, in `<profile home>/keys/state.key`, for the SSH ed25519 public keys that may unlock it. There is no passphrase source and RSA keys are not supported (`ssh-keygen -t ed25519`). State encryption is on for a profile exactly when `keys/state.key` exists.

- **First attach.** An agent with `state_key = "ssh"` and no state key tells the first client that attaches: `the agent has no state key yet, so its credentials are not encrypted`, shows the public key from `<private key>.pub` (`[remote.NAME] state_key`, default `~/.ssh/id_ed25519`) and asks `[y/N]` before the screen is drawn. On `y` the agent makes the data key, wraps it for that key, seals what was stored in the clear and removes the plaintext. Older snapshots of the volume keep the plaintext on copy-on-write storage. A client that cannot ask (no terminal) only says so, and the agent stays as it is. More recipients (a second laptop, a backup key) are added from an unlocked agent with the `state_add_recipient` host call and removed with `state_remove_recipient` (no command for them yet); removing one does not take back a data key it already unwrapped.
- **After a restart** the agent is *locked*: the data key was never on the volume. It starts and listens, `mu agent list --json` shows `"locked":true`, the Sessions tab shows `locked`, `waiting` is set in the owner record and `[agent] notify_command` runs once with `locked: attach to unlock`. A turn that needs a credential fails with `locked: attach to unlock`, and so does any read or write of `auth.json`. The model chosen at start is applied once the agent is unlocked.
- **Attaching to a locked agent** unlocks it before the screen is drawn: the client takes the wrapped keys the agent offers, decrypts the entry for its key with the private key file (ssh-agent cannot decrypt, so the file is read directly), asking for its passphrase on the terminal without echo (three tries) when it is encrypted, and returns the data key in an encrypted host call that the agent zeroes after use (a few wrong keys per minute are tried at most). A key that is not a recipient is refused with `… is not one of the keys that unlock it: the agent accepts SHA256:…`. A client that cannot unlock (no key file, no terminal, a wrong passphrase three times) still attaches, and the agent stays locked and says so.
- **A reconnect** to an agent that is still unlocked asks nothing. To one that restarted locked, a client whose key has no passphrase unlocks it again; one that needs a passphrase cannot ask with the UI up, and says to attach again.
- **Unattended agents.** `[agent] state_key_file = "/path"` or `state_key_command = "cmd"` give a 32-byte key (raw bytes, or 64 hex characters) read from the file or printed by the command at start (the file wins). The agent then makes its state key from it on the first start (this turns state encryption on) and opens it on every later one, so it restarts without a client; a wrong key is a start error. The trust is the source's: a key from a Kubernetes Secret is as exposed as that Secret and its backups, so use it only when the Secret store is protected at least as well as the volume. The keys are of one kind per profile: a profile made for SSH recipients refuses a key source.

## Encryption

What a client and an agent say to each other is encrypted end to end, on top of whatever carries it: `ssh`, `kubectl exec`, the Kubernetes API server and every hop in between see only ciphertext. The channel is Noise `XX` (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, from the pure-Rust `snow` crate; the primitives are `curve25519-dalek`, `chacha20poly1305` and `blake2`). A connection goes like this:

1. The client sends one plaintext line, `hello` with the wire number and its version, and the first Noise message. The wire number stays in the clear so that a different mu still gets a readable answer (`this agent speaks wire N, the client wire M`, as a plain line). The hello carries no host name, client id or other detail.
2. The agent answers with the second Noise message, which carries its static key. The client compares that key with the one it pinned (below) and, only if it is the pinned one or the first, sends the third message, which carries the client's own static key. A client that sees another key stops there, before it has told the agent anything.
3. Every later line, both ways, is inside the channel: first the client's real hello (host name, client id, `reconnect`, `take`), then the agent's answer (`Busy`, or its hello with the host's facts and then the catch-up), then the ordinary stream.

On the wire each Noise message is one line, a JSON string holding its base64, so the adapter's `connect` carries it as it carries any line. A line longer than a Noise message (65 535 bytes) is cut into chunks and put together on arrival, within the same line limit as before (64 MiB). A line that does not decrypt, repeated, dropped or out of order, ends the connection; a reconnect (below) starts a new handshake with new session keys.

**Keys.** Every profile has two static keys under `<profile home>/keys/`, made on first use with mode 0600 in a 0700 directory: `agent.key`, which every `mu agent` of that profile shows, and `client.key`, which the profile shows as a client. `mu agent key` prints the agent key's SHA-256 fingerprint (`SHA256:…`, as ssh shows host keys). The client pins the key it sees per remote name and agent id, in `<client profile home>/keys/pins.json` (never in `config.toml`). On the first contact the client prints `first contact with agent ID on REMOTE: its key is SHA256:…, now pinned` before the UI is drawn, so you can compare it with `mu agent key` run on the remote by another way (the remote's own terminal, say). A later connection to that agent id with another key is refused with a message that names both fingerprints and the file to edit if the agent's profile was reset on purpose. A new agent id is a new pin: the agents of one profile share one key, so their fingerprints match.

**Where it applies.** The agent's socket speaks the channel to everyone: it cannot tell a local client from one that arrived over `ssh`, so a client on the same machine goes through it too (one code path). `mu attach --stdio` itself stays a raw pipe and copies the lines, encrypted or not. A take-over's nested client (`mu --nested-client`) reaches the second agent through the first agent's channel and does not pin: that host was authenticated by the outer connection. Workers use the same channel (see [Tasks on a remote](#tasks-on-a-remote)).

**What it does not do.** On the first contact the agent's key arrives over the same transport as everything else. An active man in the middle of that transport can sit in the first handshake and show its own key; compare the fingerprint (`mu agent key`) the first time, and after pinning such a man cannot come back. The channel hides the stream from the API server and the hops in between; it does not hide it from root on the node the pod runs on, or from the agent's own machine, which has the plaintext. The agent key is not a credential: it names the agent to its clients and gives no access to anything by itself, but whoever can read `<profile home>/keys/agent.key` can pose as that agent to a client that has not pinned it.

## Programs that need your terminal

`/login` through omp, an MCP server's OAuth sign-in and the `ChatGPT` sign-in run on the agent's machine, because that is where the credentials go. The agent runs the program in a pseudo-terminal the size of your window and your terminal shows it; your keystrokes and window resizes go back. For ChatGPT, whose sign-in listens on `127.0.0.1:1455` on the agent, the URL opens in your browser and you paste the address your browser ends up at into the composer. A program is hung up when its client goes away. `$EDITOR` runs on your machine, on a private copy of a plan or memory file, and the changed text is written back to the agent's machine when the editor exits successfully.

## Taking over a `-p` run

`t` in the Sessions tab and `/resume` on a session a `mu -p` still has work as locally: the agent stops the run after its current step, starts a second agent on its session, and your terminal shows a nested client of that agent until you leave it. Quitting the nested client ends the second agent; `/detach` leaves it running (`mu agent list` shows it).

## Remotes

A remote is a named place where agents run, set in `~/.mu/config.toml`:

```toml
default_remote = "cluster"      # optional: plain `mu` then works there; `mu --local` works here

[remote.cluster]
adapter = "pod"                 # "ssh" is built in; anything else is a program on PATH or a path
[remote.cluster.settings]       # passed to the adapter as JSON; mu does not read it
namespace = "…"

[remote.devbox]
adapter = "ssh"
host = "devbox"                 # ssh's own config does the rest
```

`mu -r NAME` picks a remote for one run, `--local` runs here, and with neither `default_remote` decides. A name that is not configured is an error that lists the configured ones; with `default_remote` set and its remote unreachable, plain `mu` refuses with the adapter's error and `mu --local` is the way out. `mu remote list` shows each remote, its adapter, whether it is trusted and whether it answers (`version` and `list`, 10 s each); `mu remote check NAME` runs both and says which failed.

No secret goes in this config: the adapter reads credentials from where its tool keeps them (kubeconfig, ssh-agent). A `settings` key that looks like a secret (a word `token`, `password`, `secret`, `key` or `credential` in its name, at any depth) makes mu ignore the remote with a warning, unless it ends in `_name`, `_ref`, `_file`, `_path`, `_env` or `_var`, which names a secret instead of holding it. `trusted = true` marks a remote you control, and `repos = [...]` on an untrusted one lists the repositories (paths or origin URLs) whose workers may run there; mu keeps both itself and never asks the adapter.

### The adapter contract (version 1)

mu runs `<adapter> <verb> …` with these in the environment: `MU_REMOTE_NAME`, `MU_REMOTE_SETTINGS` (the settings as JSON) and `MU_REMOTE_TRUSTED` (`0` or `1`). An adapter must not mount or pass a secret into a remote whose `MU_REMOTE_TRUSTED` is `0`. A failing verb exits non-zero and writes its error to stderr, one line each; those lines are what mu shows. A stderr line `{"progress":"pulling image"}` is progress, not an error.

| Verb | Does | Prints |
|---|---|---|
| `version` | the contract version; mu refuses any but 1 | `{"contract":1}` |
| `start --kind agent [--resume ID \| -c] [--bundle FILE --dir PATH] [--model M]` | starts a detached `mu agent` there; with `--bundle` it first clones the bundle (the repository's full history) into `PATH` and runs the agent with `PATH` as its directory | `{"id":ID}` once it listens |
| `start --kind worker --launch ID --bundle FILE --dir PATH` | starts a detached worker | `{"id":ID}` |
| `list` | the agents and workers there | one JSON object per line, as `mu agent list --json` prints them |
| `connect ID` | a stdio pipe to that agent's socket | the stream |
| `fetch ID` | the worker's result bundle | the bundle's bytes |
| `stop ID` | ends it | nothing |

### The ssh adapter

`ssh` needs no program: it runs `ssh -T -o BatchMode=yes HOST mu agent --detach …` for `start`, `… mu agent list --json` for `list` and `… mu attach --stdio ID` for `connect`, with every argument quoted for the remote shell. `mu` must be on the PATH of a non-interactive ssh login there. `BatchMode=yes` means a host that wants a password fails, with a hint to load your key into ssh-agent. `start --kind worker`, `fetch` and `stop` are not supported over ssh. `MU_SSH_PROGRAM` replaces the `ssh` program (the tests use a fake).

### The pod adapter

`mu remote install-adapter pod` writes `mu-remote-pod` and its pod template `mu-remote-pod.yaml` from the mu binary into `~/.mu/bin/` (running it again replaces them), and `adapter = "pod"` then runs that program. It drives `kubectl` (your kubeconfig is the credential) and needs `jq`, to read the settings, on the client. One pod per agent or worker, from the `worker` (or, for the main agent, `agent`) image built with `scripts/remote/Dockerfile` (the Dockerfile stays in the repository: an image is built from a checkout).

```toml
[remote.cluster]
adapter = "pod"
[remote.cluster.settings]
context = "…"                   # kubectl context
namespace = "…"                 # workers; default "default"
agent_namespace = "…"           # agent pods; default: namespace
work_dir = "/work"              # where a new agent's repository is cloned (<work_dir>/<repo name>)
image = "…"                     # default mu-worker:latest
pull_policy = "IfNotPresent"
cpu = "2"                       # requests
memory = "4Gi"
deadline = 21600                # seconds a worker pod may live
agent_deadline = 0              # the same for an agent pod; unset: none
ready_timeout = "300s"
sccache_secret_name = "mu-sccache"   # trusted remotes only
```

- **`start`** creates the pod, copies the config tar (`config.toml`, `models.toml`, `providers.toml`, `AGENTS.md`, `RULES.md`, `agents/`, `skills/`; never `auth.json` or a token file) and then, for an agent, clones `--bundle` into `--dir` when it has one and runs `mu agent --detach` there, or, for a worker, unpacks the bundle into `--dir` and runs `mu worker run --detach`. It reports `{"progress":"scheduling"}`, `"pulling image"`, `"copying repository"` and `"starting"` on stderr. A start that fails deletes its pod.
- **`connect ID`** is `kubectl exec -i POD -- mu attach --stdio ID`. A pod is found by its name (a worker's launch id) or, for an agent, by asking each pod's `mu agent list --json`.
- **`list`** adds `"kind":"agent"` to a line that has no kind.
- **`fetch ID`** prints `mu worker bundle ID` and deletes the pod only after that succeeded. **`stop ID`** tells the worker to stop (`mu worker stop`) and deletes the pod. A dropped stream deletes nothing: the pod lives until a fetch, a stop, its deadline, or the next `start` from the same machine, which deletes this machine's pods that finished or passed their deadline (matched by a random launcher id kept in `~/.mu/remote-launcher`, not by host name).
- **Untrusted remotes** (`MU_REMOTE_TRUSTED=0`): the adapter mounts and passes no secret and refuses `sccache_secret_name`; the pod gets only `SCCACHE_BUCKET`, `SCCACHE_ENDPOINT`, `SCCACHE_REGION`, `SCCACHE_S3_USE_SSL` and `SCCACHE_S3_KEY_PREFIX` from the client's environment, if set. On a trusted remote the secret (default `mu-sccache`, optional) becomes the pod's environment, so workers can write to the shared sccache bucket: one worker can poison another's build.

For the main agent in a trusted cluster, with workers in a second namespace, see [remote-kubernetes.md](remote-kubernetes.md).

`/login` in an agent pod runs `omp login` in the agent process (the client only relays the terminal), so it writes `auth.json` into the pod's omp directory (`$HOME/.omp/agent`), never to the client; the config tar sent by `start` has no credentials and the bundle holds only the repository.

`scripts/remote/test-pod-run.sh` runs the adapter against a fake `kubectl`; pod scheduling, `kubectl wait`, jsonpath on real labels and exec behaviour on a dropped connection need a real cluster.

## Tasks on a remote

`[task] launch = ["task"]` (see [config.md](config.md)) sends `task` subagents of those kinds to `[task] remote`, or `default_remote`, as workers; scouts and kinds not listed stay local. The remote's adapter only moves bytes and runs processes; mu does the rest:

1. mu commits the working tree, untracked files included, as `refs/mu/task/<id>` and writes it with its full history as a git bundle holding that one ref (a repository with a long history sends all of it). It runs `start --kind worker --launch <id> --bundle FILE --dir /work/<id>`: the adapter makes a git checkout of `refs/mu/task/<id>` in the directory (it may map the path) and starts `mu worker run --launch <id> --dir <dir> --detach` there, which prints `{"id":"<id>"}` once it listens.
2. mu runs `connect <id>` (`mu attach --stdio <id>`, which finds the worker by its launch id). The worker reads the task from the first connection, runs `mu --headless` on it in the checkout and keeps running when the connection drops. The model calls and searches of the worker go through this session (see [config.md](config.md)); it holds no credential.
3. When the worker ends it commits what it changed, untracked files included, as `refs/mu/result/<id>` on top of the checkout's commit. mu runs `fetch <id>`, which prints `mu worker bundle <id>`: a git bundle of that ref on top of the snapshot, or nothing when the worker changed nothing. mu checks that the bundle holds exactly `refs/mu/result/<id>` and that its commit descends from the snapshot before it imports it; then it applies it when every file it touches is unchanged since the snapshot, and otherwise keeps the ref and says how to apply it (the jobs panel offers the diff and the apply). It then runs `stop <id>` (`mu worker stop <id>`). A result that cannot be fetched is left on the remote and the worker is not stopped, except when the task was cancelled.

**A lost stream.** The worker numbers every line it sends (`seq`) and keeps those the parent has not acknowledged (the newest 16 MB in memory, the rest in a file). mu reconnects with growing pauses (0.25 s up to 10 s); the first line of every connection, inside the [encrypted channel](#encryption), is `{"type":"resume","after":N}`, N being the last number it has, and the worker replays from N+1, so no line is lost or seen twice. A relay call or an approval that the parent had seen and nobody answered is sent again by the worker, with its id; the parent answers an approval it already answered from memory and does not ask you twice. After `[task] reconnect_timeout` seconds without a stream (600 by default) mu runs `fetch` for what the worker had committed (the task then fails with `lost the stream to worker …`) and, once that succeeded, `stop`. A worker that never gets its task exits after 30 minutes. The state of a worker, which `mu worker bundle` and `stop` read, is in `<profile home>/workers/<id>/`.

**Trust.** An untrusted remote (`trusted = false`, the default) runs a worker only for a repository in its `repos` (its path, or its origin URL with or without `.git`); otherwise the task fails with `remote NAME is not trusted for <repo>`. Before such a launch mu also scans `agents` and `skills` in your agent directory (what the config sent to a remote carries) for files named like credentials (`.env*`, `*.pem`, `*.key`, `id_*`, or with `credential`, `secret` or `token` in the name) and refuses, naming them. `auth.json` and token files are never sent, the task payload carries no secret, and the adapter of an untrusted remote is run with every environment variable whose name has `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL` or `AUTH` in it removed (`SSH_AUTH_SOCK` stays). The repository itself, with its full history, the task text and the transcript of the worker are on the remote: that is accepted.

**Encrypted stream.** The parent and the worker use the same channel as an agent and its client, with keys that exist only in memory: the parent makes a new key for every task, and the worker makes its own when it starts. The stream starts with the handshake (there is no plaintext line: the version check is the task's `muVersion`, which is inside), and `resume`, `task`, the relay and every numbered line follow inside it. The parent pins the worker's key from the first connection and the worker pins the parent's, so a later connection with another key ends the task (`the worker's key is not the one it showed first`) or is refused. Nothing carries the worker's key to the parent out of band, so the first handshake trusts the transport, as for agents; the encryption keeps the transport's middle hops from reading the task and the model traffic, and it does not hide them from whoever runs the node.

`mu worker run|bundle|stop` are hidden subcommands for adapters; `mu agent list --json` shows a worker with `"kind":"worker"`.

## Where your data goes

Which kind of remote may hold what:

- **Trusted** (`trusted = true`): a remote you control. It may run the main agent and hold credentials (`auth.json`, the agent's key, deploy keys, object-store keys). Namespaces, least-privilege service accounts, network policies and volumes encrypted at rest protect against other tenants and against stolen disks or backups, not against whoever administers the cluster.
- **Untrusted** (the default): it runs workers for the repositories in its `repos` only, and never a main agent. Its operator can read the repository with its full history, the task text, the workers' transcripts and the build cache. mu sends it no credential: not `auth.json`, no API key or token, no deploy key, no object-store key (the pod adapter refuses its sccache secret there).

Where each kind of data ends up:

| Data | Where | Until |
|---|---|---|
| The repository, with its full history | The worker's pod, in an `emptyDir` (a bundle is also copied in) | The pod and its volume are gone |
| The task text and the worker's transcript | The worker's session in the pod, and a mirror in your own session store | The pod is deleted; the mirror follows the retention of local sessions (none) |
| Result bundles and refs | `refs/mu/result/<id>` in your repository, and the bundle in the pod until `fetch` succeeds | You delete the ref; unapplied results are kept and listed in the Sessions tab |
| Model traffic | Relayed through your session: the worker holds no model credential, and may spend only on its task's models up to `[task] max_cost` | The provider's own retention |
| The build cache (sccache bucket) | The object store. Workers on a trusted remote share write access, so one worker can poison another's build | You clear the bucket |
| The image blob store | Your profile home (and the agent pod's volumes for a main agent): blobs no session references are removed after 30 days | See [usage.md](usage.md) |
| The main agent's pod (trusted only) | Its volumes: sessions, memory, `auth.json`, the checkouts | You delete the volumes |

The attach and relay streams are [encrypted](#encryption) on top of ssh or `kubectl exec`, which hides them from the API server and the hops in between, not from root on the node the pod runs on. A cluster you do not control and want to run a main agent on is for a later design (confidential containers).

**What state encryption protects, and against whom.** The threat it covers is at *storage time*: someone who reads the volume without being the running agent (a volume snapshot or backup, a stolen disk, another tenant or a storage administrator reading the volume, a StorageClass that turns out not to be encrypted). With [state encryption](#encrypted-state) on, `auth.json` and omp's logins are sealed under a data key that is on the volume only wrapped, for the public keys you chose, so a copy of the volume holds no credential in the clear. It does not cover the operator or root of the node *while the agent runs*: the agent holds the data key in memory, and so can anyone who can read its memory. Sessions, memory, history and artifacts stay readable on the volume (they are omp's format, opened by omp and plugin processes, and they hold code and transcripts), and the agent's own key (`keys/agent.key`) stays in the clear, because the handshake that carries the unlock needs it first: a storage reader who also sits in the transport could pose as the agent and receive the unlock key, which is a runtime attacker and out of scope. So this is defence in depth for the credentials, not a replacement for an encrypted StorageClass.

**omp's credentials with state encryption on.** omp keeps its logins (`omp login`, MCP sign-ins) in `agent.db` in its agent directory, and two loopback tokens (`auth-broker.token`, `auth-gateway.token`) in its config root. omp reads and writes them itself, so they cannot be sealed in place. Instead, once the agent is unlocked, mu restores them from sealed copies in the profile home (`keys/omp-*.sealed`) into a private directory in memory (`$XDG_RUNTIME_DIR` or `/dev/shm` when they are tmpfs; on macOS, which has no such directory, a private one under the temp dir) and starts every omp it runs against that directory: `PI_CONFIG_DIR` names a mirror of omp's config root in which only those three files are real and everything else (sessions, config, caches) is a symlink to the volume. The first unlock after turning encryption on imports the plaintext `agent.db` and tokens, seals them, and removes them from the volume (older snapshots keep them). mu seals the memory copies back (a consistent SQLite snapshot, written atomically) after every omp process it waits for, every 30 seconds while unlocked, and when the agent locks or exits, which also removes the memory directory. A crash in between loses what omp changed since the last write-back: a refresh token omp rotated may then be stale in the sealed copy, and you sign in again. While the agent is locked, anything that needs omp fails with `locked: attach to unlock` instead of running against a missing database. The gateway daemon is a separate process that several sessions share; it keeps using the directory of the agent that started it until it idles out, so lock or stop it with the agent. Files omp creates in that directory for the first time while it runs reach the volume when the agent locks or exits.

## Limits

- Terminal (`pty`) tool calls wait up to a minute for a client to size the terminal, then run without one.
- A session id or `--socket` path has to fit a Unix socket address (104 bytes on macOS).
- One agent per session: a second `mu agent --resume` on a session an agent has fails as any second opener does.
