# Remote use: `mu agent`, `mu --attach`

The agent and the terminal UI can run in two processes, on two machines. `mu agent` runs a session with no terminal and keeps running when you close the laptop; `mu --attach 'COMMAND'` runs the usual UI on your terminal as a client of it, over whatever pipe that command gives (`ssh host mu attach --stdio ID` is the plainest). Everything the UI does works the same attached: the transcript, approvals and questions, plan review, `/model`, `/login`, custom providers, MCP servers, the Sessions tab, `$EDITOR` for a plan or a memory file, terminal (`pty`) tool calls, clickable links in replies, `/open` and `/upload` for files, and `o` in the jobs panel to reach the ports a job listens on.

mu knows nothing about ssh, Kubernetes or any other way to reach a machine. It offers the entry points described here ([For extension authors](#for-extension-authors)) and a separate program uses them: a script of your own, or a tool such as `mu-remote`, which keeps named remotes, starts agents in Kubernetes pods, runs tasks there and encrypts the stream. See that program's own documentation for remotes, trust and an encrypted transport.

## Start an agent

```sh
mu agent [--model M] [--thinking T] [--permissions P] [--resume SESSION | -c] [--detach]
mu agent list [--json]
```

It prints one line, `{"session":"<id>","socket":"<path>"}`, and serves a client on that Unix socket: `<profile home>/run/<session id>.sock`, mode 0600 in a private directory, beside the session's owner record (kind `agent` in the [jobs panel](background-jobs.md#sessions-tab)). There is no network listener: who may connect is who may reach the socket, and reaching one on another machine is `ssh` or `kubectl exec` (below). With no client, the agent goes on with what it was doing; a tool approval or a question waits for a client. A `-p` run holding the session you `--resume` is stopped after its current step first, as `mu take` does, without asking.

`--detach` starts the agent in the background, in a session of its own, with SIGHUP ignored, so closing the terminal or the ssh connection that started it does not end it. Its output goes to `<profile home>/logs/agent-<id>.log`. The command returns once the socket listens and prints one line, `{"id":"<id>"}`. `mu agent list` shows the agents and `mu agent list --json` prints one JSON object per line per live agent: `{"id","cwd","started" (unix seconds),"title" (or null),"client" ({"host","since"} or null),"waiting","locked"}`. `client`, `waiting` and `locked` come from the agent's owner record, so the Sessions tab shows them too (`waiting` is added to the agent's kind).

`/new`, `/resume` and `/fork` inside the session move the socket to the new session's id; the client stays connected. `--permissions` takes effect for tool calls as usual, so an unattended agent should not be in `ask` mode unless you will attach to answer.

The agent ends on `/quit` or Ctrl-D in a client, on `SIGTERM`, `SIGHUP` or `SIGINT` (it ends the session's jobs and plugins first; a second signal exits at once), and from the Sessions tab (`S`). It does not end when a client goes away.

## Attach

```sh
mu --attach 'ssh devbox mu attach --stdio 3f2a'                  # join agent 3f2a on devbox
mu --attach 'kubectl exec -i agent-0 -- mu attach --stdio 3f2a'   # …or in a pod
mu --attach 'mu attach --stdio' --state-key ~/.ssh/work_ed25519   # this machine's only agent, unlocking it with that key
```

`mu attach --stdio [SESSION]` runs on the agent's machine: a raw pipe that copies lines between its stdin and stdout and the agent's socket (with no session it finds the one agent running in the current directory, `--socket PATH` names a socket). `mu --attach COMMAND` runs `COMMAND` under `sh -c` and speaks the protocol over its stdin and stdout; the command's stderr lines show as notices. When the command ends the UI stays up and runs it again.

The first line the client sends names a wire number (`WIRE`, now 6) and its version, and a different wire number is refused with a readable message. Display settings (`theme`, `notify`, `bell`, `hide_thinking_block`, `smooth_streaming`, the `[status]` levels) and composer history come from the client's own config and profile home; everything about models, keys, MCP servers, sessions and plugins is the agent's.

A client sees what it missed when it attaches: the stored conversation up to the turn in progress, that turn's events (up to 20 000 events or 16 MB; past that the oldest blocks are dropped with a notice), the latest status, jobs, queue and todo, and every question, approval, plan review or terminal request still waiting. Nothing rings until it is caught up.

- **One client at a time.** The client's hello says its host name and a client id that stays the same across its connections. An agent that already has another client answers `Busy` instead of letting go of it. Attaching by hand then asks `<host> has this session since <time>: [t]ake it over, [q]uit` before the screen is drawn; a client that lost its connection and finds the session taken stops retrying and asks in the UI (`y` takes it over, `n` quits). A taken-over client prints why and exits. The agent records the client's host and the time in its owner record, so `mu agent list` shows who has it.
- **`/detach`** closes the client and leaves the agent running. `/quit` ends the agent. A plain `mu` answers `/detach` with `not attached`.
- **A lost connection** keeps the UI up, says `connection lost; reconnecting`, and runs the command again after 1 s, then 2, 4 … up to 30 s. When it connects, the screen is cleared and the conversation repainted from the agent's catch-up. Ctrl-C while reconnecting quits.
- **Dialogs opened before the loss** stay on screen after a reconnect and are not opened a second time for the same request. The catch-up lists the requests the agent still waits for; a dialog for any other one was answered elsewhere and is closed.

**Privacy of the stream.** mu does not encrypt it. Every line is plain JSON, so the stream is exactly as private as the command that carries it: an ssh connection or a `kubectl exec` hides it from the network, but the API server of a cluster, a proxy or anything else in the path of a hand-typed `--attach` command sees all of it. That includes the lines that carry secrets: an API key you save with `/login` or `mu auth` from the client (`save_key`), a ChatGPT sign-in address you paste (`chatgpt_paste`), an MCP sign-in address you paste (`mcp_login_paste`) and the data key that unlocks a sealed agent (`state_unlock`, below). Use a command whose transport you trust for those, or a program that adds an encrypted channel and ends it next to the agent, such as `mu-remote`. The agent's socket is private (mode 0600 in a 0700 directory), so a bridge that runs as the same user on the agent's machine and reaches the socket loses nothing by the move: whoever can open the socket can already read the agent's memory and `auth.json`.

### When nobody is attached

A tool approval or a question that comes up with no client is kept by the agent. At once, it sets `waiting` in its owner record (the Sessions tab and `mu agent list` show it) and runs `[agent] notify_command`, if set, under `sh -c` with the session id as `$1` and a one-line summary as `$2`. With `[agent] approval_timeout = N` (seconds; off by default) an approval still waiting after `N` seconds is answered as unavailable, so the model is told no one could approve the call, a question is cancelled, and the agent notes it in its log. `waiting` clears when the request is answered or a client attaches.

### Encrypted state

With `[agent] state_key = "ssh"` (or a key source, below) the agent keeps its credentials sealed at rest (`auth.json` and plugin secrets, see [Where your data goes](#where-your-data-goes)) under a data key stored only wrapped, in `<profile home>/keys/state.key`, for the SSH ed25519 public keys that may unlock it. There is no passphrase source and RSA keys are not supported (`ssh-keygen -t ed25519`). State encryption is on for a profile exactly when `keys/state.key` exists.

- **First attach.** An agent with `state_key = "ssh"` and no state key tells the first client that attaches: `the agent has no state key yet, so its credentials are not encrypted`, shows the public key from `<private key>.pub` (`--state-key`, default `~/.ssh/id_ed25519`) and asks `[y/N]` before the screen is drawn. On `y` the agent makes the data key, wraps it for that key, seals what was stored in the clear and removes the plaintext. Older snapshots of the volume keep the plaintext on copy-on-write storage. A client that cannot ask (no terminal) only says so, and the agent stays as it is. More recipients (a second laptop, a backup key) are added from an unlocked agent with the `state_add_recipient` host call and removed with `state_remove_recipient` (no command for them yet); removing one does not take back a data key it already unwrapped.
- **After a restart** the agent is *locked*: the data key was never on the volume. It starts and listens, `mu agent list --json` shows `"locked":true`, the Sessions tab shows `locked`, `waiting` is set in the owner record and `[agent] notify_command` runs once with `locked: attach to unlock`. A turn that needs a credential fails with `locked: attach to unlock`, and so does any read or write of `auth.json`. The model chosen at start is applied once the agent is unlocked.
- **Attaching to a locked agent** unlocks it before the screen is drawn: the client takes the wrapped keys the agent offers, decrypts the entry for its key with the private key file named by `--state-key` (ssh-agent cannot decrypt, so the file is read directly), asking for its passphrase on the terminal without echo (three tries) when it is encrypted, and returns the data key in a `state_unlock` host call that the agent zeroes after use (a few wrong keys per minute are tried at most). That call crosses the attach stream, so the data key is as private as the stream (see [Privacy of the stream](#attach)). A key that is not a recipient is refused with `… is not one of the keys that unlock it: the agent accepts SHA256:…`. A client that cannot unlock (no key file, no terminal, a wrong passphrase three times) still attaches, and the agent stays locked and says so.
- **A reconnect** to an agent that is still unlocked asks nothing. To one that restarted locked, a client whose key has no passphrase unlocks it again; one that needs a passphrase cannot ask with the UI up, and says to attach again.
- **Unattended agents.** `[agent] state_key_file = "/path"` or `state_key_command = "cmd"` give a 32-byte key (raw bytes, or 64 hex characters) read from the file or printed by the command at start (the file wins). The agent then makes its state key from it on the first start (this turns state encryption on) and opens it on every later one, so it restarts without a client; a wrong key is a start error. The trust is the source's: a key from a Kubernetes Secret is as exposed as that Secret and its backups, so use it only when the Secret store is protected at least as well as the volume. The keys are of one kind per profile: a profile made for SSH recipients refuses a key source.

Sealing is general: a laptop agent can use it as well as one in a pod.

## Sign-ins and your editor

`/login`, an MCP server's OAuth sign-in and the ChatGPT sign-in run on the agent's machine, because that is where the credentials go. Each is a flow in the composer, not a program on a terminal: the sign-in URL opens in your browser, and where the browser cannot reach the agent's loopback callback (ChatGPT listens on `127.0.0.1:1455`; an MCP server's callback port is its own), you paste the address your browser ends up at into the composer. Esc cancels. `$EDITOR` runs on your machine, on a private copy of a plan or memory file, and the changed text is written back to the agent's machine when the editor exits successfully. [Open and upload](#open-upload-and-ports) cover other files, and the ports a job listens on.

## Open, upload and ports

`/open PATH` shows you a file of the agent's machine without leaving the client: it copies it here (into a private temp directory, removed when mu exits) and runs `$EDITOR` on the copy when it is text, with the notice `opened a copy; changes are not sent back`, or hands it to the system's opener otherwise, refusing the extensions that would run (`app`, `exe`, `command`, …). A trailing `:N` or `:N-M` is dropped, `~`, a relative path and `local://` resolve on the agent's machine, and `/open` with no path lists files the agent's tools touched or its replies mentioned, including replayed replies. Mentioned paths are detected heuristically without checking existence. `/upload PATH…` goes the other way: it sends files of the client's machine to `<session local dir>/uploads/<8 hex>-<basename>` on the agent's machine and puts `local://uploads/…` in the composer, for the agent's tools to resolve as usual. Pasting a path that names an existing file here only adds a hint about `/upload`; nothing is sent unless you ask.

The bytes move through [`MU_ATTACH_HELPER`](#for-extension-authors) when the program that started the client set it, and otherwise over the attach stream (32 MiB at most). The ports a `bash` job listens on arrive with its job: `o` in the [jobs panel](background-jobs.md#jobs-panel) forwards them here through the helper and opens the lowest, and they are taken back when the job ends or mu exits. Without a helper, mu says it cannot forward and prints the `ssh -L` line to run instead.

Replies in an attached client carry clickable URLs: `[display] hyperlinks` ([config.md](config.md)) writes them as OSC 8 links, which kitty, iTerm2, wezterm, foot, Ghostty and most others open on click. Under tmux, tell it the terminal handles them, with `set -as terminal-features ',*:hyperlinks'` (the `hyperlinks` feature is documented as "supports OSC 8 hyperlinks"). A terminal that ignores them shows plain text.

## Taking over a `-p` run

`t` in the Sessions tab and `/resume` on a session a `mu -p` still has work as locally: the agent stops the run after its current step, starts a second agent on its session, and your terminal shows a nested client of that agent (`mu --attach`, through a tunnel on the first connection) until you leave it. Quitting the nested client ends the second agent; `/detach` leaves it running (`mu agent list` shows it).

## For extension authors

A program that reaches other machines uses five contracts and nothing else of mu. Each is versioned: a change to one is a change to its number here.

**A. Launcher, contract 2.** `[task] launcher = ["prog", "arg", …]` and `[task] launch = ["task", …]` (the agent kinds that use it: `task`, `scout`, a custom agent's name; `/review` and the agents of a subagent stay local) in `config.toml`.

- mu runs the launcher in the task's directory in place of `mu --headless`, in a process group of its own. Its stdin and stdout carry the subagent stream exactly as a local child's do: the first stdin line is the task payload (`task`, `context`, `agent`, `parentSession`, `muVersion`, `permissions`, `relay` with `relayModels` and `relayTools`, `mirrorSession`, `outputSchema`, `model`, `depth`), later stdin lines are answers to the child's requests (`approval`, and the relay's), and the stdout lines are the child's events: `tool`, `model`, `entry` (a line of its session file), `approve`, `relay` (a model or search call) and last `result`. A launcher that runs `mu --headless` itself passes the stream through unchanged; one that runs it elsewhere must carry every line both ways. A child that is sent `relay` has no credential: its model and search calls come back to this session as `relay` lines (see [config.md](config.md)).
- Environment (on top of mu's own; no credential is added): `MU_TASK_CONTRACT=2`, `MU_TASK_DEPTH` (this subagent's depth, 1 for a child of the main agent), `MU_TASK_NAME` (the launch id: a slug of the task's name and 8 hex digits, a valid pod name and ref component), `MU_TASK_ROOT` (the git repository's root), `MU_TASK_CWD`, `MU_TASK_REF` (`refs/mu/task/<launch id>`), `MU_TASK_SESSION` (the parent's session file, when it has one), `MU_TASK_PARENT_PID`, `MU_TASK_MU_HOME`, `MU_TASK_PROFILE_HOME` and `MU_TASK_AGENT_DIR` (the parent's omp agent directory).
- `MU_TASK_BUNDLE`: a git bundle holding exactly `MU_TASK_REF`, the commit of the working tree (untracked files included) on top of `HEAD`, with its full history. mu deleted nothing yet; it removes the ref and the bundles when the run ends.
- `MU_TASK_RESULT_BUNDLE`: a path in the same private directory, an empty file to begin with. The launcher writes the result there as a bundle holding exactly `refs/mu/result/<launch id>` (the checkout's changes committed on top of the snapshot), or leaves it empty when nothing changed.
- stderr: `{"progress":"pulling image"}` lines go to the job's row in the jobs panel (until the launcher sends a line on stdout; later ones show while they last). Other lines are kept, the last twenty, as the error text when the task has no result.
- End: when the launcher exits, mu imports the result bundle (it must hold nothing but that ref, and the commit must descend from the snapshot; otherwise nothing is kept and the task says so). It then applies the changes when every file they touch is unchanged since the snapshot, and otherwise keeps the ref and says how to apply it; the jobs panel offers the diff (`d`) and the apply (`a`). A task that failed or was cancelled leaves its changes unapplied. After stdout closes mu waits for the launcher to exit for up to five minutes, then kills its group.
- Cancel: mu sends `TERM` to the group and `KILL` thirty seconds later. The launcher stops its worker, writes whatever result it can, and exits. Reconnecting, buffering and giving up are the launcher's business: to mu the stream is one pipe, so a launcher that reconnects must number and keep the lines it sends in both directions itself.
- A subagent ignores `[task] launcher`: its own subagents run locally.

**B. Agents.** `mu agent [--detach] [--resume ID | -c] [--model M]` prints `{"id":ID}` (detached) or `{"session":ID,"socket":PATH}` once it listens. `mu agent list --json` lists live agents with their client, `waiting` and `locked`. `mu attach --stdio ID` is a raw pipe to the agent's socket. The protocol on it is the one in `mu_base::wire`: `ClientLine` and `AgentLine`, JSON lines, the client's `hello` first.

**C. Attach client.** `mu --attach '<command>' [--state-key PATH]`: see [Attach](#attach). `--state-key` is the SSH ed25519 private key that unlocks a sealed agent, `~/.ssh/id_ed25519` by default.

**D. Session sources.** `[sessions] sources = [["prog", "arg", …], …]` in `config.toml`. Each command prints JSON lines in the shape of `mu agent list --json` (`id` is required, the rest optional), plus `host` (the machine, shown in a column), `hint` (how to open the row, shown in the Sessions tab's status line) and `attach` (a command that opens it: `Enter` hands the terminal to `mu --attach '<that command>'` and comes back to the tab). mu runs the commands off the UI thread, all at once, each with a 20 s limit, whenever the tab refreshes; a command that fails adds no rows. A row is not a local session: it has no pid, and only `Enter` does anything on it.

**E. Attach helper, contract 1.** `MU_ATTACH_HELPER` in the environment of the `mu --attach` client: a shell command line. mu runs `sh -c '<helper> "$@"' mu-attach-helper VERB ARGS…` and passes the arguments as separate words, so the helper may take arguments of its own (the `$EDITOR` pattern). Paths are on the agent's machine.

| Verb | stdin | stdout | |
|---|---|---|---|
| `forward PORT` | - | one JSON line `{"local":N}` | Make `127.0.0.1:N` on this machine reach `localhost:PORT` on the agent's machine, then return and leave it open |
| `unforward PORT LOCAL` | - | - | Remove that forward |
| `read PATH` | - | the file's bytes | Read a regular file |
| `write PATH` | the bytes | - | Create PATH (and parent directories), mode 0600; fail if it exists |

- Failure is a non-zero exit and one line of error text per line on stderr. Exit code **64** means "not supported here": mu then falls back to `read_bytes` and `write_bytes` host calls over the attach stream (32 MiB at most) for files, and reports that this remote cannot forward ports.
- mu allows 30 s for `forward` and `unforward`, 10 minutes for `read` and `write`, and 512 MiB through the helper; it kills the helper's process group when a read goes past that.
- `read_bytes` and `write_bytes` are host calls like the others, available with or without a helper: `read_bytes {path, max}` answers `{size, data_b64}`, and `write_bytes {path, data_b64}` creates a file that must not exist. Neither has a list of allowed paths, and it does not need one: a client that can attach is already as trusted as the agent's socket owner, who can read `auth.json`.

## Where your data goes

What mu itself moves between machines when you use these entry points:

| Data | Where | Until |
|---|---|---|
| A launched task's repository, with its full history | The launcher's bundle (`MU_TASK_BUNDLE`), wherever the launcher puts it | The run ends (mu deletes the bundle and the ref) |
| The task text and the worker's transcript | The worker's own session, and a mirror in your session store | The mirror follows the retention of local sessions (none) |
| Result bundles and refs | `refs/mu/result/<id>` in your repository | You delete the ref; unapplied results are kept 14 days and listed in the Sessions tab |
| Model traffic | Relayed through your session: a launched worker holds no model credential, and may spend only on its task's models up to `[task] max_cost` | The provider's own retention |
| The image blob store | Your profile home (and the agent's, for an agent): blobs no session references are removed after 30 days | See [usage.md](usage.md) |
| The attach stream | The command's transport, in plain JSON (see [Privacy of the stream](#attach)) | The transport's own retention |

mu sends a launcher no credential: its environment is mu's own plus the `MU_TASK_*` paths, the bundle holds only the repository and the payload carries no secret. What the launcher does next, to which machines and under what trust, is the launcher's, and its documentation says so.

**What state encryption protects, and against whom.** The threat it covers is at *storage time*: someone who reads the volume without being the running agent (a volume snapshot or backup, a stolen disk, another tenant or a storage administrator reading the volume, a StorageClass that turns out not to be encrypted). With [state encryption](#encrypted-state) on, `auth.json` and plugin secrets are sealed under a data key that is on the volume only wrapped, for the public keys you chose, so a copy of the volume holds no credential in the clear. It does not cover the operator or root of the node *while the agent runs*: the agent holds the data key in memory, and so can anyone who can read its memory. Sessions, memory, history and artifacts stay readable on the volume (they are omp's format, opened by omp and plugin processes, and they hold code and transcripts). So this is defence in depth for the credentials, not a replacement for an encrypted StorageClass.

**Plugin secrets with state encryption on.** mu runs no omp and holds none of its files. A native plugin that needs to keep a credential asks mu for it through `core/secrets` (see the plugin calls in [DESIGN.md](../DESIGN.md#plugin--core-calls)): a key-value store under `<profile home>/secrets/plugin-<name>/`, one namespace per plugin that the plugin cannot choose. With state encryption on each value is sealed like `auth.json` and an agent that is locked answers `locked: attach to unlock`; with it off the files are plain, mode 0600. A gateway plugin keeps its credentials there when it stores them (`mu-gateway` seals omp's `agent.db` and loopback tokens this way, and says how in its README). MCP OAuth tokens are kept the same way, in the `mcp-oauth` namespace through `core/mcpSecret`. Sealed copies of omp's credentials that an earlier mu kept as `keys/omp-*.sealed` are moved once, at the first unlock, into the `gateway` plugin's namespace (`plugin-gateway`) and the old files removed. A login made outside mu (for example `mu-gateway login` on the agent) is imported into the sealed store only when no sealed copy exists yet; otherwise the sealed copy wins.

## Limits

- Terminal (`pty`) tool calls wait up to a minute for a client to size the terminal, then run without one.
- A session id or `--socket` path has to fit a Unix socket address (104 bytes on macOS).
- One agent per session: a second `mu agent --resume` on a session an agent has fails as any second opener does.
