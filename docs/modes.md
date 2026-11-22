# Modes

A mode is a markdown file with YAML frontmatter that sets a prompt, a model and thinking level, which tools are allowed, the write policy, how the mode is entered, and the exit tool with the approval actions it offers. Plan mode is the built-in mode `plan`; you can override it and add your own.

While a mode is on, its prompt is added to the system prompt, its model is used (the session's previous model and thinking level come back when the mode ends), and its exit tool is offered in place of `enter_mode`. The exit tool opens the review dialog for the mode's document, a `local://<slug>-<document>.md` file the model wrote.

## Locations

Files are `*.md`; the mode's name is the `name` key or the file name. A name is lowercase letters, digits and `-`, starts with a letter, and is not `default` or `none`. A bad file is skipped with a startup warning.

Later layers override earlier ones:

1. the built-in modes (`plan`)
2. `<profile home>/modes/*.md` (see [Profile home](usage.md#profile-home))
3. `.agents/modes/*.md` in each directory from the home directory down to the current one, the nearer directory last

## Frontmatter

| Key | Meaning | Default for a new mode |
|---|---|---|
| `name` | Mode name | file name |
| `description` | Shown in `/` completion, the ACP mode list and `enter_mode`. Required in a new mode | |
| `extends` | Another mode to inherit from | none |
| `prompt` | `append` or `replace`: see [below](#extends-and-merging) | `append` |
| `model` | A selector or a list; the first usable one wins (`@role`, or a model id). An `@role` is usable when set | the session's model |
| `thinking` | A thinking level | the session's level |
| `tools.allow` | Only these tools (and the exit tool and `yield`) | all tools |
| `tools.deny` | These tools are refused | none |
| `writes` | `all`, or `local`: the working tree is read-only and only `local://` files are written | `all` |
| `subagent` | The agent every `task` call uses while the mode is on | the model's choice |
| `command` | Offer `/<name>` and an ACP mode | `true` |
| `model_entry` | Whether the model may enter the mode with `enter_mode`: `never`, `ask` (always asks) or `allow` | `never` |
| `exit.tool` | Name of the exit tool | `propose_<name>` (`-` becomes `_`) |
| `exit.description` | The exit tool's description (a template) | a generic one |
| `exit.document` | File suffix and noun: `local://<slug>-<document>.md` | the mode name |
| `exit.actions` | The review dialog's actions, in order, each effect at most once | those of `plan`: `execute, compact, keep, park, refine` |
| `exit.default` | The preselected effect | `execute` |
| `exit.approved` | Hidden prompt after `execute`, `compact` and `keep` (a template) | that of `plan` |
| `exit.refine` | Prompt for refining a parked document (a template) | that of `plan` |
| `exit.compact_focus` | Focus text of the `compact` effect | that of `plan` |
| `exit.headless` | Added to the prompt in `mu plan new` and `mu plan refine` | that of `plan` |

The text keys are YAML block scalars (`|-`); the Markdown body after the frontmatter is the prompt.

## Extends and merging

A file whose name matches an existing mode merges onto it; `extends: <mode>` merges onto that mode and defines a new one (the file's own name). Keys you leave out are inherited; keys you set replace the inherited value, so `exit.actions` and `model` are replaced whole, not combined. A new mode that extends nothing starts from the defaults above.

The body is appended to the inherited prompt, after a blank line, unless `prompt: replace`, which uses the body alone. A mode that extends a mode defined in the same directory waits for it; an unknown `extends` is skipped with a warning.

## Effects and labels

Each action is an effect name or `{effect: <name>, label: <text>}`. `{n}` in a label is the document noun and `{name}` the mode name.

| Effect | Default label | Does |
|---|---|---|
| `execute` | Approve and execute | runs the document in a fresh session |
| `compact` | Approve and compact context | compacts this conversation, then runs it here |
| `keep` | Approve and keep context | runs it here with the exploration history |
| `park` | Park {n} | saves it for later approval and ends the turn |
| `refine` | Refine {n} | sends your feedback to the model; the mode stays on |
| `exit` | Accept and leave {name} mode | ends the mode and runs nothing |

## Placeholders

In the prompt body and every template key: `{{name}}` (the mode), `{{tool}}` (the exit tool), `{{file}}` (`local://<slug>-<document>.md`) and `{{actions}}` (the actions, as a list the model reads).

Also in `exit.approved`: `{{context}}` (a note that the history is usable, empty for `execute`), `{{path}}` (the document's `local://` path) and `{{document}}` (its text). Also in `exit.refine`: `{{id}}` (the parked document's id), `{{path}}`, `{{drift}}` (what changed since it was written) and `{{request}}` (your text).

## Entering

- `/<name> [prompt]` for every mode with `command: true`. With a prompt it enters the mode and runs the prompt; without, it toggles the mode. Switching from another mode leaves it first. Built-in command names win over a mode of the same name.
- The model's `enter_mode` tool, offered while no mode is on and at least one mode has `model_entry` other than `never`. `ask` always asks you, in every permission mode, with no grant, and the prompt can enter the mode on another model: "Yes, on the current model" (when the mode would replace it) and one row per role in `cycle_order` (see [model roles](model-roles.md)) that has a model, each used in place of the mode's own model and thinking level; `allow` enters without asking. `-p` and `mu plan` runs and subagents are never offered it.
- [ACP](acp.md) clients list `default` plus every `command` mode.
- `mu plan new --mode <name>` plans in another mode, headless.

## Exiting and parking

Approving with `execute`, `compact` or `keep` ends the mode and starts the work. `exit` ends it with nothing to run. Toggling `/<name>` again leaves it too. `park` saves the document in the [plan store](durable-plans.md) and ends the turn; the store records the mode, so `/plans` and `mu plan list` show it, and approving or refining it later opens that mode's dialog (refine re-enters the mode). A stored document whose mode is not defined falls back to `plan` with a notice.

## Example: override plan

`.agents/modes/plan.md` keeps the built-in plan mode, changes its model and preselects keep-context:

```markdown
---
model: "@slow"
exit:
  default: keep
---
Prefer small, reviewable steps.
```

The body is appended to the built-in plan prompt.

## Example: a new mode

`<profile home>/modes/design.md`:

```markdown
---
description: Write a design document for review
extends: plan
prompt: replace
model_entry: ask
exit:
  document: design
  actions: [keep, park, refine, exit]
---
Design mode: the working tree is read-only. Write the design to `{{file}}`,
then call `{{tool}}`. Review options:
{{actions}}
```

It inherits plan's read-only writes and `scout` subagent. `/design` enters it, the model writes `local://<slug>-design.md` and calls `propose_design`.
