# Providers

mu reaches a model one of three ways:

- **Natively**, for Anthropic, OpenAI and Baseten, with an API key you add in mu. Requests go straight to the provider's API. No omp process runs.
- **Natively, on your ChatGPT plan**, for `openai-chatgpt/*` models after [Sign in with ChatGPT](#sign-in-with-chatgpt). Requests go straight to `api.openai.com`. No omp process runs.
- **Through the omp gateway** for everything else: subscription logins (Claude Pro/Max, Codex), every other omp provider, and any of the three providers you have not given a key to. The gateway holds those credentials; log in with omp (`/login` lists omp's providers too).

Model ids are omp's (`anthropic/claude-sonnet-5-5`, `openai/gpt-5.5`, `baseten/moonshotai/Kimi-K3`), so the roles you copy from omp (`mu models import`) work in both tools. Plan models are `openai-chatgpt/<model>` (`openai-chatgpt/gpt-6.1-sol`): the same models as `openai/<model>`, billed to your ChatGPT plan instead of a key.

## How routing is decided

A provider is native **only when mu's key store has an entry for it and the entry says its chat models use the key**. A `ANTHROPIC_API_KEY` in your environment does not switch anything: that would silently move `anthropic/*` from a Claude subscription to per-token billing. To use the variable, add it explicitly (`mu auth add anthropic --chat --env ANTHROPIC_API_KEY`, or pick it in `/login`).

For Anthropic and OpenAI the key can serve chat models, or only web search ([web.md](web.md)), and you say which: `/login` asks "Use this key for chat models?", `mu auth add` needs `--chat` or `--no-chat`, and `mu auth use <provider> --chat|--no-chat` changes it later. A key saved with `--no-chat` leaves `anthropic/*` on omp (your subscription). Baseten's key is only for chat. Keys for Parallel and Exa are only for web search.

Add a key for chat and the provider's models go native at once, in running sessions too; remove it (`/logout`, `mu auth rm`) and they go back through omp. The status line shows `· key` after a natively routed model, and cost is shown instead of subscription usage, since a key is metered.

A ChatGPT sign-in routes `openai-chatgpt/*` natively on the same terms, once the account's grant includes plan usage. Nothing picks between `openai/*` and `openai-chatgpt/*` for you: the model you choose decides the credential, and a plan model never falls back to your key or the reverse. The status line shows `· ChatGPT plan` for a plan model.

## Sign in with ChatGPT

OpenAI lets open-source tools run requests on a ChatGPT Plus or Pro plan. mu signs you in through the browser and sends requests to `api.openai.com` with the resulting access token. This is not the Codex CLI's login, and omp is not involved. OpenAI offers plan usage to open-source and local personal tools such as mu; a sign-in whose grant lacks plan usage (`chatgpt.tokens.use.direct`) is kept, but cannot run models until you approve that permission by signing in again.

Sign in from `/login` (the first row, "Continue with ChatGPT") or with `mu auth login openai-chatgpt`. mu opens the sign-in page in your browser; the browser returns to `127.0.0.1:1455`. Over SSH, or wherever the browser cannot reach that address, the browser ends on a page that does not load. Copy its address and paste it into mu (or the terminal running `mu auth login`), then press Enter. Esc or Ctrl-C cancels, and nothing is saved.

Plan models come from your account's own list and appear in `/model` marked `ChatGPT plan`. `openai/<model>` (your API key) and `openai-chatgpt/<model>` (your plan) are separate choices; mu has no preference between them.

Plan usage counts against your plan's limits. At a limit, mu stops with OpenAI's message: open https://chatgpt.com/settings/usage (Manage usage) to see or change usage, or ask your workspace admin for a workspace plan. The first time you sign in, mu says "You're using your ChatGPT plan" once.

Sign out with `/logout` (or `mu auth logout openai-chatgpt`). mu revokes the refresh token at OpenAI. If OpenAI does not confirm the revocation, mu says so; disconnect mu under your ChatGPT settings, apps, then. The client registration is kept, so a later sign-in reuses it. `mu auth rm openai-chatgpt` does the same.

To sign in as another ChatGPT account, forget the registration as well: pick "Use a different ChatGPT account" in `/login`, or run `mu auth logout openai-chatgpt --forget`. mu revokes the sign-in, then starts the browser sign-in, which registers a new client and may use a different account. The old app stays listed under your ChatGPT settings, apps, until you disconnect it there.

The sign-in is one account at a time. mu checks the ID token against OpenAI's published keys, and refuses a sign-in as a different ChatGPT identity than the one its registration is for; forget the registration (above) to change account. Tokens are stored in the `chatgpt` section of `auth.json` (see [The store](#the-store)) and redacted like keys ([Secrets](secrets.md)).

The route is HTTP streaming only (no WebSocket). It leaves out request fields the route does not accept (such as `temperature`, `top_p`, `max_output_tokens` and `previous_response_id`) and does not send the image generation, file search, code interpreter, computer use, hosted MCP or tool search tools.

## Adding a key

In the TUI, `/login` lists the providers and the search services under "API keys" (`key saved · chat`, `key saved · not for chat`, `$ANTHROPIC_API_KEY is set`, `not set`), then omp's OAuth providers. `/login anthropic` goes straight to the key flow: paste a key (masked, nothing is echoed), use the environment variable, or give a command. mu asks the provider whether the key works before saving (`401`/`403` rejects it; if the provider cannot be reached you can save anyway; Parallel and Exa have no free way to test a key, so theirs are saved unchecked). With a key already stored for Anthropic or OpenAI, `/login anthropic` offers to change its chat use or replace it. `/logout` removes a stored key.

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

Stored keys are redacted from everything mu stores or sends (see [Secrets](secrets.md)): a model that runs `cat ~/.mu/auth.json` sees `[REDACTED:ANTHROPIC_API_KEY]`. The ChatGPT sign-in tokens are redacted the same way, as `OPENAI_CHATGPT_TOKEN`, `OPENAI_CHATGPT_REFRESH_TOKEN` and `OPENAI_CHATGPT_ID_TOKEN`.

## Thinking per provider

mu's levels (`off`, `low`, `medium`, `high`, `xhigh`, `max`) map onto each model's own ladder, taking the nearest effort it serves (a tie goes lower).

| Model kind | Request |
|---|---|
| Anthropic adaptive (Opus 4.6+, Sonnet 4.6+, the 5-series) | `thinking: {type: adaptive}` plus `output_config.effort`; `display: summarized` where the model accepts it, so thinking text streams. Off: `between_tools` where that is the model's lowest setting (Sonnet 5.5), else no `thinking` and effort `low` (these models reject `disabled`) |
| Anthropic budget (Haiku 4.5, Sonnet 4.5, Opus 4.5) | `thinking: {type: enabled, budget_tokens}` with mu's budgets (4k, 8k, 16k, 32k, 64k), shrunk to leave 1024 tokens for the answer; Opus 4.5 also sends the effort |
| OpenAI (Responses) | `reasoning: {effort, summary: auto}`; reasoning items are kept across turns (`store: false`, encrypted content) |
| Baseten (Chat Completions) | `reasoning_effort`; off follows the model's disable mode (`omit`, lowest effort, or `none`); models that need it get `reasoning_content` replayed |

Anthropic requests also carry 5-minute prompt-cache breakpoints on the last tool, the system prompt and the last message, and keep earlier thinking (`clear_thinking keep: all`). No `Authorization` header goes with an `x-api-key`.

## Models and prices

Context windows, output limits, prices, efforts and Chat Completions quirks come from a snapshot of omp's catalog (`crates/mu-provider/src/model/catalog.json`), regenerated on every omp bump by `scripts/maint/gen-catalog.sh`. Baseten also lists its models live (`GET /models`, at most daily, an hour after a failed attempt, cached in `native-models.json`) so new ids appear in `/model` with the provider's own limits and prices; listed prices win over the snapshot's. A discovered id without a snapshot row runs with reasoning off, because a new family's reasoning shape is not guessed. `models.toml` limits win over both.

`/model` marks native models `key` and plan models `ChatGPT plan`. mu never picks a model for you: with no `--model` and no `default` role (set with `/model` or in `models.toml`; `mu models import` copies omp's), `-p`, `--headless` and ACP runs stop with an error that says so, and the TUI starts with no model (the status line shows `no model · /model`), refuses prompts, and waits for you to `/login` and choose one with `/model`. A chosen model stays on its credential until you pick another; nothing falls back from one to another. A gateway model whose provider is native is routed natively and is marked `key` too.

## Sessions

Messages store the same `api` strings as omp (`anthropic-messages`, `openai-responses`, `openai-completions`), and reasoning is stored the way omp stores it (a Responses reasoning item in `thinkingSignature`; the field name `reasoning_content` for Chat Completions), so a session can continue in either tool. Reasoning is only replayed natively to where it came from: Anthropic thinking to the same API and provider; OpenAI and Baseten reasoning to the same model. Anthropic and OpenAI drop other models' thinking; Baseten models get it as text in a `<think>` block, as omp does.

## What still needs omp

- Subscription logins through omp (Claude Pro/Max, Codex) and every provider other than Anthropic, OpenAI, Baseten and ChatGPT (the gateway). Sign in with ChatGPT itself needs no omp.
- OAuth for MCP servers. Web search and fetch no longer use omp ([web.md](web.md)).
- Model roles and limits are mu's own `models.toml`; omp's `models.yml` only matters to the gateway.

## Not in v1

- Custom OpenAI- or Anthropic-compatible providers (a table row plus config later), key pools and rotation, an OS keychain.
- Anthropic 1-hour cache, thinking prefix binding, server compaction.
- OpenAI stateful chaining (`previous_response_id`), freeform tools, `text.verbosity`.
- Baseten model-family quirks omp handles: DeepSeek tool-markup healing and thinking-loop guards. Only snapshot rows have reasoning on.
- Keys sit in process memory unzeroed, as in omp.
- For the ChatGPT route: WebSocket transport, device-code sign-in, and more than one account in use at a time.

## Debugging

`MU_DUMP_REQUESTS=/tmp/req.jsonl` appends the shape of each request (model, token limit, thinking and effort, counts; never content). Native requests add `api` and `reasoning`.
