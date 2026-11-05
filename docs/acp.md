# Agent Client Protocol

`mu --acp` serves [ACP](https://agentclientprotocol.com) over stdio, so ACP clients such as [Paseo](https://paseo.sh) can drive it. In Paseo's `~/.paseo/config.json`:

```json
{"agents": {"providers": {"mu": {"extends": "acp", "label": "mu", "command": ["mu", "--acp"], "env": {"OMP_PROFILE": "private"}}}}}
```

Each session runs its own agent in the `cwd` it was opened with.

## Sessions

`session/new`, `load` (replays history), `resume`, `list`, `close`, `delete` (removes the session file; deleting one that is gone succeeds), `prompt` (text, images, embedded resources), `cancel` and `$/cancel_request` (a cancelled prompt answers `cancelled`). `set_config_option` and `set_mode` are rejected while a prompt runs.

A prompt that fails (a provider error, a rejected key) answers with a JSON-RPC error (`-32603`, the error text as message), after an `**Error:**` message chunk; clients show it as a failed turn. A cancelled turn answers `cancelled`, anything else `end_turn`.

## Options and modes

Config options: `model` (the roles you have set, each with what it is for ([Model roles](model-roles.md)), then the advertised `provider/model` ids; a role selector such as `@slow:high` is also accepted, resolved as `--model` is; an unconfigured role fails with an invalid-params error that names what the role is for and its fallback), `thinking`, and `mode`. The modes are `default` and `plan`: plan mode is read-only exploration that ends with a proposed plan. `session/set_mode` and the `mode` option are the same switch, and `current_mode_update` reports changes the agent makes itself (a plan being approved or parked).

## Slash commands

The session advertises its commands with `available_commands_update`, and a prompt that starts with one runs it: `/compact [focus]`, `/review [focus]`, `/plan [prompt]` (toggles plan mode; with a prompt, runs it in plan mode) and `/rename <title>`, plus the prompt templates from `.omp/commands` and friends and `/skill:<name> [args]`. Anything else that starts with `/` goes to the model as typed. The commands that need the terminal UI (`/model`, `/login`, `/resume`, `/mcp`, ...) are not offered; use the options and the client's own pickers.

## Tool calls

`tool_call` carries the tool's `name`, a `kind` (`read`, `edit`, `search`, `execute`, `fetch`, `switch_mode`, else `other`), the raw input, and `locations`: the file of a `read` or `write`, every file an `edit` names. A finished `write` or `edit` carries `diff` content (path, old and new text; files over 512 KiB are left out) next to the result text. `rawOutput` is `{text}`, or `{output, exitCode}` for `bash`.

While a command runs its newest output (up to 16 KiB, secrets redacted) is sent as `in_progress` updates, at most four a second per call, so a long build shows progress. mu runs commands itself and does not use the client's `terminal/*` methods.

Background jobs (`task`, and `bash` with `async`) show as tool calls of their own, `job:<id>`: `in_progress` with the subagent's tool count and latest activity, or the command's output, then `completed` or `failed`. Their results still reach the model as a notice, as in the TUI.

## Asking the client

- **Plans.** In plan mode, `propose_plan` asks for approval with `session/request_permission` (kind `switch_mode`, the plan as content). Options, in this order: approve and implement now keeping the conversation, approve and implement in a fresh session, approve and compact first, save the plan for later (a rejection: it is parked, not run), and keep planning. A client that auto-approves picks the first, so Paseo's Auto Accept implements the plan in this conversation. A cancelled request closes the review.
- **Questions.** The model's `ask` tool is offered only when the client advertises `elicitation.form`; each question becomes a form field (a choice, many choices, plus an optional typed answer). Without it the tool is hidden and the model asks in prose.

## Notices

Retry waits, compaction and other notices go out as `notice` updates when the client advertises `session.notices`. Any other client gets them on stderr, and during a prompt also as an italic line of the reply, so a long retry wait is visible.

## Not supported

`mcpServers` in `session/new` (ignored; mu uses its own plugins), `terminal/*` and `fs/*` (mu reads, writes and runs commands itself), per-turn token usage and cost (context size is reported with `usage_update`), authentication methods, and session fork.
