# Model roles

A role is a name for a model, set in mu's own `models.toml` (see [below](#models-toml)). Select one with `--model @role`, `/switch @role`, or in the `model:` list of a [custom agent](custom-agents.md).

`/model` (Alt+M) edits the saved roles: every role is listed, set or not, with the model it points at and what it is for, and the screen names the active set and the `models.toml` it writes to. Highlighting one shows what uses it, which of your agents list it, and what it is now: its model, or (when unset) the role mu uses instead. `/model @role` opens that role's editor directly. The same descriptions appear in the `model` option of an [ACP](acp.md) client. `/model` never changes the current session: `/switch` does that.

| Role | Used for | When unset |
|---|---|---|
| `default` | The model a session starts with; the keep-this-model choice when you approve a plan | mu picks a model itself |
| `plan` | The built-in plan mode's `model: "@plan"` ([Modes](modes.md)) | the session keeps its model |
| `task` | `task` subagents (they can edit and run commands), and agents whose `model:` names nothing you have set | `@default` |
| `scout` | `scout` subagents: read-only research, plan-mode research, the `/review` scouts | `@smol`, then `@default` |
| `review` | The `/review` lead (its scouts run on `@scout`) | `@plan`, then `@default` |
| `smol` | Session titles, plugins that ask for a model, a cheap choice when you approve a plan | `@default` |
| `memory` | The memory plugin's background review of sessions | `@smol`, then `@default` |
| `slow` | A stronger model offered when you approve a plan | not offered |

Roles in `cycle_order` are the ones offered when you approve a plan, and when you approve the model's request to enter a mode. Roles that only appear in `models.toml` and that nothing in mu or your agents refers to (omp's `commit`, say) are not listed.

Enter on a role in `/model` opens its editor: **Model** (a concrete model; the list is fetched only then; unassigned, it names the role the chain reaches), **Thinking** (`No override`, or `off` to `max`), **Clear assignment** (only while a model is assigned; Save then removes the key, so the fallback chain applies again), **Save** and **Cancel**. A custom role also has **Fallbacks** — every other role, each one toggled into the chain in the order you add it, tried in that order before `@task`'s chain when the role has no model, and reflected in the **Model** row as you add them — **Description** (what the model reads when it may delegate the role; its first line is shown beside it), **Delegate** (whether the model may pick the role for a `task` subagent) and **Delete role…**, a confirmation that names the other sets that keep the role and the agents that use it before removing it from this set and dropping its `[roles.<name>]` settings. Nothing is written until Save, and Esc or Cancel discards the draft. Changing only the model keeps the role's existing thinking suffix. Saving writes to the set that was shown when the editor opened, and never switches the current session; running sessions keep their model, and later reads of the role see the new value. Roles your agents or `cycle_order` name but mu has no use for are listed as `custom role`; an agent whose role is unset takes the `task` model.

`@role` that is not configured fails with a message that names what the role is for and its fallback chain.

## models.toml

mu reads model config from `models.toml` in its profile home (`~/.mu/models.toml`, or `~/.mu/profiles/<name>/models.toml` under an omp profile), never from omp's files. It holds the roles, the plan-approval `cycle_order`, per-model limits (what `models.yml` did) and compaction thresholds:

```toml
set = "openai"                        # the active set; optional
cycle_order = ["smol", "default", "slow"]

[sets.main]
default = "anthropic/claude-opus-5-5:high"
smol = "anthropic/claude-haiku-5-5"

[sets.openai]
default = "openai/gpt-5.6:high"

[roles.writer]                        # settings only a custom role has
description = "Long-form docs"        # what the model reads when it may delegate the role
fallback = ["smol"]                   # roles tried, in order, when @writer has no model
delegate = true                       # the model may pick @writer for a task subagent

[models."modal/a/b"]                  # the full provider/model id
context_window = 1048576
max_tokens = 65536

[compaction.thresholds]               # an integer caps the window, "N%" is a share of it, "fN" a fixed token count; keys may be globs
"openai/*" = "50%"
```

Every role lives in a **set**: a complete role list, usually one per provider. With one set (`main` by default) you never see sets. A role a set leaves out falls back along the chain in the table above, within that set. The active set is `set` if it names one; otherwise the only set, else `main`, else the first by name (and the file warns at startup when `set` is missing or wrong with several sets). `[roles.<name>]` is not part of a set: a custom role's `description`, `fallback` and `delegate` belong to the role in every set, while the model it points at is per set.

`/modelset` switches sets: bare it lists them, `/modelset <name>` makes one active for this profile (new sessions and subagents follow, and the current session moves to the set's `@default`, or the active mode's model). `/modelset new <name>` copies the active set and switches to it, `delete`, `rename`, and `import` complete it. `/model` edits roles in the active set.

### Importing omp's config

mu copies omp's model config only when you say so: a dialog at startup while `models.toml` does not exist and omp has model config, `/modelset import`, or `mu models import [--yes] [--force]`. It copies `modelRoles` as set `main`, each `modelPresets.<name>.modelRoles` as set `<name>`, `cycleOrder`, the limits in omp's `models.yml`, `compaction.modelThresholds`, and thresholds from `[compaction.model_thresholds]` in mu's old `config.toml`. omp's files are never written.

### Importing pi's setup

`mu models import --from pi` copies from pi's agent dir (`~/.pi/agent`, or `PI_CODING_AGENT_DIR`) the same one-time way: a startup row beside omp's (when `models.toml` is missing and pi has something to copy), `/modelset import [omp|pi]`, and `mu models import [--from omp|pi] [--yes] [--force]`. Without `--from` it uses the tool that has something and asks when both do (a script must pass `--from`). The listing before the approval shows what is copied and, one line each, what is skipped and why. pi's files are never written, and neither the listing nor the report shows a key. Each part goes in on its own:

| pi | mu |
|---|---|
| `settings.json` `defaultProvider` + `defaultModel` | role `default` of set `main` (`provider/model`; pi and mu name providers alike). Skipped when one is missing or the provider is a `models.json` provider mu did not import |
| `defaultThinkingLevel` | the role's `:level` suffix (`minimal` is `low`) |
| `compaction.modelOverrides.<provider/model>.reserveTokens` | `[compaction.thresholds]` `"provider/model" = "f<window - reserve>"`, only for a `models.json` model, whose context window is known; `keepRecentTokens` has no equivalent. Other models are skipped: set the threshold by hand |
| `models.json` providers with `api: "openai-completions"` | custom endpoints, see [providers.md](providers.md#importing-pi-endpoints-and-keys) |
| `auth.json` `api_key` logins | keys, see [providers.md](providers.md#importing-pi-endpoints-and-keys) |
| `theme` | `[theme] source`: `dark` is `tokyo-night`, `light` is `catppuccin-latte`, `system` is `terminal`; a custom pi theme is skipped |
| `hideThinkingBlock` | `[display] hide_thinking_block` ([config.md](config.md)) |
| `enabledModels` | not imported |

An existing `models.toml`, a key mu already stores, a `[theme] source` or `[display] hide_thinking_block` in `config.toml` are kept and reported as skipped; `--force` replaces them (`--force` replaces the whole `models.toml`, as for omp). A custom endpoint is never replaced. The other parts still import when `models.toml` exists.
