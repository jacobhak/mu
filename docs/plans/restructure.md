# Restructure: break up the god-objects, split `src/` into layered crates

Status: all decisions agreed (Jacob, 2026-11-04). Implemented on branch `feat/restructure`; see "Implementation notes" at the end.

## Context

mu is 12 days old, has had ~200 commits and holds 60k lines (about 40% of them inline tests). `src/` has 42 flat
top-level modules in one crate. The size is not the problem by itself. Most of the large files are cohesive:
`select.rs`, `markdown.rs`, `editor.rs`, the three provider `Assembler`s and the MCP data modules. The real problems
are these:

**God-objects**
- `Agent` (`src/agent.rs:31-98`) has 50 fields and ~2,200 production lines in one `impl`. It owns the turn loop,
  session lifecycle, plan mode, review orchestration, job/notify delivery, redaction, quota/pricing, title generation
  and a 20-arm command dispatcher. A second `impl Agent` lives in `compaction.rs:158`. Six functions are over 110 lines:
  `review_run` 241, `run_loop` 211, `turn_from` 136, `run_tool` 129, `run` 117, `load_session` 114.
- TUI `App` (`src/tui/app.rs:174-228`) has 41 fields and a ~1,300-line `impl`. It is the event loop, the agent-event
  reducer (`on_agent`, ~200 lines), the slash router, every picker, the child-process runner, a config writer and
  draw. It mutates the agent's `SharedInbox` directly (`app.rs:1019-1020`).
- `PluginHost` (`src/plugin/mod.rs`) has 31 methods across five concerns: registry, MCP lifecycle, spawning, hooks and
  dispatch. `Slot` encodes native-or-MCP as `manifest` plus `mcp: Option<McpSlot>`, which leads to ~25 `slot.mcp`
  branches and fabricated manifests for MCP servers (577-585).
- `Providers` (`src/provider/mod.rs:273`) mixes the HTTP client, the gateway model-list cache, the key store, the
  gateway source, role-fallback `complete` and a test queue. `attempt()` is 81 lines.
- `main.rs::run()` (317-608) is 292 lines. Its mode is encoded as booleans that are recomputed at each use
  (`headless_run`, `task_payload.is_none()`, `!cli.headless`).

**Cycles and layering leaks** (production code only)
- core → TUI: `jobs.rs:18` and `tools/mod.rs:20` import `tui::line::truncate_text`.
- agent ↔ plans (`plans.rs:432` calls `agent::tool_calls`). TUI → agent (`session_view.rs:10` calls `history_events`).
- tools ↔ jobs (`BackgroundResult`, `run_child_observed`), plugin ↔ tools (`is_builtin`), context ↔ tools
  (`context.rs:47,473` use `tools::grep::walk_options` and `tools::edit::is_hashline`; `tools` uses
  `context::AgentDef`), provider → gateway → plugin → provider, secrets ↔ auth_store, auth_store ↔ provider::native,
  models ↔ provider::{native,catalog,discovery}.
- Core depends on workflow: `agent.rs` imports `review`, `plans`, `notify` and `gitinfo`, and spawns `mu deliver`
  and the review child itself. `events.rs` (the protocol) imports `plans::PlanMeta`, `quota`, `gitinfo` and `tools`.
- `tools` depends on `provider` for `ToolDef`. `tools::summary()` hardcodes the names of plugin tools.

**Duplication**
- `/review` (`agent.rs::review_run`, 1130-1370) repeats `mu review` (`review.rs::review_steps`, 504-570) step for step.
- Agent setup: `main.rs:437-481` and `acp.rs:524-538`. Print mode and subagent mode in `headless.rs` are two
  near-identical state machines. The retry message is written three times.
- Provider wires: the tool-pair backstop, the `wire()` shell, the SSE feed loop and the tools JSON map are copied in
  each of the three. There are three `total_tokens` formulas, three stop-reason maps and three stream-error
  classifiers. `anthropic_native.rs` rebuilds the top-level body of `anthropic.rs` and reaches back into it for
  `clamp_budget`.
- Process handling: three kill escalations, four copies of grouped-spawn setup, three spawn-and-capture paths for
  git, `deliver::detach` against `pty.rs:84`, `deliver::try_lock` against `session::try_flock`, and four
  implementations of "signal only the same live run" (`take.rs:97,120,178`, `notify.rs:187`).
- In the agent: compaction-failure reporting is copied 5 times, the skipped-tool emit twice, the `local://` file write
  3 times and the `drive(...)` call 7 times. The `["ask","propose_plan"]` skip list appears 3 times and the `len()/4`
  token heuristic 4 times. Plan decisions take three forms: `PlanDecision`, `PlanMode` and string literals.
- In the TUI: there are two markdown walkers (`reply.rs` `Builder` and `markdown.rs`), and about 9 hand-written
  `Pending/Chosen/_` dispatches in `Overlay::on_key`. `env_is_set`/`env_set` are duplicates, and `fn key()` is
  redeclared in 8 test modules. The `tmp`/`repo`/`sh`/`rt` test helpers are copied into deliver, review, plans and
  `plugin/mcp`.

**Bugs found while reading**
- `plugin/mod.rs:270`: `Proc::shutdown` runs `kill(-pid, SIGKILL)` with no `pid > 1` guard, and `pid` comes from
  `child.id().unwrap_or(0)` (1030). Pid 0 would SIGKILL mu's own process group.
- `plugin/mod.rs:443`: `sibling_or_path` canonicalizes `current_exe` before it looks for the sibling binary. After a
  reinstall (the `(deleted)` case that `resolve_command` at 137 handles) it falls back to a bare PATH lookup for
  `mu-mcp-http`. Nothing tests this. (The empty `fix/plugin-reinstall` worktree may be meant for this.)
- `tools/bash.rs:630,635,644` await `terminate_group` (200 ms) inside a `select!` arm, which stalls output reads.

## Decisions

1. **Depth: moves plus seams** (agreed). The plan moves code, and it also adds sub-structs for agent and app state,
   `AgentInit` in place of field mutation after construction, shared front-end setup, a `Slot` enum, a wire trait and
   shared review fact-gathering. Each of these is decided later, one by one: shared front-end event handling, an
   overlay/picker trait, and a single markdown walker.
2. **Layers are enforced by workspace crates** (agreed). The compiler rejects a dependency cycle. A new edge between
   crates is a `Cargo.toml` change that shows up in review. No `check-layers` script is needed.
3. **No shims** (agreed). Every call site is rewritten in the phase that moves its target. Crates do not re-export
   lower crates' items to keep old paths alive.
4. **No behaviour changes** (agreed), except the three bug fixes in Phase 0. The deduplications that would change
   behaviour are listed under Out of scope.
5. **Freeze** (agreed). `src/` is frozen from the start of Phase 1 until Phase 2 lands, because those two phases
   touch imports in almost every file. The `predict/*` branches touch only `plugins/predict` and are not affected.

## Target crates

Each crate may depend only on the crates below it, as listed. Nothing depends on `mu-tui` or `mu`.

| Crate | Holds | Depends on |
|---|---|---|
| `mu-common` (exists, shared with the plugins) | paths, fs, rpc; gains `time` (`iso_ms`) | — |
| `mu-base` | protocol and foundation: `message`, `image`, `events` (with `PlanMeta` and the quota and git snapshot types moved in), the tool contract (`ToolDef`, `ToolOutput`, `ToolMeta`), `text` (from `tui/line.rs`), `process` (`proc.rs`, the group registry, `spawn_grouped`, `detach`, the lock helper) and `pty`, `jsoncache`, `schema` + `jtd`, `secrets`, `config`, `models` (types, roles, `resolve`), `git` (`gitinfo` + one git runner), `mem` | mu-common |
| `mu-provider` | `provider/` (types, client, complete, gateway_models, retry, sse, `providers.rs` table, `wire/`), `model/` (catalog, discovery, pricing, cost, `native_spec`), `auth/` (store, key check), `gateway/` (mod, quota) | mu-base |
| `mu-session` | `session/` (file, format, list, owner with a `RunKind` enum, tail, inbox from `notify`, control from `take`) | mu-base |
| `mu-tools` | `tools/` (with `shell/`, `subagent.rs`, `spill.rs`), `jobs/`, `plugin/` (host, process, mcp_runtime, manifest), `mcp/` (the data layer from `plugin/mcp`), `context/` discovery (files, skills, agents, commands, `at_imports`) | mu-base, mu-provider, mu-session |
| `mu-agent` | `agent/`, `compaction`, `prompt` (the system prompt from `context.rs`), `ask_nudge`, `policy`, `plans/` (store, drift), `review/` (facts, report), `AgentHandle` and `build_agent` | everything above |
| `mu-tui` | `tui/` (layout below), palettes, the tree-sitter `lang-*` features | everything above |
| `mu` (binary, stays the root package) | `main.rs`, `cli/` (Cli, Sub, select, auth from `auth_cmd`, predict from `predict_setup`), `run/` (Mode, setup, signals, headless, `acp/`), `workflow/` (deliver, review_cli, plans_cli, notify sender), `update.rs`, `build.rs` | everything |

Plus `crates/mu-testutil`, a dev-dependency only: `home()`, `fake_server`/`Reply`, `tmp`, `repo`, `sh`, `rt`, `key`.

Notes:
- `mu-base` is the largest of these crates. It holds only leaf code (nothing in it starts a model request, a tool or
  a session). It may be split again later if it grows.
- The tools/provider edge exists only because the plugin host serves `core/complete` through `Providers`. That is a
  real dependency, not a leak.
- The root package's `lang-*` features forward to `mu-tui/lang-*`, so `cargo build --features ...` behaves as it
  does today.

`mu-tui` layout: `app/` (mod, agent_view, ui_msgs, slash, pickers, child, keys, mcp), `widgets/` (select, editor,
complete, overlay), `panels/` (jobs_panel, sessions_tab, session_view, reply, plan_review, ptyview), `chat/` (composer,
transcript, blocks, partial_json), `mcp/` (mcp_cmd, mcp_presets, mcp_wizard), `text/` (markdown, highlight,
diffview), `term/` (term, input, clipboard, bell, probe), `render.rs`, `status.rs`, `theme.rs`.

**Leave alone**, even though some of these files are large: `select.rs`, `editor.rs`, `markdown.rs`, `highlight.rs`,
`theme.rs`, `input.rs`, `status.rs`, `keys_ui.rs`, the provider `Assembler` state machines, `anthropic_native`
thinking and body code, `sse.rs`, `retry.rs`, `catalog.rs`, the MCP data modules, `convert.rs` (wyhash pinned by
vectors), the per-tool files, the `BUILTINS` table, `jobs::OutputSink`, `pty::Cleaner`, `session` `parse` (move it
intact), `config::rewrite_role`, `ask_nudge.rs`, `at_imports.rs`, `policy.rs`, `schema.rs`/`jtd.rs`, and `take.rs`
apart from its live-run check.

## Phases

Every phase lands on its own and keeps `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked --
-D warnings` and `cargo test --workspace --locked` green. Only Phase 0 changes behaviour. Use `git mv` for moves, and
keep a move and an edit of the same file in separate commits so `git log --follow` and blame survive.

### Phase 0: bug fixes
- Route `Proc::shutdown`'s kill through `proc::terminate_group`, which has the `pid > 1` guard.
- Make `sibling_or_path` resolve the way `resolve_command` does, and add a test for the `(deleted)` exe path.
- Move `terminate_group` in `bash.rs` out of the `select!` arm, or spawn it, so output keeps draining.

Done when: there is a test for each fix that fails on `main`.

### Phase 1: break the cycles, still in one crate
Iterating is cheap inside one crate, so every edge that would block a crate boundary is cut here first.
- `tui/line.rs` → `src/text.rs`. `tool_calls` (one version) and `history_events` → `message.rs`.
- `ToolDef` joins `ToolOutput`/`ToolMeta` in `tools/output.rs` (headed for `mu-base`). `provider` imports it from
  there.
- `plugin` takes a `reserved: fn(&str) -> bool` instead of calling `tools::is_builtin`, and returns `ToolOutput`
  instead of its own `ToolResult`.
- `BackgroundResult` and `run_child_observed` → `tools/subagent.rs`, which breaks tools ↔ jobs.
- context ↔ tools: the walk options move to a neutral module, and the system prompt gets the edit mode as an
  argument, so `context` does not read `tools::edit`.
- `gateway::validate_url` → a util module. `GatewaySource` takes a start function instead of importing `PluginHost`.
- secrets ↔ auth_store: the store pushes stored keys into `secrets`, and nothing calls back.
- `models::native_spec` moves to the provider side, so `models` imports only `config`.
- `events.rs` stops importing domain modules: `PlanMeta` and the quota and git snapshot types move into it or below
  it.
- Agent → workflow: `agent` keeps plan mode and `/review`, but spawning `mu deliver` goes behind a function the
  binary passes in through `AgentInit`.
- `iso_ms` → `mu_common::time`.
- Test plumbing: create `crates/mu-testutil` and move `testutil.rs` into it. Every `#[cfg(test)]` hook that another
  module's tests use (`Providers::scripted` and `default_store`; the hooks in `cost`, `jobs`, `plugin`, `quota`,
  `session`, `tools`, `task`, `manifest`, `auth_store`) is put behind a `test-support` cargo feature on the crate it
  will land in. The upper crates enable that feature in their `[dev-dependencies]`, so release builds never contain
  the hooks.

Done when: each edge listed under Cycles above is gone, so a grep for its import in production code finds nothing,
and `mu-testutil` is the only place the shared test helpers are defined.

### Phase 2: extract the crates, bottom-up
Before the first extraction, record the release binary size and idle private RSS (`scripts/measure-idle.sh`).
1. Workspace prep: `[workspace.package]` (version, edition, license) and `[workspace.dependencies]` for the shared
   dependencies (tokio, serde, reqwest, the pi-* git deps, …), so every crate uses the same versions.
   `env!("CARGO_PKG_VERSION")` in the plugin handshake and ACP then still reads mu's version.
2. Extract the crates in this order, one commit per crate: `mu-base`, `mu-provider`, `mu-session`, `mu-tools`,
   `mu-agent`, `mu-tui`. Each extraction is `git mv`, a `Cargo.toml`, visibility changes and import rewrites, with no
   body edits. The directory regrouping inside each crate happens in the same move. Make items `pub` only where
   another crate uses them, and keep everything else `pub(crate)`.
3. The binary keeps `main.rs`, `cli/`, `run/`, `workflow/` and `update.rs`. `build.rs` and the `MU_VERSION`,
   `MU_GIT_SHA` and `MU_OMP_VERSION` env vars stay on the binary, which is the only user of them.
4. `include_str!`/`include_bytes!` assets move with their users:
   - the plan, compaction and review-request prompts go to `mu-agent`;
   - `ask-tool.md`, `todo-tool.md` and the review-lead and review-worker agent prompts go to `mu-tools`;
   - `catalog.json` and the provider `testdata/` go to `mu-provider`;
   - the palettes go to `mu-tui`;
   - `plugin/mcp/testdata` goes to `mu-tools`.

   Update `scripts/maint/gen-catalog.sh`, which writes `src/provider/catalog.json`, and `scripts/smoke.sh:44`, which
   picks `src/*.rs src/*/*.rs` as fixtures.

Done when:
- `cargo tree -e normal --depth 1 -p <crate>` shows only the edges in the Target crates table, for every crate;
- root `src/` holds only the binary's modules;
- `cargo build --release --workspace --locked` and `scripts/install.sh` work unchanged;
- release binary size and idle private RSS are within 5% of the recorded figures. If they are not, stop and bring the
  numbers to Jacob. Any profile change (`lto`, `codegen-units`) is his call, because it trades against build time.

From here on, Phases 3 to 8 each stay inside one or two crates and can run in parallel worktrees. Phase 9 comes last.

### Phase 3: agent (`mu-agent`)
1. One `impl Agent` per file under `agent/`: mod (struct, new, `run` dispatcher), turn, tool_exec, session, plan,
   review, inbox, status, compaction, title. This is a pure move, with fields `pub(super)`.
2. Helpers: one `report_compaction_failure`, one `emit_skipped`, one `write_local_file`, one `drive` wrapper, one
   constant for the front-end skip list and one `estimate_tokens`.
3. Sub-structs:
   - `ModelState` {model, thinking, live_thinking, pins, pricing_checked};
   - `PlanFlow` {plan, exec_plan, stored_plan};
   - `Mailbox` {notify, seen, announced, wake_*};
   - `TitleState` {the 5 title fields};
   - `RunConfig` {subagent, excluded, allow, agent_prompt, parent_session, exit_on_resume_failure}, passed in
     through `AgentInit`.

   No public fields get mutated after `new()`.
4. `turn_from`'s `Option<Option<UserInput>>` and its booleans become an enum. `PlanMode` folds into
   `PlanDecision`, with strings only at the tool boundary.
5. `agent.plan` / `tools.plan` and `agent.jobs` / `tools.jobs` stop mirroring each other: each gets one owner.

Done when: `Agent` has ≤ 20 direct fields, no file in `agent/` is over ~600 production lines, no function there is
over ~100 lines, and `loop_tests` passes unchanged.

### Phase 4: front-end seam and workflow dedupe (`mu-agent`, binary)
- Add one `build_agent(cfg, cwd, role, mode) -> AgentHandle`, used by `run/` and `acp`. `AgentHandle { cmd, ev }` has
  `send/stop/cancel`. Steer, follow-up and cancel go through `Command` only, so no front-end touches `SharedInbox`.
- `headless`: print mode and subagent mode share one event accumulator, and one formatter writes the retry message.
- `main.rs::run()` → `run/mod.rs` with `enum Mode { Tui, Print, Plan, Subagent }`. Keep the signal-handler order as
  documented. First add a smoke test per mode: `-p` against the scripted provider, and `--headless` with a payload.
- `review::facts::gather()` is shared by `/review` (agent) and `mu review` (workflow). The two front ends stay
  separate. Add a test for the agent review path, which has none today.
- Make one of each: git runner (`mu-base` `git`), `detach`, lock helper, live-run check, `short_sha`, and a
  `current_thread_runtime()` helper for deliver, review, plans, take and main.

Done when: `grep -rn 'inbox.borrow' crates/mu-tui src/run` finds nothing, `run()` is gone from `main.rs`, and
`agent/review.rs` does not reimplement what `review/facts.rs` gathers.

### Phase 5: TUI (`mu-tui`)
- Split `app/` along its method groups. Add an `AgentView` struct (transcript, tools, calls, jobs, todo) that owns
  the `on_agent` reducer and can be tested without `App`.
- `App`'s writes to config, the auth store, the MCP store and sessions go through a small `Services` struct, not
  through `mu_*` calls scattered across `app`, `keys` and `mcp`.
- `jobs_panel` stops reading session files from its render path. Polling moves to `on_ui`.
- `pad`/`wrap`/`accent` move to `tui/text`, so `sessions_tab` no longer depends on `jobs_panel`/`reply`.
- Deduplicate `env_is_set` and `accent_bold`.

Done when: `App` has ≤ 20 direct fields, `app/mod.rs` is under ~500 lines, and `on_agent` has unit tests.

### Phase 6: providers (`mu-provider`)
- Split the contract and client into:
  - `types.rs` (the contract);
  - `client.rs` (route, attempt and stream, with `attempt` split into build, status and `pump_sse`);
  - `gateway_models.rs` (one `/v1/models` row parser);
  - `complete.rs`.
- Add `wire/` with a `WireFormat` trait (`url`, `headers`, `body`, `decoder`, `classify`).
  - `wire/convert.rs` holds the tool-pair backstop and the tools and image helpers.
  - `wire/usage.rs` holds the stop and usage maps. Each wire keeps its current `total_tokens` formula (Decision 4).
  - `anthropic_native` builds on the shared body in `anthropic`, and `clamp_budget` moves into that shared part.
- Rename `native.rs` → `providers.rs`, and `Route::is_native` → `is_direct`. "Native" then means only
  `anthropic_native`'s request flavour.
- Key validation → `auth/check.rs`, used by `mu auth test` and the TUI key flow. `discovery.rs` uses the shared
  client.

Done when: no wire module defines its own backstop or SSE feed loop, and the golden, completions, responses and
`omp_session.jsonl` replay tests pass unchanged.

### Phase 7: plugin host (`mu-tools`)
- Split `host.rs` / `process.rs` / `mcp_runtime.rs`. `Slot` → `enum { Native { manifest }, Mcp(McpSlot) }`, and
  `Proc` gets a `Dialect` so MCP-only fields leave the native path. No more fabricated manifests.
- One `stop_slot` helper replaces the 9 copies. Plugin, task and bash spawn through `mu-base`'s `spawn_grouped`.
- The MCP data modules read JSON through one reader. `store.rs` writes through `mu_common::fs::write_atomic`, with
  its mode preservation moved there.

Done when: `grep -c 'slot.mcp'` over the plugin host is 0, and the fake-plugin tests pass unchanged.

### Phase 8: tools, shell, jobs (`mu-tools`)
- `tools/shell/`:
  - `bash_pty` becomes a `PtyRun` struct with a `term()` method (one SIGTERM path);
  - `finish` takes a `RunEnd` struct;
  - there is one exit-code mapping, and bash is looked up in one place (`pty::login_shell`).
- `tools/spill.rs`: `Collector` and `cap_text` share their spill code.
- `Tools` splits its services from its session state (`ToolSession`: todo, session ids, skills).
- `jobs`: one `job_mut(seq)`, one spawn path, and `Spec` embedded in `Job`.
- `tools/subagent.rs` parses the child protocol once, into a `serde(tag = "type")` `ChildEvent`.

Done when: no function in `tools/` is over ~100 lines apart from `read` and `todo::apply` (low priority), and the
bash and jobs tests pass unchanged.

### Phase 9: finish
- `context::system_prompt`'s literal → `prompts/system.md`.
- `DESIGN.md` gains a short "Crates and layers" section with the Target crates table as it actually is.

Done when: DESIGN.md matches `cargo tree`.

## Risks
- **Merge conflicts.** Phases 1 and 2 touch every import. `src/` is frozen while they run (Decision 5). Keep them
  short, and rebase any branch that waited right after them.
- **Order-sensitive globals.** The `session` run-flag statics must be set before a claim (`main.rs:493-540`), and the
  `auth_store` `CACHE`/`CMD_CACHE` and `secrets` statics are process-wide. Move them intact. One binary still links
  each of them once.
- **Test isolation changes.** Each crate gets its own test binary, so `mu-testutil::home()` sets `MU_HOME` per crate
  instead of once. Tests that relied on sharing state across modules surface here.
- **Lost dead-code warnings.** `pub` items in a library crate are never reported as unused. Keep `pub` to what
  another crate uses. After Phase 2, check for orphans with `cargo +nightly udeps` or a search.
- **Codegen.** Without LTO, small non-generic functions no longer inline across crate boundaries. Phase 2's binary and
  RSS check is the guard.
- **Thin coverage.** Nothing tests `main.rs::run()`, the agent review path or `sibling_or_path`. Phases 0 and 4 add
  tests before they change anything.
- **macOS.** Process-group and `pre_exec` code moves into `mu-base`. Keep the `#[cfg(target_os)]` branches as they
  are. The release build runs the tests on darwin-arm64.
- **Hiding instead of fixing.** Splitting `impl Agent`/`impl App` across files does not reduce the field counts.
  Phases 3 and 5 are done only when the field-count checks pass, not when the files are split.

## Out of scope (follow-ups)
- Deduplications that change behaviour: normalizing `total_tokens` across wires, plugin-manifest labels for
  `summary()`, one `Overlay` event model and an overlay/picker trait, a single markdown walker, and shared front-end
  event handling.
- Splitting `read.rs` selector parsing and `todo::apply` per op.
- Splitting `mu-base` further.

## Implementation notes

Deviations from the plan, and what to know:
- `ask_nudge` lives in `mu-base`, not `mu-agent`: `message::history_events` uses `is_reminder`.
- The provider table (`PROVIDERS`, `native_def`) moved to `mu-provider`'s `provider/providers.rs` in Phase 6 (see the commit for the check on `mu-base`).
- `mu-tui` does not depend on `mu-agent` (nothing in it needs it). `notify::send` and the session-control functions the jobs panel calls moved down into `mu-session` (`inbox`, `control`), so the TUI does not import binary modules.
- `an_omp_session_loads_and_replays_only_to_its_own_wire` stays in `mu-provider`, with `mu-session` as a dev-dependency only.
- `mu-testutil::home()` honours `MU_TEST_HOME` so `tests/modes.rs` can spawn the real binary against a fake API.
- Phase 5: jobs-panel polling is in `App::on_tick` (the run-loop tick), not `on_ui`. `env_is_set` is not deduplicated: the key flow (`var`, blanks count as unset) and the MCP wizard (`var_os`, only empty counts as unset) differ, and merging them would change the wizard's GitHub preset, so each keeps its own check.
- Phase 0 tests: the sibling-binary test exercises `sibling_of` (the extracted lookup); the other two fail on `main` and pass now.
- Phase 1 restored the auth-store redaction refresh through `secrets::set_stored_refresh` (registered in `run()`), since dropping it changed behaviour.
- Release binary after Phase 2: 66.94 MB vs 64.21 MB (+4.3%); idle Pss 19.6 MB vs 18.7 MB (+4.8%), Pss_Anon 3.7 vs 3.6 MB.
- Review fixes (after the first review):
  - `Command::Bash` and `Command::Compact` run under `drive`, so `Cancel` (Esc, quit, SIGUSR1) aborts them mid-run again. A follow-up that `turn_from` starts is published, so it leaves the queue bar. `/review` returns input typed during the review at its `TurnEnd`, before the fix turn.
  - `review::facts` is two steps: `Worktree::snapshot` + `Worktree::gather(base)` collect the git facts, and `Facts::request(plan, notes, focus)` builds the request. Each front end resolves the plan between the two, in `main`'s order: no plan check or plan notice when there is nothing to review, and the bare "review needs a git repository" error.
  - `test-support` only adds constructors and inert seams, so the `target/debug/mu` that `cargo test` builds behaves like a normal build: `Providers::new` always opens the real key store (tests use `Providers::for_tests`), and `__panic` panics only when `Tools::panic_tool` is set by `mu_agent::testing`. `Agent.review_child` is `cfg(test)`, `Agent::providers()` is test-support only.
  - `WireFormat` has no `classify`: nothing called it, and each decoder maps its own in-stream error frames.
  - The direct Anthropic headers use `native::betas(req)`, so the body is built once per attempt.
  - Key validation builds its own client again (15 s overall, no separate connect timeout), as on `main`.
  - `AuthStyle` lives in `mu-provider`'s `providers.rs`.
  - `mu-session` is `session/` with `file`, `format`, `list`, `owner` (with `RunKind`), `tail`, `inbox` and `control`. `profile_flag` sits in `list` (it builds resume hints). `LiveSession.kind` stays a string: it is read from records other mu versions may have written.
  - Visibility: modules and items that no other crate uses are `pub(crate)` in every library crate (`mu-agent`'s `policy`, `mu-provider`'s `wire`, `sse`, `retry`, `complete`, `gateway_models`, `catalog`, `discovery`, `mu-tools`' `walk`, `shell`, `grep`, `mcp::{auth, convert}`, and the individual helpers inside the modules that stay `pub`). The compiler reported no new dead code afterwards. `mu-tui` already had its modules narrowed.
