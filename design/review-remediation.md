# Review remediation: bugs, safety, lifecycle and structure

## Context

On 2026-10-26, six read-only reviewers went through all of mu: the core, the TUI, the tools, the providers and sessions, ACP and the plugins, and the scripts. They found about 130 issues. This plan covers every one of them. Each item has a stable ID (`P<phase>.<n>`), the files involved, the fix and how to verify it. Items marked ✔ were confirmed by re-reading the code. Items marked *verify first* were not, so reproduce them before fixing, and drop any that don't reproduce, with a note.

The phases run in order. Within a phase, items are independent unless noted. Use one commit per item, or per small group of items that touch the same function. Run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` before each commit.

Work in a fresh worktree (`git worktree add ../mu-review -b review-remediation`). Do not use the main checkout.

### Out of scope

- **Session branching and checkpoint/rewind.** DESIGN.md lists these under Sessions. They are not part of this plan. P8.5 only updates DESIGN.md to mark them as not yet implemented.
- **The Paseo `/tmp/paseo-mu` working-directory problem.** That is Paseo configuration, not mu code.

### Decisions made up front

- **D1. Unparseable or orphaned messages on load.** Load the session anyway and repair it: drop orphaned `tool_result`s and add an error `tool_result` for each orphaned `tool_use`. Emit a warning that names the entry ids. The same repair runs before every request, as a backstop (P1.4).
- **D2. Bash in plan mode.** Match omp. Check what omp does in plan mode. If omp allows bash, say so in the plan-mode prompt and the tool description. If omp blocks it, block it in the policy check (P2.1).
- **D3. Esc inside the pty view.** Forward Esc to the child. Killing the command becomes Ctrl+C twice within one second; the first Ctrl+C reaches the child as SIGINT through the pty. Update the footer text (P6.10).
- **D4. Images.** Implement DESIGN's spill. Store the image bytes under the session's artifacts directory, keep only the path, media type and hash in the message, and read the bytes back when the request is built (P8.1).
- **D5. Plugin calls listed in DESIGN but never implemented** (`ui/*`, `core/tool`). Mark them in DESIGN.md as not yet implemented, and delete the unused `rpc::Client`. Add both back when a plugin needs them (P7.9, P8.3).

---

## Phase 0 — Tooling and a safety net

- **P0.1 ✔ Clippy fails the build.** `tools/bash.rs:194`: `while !eof` trips the deny-by-default lint `while_immutable_condition`. Rewrite it as `if !eof { loop { … } }`. Then fix the remaining warnings: 31 collapsible `if`s, two manual `is_multiple_of`, two "items after a test module", one large `Err` variant (`tools/mod.rs:190`), one complex type (`tui/clipboard.rs:83`), one needless `return`, one `repeat().take()`, one `match` that can be `?`, two needless range loops, one `openpty` `&mut`, and the warnings in `mu-gateway`, `mu-predict` and `mu-common`.
- **P0.2 ✔ CI runs only on release.** Add `.github/workflows/ci.yml`, triggered on push and pull request. It runs `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and `cargo test --workspace --locked`.
- **P0.3 Tool-pairing invariant helper.** Add `message::check_tool_pairs(&[Message]) -> Result<(), String>`: every `tool_use` id has exactly one later `tool_result`, and every `tool_result` has a `tool_use`. Use it in agent-loop tests from P1 onwards, and in `debug_assert!` before each request.
- **P0.4 Dev scripts kill other processes.** `scripts/smoke.sh:14` runs `pkill -f "mu-gateway daemon"` and `scripts/measure-idle.sh:14` runs `pkill -f "^$bin"`. Kill by the pid captured at launch instead. Give the smoke test its own `MU_HOME` so it never touches the real daemon.

## Phase 1 — Crashes and sessions that cannot continue

- **P1.1 ✔ `read path:0` exits mu.** `tools/read.rs:17-33`. In `select`, reject `n == 0` in every form (`N`, `N-M`, `N+K`), and reject `-0`, so the existing "Invalid selector" error is returned. Test: each form with 0, plus a selector past the end of the file.
- **P1.2 ✔ A tool panic kills the agent task.** `agent.rs` `run_tool`. Wrap `self.tools.call(...)` in `AssertUnwindSafe(fut).catch_unwind()` (`futures_util::FutureExt`) and turn a panic into `ToolOutput::err("tool panicked: …")`. Test: a test-only tool that panics; the turn continues.
- **P1.3 ✔ A truncated reply with tool calls wedges the session.** `agent.rs:962-983`. When `stop != ToolUse` and `calls` is not empty, push an error `ToolResult` for each call: "Tool call not executed: output was truncated (max_tokens)". Test: a provider stub that returns `Length` with a partial tool call; `check_tool_pairs` passes, and the next request succeeds.
- **P1.4 Repair broken tool pairs (D1).** `session.rs:234-236` drops messages that fail to parse, which leaves one side of a tool pair. Add `message::repair_tool_pairs(&mut Vec<Message>) -> Vec<String>`. Run it in `session::parse`, and report what it repaired as a warning event. Also run it in `anthropic::to_wire`, so existing broken sessions recover. Test: a fixture session with an unparseable `tool_result` loads and continues.
- **P1.5 ✔ A plugin manifest panics at startup on a multibyte `lifetime`.** `plugin/manifest.rs:20` uses `split_at` on a byte index. Use `d.char_indices().next_back()` to split off the unit. Test: `idle:5é` returns `None`.
- **P1.6 A crashed agent reports success.** `main.rs:311` ignores the `JoinError`. `headless.rs` `print_mode` and `subagent_mode` treat a closed channel as success. Fix: return exit code 1 on a `JoinError`, and if the channel closes without `Exit`, return code 1 with `isError: true`. Test: a subagent whose agent task panics gives `isError: true`.
- **P1.7 A stale `exec_plan` runs on a later prompt.** `agent.rs:826-836, 1002-1005`. Clear `exec_plan` on `Flow::Aborted` and `Flow::Failed`, and in `reset_conversation`. Test: approve a plan, cancel in the same batch, send a new prompt; no "Execute this plan" turn starts.

## Phase 2 — Tool policy and security

- **P2.1 ✔ Tool limits are enforced in one place.** Add `ToolPolicy { allow: Option<Set>, excluded: Set, depth, plan }` and check it in `Agent::run_tool` before dispatch, including plugin tools. `tool_defs` filters with the same policy, so the advertised list and the enforced list cannot drift. This covers:
  - `allow`/`excluded` are ignored by `Tools::call` (`tools/mod.rs:145`, `main.rs:141,254`);
  - the subagent depth limit is checked only in `agent.rs:236` (enforce `task::depth() < 2` in the policy too);
  - bash in plan mode (D2).

  Test: a scout policy rejects `bash`; depth 2 rejects `task`.
- **P2.2 `ask` in `-p` mode.** `main.rs:211-215` and `headless.rs:49`. Exclude `ask` from print mode through the P2.1 policy. Test: the print-mode tool definitions contain no `ask`.
- **P2.3 ✔ Strip control characters in everything rendered.** Add `line::sanitize(&str) -> Cow<str>`. It replaces C0/C1 controls (keeping `\n`), including `\x1b` and `\x9b`, with a visible glyph such as `␛`, and expands tabs to the next 4-column stop. Call it in `line::span`, so every path is covered: bash and other tool output, error text, the markdown `Text`/`Html` path, and `blocks.rs:46,299,309`. Remove the separate tab replacements (`blocks.rs:47,235,284`, `markdown.rs:488`). This also fixes tabs counting as zero width (`line.rs:52,80`). Test: OSC 52, a CSI cursor move and tabs render as plain cells with the correct width.
- **P2.4 ✔ The live bash tail shows raw bytes.** `tools/bash.rs:131` sends the raw chunk as `ToolOutput`. Send the cleaned chunk (`cleaner.push` output) for the non-pty tail. The pty view keeps the raw feed, because `PtyView` emulates a terminal. `app.rs:555-566` then needs no special case.
- **P2.5 Paste into the pty view.** `tui/ptyview.rs:72-83`. Strip ESC and `ESC[201~` before wrapping the text in bracketed-paste markers, as `composer::normalize_paste` does.
- **P2.6 Predictable editor temp file.** `tui/app.rs:1332-1347` writes `/tmp/mu-edit-<pid>.md`. Create the file with `O_CREAT|O_EXCL` and mode 0600 in `$XDG_RUNTIME_DIR` or `std::env::temp_dir()` under a random name (add the `tempfile` crate if it isn't already a dependency).
- **P2.7 `mu update` installs unverified code.**
  - `update.rs:194` should fetch `install.sh` from the release tag it installs, not from `main`.
  - `release.yml` should publish `SHA256SUMS` for every tarball.
  - `install.sh:38-48` should check the tarball against `SHA256SUMS`, the same way `scripts/fetch-omp.sh` does, and stop on a mismatch.
- **P2.8 The web plugin can be pointed at internal addresses (SSRF).** `plugins/web/src/main.rs:96-110`. Resolve the host and refuse loopback, link-local, private (RFC 1918, ULA) and unspecified addresses unless `MU_WEB_ALLOW_PRIVATE=1`. Check every redirect target too, if `omp read` exposes redirects; otherwise document the limit.
- **P2.9 The gateway URL is not validated.** `gateway.rs:17-28` and `quota.rs:69-74`. Accept `https://`, or `http://` only to loopback. Refuse anything else, with a clear error, before the bearer token is sent. This uses the shared resolver from P4.12.
- **P2.10 Session ids are not sanitized.** Move `acp.rs`'s `valid_id` into `session.rs`. Allow only `[A-Za-z0-9-]`, which rejects `.`, `..` and the empty string. Apply it in:
  - `session::id_of` callers that build paths (`tools/uri.rs:44-50`, artifact dir);
  - `session::find`.

  ACP resolves ids only against the session directory and never as a path relative to the working directory (`session.rs:289`).
- **P2.11 `session::find` is ambiguous.** `session.rs:179,293-302`. An empty argument is an error. A prefix that matches more than one session is an error that lists the matches. `arg.exists()` counts only for regular files ending in `.jsonl`.
- **P2.12 The quota PID is not range-checked.** `quota.rs:77-81`. Reject `pid <= 1` before `kill(pid, 0)`.

## Phase 3 — Session and config durability

- **P3.1 ✔ A torn last line swallows the next entry.** `session.rs:103-107`. In `open_append`, if the file doesn't end in `\n`, append one before any entry. Test: truncate a fixture in the middle of a line, append, reload; the new entry is present.
- **P3.2 ✔ Two writers can corrupt one session.** In `create`/`open_append`, take an exclusive `flock` (`LOCK_EX|LOCK_NB`) on the session file and hold it for the `SessionFile`'s lifetime. If the lock is held, fail with "session is open in another mu process (pid …)". The lock also covers ACP `session/load`. Test: a second `open_append` fails.
- **P3.3 ✔ `set_title` can overwrite the header.** `session.rs:162-171`. Only write in place when line 1 is a title slot exactly `TITLE_SLOT_BYTES` long. Otherwise record only the `title_change` entry, and have `parse` take the latest `title_change` as the title.
- **P3.4 A failed session write disables persistence silently.** `session.rs:127-133`. Replace `eprintln!` with an `AgentEvent::Error` sent through a callback or channel that `SessionFile` holds. Keep `file`, and retry on the next append; give up after three consecutive failures, with a visible error. `append` returns `Option<String>`, which is `None` when nothing was written.
- **P3.5 Entries can reference ids that were never written.** `session.rs:224,137`, `agent.rs:525`, `compaction.rs:189`. With P3.4's `Option` id, `push` keeps `None`, and compaction refuses to cut at an entry without an id (it reports "Skipped"). If `ensure_session` fails (`agent.rs:439`), the turn does not start and an error is shown.
- **P3.6 No `fsync`.** `fsync` after compaction, title and mode-change entries, and at turn end. Do not `fsync` on every append.
- **P3.7 `parse` loads the whole file.** `session.rs:186-188,270`. Parse with a buffered line reader. On a first pass, keep only `(id, parentId, byte offset, type)`. Then walk the chain from the last entry and deserialize only the entries from the latest compaction's kept entry onwards, plus the latest `todo` details. Peak memory is then proportional to the active chain.
- **P3.8 ✔ `set_role` can empty omp's `config.yml`.** `config.rs:264-273`. A read error other than `NotFound` is returned as an error. Write to `config.yml.tmp.<pid>`, then `fsync`, then `rename`. Put this in a shared `mu_common::fs::write_atomic` helper.
- **P3.9 `rewrite_role` assumes one-line values.** `config.rs:217-240`. Parse `modelRoles` with `serde_yaml_ng`. If the role's current value is not a plain scalar on a single line, refuse with "edit config.yml by hand: role X is not a single-line value". Keep the file's line endings (detect `\r\n`).
- **P3.10 One bad field in `config.yml` drops every setting.** `config.rs:248-254`. Deserialize to `serde_yaml_ng::Value` first, then each section separately. A bad section falls back to its default, with a warning naming that section only. Warnings go through the notice channel, not `eprintln!`.
- **P3.11 Threshold values are truncated with `as u32`.** `config.rs:19,27`. Use `u32::try_from`, clamped to a documented maximum, with a warning when out of range.
- **P3.12 Caches can be overwritten with empty data.** `provider/mod.rs:146,298-327` and `pricing.rs:145-147,229-255`.
  - Never save an empty model list.
  - Write through `mu_common::fs::write_atomic`, which uses a per-process temp name.
  - Move the two copies of the load/save code into one `JsonCache<T>` helper.
- **P3.13 `history.jsonl` grows without limit.** `tui/composer.rs:305-318,855-861`. Read only the last `HISTORY_MAX` lines by seeking back from the end of the file. When the file passes 4×`HISTORY_MAX` lines, rewrite it atomically with the last `HISTORY_MAX` lines.
- **P3.14 The predict plugin loads the whole prompt history.** `plugins/predict/src/corpus.rs:33,61-62`. Stream both histories with a buffered reader and keep the most recent N prompts per cwd (N=2000).

## Phase 4 — Providers, network and timeouts

- **P4.1 No timeouts.** `provider/mod.rs:102`, `compaction.rs:214`.
  - Build one shared `reqwest::Client` with `connect_timeout(10s)`.
  - Wrap each `bytes.next()` in a 90 s idle timeout that maps to a retryable error.
  - Give title generation the session's cancel token, plus a 60 s total timeout.
- **P4.2 `Retry-After` and jitter.** `provider/retry.rs:16-21`.
  - Honour `Retry-After` up to 120 s.
  - Above that, fail at once with "rate limited; retry after N min".
  - Add ±20% jitter to the backoff.
- **P4.3 The thinking budget can exceed `max_tokens`.** `provider/anthropic.rs:14-15,25`. Clamp the budget to `max_tokens - 1024`. If that is under 1024, disable thinking for the request.
- **P4.4 `complete()` ignores roles set during the session.** `provider/mod.rs:248`. Filter with `cfg.role(r).is_some()`.
- **P4.5 *Verify first:* an image-only prompt sends an empty text block.** `message.rs:133-134`, `anthropic.rs:52`. Skip empty text blocks when building user messages, and filter them in `to_wire` as a backstop.
- **P4.6 *Verify first:* `redacted_thinking` blocks are dropped.** `provider/anthropic.rs:146`. Add `Block::RedactedThinking { data }`, store it and replay it in `to_wire`. Check against the Anthropic docs.
- **P4.7 SSE framing.** `provider/sse.rs:11-43`. Normalize `\r\n` and bare `\r` to `\n` before looking for frame boundaries. Cap a pending frame at 16 MB and treat anything larger as a fatal protocol error. Test: CRLF, CR-only and mixed streams.
- **P4.8 `gateway_models()` runs on every retry attempt.** `provider/mod.rs:183-184`. Resolve it once per `stream()` call, outside the retry loop, and include it in the cancel `select!`.
- **P4.9 One bad limit hides all quota windows.** `quota.rs:36`. Use `continue` instead of `?` inside the loop.
- **P4.10 Compaction input is not capped.** `compaction.rs:74-102`. Apply the 2000-character cap (head and tail, with an elision marker) to tool-call arguments and user text as well. Cap the whole summary input at 60% of the summarizer model's context window by dropping the oldest messages first.
- **P4.11 Update check.**
  - `update.rs:74-78`: pass `--connect-timeout 5 --max-time 20` to curl.
  - Write the cache on failure too, with a one-hour backoff.
  - `update.rs:88-98,121,188`: an unauthenticated 404 is an error ("repo is private; authenticate gh"), not "no releases", and the exit code is not 0.
- **P4.12 Gateway resolver duplicated.** `gateway.rs:16-31` vs `quota.rs:58-85`. Make one `gateway::endpoint()` (URL, token, dir) and have quota use it.
- **P4.13 Gateway daemon startup.** `plugins/gateway/src/daemon.rs:20-49,78-83`. Kill the spawned child when the startup wait times out. Run `omp auth-gateway token` with a 15 s timeout that kills the process.
- **P4.14 *Verify first:* the ACP `set_config_option` for `model` may never resolve.** `acp.rs:374-381`. This happens if `providers.resolve()` returns an id other than the requested `value`. Match on the resolved id, or answer the request directly once `SetModel` is acknowledged.

## Phase 5 — Agent loop, processes and plugin lifecycle

### Agent loop

- **P5.1 Commands wait until the turn ends.** `agent.rs:663-745`. Split commands into turn-safe ones (`KillJobs`, `Rename`, status queries), handled from a `select!` inside `turn()`, and the rest, which stay queued. `/jobs kill` while `wait` is running then takes effect at once.
- **P5.2 Cancelling compaction shows a false error.** `agent.rs:816-818,903-905`. Add the `!cancel.is_cancelled()` guard used at line 845.
- **P5.3 Follow-ups survive a failed or aborted turn.** `agent.rs:832,851,861`. After `Failed` or `Aborted`, move the queued follow-ups back into the composer as editable text, with a notice, rather than sending them after an unrelated prompt.
- **P5.4 `turn_end` sees stale messages.** `agent.rs:826-871,783`. Drain `turn_msgs` on every exit path through a guard at the end of `turn()`. `user_bash` fires its own `turn_end` hook, or clears its messages after pushing.
- **P5.5 A failed resume has already ended the current session.** `agent.rs:566-575,608-612,712-715`. Load and `open_append` the target first. Call `end_session()` only once both succeed.
- **P5.6 A resumed plan session can't restore the model.** `agent.rs:657`. Store `prevModel` in the plan `mode_change` entry, and restore it in `load_session`.
- **P5.7 `subscription: true` is hard-coded.** `agent.rs:312`. Derive it from the resolved provider source (gateway or API key).
- **P5.8 Subagent exit code.** `headless.rs:82-106`. Return 1 when `isError` is true.
- **P5.9 *Verify first:* the notice printer task is never joined.** `main.rs:276-291,311`. Keep the `JoinHandle`, drop the sender after `shutdown_all()`, then await the handle.
- **P5.10 A dot-named cwd loses its context.** `context.rs:21,46`. Skip dot-directories only among ancestors and children, never the cwd itself.
- **P5.11 Front-matter edge cases.** `context.rs:87-88`.
  - Match the closing `---` as a whole line.
  - Accept an empty front-matter block.
  - Report YAML errors as a notice naming the file.

### Processes and tools

- **P5.12 Cancelling a subagent can hang.** `tools/task.rs:183-193`.
  - Move the TERM → 200 ms → KILL escalation from `bash_pty` into a shared `proc::terminate_group(pgid)`, and use it here.
  - Bound the final `wait` with a 5 s timeout.
  - On a stdout read error, kill the group and keep draining rather than breaking.
- **P5.13 The bash spill file has no size cap.** `tools/bash.rs:34-61`. Use a `BufWriter`, and stop writing at 128 MB with a final `[spill truncated at 128 MB]` line. Report spill I/O errors once, in the output.
- **P5.14 Pty input can deadlock.** `tools/bash.rs:155`. Queue input bytes in a `VecDeque`, and write them from a separate `select!` arm gated on write readiness, so reads, cancel and timeout keep running.
- **P5.15 Background processes outlive the pty command.** `tools/bash.rs:186-200`. After the leader is reaped and drained, call `terminate_group(pgid)` (P5.12). Document in the bash tool description that background processes are killed when the command ends, and that `pty: false` with `&`/`nohup` is the way to keep one alive. Check the plain path's behaviour in pi-shell and document it.
- **P5.16 Delivered job output stays in memory.** `jobs.rs:112-133`. Drop `text` once it has been delivered, and keep only the snapshot fields.
- **P5.17 A finished job can be reported as cancelled.** `jobs.rs:111`. Report "cancelled" only when the child produced no result.
- **P5.18 A dropped plan-review reply reads as "changes requested".** `tools/plan.rs:36`. Return `ToolOutput::err("Plan review was closed without a decision")` instead.
- **P5.19 Bash timeout logic is duplicated.** `tools/bash.rs:85-90,106-107,212-213`. Parse once into `struct Limits { secs, timeout: Option<Duration> }` and use it on both paths.
- **P5.20 The pty cleaner loses text on `\r\r\n`.** `pty.rs:209-216`. In `State::Cr`, a second `\r` stays in `Cr` without clearing the line. Test: `"abc\r\r\n"` gives `abc`.

### Plugins (`src/plugin`)

- **P5.21 ✔ A cancelled call leaks the `inflight` counter.** `plugin/mod.rs:84-104`. Use an RAII `InflightGuard` that decrements the counter, removes the `pending` entry and touches `last` on drop. Test: cancel a slow call; the plugin shuts down once idle.
- **P5.22 A read error leaves the plugin's reader stuck.** `plugin/mod.rs:127-153`. Read lines as bytes (`read_until`), skip invalid UTF-8 with a warning, and treat an I/O error as EOF. On EOF, fail all pending requests, clear the slot, close stdin and kill the process group, and only then `wait()`.
- **P5.23 Calls fail during idle shutdown.** `plugin/mod.rs:246-249,301-316`. Clear `slot.proc` before calling `shutdown()`, so `ensure()` starts a fresh process.
- **P5.24 Hooks run one after another.** `plugin/mod.rs:354-372`. Run them concurrently with `join_all` under one 30 s deadline. After a timeout, skip that plugin's hooks for 5 minutes and show one notice.

### ACP (`src/acp.rs`)

- **P5.25 A request without `method` gets no reply.** `acp.rs:146`. Answer requests that have an `id` with `INVALID_REQUEST`.
- **P5.26 Invalid UTF-8 on stdin ends ACP.** `acp.rs:53-54`. Use `read_until(b'\n')` with `serde_json::from_slice`. On a parse error, send a `PARSE_ERROR` reply (id `null`) and keep going.
- **P5.27 `open()` blocks the loop.** `acp.rs:121-123,169,203,323`. Run `open()` as a `spawn_local` task that replies through the main loop's channel. Fetch the model list in the background, with the cached list as the default.
- **P5.28 A pending `set_config_option` is never answered.** `acp.rs:236,239-253,358-363`. Fail `pending_cfg` on `session/close` and on the exit path.
- **P5.29 `session/list` reads every session file.** `acp.rs:177,188`, `session.rs:311-363`. Sort file names, which start with a timestamp, in descending order, then read only the first 500 matches, on `spawn_blocking`.

### Memory and predict plugins, and the installer

- **P5.30 Memory plugin splits surrogate pairs.** `plugins/memory/main.ts:171`, `content.ts:52-54,87`. Truncate on code-point boundaries with a shared `truncateCodePoints` helper.
- **P5.31 Timed-out memory calls leak `pending` entries.** `plugins/memory/rpc.ts:43-49`, `main.ts:78-81`. Give `call()` a timeout option that removes the entry.
- **P5.32 Predict drops valid candidates.** `plugins/predict/src/engine.rs:50-58`. Filter with an exact-case `starts_with` before taking the maximum.
- **P5.33 Plugin install is not atomic.** `scripts/install.sh:88-89`. Copy to `<dir>.new`, then `rm -rf <dir>.old`, `mv <dir> <dir>.old` and `mv <dir>.new <dir>`.

## Phase 6 — TUI correctness

### Terminal state

- **P6.1 SIGTERM, SIGHUP and SIGQUIT don't restore the terminal.** `tui/term.rs:37-41`, `app.rs:208-216`. Add `tokio::signal::unix` handlers that set a quit flag. The loop exits normally, so `Term::drop` restores the terminal and the "resume with" hint is printed.
- **P6.2 The panic hook runs on every panic.** `tui/term.rs:90-96`. Restore the terminal only for panics on the main thread or the TUI task, and exit the process after restoring. With P1.2, panics in background tasks are logged as notices instead. The hook first moves the cursor below the viewport (it shares the renderer's last row through an atomic), so the panic text doesn't land on top of the composer.

### Input

- **P6.3 An unfinished OSC freezes input.** `tui/input.rs:207-217,242-244`. Enter OSC buffering only after `ESC ] 5522;` (the clipboard reply mu asks for). Treat any other `ESC ]` as Alt+]. Cap OSC buffering at 64 KB, and flush the bytes as keys if the cap is reached.
- **P6.4 Pastes are rescanned from the start.** `tui/input.rs:180-219`. Keep a resume offset for the `PASTE_END` and OSC searches. If a paste overflows `MAX_PENDING`, drop the rest of the paste until the end marker, and show "paste truncated".
- **P6.5 Each read allocates 64 KB.** `tui/input.rs:101-102,160-170`. Keep the buffer in `Reader`, and inline the one-method `PushResize` trait.
- **P6.6 `paste_inner` can attach part of a paste.** `tui/composer.rs:611-617`. Load all the images first, then attach them all, or attach none and show the error.
- **P6.7 The slash menu rebuilds usage counts on every keystroke.** `tui/composer.rs:480-497`. Build the usage map once, when history changes.
- **P6.8 Space can't be typed in the multi-select filter.** `tui/select.rs:119-126`. When the filter is not empty, Space types a space. Toggling uses Tab, and Space while the filter is empty.

### Drawing

- **P6.9 A draw error loses lines and spins the loop.** `tui/app.rs:294-299,1449-1495`. Put `pending` back on error, set `last_draw` so the next try waits a frame, and show the error once.
- **P6.10 Esc always kills the pty command (D3).** `tui/ptyview.rs:59-66`, `app.rs:1471`.
- **P6.11 Lines committed just before exit are never drawn.** `tui/app.rs:309-319`. Call `app.draw()` once after the loop, before `renderer.finish()`.
- **P6.12 The cursor can land on the status bar.** `tui/render.rs:70-76`. Adjust `skip` so the composer's cursor row stays visible. If it can't, hide the cursor.
- **P6.13 Width is counted per `char`, not per grapheme.** `tui/line.rs:36-94`, `composer.rs:651`, `render.rs:150-166`. Add `unicode-segmentation`. Wrapping, truncation, cursor movement and backspace then work on grapheme clusters. This builds on P7.2, the single wrap function, so do it after P7.2 or together with it.
- **P6.14 `trunc` counts chars, not columns.** `app.rs:194-196,1420-1441`, `blocks.rs:37,391`, `tools/mod.rs:202`. Delete all three copies and use `line::truncate`.
- **P6.15 `/resume` blocks the event loop.** `tui/app.rs:769-771`. List sessions on `spawn_blocking`, reusing P5.29's capped, sorted scan. Show the picker with a loading row until the list arrives.
- **P6.16 Login and editor take over the terminal.** `tui/app.rs:1290-1330`. Taking over the terminal is intended. Run the child's `wait` on `spawn_blocking` so agent and plugin tasks keep running, and suspend input reading while the child owns the terminal.
- **P6.17 Clipboard copy blocks the UI.** `tui/clipboard.rs:18-63`. Run the copy on `spawn_blocking` with a 2 s timeout, and report the result through `UiMsg`.

### Markdown and streaming

- **P6.18 Streaming commits split lists and references.** `tui/markdown.rs:108-110`, `app.rs:400-403`. Commit only up to the end offset of the last complete top-level block, using pulldown-cmark's offset iterator. Never commit while a list or blockquote is still open at the end. Keep reference definitions and re-render them with later text. Test: a nested list across commits, and a reference definition committed before its use.
- **P6.19 The open block is re-parsed every frame.** `app.rs:398-403,1374-1375`. Keep a scan offset for `stable_prefix`. Cache the rendered lines of the open block, keyed by `(width, text_len)`. Re-highlight an open fence at most once per frame, and only when its text has changed.
- **P6.20 Partial JSON is re-parsed on every delta.** `app.rs:1451-1457`, `tui/partial_json.rs`. Throttle parsing to frame ticks. Keep the parser state, so each pass only scans the new bytes. At the same time, merge the two truncation steps (`partial_json.rs:143-167`) into one "end of the last complete member" offset tracked in `Frame`.
- **P6.21 Dead markdown branches.** `markdown.rs:38,292-295`. Enable task lists, which render as ☐/☑. Remove the footnote arm.
- **P6.22 `links()` cuts URLs at parentheses.** `markdown.rs:82-87`. Balance parentheses, and reject an empty host.

### Blocks, status and theme

- **P6.23 Diff line counts.** `blocks.rs:266-268`. Exclude only the file headers (lines that come before the first `@@` hunk), so content lines starting with `++` or `--` are counted.
- **P6.24 `format_number` gives `1000K`.** `status.rs:327-330`. Choose the unit after rounding. Test: 999_600 shows `1M`, and 999_600_000 shows `1B`.
- **P6.25 The theme can be pinned silently.** `theme.rs:240-262`. If `THEME.set` fails in `init`, warn, and add a `debug_assert!` that `get()` is never called before `init`.
- **P6.26 A wrong palette name delays startup.** `theme.rs:205-207,253-259`. When loading a palette fails, fall back to the bundled default and show a notice. Probe the terminal only for `theme = "terminal"`.
- **P6.27 The plan-review cursor duplicates its label.** `plan_review.rs:77,251,285-289`. Keep the label in one constant, and compute the cursor row and column from the rendered row index.
- **P6.28 The pty cursor row depends on box layout.** `blocks.rs:370-377`. `bash_box` returns the first row of the output section, and with zero rows the cursor is hidden.

## Phase 7 — Structural refactors

Do these after phases 1–6, so the fixes land in small diffs first. Each refactor keeps behaviour the same and has existing or new tests.

- **P7.1 Structured tool results.** `ToolOutput` gets `meta: Option<ToolMeta>`. For bash, `ToolMeta` holds the exit code, wall time, timed-out and cancelled flags, and spill artifact. For read, edit and write it holds the path and tag. The model still gets the same text. The TUI renders from `meta` and stops parsing text. This covers:
  - bash trailer lines read out of program output (`blocks.rs:197-225`);
  - stringly-typed footer facts, which become an enum with label and role (`blocks.rs:174-184,256`);
  - `summary()` re-parsing `[path#tag]`, and matching the plan header on `local://…-plan.md` (`blocks.rs:72-82,333`).
- **P7.2 One wrap function.** `line::layout(spans, width) -> Vec<Row>` with per-cell source offsets. The composer, the renderer and cursor placement all use it, and `composer.rs:640-741` and `render.rs:150-166 locate` are removed (`line.rs:45-68`). P6.13 builds on this.
- **P7.3 Split `App`.** `app.rs` (about 45 fields).
  - Move commit, reveal and thinking state into a `Transcript` struct.
  - Move the six overlays and their key handling into an `Overlays` struct.
  - `on_agent` and `overlay_key` dispatch to these.
  - Add an `Overlay::select_mut()` helper, which removes the repeated `Model | Role | Login | Session | Ask | Copy` pattern (`app.rs:801,1472`).
  - Add a `thinking_rows()` helper, which removes the three copies of the thinking layout (`app.rs:363-384,417-434,1377-1385`).
- **P7.4 Split `Agent`.** `agent.rs:24-85`. Move `tick_quota` and the `quota_*` fields into a `QuotaTracker`, and `settle_prices` and the cost fields into a `CostLedger`, each with its own tests. Also:
  - One `estimate_context_tokens()` replaces the duplicated formula (`agent.rs:141,192`).
  - Move `text_of` into `message.rs` (`agent.rs:580-582`, `compaction.rs:70-72`).
  - Estimate tokens by summing block lengths without serializing, and cache the estimate until the messages change. Cache the system prompt per turn instead of rebuilding it for each snapshot, which removes the `/proc` read (`message.rs:145-150`, `compaction.rs:47-50`, `agent.rs:140-142,290-300`).
- **P7.5 One table of built-in tools.** `tools/mod.rs:49,93-123,145-185`. Add a `static BUILTINS: &[Builtin { name, def: fn() -> ToolDef, call }]`. `BUILTIN_NAMES`, `definitions()` and dispatch all come from it.
- **P7.6 Blocking filesystem work in tools.**
  - `grep`, `glob`, `read`, `write` and `edit` run their filesystem work on `spawn_blocking` (`tools/grep.rs:24`, `glob.rs:15`, `read.rs:36`, `write.rs:13`, `edit.rs:23`).
  - `read` streams lines for selected ranges and for the 300-line default, and stops after the window. The edit-store hash still needs the whole file, so read it once, as bytes, without the extra copies (`read.rs:65-78`, `write.rs:33`).
  - `image::load` checks `metadata().len()` before reading (`image.rs:27-38`).
  - The 200-match grep limit is checked inside the per-match callback (`grep.rs:52-84`).
- **P7.7 Types instead of strings.** Session entry kinds become an enum serialized to the same strings. `set_title` takes a `TitleSource` enum. `api` uses the `models::API` constant type (`session.rs`, `message.rs`).
- **P7.8 Layering.**
  - Move `UserInput` from `events.rs` into `message.rs` (`message.rs:4,133`).
  - The input parser returns reply bytes, and the app writes them to the terminal. Probe and input share one tty handle (`input.rs:71-96`, `probe.rs:16`).
- **P7.9 One JSON-RPC implementation (D5).** Put the framing and pending-map code in `mu-common` (`rpc::{Framed, Pending}`). `plugin/mod.rs:71-155`, `acp.rs:31-69` and the plugin servers use it. Delete the unused `rpc::Client`.
- **P7.10 mu-common helpers.**
  - One profile-name guard and one `home()` (`mu-common/src/paths.rs:52-53`, `plugins/gateway/src/shared.rs:23-24`, `plugins/predict/src/main.rs:15-17`).
  - One clipboard `Display` enum, detected once (`clipboard.rs:45-93`).
- **P7.11 Table-driven lists.**
  - One language table with aliases, feature, query and injections, and a corrected "omp's 11 categories" comment, since `Cat` has 17 variants (`highlight.rs:2,31-90,171-225`).
  - One status segment table with id and render function, and drop the `path_display` bool that is never used (`status.rs:14-18,201-261,373-380`).
- **P7.12 `select.rs` rebuilds rows on every key and frame.** `select.rs:97,197,223-238`. Cache the filtered rows and the lowercased haystacks, and rebuild them only when the filter or the items change.
- **P7.13 Dead code and misleading comments.**
  - Remove `StopReason::Error`/`Aborted` and `error_message` unless P1.3/P4 produce them (`message.rs:33-34`).
  - Remove the unused `context.rs:140` `path` field and the `config.rs:57` `allow(dead_code)` field.
  - Fix the `events.rs:77` comment about `context_tokens`.

## Phase 8 — Alignment with DESIGN.md

- **P8.1 Images are kept in memory (D4).** `image.rs:32`, `message.rs`, `anthropic::to_wire`. Add `Block::Image { source: ImageRef { path, media_type, sha256 } }`. Keep the old inline base64 form for deserializing existing sessions, and write new sessions in omp's format, *after verifying* what omp writes. Load the bytes in `to_wire`. Test: a session with an image resumes, and resident memory doesn't grow with each image (check with `scripts/measure-idle.sh`).
- **P8.2 The gateway plugin's lifetime.** `plugins/gateway/plugin.toml:3`. Change it to `idle:10m`, and confirm that re-acquiring after an idle shutdown works (the lease and the daemon are unaffected).
- **P8.3 `ui/*` and `core/tool` (D5).** `plugin/mod.rs:186`. Update the DESIGN.md protocol section to list them as not yet implemented.
- **P8.4 Plugin schemas are parsed at startup.** `plugin/manifest.rs:94-96`. Keep each schema's file path, and parse it the first time the tool definitions are built for a request. DESIGN's "nothing parsed at startup" then holds.
- **P8.5 Branching and rewind are out of scope.** DESIGN.md → Sessions table: mark "branching, checkpoint/rewind" as *not implemented — out of scope for now*. No code changes.

---

## Verification

- **Per item:** the test named in the item, plus `cargo clippy … -D warnings` and `cargo test --workspace --locked`.
- **At the end of each phase:**
  - `scripts/smoke.sh`, run in an isolated `MU_HOME` after P0.4.
  - `scripts/measure-idle.sh`: idle resident memory must not regress against the number in DESIGN.md. Expect a drop after P3.7, P3.13 and P8.1.
- **Manual TUI checklist after phases 2 and 6:**
  - `printf '\e]52;c;aGk=\a'` in bash leaves the clipboard unchanged.
  - `kill <mu-pid>` leaves a usable shell.
  - Alt+] does not freeze input.
  - A paste of more than 10 MB stays responsive.
  - A nested list streams with its indentation intact.
  - vim in a pty bash receives Esc.
  - Ctrl+C twice kills the command.
- **ACP after phase 5:** run `mu --acp` under Paseo:
  - `session/new` with an invalid cwd gets an error reply;
  - `session/list` returns quickly with more than 500 sessions;
  - a malformed line doesn't end the server.
- **Session files:** a session written after phase 3 still loads in omp (`omp --resume <file>`) to confirm the format is unchanged.
