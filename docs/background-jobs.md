# Background jobs

`task` runs subagents in the background: it returns job ids at once and the results arrive later as a `<system-notice>` message that wakes an idle session. `wait` blocks until a job finishes; `/jobs` lists jobs and `/jobs kill <id|all>` cancels them. With `-p` and for subagents themselves, `task` blocks until its subagents finish.

A turn that began this way rings the terminal; see [Detached runs](detached-runs.md#terminal-bell).
