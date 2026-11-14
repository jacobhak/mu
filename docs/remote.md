# Remote use: `mu agent`, `mu attach`, `mu --attach`

The agent and the terminal UI can run in two processes, on two machines. `mu agent` runs a session with no terminal and keeps running when you close the laptop; `mu --attach` runs the usual UI on your terminal as a client of it. Everything the UI does works the same attached: the transcript, approvals and questions, plan review, `/model`, `/login`, custom providers, MCP servers, the Sessions tab, `$EDITOR` for a plan or a memory file, terminal (`pty`) tool calls. The plain `mu` command is unchanged.

## Start an agent

```sh
mu agent [--model M] [--thinking T] [--permissions P] [--resume SESSION | -c]
```

It prints one line, `{"session":"<id>","socket":"<path>"}`, and serves a client on that Unix socket: `<profile home>/run/<session id>.sock`, mode 0600, beside the session's owner record (kind `agent` in the [jobs panel](background-jobs.md#sessions-tab)). There is no network listener: who may connect is who may reach the socket, and reaching one on another machine is `ssh` or `kubectl exec` (below). With no client, the agent goes on with what it was doing; a tool approval or a question waits for a client. A `-p` run holding the session you `--resume` is stopped after its current step first, as `mu take` does, without asking.

`/new`, `/resume` and `/fork` inside the session move the socket to the new session's id; the client stays connected. `--permissions` takes effect for tool calls as usual, so an unattended agent should not be in `ask` mode unless you will attach to answer.

The agent ends on `/quit` or Ctrl-D in a client, on `SIGTERM`, `SIGHUP` or `SIGINT` (it ends the session's jobs and plugins first; a second signal exits at once), and from the Sessions tab (`S`). It does not end when a client goes away.

## Attach

```sh
mu attach [SESSION]            # stdin and stdout to the agent's socket
mu attach --socket PATH
mu --attach 'COMMAND'          # the UI, as a client of the agent COMMAND reaches
```

`mu attach` copies lines between its stdin and stdout and the socket and knows nothing of them; with no argument it finds the one agent running in the current directory (several: it lists them). `mu --attach 'COMMAND'` runs `COMMAND` under `sh -c` and speaks the protocol over its stdin and stdout; its stderr lines show as notices. On the same machine:

```sh
mu --attach 'mu attach'
```

To another machine or a pod:

```sh
mu --attach 'ssh host mu attach'
mu --attach 'kubectl exec -i pod -- mu attach'
```

Both ends must run the same mu build: the first line each way names a wire number, and a different one is refused with a message. Display settings (`status_line`, `theme`, `notify`, `bell`, `hide_thinking_block`, `smooth_streaming`, the `[status]` levels) and composer history come from the client's own config and profile home; everything about models, keys, MCP servers, sessions and plugins is the agent's.

A client sees what it missed when it attaches: the stored conversation up to the turn in progress, that turn's events (up to 20 000 events or 16 MB; past that the oldest blocks are dropped with a notice), the latest status, jobs, queue and todo, and every question, approval, plan review or terminal request still waiting. Nothing rings until it is caught up.

- **One client at a time.** A second attach takes the session; the first prints why and exits.
- **`/detach`** closes the client and leaves the agent running. `/quit` ends the agent. A plain `mu` answers `/detach` with `not attached`.
- **A lost connection** keeps the UI up, says `connection lost; reconnecting`, and runs the command again after 1 s, then 2, 4 … up to 30 s. When it connects, the screen is cleared and the conversation repainted from the agent's catch-up. Ctrl-C while reconnecting quits.
- **Dialogs opened before the loss** stay on screen after a reconnect; an approval that was asked again appears as a second dialog. Answer either.

## Programs that need your terminal

`/login` through omp, an MCP server's OAuth sign-in and the `ChatGPT` sign-in run on the agent's machine, because that is where the credentials go. The agent runs the program in a pseudo-terminal the size of your window and your terminal shows it; your keystrokes and window resizes go back. For ChatGPT, whose sign-in listens on `127.0.0.1:1455` on the agent, the URL opens in your browser and you paste the address your browser ends up at into the composer. A program is hung up when its client goes away. `$EDITOR` runs on your machine, on a private copy of a plan or memory file, and the changed text is written back to the agent's machine when the editor exits successfully.

## Taking over a `-p` run

`t` in the Sessions tab and `/resume` on a session a `mu -p` still has work as locally: the agent stops the run after its current step, starts a second agent on its session, and your terminal shows a nested client of that agent until you leave it. Quitting the nested client ends the second agent; `/detach` leaves it running (`mu attach` finds it).

## Limits

- Terminal (`pty`) tool calls wait up to a minute for a client to size the terminal, then run without one.
- A session id or `--socket` path has to fit a Unix socket address (104 bytes on macOS).
- One agent per session: a second `mu agent --resume` on a session an agent has fails as any second opener does.
