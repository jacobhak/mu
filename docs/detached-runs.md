# Detached runs

## Interrupting a `-p` run

`mu -p` prints `session: <path>` to stderr as soon as the session exists. Exit codes: 0 done, 1 error, abort or cost cap, 3 interrupted and resumable. `kill -USR1 <pid>` (never a process group) asks a `-p` run to stop: it finishes the running tool, skips the rest of that batch, ends the turn, releases the session and exits 3 with `mu: interrupted; resume with: mu --resume <path>`. A second `SIGUSR1` cancels the current step. Either way the session loads without repair.

Stop details: a stop that arrives before the next tool starts skips every call of that batch, so the run ends at once; one that arrives during a tool lets that tool finish. Any second `SIGUSR1` cancels the current step, including one sent by `mu take` after a manual `kill -USR1` (or by two takes). A fatal error (such as a failed provider request) exits 1 even after a stop; non-fatal errors (a truncated reply, a failed auto-compaction) do not change exit 3.

## Taking over: `--resume` and `mu take`

`mu --resume <session|id>` (and `-c`, and the `/resume` picker) takes over a session a `-p` run still has: it signals the run, waits until the session is free, then opens it. `mu take <session|id>` is the same thing for a terminal that is not yet running mu: it also changes to the session's directory and execs `mu --resume <path>`, and refuses unless stdin and stdout are terminals. Either way, a session open in an interactive mu (or a subagent or ACP run) or held by a process that left no owner record is refused. Ctrl-C while waiting sends the second signal; a second Ctrl-C gives up waiting.

Every open session is recorded in `<profile home>/run/<id>.json` (see [Profile home](usage.md#profile-home)), with `pid`, `print_run`, `inbox`, `started`, `session`, and since the jobs panel `kind`: `tui`, `print`, `subagent` or `acp`; `agent`, the custom agent it runs as; `cwd`; `parent`, the session that spawned it; `abort`, set by an interactive mu that cancels its current turn on `SIGUSR1`. The record is held under `flock` by its owner, so a busy-lock error can name the pid; the files are safe to delete while no mu runs. Records without the newer fields still read (the kind is guessed from `print_run` and `inbox`, the parent from the session header). The [jobs panel](background-jobs.md#sessions-tab) lists the live records. The session file format is unchanged. A taken-over session loses `--max-cost` and a custom `--agent` prompt; `ask` and jobs come back.

The pid in an owner record is only meaningful on the same host and pid namespace: `mu take` from another container or namespace that shares `MU_HOME` and the session directory can signal the wrong process or none, so run it where the `-p` run lives.

## Messaging a session: `mu notify`

`mu notify <session|id|path> [--from NAME] <text|->` leaves a message for a session and, if an interactive mu has it open, wakes it: that mu starts a turn by itself, shows `↳ mu notify from NAME: …` and the model sees the text in a `<system-notice>` user message (`-` reads the text from stdin; at most 64 KiB; `--from` defaults to `another process`).

It prints one line: `delivered (pid N)` or `queued (<reason>)`, where the reason is `not running`, `pid N does not take messages`, `owner unknown`, `owner changed` or `cannot signal pid N: <error>`; exit 0 for delivered or queued, 1 for an error (unknown session, empty or oversized text).

Messages are files in `<profile home>/inbox/<id>/` (directory mode 0700), written atomically before the owner is looked at, and removed once the notice is in the session file. Only an interactive mu installs the `SIGUSR2` handler and says so in its owner record (`inbox: true`); `-p`, subagents and ACP are never signalled (the signal would kill them), so a message for them waits until the session is opened again (`mu take`, `--resume`), which announces it (`N message(s) from mu notify waiting`) and sends it with the next prompt. A message that arrives mid-turn goes out after the current tool batch or reply, in the same turn. Every `mu notify` call deletes queued messages older than 30 days.

## Terminal bell

A turn you started rings the terminal when it ends, unless a [background job](background-jobs.md) or subagent is still running (aborting does not ring). A turn that began without user input (a `mu notify` message or a finished job or subagent) rings only when it needs you (`ask`, plan review), not when it finishes: BEL, plus an OSC 9 notification under tmux (needs `allow-passthrough`) and in Ghostty, WezTerm, iTerm2 and Warp; omp's `completion: {notify: off}` in `config.yml` turns it off, and `[bell] turn_end` / `attention` in `config.toml` turn off each kind (see [Config](config.md)).

## Session environment

Bash commands (and `!cmd`) get `PI_SESSION_FILE` and `PI_SESSION_ID` (empty without a session), as in omp.
