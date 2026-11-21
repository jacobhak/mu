# Providers

mu reaches a model one of three ways:

- **Natively**, for Anthropic, OpenAI and Baseten, with an API key you add in mu. Requests go straight to the provider's API. No gateway runs.
- **Natively, on your ChatGPT plan**, for `openai-chatgpt/*` models after [Sign in with ChatGPT](#sign-in-with-chatgpt). Requests go straight to `api.openai.com`. No gateway runs.
- **Through a gateway provider plugin** for everything else: subscription logins (Claude Pro/Max, Codex), every other provider omp supports, and any of the three providers you have not given a key to. The gateway holds those credentials. It is installed separately, for example [`ai-provider-gateway`](#gateway-plugins), which wraps omp's auth broker and gateway; log in with `ai-provider-gateway login`. mu does not run omp itself, and without a gateway plugin these models are unavailable.

How a gateway starts, is shared between mu processes and stops when idle is up to the plugin; see its README. mu starts it with the first request that needs it.

Model ids are omp's (`anthropic/claude-sonnet-5-5`, `openai/gpt-5.5`, `baseten/moonshotai/Kimi-K3`), so the roles you copy from omp (`mu models import`) work in both tools. Plan models are `openai-chatgpt/<model>` (`openai-chatgpt/gpt-6.1-sol`): the same models as `openai/<model>`, billed to your ChatGPT plan instead of a key.

## How routing is decided

A provider is native **only when mu's key store has an entry for it and the entry says its chat models use the key**. A `ANTHROPIC_API_KEY` in your environment does not switch anything: that would silently move `anthropic/*` from a Claude subscription to per-token billing. To use the variable, add it explicitly (`mu auth add anthropic --chat --env ANTHROPIC_API_KEY`, or pick it in `/login`).

For Anthropic and OpenAI the key can serve chat models, or only web search ([web.md](web.md)), and you say which: `/login` asks "Use this key for chat models?", `mu auth add` needs `--chat` or `--no-chat`, and `mu auth use <provider> --chat|--no-chat` changes it later. A key saved with `--no-chat` leaves `anthropic/*` on the gateway (your subscription). Baseten's key is only for chat. Keys for Parallel and Exa are only for web search.

Add a key for chat and the provider's models go native at once, in running sessions too; remove it (`/logout`, `mu auth rm`) and they go back through the gateway. The status line shows `· key` after a natively routed model, and cost is shown instead of subscription usage, since a key is metered.

A ChatGPT sign-in routes `openai-chatgpt/*` natively on the same terms, once the account's grant includes plan usage. Nothing picks between `openai/*` and `openai-chatgpt/*` for you: the model you choose decides the credential, and a plan model never falls back to your key or the reverse. The status line shows `· ChatGPT plan` for a plan model and tracks session cost at model token rates, as for gateway subscriptions.

## Sign in with ChatGPT

OpenAI lets open-source tools run requests on a ChatGPT Plus or Pro plan. mu signs you in through the browser and sends requests to `api.openai.com` with the resulting access token. This is not the Codex CLI's login, and no gateway is involved. OpenAI offers plan usage to open-source and local personal tools such as mu; a sign-in whose grant lacks plan usage (`chatgpt.tokens.use.direct`) is kept, but cannot run models until you approve that permission by signing in again.

Sign in from `/login` (the first row, "Continue with ChatGPT") or with `mu auth login openai-chatgpt`. mu opens the sign-in page in your browser; the browser returns to `127.0.0.1:1455`. Over SSH, or wherever the browser cannot reach that address, the browser ends on a page that does not load. Copy its address and paste it into mu (or the terminal running `mu auth login`), then press Enter. Esc cancels, and nothing is saved.

Plan models come from your account's own list and appear in `/switch` and `/model` marked `ChatGPT plan`. `openai/<model>` (your API key) and `openai-chatgpt/<model>` (your plan) are separate choices; mu has no preference between them.

Plan usage counts against your plan's limits. At a limit, mu stops with OpenAI's message: open https://chatgpt.com/settings/usage (Manage usage) to see or change usage, or ask your workspace admin for a workspace plan. The first time you sign in, mu says "You're using your ChatGPT plan" once.

Sign out with `/logout` (or `mu auth logout openai-chatgpt`). mu revokes the refresh token at OpenAI. If OpenAI does not confirm the revocation, mu says so; disconnect mu under your ChatGPT settings, apps, then. The client registration is kept, so a later sign-in reuses it. `mu auth rm openai-chatgpt` does the same.

To sign in as another ChatGPT account, forget the registration as well: pick "Use a different ChatGPT account" in `/login`, or run `mu auth logout openai-chatgpt --forget`. mu revokes the sign-in, then starts the browser sign-in, which registers a new client and may use a different account. The old app stays listed under your ChatGPT settings, apps, until you disconnect it there.

The sign-in is one account at a time. mu checks the ID token against OpenAI's published keys, and refuses a sign-in as a different ChatGPT identity than the one its registration is for; forget the registration (above) to change account. Tokens are stored in the `chatgpt` section of `auth.json` (see [The store](#the-store)) and redacted like keys ([Secrets](secrets.md)).

The route is HTTP streaming only (no WebSocket). It leaves out request fields the route does not accept (such as `temperature`, `top_p`, `max_output_tokens` and `previous_response_id`) and does not send the image generation, file search, code interpreter, computer use, hosted MCP or tool search tools.

## Adding a key

In the TUI, `/login` lists the providers and the search services under "API keys" (`key saved · chat`, `key saved · not for chat`, `$ANTHROPIC_API_KEY is set`, `not set`), then ChatGPT, custom endpoints and MCP servers that need a login. `/login anthropic` goes straight to the key flow: paste a key (masked, nothing is echoed), use the environment variable, or give a command. mu asks the provider whether the key works before saving (`401`/`403` rejects it; if the provider cannot be reached you can save anyway; Parallel and Exa have no free way to test a key, so theirs are saved unchecked). With a key already stored for Anthropic or OpenAI, `/login anthropic` offers to change its chat use or replace it. `/logout` removes a stored key.

From a shell, for scripts and headless setups:

```sh
mu auth add anthropic --chat < key.txt          # or a no-echo prompt on a terminal
mu auth add openai --no-chat --env OPENAI_API_KEY   # read at request time, never stored; web search only
mu auth add baseten --cmd 'op read op://dev/baseten/key'
mu auth add exa --env EXA_API_KEY               # web search only
mu auth add anthropic --chat --base-url https://proxy.example/anthropic   # a proxy; https, or http to loopback, no user:password
mu auth use openai --chat                       # let openai/* chat models use the stored key
mu auth list                                    # provider, source, last four characters, what the key is for
mu auth test anthropic
mu auth rm anthropic
mu auth login openai-chatgpt                 # sign in with ChatGPT; over SSH, paste the redirect URL
mu auth logout openai-chatgpt                # sign out and revoke
mu auth logout openai-chatgpt --forget       # also forget the registration, to sign in as another account
mu auth test openai-chatgpt                  # does the plan answer?
```

`--no-validate` saves without asking the provider. `--env` and `--cmd` are exclusive.

## Custom endpoints (vLLM, Modal, any OpenAI-compatible server)

Any server that speaks Chat Completions (`POST <base>/chat/completions`, `GET <base>/models`) can be a named provider, with no gateway involved. Set one up from `/login` ("+ Add a custom endpoint…", or `/login custom`), or from a shell:

```sh
mu providers add modal --base-url https://example--serve.modal.run/v1 --label Modal --env MODAL_API_KEY
mu providers add local --base-url http://127.0.0.1:8000/v1 --no-auth --model Qwen/Qwen3-32B --context-window 32768 --max-tokens 4096 \
  --input-cost 0.30 --output-cost 1.20   # dollars per million tokens, so spend shows as dollars
mu providers add vllm --base-url http://127.0.0.1:8001/v1 --no-auth --model Qwen/Qwen3-32B --reasoning template \
  --thinking-on 'chat_template_kwargs.enable_thinking = true' \
  --thinking-off 'chat_template_kwargs.enable_thinking = false'
mu providers list
mu providers models modal --refresh   # GET /models now, with the real error if it fails
mu providers rm modal                 # removes the definition and its stored key; roles are never touched
```

The base URL is used exactly as given (include `/v1` when the server's API lives there); `/models` and `/chat/completions` are appended. It must be `https`, or `http` to a loopback address, and carry no user name, password, query or fragment; redirects are never followed, so a key only reaches the host you named. Keys are never accepted as arguments: `mu providers add` reads one from stdin or a no-echo prompt, or takes `--env NAME` / `--cmd 'command'` (read at request time, as for `mu auth add`). `--no-auth` is for servers that need none. mu checks `GET /models` with the key before saving (`401`/`403` saves nothing; other failures need `--no-validate`). The `/login` flow is a panel of the same fields: Enter edits the one highlighted, its Models screen reads `GET /models` once (and says why when it cannot), and Save writes the definition and the key together. `←` / `⇧Tab` / Esc go back a screen, and Esc on the first writes nothing; a key that is refused is asked for again rather than saved. Its model screen also sets **how the model reasons**: no reasoning, an effort ladder (`minimal`…`max`, picked one level at a time) or request switches typed as `path = value`, e.g. `chat_template_kwargs.enable_thinking = true`; and **what its tokens cost**, a dollar rate per million tokens for input, output and cache read and write, when you know the server's prices. A model added there starts on the effort ladder `low`, `medium`, `high`, so `/thinking` and Shift+Tab work at once; only a level above `off` sends anything. The same panel reopens on an endpoint that already exists (`/login custom <id>`, or the `Edit custom endpoint: …` row of `/login`), with its models and reasoning as saved and its key shown by how it is taken; Save replaces the definition and writes a key only when you choose one. Neither flow picks a model or edits a role: use `/switch <id>/<model>` to try it and `/model` to save a role.

Definitions live in `providers.toml` in the [profile home](usage.md#profile-home), never keys. The file is read afresh on every use, so a change made by `mu providers`, another session or your editor applies without a restart. A file that does not parse or validate blocks model resolution (and `/switch`, `/model`) with the path and the key at fault, until it is fixed. Unknown keys are errors. The key itself is stored under the provider id in `auth.json` like any other (`mu auth add|test|rm <id>`, `/login <id>`, `/logout`), always for chat, and never with its own `baseUrl`.

```toml
[providers.modal]
label = "Modal"                     # optional; defaults to the id
base_url = "https://endpoint.example/v1"
auth = "bearer"                     # default; or "none"
discover = true                     # default: GET <base_url>/models fills the model lists

[providers.modal.models."Qwen/example"]   # optional: a manual row, limits and request settings
name = "Example"
context_window = 32768              # defaults: 32768 (or what /models reports)
max_tokens = 4096                   # default 4096, at most the context window
images = false
reasoning = "effort"                # "none", "effort" (with the efforts list) or "template"
efforts = ["low", "medium", "high"] # effort only, ascending: the ladder /thinking picks from

[providers.modal.models."Qwen/example".cost]          # optional: dollars per million tokens
input = 0.30                        # prompt tokens
output = 1.20                       # generated tokens
cache_read = 0.06                   # cached prompt tokens; 0 when the server does not bill them

[providers.modal.models."Qwen/example".compat]        # the same flags as the catalog's compat, camelCase
reasoningContentField = "reasoning_content"
requiresReasoningContentForToolCalls = true

[providers.modal.models."Qwen/example".extra_body]    # non-secret request fields, merged into every request
temperature = 0.6
[providers.modal.models."Qwen/example".extra_body_on.chat_template_kwargs]    # merged while thinking is on
enable_thinking = true
[providers.modal.models."Qwen/example".extra_body_off.chat_template_kwargs]   # and while it is off
enable_thinking = false
```

A selector is `<id>/<exact server model id>` (`modal/Qwen/example`); it works whether or not the model is listed or configured, with text and tools, no images and no reasoning unless you say so. A model the panel or `mu providers add` adds starts on `reasoning = "effort"` with `efforts = ["low", "medium", "high"]`, so `/thinking` and Shift+Tab have levels; a model written by hand without `reasoning` sends nothing. `effort` sends `reasoning_effort` from the ladder like Baseten (`--effort` picks the levels, default `low`, `medium`, `high`); `template` sends no effort and toggles the `extra_body_on`/`extra_body_off` maps (every level but `off` means on; `--thinking-on`/`--thinking-off` write one `path = value` switch each). Extra maps merge objects key by key and replace anything else; their top level may not hold `model`, `messages`, `tools`, `tool_choice`, `functions`, `function_call`, `stream`, `stream_options`, `max_tokens`, `max_completion_tokens` or `reasoning_effort`, and they are for non-secret values only. Limits: `models.toml` over your entry over what `/models` reported over the defaults. Prices are never discovered from an OpenAI-compatible server, so the `cost` table is the only price a custom model has: without it, usage is stored unpriced and the status line shows no dollars, as before. With it, spend is tracked and stored like any other model's, and rates added while a session runs price the turns it already spent. A custom model never falls through to the gateway: a missing key, an unresolvable `$NAME` or `!command`, a key not enabled for chat, or a removed definition is an error naming the provider. The status line shows `· key`, or `· no auth` for a keyless endpoint.

Discovery (`GET /models`, at most daily, an hour after a failure) is cached per provider id and base URL in `native-models.json`, so changing the URL never shows the old server's models. Session files record the provider and model ids but no URL or key: resuming one in another profile needs a definition with the same id there.

## The store

`auth.json` lives in the [profile home](usage.md#profile-home) (`~/.mu/auth.json`, or `~/.mu/profiles/<name>/auth.json` under an omp profile), mode 0600, in a 0700 directory. mu warns when it is readable by others, and at startup when it cannot be read or parsed.

```json
{"version":2,"providers":{
  "anthropic":{"key":"sk-ant-…","chat":true,"added":"2026-11-04T10:00:00Z"},
  "openai":{"ref":"!op read op://dev/openai/key","chat":false},
  "baseten":{"ref":"$BASETEN_API_KEY","chat":true,"baseUrl":"https://…"},
  "exa":{"ref":"$EXA_API_KEY"}},
 "web":{"backend":"exa","keyless":false}}
```

The ChatGPT sign-in is a `chatgpt` section beside `providers` and `web`. It is written when you sign in, and a file that has it is version 3, which older mu versions refuse. A file without it stays version 2. Inside the object it looks like this:

```json
"chatgpt":{"hostId":"urn:uuid:…","active":"oaiapp_…","welcomed":true,
  "accounts":[{"clientId":"oaiapp_…","subject":"…","email":"you@example.com",
  "tokens":{"idToken":"…","accessToken":"…","refreshToken":"…","refreshAt":1793000000,"scopes":["…"]},
  "added":"…"}]}
```

`hostId` is generated once and kept through sign-outs. Signing out clears `tokens` and keeps the account's `clientId`, so the next sign-in reuses the registration.

Version 2 added `chat` and `web`. A version-1 entry has no `chat` and routed chat models; mu writes `chat: true` for such entries on the next change, and an older mu refuses a version-2 file rather than rewriting it without them. One entry per provider or search service. An entry has a literal `key` or a `ref`:

- `$NAME` reads the environment variable. If it is unset, the request fails with that message; mu never falls back to sending the text `$NAME`.
- `!command` runs through `sh -c` in its own process group (10 s limit; on timeout the whole group is killed) and uses its output. The result is cached in the process; after a `401` mu runs the command once more, in case the key rotated.

The file is re-read when it changes, so a key added in one session reaches the others and new subagents. Writers lock `auth.json.lock` and replace the file atomically. A corrupt file is never overwritten. A `baseUrl` is checked when saved and again before every use, and redirects are never followed, so a key only goes to the host you configured. There is no keychain integration.

With [state encryption](remote.md#where-your-data-goes) on (an agent with a state key), `auth.json` is one sealed envelope instead of JSON, readable only while the agent is unlocked: until then every read of a stored key and every write fails with `locked: attach to unlock`, a chat to a provider mu speaks natively fails the same way instead of going to the gateway. A plaintext file from before is still read, and is sealed at the first unlock. The lock file stays plaintext. With state encryption off the file is as described above.

Stored keys are redacted from everything mu stores or sends (see [Secrets](secrets.md)): a model that runs `cat ~/.mu/auth.json` sees `[REDACTED:ANTHROPIC_API_KEY]`. The ChatGPT sign-in tokens are redacted the same way, as `OPENAI_CHATGPT_TOKEN`, `OPENAI_CHATGPT_REFRESH_TOKEN` and `OPENAI_CHATGPT_ID_TOKEN`.

## Thinking per provider

mu's levels (`off`, `low`, `medium`, `high`, `xhigh`, `max`) map onto each model's own ladder, taking the nearest effort it serves (a tie goes lower).

| Model kind | Request |
|---|---|
| Anthropic adaptive (Opus 4.6+, Sonnet 4.6+, the 5-series) | `thinking: {type: adaptive}` plus `output_config.effort`; `display: summarized` where the model accepts it, so thinking text streams. Off: `between_tools` where that is the model's lowest setting (Sonnet 5.5), else no `thinking` and effort `low` (these models reject `disabled`) |
| Anthropic budget (Haiku 4.5, Sonnet 4.5, Opus 4.5) | `thinking: {type: enabled, budget_tokens}` with mu's budgets (4k, 8k, 16k, 32k, 64k), shrunk to leave 1024 tokens for the answer; Opus 4.5 also sends the effort |
| OpenAI (Responses) | `reasoning: {effort, summary: auto}`; reasoning items are kept across turns (`store: false`, encrypted content) |
| Baseten (Chat Completions) | `reasoning_effort`; off follows the model's disable mode (`omit`, lowest effort, or `none`); models that need it get `reasoning_content` replayed |
| Custom endpoint, `reasoning = "effort"` | `reasoning_effort` from the model's `efforts` ladder (nearest level, ties lower); off sends nothing unless `compat.reasoningDisableMode` says otherwise |
| Custom endpoint, `reasoning = "template"` | no effort; `extra_body_on` is merged while thinking is above `off`, `extra_body_off` while it is off |

Anthropic requests also carry 5-minute prompt-cache breakpoints on the last tool, the system prompt and the last message, and keep earlier thinking (`clear_thinking keep: all`). No `Authorization` header goes with an `x-api-key`.

## Models and prices

Context windows, output limits, prices, efforts and Chat Completions quirks come from a snapshot of omp's catalog (`crates/mu-provider/src/model/catalog.json`), still generated from oh-my-pi's sources at every crates bump by `scripts/maint/gen-catalog.sh`. Baseten also lists its models live (`GET /models`, at most daily, an hour after a failed attempt, cached in `native-models.json`) so new ids appear in `/switch` and `/model` with the provider's own limits and prices; listed prices win over the snapshot's. A discovered id without a snapshot row runs with reasoning off, because a new family's reasoning shape is not guessed. `models.toml` limits win over both. A [custom endpoint](#custom-endpoints-vllm-modal-any-openai-compatible-server)'s model is in neither catalogue, so its `cost` rates in `providers.toml` are its price, and only that.

`/switch` and the model list of the `/model` editor mark native models `key` and plan models `ChatGPT plan`. mu never picks a model for you: with no `--model` and no `default` role (set with `/model`, or in `models.toml`; `mu models import` copies omp's), `-p`, `--headless` and ACP runs stop with an error that says so, and the TUI starts with no model (the status line shows `no model · /switch`), refuses prompts, and waits for you to `/login` and choose one with `/switch` (`/model` only edits the saved roles, so saving `@default` does not choose a model for the running session). A chosen model stays on its credential until you pick another; nothing falls back from one to another. A gateway model whose provider is native is routed natively and is marked `key` too.

## Sessions

Messages store the same `api` strings as omp (`anthropic-messages`, `openai-responses`, `openai-completions`), and reasoning is stored the way omp stores it (a Responses reasoning item in `thinkingSignature`; the field name `reasoning_content` for Chat Completions), so a session can continue in either tool. Reasoning is only replayed natively to where it came from: Anthropic thinking to the same API and provider; OpenAI and Baseten reasoning to the same model. Anthropic and OpenAI drop other models' thinking; Baseten models get it as text in a `<think>` block, as omp does.

## What still needs a gateway

- Subscription logins (Claude Pro/Max, Codex) and every provider other than Anthropic, OpenAI, Baseten and ChatGPT: they go through a [gateway plugin](#gateway-plugins). Sign in with ChatGPT itself needs no gateway.
- Nothing else. MCP OAuth is done by mu ([mcp.md](mcp.md)), and web search and fetch never needed omp ([web.md](web.md)).
- Model roles and limits are mu's own `models.toml`; omp's `models.yml` only matters to the gateway.

## Gateway plugins

A gateway is a native plugin whose manifest says `provider = "gateway"`. mu asks it for two things over the plugin's stdio JSON-RPC:

- `provider/endpoint {clientPid, start}` returns `{baseUrl, token}`, or `null` when `start` is false and nothing is running. mu calls it with `start: true` to use the gateway and with `start: false` to peek at quota without starting anything.
- `provider/prices {}` returns `{"models":[{provider,id,name?,reasoning?,pricingStatus?,cost:{input,output,cacheRead,cacheWrite,longContext?}}]}`, the price list mu caches per profile in `pricing.json`.

The base URL must serve `/v1/messages`, `/v1/models` and `/v1/usage`.

`ai-provider-gateway` is such a plugin: a separate tool that wraps omp's auth broker and gateway and also works without mu. Its `install.sh` writes the manifest as the `gateway` plugin under `~/.mu/plugins` unless you pass `--no-mu`. Without a plugin, `MU_GATEWAY_URL` plus `MU_GATEWAY_TOKEN` point mu at a running gateway directly; that has no price list, so gateway models outside the snapshot show no cost.

## Not in v1

- Custom Responses or Anthropic-compatible providers (custom endpoints speak Chat Completions only), key pools and rotation, an OS keychain.
- Anthropic 1-hour cache, thinking prefix binding, server compaction.
- OpenAI stateful chaining (`previous_response_id`), freeform tools, `text.verbosity`.
- Baseten model-family quirks omp handles: DeepSeek tool-markup healing and thinking-loop guards. Only snapshot rows have reasoning on.
- Keys sit in process memory unzeroed, as in omp.
- For the ChatGPT route: WebSocket transport, device-code sign-in, and more than one account in use at a time.

## Debugging

`MU_DUMP_REQUESTS=/tmp/req.jsonl` appends the shape of each request (model, token limit, thinking and effort, counts; never content). Native requests add `api` and `reasoning`.
