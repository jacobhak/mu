# Background jobs

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session. `wait` blocks until a job finishes. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

A turn that began this way rings the terminal; see [Detached runs](detached-runs.md#terminal-bell).

## Jobs panel

`Alt+J` or `/jobs` opens the jobs panel: this session's subagents and `bash` jobs run with `async`, one row each (state, name, agent, tool count, elapsed time, last activity), with the selected job's details below: state and whether its result has been delivered to the model, working directory, the task prompt or command, and how a command ended. It follows the jobs live. `j`/`k` or the arrow keys move, `g`/`G` jump to the first and last job, `x` cancels the selected job, `X` cancels every running job, `q` or Esc closes. `/jobs kill <id|all>` cancels from the prompt.

