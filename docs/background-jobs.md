# Background jobs

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session. `wait` blocks until a job finishes. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

A turn that began this way rings the terminal; see [Detached runs](detached-runs.md#terminal-bell).

## Jobs panel

`Alt+J` or `/jobs` opens the jobs panel: this session's subagents and `bash` jobs run with `async`, one row each (state, name, agent, tool count, elapsed time, what a subagent has cost, last activity), with the selected job's details below: state and whether its result has been delivered to the model, working directory, the task prompt or command, and how a command ended. It follows the jobs live. `j`/`k` or the arrow keys move, `g`/`G` jump to the first and last job, `x` cancels the selected job, `X` cancels every running job, `q` or Esc closes. `/jobs kill <id|all>` cancels from the prompt.

`Enter` opens the selected job full height. For a subagent that is its session transcript, drawn like the main one (prompt, replies, tool results) and read from the session file as the subagent writes it, with its cost and token count in the title; a message shows up once the subagent has stored it, so text streaming within a turn appears when the reply is complete. For a `bash` job it is the newest output (up to 16 KiB, escape sequences dropped, secrets redacted), updated a few times a second while the command runs. The view follows new output; `j`/`k`, PgUp/PgDn, Ctrl+D/U and `g` scroll and pause it, `G` or scrolling to the end follows again, `f` toggles, `x` cancels the job, `q` goes back to the list. Only the newest 500 messages of a transcript are kept in memory; the panel reads nothing while closed.

The `subagents` status-line segment (add it to omp's `statusLine` in `config.yml`) shows the job counts as `◌2 ✓1 ✗1` (running, done, failed); zero counts are left out, and with no jobs there is no segment.

