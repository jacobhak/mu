# Native API-key providers: Anthropic, OpenAI, Baseten

## Context

Every model request today is an Anthropic Messages request to omp's auth gateway (`src/provider/mod.rs` `attempt()`), and the gateway owns every credential. That has three costs:

- the gateway pair (`omp auth-broker serve` + `omp auth-gateway serve`) is about 210 MB private while it runs (`DESIGN.md` table), which dwarfs mu's 3 MB;
- a working mu needs omp for login, models, context windows (`/v1/models`) and pricing (`omp models ls --json`);
- a plain API key can't be used without omp's login, which for Anthropic and OpenAI has no API-key flow at all.

Goal: mu talks directly to Anthropic, OpenAI and Baseten with an API key you add in mu. The gateway stays for everything else (OAuth subscriptions, other providers). A session that only uses native providers starts no omp process.

What omp does, for reference (checkout at `~/.mu/maint/oh-my-pi`):

- Keys are plaintext JSON rows in SQLite `~/.omp/agent/agent.db` (0600), resolved runtime → `models.yml apiKey` → OAuth → login key → env → stored key (`packages/ai/src/auth/cascade.ts`).
- `/login` only has an API-key flow for providers with a `login "api-key"` rule (Baseten yes; Anthropic and OpenAI no). It echoes the pasted key.
- Anthropic API key: `x-api-key`, `anthropic-version: 2023-06-01`, a few betas, no Claude Code fingerprint (`providers/anthropic.ts`).
- OpenAI: Responses API, `store:false`, `include:["reasoning.encrypted_content"]`, reasoning items replayed verbatim (`providers/openai-shared.ts`).
- Baseten: Chat Completions at `https://inference.baseten.co/v1`, `reasoning_effort`, `reasoning_content` replay for Kimi/DeepSeek, `/v1/models` discovery with prices.
- `packages/catalog/src/models.json` holds resolved rows per provider/model: `api`, `contextWindow`, `maxTokens`, `cost`, `thinking {mode, efforts}` and, for Chat Completions rows, the resolved `compat` flags. For these three providers the useful fields come to about 33 KB.

## Decisions (agreed)

1. **Routing is explicit.** A provider is native only when mu's key store has an entry for it. An env var alone never switches routing, because that would silently move `anthropic/*` from a Claude subscription to per-token billing. Everything else goes to the gateway as today. Model ids stay omp's (`anthropic/claude-sonnet-5-5`, `openai/gpt-5.5`, `baseten/moonshotai/Kimi-K3`), so model roles in omp's `config.yml` keep working in both tools.
2. **Key store:** a mu-owned JSON file with 0600 permissions, one per omp profile (`paths::profile_cache_dir()/auth.json`). Each entry holds either a literal key or a reference (`$ENV_NAME` or `!command`). No keychain.
3. **TLS in the core:** `reqwest` with rustls (ring), as `mu-mcp-http` already does. The "no rustls in the core" guard goes.
4. **Wire formats:** OpenAI uses the Responses API; Baseten uses Chat Completions. The Anthropic Messages code is shared between the gateway and native routes.
5. **Model metadata and pricing** come from a curated snapshot of omp's catalog at the pinned version, regenerated on every bump. Baseten adds live `/v1/models` discovery. Native providers need no omp.
6. **UI:** `/login` lists the three API-key providers next to omp's OAuth providers. Picking one opens a masked input, checks the key and saves it. `/logout` removes it. `mu auth add|list|rm|test` covers headless and scripted use.
7. **No custom providers in v1.** Baseten is built as a preset of a generic OpenAI-compatible client, so a custom provider later is a table row plus config.
8. **Anthropic prompt caching** follows omp: 5-minute ephemeral `cache_control` on the last system block and the last message, at most 4 breakpoints, never on thinking blocks.

## Architecture

```
Providers::stream(req)
  └─ route(model.provider)
       ├─ Native(def, key)  ── https ──> api.anthropic.com / api.openai.com / inference.baseten.co
       └─ Gateway           ── http loopback ──> omp auth-gateway (unchanged)
  └─ wire(model.api): AnthropicMessages | OpenAiResponses | OpenAiCompletions
       body() + headers() + Assembler  →  StreamEvent / Message::Assistant
```

- **`ProviderDef`** (static table in `src/provider/native.rs`): `id`, `label`, `api`, `base_url`, `auth` (`XApiKey` or `Bearer`), `validate` (URL of a cheap authenticated GET), `key_url` (where to create a key), `env_hint` (`ANTHROPIC_API_KEY`, …), `default_model`. Three rows in v1.
- **`Api`** enum replaces the `models::API` constant. Its strings are omp's (`anthropic-messages`, `openai-responses`, `openai-completions`), so sessions stay readable by omp in both directions. `same_origin` compares the message's `api` and `provider` with the model's.
- **`ModelSpec`** gains `api`, `wire_id` (the id without the `provider/` prefix, for native requests; the gateway keeps the full id), `thinking: ThinkingSpec { mode, efforts }`, `images: bool` and `compat: Option<CompletionsCompat>`.
- **Replay conventions follow omp,** so an omp session can continue natively and the other way round:
  - Responses reasoning: a `Thinking` block whose `thinking` is the summary text and whose `thinking_signature` is the reasoning item JSON. Tool call id is `call_id|item_id`.
  - Chat Completions reasoning: a `Thinking` block whose `thinking_signature` is the field name (`reasoning_content`).
- **Thinking mapping** (`models.rs`, one `clamp_effort(level, efforts)` helper): mu's `off…max` maps to the nearest effort in the model's ladder.
  - `anthropic-adaptive`: `thinking:{type:"adaptive"}` + `output_config.effort`. When thinking is off on an adaptive-only model, omit `thinking` and pin effort `low`, as omp does.
  - Budget models keep today's budgets.
  - Responses: `reasoning.effort`.
  - Completions: `reasoning_effort`, disabled per the row's `reasoningDisableMode`: `omit`, `lowest-effort` or `none-effort`. Rows with any other mode are treated as non-reasoning in v1.

## Phases

Each phase leaves `main` releasable: fmt, clippy `-D warnings` and tests pass, and with no native key mu behaves exactly as today. Live checks run in the test profile (`OMP_PROFILE=mutest`), never the production profile.

### Phase 1: foundation (no behaviour change)

1. **TLS in the core.**
   - `Cargo.toml`: reqwest features `["stream", "rustls-no-provider"]`, plus `rustls = { default-features = false, features = ["ring", "std", "tls12"] }` (same as `plugins/mcp-http`).
   - Install `rustls::crypto::ring::default_provider()` first thing in `main`, and in `#[cfg(test)]` helpers that build clients.
   - Delete the `check` arm of `scripts/build-mcp-http.sh` and its CI and release steps. Update the comment there and `docs/mcp.md` (mcp-http stays a separate project; folding it back is out of scope).
   - Measure with `scripts/measure-idle.sh target/release/mu` before and after, and record both numbers in `BENCHMARKS.md`.
2. **Route and wire seam** in `src/provider/mod.rs`.
   - Split `attempt()` into `route()` (an endpoint, auth header and whether it is native) and `wire()` (`body`, `url`, `headers`, assembler) dispatched on `ModelSpec.api`.
   - Only the gateway route and the Anthropic wire exist after this phase.
   - The `gateway_models()` pre-fetch in `stream()` runs only for gateway-routed models.
3. **`Api` enum and the new `ModelSpec` fields.** Replace `models::API` and update `same_origin` and both `Assembler::new` call sites. Existing session files must still load: check a test against an omp-written fixture.
4. **Catalog snapshot.**
   - `scripts/maint/gen-catalog.sh <ver>`: `git -C "$OMPSRC" show "v$ver:packages/catalog/src/models.json" | jq …` keeps rows for `anthropic`, `openai` and `baseten` whose `api` is one of the three supported. Fields kept: `id, name, api, contextWindow, maxTokens, cost, reasoning, input, thinking{mode,efforts,defaultLevel}`, plus for Completions rows the `compat` subset `maxTokensField, supportsDeveloperRole, supportsMultipleSystemMessages, supportsUsageInStreaming, reasoningContentField, requiresReasoningContentForToolCalls, requiresReasoningContentForAllAssistantTurns, reasoningDisableMode, thinkingFormat, stripImageInput`. Output goes to `src/provider/catalog.json`, committed.
   - `scripts/maint/bump.sh` runs it. `check.sh` fails when the committed file differs from a fresh run, so the maint PR shows catalog changes.
   - `src/provider/catalog.rs`: `include_str!`, parsed once on first native lookup (load on use). `models::resolve` uses catalog rows for native providers, before `models.yml` overrides and after explicit `models.yml` entries (same precedence as gateway rows today).
   - Test: the generator's jq filter against a small fixture, and `resolve` precedence with a catalog row.

**Acceptance:** all existing tests pass; `cargo tree -p mu | grep rustls` is non-empty; a gateway session (smoke) is byte-identical in request shape (`MU_DUMP_REQUESTS`); idle-memory delta recorded.

### Phase 2: key store, redaction, `mu auth`

1. **`src/auth_store.rs`**
   - File `profile_cache_dir()/auth.json`:
     ```json
     {"version":1,"providers":{
       "anthropic":{"key":"sk-ant-…","added":"2026-11-04T10:00:00Z"},
       "openai":{"ref":"!op read op://dev/openai/key"},
       "baseten":{"ref":"$BASETEN_API_KEY","baseUrl":"https://…"}}}
     ```
     One entry per provider (no key pools). `baseUrl` is an optional override for proxies and tests, checked with `gateway::validate_url` (https, or http to loopback only).
   - **Writes:** new `mu_common::fs::write_private`: a temp file created with mode 0600 (`OpenOptionsExt::mode`), fsync, then rename. Directory 0700. Writers hold an `flock` on `auth.json.lock`. This is portable to macOS; no `/proc`.
   - **Reads** happen on use and are re-read when the mtime changes, so a key added in one session reaches running sessions and new subagents without a restart.
   - **References:**
     - `$NAME` reads the environment and errors if unset. Unlike omp and MCP's `resolve_value`, a missing variable never falls back to sending the literal text.
     - `!cmd` runs through `sh -c` with a 10 s cap; reuse the runner from `src/plugin/mcp/auth.rs`, moved to a shared module. The result is cached in the process, and the command runs again once after a 401.
   - A world- or group-readable `auth.json` gets a warning at load, like ssh does.
2. **Redaction.**
   - `secrets.rs` gains `Source::Stored` and a mutable registry (`Mutex<Vec<Known>>`) next to the `ENV` `OnceLock`.
   - Every resolved key value is registered under the provider's `env_hint` label (`[REDACTED:BASETEN_API_KEY]`), because Baseten keys match no known key format.
   - Provider error bodies pass through `redact` before they are shown or stored.
3. **CLI** (`src/main.rs` subcommand, no TUI):
   - `mu auth list`: provider, source (`stored`, `$NAME`, `!cmd`), last 4 characters only.
   - `mu auth add <provider> [--env NAME | --cmd '…'] [--base-url URL]`: with neither flag, the key is read from stdin when not a tty, else from a no-echo prompt (termios `ECHO` off; works on Linux and macOS). The key is validated (see Phase 4) before saving; `--no-validate` skips that.
   - `mu auth rm <provider>` and `mu auth test <provider>`.
4. **Tests:**
   - file mode is 0600 after create and after rewrite;
   - a concurrent writer under the lock;
   - a reference to an unset variable is an error, not a literal;
   - the `!cmd` timeout;
   - a stored key is redacted from a tool result;
   - `auth list` never prints more than 4 characters.

**Acceptance:** `mu auth add anthropic < key.txt` writes a 0600 file; `mu auth list` shows it masked; `cat ~/.mu/.../auth.json` run by the model in a session comes back redacted.

### Phase 3: native Anthropic

1. **Route:** for a model whose provider has a store entry, send to `def.base_url` with `x-api-key` and `anthropic-version: 2023-06-01`. No `Authorization` header and no `?beta=true`. The model is `wire_id`.
2. **Body** (`src/provider/anthropic.rs`, native branch only; the gateway body is unchanged):
   - Thinking by `ThinkingSpec.mode`, per Architecture.
   - Betas: `effort-2025-11-24` when `output_config.effort` is sent. With thinking on, `context-management-2025-06-27` plus `context_management:{edits:[{type:"clear_thinking_20251015",keep:"all"}]}`, so replayed thinking is kept. `fine-grained-tool-streaming-2025-05-14` when tools are present.
   - `cache_control:{type:"ephemeral"}` on the last system block and on the last cacheable block of the last message. Never on `thinking`/`redacted_thinking`. At most 4 breakpoints.
   - Sampling parameters are never sent (mu sends none today).
3. **Assembler:** reused unchanged. The gateway already speaks Anthropic SSE.
4. **Errors and retries** (`retry.rs`):
   - Honour `retry-after-ms` as well as `retry-after`, and `x-should-retry`.
   - `LOGIN_HINT` depends on the route. Native 401/403: "Anthropic rejected the API key; run /login to replace it". The gateway keeps the omp hint.
   - Connect errors name the host, not "gateway".
5. **Quota and pricing:**
   - Native models are metered, so `quota.rs` skips `/v1/usage` for them and the status line shows cost only.
   - `pricing.rs` prices native models from catalog `cost`; check whether `Table::from_json` already accepts the catalog row shape (both come from omp's catalog), else add a small adapter. Native models never trigger `omp models ls`.
6. **Tests:**
   - golden request bodies for adaptive, budget, off on an adaptive-only model, and the cache breakpoint count;
   - a local TCP fake (`tokio::net::TcpListener`, as in the gateway tests) asserting headers and that no `Authorization` header is sent;
   - `#[ignore] contract_native_anthropic` hitting the real API when `ANTHROPIC_API_KEY` is set (one short turn with a tool call and thinking replay).

**Acceptance:** in `mutest` with only a stored Anthropic key and no gateway plugin installed, a multi-turn session with tools and `:high` thinking works. `ps` shows no omp process. Usage shows `cacheRead > 0` from the second turn. Cost is shown.

### Phase 4: TUI: masked input, `/login`, `/logout`, `/model`

1. **Masked input.**
   - `Editor` gains `masked: bool`: renders one `•` per character, keeps the real buffer, and supports bracketed paste.
   - While masked, the composer turns off history (private mode already does), ghost text (no text reaches the predict plugin), `@`/slash completion, copy-to-clipboard and the external editor.
   - Esc cancels and clears the buffer.
2. **`/login` overlay** (`src/tui/app.rs` `login()`, `overlay.rs` `LoginTarget::ApiKey(id)`):
   - An "API keys" section with the three rows and their status: `key saved`, `$ANTHROPIC_API_KEY is set`, or `not set`. Below it, omp's OAuth providers as today (scraped from `omp login`; Baseten is dropped from omp's list since mu owns it), then MCP servers.
   - `/login anthropic|openai|baseten` jumps straight to the key flow.
3. **Key flow:**
   - A notice with `key_url`, then a choice: paste key / use `$ENV_HINT` (shown only if set) / use a command.
   - The choice leads to the masked input (or a plain input for a command), then validation:
     - Anthropic `GET /v1/models?limit=1` (`x-api-key`);
     - OpenAI `GET /v1/models`;
     - Baseten `GET /v1/models`;
     - all with a 15 s timeout. 401/403 rejects. A network error offers "save anyway".
   - On save, a notice states the routing and billing change: "anthropic/* models now use your API key (billed per token) instead of omp."
4. **`/logout`:** a picker of stored native entries, which removes the chosen one. omp logout stays in omp.
5. **`/model` picker** lists catalog (and Baseten-discovered) models of native providers, marked `key`. Gateway models are unchanged. The status line shows `· key` after a natively routed model.
6. **Default model** (`main.rs` `pick_model`): with no role and no model given, prefer `DEFAULT_MODEL` when the gateway is installed; otherwise use the first native provider's `default_model`.
7. **Tests:** masked rendering never contains the buffer text; Esc clears; predict receives nothing while masked; login overlay items for each status. tmux smoke in `mutest`: add a key through `/login`, pick a native model, send a prompt.

**Acceptance:** you can add, use and remove a key entirely in the TUI; the key never appears on screen, in `history.jsonl`, in the session file or in predict's corpus.

### Phase 5: OpenAI Responses wire (`src/provider/openai_responses.rs`)

1. **Request:** `POST {base}/responses`, `Authorization: Bearer`. Body:
   - `model`, `instructions` (system), `input`, `tools` (`{type:"function",name,description,parameters,strict:false}`), `stream:true`, `store:false`, `include:["reasoning.encrypted_content"]`;
   - `reasoning:{effort,summary:"auto"}` for reasoning models;
   - `max_output_tokens`;
   - `prompt_cache_key` = session id (new `Request.session_id` field).
2. **`to_wire`:**
   - User text becomes `input_text`; images become `input_image` data URLs.
   - Assistant messages become a `reasoning` item replayed verbatim from `thinking_signature` (same origin only), `message` with `output_text`, and `function_call {call_id, name, arguments}` (arguments as a JSON string).
   - Tool results become `function_call_output {call_id, output}`.
   - Each `function_call` must have its output (reuse `repair_tool_pairs`).
3. **Assembler** handles these events:
   - `response.created`, `response.output_item.added`/`done`;
   - `response.output_text.delta`, `response.reasoning_summary_text.delta`, `response.function_call_arguments.delta`/`done`;
   - terminal `response.completed`/`incomplete`;
   - `response.failed` and `error`.

   Mapping:
   - Usage: input = `input_tokens − cached_tokens`; `cache_read` = `cached_tokens`; reasoning tokens are counted in output.
   - Stop reasons: `completed` gives Stop, or ToolUse when there are calls; `incomplete` because of `max_output_tokens` gives Length.
   - In-stream errors: `rate_limit_exceeded` and `server_error` are Retryable; `context_length_exceeded` is Overflow.
4. **Not in v1:** stateful `previous_response_id`, freeform `apply_patch` custom tools, `text.verbosity`, 24 h cache retention.
5. **Tests:** golden bodies including reasoning replay, recorded SSE fixtures (`src/provider/testdata/responses/*.sse`, keys scrubbed), a cross-provider session (reasoning items are not sent to Anthropic and the other way round), and `#[ignore] contract_native_openai`.

**Acceptance:** a multi-turn tool session on `openai/gpt-5.5:high` keeps its reasoning across turns (no "reasoning item missing" errors), and an omp-written OpenAI session resumes natively.

### Phase 6: Chat Completions wire and the Baseten preset (`src/provider/openai_completions.rs`)

1. **Request:** `POST {base}/chat/completions`, Bearer. Body:
   - `model`, `messages`, `stream:true`, `stream_options:{include_usage:true}` (when `supportsUsageInStreaming`);
   - `tools`;
   - max tokens under `compat.maxTokensField`;
   - `reasoning_effort` per ladder and disable mode.
2. **`to_wire`:**
   - The system message uses the `system` role (`developer` only if `supportsDeveloperRole`), coalesced into one when `!supportsMultipleSystemMessages`.
   - User content becomes text and `image_url` data URLs, dropped with a text note when `stripImageInput` is set.
   - Assistant messages carry `content` and `tool_calls[{id,type:"function",function{name,arguments}}]`, plus `reasoning_content` (field from compat) when `requiresReasoningContentForToolCalls` / `…AllAssistantTurns` and same origin.
   - Tool results use the `tool` role.
3. **Assembler:**
   - `choices[0].delta.content`;
   - reasoning from the first non-empty of `reasoning_content`, `reasoning`, `reasoning_text`;
   - `tool_calls` assembled by `index`;
   - `finish_reason` `stop`/`length`/`tool_calls`;
   - the usage chunk (`prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens`);
   - `[DONE]`.
4. **Baseten discovery:**
   - With a key, `GET /v1/models` at most daily, cached in `profile_cache_dir()/native-models.json`.
   - Discovery supplies `context_length`, `max_completion_tokens`, `pricing.*` × 1e6 and `supported_features`.
   - A snapshot row wins for `thinking` and `compat`. Discovered ids without a snapshot row get reasoning off (omp's rule: never guess an unknown family's reasoning shape) and tools per `supported_features`.
5. **Tests:** golden bodies for Kimi (replay required), GLM and gpt-oss rows; SSE fixtures with interleaved reasoning and tool-call deltas; the discovery mapper against a recorded `/v1/models` body; `#[ignore] contract_native_baseten`.

**Acceptance:** multi-turn tool sessions work on one Kimi, one GLM and one DeepSeek model on Baseten, with `:high` and `:off`. `/model` lists newly discovered Baseten models.

### Phase 7: docs and measurements

- `README.md`: lines 5, 12, 15 and 31 (mu now has its own key login; omp is needed only for OAuth providers, web and MCP OAuth); install notes.
- `DESIGN.md`: principle 4 ("omp's own gateway for providers" becomes "…for OAuth and other providers; native clients for API keys"), the Provider client row, the gateway row, the Providers section, the TLS row, and the mcp-http row (its TLS-isolation reason is gone).
- New `docs/providers.md`:
  - how routing is decided;
  - `/login`, `/logout`, `mu auth`;
  - the store file and reference syntax;
  - per-provider thinking mapping;
  - what still needs omp.
- `docs/secrets.md` (stored keys are redacted), `docs/usage.md` (effort per provider), `docs/config.md` (unchanged unless a key gets added), `docs/README.md` index.
- `BENCHMARKS.md`: memory of an idle and an active native-Anthropic session next to the gateway figures. This is the headline number.

## Parallelism

Phase 1 goes first and in one piece, since it changes the shared seam. After Phase 2, Phases 3, 5 and 6 (one wire module each) and Phase 4 (TUI) can proceed in parallel worktrees. `src/provider/mod.rs` dispatch and `catalog.rs` are owned by Phase 1, and later phases only add match arms.

## Risks

- **Anthropic thinking parity:** today the gateway maps mu's budgets to adaptive thinking. Getting adaptive-only models wrong returns 400s. Mitigation: golden bodies taken from omp's `buildParams` cases, plus the live contract test.
- **Responses replay errors** (a reasoning item without its following item, or orphan outputs): fixture tests built from real transcripts.
- **Baseten family quirks** omp handles and v1 does not (DeepSeek DSML markup healing, thinking-loop guards): v1 only enables reasoning for snapshot rows, and the gap is listed in `docs/providers.md`.
- **Billing surprise:** guarded by explicit opt-in (Decision 1), the save notice and the `· key` status marker.
- **Keys in process memory** are not zeroised. This is accepted, matching omp.
- **Catalog drift:** regenerated on each bump; `check.sh` flags differences for review.

## Out of scope (follow-ups)

- Native OAuth (Claude Pro/Max, Codex); key pools and rotation; the OS keychain.
- Custom OpenAI- or Anthropic-compatible providers from config or `models.yml` `baseUrl`/`api`/`apiKey`.
- Anthropic 1 h cache; Responses stateful chaining and freeform tools.
- Live model discovery for Anthropic and OpenAI.
- Web search and fetch without omp (the web plugin still runs `omp search`/`omp read`).
- Moving `mu-mcp-http` back into the workspace.
