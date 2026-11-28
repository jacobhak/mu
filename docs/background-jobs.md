# Background jobs

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session (or inline from `wait`), so a turn that has nothing else to do just ends. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

A result longer than 5000 characters comes back as its head plus a pointer: the whole text is written to an `artifact://` file the notice names, which `read` opens. A subagent also runs under a soft request budget ([`[task]`](config.md)): at the budget it is told to wrap up, at 1.5x it is stopped and asked for what it has, and five requests later the run ends.

`/review` runs its `review-lead` as a job too (the card says `job review`): it is listed here, Enter opens its transcript and `x` cancels it, but its result goes to the findings dialog, never to the model as a notice.

A `bash` call with `async` shows in the transcript as one line, `● Bash <command>`, since all it does is start the job. Its row above the prompt carries the run time while it runs, and when the job finishes its output is printed as the usual `bash` card: the command, the tail of its output and a footer with `Wall:` and, unless it ended at exit 0, how it ended.

A turn that began this way rings the terminal only when it needs you (a question, an approval, plan review), not when it finishes; see [Detached runs](detached-runs.md#terminal-bell).

## Output notices

`bash` also takes `notify`, a regex (Rust syntax, the same as `grep`) that only counts with `async`. While the job runs, each output line that matches reaches the model as a `<system-notice>` with the matching lines, so a model that started a dev server or a watcher hears when `ready` or an error line appears instead of polling. The notice wakes an idle session, arrives at the next tool boundary during a turn and ends a blocked `wait` (which then says output matched, and the job keeps running). Lines that match within 200 ms of each other arrive as one notice (at most 50 lines of 500 characters), and matches that have not been sent yet when the job ends are dropped, since its result follows anyway. The lines are secret-redacted like the jobs panel. An invalid regex is refused before the job starts. The transcript shows `↳ bg_1 output matched: <first line>`.

## `pty` commands waiting for input

A `bash` call with `pty` runs in a terminal you can type into. A `sudo` or `ssh` password prompt there looks like a hung command, so mu checks twice a second, once the output has been quiet for half a second, whether the command waits for the keyboard:

- terminal echo is off (password prompts, full-screen programs), on Linux and macOS;
- a process in the terminal's foreground group is blocked reading it: on Linux through `/proc/<pid>/syscall`, on macOS through the `ttyin` wait channel `ps` reports;
- failing both, text with the cursor behind it (a prompt without a newline) and no output for 2 s.

When it does, the call's card says `waiting for input` and the terminal rings once (the `attention` bell in [`[bell]`](config.md); a detached run rings as it does for a question). Typing or any output clears the mark, and nothing rings while you have typed in the last 10 s.

## Waiting for a job

In an interactive session a turn does not have to end to wait: an ordinary `bash` call (no `async`, no `pty`) runs as a tracked job and waits for its own result.

- The result comes back in that call, not as a separate notice. Each call runs in its own shell: pass `cwd` and the environment the command needs, group setup that must carry over into the same command, and expect `cmd &` and background descendants to end with the call.
- It stays an ordinary `bash` card while it runs: it is not listed among the background jobs (no row above the prompt, no jobs-panel entry, no count in the status line) until a prompt releases the wait, at which point the command keeps running as a job of its own.
- The model can also call `wait` with job ids (or none, for any job). It returns when one of those jobs finishes, with the finished jobs' results in the call itself, so no notice follows for them. Options: `all` waits until every named job (every job when none is named) has finished; `timeout` (1-3600 s) gives up after that long and still returns what finished, naming what still runs; `until` is a regex (Rust syntax) that returns as soon as a named `bash` job prints a matching line, counting output printed before the call, with the job still running (a bash job that ends without a match returns its result instead).
- A prompt you submit while the model waits releases the wait: the job keeps running and its result arrives later, and your message steers the turn at the next tool boundary. A second tool call in the same reply is skipped so your message is answered first.
- Esc still cancels the turn, including a waiting command. Nothing else cancels a wait, and releasing a wait never cancels the work.

Waiting does not apply to `pty` commands, to `async` calls (which keep returning a job id at once), to commands you run yourself with `!`, or to `-p` runs and subagents. Those places keep a shell that persists across calls, and a model `bash` call there blocks the turn until the command ends.

## Looking at jobs (`job_status`)

`job_status` shows the model what its jobs are doing without a turn of polling. With no arguments it lists every job of the session, oldest first, one line each: `id · agent (or bash) · queued|running|done|failed · elapsed · N tools · $cost · last activity`, plus the listening ports of a bash job and `result waiting` for a finished job whose result has not been delivered. With `id` it shows one job in detail: its directory, task or command, what it waits for, model, session file, cost, messages sent to it, then the last 10 tool calls of a subagent, or the last `tail` lines of a bash job's output (default 40, at most 400; secrets redacted). A subagent's cost is read from its session file, so a job with no session yet shows none.

## Messaging a subagent (`job_send`)

`job_send` sends a message to a running subagent. The subagent reads it at its next tool boundary, as it would a user steering a turn. With `urgent` it also cancels the tool call it is running (the result says `[interrupted: a new message arrived…]`) and skips the rest of that reply's calls, so it reads the message now; the turn itself goes on. A subagent that has not started yet (queued) gets the message added to its task instead. A finished subagent cannot be messaged: the error points at `resume`. A message that reaches a subagent after its last turn was not read; the job's result says so (`N message(s) sent with job_send arrived after the subagent had finished`).

## Continuing a subagent (`resume`)

`task` with `resume: "<id>"` continues the conversation of a finished subagent of this session, with `task` as its next prompt. It runs in the same directory and as the same agent (leave `agent` and `cwd` out), keeps the model of its session unless `model` names one, and runs under the current permissions, not those of its first run. The continuation is a new job named after the old one (`<id>-2`), so the original's result and row stay. Only subagents that ran on this machine can be resumed (not those on a remote worker, not `bash` jobs), and not one that another job is already continuing.

## Queue and dependencies

At most 8 jobs run at once. Further subagents wait in a queue of at most 16, start in order as slots free, and show as `· queued` in the jobs panel and `job_status` (the `task` result also says why: `[queued: no free slot]`). A queued job takes no slot; `job_cancel` and `x` in the panel cancel it without starting it, and `job_send` adds to its task. An async `bash` call is not queued: at 8 running jobs it still errors.

`after` on a task lists job ids, or names of other tasks in the same call, that must succeed first. The task starts when all of them are done and its context gets their results (`# Results of the jobs this task waited for`, each capped like a result in a notice); when one fails or is cancelled the task ends without starting, with a result that names it. A task cannot wait for itself, for an unknown job, or in a cycle.

## Per-task limits

`task` takes three limits per task:

- `timeout` (seconds, at most 7200): at the limit the subagent is told to stop and report what it has; if it is still running two minutes later it is cancelled. The result notes it either way.
- `maxRequests`: lowers the soft request budget for this task (wrap-up notice at it, stop at 1.5x). It can only lower what `[task] soft_request_budget` and the agent's bundled cap allow, but sets a budget where none is configured.
- `maxCost` (USD): a wrap-up notice at 80 %, tools refused at the limit and the result asked for. For a [relayed](remote.md) worker it also lowers the spend cap `[task] max_cost` sets, for this task.

## Cancelling a job

You do not need the jobs panel to stop a job. While jobs run, their rows sit above the prompt:

- `Alt+↓` marks the first running job (`▸`, highlighted, with the keys listed under it) and moves down the list; past the last job the mark is off. `Alt+↑` moves up. (`Shift+↑` edits the last queued message.)
- While a job is marked, a bare `k` or `x` cancels it and Esc takes the mark off; any other key takes the mark off and then does what it normally does, so typing is never swallowed. (Ctrl and Alt chords such as `Ctrl+O` leave the mark alone.)
- `Alt+K` cancels the marked job, or the only running job when none is marked. With several running and none marked it says so instead of guessing. No key cancels everything at once; use the jobs panel (`X`, which asks `y` to confirm) or `/jobs kill all` for that.
- `Ctrl+O` expands the rows with each job's task, run time and output tail.

The model has the same power through the `job_cancel` tool, offered in interactive sessions: `id` cancels one job, `all` every running or queued job. The job's result reaches the model as the usual notice, so the model never needs `kill` to stop something it started with `bash` `async` or `task`.

## Jobs panel

`Alt+J` or `/jobs` opens the jobs panel: this session's subagents and `bash` jobs run with `async`, one row each (state, name, agent, tool count, elapsed time, what a subagent has cost, last activity), with the selected job's details below: state and whether its result has been delivered to the model, working directory, the task prompt or command, and how a command ended.

A running `bash` job's row also carries the TCP ports its processes listen on (` :3000`), and its details say `listening localhost:3000`. It follows the jobs live. `j`/`k` or the arrow keys move, `g`/`G` jump to the first and last job, `x` cancels the selected job, `X` cancels every running job after a `y` (any other key keeps them), `q` or Esc closes. `/jobs kill <id|all>` cancels from the prompt.

The ports come from the job's process group and its children, read every two seconds: `/proc` on Linux, `lsof` on macOS. Only a port on a wildcard or loopback address counts, since that is what `localhost` reaches. `o` forwards those ports to this machine (through the attach helper, when you are [attached](remote.md)) and opens the lowest one in the browser; a forwarded port is taken back when the job ends or mu exits.

`Enter` opens the selected job full height. For a subagent that is its session transcript, drawn like the main one (prompt, replies, tool results) and read from the session file as the subagent writes it, with its cost and token count in the title; a message shows up once the subagent has stored it, so text streaming within a turn appears when the reply is complete. For a `bash` job it is the newest output (up to 16 KiB, escape sequences dropped, secrets redacted), updated a few times a second while the command runs. The view follows new output; `j`/`k`, PgUp/PgDn, Ctrl+D/U and `g` scroll and pause it, `G` or scrolling to the end follows again, `f` toggles, `x` cancels the job, `q` goes back to the list. Only the newest 500 messages of a transcript are kept in memory; the panel reads nothing while closed.

### Sessions tab

`Tab` switches to the Sessions tab (`/jobs` and `Alt+J` open on it when there are no jobs): every other mu process that has a session open, read from the active profile's owner records in `<profile home>/run` (see [Detached runs](detached-runs.md)) and refreshed every two seconds. One row each: kind (`tui`, `print` for `mu -p`, `subagent`, `acp`, `agent` for a [`mu agent`](remote.md), with the agent's name), pid, what the session has cost, how long it has run, its directory and its first prompt; a subagent is listed under the session that spawned it. This session is left out (its jobs are the other tab), and so is anything whose owner is gone. `[sessions] sources` (see [Remote contracts](remote-contracts.md#d-session-sources-contract-2)) list agents on other machines too: they get a host column (the source's own name for the machine, `here` for the others) and show who is attached and whether they wait for you. `Enter` on one runs its `attach` command as `mu --attach` and brings you back to the tab when you leave (without one it shows the source's hint), and `s`, `S` (after `y`) and `n` run the source's `cancel`, `stop` and `notify` commands when it gives them, and do nothing otherwise; `t` does nothing on them. `mu review` and `mu deliver` do not appear themselves: they hold no session, but the `review-lead` subagent and the implementer they run do.

`Enter` reads a session's transcript, as for a job, but never writes. `n` types a one-line message that goes out through `mu notify` (the footer says `delivered` for a live interactive mu and `queued` for a run that is never signalled, which sees it when its session is opened again). `s` stops a session: a `-p` run after its current step (`SIGUSR1`; press it again to cancel the step), a subagent by terminating its process group (`SIGTERM`), and an interactive mu by cancelling its current turn, as Esc does there (`SIGUSR1`; the mu keeps running; only an interactive mu new enough to say so in its owner record can be aborted). `S` terminates an interactive mu or a `mu agent` after a `y` confirmation (`SIGTERM`: it exits and restores its terminal, and whatever is typed but unsent in it is lost). `/agents` does the same from the composer. `t` takes a `-p` run over: the panel hands the terminal to `mu take`, which stops the run and opens its session in a nested mu; when that exits you are back in the panel. `s`, `S` and `t` first check that the session is still held by the run that was listed (same pid and start time) and otherwise do nothing. An interactive session cannot be taken over, since that would end somebody's open terminal; terminate it first if you must.

The `subagents` status-line segment shows the job counts as `◌2 ✓1 ✗1` (running, done, failed; a queued job counts as running); zero counts are left out, and with no jobs there is no segment.

A queued job's row has a `·` mark and the state `queued` in the details.

The `cost` segment shows the session's own spend, then its subagents' as `$0.38 (+0.12)`, as omp does. The subagent figure sums the assistant replies in the session files of every subagent the session has started, grandchildren included, and updates while they run. A resumed session finds its earlier subagents by the `parentSession` in their headers, so the figure survives a restart. The unit (`$`, or `S` for subscription spend) belongs to the session's own amount; subagents on a different billing are not told apart. Replies the price table could not price are stored as zero, so the figure can undercount for a model it does not know.

