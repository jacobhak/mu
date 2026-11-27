# Remote contracts: what a program that reaches other machines builds on

mu's core knows no transport: nothing in it speaks ssh, Kubernetes or any other way to reach a machine. A separate program plugs in through five contracts, and nothing else of mu. [`mu-remote`](../plugins/remote/README.md) (in `plugins/remote`, shipped with mu) is one such program, and the worked example; anyone can write another, in any language.

```
 this machine                                        far side
 ┌──────────────────────────┐  attach command   ┌──────────────────────────────┐
 │ mu --attach 'COMMAND'    │ ───stdin/stdout──▶│ mu attach --stdio ID         │
 │ (the terminal UI)        │ ◀─────────────────│   │ raw lines                │
 └──────────────────────────┘   (your transport)│   ▼                          │
                                                │ mu agent (the session)       │
                                                └──────────────────────────────┘
```

The transport is whatever `COMMAND` runs over: `ssh`, `kubectl exec -i`, a socket helper of your own. mu sees one pipe.

## Versions

| Contract | Name | Version | Environment |
|---|---|---|---|
| A | launcher | 2 | `MU_TASK_CONTRACT` |
| B | agents | 1 | - |
| C | attach | 1 | - |
| D | sources | 2 | `MU_SOURCES_CONTRACT` |
| E | helper | 1 | `MU_ATTACH_HELPER_CONTRACT` |
| - | wire | 8 | - |

`mu contracts` prints the current numbers as one JSON line: `{"wire":8,"launcher":2,"agents":1,"attach":1,"sources":2,"helper":1}`. mu sets the environment variable in the environment of the program it runs under that contract (A: the launcher; D: each session source; E: the attach helper). A program that speaks one number checks it and refuses another, with a message that names both.

**Policy.** A contract's number goes up when a program written for the old number could break: a removed or renamed field, verb or flag, a changed meaning, a new required input. An optional addition keeps the number and gets a line in the contract's history list below. A program may therefore ignore fields and verbs it does not know, and must.

**The wire** is the protocol between an attached UI and its agent (`WireCommand`, `WireEvent`, the host calls). It is internal to mu and changes with every release that touches it, so both ends of an attach must be the same mu release. A transport carries it opaquely, line by line, and must not parse or buffer it. Only the first line of each side is stable:

- client to agent: `{"hello":{"wire":N,"version":"…", …}}` (more fields may follow);
- agent to client: `{"hello":{"wire":N,"version":"…", …}}`, or, when the numbers differ, `{"error":"this agent speaks wire 8, the client wire 7"}` and the agent closes the connection.

## A. Launcher (contract 2)

`[task] launcher = ["prog", "arg", …]` and `[task] launch = ["task", …]` in `config.toml`. `launch` names the agent kinds that use it (`task`, `scout`, a custom agent's name); `/review` and the subagents of a launched subagent stay local.

mu runs the launcher in the task's directory, in place of `mu --headless`, in a process group of its own. Its stdin and stdout carry the subagent stream exactly as a local child's do.

**stdin, first line: the task payload**, a JSON object:

| Field | Type | |
|---|---|---|
| `task` | string | what to do |
| `context` | string or null | shared background |
| `agent` | string | the agent kind (`task`, `scout`, a custom name) |
| `parentSession` | string or null | the parent's session file |
| `muVersion` | string | the parent's build; the worker's mu must match it exactly, or it refuses the task |
| `permissions` | object | the parent's permission mode and rules: `mode`, `session`, `searched`. A `yolo` parent sends `yolo` |
| `relay` | bool | the child has no credential: its model and search calls come back as `relay` lines |
| `relayModels` | array | the models a relayed child may ask for (with `relay`) |
| `relayTools` | array | tools the parent serves to a relayed child (with `relay`, when there are any) |
| `mirrorSession` | bool | send the child's session file as `entry` lines |
| `outputSchema` | object | typed output wanted (when set) |
| `model` | string | the model chosen for this task (when set) |
| `depth` | integer | this subagent's depth, 1 for a child of the main agent |

Later stdin lines are answers to the child's requests: `{"type":"approval","id":N,"decision":…}` and the relay's answers. A launcher that runs `mu --headless` itself passes the stream through unchanged; one that runs it elsewhere must carry every line both ways, and must number and keep the lines it sends in both directions if it reconnects, because to mu the stream is one pipe.

**stdout: the child's events**, one JSON object per line, with a `type`:

| `type` | Fields | |
|---|---|---|
| `tool` | `name`, `summary` | a tool call began (shown in the jobs panel) |
| `model` | `label` | the model in use |
| `entry` | `file`, `line` | one line of the child's session file (with `mirrorSession`) |
| `approve` | `id`, `request` | a tool call needs approval; mu answers with an `approval` line |
| `relay` | `id`, `call`, … | a model or search call of a relayed child; mu answers on stdin |
| `result` | `isError` (bool), `text` (string), `data` (any), `schemaOk` (bool) | the end |

A line mu cannot read is dropped (and quoted as the error text when no result comes). A `result` line does not end the stream: mu reads to end of file and then waits for the launcher to exit, up to 300 s, then kills its group. The launcher's exit code is ignored: a failure is the result's `isError`, or, with no result, the last 20 stderr lines.

**Environment** (on top of mu's own): `MU_TASK_CONTRACT=2`, `MU_TASK_DEPTH`, `MU_TASK_NAME` (the launch id: a slug of the task's name and 8 hex digits, a valid pod name and ref component), `MU_TASK_ROOT` (the git repository's root), `MU_TASK_CWD`, `MU_TASK_REF` (`refs/mu/task/<launch id>`), `MU_TASK_SESSION` (the parent's session file, when it has one), `MU_TASK_PARENT_PID`, `MU_TASK_MU_HOME`, `MU_TASK_PROFILE_HOME` and `MU_TASK_AGENT_DIR`.

- `MU_TASK_BUNDLE`: a git bundle holding exactly `MU_TASK_REF`, the commit of the working tree on top of `HEAD`, with its full history. The working tree includes untracked files that are not ignored. mu deletes the ref and the bundles when the run ends.
- `MU_TASK_RESULT_BUNDLE`: a path in the same private directory, an empty file to begin with. The launcher writes the result there as a bundle holding exactly `refs/mu/result/<launch id>` (the checkout's changes committed on top of the snapshot), or leaves it empty when nothing changed.
- stderr: `{"progress":"pulling image"}` lines go to the job's row in the jobs panel. Other lines are kept, the last twenty.
- **Security.** The launcher inherits mu's whole environment, which may hold API keys (`ANTHROPIC_API_KEY`, `MU_GATEWAY_*`); it must not forward it to a machine you do not control. A relayed child needs no credential.
- End: when the launcher exits, mu imports the result bundle (it must hold nothing but that ref, and the commit must descend from the snapshot; otherwise nothing is kept and the task says so). It then applies the changes when every file they touch is unchanged since the snapshot, and otherwise keeps the ref and says how to apply it. A task that failed or was cancelled leaves its changes unapplied.
- Cancel: mu sends `TERM` to the group and `KILL` thirty seconds later. The launcher stops its worker, writes whatever result it can, and exits.

## B. Agents (contract 1)

- `mu agent [--detach] [--resume ID | -c] [--model M] [--thinking T]` runs a session with no terminal. It prints `{"session":ID,"socket":PATH}` once it listens, or, with `--detach` (which leaves the terminal and logs to `<profile home>/logs/agent-<id>.log`), `{"id":ID}`. `--detached` (what `--detach` starts) is for mu's own use, and so is `mu --headless`, the local child that a launcher stands in for.
- `mu agent list --json` prints one JSON line per live agent here, oldest first: `id` (string), `kind` (`"agent"`), `cwd` (string), `started` (unix seconds), `title` (string or null), `client` (`{"host":…,"since":…}` or null: who is attached), `waiting` (bool: a request waits for a client), `locked` (bool: sealed state not yet unlocked).
- `mu agent stop ID` ends a running agent here with SIGTERM, waits up to 15 s for it to go and exits 1 if it still runs. `mu agent cancel ID` cancels its current turn. `mu notify ID TEXT|-` leaves it a message (`-` reads the text from stdin). ID is a session id or a prefix of one.
- `mu attach --stdio [ID] [--socket PATH]` is a raw pipe between its stdin and stdout and the agent's Unix socket. With no ID it finds the one agent running in the current directory; `--socket` names a socket instead. The socket is private (mode 0600 in a 0700 directory): whoever can run this can already read the agent's memory and `auth.json`.

## C. Attach client (contract 1)

`mu --attach COMMAND [--state-key PATH]` runs the usual terminal UI as the client of an agent over the pipe `COMMAND` gives. `COMMAND` runs under `/bin/sh -c`; its stdin and stdout are the stream, and each stderr line shows as a notice (before the first hello they are kept as the error text, so an adapter's diagnostics reach the user). mu waits 30 s for the agent's hello. When the command exits, mu says `connection lost; reconnecting` and runs it again after 1 s, then 2, 4 … up to 30 s; the conversation is repainted from the agent's catch-up. `--state-key` is the SSH ed25519 private key that unlocks a sealed agent (`~/.ssh/id_ed25519` by default).

**The stream is plain JSON lines.** mu does not encrypt it, so it is as private as `COMMAND`'s transport: ssh or `kubectl exec` hides it from the network, but the API server of a cluster, a proxy or anything else in the path sees all of it, including a key saved with `/login`, a ChatGPT or MCP sign-in address pasted into the client, and the data key that unlocks a sealed agent. A transport that adds an encrypted channel and ends it next to the agent (as `mu-remote` does) protects those too.

## D. Session sources (contract 2)

`[sessions] sources = [["prog", "arg", …], …]` in `config.toml`. mu runs each command **as an argument vector, with no shell**, all at once, each with a 20 s limit, off the UI thread, whenever the Sessions tab refreshes. `MU_SOURCES_CONTRACT=2` is in its environment. A command that fails or is slow adds no rows. Each prints one JSON object per line (a line that is not one is skipped):

| Field | Type | |
|---|---|---|
| `id` | string | required |
| `cwd`, `started`, `title`, `client` (`{host, since}`), `waiting`, `locked` | | as in `mu agent list --json` |
| `host` | string | the machine, shown in a column; the source's name when absent |
| `hint` | string | how to open the row, shown in the tab's status line when there is no `attach` |
| `attach` | array of strings, or string | what opens it: `Enter` hands the terminal to `mu --attach` with it, and comes back to the tab |
| `helper` | array of strings, or string | becomes `MU_ATTACH_HELPER` for that attach (contract E) |
| `state_key` | string | becomes `--state-key` for that attach |
| `stop` | array of strings | ends the agent there |
| `cancel` | array of strings | cancels the agent's current turn |
| `notify` | array of strings | leaves the agent a message: it arrives on stdin, one line |

`attach` and `helper` as an array are an argument vector: mu quotes each word for `sh` and joins them, so an agent id from a remote never reaches a shell unquoted. This is the preferred form. A string is a shell line run as written, and so is the source's responsibility to quote. An empty array or a blank string means none. A row is not a local session: it has no pid. `stop`, `cancel` and `notify` are argument vectors only (no string form); an empty array or a blank first word means none, and a field of any other shape is ignored without dropping the row. `S` (after a confirmation), `s` and `n` on a row run them where the source ran, with no shell and `MU_SOURCES_CONTRACT` set, with a 60 s limit; the first line of stdout is shown, or on failure the first line of stderr. Without them those keys do nothing on the row.

**Starting an agent.** A line with no `id`, a `remote` (a name, which `/remote` takes) and a `new` argument vector says that agents can be started there: `{"remote":"devbox","new":["prog","new","devbox"]}`. `/remote new NAME [ARGS…]` runs `new` with the words after the name appended (each as one argument, with no shell), in the session's directory, with `MU_SOURCES_CONTRACT` set and a 300 s limit. The first line it prints must be a row for the new agent, as above, with `attach` (and `helper`, `state_key`, `stop`, … when it has them): mu hands the terminal to `mu --attach` with it. Without `attach` mu shows the row's `hint`. On failure the first line of stderr is shown. A source should print the line only for a remote that answers and that it may start agents on; a name two sources both give goes to the later one.

**Trust.** A configured source is trusted as much as the launcher is: `Enter` runs what it prints, as you, and so does `/remote new`.

History: 2 added the argument-vector form of `attach`, and `helper`, `state_key`, `stop`, `cancel` and `notify`. (1 had `attach` as a shell line only.) The `remote`/`new` line was added to 2 later, without a new number: a source line that lacks an `id` was skipped before.

## E. Attach helper (contract 1)

`MU_ATTACH_HELPER` in the environment of the `mu --attach` client: a shell command line, set by whatever started the client (or by a session source's `helper`). mu runs `sh -c '<helper> "$@"' mu-attach-helper VERB ARGS…` and passes the arguments as separate words, so the helper may take arguments of its own (the `$EDITOR` pattern). `MU_ATTACH_HELPER_CONTRACT=1` is in its environment. Paths are on the agent's machine.

| Verb | stdin | stdout | | stdout limit | time limit |
|---|---|---|---|---|---|
| `forward PORT` | - | one JSON line `{"local":N}` | Make `127.0.0.1:N` on this machine reach `localhost:PORT` on the agent's machine, then return and leave it open | 4 KiB | 30 s |
| `unforward PORT LOCAL` | - | - | Remove that forward | 4 KiB | 30 s (5 s in all when mu quits) |
| `read PATH` | - | the file's bytes | Read a regular file | 512 MiB | 10 min |
| `write PATH` | the bytes | - | Create PATH (and parent directories), mode 0600; fail if it exists | 1 MiB | 10 min |

- Failure is a non-zero exit; the first line of stderr is the error text shown. Exit code **64** means "not supported here": mu then falls back to `read_bytes` and `write_bytes` host calls over the attach stream (32 MiB at most) for files, and reports that this remote cannot forward ports.
- Past its stdout limit or its time limit, mu kills the helper's process group. Children a helper leaves holding its output open are given 0.5 s after it exits.
- `read_bytes` and `write_bytes` are host calls like the others, available with or without a helper: `read_bytes {path, max}` answers `{size, data_b64}`, and `write_bytes {path, data_b64}` creates a file that must not exist. Neither has a list of allowed paths, and it does not need one: a client that can attach is already as trusted as the agent's socket owner.

## Minimal examples

A session source that lists one agent on `devbox`, opened over ssh (`sources = [["sh", "/path/to/source.sh"]]`):

```sh
#!/bin/sh
[ "${MU_SOURCES_CONTRACT:-2}" = 2 ] || exit 1
printf '%s\n' '{"id":"3f2a","host":"devbox","attach":["ssh","devbox","mu","attach","--stdio","3f2a"]}'
```

A helper that supports nothing, so mu uses the attach stream for files and says ports cannot be forwarded:

```sh
#!/bin/sh
exit 64
```
