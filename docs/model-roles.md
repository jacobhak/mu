# Model roles

A role is a name for a model, set in omp's `config.yml` (`modelRoles:`) and shared with omp. Select one with `--model @role`, `/switch @role`, or in the `model:` list of a [custom agent](custom-agents.md).

`/model` is where roles are seen and set: the role rows come first, each with the model it points at and what it is for. Highlighting one shows what uses it, which of your agents list it, and what it is now: its model, or (when unset) the role mu uses instead. The same descriptions appear in the "Set as @role" step, and in the `model` option of an [ACP](acp.md) client.

| Role | Used for | When unset |
|---|---|---|
| `default` | The model a session starts with; the keep-this-model choice when you approve a plan | mu picks a model itself |
| `plan` | Plan mode | `@default` |
| `task` | `task` subagents (they can edit and run commands), and agents whose `model:` names nothing you have set | `@default` |
| `scout` | `scout` subagents: read-only research, plan-mode research, the `/review` scouts | `@smol`, then `@default` |
| `review` | The `/review` lead (its scouts run on `@scout`) | `@plan`, then `@default` |
| `smol` | Session titles, plugins that ask for a model, a cheap choice when you approve a plan | `@default` |
| `memory` | The memory plugin | `@smol`, then `@default` |
| `slow` | A stronger model offered when you approve a plan | not offered |

Roles in `cycleOrder` are the ones offered when you approve a plan. Roles that only appear in `config.yml` and that nothing in mu or your agents refers to (omp's `commit`, say) are not listed.

Pressing Enter on a role with no model in `/model` opens the model list for it: the model you pick is assigned to that role. On a role that has a model, Enter switches this session to it. Roles your agents or `cycleOrder` name but mu has no use for are listed as `custom role`; an agent whose role is unset takes the `task` model.

`@role` that is not configured fails with a message that names what the role is for and its fallback chain.
