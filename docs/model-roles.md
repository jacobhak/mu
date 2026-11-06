# Model roles

A role is a name for a model, set in mu's own `models.toml` (see [below](#models-toml)). Select one with `--model @role`, `/switch @role`, or in the `model:` list of a [custom agent](custom-agents.md).

`/model` is where roles are seen and set: the role rows come first, each with the model it points at and what it is for. Highlighting one shows what uses it, which of your agents list it, and what it is now: its model, or (when unset) the role mu uses instead. The same descriptions appear in the "Set as @role" step, and in the `model` option of an [ACP](acp.md) client.

| Role | Used for | When unset |
|---|---|---|
| `default` | The model a session starts with; the keep-this-model choice when you approve a plan | mu picks a model itself |
| `plan` | Plan mode | `@default` |
| `task` | `task` subagents (they can edit and run commands), and agents whose `model:` names nothing you have set | `@default` |
| `scout` | `scout` subagents: read-only research, plan-mode research, the `/review` scouts | `@smol`, then `@default` |
| `review` | The `/review` lead (its scouts run on `@scout`) | `@plan`, then `@default` |
| `smol` | Session titles, plugins that ask for a model, a cheap choice when you approve a plan | `@default` |
| `memory` | The memory plugin's background review of sessions | `@smol`, then `@default` |
| `slow` | A stronger model offered when you approve a plan | not offered |

Roles in `cycle_order` are the ones offered when you approve a plan. Roles that only appear in `models.toml` and that nothing in mu or your agents refers to (omp's `commit`, say) are not listed.

Pressing Enter on a role with no model in `/model` opens the model list for it: the model you pick is assigned to that role in the active set. On a role that has a model, Enter switches this session to it. Roles your agents or `cycle_order` name but mu has no use for are listed as `custom role`; an agent whose role is unset takes the `task` model.

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

[models."modal/a/b"]                  # the full provider/model id
context_window = 1048576
max_tokens = 65536

[compaction.thresholds]               # an integer caps the window, "N%" is a share of it, "fN" a fixed token count; keys may be globs
"openai/*" = "50%"
```

Every role lives in a **set**: a complete role list, usually one per provider. With one set (`main` by default) you never see sets. A role a set leaves out falls back along the chain in the table above, within that set. The active set is `set` if it names one; otherwise the only set, else `main`, else the first by name (and the file warns at startup when `set` is missing or wrong with several sets).

`/modelset` switches sets: bare it lists them, `/modelset <name>` makes one active for this profile (new sessions and subagents follow, and the current session moves to the set's `@default`, or `@plan` in plan mode). `/modelset new <name>` copies the active set and switches to it, `delete`, `rename`, and `import` complete it. `/model` assigns roles in the active set.

### Importing omp's config

mu copies omp's model config only when you say so: a dialog at startup while `models.toml` does not exist and omp has model config, `/modelset import`, or `mu models import [--yes] [--force]`. It copies `modelRoles` as set `main`, each `modelPresets.<name>.modelRoles` as set `<name>`, `cycleOrder`, the limits in omp's `models.yml`, `compaction.modelThresholds`, and thresholds from `[compaction.model_thresholds]` in mu's old `config.toml`. omp's files are never written.
