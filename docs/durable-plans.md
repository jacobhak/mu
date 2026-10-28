# Durable plans

A plan does not have to be approved in the session that wrote it. It can be parked, listed, refined and approved later, in the TUI or headless.

## Parking and approving

- In the plan review, **Park plan** saves the plan and ends the turn. Plan mode turns off.
- `/plans` lists the repository's pending plans. Choosing one opens the same review dialog as a live plan, with **Drop plan** in place of **Park plan**. Execute, compact, keep and refine apply to the current session. Esc leaves the plan parked.
- Execute, compact and keep on a live plan also save it, so every approved plan is in the store. A failed save never blocks approval.
- mu does not create worktrees: a plan runs in the directory mu runs in. There is no automatic review/fix loop; a run ends when the implementer's turn ends, and `/review` works against the approved plan as usual.

## Store

Per machine, under `$MU_HOME/plans/<repo-key>/`. The key is the main checkout's path, so all worktrees of one clone share a store. Outside git the key is the current directory.

- `<id>.md`: the plan text as written. No front matter; edit it with any tool. The review dialog re-reads it, so Ctrl-G edits count.
- `<id>.json`: metadata.
  - `status`: `pending`, `approved` or `dropped`.
  - `base`: the tree (working tree snapshot, uncommitted files included) and commit the plan was written against.
  - `read`: repo-relative files the session read with `read`. `grep` and `glob` are ignored.
  - `session`: the session that last saved the plan.
  - `approved`: time, decision, approving session path and a hash of the text approved.

The id is the plan's slug, or `slug-2`, `slug-3`, … when taken. Saving again from the same session, or while refining that plan, updates it in place: `created` is kept, `read` is merged, `base` is replaced and the status goes back to `pending`.

## Drift

Opening a plan compares the current working tree with `base`. Only files in `read` count; with an empty `read`, any change counts.

- `none of the files it read changed since abc1234`, plus ` (N other files changed)`.
- `N file(s) it read changed since abc1234: a.rs, b.rs`, listing five at most.
- `drift unknown: …` when the directory is not a git repository, no base was recorded, or the base was garbage-collected.

## Headless: `mu plan`

- `list [--all] [--json]`: pending plans, or all. `--json` adds `path`.
- `show <id> [--json]`: metadata, drift and text. `--json` adds `path`, `content` and `drift` (`state`: `unknown`, `clean` or `changed`).
- `new "<brief>"`: plans in plan mode with no user and parks the result. Prints `plan: <id> · <path>` to stderr. Exits 1 if nothing was parked.
- `refine <id> [text]`: revises a pending plan, then parks it again under the same id. Without text it brings the plan up to date with the current code.
- `approve <id> [--anyway]`: runs the plan like `-p`, in the current directory. `ask` is unavailable, no terminal is needed and `mu take` works. It exits 1 with `is stale` if files the plan read changed, unless `--anyway`. `--model` goes before `plan`.
- `drop <id>`: drops a pending plan.

`approve`, `refine` and `drop` need a pending plan. mu knows nothing about outside tools that call these commands.
