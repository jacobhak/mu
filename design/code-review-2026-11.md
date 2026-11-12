# Code review: assessment and plan (November 2026)

Reviewed at `815bf07`. Scope: every crate, every plugin, `src/`, `tests/`, `scripts/`, the workflows and the docs. Focus: maintainability, patterns and anti-patterns, engineering quality. Bugs and product observations found on the way are included.

Nothing in this PR changes code. It is an assessment (sections 1 to 4) and a plan with options (sections 5 and 6). The recommendations are mine; every choice is yours.

## 1. Summary

mu is about 101,000 lines of Rust in 282 files with about 1,700 test functions: `mu-tui` 29k, `mu-tools` 17k, `mu-provider` 14k, `src/` 8k, `mu-agent` 8k, `mu-base` 7k, the plugins 10k.

The engineering quality is high where it is hardest to get right: crate layering the compiler enforces, process groups and file descriptors, terminal restore, secrets kept out of `Debug` and error text, atomic writes, and pure state machines with tests around the risky logic. Section 3 lists what to keep.

The problems cluster in five places:

1. **Checks that parse their input separately from the code that acts on it.** The permission gate, the edit gate and the web fetch guard each have their own reading of a path, an edit header, a shell line or a URL. Where the two readings differ, the check passes and the action does something else. Five such gaps were reproduced with tests (S1 to S5).
2. **Failures that turn into an empty value.** A damaged session line, an unreadable memory file, a corrupt `auth.json` and a failed compaction each continue as if nothing was there, and the user sees a different symptom later (B1, B2, B3, B10).
3. **State held by convention.** Modes as independent `Option`s and booleans, item data in vectors that run parallel to the items, and rows found by position. The TUI and the tool layer carry most of it (M3, M4, M6).
4. **Three gates that run different checks.** CI, the maintenance bot and the release build do not run the same list, and the lint policy exists only as a command-line flag (C1 to C3).
5. **A dozen large files where most changes land**, each mixing several jobs (M1 to M7).

Suggested order: close the reproduced holes first (workstream A, small), then the silent failures (B), then align the gates (C). The structural work (D to G) pays off over time and can be taken one file at a time.

## 2. Method, and how far to trust each item

The codebase was split into 16 slices, each read by a read-only subagent with the same brief. I then checked their evidence in the code myself. Following your note, I used only the facts they reported (locations and what the code does); severities, options and recommendations here are mine.

Every finding says how it was checked:

- **ran**: reproduced with a throwaway test or command at `815bf07`. The tests are not in this PR; each would make a good regression test for its fix.
- **read**: I read the cited code and the claim follows from it. Not executed.
- **reported**: a subagent reported it with file and line; I did not re-read it. Treat it as a lead.

Not covered: nothing was run on macOS, no timings were measured (every performance item is from reading), `render.rs` was read only in part, and `mu-bench` only in part. Pinned dependency code (`pi-shell`, `pi-edit`) was read at the revision in `Cargo.lock` (`9058ab9`).

Line numbers are as of `815bf07`.

## 3. What is good, and worth keeping as the rule

This list comes from the subagents' reports and from code I read while checking findings. I did not verify each item separately.

- **Layering by crate.** `mu-tui` does not depend on `mu-agent`; they share `Command` and `AgentEvent` from `mu-base`. The crate table in `DESIGN.md:69-78` matches the `Cargo.toml` edges.
- **Pure cores with effects outside.** `AgentView::on_event` returns `Effect` values (`app/agent_view.rs:203-395`); panels return intent enums and `app/services.rs` runs them; the MCP and custom-provider wizards are `Answer -> Step` machines; the subagent budget is a table-tested state machine (`agent/budget.rs`); the quota tracker does no I/O.
- **One table drives several consumers.** The built-in tool table gives names, definitions and dispatch; `ToolPolicy` decides both what is advertised and what runs; `scripts/ci-checks.sh` is the one list CI, `ship.sh` and `AGENTS.md` share; the CLI reference is generated from clap.
- **Provider wire formats behind one trait** with a shared SSE loop (`wire/mod.rs:17-62`), and one session fixture replayed through all three wires.
- **Process and terminal care.** Liveness by `flock`, not pid; pid plus start time checked before a signal; `kill_group` refuses pgid <= 1; zombie-aware `running()`; RAII terminal restore that is panic-aware; `NoEcho` restores termios and the previous signal handlers.
- **Secrets.** `auth.json` is 0600 from creation; hand-written `Debug` without key material, with tests asserting no key appears in errors; output redacted before it is written to artifacts, with a split secret held back across chunks; all text sanitised where spans are built (`text/line.rs:14-20`), which blocks terminal escape injection from model and tool output.
- **Web fetch design.** Each hop is resolved, checked and pinned; redirects are followed by hand; identity encoding only; tests use loopback servers and never the internet. (The one hole, S1, is in the first hop's URL parsing.)
- **Stale-result guards.** Every async UI flow carries an id or sequence number so a late answer is dropped.
- **Test seams behind `cfg(any(test, feature = "test-support"))`**, as `DESIGN.md:80` requires, in most places.
- **Shell scripts.** `set -euo pipefail` throughout, checksum-verified downloads, staged installs, `--no-track` worktrees, a lock and a spend cap on the maintenance bot, and `change=true` as the default when triage output does not parse.

## 4. Findings

Severity is my judgement: **high** is a reproduced way past a stated guarantee, or silent loss of user data; **medium** is a real defect with a narrower trigger, or a structure that slows every change in its area; **low** is the rest.

### 4.1 Security

The default permission mode is `yolo`, where the gate does not ask at all. S2 to S5 therefore matter to exactly the users who chose `ask` or `edits` for what `docs/permissions.md` promises. S1 applies in every mode.

**S1. Web fetch: the guard and the HTTP client parse the URL differently (high, ran).**
`plugins/web/src/net.rs:386-390` (`host_port`) is a hand-written parser; `net.rs:241` then hands the raw string to reqwest, which parses it with the `url` crate. The `url` crate ends the authority at a backslash; `host_port` does not. With `http://127.0.0.1:<port>\@8.8.8.8/` the guard checked `8.8.8.8` and the client connected to `127.0.0.1:<port>`: `load_page` with the default guard returned the loopback server's body, while the plain loopback URL was refused. The same shape reaches `169.254.169.254`. Redirect hops are normalised through `Url` (`net.rs:254-260`), so only the first URL is affected. Text on a fetched page can steer the model to such a URL, and the body comes back to the model.

**S2. `edits` mode: a `..` after a missing directory leaves the project (high, ran).**
`permissions/mod.rs:726-745` (`canonical_nearest`) returns the path unchanged when a missing component is followed by `..`, and `starts_with(root)` then accepts it (`mod.rs:662-672`, `692-697`). In `edits` mode, `write zz_missing/../../outside.txt` and `write <root>/zz_missing/../.git/config` were both allowed without asking (`../x.txt` and `<root>/.git/config` ask, as intended). `tools/write.rs:41-43` creates the missing directory first, so the write then lands outside the project or in `.git`. The same check guards `~/.mu` (`mod.rs:692-697`), so by reading, `~/.mu/config.toml` is reachable the same way, and a `[permissions.rules]` entry written there would persist.

**S3. `>&file` is treated as read-only (high, ran).**
`permissions/bash.rs:437` returns read-only for every `DupOut` redirect. In pi-shell a `>&word` whose word is not a number is `&>word` and writes the file (`vm/redir.rs:727-730`). In `ask` mode `echo x >&out.txt`, `echo x >&~/.bashrc` and `echo x >&/etc/zz` were allowed without asking; `echo x > out.txt` asks.

**S4. A command grant also covers its redirects (high, ran).**
A bash subject is built from the command's words only (`permissions/bash.rs:198-199`), so redirect targets are not part of what a rule matches. After approving `cargo test` for the session, `cargo test -q > ~/.bashrc` and `cargo test -q > /etc/zz` ran without asking in `ask` mode.

**S5. The edit gate reads headers differently from the editor (high, ran).**
`tools/pathnote.rs:19-40` (`edit_paths`) is a second parser for edit payloads. For `[*** Begin Patch] [/etc/zz#AB12]` it returns the path `*** Begin Patch] [/etc/zz`, which is relative and so inside the project, and `edits` mode allowed the call; pi-edit unwraps that envelope and targets `/etc/zz` (`hashline/input.rs:332-351`). In sloppy mode, `***  Edit   File: /abs/x` (extra spaces, or a tab) gives no path at all, while pi-edit's header regex accepts any run of blanks (`sloppy/parse.rs:48`).

**S6. `@path` imports in context files read any file, at startup, with no gate (medium, read).**
`context/at_imports.rs:116-129` resolves `~`, absolute and `..` paths and calls `read_to_string` with no containment, size cap or file-type check, for every context file. The system prompt is not passed through redaction (`agent/mod.rs:375-394`), although `docs/secrets.md:3` says nothing with a recognised secret reaches the provider. A cloned repository's `AGENTS.md` can therefore put `~/.ssh/id_ed25519` into the first request, and an import of a FIFO or `/dev/zero` blocks startup. No permission mode applies here.

**S7. The read-only classifier lets through flags that write, execute or read elsewhere (medium, ran for the verdicts; what each program then does was not run).**
In `ask` mode all of these were allowed without asking: `rg --pre cat foo .`, `tree -o out.txt`, `wc --files0-from=/etc/shadow`, `grep -f/etc/shadow x README.md`, `git grep -O foo`, `sort --compress-program=sh README.md`, `date -f /etc/shadow`. `rg` and `tree` accept any arguments (`bash.rs:29,37`), and every word starting with `-` is skipped as a path (`bash.rs:611-613`).

**S8. `deny` rules match the text as written (medium, ran).**
With `"git push *" = "deny"`: `git push origin` is refused, while `"git" push origin`, `/usr/bin/git push origin`, `env git push origin` and `sh -c 'git push origin'` run. The built-in `rm` guard does unwrap quotes and wrappers (`guard.rs:9-10,106-114`); user rules do not. `docs/permissions.md:43` says a `deny` always wins.

**S9. The built-in `rm` guard covers literal forms only (low, ran).**
Not refused: `cd / && rm -rf *`, `X=/; rm -rf $X`, `echo ~ | xargs rm -rf`, `rm -rf "$(echo ~)"`, `rm -rf /..`, `rm -rf ~/../..`. `docs/permissions.md:45` lists the forms it does cover, so the docs are accurate; the code comment at `guard.rs:77` ("however written") is not. This is the only backstop in `yolo`.

**S10. With `HOME` and `MU_HOME` unset, mu's home is the relative path `.mu` (medium, read).**
`mu-common/src/paths.rs:3-5` returns an empty path for a missing `HOME`, and `mu_home()` joins `.mu` onto it, so `auth.json` and sessions would be written under the current directory (cron, `env -i`, some service units). `env -u HOME -u MU_HOME mu auth list` exits 0 instead of refusing. Other sites read `HOME` themselves with different handling (about a dozen, reported).

**S11. A mistyped `[permissions] mode` falls back to `yolo` (medium, read).**
`mu-base/src/config.rs:179-190` warns and uses the default, which is the least restrictive mode. A typo in a setting meant to restrict gets one startup warning.

**S12. Input that can panic the process through `Duration::from_secs_f64` (medium, ran for two, read for one).**
`provider/retry.rs:26-44` (`hint`) panics on `retry-after-ms=` followed by 40 digits; the text comes from a provider or gateway response body. `plugins/web/src/net.rs:305-306` panics on `Retry-After: 1e30`. `plugins/gateway/src/daemon.rs:157-160` accepts `MU_GATEWAY_IDLE_SECS=inf`. All three want `Duration::try_from_secs_f64`.

**S13. Smaller security items.**

| | What | Where | Checked |
|---|---|---|---|
| a | Session files are created with the umask's mode (0664 here) and `history.jsonl` 0644, while `auth.json` and the inbox are private. Under a profile the 0700 profile directory shields them; `~/.mu` itself is 0755 here, so without a profile they are readable by whoever can enter the home directory. | `mu-session/src/session/file.rs:81,85`; `chat/composer.rs:799` | ran (`stat`) |
| b | The ChatGPT sign-in callback acts on an `error` parameter before it checks `state`, and echoes its text. Any local request to the callback port ends a pending sign-in. | `auth/chatgpt/oauth/callback.rs:56-65` | read |
| c | When the HTTP client builder fails, the fallback is `reqwest::Client::new()`, which drops the no-redirect policy and the connect timeout. | `provider/client.rs:143` | read |
| d | `memory` is in the read-tool list, but its `edit` action writes memory files; in `ask` mode it runs without asking. Tiers are assigned by tool name. | `permissions/mod.rs:50-64` | ran |
| e | A grant for a tool without patterns is stored as `"*"` for the whole tool (any plugin or MCP tool; `fetch` with a URL that has no scheme). | `permissions/mod.rs:465-467,602-604` | ran |
| f | `/mcp reconnect` restarts a server from its old config when its entry is no longer in the trusted set. | `plugin/mcp_runtime.rs:404-418` | read |
| g | Tool-call hooks run concurrently on the original arguments; the last answer's `arguments` wins; a hook that fails or times out lets the call proceed and is skipped for 5 minutes. | `plugin/host.rs:380-423`; `agent/tool_exec.rs:271-279` | read |
| h | A plugin's `core/complete` request cannot be cancelled and has no timeout; each plugin request spawns a task with no cap. | `plugin/process.rs:299-302,343` | read |
| i | Environment secrets are snapshotted at the first redaction; a value exported later inside the shell is unknown to the redactor unless it matches a key shape. | `mu-base/src/secrets.rs:102,118` | read |
| j | `signal_pid` accepts pid 0, which signals the caller's own group, though its doc says it never signals a group. | `mu-base/src/process.rs:11-17` | read |
| k | `100.100.100.200` (a cloud metadata address inside 100.64/10) and `0.0.0.0/8` other than `0.0.0.0` are not treated as internal. | `plugins/web/src/net.rs:415-439` | ran |
| l | The release workflow grants `contents: write` to every job, and the installer checks the tarball against a checksum from the same release. | `.github/workflows/release.yml:7-8`; `scripts/install.sh:55-70` | reported |
| m | The MCP wizard summary masks `env` and headers but prints `args` as typed. | `mcp/mcp_wizard.rs:370-381` | reported |

### 4.2 Bugs and silent failures

**B1. A damaged line in the middle of a session silently cuts its history (high, read).**
`mu-session/src/session/format.rs:239` skips a line that does not parse, without a warning, and `format.rs:175` ends the parent walk when a parent id is not indexed. Everything before the damaged line drops out of the loaded conversation; appends then continue from the leaf, so the loss becomes permanent. Only unparseable `message` entries warn (`format.rs:294-296`).

**B2. Memory: an unreadable entry file is read as empty and then rewritten (medium, read).**
`plugins/memory/src/mem.rs:116` uses `read_to_string(..).unwrap_or_default()` for the user and project files, and `edit` (`mem.rs:168-183`) writes the whole file back from that state. A file that is not valid UTF-8, or not readable, is replaced by the new entries alone. With git present the old bytes are in history; without git they are gone.

**B3. Compaction on small context windows (medium, read).**
Three defects that only show below about 36k tokens, which is local-model territory:
- The default threshold has a cliff: a 19,000-token window gives 16,150, a 19,300-token window gives 2,916 (`agent/compaction.rs:47-49`; the test at `compaction.rs:348` pins 3,616 for 20,000).
- The kept tail is a fixed 20,000 tokens (`compaction.rs:15`), which is above the threshold for those windows, so the context is still over the threshold after compacting.
- A failed compaction is retried on every tool round with no memo or backoff (`agent/turn.rs:146-150,199-204`), one summariser request and one error line each time.

**B4. Bash: output from a descendant keeps a call open (medium, read).**
In the pty path the drain after the command exits has no cancel or timeout arm (`tools/shell/bash.rs:149-158`), and the process group is only terminated after it; a descendant that writes more often than every 300 ms holds the call, and Esc does nothing. In the background path the idle timer is rebuilt on every loop pass (`bash.rs:773`), so a writing descendant keeps the job "running" until the timeout or `job_cancel`.

**B5. `edit` writes in place; `write` is atomic (medium, read).**
`tools/edit.rs:230` truncates and writes, while `tools/write.rs:64` uses `write_atomic`. A crash or a full disk during an edit leaves a truncated source file.

**B6. Retry notices say `/5` whatever `max_attempts` is (low, read).**
Hard-coded in `mu-base/src/events.rs:26` and `mu-tui/src/app/agent_view.rs:474`; `[retry] max_attempts` accepts 1 to 20.

**B7. Editing a paste marker drops the pasted text (medium, read).**
A large paste is inserted as the plain text `[Paste #n, +k lines]` (`chat/composer.rs:578-584`) and expanded on submit by `text.replace(marker, content)` (`composer.rs:704-708`). Backspace one character out of the marker and the prompt goes out with the broken marker and without the content. The sequential replace also expands a later marker's text if it occurs inside an earlier paste.

**B8. A retried request leaves the failed attempt's text in `/copy` and on screen (medium, read).**
`chat/transcript.rs:247-253` (`reset_stream`) clears the live text only. `block_text`, which feeds `/copy`, keeps the first attempt and then appends the retry.

**B9. Dialog sizing (low, read).**
The plan review counts four action rows in its height budget (`panels/plan_review.rs:234`) and draws five (`plan_review.rs:90-96`), so it can be one row taller than the space it was given. Generic pickers size from `rows / 2` and ignore the rows already used (`widgets/overlay.rs:930-931`), unlike the review, reply, jobs and ask views.

**B10. A corrupt or newer `auth.json` reads as "no API key stored" (medium, read).**
`auth/store.rs:231-242` turns a read failure into an empty store; only `check()` reports the real reason, and the resolve path does not call it. Related: when a ChatGPT token refresh succeeds but the save fails, the fresh access token is discarded although the old refresh token is already spent (`auth/chatgpt/store.rs:185-196`).

**B11. Session reopen can leave the agent with a transcript and no session (medium, read).**
`agent/session.rs:155-170` ends the session and clears it before `open_append`; if that fails, the messages of the old session stay loaded and nothing is persisted from then on.

**B12. Memory review queue (medium, read).**
The drain takes the first three queued sessions (`plugins/memory/src/review.rs:326-333`); a failing one stays at the head, so three permanently failing sessions stop every later review. The retry loop (`review.rs:130-145`) treats any error as a rejected reply, so a lock timeout or a failed git commit is sent back to the model as "Your previous reply was rejected".

**B13. Gateway daemon start (medium, read).**
The starter kills the daemon after 30 s (`plugins/gateway/src/daemon.rs:15,59-69`), but the daemon's own steps can legitimately take longer before it writes its state (two omp spawns and a 15 s token step, `daemon.rs:176-197`; the per-spawn wait of 20 s is reported, not read). The omp children run in their own process groups (`daemon.rs:277`), so that kill misses them, and `PR_SET_PDEATHSIG` exists only on Linux (`daemon.rs:278-284`) while the comment says the child dies with the daemon. On macOS the broker and gateway can be left running.

**B14. Smaller bugs.**

| | What | Where | Checked |
|---|---|---|---|
| a | `main` prints `{e}`, the outermost context only, so `mu providers models <id> --refresh` against a bad key prints "refreshing the models of ..." without the cause. | `src/main.rs:16` | read |
| b | `build.rs` reruns only when `upstream.toml` or git's `HEAD`/index change, so an unstaged edit leaves the version without `-dirty`; `mu update` and `install.sh --skip-if-dev` rely on that suffix. | `build.rs:9-19` | read |
| c | `ship.sh` installs from the main checkout even when it could not fast-forward it, and reports success. | `scripts/ship.sh:62-76` | read |
| d | The mcp-http bridge exits on any transport error that is not a 401 or an expired session; the core then fails pending calls with "plugin exited". Whether the next call respawns it was not checked. | `plugins/mcp-http/src/main.rs:282-305` | read |
| e | An unparseable line from a subagent child is dropped, so a malformed result becomes "subagent exited without a result"; the wait for stderr has no bound. | `tools/subagent.rs:199,224` | read |
| f | `models.toml` is edited read-modify-write with no file lock, unlike the provider registry, `auth.json` and the permission store. | `mu-base/src/config.rs:700-717` | read |
| g | The session and project directory name maps `/` to `-` without escaping, so `~/code/a-b` and `~/code/a/b` share a directory and stored grants. The scheme is omp's, so a change has a compatibility cost. | `mu-common/src/paths.rs:104-117` | read |
| h | The live-render cache is keyed by text length, so a new block of the same length shows the previous block's rows for a frame. | `chat/transcript.rs:269` | read |
| i | The fallback screen says "No fallbacks: nothing is tried when @name has no model" and, on the next line, that `@task`'s chain is used. The second is what the code does. | `panels/model_roles.rs:329-335` | read |
| j | A permission grant is reported as saved when the file already has a whole-tool action or an `ask` for the same pattern, and nothing was written. | `permissions/store.rs:81-116` | read |
| k | The pre-flight gateway model fetch has no overall timeout and its result is discarded. | `provider/client.rs:265-270` | read |
| l | The request body is built as a JSON tree, serialised to a second copy, and the tree stays alive while the answer streams; images are base64 inside it. `DESIGN.md:219` says request bodies are streamed and released. | `provider/client.rs:298-336` | read |
| m | Every wire conversion clones the whole message history to check tool pairing. | `mu-base/src/message.rs:468-472` | read |
| n | An agent task that panics leaves an ACP `session/prompt` unanswered; headless checks the task result. | `agent/handle.rs:99`; `src/run/acp/mod.rs:943-955` | reported, spawn site read |
| o | Web fetch: the 30 s budget wraps the whole render, so a slow `llms.txt` probe discards a finished conversion; `raw=true` collapses blank-line runs; GitHub blobs are returned as text whatever their type; HTML conversion runs synchronously on the plugin's single thread for bodies up to 50 MB. | `plugins/web/src/fetch.rs:44-46,88-89,148-171`; `github.rs:96-102` | reported |
| p | Cursor movement in the editor is quadratic in the column on one long line. | `widgets/editor.rs:74-97` | read |
| q | An open code fence is re-parsed and re-highlighted in full on every streamed line. | `chat/transcript.rs:136,154-160` | reported |
| r | Session search opens a write transaction per session file on every search. | `plugins/memory/src/search/index.rs:157-161` | reported |
| s | The JSON schema validator silently ignores keywords it does not implement (`pattern`, `format`, ...). | `mu-base/src/schema.rs:1-6` | read |
| t | `self_exe()` falls back to `/proc/self/exe` only; the OS release in the system prompt is read from `/proc`. No macOS path for either. | `mu-common/src/paths.rs:96`; `agent/prompt.rs:81-84` | read / reported |

### 4.3 Structure and maintainability

**M1. The permission gate (`permissions/mod.rs`, 1,140 lines).** Classification, grant construction, the width ladder, rule matching and prompt text live in one file; `check` takes the whole `Tools` struct to resolve a path; a tool's tier comes from a list of names. Each hole in S2 to S5 sits where the gate re-derives something the executor also derives. (read)

**M2. The bash tool (`tools/shell/bash.rs`, 1,616 lines; `jobs.rs`, 1,199).** Four execution paths (foreground, interruptible, background, pty) plus `!cmd` each implement timing, drain and kill, with different semantics (B4 is one result). `Job` mixes subagent and bash fields, with its kind implied by several flags, and completion is written twice. A foreground `bash` counts against the eight-job limit, so with eight subagents running a plain command is refused (`jobs.rs:320-323`; this happened during the review). (reported; B4 and the limit read)

**M3. TUI widgets.** `Select` (`widgets/select.rs:61-107`) has about 25 fields covering state, keys, layout and previews, with `editing` and `noting` as two booleans and previews, kinds and details in vectors parallel to the items. `Overlay` has over 20 variants, most of them a `Select` plus a parallel payload vector; `select()` and `select_mut()` are two identical 20-arm matches (`overlay.rs:838-886`) and `on_key` repeats one pattern per variant. Dialog rows are addressed by position (`Some(0)`, `Some(3)`), although `custom/panel.rs` already shows the fix: a `Row` enum and a `rows()` function. (read)

**M4. TUI app state.** Five independent `Option` flows and several composer flags decide who owns input; exclusivity comes from the order of checks in `keys.rs` and from each starter remembering to end the others. Slash commands are described in four places (`app/slash.rs:16-49`, the busy list, the dispatch and the argument table), and they already disagree: `/permissions` and `/mcp` are marked as needing an argument but work bare, which costs an extra Enter. About 40 sites assign `self.overlay`; only ask and approval preserve an open dialog, so opening a picker over a pending approval drops its reply, which counts as a denial. (reported; `slash.rs` table and the denial read)

**M5. TUI panels.** `jobs_panel.rs` holds two tabs, a transcript viewer, compose and confirm states and their rendering, and reads session files on the UI thread on a timer. Row, header and footer code is copied between panels with hand-counted heights (B9 came from that). The editor's fallback preview restates the rule in `mu-base/src/roles.rs`. (reported; polling read)

**M6. The agent loop.** `run_loop` (`agent/turn.rs:194-297`) steers by five local flags; reminders are recognised by comparing message text (`turn.rs:185,226`); `messages` and `entry_ids` are parallel vectors with an empty string for "not persisted" (`agent/mod.rs:63-65`, `turn.rs:19`). There is no loop-level test for compaction or overflow (reported). (read)

**M7. Entry points.** `run()` is one 220-line function with mode-specific invariants held by `expect` and `unreachable!` (`src/run/mod.rs:69-287`); `src/cli/auth.rs` (1,152 lines) holds the clap enum, the login flow, the no-echo terminal layer with its signal handlers, rendering and tests; `src/workflow/deliver.rs` mixes the record, the steps, review parsing, the loop and locking (reported). (read for `run()`)

**M8. `mu-base` as a grab bag.** All 20 modules are `pub`. Beyond the message and event types it holds role descriptions and view models, git, pty and process wrappers, model resolution, and a 1,118-line `config.rs`. (read)

**M9. Provider.** `provider/custom.rs` (1,314 lines) mixes schema, switch parsing, registry writes, save workflows and UI labels. HTTP and stream errors are classified in five functions with diverging rules (`retry.rs:117,205`; one per wire). Three on-disk caches (`model/discovery.rs`, `chatgpt_models.rs`, `pricing.rs`) repeat freshness and backoff logic and have drifted: mtime and length in one, inode and ctime in `auth/store.rs`, no failure backoff in pricing. (reported; classifier sites read)

**M10. No rule for error types.** `Result<_, String>` appears in 254 signatures, next to `anyhow` and a few typed enums, sometimes in the same crate (`mu-session`: `file.rs` uses `anyhow`, `control.rs` strings). Conversions flatten the cause in both directions. Three contracts match on message text, one of them across a process boundary: the memory plugin checks `starts_with("no model configured")` (`plugins/memory/src/review.rs:90`) against a string produced in `mu-tools/src/plugin/process.rs:348`. (reported; the producer read)

**M11. Duplicated helpers.** Truncate-with-ellipsis exists eight times; ANSI stripping twice with different rules; `~/` expansion five times; file locking three times in the plugins although `mu_common::fs::FileLock` exists (it only blocks, with no timeout or try); `write_atomic` and `write_private` differ in `create_new` and the directory fsync; two wrap implementations measure width per character and per grapheme. (reported; `FileLock` and the plugin locks read)

**M12. Process-wide state.** Set-once globals ignore a second set silently (`tools/edit.rs:169`, `secrets.rs`); library code prints to stderr (`agent/plans.rs:96`); owner state lives in statics (reported). Test helpers and production code read `HOME` directly in about a dozen places.

**M13. Synchronous I/O on the single-threaded runtime.** The stated rule is no file I/O on the draw path. Exceptions: jobs panel polling (every 250 ms with a transcript open, `jobs_panel.rs:292-344`), `/mcp` config loads, session cost on job completion, ACP `session/delete` and setup, and context loading at startup. None was measured. (polling read, the rest reported)

### 4.4 Tests, CI and release

**C1. The maintenance bot's gate is narrower than CI (medium, read).**
`scripts/maint/check.sh:22-26` runs build, test, contract and smoke: no `cargo fmt --check` and no clippy, both of which `ci-checks.sh` runs. A bump classified `s2` is merged right after that check (`run.sh:283-285`) without waiting for the PR's own CI run, and `publish()` tags `origin/main` after the same check (`run.sh:288-312`). The auto-merge itself was your recent decision (`2156194`); the point here is only that the gate behind it runs less than CI does. Also: the smoke stage is retried once and a retried pass is recorded as a plain pass (`check.sh:26`).

**C2. The test-only gate accepts weakened tests (medium, read).**
`scripts/maint/test-only.sh:21-35` accepts any hunk inside a trailing test module, including deleted tests and loosened assertions, and any file under `tests/`. A fixer diff that removes the failing test is "test-only" and merges unreviewed.

**C3. Lint policy exists only on a command line (medium, reported; absence of `[workspace.lints]` and `clippy.toml` checked).**
`scripts/clippy-workspace.sh:9` passes `-Dwarnings` to clippy's default groups. A plain `cargo clippy` or an editor sees a different policy. Nothing lints `unwrap` outside tests. There is no shellcheck for about 1,800 lines of shell, and no dependency audit. The release workflow runs tests and a build, without fmt or clippy.

**C4. Timing-dependent tests and shared process state (medium, reported; `mu-testutil` read).**
Fixed sleeps in `tools/shell/bash.rs`, `plugin/tests.rs`, `agent/loop_tests.rs` and `queue_tests.rs`; `pkill -f "sleep {arg}$"` patterns that can match another test's process (`bash.rs:947,983,1022,1025`); `mu_testutil::home()` calls `set_var` lazily from whichever test thread gets there first (`mu-testutil/src/lib.rs:11-28`), and `HomeGuard` sets `HOME` under a lock that other readers do not take. These are plausible sources for issue #21 (flaky tests, one workspace test hang); I did not confirm the link.

**C5. Gaps where the bugs above were found.**
No test covers `>&`, a redirect under a grant, `..` after a missing component, an enveloped edit header, a backslash in a fetch URL, or an overflowing retry hint. No loop-level test covers compaction. No test calls `App::draw`. `src/workflow/take.rs` has no tests. The jobs panel test reads the real `$MU_HOME/run` (`jobs_panel.rs:1343-1349`, reported).

**C6. Test hooks in production code (low, read / reported).**
`run_omp_blocking` is `pub` in `mu-common` and panics (`paths.rs:119-150`); `HANG_GRACE` is 200 ms under `cfg!(test)` and 5 s otherwise, so tests exercise different timing; a `__panic` tool is compiled into every build (reported).

**C7. The memory gate runs by hand (low, reported).**
`DESIGN.md` treats memory use as a principle; `mu-bench check` runs nowhere automatically.

### 4.5 Documentation drift

| What the text says | What is there | Checked |
|---|---|---|
| `AGENTS.md:17`: "`ci.yml` only runs on Linux, so a macOS-only failure first shows up when a release is tagged" | `ci.yml:29-43` has a `macos` job. Nothing gates a merge on either job (`ci.yml:30-31`). | read |
| `ci.yml:9`: "Four workspace builds (clippy and test, with and without default features)" | `ci-checks.sh` runs one test pass through the clippy wrapper. | reported |
| `design/jobs-panel.md:3`: "Status: proposal. Nothing here is implemented." | Phases 1 to 5 in the same file are marked implemented. It also promises a `y` copy key that does not exist. | read / reported |
| `DESIGN.md:67`: `mu-testutil` "is a dev-dependency only" | It is an optional normal dependency of `mu-agent` and `mu-provider` behind `test-support`. | read |
| `README.md:13`: the gateway pair is "about 240 MB" | `DESIGN.md:27` and `BENCHMARKS.md:15`: 306 MB total, 216 MB private. | read |
| `docs/secrets.md:3`: nothing with a recognised secret reaches the provider | The system prompt (context files, rules, MCP instructions) is not redacted (S6). | read |
| `scripts/maint/material.sh:29-30` lists patched `brush-core`, `brush-parser` | `Cargo.toml:109-111` patches `tree-sitter-go`, `grep-searcher`, `jaq-core`. This text is given to the triage and fixer agents. | reported |
| `plugins/gateway/src/daemon.rs:273`: the child dies with the daemon | Linux only (B13). | read |
| `chat/transcript.rs:246`: a retry drops everything streamed in the attempt | It clears the live text only (B8). | read |
| ACP comments say opening a session runs off the request loop | `setup()` runs synchronously inside it. | reported |

### 4.6 Product observations

These are behaviours a user may find surprising. Several are deliberate and tested; they are questions, not defects.

- **Ctrl+C never reaches a command in a pty**, and Esc kills it (`app/keys.rs:173-179`, `panels/ptyview.rs:59-72`). A REPL or a program waiting for Ctrl+C can only be killed.
- **One-key destructive actions without a confirm**: `X` in the jobs panel cancels every running job (`jobs_panel.rs:532`), `r` in the memory log reverts a commit (`overlay.rs:1094-1099`), and a bare `k` or `x` cancels a marked job (`keys.rs:273-295`). Stopping a session with `S` does confirm.
- **`mu -c` and `--resume` stop a live `-p` run that holds the session**, without asking (`src/run/mod.rs:153-158`). An in-place `mu deliver` is such a run.
- **Tool calls in one assistant message run one after another** (`agent/turn.rs:417-458`). Read-only calls could run concurrently.
- **`fetch` is never asked about in `ask` mode** (documented). A model that can read project files and call `fetch` can send their content in a URL. A per-host ask would close that; whether `ask` is meant to cover it is your call.
- **Ghost text reads other tools' histories** (omp's `history.db`, `~/.claude/history.jsonl`; `plugins/predict/src/corpus.rs:38-108`, reported), while the docs say "your own history".
- **The gateway pair lingers 30 minutes** after the last session (`daemon.rs:165`), which no doc page mentions.
- **The list filter is a substring match and the slash menu is fuzzy** (`select.rs:626`, `complete.rs:16-55`), so the same letters pick different rows.
- **ACP `initialize` with version 0 returns an error**; the protocol asks for an answer with the supported version (`src/run/acp/mod.rs:325-331`, test pins the error).

## 5. Plan

Sizes are relative: **S** is one or two files and a test, **M** is several files in one crate, **L** crosses crates or changes a public shape. Each workstream is independent unless it says otherwise.

Every finding in section 4 has a place below: in a workstream, or in section 6 as a decision. Status on 2026-11-12: A is merged (`8e3e6c3`); B is in progress on `feat/gate-one-parse` with option 2.

### A. Close the reproduced holes (S each; done, merged as `8e3e6c3`)

One PR per item, each with the probe turned into a regression test.

| Item | Options | Recommended |
|---|---|---|
| S1 fetch URL | (1) Parse once with `Url::parse`, take host and port from it, and send the parsed URL. (2) Reject backslashes, whitespace and control characters before the check. | 1. It removes the second parser instead of adding a rule to it. |
| S2 `..` | (1) Normalise `..` lexically in one place (`Tools::resolve`), then canonicalise the nearest existing ancestor. (2) Refuse a write or edit target that contains `..` and does not fully resolve. | 1. Legitimate `../` paths keep working. |
| S3 `>&` | (1) Treat `>&word` as output unless the word is digits or `-`. (2) Treat every `>&` as output. | 1. Option 2 would ask on `2>&1`. |
| S4 redirects under a grant | (1) Make each output redirect target a write subject with its own path check; a command grant then covers the command, not the file. (2) Never let a command grant cover a line with an output redirect. | 1, with 2 as a stopgap if 1 takes longer. |
| S5 edit headers | (1) Get the target list from pi-edit's own parse and gate that. (2) Mirror its envelope and whitespace rules in `edit_paths` and test against its fixtures. | 1 if pi-edit exposes the targets before applying (the tool already calls `begin_apply_url_targets`; whether plain paths are available the same way needs a look). Otherwise 2, plus failing closed: an edit payload with no recognised target asks. |
| S12 durations | `Duration::try_from_secs_f64` at the three sites. | No alternative worth weighing. |
| S10 `HOME` | (1) Fail at startup with a message when neither `HOME` nor `MU_HOME` is set. (2) Fall back to the passwd entry. | 1. One function, and it turns a silent misplacement into an error. |
| S11 bad mode | (1) Keep `yolo` only when `mode` is absent; an unparseable value falls back to `ask`. (2) Refuse to start. | 1. A broken config should not block every session, and should not open everything either. |

### B. The permission gate: what it is, and how it is built (option 2 chosen; in progress)

A is patching. The question behind it is what `ask` and `edits` are meant to be.

- **Option 1: guard rails for a cooperative model.** Keep the design, fix A, and say in `docs/permissions.md` that the modes prevent mistakes and are not a boundary against a model that is trying to get past them. S7, S8 and S9 then become documentation: deny rules match text as written, the classifier knows common forms. Cost: S. Risk: users who read "deny always wins" as a guarantee.
- **Option 2: one parse, shared with the executor.** Restructure so the gate cannot disagree with what runs: paths normalised once; edit targets from pi-edit; redirect targets and wrapper-unwrapped commands from the pi-shell AST as subjects; deny and ask rules matched against the normalised program name as well as the text; a tool's tier declared by the tool or its plugin manifest instead of a name list (fixes S13d); no grant offered when there is nothing to scope it (S13e). Split the file into classify, grant and verdict (M1). Cost: M to L, in steps. This is what makes A stay fixed.
- **Option 3: an OS-level boundary for `bash`** (Landlock on Linux, Seatbelt on macOS), with the classifier kept for prompts. The only option that holds against a hostile command line. Cost: L, two platforms, a project of its own.

Recommended: 2, taken in steps after A, and the honest sentence from 1 in the docs now, since a flag-by-flag classifier (S7) will always have gaps. Leave 3 until there is a concrete need.

The remaining gate findings belong here too:

| Item | Options | Recommended |
|---|---|---|
| S7 flags the classifier lets through | (1) Give `rg` and `tree` an allowed-flag list like the other programs, and make a flag that names a file (`-f`, `--files0-from`, `-o`, `-O`, `--pre`, `--compress-program`) either a path subject or a reason the command is not read-only. (2) Leave it to the sentence in the docs. | 1 for the seven forms in S7, as a step of option 2. The docs sentence stays: other flags will exist. |
| S8 `deny` rules | Covered by option 2 (rules matched against the unwrapped, normalised program name as well as the text). `sh -c '...'` needs the inner line parsed too; if that is not done, `docs/permissions.md:43` should say which forms a `deny` sees. | |
| S9 `rm` guard | (1) Correct the comment at `guard.rs:77` to match the docs. (2) Also resolve `..` lexically, so `rm -rf /..` and `rm -rf ~/../..` are refused. (3) Follow `cd`, variables and pipes. | 1 and 2. 3 is a shell interpreter; option 3 of this workstream is the real answer to it. |
| S13f `/mcp reconnect` | Refuse to reconnect a server whose entry is no longer in the trusted set, and say why. | No alternative worth weighing. |
| S13g tool-call hooks | (1) Run hooks in a fixed order, each on the previous one's arguments, and let a hook be marked required so that its failure or timeout blocks the call. (2) Keep them concurrent and make two different rewrites an error. (3) Document the current behaviour. | 1. Whether a failing hook lets the call through is a decision (section 6). |

For S6 (`@` imports) the choices are: (1) confine imports to the project and mu's own directories, read only regular files under a size cap, and treat anything else as an outside read that needs a rule; (2) keep them unrestricted and list every outside import in a startup notice; (3) pass the system prompt through redaction. Recommended: 1 and 3 together. 3 alone only hides secrets that match a known shape. Decided: 1 only.

### C. Stop failures from turning into empty values (S to M)

| Item | Options | Recommended |
|---|---|---|
| B1 damaged session line | (1) Warn on load: "N earlier entries are unreachable (line X is damaged)". (2) Also recover the `id` and `parentId` of a damaged line so the chain stays connected. (3) Refuse to resume. | 1 now; 2 if damaged files turn up in practice. 3 strands the user. |
| B2 memory file | (1) `load` returns an error for a file that exists and cannot be read or decoded, and the edit is refused. (2) Keep going and write a `.bak` first. | 1. |
| B10 `auth.json` | (1) Keep the read error next to the snapshot so `resolve` can say why. (2) Call `check()` on the error path. | 1. Also keep a refreshed token in memory when the save fails. |
| B11 session reopen | Open the new handle before letting go of the old one. | |
| B3 compaction | One formula for the threshold with a monotonicity test; cap the kept tail at a fraction of the threshold; remember a failure until the context grows or a new prompt arrives. | All three; they share a test. |
| B12 memory queue | Move a failed job to the back, or give up after N attempts with one notice; separate "the model's reply was rejected" from infrastructure errors. | Both. |
| B5 `edit` | Use `write_atomic`, as `write` does. | |
| B4 bash drains | On leader exit, terminate the group first and then drain under a hard cap, in both paths. | Fits the tool's documented rule that background descendants do not outlive the call. |
| B13 gateway daemon start | (1) Derive the starter's wait from the daemon's own step limits, kill the omp children's process groups as well as the daemon, and give the children a portable parent-death signal (a pipe they see close) for macOS. (2) Only raise the 30 s and fix the comment. | 1. 2 leaves the orphans on macOS. |
| B14d mcp-http bridge | Check first whether the next call respawns the bridge. If not: retry a transport error once inside the bridge, and report it as a call error, not an exit. | |
| B14e subagent output | Report an unparseable child line as the result's error, with the line; bound the wait for stderr. | |
| B14j grant not written | Return what was written, and say "already covered by ..." when nothing was. | |
| B14n agent task panic under ACP | Check the task result as headless does, and answer the pending `session/prompt` with an error. | |

### D. One gate (S)

- `scripts/maint/check.sh` calls `scripts/ci-checks.sh` for its build and test stages, so the bot, CI and `ship.sh` run one list (C1). Options beyond that: (1) also wait for the PR's CI run before the squash; (2) leave as is. Decided: 1, wait for CI.
- `test-only.sh` rejects a diff that removes a `#[test]` or lowers the count of assertions in a touched module (C2); such a bump goes to review.
- Move lint levels into `[workspace.lints]` (C3). Option: add `clippy::unwrap_used` as a warning outside tests and count before deciding whether to enforce it.
- Add shellcheck for `scripts/`. `cargo deny` is optional; the git-pinned dependencies are the argument for it.
- Fix the drift in section 4.5 in one pass, `AGENTS.md` first, since agents act on it.
- Record a retried smoke pass as such.
- The release workflow runs `scripts/ci-checks.sh` instead of its own test and build list, so fmt and clippy also gate a release (C3).
- `ship.sh` exits non-zero when it could not fast-forward the main checkout, instead of installing an older commit and reporting success (B14c).
- `-dirty` (B14b). Options: (1) make `build.rs` rerun on every build, which rebuilds the top crate each time; (2) have `mu update` and `install.sh --skip-if-dev` ask git themselves instead of trusting the suffix. Recommended: 2.
- The memory gate (C7). It was taken out of CI in `829c1cd`. Options: (1) run `mu-bench check` in the release dry run; (2) run it on a schedule; (3) leave it manual and say so in `DESIGN.md`. Decided: 3. The baseline is calibrated to one machine, which is why it left CI.

### E. A written rule for errors (M, then gradual)

- **Option 1:** `anyhow` inside a crate; a typed enum wherever a caller branches on the kind; `String` only at display and RPC boundaries. Migrate `mu-session` and `mu-base` first.
- **Option 2:** one `thiserror` enum per crate. More uniform, more churn, a new dependency.
- **Option 3:** write down the current practice and stop there.

Recommended: 1. Independent of that choice: give JSON-RPC errors a machine-readable kind and stop matching on message text between the core and a plugin (M10), print `{e:#}` in `main` (B14a), and return an error from set-once globals on a second set.

The rest of M12 goes with it: library code returns a warning to its caller instead of printing to stderr (`agent/plans.rs:96`), and the owner state in statics moves into a value that is passed in (reported; read it before planning).

### F. TUI structure (M each, in this order)

1. **`Picker<T>`**: a `Select` that carries a payload per item and returns it. This removes the parallel vectors, the duplicated `select()`/`select_mut()` and most of `Overlay::on_key` (M3). Option: a macro over the repeated arms, which is less work and hides the same duplication.
2. **One table for slash commands**: name, description, argument (none, optional, required), allowed while busy, handler. Help, completion and dispatch read it (M4). Fixes the extra Enter.
3. **One owner of input**: an enum for the active flow instead of five `Option`s, and one helper to open a dialog that queues behind a pending approval, ask or plan review (M4).
4. **Split `Select`** into state, layout and preview without changing its API, and replace `editing`/`noting` with one enum (M3).
5. **Shared panel frame**: one helper that takes header, body and footer and returns the body height, used by plan review, jobs, reply and the pickers (M5, B9).
6. **Jobs panel data behind a source** in `mu-session` that returns snapshots, so the panel only renders and the tests need no real run directory (M5, M13). Whether the reads then move off the UI thread is a separate choice: the design note says no worker. I would measure first.
7. Paste markers as atomic tokens in the editor (B7).

Small TUI fixes, each S and independent of the steps above:

| Item | Options | Recommended |
|---|---|---|
| B6 retry notice | Carry `max_attempts` in the retry event and print it. | |
| B8 failed attempt's text | (1) `reset_stream` also drops the attempt's text from `block_text`, as the comment at `transcript.rs:246` says. (2) Keep the failed attempt on screen, marked as discarded, and leave it out of `/copy`. | 1. |
| B14h render cache key | Key the live-render cache by block identity or a generation counter, not by text length. | |
| B14i fallback screen text | Delete the "nothing is tried" line; the `@task` line is what the code does. Take the preview's rule from `mu-base/src/roles.rs` instead of restating it (M5). | |
| B14p editor cursor | Keep the cursor's column offset instead of recounting from the line start. | Measure first; it is from reading. |
| B14q open code fence | Highlight only the lines added since the last pass, when the highlighter's state allows it. | Measure first; reported, not read. |

### G. Tools, agent and entry points (M each)

- **Bash paths**: one spawn, collect and finish sequence shared by the four paths, with the kill and drain policy in one place (M2, B4). Give a foreground command its own slot outside the eight-job limit, or count only background work.
- **Agent loop**: name the loop's states instead of five flags; mark reminders with a field instead of matching their text; one vector of `(message, entry id)` (M6). Add loop-level tests for compaction and overflow before touching it.
- **`run()`**: one small function per mode, with the mode's data inside the enum variant, so the `expect`s disappear (M7).
- **Shared helpers into `mu-common`**: truncate, `~` expansion, ANSI stripping, a `FileLock` with timeout and try (M11). The plugins can use them.
- **`mu-base`**: make internal modules `pub(crate)` as `mu-agent` does; split `config.rs` by section (M8). Lower priority.
- **Provider**: one cache helper for the three model and pricing caches; one error classifier with per-wire inputs (M9). Split `provider/custom.rs` into schema, registry writes and the save workflow, with the UI labels moved to the TUI (M9).
- **`Job`**: an enum per kind (subagent, bash) instead of shared fields and flags, with completion written in one place (M2).
- **`src/cli/auth.rs` and `src/workflow/deliver.rs`** (M7): move the no-echo terminal layer and the login flow out of the clap file; split `deliver.rs` into the record, the steps and the loop. Both as the files come up.
- **The other synchronous I/O sites** (M13): `/mcp` config loads, session cost on job completion, ACP `session/delete` and setup, context loading at startup. Measure each; move the ones that show up to `spawn_blocking`. For the ACP ones, fix the comment if the code stays (4.5).

Small fixes in this area, each S:

| Item | Options | Recommended |
|---|---|---|
| B14f `models.toml` | Take `FileLock` around the read-modify-write, as the provider registry does. | |
| B14k pre-flight model fetch | Give it an overall timeout; use its result or stop making the request. | |
| B14l request body | (1) Drop the JSON tree once it is serialised. (2) Serialise straight into the request stream. (3) Correct `DESIGN.md:219` to describe what happens. | 1, and 3 until 2 is worth doing. |
| B14m tool pairing check | Check pairing over borrowed messages; no clone. | |
| B14o web fetch | Four separate fixes: give the `llms.txt` probe its own short budget; leave blank lines alone under `raw=true`; refuse or describe a binary GitHub blob; run the HTML conversion on a blocking thread. | Reported; read each before fixing. |
| B14r session search | Open a read transaction and write only for files that changed. | Reported. |
| B14s schema validator | (1) Implement `pattern` and treat `format` as an annotation. (2) Warn once per tool for a constraint keyword that is not checked. (3) Document the supported set. | 3 now, 2 if a tool depends on one. |
| B14t macOS paths | `self_exe()` falls back to `std::env::current_exe()`; the OS release comes from `uname` where `/proc` is absent. | Needed for the "both platforms" rule in `AGENTS.md`. |

### H. Tests (S to M, alongside the others)

- Replace fixed sleeps with waits on observable state, starting with the files named in C4; track test children by pid instead of `pkill -f`.
- Pass the home directory into the code under test instead of setting environment variables from test threads. This is the same change as making `paths::home()` fallible (A, S10).
- Put `run_omp_blocking` and the `__panic` tool behind `test-support` (C6).
- A frame-level test of `App::draw` with an injectable writer.
- Tests for `src/workflow/take.rs` (C5). The jobs panel test stops reading the real `$MU_HOME/run` once F6 gives the panel a source.
- Pass the hang grace in as a value, so tests set a short one explicitly instead of `cfg!(test)` changing a constant (C6).

### I. Smaller security items (S each, independent)

| Item | Options | Recommended |
|---|---|---|
| S13a file modes | (1) Create session files and `history.jsonl` with 0600 and `~/.mu` with 0700. (2) Only the directory. | 1. Whether to tighten files that already exist is a small choice; I would do it when a file is next opened for append. |
| S13b sign-in callback | Check `state` before anything else, and answer with fixed text instead of echoing the `error` parameter. | |
| S13c client fallback | Return the builder's error instead of falling back to `reqwest::Client::new()`. | |
| S13h plugin requests | A timeout and a cancel path for `core/complete`; a cap on concurrent requests per plugin. | The cap's value needs a look at what plugins send. |
| S13i late-exported secrets | (1) Refresh the redactor's snapshot from the shell's exported variables after a `bash` call. (2) Document the limit in `docs/secrets.md`. | 1 if the shell exposes its exports cheaply (needs a look); otherwise 2. |
| S13j `signal_pid` | Refuse pid <= 0. | |
| S13k internal addresses | Add `100.100.100.200` and the whole of `0.0.0.0/8`. | |
| S13l release | Give `contents: write` to the publish job only. For the installer: (1) verify a GitHub artifact attestation or a signature; (2) keep the same-release checksum and say it detects corruption, not tampering. | The permissions change now. The installer is a decision (section 6). |
| S13m wizard summary | (1) Pass `args` through the redactor. (2) Mask every argument. | 1. Reported; read it first. |

### Suggested order

1. A (all items), with the docs sentence from B option 1. Done.
2. B option 2 in steps. In progress; S7, S9, S13f and S13g join it.
3. C: B1, B2, B10, B5 first.
4. D, and I (each item is a small PR).
5. F1 and F2.
6. The rest of F and G as the files come up for other reasons, the small-fix tables in any order; E's rule before any of them touches error types.

## 6. Decisions

All were made on 2026-11-12. Where a decision differs from a recommendation in section 5, the decision holds.

1. **What `ask` and `edits` promise** (section 5B): option 2, one parse shared with the executor. Inside it:
   - S7: fix the seven listed forms (`rg` and `tree` get allowed-flag lists; a flag that names a file is a path subject or makes the command not read-only). No allow-list for every program.
   - S8: `deny` and `ask` rules also see inside a literal `sh -c '...'`, `bash -c` and `eval`; the inner line is analysed like the outer one. How a non-literal `-c` string is treated is for the implementation plan.
2. **`@` imports** (S6): confine. Only the project and mu's own directories, regular files under a size cap; anything else needs a permission rule. The system prompt is not passed through redaction, so `docs/secrets.md:3` has to be corrected instead.
3. **`fetch` in `ask` mode**: ask per host, with hosts returned by `web_search` in the same session exempt.
4. **The bot's gate** (C1, C2): `check.sh` runs `ci-checks.sh`, and the bot also waits for the PR's CI run before the squash. A fixer diff that removes a test or lowers the assertion count is not test-only and goes to review.
5. **Error rule** (section 5E): option 1. `anyhow` inside a crate, a typed enum where a caller branches on the kind, `String` only at display and RPC boundaries.
6. **`HOME` unset** (S10): mu fails at startup (done with A).
7. **Session directory names** (B14g): keep omp's naming; stored grants record the project's absolute path and apply only when it matches.
8. **Product questions** in 4.6:
   - Ctrl+C in a pty: stays as it is.
   - Confirms: `X` (cancel every job) asks. `r` in the memory log and `k`/`x` on a marked job stay one key.
   - `mu -c` and `--resume` on a session held by a live run: ask before stopping it. What a non-interactive caller gets is for the implementation plan.
   - Read-only tool calls in one message run concurrently. To be planned, after the loop-level tests in G.
   - Ghost text: reading other tools' histories becomes a setting, and the docs say what is read.
   - The gateway pair's 30-minute linger: document it.
   - Filtering: substring in lists and fuzzy in the slash menu stay.
   - ACP `initialize` with version 0: answer with the supported version.
9. **Tool-call hooks** (S13g): fail-open stays the default; a hook can be marked required, and then its failure or timeout blocks the call. Hooks run in a fixed order, each on the previous one's arguments.
10. **The memory gate** (C7): manual, and `DESIGN.md` says so. `829c1cd` took it out of CI because its baseline is calibrated to one machine.
11. **The installer's check** (S13l): the release job attests the tarballs and the installer verifies the attestation when `gh` is present, with the checksum as the fallback.
12. **Detailed plans**: workstream C next. None for D to I yet.
