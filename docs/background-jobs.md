# Background jobs

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session, so a turn that has nothing else to do just ends. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

`/review` runs its `review-lead` as a job too (the card says `job review`): it is listed here, Enter opens its transcript and `x` cancels it, but its result goes to the findings dialog, never to the model as a notice.

A `bash` call with `async` shows in the transcript as one line, `● Bash <command>`, since all it does is start the job. Its row above the prompt carries the run time while it runs, and when the job finishes its output is printed as the usual `bash` card: the command, the tail of its output and a footer with `Wall:` and, unless it ended at exit 0, how it ended.

A turn that began this way rings the terminal only when it needs you (a question, an approval, plan review), not when it finishes; see [Detached runs](detached-runs.md#terminal-bell).

## Waiting for a job

In an interactive session a turn does not have to end to wait: an ordinary `bash` call (no `async`, no `pty`) runs as a tracked job and waits for its own result.

- The result comes back in that call, not as a separate notice. Each call runs in its own shell: pass `cwd` and the environment the command needs, group setup that must carry over into the same command, and expect `cmd &` and background descendants to end with the call.
- It stays an ordinary `bash` card while it runs: it is not listed among the background jobs (no row above the prompt, no jobs-panel entry, no count in the status line) until a prompt releases the wait, at which point the command keeps running as a job of its own.
- The model can also call `wait` with job ids (or none, for any job). It returns when one of those jobs finishes; their results still arrive as a notice, once.
- A prompt you submit while the model waits releases the wait: the job keeps running and its result arrives later, and your message steers the turn at the next tool boundary. A second tool call in the same reply is skipped so your message is answered first.
- Esc still cancels the turn, including a waiting command. Nothing else cancels a wait, and releasing a wait never cancels the work.

Waiting does not apply to `pty` commands, to `async` calls (which keep returning a job id at once), to commands you run yourself with `!`, or to `-p` runs and subagents. Those places keep a shell that persists across calls, and a model `bash` call there blocks the turn until the command ends.

## Cancelling a job

You do not need the jobs panel to stop a job. While jobs run, their rows sit above the prompt:

- `Alt+↓` marks the first running job (`▸`, highlighted, with the keys listed under it) and moves down the list; past the last job the mark is off. `Alt+↑` moves up. (`Shift+↑` edits the last queued message.)
- While a job is marked, a bare `k` or `x` cancels it and Esc takes the mark off; any other key takes the mark off and then does what it normally does, so typing is never swallowed. (Ctrl and Alt chords such as `Ctrl+O` leave the mark alone.)
- `Alt+K` cancels the marked job, or the only running job when none is marked. With several running and none marked it says so instead of guessing. No key cancels everything at once; use the jobs panel (`X`) or `/jobs kill all` for that.
- `Ctrl+O` expands the rows with each job's task, run time and output tail.

The model has the same power through the `job_cancel` tool, offered in interactive sessions: `id` cancels one job, `all` every running job. The job's result reaches the model as the usual notice, so the model never needs `kill` to stop something it started with `bash` `async` or `task`.

## Jobs panel

`Alt+J` or `/jobs` opens the jobs panel: this session's subagents and `bash` jobs run with `async`, one row each (state, name, agent, tool count, elapsed time, what a subagent has cost, last activity), with the selected job's details below: state and whether its result has been delivered to the model, working directory, the task prompt or command, and how a command ended. It follows the jobs live. `j`/`k` or the arrow keys move, `g`/`G` jump to the first and last job, `x` cancels the selected job, `X` cancels every running job, `q` or Esc closes. `/jobs kill <id|all>` cancels from the prompt.

`Enter` opens the selected job full height. For a subagent that is its session transcript, drawn like the main one (prompt, replies, tool results) and read from the session file as the subagent writes it, with its cost and token count in the title; a message shows up once the subagent has stored it, so text streaming within a turn appears when the reply is complete. For a `bash` job it is the newest output (up to 16 KiB, escape sequences dropped, secrets redacted), updated a few times a second while the command runs. The view follows new output; `j`/`k`, PgUp/PgDn, Ctrl+D/U and `g` scroll and pause it, `G` or scrolling to the end follows again, `f` toggles, `x` cancels the job, `q` goes back to the list. Only the newest 500 messages of a transcript are kept in memory; the panel reads nothing while closed.

### Sessions tab

`Tab` switches to the Sessions tab (`/jobs` and `Alt+J` open on it when there are no jobs): every other mu process that has a session open, read from the active profile's owner records in `<profile home>/run` (see [Detached runs](detached-runs.md)) and refreshed every two seconds. One row each: kind (`tui`, `print` for `mu -p`, `subagent`, `acp`, with the agent's name), pid, what the session has cost, how long it has run, its directory and its first prompt; a subagent is listed under the session that spawned it. This session is left out (its jobs are the other tab), and so is anything whose owner is gone. `mu review` and `mu deliver` do not appear themselves: they hold no session, but the `review-lead` subagent and the implementer they run do.

`Enter` reads a session's transcript, as for a job, but never writes. `n` types a one-line message that goes out through `mu notify` (the footer says `delivered` for a live interactive mu and `queued` for a run that is never signalled, which sees it when its session is opened again). `s` stops a session: a `-p` run after its current step (`SIGUSR1`; press it again to cancel the step), a subagent by terminating its process group (`SIGTERM`), and an interactive mu by cancelling its current turn, as Esc does there (`SIGUSR1`; the mu keeps running; only an interactive mu new enough to say so in its owner record can be aborted). `S` terminates an interactive mu after a `y` confirmation (`SIGTERM`: it exits and restores its terminal, and whatever is typed but unsent in it is lost). `t` takes a `-p` run over: the panel hands the terminal to `mu take`, which stops the run and opens its session in a nested mu; when that exits you are back in the panel. `s`, `S` and `t` first check that the session is still held by the run that was listed (same pid and start time) and otherwise do nothing. An interactive session cannot be taken over, since that would end somebody's open terminal; terminate it first if you must.

The `subagents` status-line segment (add it to omp's `statusLine` in `config.yml`) shows the job counts as `◌2 ✓1 ✗1` (running, done, failed); zero counts are left out, and with no jobs there is no segment.

The `cost` segment shows the session's own spend, then its subagents' as `$0.38 (+0.12)`, as omp does. The subagent figure sums the assistant replies in the session files of every subagent the session has started, grandchildren included, and updates while they run. A resumed session finds its earlier subagents by the `parentSession` in their headers, so the figure survives a restart. The unit (`$`, or `S` for subscription spend) belongs to the session's own amount; subagents on a different billing are not told apart. Replies the price table could not price are stored as zero, so the figure can undercount for a model it does not know.

