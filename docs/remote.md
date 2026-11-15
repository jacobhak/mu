# Remote use: `mu agent`, `mu --attach`

The agent and the terminal UI can run in two processes, on two machines. `mu agent` runs a session with no terminal and keeps running when you close the laptop; `mu --attach` runs the usual UI on your terminal as a client of it. Everything the UI does works the same attached: the transcript, approvals and questions, plan review, `/model`, `/login`, custom providers, MCP servers, the Sessions tab, `$EDITOR` for a plan or a memory file, terminal (`pty`) tool calls. The plain `mu` command is unchanged.

## Start an agent

```sh
mu agent [--model M] [--thinking T] [--permissions P] [--resume SESSION | -c] [--detach]
mu agent list [--json]
```

It prints one line, `{"session":"<id>","socket":"<path>"}`, and serves a client on that Unix socket: `<profile home>/run/<session id>.sock`, mode 0600, beside the session's owner record (kind `agent` in the [jobs panel](background-jobs.md#sessions-tab)). There is no network listener: who may connect is who may reach the socket, and reaching one on another machine is `ssh` or `kubectl exec` (below). With no client, the agent goes on with what it was doing; a tool approval or a question waits for a client. A `-p` run holding the session you `--resume` is stopped after its current step first, as `mu take` does, without asking.

`--detach` starts the agent in the background, in a session of its own, with SIGHUP ignored, so closing the terminal or the ssh connection that started it does not end it. Its output goes to `<profile home>/logs/agent-<id>.log`. The command returns once the socket listens and prints one line, `{"id":"<id>"}`; `mu agent list` shows the agent and `mu agent list --json` prints one JSON object per line per live agent: `{"id","cwd","started" (unix seconds),"title" (or null),"client" ({"host","since"} or null),"waiting"}`. `client` and `waiting` come from the agent's owner record, so the Sessions tab shows them too (`waiting` is added to the agent's kind).

`/new`, `/resume` and `/fork` inside the session move the socket to the new session's id; the client stays connected. `--permissions` takes effect for tool calls as usual, so an unattended agent should not be in `ask` mode unless you will attach to answer.

The agent ends on `/quit` or Ctrl-D in a client, on `SIGTERM`, `SIGHUP` or `SIGINT` (it ends the session's jobs and plugins first; a second signal exits at once), and from the Sessions tab (`S`). It does not end when a client goes away.

## Attach

```sh
mu --attach 'COMMAND'          # the UI, as a client of the agent COMMAND reaches
```

`mu --attach 'COMMAND'` runs `COMMAND` under `sh -c` and speaks the protocol over its stdin and stdout; its stderr lines show as notices. `COMMAND` is a raw pipe to an agent's socket: `mu attach --stdio [SESSION]` (a hidden subcommand; it copies lines between its stdin and stdout and the socket and knows nothing of them; with no session it finds the one agent running in the current directory, `--socket PATH` names a socket). On the same machine:

```sh
mu --attach 'mu attach --stdio'
```

To another machine or a pod:

```sh
mu --attach 'ssh host mu attach --stdio ID'
mu --attach 'kubectl exec -i pod -- mu attach --stdio ID'
```

Both ends must run the same mu build: the first line each way names a wire number, and a different one is refused with a message. Display settings (`status_line`, `theme`, `notify`, `bell`, `hide_thinking_block`, `smooth_streaming`, the `[status]` levels) and composer history come from the client's own config and profile home; everything about models, keys, MCP servers, sessions and plugins is the agent's.

A client sees what it missed when it attaches: the stored conversation up to the turn in progress, that turn's events (up to 20 000 events or 16 MB; past that the oldest blocks are dropped with a notice), the latest status, jobs, queue and todo, and every question, approval, plan review or terminal request still waiting. Nothing rings until it is caught up.

- **One client at a time.** The client's first line says its host name and a client id that stays the same across its connections. An agent that already has another client answers `Busy` instead of letting go of it. Attaching by hand then asks `<host> has this session since <time>: [t]ake it over, [q]uit` before the screen is drawn; a client that lost its connection and finds the session taken stops retrying and asks in the UI (`y` takes it over, `n` quits). A taken-over client prints why and exits. The agent records the client's host and the time in its owner record, so `mu agent list` shows who has it.
- **`/detach`** closes the client and leaves the agent running. `/quit` ends the agent. A plain `mu` answers `/detach` with `not attached`.
- **A lost connection** keeps the UI up, says `connection lost; reconnecting`, and runs the command again after 1 s, then 2, 4 … up to 30 s. When it connects, the screen is cleared and the conversation repainted from the agent's catch-up. Ctrl-C while reconnecting quits.
- **Dialogs opened before the loss** stay on screen after a reconnect and are not opened a second time for the same request. The catch-up lists the requests the agent still waits for; a dialog for any other one was answered elsewhere and is closed.

### When nobody is attached

A tool approval or a question that comes up with no client is kept by the agent. At once, it sets `waiting` in its owner record (the Sessions tab and `mu agent list` show it) and runs `[agent] notify_command`, if set, under `sh -c` with the session id as `$1` and a one-line summary as `$2`. With `[agent] approval_timeout = N` (seconds; off by default) an approval still waiting after `N` seconds is answered as unavailable, so the model is told no one could approve the call, a question is cancelled, and the agent notes it in its log. `waiting` clears when the request is answered or a client attaches.

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
| `start --kind agent [--resume ID \| -c] [--dir PATH] [--model M]` | starts a detached `mu agent` there | `{"id":ID}` once it listens |
| `start --kind worker --launch ID --bundle FILE --dir PATH` | starts a detached worker | `{"id":ID}` |
| `list` | the agents and workers there | one JSON object per line, as `mu agent list --json` prints them |
| `connect ID` | a stdio pipe to that agent's socket | the stream |
| `fetch ID` | the worker's result bundle | the bundle's bytes |
| `stop ID` | ends it | nothing |

### The ssh adapter

`ssh` needs no program: it runs `ssh -T -o BatchMode=yes HOST mu agent --detach …` for `start`, `… mu agent list --json` for `list` and `… mu attach --stdio ID` for `connect`, with every argument quoted for the remote shell. `mu` must be on the PATH of a non-interactive ssh login there. `BatchMode=yes` means a host that wants a password fails, with a hint to load your key into ssh-agent. `start --kind worker`, `fetch` and `stop` are not supported over ssh. `MU_SSH_PROGRAM` replaces the `ssh` program (the tests use a fake).

## Limits

- Terminal (`pty`) tool calls wait up to a minute for a client to size the terminal, then run without one.
- A session id or `--socket` path has to fit a Unix socket address (104 bytes on macOS).
- One agent per session: a second `mu agent --resume` on a session an agent has fails as any second opener does.
