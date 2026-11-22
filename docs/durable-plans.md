# Durable plans

A plan does not have to be approved in the session that wrote it. It can be parked, listed, refined and approved later, in the TUI or headless.

## Parking and approving

- The model can switch the session to a [mode](modes.md) itself with the `enter_mode` tool, when you ask for a plan or before a large or risky change. Each mode's `model_entry` decides whether that asks: plan's is `ask`, so every call shows an approval prompt, in every permission mode, with no "don't ask again"; deny it with a note and the note goes back to the model. After you park a plan the model entered plan mode for, it may carry on in the same turn (for "write plans for A and B"); each new entry asks again. `-p` and `mu plan` runs and subagents are never offered `enter_mode`.
- In the plan review, **Park plan** saves the plan and ends the turn. Plan mode turns off, and the session keeps the model and thinking level it planned with; leaving plan mode with `/plan` and approving a plan still restore or switch the model. Parking applies to any mode: its dialog offers `park` when the mode's `exit.actions` lists it.
- The store records the mode a document was written in; `/plans` and `mu plan list` show it when it is not `plan`.
- `/plans` lists the repository's pending plans. Choosing one opens the same review dialog as a live plan, with **Drop plan** in place of **Park plan**. Execute, compact, keep and refine apply to the current session. Esc leaves the plan parked.
- Execute, compact and keep on a live plan also save it, so every approved plan is in the store. A failed save never blocks approval.
- Approving in a session does not create a worktree: the plan runs in the directory mu runs in. There is no automatic review/fix loop; a run ends when the implementer's turn ends, and `/review` works against the approved plan as usual.
- `/deliver [id]` hands a parked plan to `mu deliver <id> --worktree`, started detached (a picker of parked plans without an id): it implements the plan in a new worktree on branch `plan/<id>`, reviews it against the plan, sends blocking findings back to the implementer, then commits. The session wakes when it finishes.

## Store

Per machine and profile, under `<profile home>/plans/<repo-key>/` (see [Profile home](usage.md#profile-home)). The key is the main checkout's path, so all worktrees of one clone share a store. Outside git the key is the current directory.

- `<id>.md`: the plan text as written. No front matter; edit it with any tool. The review dialog re-reads it, so Ctrl-G edits count.
- `<id>.json`: metadata.
  - `status`: `pending`, `approved` or `dropped`.
  - `base`: the tree (working tree snapshot, uncommitted files included) and commit the plan was written against. When the snapshot differs from the commit, mu shows the base as `abc1234 plus uncommitted changes`.
  - `read`: repo-relative files the session read with `read`, selectors and `:raw` stripped. Directory listings, `grep` and `glob` are ignored: they show names, not contents. Paths that do not exist when the plan is saved are dropped.
  - `session`: the session that last saved the plan.
  - `approved`: time, decision, approving session path and a hash of the text approved.

The id is the plan's slug, or `slug-2`, `slug-3`, … when taken. Saving again from the same session, or while refining that plan, updates it in place: `created` is kept, `read` is merged, `base` is replaced and the status goes back to `pending`.

## Drift

Opening a plan compares the current working tree with `base`. Only files in `read` count; with an empty `read`, any change counts. Entries that are directories now (older plans recorded directory reads) are skipped.

- `none of the files it read changed since abc1234`, plus ` (N other files changed)`.
- `N file(s) it read changed since abc1234: a.rs, b.rs`, listing five at most.
- `drift unknown: …` when the directory is not a git repository, no base was recorded, or the base was garbage-collected.
- ` plus uncommitted changes` follows the commit when the base snapshot differs from it; `--json` drift has `dirty`.

## Headless: `mu plan`

- `list [--all] [--json]`: pending plans, or all. `--json` adds `path`.
- `show <id> [--json]`: metadata, drift and text. `--json` adds `path`, `content` and `drift` (`state`: `unknown`, `clean` or `changed`).
- `new [--mode <name>] "<brief>"`: plans in plan mode (or the named mode) with no user and parks the result. The model is told the run is headless and that parking is the expected result. Prints `plan: <id> · <path>` to stderr. Exits 1 if nothing was parked.
- `refine <id> [text]`: revises a pending plan, then parks it again under the same id. Without text it brings the plan up to date with the current code. The model is told the run is headless and that parking is the expected result.
- `approve <id> [--anyway]`: runs the plan like `-p`, in the current directory. `ask` is unavailable, no terminal is needed and `mu take` works. It exits 1 with `is stale` if files the plan read changed, unless `--anyway`. `--model` goes before `plan`.
- `drop <id>`: drops a pending plan.

`approve`, `refine` and `drop` need a pending plan. A refused `approve` or `refine` (not found, not pending, stale) creates no session. mu knows nothing about outside tools that call these commands.
