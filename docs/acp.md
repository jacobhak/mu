# Agent Client Protocol

`mu --acp` serves [ACP](https://agentclientprotocol.com) over stdio, so ACP clients such as [Paseo](https://paseo.sh) can drive it. In Paseo's `~/.paseo/config.json`:

```json
{"agents": {"providers": {"mu": {"extends": "acp", "label": "mu", "command": ["mu", "--acp"], "env": {"OMP_PROFILE": "private"}}}}}
```

Supported: `session/new`, `load` (replays history), `resume`, `close`, `prompt` (text, images, embedded resources), `cancel`, and `set_config_option` for `model` and `thinking`. `model` takes an advertised `provider/model` id or a role selector such as `@slow` or `@slow:high`, resolved as `--model` is (the advertised options list only concrete models); an unconfigured role fails with an invalid-params error. Updates cover message and thought chunks, tool calls, the todo list as a plan, context usage, and session titles. Each session runs its own agent in the `cwd` it was opened with.

Not supported: `mcpServers` in `session/new` (ignored; mu uses its own plugins), the `ask` and `propose_plan` tools (hidden from the model), permission prompts (mu never asks), and streaming bash output (the full output arrives when the tool ends). Setting a config option while a prompt runs is rejected. A failed turn ends with `end_turn` plus an `**Error:**` message chunk, not a JSON-RPC error.
