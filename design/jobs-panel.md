# Jobs panel — design

Status: proposal. Nothing here is implemented.

## Problem

Today `/jobs` prints one line per background job (`◌ name (agent) · N tools · state`) and `/jobs kill`
cancels. Running subagents show as spinner rows above the composer. There is no way to see what a
subagent is doing beyond its last tool name, to read its output before it finishes, or to see any
session that is not this one (`-p` runs, other terminals, detached `mu deliver`/`mu review`). Those are
reachable only through `mu take` / `mu notify` on the command line, and you have to know the id.

## Goals

1. One place, inside the TUI, for every unit of background work related to you: this session's jobs
   and every other live mu session.
2. Drill into a job and read what it is doing: its transcript, live.
3. Act from there: kill a job, stop or message a detached run, copy a result.
4. An always-visible cue (status line) that something is running or failed.
5. Keep the resident-memory principle: no cost while closed, bounded cost while open.

Non-goals: editing another session from the panel; replacing `/session` (the picker of past sessions);
a web or non-TTY UI; cross-host sessions (owner records are same-host, same-pid-namespace only).

## Entry points

| Entry | Behaviour |
|---|---|
| `Alt+J` | Opens the panel. Same Alt convention as `Alt+M` (model picker); where Option does not act as Meta, `/jobs` is the route, as `/model` is for `Alt+M`. `Ctrl+J` is newline, so no Ctrl binding. |
| `/jobs` | Opens the panel. `/jobs kill <id\|all>` stays as the scripted form. |
| `subagents` status segment | `◌2 ✓1 ✗1`; zero parts hidden, whole segment hidden when no jobs; `✗` in `err`. omp already has the id, mu currently skips it. |

## Layout

A full-height overlay like the reply picker: titled rule, body, closing rule, two-row footer. Tabs in the
title. The body is a list on top and a detail pane below; the split gives the list `min(rows, 8)` rows.

```
── Jobs · Sessions ───────────────────────────────────────────────
 ◌ scout-1   scout   4 tools  00:12  $0.03  reading src/tui/theme.rs
 ✓ review    review  9 tools  00:41  $0.11
 ◌ build     bash             02:03         cargo test --workspace
── scout-1 · /home/u/proj · session 018f… ────────────────────────
 ▸ Find where subagent rows are drawn
 ● grep subagent_rows
   src/tui/blocks.rs:664
 ● read src/tui/blocks.rs 650-700
 ⠙ …
── follow ────────────────────────────────────────────────────────
 j/k move  Enter full view  x kill  f follow  y copy result  Tab tab  q close
```

Colours follow `design/theme/README.md`: state mark `accent`/`ok`/`err`, id `text`, the rest `muted`,
rules `line`.

## Jobs tab (this session)

Rows: state mark, id, agent, elapsed (running: live, finished: total), tool count, cost, last activity.
Finished jobs stay until `/clear`-style `JobsHandle::clear`, delivered or not (delivery state shown as
`·` when the model has not seen the result yet).

Detail pane, by job kind:

| Kind | Header | Body |
|---|---|---|
| Subagent (`task`) | agent, cwd, first lines of the task prompt, state, elapsed, child session path | The child's session transcript, rendered with the normal transcript blocks (tool cards, markdown, thinking) |
| Async bash | command, cwd, state, exit note | Live output tail (bounded ring, 64 KiB) |

Keys: `j k` / arrows move the list; `Enter` opens the detail full height (`q`/`Esc` back); in the detail
`j k PgUp PgDn g G` scroll; `f` toggles follow (default on while running, off once you scroll up);
`x` kills the highlighted job (confirm-free: it is a cancel, not a delete); `X` kills all running jobs;
`y` copies the result text; `Tab` switches tab; `q`/`Esc` closes.

## Sessions tab (other mu processes)

Source: `$MU_HOME/run/*.json` owner records, kept only when `owner()` says `Live` (the flock is held,
so a stale record is never listed and pid reuse cannot fool it). This session's own record is hidden.
Rows: kind (plus agent name for subagents), pid, cwd, title or first prompt, cost, age since start,
idle time (session file mtime).

Children appear nested under their parent via the session header's `parentSession`, so this session's
subagents show here as a tree too (grandchildren included, up to the depth limit). A child that is also
in the Jobs tab is shown once, in Jobs, and marked in Sessions.

A row is a session, never a process: `mu review` and `mu deliver` are orchestrators that hold no session
of their own. What they run does: `mu review` spawns a `review-lead` headless child (`review::review_steps`
via `run_child`), and `mu deliver` resumes the implementer with `mu --resume <session> -p`. Those are
ordinary sessions with owner records, so they already list with no review- or deliver-specific record:
the review shows as `subagent review-lead` (no parent: the orchestrator has no session), the
implementer as `print`. A separate record for the orchestrator would have no session to peek and no key
to file it under, which is why it is not added.

The owner record gains optional fields, written once at claim; readers treat absence as unknown, so old
and new mu coexist and the session file format is unchanged:

| Field | Values | Why |
|---|---|---|
| `kind` | `tui`, `print`, `subagent`, `acp` | What a row is, and which actions are safe (only `print` takes SIGUSR1, only `tui` with `inbox` takes SIGUSR2) |
| `agent` | custom agent name, e.g. `review-lead` | Label for `subagent`/`print` rows; the headless child takes it from the task payload |
| `cwd` | path | Listing without opening every session file |
| `parent` | session path | Tree without reading headers |

Detail pane: read-only transcript of that session, same tailing viewer as Jobs.

### Cost

Assistant messages in the session file already carry `usage.cost`, so a row's cost is the sum over the
file. Reading every live file whole every 2 s would be wasteful; each listed session keeps a running
`(offset, cost, tokens)` and a refresh reads only the bytes appended since (the same whole-lines
reader as the viewer). A session seen for the first time is scanned once off the UI thread and shows `…`
until done. Cost is that session's own: a parent's row does not include its subagents'. Usage the price
table could not price is stored as zero, so a row can undercount for a model the table does not know.

### Actions

Actions, all reusing the existing code paths so signalling rules stay in one place (`session::owner`,
`notify`, `take`):

| Key | Action | Allowed when |
|---|---|---|
| `Enter` | Peek (read-only tail) | any live row |
| `n` | Message: one-line prompt, then the `mu notify` path. The result (`delivered (pid N)` or `queued (<reason>)`) is shown as a notice; `queued` is expected for `print`/`subagent` rows, which are never signalled | any |
| `s` | Stop. `print`: `SIGUSR1`, "stops after the current step", same as the first step of `mu take`. `subagent`: `SIGTERM` to its process group, which is what `run_child` itself sends on cancel; its orchestrator then reports the run as cancelled or failed | `print`, `subagent` |
| `t` | Take over: suspend this TUI and run `mu take <id>` on the terminal, then restore (below) | `print` |

### Take over from the panel

`App::start_child` already hands the terminal to a blocking child while the agent, plugins and timers keep
running (`$EDITOR`, `omp login`). `t` uses it: close the panel, `start_child(ChildThen::Take, …)` runs
`mu take <id>` with the terminal inherited (it refuses without a TTY, which holds here), with SIGINT
caught rather than ignored as in `run_login`, so Ctrl-C reaches the child's `mu take` and not this mu.
Notes:

- This session is not idle meanwhile: a running turn or job continues and its events are buffered; drawing
  and input resume on `ChildDone`, exactly as after an editor.
- `mu take` already verifies the owner is the same live run before signalling and refuses a session in
  an interactive mu; the panel only offers `t` on `print` rows and re-checks nothing itself.
- On return the panel reopens on the Sessions tab with a fresh list. The taken session, now owned by
  the nested mu, is gone from the list until that mu exits.
- A nested mu is a full interactive mu (its own plugins, terminal probe, bell). Editors and `omp login`
  do not exercise that, so it needs its own smoke test: terminal probing and the bell under tmux.

## Child session transcript

Child sessions are already persisted with `parentSession` (`crates/mu-agent/src/agent/mod.rs`, `session::create`). Two gaps:

1. The parent learns the child's session path only from the final `result` line. The child emits
   `{"type":"session","path":…}` as soon as its session exists (the `-p` path already does this with
   `session: <path>` on stderr; `headless::session_line` is the model). `run_child` stores it on the job
   and the job snapshot carries it.
2. `Agent::replay_history` turns messages into `AgentEvent`s inline. Extract it as a pure
   `events_for(&[Message]) -> Vec<AgentEvent>` so the viewer and resume share one conversion.

The viewer owns a private `Transcript` (not the main one). Opening feeds it the loaded messages; follow
mode then tails:

- Append-only JSONL, so keep a byte offset and read only whole lines (up to the last `\n`; a partial
  tail line waits for the next poll). No lock is taken: a read needs none, and `LOCK_EX` would fight
  the owner.
- Poll on the existing frame tick (about 250 ms) with `stat`; no inotify/kqueue, so Linux and macOS
  behave alike.
- Granularity is one message: text streamed inside a turn appears when the message is stored. The
  running row's `last` (from the `tool` lines) stays the live signal in the list.
- Rows kept are bounded (last 2000); older ones drop from the top. Close frees everything.
- Branches and compaction entries: show the current leaf's branch as `session::load` does; a
  compaction entry renders as a notice.
- Secrets: text is redacted before it is stored, so the viewer shows nothing new.

## Data model

`SubagentStatus` (events.rs) grows in place into the panel's job record (the type keeps its name to avoid
churn; the sketch below calls it `JobInfo`); the event stays `AgentEvent::Jobs`:

```rust
pub struct JobInfo {
    pub name: String,            // id, as today
    pub kind: JobKind,           // Subagent | Bash
    pub agent: String,
    pub cwd: PathBuf,
    pub task: String,            // truncated prompt or command
    pub tools: u32,
    pub last: String,
    pub state: SubagentState,
    pub started: SystemTime,
    pub ended: Option<SystemTime>,
    pub session: Option<PathBuf>, // subagent child session
    pub delivered: bool,
    pub note: Option<String>,
}
```

Bash output is not in the snapshot: the panel asks for it with a new `Command::JobOutput(id)` answered by
an `AgentEvent::JobOutput { id, tail }`, so snapshots stay small. Elapsed is computed in the TUI from
`started`/`ended`.

Cost is not in the snapshot either: the TUI derives it from the child session file with the running-total
reader described under Cost, keyed by `session`.

## Resident-memory fit

Closed panel: nothing but the existing snapshot (a few short strings per job). Open: one `Transcript`
of at most 2000 rows and one file offset per viewed session; the Sessions list is read from `run/`
on open and on a 2 s tick while visible. The bash tail ring is per running job, 64 KiB, released on
delivery. Each listed session adds a running `(offset, cost, tokens)`, tens of bytes, dropped on close.

## Phasing

Each phase ships alone.

1. **Model + list** (implemented). The extra `SubagentStatus` fields, `crates/mu-tui/src/panels/jobs_panel.rs`, `/jobs` and
   `Alt+J` open the panel, `x`/`X`. Detail is facts only (state, delivery, cwd, task or command, note).
   Deviation: `take_undelivered` now emits a snapshot so the panel's delivery state is current.
2. **Status segment** `subagents` (implemented). Drawn only where the user's omp `statusLine` lists it; mu's
   fallback segment list is unchanged.
3. **Child transcript and cost** (implemented). The headless child prints `{"type":"session","path":…}` once
   its session exists; `run_child_observed` hands it to the job. `agent::history_events` is the resume
   replay as a pure function; `panels/session_view.rs` draws its events with the live transcript's blocks;
   `session_tail.rs` follows the file (byte offset, whole lines, at most 1 MiB per poll so a big file fills
   in over several ticks on the UI thread instead of needing a worker). `Enter` opens the view. Cost is the
   running total of each listed subagent's session, polled every 2 s while the panel is open.
   Deviation for bash: output reaches the panel as `AgentEvent::JobOutput` (the newest 16 KiB, redacted,
   throttled to 4 per second with a deferred final update), not through a `Command::JobOutput` request; the
   app keeps the latest per job and drops it when the job leaves the snapshot.
4. **Sessions tab** (implemented). The headless child, `-p`, the TUI and ACP write `kind` (from
   `owner::set_run_info`), `agent`, `cwd` and, for a spawned session, `parent`. `owner::live_sessions`
   reads every record whose lock is held (as `owner()` does) and whose session file exists; a record without
   `parent` takes it from the session header. `panels/sessions_tab.rs` builds the tree (roots oldest first,
   this process and its own session left out, this session's subagents marked with their job). Rows show
   cost from the same running-total reader as jobs. The list is re-read every 2 s while the tab is open; the
   panel asks the app for a redraw tick while it is open, since nothing else would repaint an idle screen.
   `Alt+J` and `/jobs` open on this tab when there are no jobs.
5. **Actions on other sessions** (implemented). `n` types a one-line message in the footer and sends it
   with `mu_session::session::inbox::send`; `s` is `mu_session::session::control::stop_session`, which re-checks the owner (pid and start time) before
   `SIGUSR1` (a `-p` run) or `SIGTERM` to the process group (a subagent, only if it leads its own group);
   `t` closes the panel and runs `mu take <session>` through `App::start_child` (SIGINT caught as in
   `run_login`), then reopens the Sessions tab. What an action did shows in the footer until the next key.
   Deviations: the stop on a `-p` run is the first step of `mu take`, so pressing `s` twice cancels the
   current step, as the design said for take.
   Interactive sessions (decided after the first cut left them out): an interactive mu installs a
   `SIGUSR1` handler that cancels the running turn like Esc and says so with `abort: true` in its owner
   record (`session::set_abort_signal`), so `s` aborts a turn without ending the session, and only for a mu
   that promised it. `S` (after `y`) sends `SIGTERM`, which the TUI already turns into a clean exit with
   the terminal restored. Both re-check pid and start time against `live_sessions` first. Take over stays
   unavailable: it would end somebody's open terminal. (The inbox + `SIGUSR2` route was not used for abort:
   the agent reads it only between steps, so it could not interrupt a running tool.)

## Tests

- Tail reader: partial last line, truncation/rotation, empty file, compaction entry, a writer racing the
  reader (loop test as in `session.rs`).
- Owner record: old record without the new fields still lists; stale record is dropped; the headless
  child writes `kind = subagent` and `agent` from its payload.
- Cost reader: incremental sums equal a whole-file sum; a partial last line is not counted twice; an
  unpriced message adds zero.
- `events_for`: the existing resume tests keep passing unchanged through it.
- Panel: key handling and render snapshots at several heights (as `reply.rs` does); tab switch, follow
  toggle, kill sends `KillJobs`.
- `scripts/smoke.sh`: a tmux scenario that starts two scouts, opens `Alt+J`, sees the child transcript.
- macOS: no `/proc`, no inotify; liveness is the flock, signalling is `libc::kill` already used.

## Docs to update when built

`docs/background-jobs.md` (panel, keys, status segment), `docs/detached-runs.md` (Sessions tab, owner
fields), `HOTKEYS` in `crates/mu-tui/src/app/mod.rs`, the Subagents and Sessions rows in `DESIGN.md`.

## Decisions

1. **Key.** `Alt+J` only, plus `/jobs`.
2. **Take over.** Suspend the TUI, run `mu take`, restore (see "Take over from the panel").
3. **`mu review` and `mu deliver`.** Visible through the sessions they run, with no record type of their
   own (see the Sessions tab). This keeps one rule, "a row is a session", instead of a record that has
   no session to peek or file it under.
4. **Cost.** Shown on list rows, from running totals read incrementally off the session files.
